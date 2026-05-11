//! `autocoder run` — daemon entry point. Spawns one polling task per
//! configured repository and waits for shutdown signal (SIGINT/SIGTERM) or
//! all tasks to finish.

use crate::chatops::ChatOps;
use crate::code_reviewer::CodeReviewer;
use crate::config::{Config, ExecutorKind, GithubConfig, RepositoryConfig};
use crate::executor::{Executor, claude_cli::ClaudeCliExecutor};
use crate::github::parse_repo_url;
use crate::github_credentials::resolve_token;
use crate::polling_loop::ChatOpsContext;
use crate::{git, polling_loop, workspace};
use anyhow::{Context, Result, anyhow};
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub async fn execute(cfg: Config) -> Result<()> {
    workspace::detect_collisions(&cfg.repositories)?;
    validate_github_token_routes(&cfg.github, &cfg.repositories)?;

    let executor: Arc<dyn Executor> = match cfg.executor.kind {
        ExecutorKind::ClaudeCli => Arc::new(ClaudeCliExecutor::new(
            cfg.executor.command.clone(),
            cfg.executor.timeout_secs,
        )),
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
            let token = std::env::var(&s.bot_token_env).map_err(|_| {
                anyhow::anyhow!(
                    "slack.bot_token_env `{}` is not set in the process environment",
                    s.bot_token_env
                )
            })?;
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

    let mut tasks: JoinSet<()> = JoinSet::new();
    for repo in cfg.repositories.iter().cloned() {
        if !repo_passes_startup_check(&repo) {
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
            polling_loop::run(repo, executor, github, reviewer, chatops_ctx, cancel).await
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
        match resolve_token(github, &owner) {
            Ok(_) => {
                let env_var = pick_env_var_name(github, &owner);
                tracing::info!(
                    "repository {} will use GitHub token from env var {}",
                    repo.url,
                    env_var
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
    Ok(())
}

/// Return the env-var NAME (not value) that `resolve_token` will read for
/// this owner. Used only for the startup log line.
fn pick_env_var_name(github: &GithubConfig, owner: &str) -> String {
    if let Some(map) = github.owner_tokens.as_ref() {
        if let Some((_k, env_name)) = map
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(owner))
        {
            return env_name.clone();
        }
    }
    github.token_env.clone()
}

/// Initialize the workspace and check for a dirty working tree. Returns
/// `true` if the repository is healthy and a polling task should be spawned;
/// `false` (with a logged error) if the workspace is dirty or cannot be
/// initialized.
pub fn repo_passes_startup_check(repo: &RepositoryConfig) -> bool {
    let workspace_path = workspace::resolve_path(repo);
    if let Err(e) = workspace::ensure_initialized(&workspace_path, &repo.url) {
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
            tracing::error!(
                url = repo.url.as_str(),
                workspace = %workspace_path.display(),
                "workspace is dirty at startup ({dirty_count} entries from `git status --porcelain`); skipping this repository for the process lifetime"
            );
            false
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
        map.insert("covered-org".into(), covered_var.into());
        let github = GithubConfig {
            token_env: fallback_var.into(),
            owner_tokens: Some(map),
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
    fn startup_passes_when_every_repo_has_a_route() {
        let _g = ENV_LOCK.lock().unwrap();
        let personal_var = "AUTOCODER_TEST_STARTUP_PERSONAL";
        let fallback_var = "AUTOCODER_TEST_STARTUP_FALLBACK_SET";
        unsafe {
            std::env::set_var(personal_var, "personal-secret");
            std::env::set_var(fallback_var, "fallback-secret");
        }

        let mut map = HashMap::new();
        map.insert("rabbeverly".into(), personal_var.into());
        let github = GithubConfig {
            token_env: fallback_var.into(),
            owner_tokens: Some(map),
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

    /// 13.1.3 / orchestrator-cli baseline: a workspace dirty at startup
    /// causes that repository to be skipped for the process lifetime.
    /// Other configured repositories continue to be serviced.
    #[test]
    fn dirty_workspace_skipped_at_startup() {
        let (_dirty, dirty_path) = dirty_workspace_fixture();
        let (_clean, clean_path) = clean_workspace_fixture();

        let dirty_repo = cfg_with(dirty_path);
        let clean_repo = cfg_with(clean_path);

        // Dirty repo fails the startup check; clean repo passes.
        assert!(!repo_passes_startup_check(&dirty_repo),
            "dirty workspace must fail startup check");
        assert!(repo_passes_startup_check(&clean_repo),
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
