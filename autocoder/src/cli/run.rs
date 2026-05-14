//! `autocoder run` — daemon entry point. Spawns one polling task per
//! configured repository and waits for shutdown signal (SIGINT/SIGTERM) or
//! all tasks to finish.

use crate::chatops::ChatOps;
use crate::code_reviewer::CodeReviewer;
use crate::config::{Config, ExecutorKind, GithubConfig, RepositoryConfig};
use crate::executor::{Executor, claude_cli::ClaudeCliExecutor};
use crate::github::parse_repo_url;
use crate::github_credentials::resolve_token_with_source;
use crate::polling_loop::ChatOpsContext;
use crate::{git, polling_loop, workspace};
use anyhow::{Context, Result, anyhow};
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub async fn execute(cfg: Config) -> Result<()> {
    openspec_preflight()?;
    workspace::detect_collisions(&cfg.repositories)?;
    validate_github_token_routes(&cfg.github, &cfg.repositories)?;
    ensure_forks_exist(&cfg.github, &cfg.repositories).await?;

    let executor: Arc<dyn Executor> = match cfg.executor.kind {
        ExecutorKind::ClaudeCli => Arc::new(
            ClaudeCliExecutor::from_config(&cfg.executor)
                .context("initializing ClaudeCliExecutor from config")?,
        ),
    };

    let reviewer: Option<Arc<CodeReviewer>> = match cfg.reviewer.as_ref() {
        Some(rcfg) if rcfg.enabled => {
            let r = CodeReviewer::from_config(rcfg)
                .context("initializing code reviewer from config")?;
            tracing::info!(
                provider = ?rcfg.provider,
                model = rcfg.model.as_str(),
                "code reviewer enabled"
            );
            Some(Arc::new(r))
        }
        _ => {
            tracing::info!("code reviewer disabled (no reviewer block, or enabled: false)");
            None
        }
    };

    let chatops: Option<Arc<ChatOps>> = match cfg.slack.as_ref() {
        Some(s) => {
            let token = match (s.bot_token.as_ref(), s.bot_token_env.as_ref()) {
                (Some(inline), env_name_opt) => {
                    let resolved = inline.resolve("slack.bot_token")?;
                    if inline.is_inline() {
                        if let Some(env_name) = env_name_opt {
                            if std::env::var(env_name).is_ok() {
                                tracing::warn!(
                                    "slack.bot_token (inline) takes precedence; env var `{env_name}` is being ignored for the Slack bot token"
                                );
                            }
                        }
                    }
                    resolved
                }
                (None, Some(env_name)) => crate::config::SecretSource::EnvVar(env_name.clone())
                    .resolve(&format!("slack.bot_token_env={env_name}"))?,
                (None, None) => {
                    return Err(anyhow::anyhow!(
                        "slack config has neither `bot_token` (inline) nor `bot_token_env` (env var name) set"
                    ));
                }
            };
            let client = ChatOps::new(token)
                .await
                .context("initializing Slack ChatOps from config")?;
            tracing::info!(
                bot_user_id = client.bot_user_id(),
                default_channel = s.default_channel_id.as_str(),
                "ChatOps escalation enabled"
            );
            Some(Arc::new(client))
        }
        None => {
            tracing::info!("ChatOps escalation disabled (no `slack:` config block)");
            None
        }
    };

    for repo in &cfg.repositories {
        let derived = workspace::resolve_path(repo);
        tracing::info!(
            url = repo.url.as_str(),
            workspace = %derived.display(),
            poll_interval_sec = repo.poll_interval_sec,
            "configured repository"
        );
    }

    let cancel = CancellationToken::new();

    // Busy-marker stuck threshold: how long an in-flight iteration is
    // allowed to hold the marker before the next pass treats it as
    // potentially crashed. Sized as the executor's wall-clock budget
    // plus a 10-minute buffer for review/push/PR steps.
    let stuck_threshold_secs: u64 = cfg.executor.timeout_secs.saturating_add(600);

    let mut tasks: JoinSet<()> = JoinSet::new();
    for repo in cfg.repositories.iter().cloned() {
        if !repo_passes_startup_check(&repo, &cfg.github) {
            // Per orchestrator-cli baseline: a repo dirty at startup is
            // skipped for the remainder of the process lifetime. Other
            // configured repositories continue to be serviced.
            continue;
        }
        let executor = executor.clone();
        let github = cfg.github.clone();
        let reviewer = reviewer.clone();
        let cancel = cancel.clone();

        // Build the per-repo ChatOps context: resolve the channel via the
        // per-repo override or the global default.
        let chatops_ctx: Option<Arc<ChatOpsContext>> = match (chatops.clone(), cfg.slack.as_ref()) {
            (Some(co), Some(slack_cfg)) => {
                let channel = repo
                    .slack_channel(&slack_cfg.default_channel_id)
                    .to_string();
                Some(Arc::new(ChatOpsContext { chatops: co, channel }))
            }
            _ => None,
        };

        tasks.spawn(async move {
            polling_loop::run(
                repo,
                executor,
                github,
                reviewer,
                chatops_ctx,
                stuck_threshold_secs,
                cancel,
            )
            .await
        });
    }

    spawn_signal_handler(cancel.clone());

    while let Some(joined) = tasks.join_next().await {
        if let Err(e) = joined {
            tracing::error!("polling task panicked: {e}");
        }
    }

    tracing::info!("shutdown complete");
    Ok(())
}

/// Verify the `openspec` binary is reachable before the polling loop
/// starts. A failed preflight aborts daemon startup so misconfigured
/// deployments fail loudly instead of looping forever producing nothing.
pub fn openspec_preflight() -> Result<()> {
    openspec_preflight_with("openspec")
}

/// Internal preflight that takes the binary name as an argument so tests
/// can target a name guaranteed to be absent.
fn openspec_preflight_with(bin: &str) -> Result<()> {
    match std::process::Command::new(bin).arg("--version").output() {
        Ok(out) if out.status.success() => {
            tracing::info!(
                version = %String::from_utf8_lossy(&out.stdout).trim(),
                "openspec preflight passed"
            );
            Ok(())
        }
        Ok(out) => {
            let stderr_tail: String =
                String::from_utf8_lossy(&out.stderr).chars().take(200).collect();
            Err(anyhow!(
                "openspec preflight failed: `{bin} --version` exited {code:?}. stderr: {stderr_tail}",
                code = out.status.code(),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "openspec preflight failed: `{bin}` binary not found on PATH. \
             Install openspec and ensure the systemd unit's PATH covers its install directory."
        )),
        Err(e) => Err(anyhow!(
            "openspec preflight failed: spawning `{bin} --version` errored: {e}"
        )),
    }
}

/// Resolve a GitHub PAT route for every configured repository before any
/// polling task spawns. Returns `Err` aggregating every failure when one
/// or more repos have no routable token; on success, emits one info log
/// per repo naming the env var (never the token value) that will be used.
pub fn validate_github_token_routes(
    github: &GithubConfig,
    repos: &[RepositoryConfig],
) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    for repo in repos {
        let owner = match parse_repo_url(&repo.url) {
            Ok((o, _r)) => o,
            Err(e) => {
                failures.push(format!("repo `{}`: {e:#}", repo.url));
                continue;
            }
        };
        match resolve_token_with_source(github, &owner) {
            Ok((_value, source_desc)) => {
                tracing::info!(
                    "repository {} will use GitHub token from {}",
                    repo.url,
                    source_desc
                );
            }
            Err(e) => {
                failures.push(format!("repo `{}`: {e:#}", repo.url));
            }
        }
    }
    if !failures.is_empty() {
        return Err(anyhow!(
            "GitHub token routing failed for {} repository(ies):\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        ));
    }
    // Precedence warning: if `github.token` is inline AND the env var named
    // by `github.token_env` is also set, the inline value wins; tell the
    // operator their env var is being ignored on this field.
    if github
        .token
        .as_ref()
        .map(|s| s.is_inline())
        .unwrap_or(false)
        && std::env::var(&github.token_env).is_ok()
    {
        tracing::warn!(
            "github.token (inline) takes precedence; env var `{}` is being ignored for the global GitHub token",
            github.token_env
        );
    }
    Ok(())
}

/// When fork-PR mode is active, ensure each configured repository has a
/// reachable fork at the derived URL. Missing forks are created via the
/// GitHub REST API, then probed via `git ls-remote` with a 60-second
/// timeout. Aggregates failures into a single startup error.
pub async fn ensure_forks_exist(
    github: &GithubConfig,
    repos: &[RepositoryConfig],
) -> Result<()> {
    let Some(fork_owner) = github.fork_owner.as_deref() else {
        return Ok(());
    };
    let mut failures: Vec<String> = Vec::new();
    for repo in repos {
        let fork_url = match crate::github::derive_fork_url(&repo.url, fork_owner) {
            Ok(u) => u,
            Err(e) => {
                failures.push(format!("repo `{}`: {e:#}", repo.url));
                continue;
            }
        };
        // Quick probe: if the fork is already there, do nothing.
        if crate::git::ls_remote_head(&fork_url).is_ok() {
            continue;
        }
        // Missing fork → POST to GitHub.
        let (upstream_owner, upstream_repo) = match parse_repo_url(&repo.url) {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("repo `{}`: {e:#}", repo.url));
                continue;
            }
        };
        let token = match resolve_token_with_source(github, &upstream_owner) {
            Ok((tok, _src)) => tok,
            Err(e) => {
                failures.push(format!("repo `{}`: cannot resolve PAT for fork creation: {e:#}", repo.url));
                continue;
            }
        };
        tracing::info!(
            "creating fork for {} → {fork_url}",
            repo.url
        );
        if let Err(e) =
            crate::github::create_fork(&upstream_owner, &upstream_repo, &token).await
        {
            failures.push(format!(
                "repo `{}`: fork creation POST failed: {e:#}",
                repo.url
            ));
            continue;
        }
        // Poll until reachable, up to 60 seconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut reachable = false;
        tracing::info!(
            "waiting for fork `{fork_url}` to become reachable (up to 60s)"
        );
        while std::time::Instant::now() < deadline {
            if crate::git::ls_remote_head(&fork_url).is_ok() {
                reachable = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if reachable {
            tracing::info!(
                "created fork {fork_url} from upstream {}",
                repo.url
            );
        } else {
            failures.push(format!(
                "repo `{}`: fork creation succeeded but `{fork_url}` was not reachable within 60s",
                repo.url
            ));
        }
    }
    if !failures.is_empty() {
        return Err(anyhow!(
            "fork-PR mode: {} repository(ies) could not be set up under `{fork_owner}`:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        ));
    }
    Ok(())
}

/// Initialize the workspace and check for a dirty working tree. Returns
/// `true` if the repository is healthy and a polling task should be spawned;
/// `false` (with a logged error) if the workspace is dirty or cannot be
/// initialized.
pub fn repo_passes_startup_check(repo: &RepositoryConfig, github: &GithubConfig) -> bool {
    let workspace_path = workspace::resolve_path(repo);
    let fork_url = match github.fork_owner.as_deref() {
        Some(owner) => match crate::github::derive_fork_url(&repo.url, owner) {
            Ok(u) => Some(u),
            Err(e) => {
                tracing::error!(
                    url = repo.url.as_str(),
                    "cannot derive fork URL for fork-PR mode: {e:#}; this repository is skipped for the process lifetime"
                );
                return false;
            }
        },
        None => None,
    };
    if let Err(e) = workspace::ensure_initialized(&workspace_path, &repo.url, fork_url.as_deref()) {
        tracing::error!(
            url = repo.url.as_str(),
            workspace = %workspace_path.display(),
            "workspace initialization failed; this repository is skipped for the process lifetime: {e:#}"
        );
        return false;
    }
    match git::status_porcelain(&workspace_path) {
        Ok(s) if s.is_empty() => true,
        Ok(dirty) => {
            let dirty_count = dirty.lines().count();
            tracing::warn!(
                url = repo.url.as_str(),
                workspace = %workspace_path.display(),
                "workspace dirty at startup ({dirty_count} entries); attempting recovery (git reset --hard origin/{} + git clean -fd)",
                repo.base_branch
            );
            // Best-effort: ignore checkout failures (might already be on
            // base, or HEAD might be detached). The reset + clean are what
            // actually clear the dirty state.
            let _ = git::checkout(&workspace_path, &repo.base_branch);
            if let Err(e) = git::reset_hard_to_remote(&workspace_path, &repo.base_branch) {
                tracing::error!(
                    url = repo.url.as_str(),
                    "recovery `git reset --hard origin/{}` failed: {e:#}; skipping this repository for the process lifetime",
                    repo.base_branch
                );
                return false;
            }
            if let Err(e) = git::clean_force(&workspace_path) {
                tracing::error!(
                    url = repo.url.as_str(),
                    "recovery `git clean -fd` failed: {e:#}; skipping this repository for the process lifetime"
                );
                return false;
            }
            match git::status_porcelain(&workspace_path) {
                Ok(s) if s.is_empty() => {
                    tracing::info!(
                        url = repo.url.as_str(),
                        "workspace recovered; proceeding to normal polling"
                    );
                    true
                }
                _ => {
                    tracing::error!(
                        url = repo.url.as_str(),
                        "workspace still dirty after recovery; skipping this repository for the process lifetime"
                    );
                    false
                }
            }
        }
        Err(e) => {
            tracing::error!(
                url = repo.url.as_str(),
                "could not run git status on workspace: {e:#}; skipping this repository for the process lifetime"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn run_git(path: &Path, args: &[&str]) {
        let st = Command::new("git").args(args).current_dir(path).status().unwrap();
        assert!(st.success(), "git {args:?} failed");
    }

    #[test]
    fn preflight_errors_when_openspec_binary_missing() {
        let err = openspec_preflight_with("openspec-definitely-not-installed-on-this-host")
            .expect_err("missing binary must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("openspec"), "error must name openspec: {msg}");
        assert!(
            msg.contains("PATH") || msg.contains("not found"),
            "error must hint at PATH/install: {msg}"
        );
    }

    #[test]
    fn preflight_errors_when_binary_exits_nonzero() {
        // `false` always exits 1. Path differs by platform (/bin/false on
        // Linux, /usr/bin/false on macOS) — pick whichever exists so the
        // test runs on both.
        let false_bin = ["/bin/false", "/usr/bin/false"]
            .iter()
            .copied()
            .find(|p| std::path::Path::new(p).exists())
            .expect("a `false` binary must exist for this test");
        let err = openspec_preflight_with(false_bin).expect_err("nonzero exit must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("exited"), "error must mention exit code: {msg}");
    }

    /// Build a remote + workspace clone pair. The workspace has `origin`
    /// pointing at the remote, so `git fetch` succeeds during the startup
    /// check.
    fn workspace_pair() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "-q", "-b", "main"]);
        run_git(&remote, &["config", "user.email", "test@example.com"]);
        run_git(&remote, &["config", "user.name", "test"]);
        std::fs::write(remote.join("README.md"), "x").unwrap();
        run_git(&remote, &["add", "README.md"]);
        run_git(&remote, &["commit", "-q", "-m", "initial"]);

        let parent = workspace.parent().unwrap();
        let st = Command::new("git")
            .args(["clone", "-q", remote.to_string_lossy().as_ref(),
                   workspace.to_string_lossy().as_ref()])
            .current_dir(parent)
            .status()
            .unwrap();
        assert!(st.success(), "clone failed");
        run_git(&workspace, &["config", "user.email", "test@example.com"]);
        run_git(&workspace, &["config", "user.name", "test"]);
        (dir, workspace)
    }

    fn dirty_workspace_fixture() -> (TempDir, PathBuf) {
        let (dir, path) = workspace_pair();
        // Untracked file → status --porcelain non-empty → dirty.
        std::fs::write(path.join("LEFTOVER.txt"), "stale\n").unwrap();
        (dir, path)
    }

    fn clean_workspace_fixture() -> (TempDir, PathBuf) {
        workspace_pair()
    }

    fn cfg_with(local: PathBuf) -> RepositoryConfig {
        RepositoryConfig {
            url: format!("git@github.com:fixture/{}.git", local.file_name().unwrap().to_string_lossy()),
            local_path: Some(local),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            slack_channel_id: None,
        }
    }

    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Env-var mutation is global; serialize the startup-validation tests
    /// that touch real env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn repo(url: &str) -> RepositoryConfig {
        RepositoryConfig {
            url: url.into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            slack_channel_id: None,
        }
    }

    #[tokio::test]
    async fn ensure_forks_exist_skipped_in_direct_push_mode() {
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
        };
        // No repos to validate; no fork_owner means the function returns Ok
        // without probing anything.
        let repos = vec![repo("git@github.com:any/repo.git")];
        ensure_forks_exist(&github, &repos)
            .await
            .expect("direct-push mode skips fork probing");
    }

    #[tokio::test]
    async fn ensure_forks_exist_errors_on_unsupported_url_scheme() {
        // Non-github URL combined with fork-PR mode → derive_fork_url
        // rejects → validation aggregates the failure.
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: Some("machine-user".into()),
        };
        let repos = vec![repo("ssh://git@github.com/upstream/repo.git")];
        let err = ensure_forks_exist(&github, &repos)
            .await
            .expect_err("unsupported URL scheme must error in fork mode");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("upstream/repo.git"),
            "error must name the offending URL; got: {msg}"
        );
    }

    #[test]
    fn startup_fails_when_no_token_route() {
        // Two repos: one has a matching owner_tokens entry whose env var
        // is set; the other has no entry AND `token_env`'s named env var
        // is unset. The aggregated error must name the unmappable repo.
        let _g = ENV_LOCK.lock().unwrap();
        let covered_var = "AUTOCODER_TEST_STARTUP_COVERED";
        let fallback_var = "AUTOCODER_TEST_STARTUP_FALLBACK_UNSET";
        unsafe {
            std::env::set_var(covered_var, "ok");
            std::env::remove_var(fallback_var);
        }

        let mut map = HashMap::new();
        map.insert(
            "covered-org".into(),
            crate::config::SecretSource::EnvVar(covered_var.into()),
        );
        let github = GithubConfig {
            token_env: fallback_var.into(),
            token: None,
            owner_tokens: Some(map),
            fork_owner: None,
        };

        let repos = vec![
            repo("git@github.com:covered-org/repo-a.git"),
            repo("git@github.com:other-org/repo-b.git"),
        ];

        let err = validate_github_token_routes(&github, &repos)
            .expect_err("must fail when a repo has no route");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("other-org/repo-b.git"),
            "error must name the unmappable repo URL; got: {msg}"
        );
        assert!(
            msg.contains(fallback_var),
            "error must name the unset fallback env var; got: {msg}"
        );
        assert!(
            !msg.contains("covered-org/repo-a.git"),
            "error must not include the successfully-routed repo; got: {msg}"
        );

        unsafe { std::env::remove_var(covered_var) };
    }

    #[test]
    fn startup_passes_with_inline_owner_token_and_no_env() {
        // No env vars set for either the owner-specific source or the
        // fallback; both routes resolved entirely via inline values.
        let _g = ENV_LOCK.lock().unwrap();
        let mut map = HashMap::new();
        map.insert(
            "fixture-org".into(),
            crate::config::SecretSource::Inline {
                value: "inline-org-pat".into(),
            },
        );
        let github = GithubConfig {
            token_env: "AUTOCODER_TEST_INLINE_ROUTE_FALLBACK_NEVER_SET".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "inline-fallback-pat".into(),
            }),
            owner_tokens: Some(map),
            fork_owner: None,
        };
        let repos = vec![
            repo("git@github.com:fixture-org/repo.git"),    // owner_tokens hit
            repo("git@github.com:uncovered-org/repo.git"),  // fallback to github.token inline
        ];
        validate_github_token_routes(&github, &repos)
            .expect("both repos should resolve via inline sources");
    }

    #[test]
    fn startup_passes_when_every_repo_has_a_route() {
        let _g = ENV_LOCK.lock().unwrap();
        let personal_var = "AUTOCODER_TEST_STARTUP_PERSONAL";
        let fallback_var = "AUTOCODER_TEST_STARTUP_FALLBACK_SET";
        unsafe {
            std::env::set_var(personal_var, "personal-secret");
            std::env::set_var(fallback_var, "fallback-secret");
        }

        let mut map = HashMap::new();
        map.insert(
            "rabbeverly".into(),
            crate::config::SecretSource::EnvVar(personal_var.into()),
        );
        let github = GithubConfig {
            token_env: fallback_var.into(),
            token: None,
            owner_tokens: Some(map),
            fork_owner: None,
        };

        let repos = vec![
            repo("git@github.com:rabbeverly/personal-repo.git"),
            repo("git@github.com:some-org/work-repo.git"),
        ];

        validate_github_token_routes(&github, &repos)
            .expect("both repos should resolve: one via owner_tokens, one via fallback");

        unsafe {
            std::env::remove_var(personal_var);
            std::env::remove_var(fallback_var);
        }
    }

    /// A workspace dirty at startup (residue from a prior failed run) is
    /// auto-recovered via `git reset --hard origin/<base>` + `git clean
    /// -fd`. After recovery the workspace is clean and the startup check
    /// returns true.
    #[test]
    fn dirty_workspace_recovers_at_startup() {
        let (_dirty, dirty_path) = dirty_workspace_fixture();
        // Sanity: fixture really is dirty before the check.
        let before = git::status_porcelain(&dirty_path).unwrap();
        assert!(!before.is_empty(), "fixture must start dirty");

        let dirty_repo = cfg_with(dirty_path.clone());
        let direct_push_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
        };
        assert!(
            repo_passes_startup_check(&dirty_repo, &direct_push_github),
            "dirty workspace must auto-recover and pass the startup check"
        );

        // After recovery the workspace is clean.
        let after = git::status_porcelain(&dirty_path).unwrap();
        assert!(after.is_empty(), "workspace must be clean after recovery, got: {after}");
    }

    #[test]
    fn clean_workspace_still_passes_startup() {
        let (_clean, clean_path) = clean_workspace_fixture();
        let clean_repo = cfg_with(clean_path);
        let direct_push_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
        };
        assert!(repo_passes_startup_check(&clean_repo, &direct_push_github),
            "clean workspace must pass startup check");
    }
}

fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };

        #[cfg(unix)]
        let terminate = async {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(e) => {
                    tracing::warn!("could not install SIGTERM handler: {e}");
                    std::future::pending::<()>().await;
                }
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            () = ctrl_c => tracing::info!("received SIGINT; shutting down"),
            () = terminate => tracing::info!("received SIGTERM; shutting down"),
        }
        cancel.cancel();
    });
}
