//! `autocoder run` — daemon entry point. Spawns one polling task per
//! configured repository and waits for shutdown signal (SIGINT/SIGTERM) or
//! all tasks to finish.

use crate::audits::{Audit, AuditRegistry, brightline::ArchitectureBrightlineAudit};
use crate::chatops;
use crate::code_reviewer::CodeReviewer;
use crate::config::{
    AuditSettings, AuditsConfig, Config, ExecutorKind, GithubConfig, NotificationsConfig,
    RepositoryConfig, validate_audit_type_names,
};
use crate::control_socket::{
    self, ChatOpsHolder, ChatOpsSlot, ControlState, GithubHolder, RepoTaskHandle, RepoTaskMap,
    ReviewerHolder, SpawnOutcome, SpawnRepoFn,
};
use crate::executor::{Executor, claude_cli::ClaudeCliExecutor};
use crate::github::parse_repo_url;
use crate::github_credentials::resolve_token_with_source;
use crate::{git, polling_loop, workspace};
use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub async fn execute(cfg: Config, config_path: PathBuf) -> Result<()> {
    openspec_preflight()?;
    workspace::detect_collisions(&cfg.repositories)?;
    validate_github_token_routes(&cfg.github, &cfg.repositories)?;
    if cfg.github.recreate_fork_on_reinit && cfg.github.fork_owner.is_none() {
        tracing::info!(
            "github.recreate_fork_on_reinit is true but fork_owner is unset; flag will have no effect"
        );
    }
    ensure_forks_exist(&cfg.github, &cfg.repositories).await?;

    let executor: Arc<dyn Executor> = match cfg.executor.kind {
        ExecutorKind::ClaudeCli => Arc::new(
            ClaudeCliExecutor::from_config(&cfg.executor)
                .context("initializing ClaudeCliExecutor from config")?,
        ),
    };

    let reviewer_initial: Option<Arc<CodeReviewer>> = match cfg.reviewer.as_ref() {
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

    let chatops_initial: Option<ChatOpsSlot> = match cfg.chatops.as_ref() {
        Some(co) => {
            let backend = chatops::from_config(co)
                .await
                .context("initializing chatops backend from config")?;
            emit_chatops_startup_log(backend.provider_name(), backend.is_experimental());
            Some(ChatOpsSlot {
                backend,
                default_channel_id: co.default_channel_id.clone(),
                start_work_enabled: NotificationsConfig::start_work_enabled(Some(co)),
                failure_alerts_enabled: NotificationsConfig::failure_alerts_enabled(Some(co)),
                pr_opened_enabled: NotificationsConfig::pr_opened_enabled(Some(co)),
            })
        }
        None => {
            tracing::info!("ChatOps escalation disabled (no chatops: config block)");
            None
        }
    };

    // Hot-swappable holders. The control socket swaps into these on
    // `autocoder reload`; the polling loops read snapshots once per pass.
    let github_holder: GithubHolder = Arc::new(ArcSwap::from_pointee(cfg.github.clone()));
    let reviewer_holder: ReviewerHolder = Arc::new(ArcSwap::from_pointee(reviewer_initial));
    let chatops_holder: ChatOpsHolder = Arc::new(ArcSwap::from_pointee(chatops_initial));

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

    // Perma-stuck consecutive-failure threshold. `perma_stuck_threshold`
    // clamps a misconfigured 0 to 1 internally; we WARN once here so the
    // operator notices their config is bogus.
    if cfg.executor.perma_stuck_after_failures == Some(0) {
        tracing::warn!(
            "executor.perma_stuck_after_failures is set to 0; clamping to 1 (a zero threshold would mark every change perma-stuck before the first attempt — fix your config)"
        );
    }
    let perma_stuck_threshold: u32 = cfg.executor.perma_stuck_threshold();

    // Per-PR change cap. Misconfigured `0` is clamped to `1` inside
    // `RepositoryConfig::max_changes_per_pr`; we WARN once here at startup
    // so the operator notices.
    if cfg.executor.max_changes_per_pr == Some(0) {
        tracing::warn!(
            "executor.max_changes_per_pr is set to 0; clamping to 1 (each PR would ship zero commits otherwise — fix your config)"
        );
    }
    for (idx, repo) in cfg.repositories.iter().enumerate() {
        if repo.max_changes_per_pr == Some(0) {
            tracing::warn!(
                idx = idx,
                url = %repo.url,
                "repositories[{idx}].max_changes_per_pr is set to 0; clamping to 1"
            );
        }
    }

    // Build the audit registry once at startup. Operators wire the
    // architecture-brightline audit by listing its slug under
    // `audits.defaults` (and optionally setting `extra` knobs under
    // `audits.settings.architecture_brightline`); the cadence resolver
    // returns `Disabled` for absent entries so the registry can stay
    // populated without forcing any audit to run.
    let audit_settings: HashMap<String, AuditSettings> = cfg
        .audits
        .as_ref()
        .map(|a| a.settings.clone())
        .unwrap_or_default();
    let audits_cfg_arc: Option<Arc<AuditsConfig>> = cfg.audits.clone().map(Arc::new);
    let mut registry = AuditRegistry::new();
    registry.register(Arc::new(ArchitectureBrightlineAudit::new(&audit_settings)));
    // Validate every audit type name in the operator's config is in the
    // registry. A typo here means the audit will silently never run, so
    // we fail fast at startup with the list of known names.
    validate_audit_type_names(&cfg, &registry.known_type_names())?;
    let audit_registry: Arc<AuditRegistry> = Arc::new(registry);
    let audit_settings_arc: Arc<HashMap<String, AuditSettings>> =
        Arc::new(audit_settings);

    let task_map: RepoTaskMap = Arc::new(Mutex::new(HashMap::new()));
    let executor_max_changes_per_pr = cfg.executor.max_changes_per_pr;
    let startup_jitter_max_secs = cfg.executor.startup_jitter_max_secs();
    let inter_iteration_jitter_pct = cfg.executor.inter_iteration_jitter_pct();
    let spawn_repo = build_spawn_repo_fn(SpawnDeps {
        executor: executor.clone(),
        github_holder: github_holder.clone(),
        reviewer_holder: reviewer_holder.clone(),
        chatops_holder: chatops_holder.clone(),
        stuck_threshold_secs,
        perma_stuck_threshold,
        executor_max_changes_per_pr,
        startup_jitter_max_secs,
        inter_iteration_jitter_pct,
        audit_registry: audit_registry.clone(),
        audits_cfg: audits_cfg_arc.clone(),
        audit_settings: audit_settings_arc.clone(),
        global_cancel: cancel.clone(),
        task_map: task_map.clone(),
    });

    for repo in cfg.repositories.iter().cloned() {
        match spawn_repo(repo) {
            SpawnOutcome::Spawned => {}
            SpawnOutcome::AlreadyPresent => {
                // Cannot happen at startup (map is empty) but log defensively.
                tracing::warn!("startup: spawn helper reported duplicate URL — ignoring");
            }
            SpawnOutcome::StartupCheckFailed => {
                // Per orchestrator-cli baseline: a repo whose workspace
                // fails the startup check is skipped for the remainder
                // of the process lifetime. Other repos continue.
            }
        }
    }

    // Spawn the control-socket listener as a sibling task. It shares the
    // same cancellation token as the polling tasks.
    let control_state = ControlState {
        github: github_holder.clone(),
        reviewer: reviewer_holder.clone(),
        chatops: chatops_holder.clone(),
        last_config: Arc::new(ArcSwap::from_pointee(cfg.clone())),
        config_path,
        repo_tasks: task_map.clone(),
        spawn_repo: spawn_repo.clone(),
    };
    let listener_cancel = cancel.clone();
    let control_handle: JoinHandle<()> = tokio::spawn(async move {
        if let Err(e) = control_socket::listen(control_state, listener_cancel).await {
            tracing::error!("control socket listener exited: {e:#}");
        }
    });

    spawn_signal_handler(cancel.clone());

    // The polling tasks loop until the global cancellation token fires
    // (or the per-repo token from a reload-induced cancel). Wait for the
    // global cancel, then drain the task map and await every polling
    // task. The wrapper inside the spawn closure removes its own entry
    // on exit, so by draining the map first we take ownership of the
    // JoinHandles before any wrapper races us for the lock.
    cancel.cancelled().await;
    let handles: Vec<JoinHandle<()>> = {
        let mut guard = task_map.lock().unwrap();
        guard.drain().map(|(_, h)| h.join).collect()
    };
    for h in handles {
        if let Err(e) = h.await {
            tracing::error!("polling task panicked: {e}");
        }
    }
    if let Err(e) = control_handle.await {
        tracing::error!("control socket task panicked: {e}");
    }

    tracing::info!("shutdown complete");
    Ok(())
}

/// Dependencies the daemon captures into the spawn closure so the reload
/// handler can launch new polling tasks without re-deriving them.
struct SpawnDeps {
    executor: Arc<dyn Executor>,
    github_holder: GithubHolder,
    reviewer_holder: ReviewerHolder,
    chatops_holder: ChatOpsHolder,
    stuck_threshold_secs: u64,
    perma_stuck_threshold: u32,
    executor_max_changes_per_pr: Option<u32>,
    startup_jitter_max_secs: u64,
    inter_iteration_jitter_pct: u8,
    audit_registry: Arc<AuditRegistry>,
    audits_cfg: Option<Arc<AuditsConfig>>,
    audit_settings: Arc<HashMap<String, AuditSettings>>,
    global_cancel: CancellationToken,
    task_map: RepoTaskMap,
}

/// Build a `SpawnRepoFn` that runs the repo's startup check, then spawns
/// the per-repo polling task with a fresh child cancellation token and a
/// new `Arc<ArcSwap<RepositoryConfig>>` holder. The spawned task removes
/// its own map entry on exit so the next reload sees an absent URL.
fn build_spawn_repo_fn(deps: SpawnDeps) -> SpawnRepoFn {
    Arc::new(move |repo: RepositoryConfig| {
        let url = repo.url.clone();
        // Fast-path duplicate check before doing the (potentially slow)
        // startup check. Re-checked under the lock below to close the
        // race window between this and the insert.
        {
            let guard = deps.task_map.lock().unwrap();
            if guard.contains_key(&url) {
                return SpawnOutcome::AlreadyPresent;
            }
        }
        // Startup check uses the live github config (post-reload it may
        // differ from what was on disk at process start).
        let github_snap = deps.github_holder.load_full();
        if !repo_passes_startup_check(&repo, &github_snap) {
            return SpawnOutcome::StartupCheckFailed;
        }
        let child_cancel = deps.global_cancel.child_token();
        let config_holder: Arc<ArcSwap<RepositoryConfig>> =
            Arc::new(ArcSwap::from_pointee(repo));
        let cancel_for_task = child_cancel.clone();
        let config_for_task = config_holder.clone();
        let map_for_task = deps.task_map.clone();
        let url_for_task = url.clone();
        let executor_for_task = deps.executor.clone();
        let github_for_task = deps.github_holder.clone();
        let reviewer_for_task = deps.reviewer_holder.clone();
        let chatops_for_task = deps.chatops_holder.clone();
        let stuck = deps.stuck_threshold_secs;
        let perma = deps.perma_stuck_threshold;
        let exec_max = deps.executor_max_changes_per_pr;
        let startup_jitter = deps.startup_jitter_max_secs;
        let iter_jitter = deps.inter_iteration_jitter_pct;
        let registry_for_task = deps.audit_registry.clone();
        let audits_cfg_for_task = deps.audits_cfg.clone();
        let audit_settings_for_task = deps.audit_settings.clone();
        let join: JoinHandle<()> = tokio::spawn(async move {
            polling_loop::run(
                config_for_task,
                executor_for_task,
                github_for_task,
                reviewer_for_task,
                chatops_for_task,
                stuck,
                perma,
                exec_max,
                startup_jitter,
                iter_jitter,
                registry_for_task,
                audits_cfg_for_task,
                audit_settings_for_task,
                cancel_for_task,
            )
            .await;
            let mut guard = map_for_task.lock().unwrap();
            guard.remove(&url_for_task);
        });
        let mut guard = deps.task_map.lock().unwrap();
        if guard.contains_key(&url) {
            // Lost the race against another spawn. Cancel ours and
            // report the URL as already present.
            child_cancel.cancel();
            return SpawnOutcome::AlreadyPresent;
        }
        guard.insert(
            url,
            RepoTaskHandle {
                cancel: child_cancel,
                config: config_holder,
                join,
            },
        );
        SpawnOutcome::Spawned
    })
}

/// Emit the one-shot startup log line for the active ChatOps backend.
/// Experimental backends get a `warn`-level line containing `"EXPERIMENTAL"`
/// and `"best-effort"`; Slack (and any future official backend) gets an
/// `info`-level line without those markers.
pub fn emit_chatops_startup_log(provider: &str, experimental: bool) {
    if experimental {
        tracing::warn!(
            "EXPERIMENTAL: ChatOps escalation enabled via {provider} — best-effort support, may break without notice, no API-stability guarantees"
        );
    } else {
        tracing::info!("ChatOps escalation enabled via {provider} (officially supported)");
    }
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
    // Recreate-fork mode + absent workspace: defer all init to the first
    // polling iteration. The recreate path runs async (`DELETE /repos/...`
    // + `POST /repos/.../forks`) and we can't call async work from the
    // sync spawn closure that wraps this function. The polling iteration
    // has its own async context and its own failure-alert plumbing.
    let defer_init =
        github.recreate_fork_on_reinit && fork_url.is_some() && !workspace_path.exists();
    if defer_init {
        tracing::info!(
            url = repo.url.as_str(),
            workspace = %workspace_path.display(),
            "deferring workspace init to first polling iteration \
             (recreate_fork_on_reinit + absent workspace)"
        );
        return true;
    }
    let fork_arg = fork_url
        .as_deref()
        .map(|u| (u, repo.agent_branch.as_str()));
    if let Err(e) = workspace::ensure_initialized(&workspace_path, &repo.url, fork_arg) {
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
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
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
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    #[tokio::test]
    async fn ensure_forks_exist_skipped_in_direct_push_mode() {
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
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
            recreate_fork_on_reinit: false,
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
            recreate_fork_on_reinit: false,
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
            recreate_fork_on_reinit: false,
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
            recreate_fork_on_reinit: false,
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
            recreate_fork_on_reinit: false,
        };
        assert!(
            repo_passes_startup_check(&dirty_repo, &direct_push_github),
            "dirty workspace must auto-recover and pass the startup check"
        );

        // After recovery the workspace is clean.
        let after = git::status_porcelain(&dirty_path).unwrap();
        assert!(after.is_empty(), "workspace must be clean after recovery, got: {after}");
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn startup_logs_info_for_slack() {
        emit_chatops_startup_log("slack", false);
        assert!(logs_contain("ChatOps escalation enabled via slack"));
        assert!(logs_contain("officially supported"));
        assert!(!logs_contain("EXPERIMENTAL"));
        assert!(!logs_contain("best-effort"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn startup_logs_experimental_warning_for_discord() {
        emit_chatops_startup_log("discord", true);
        assert!(logs_contain("EXPERIMENTAL"));
        assert!(logs_contain("best-effort"));
        assert!(logs_contain("discord"));
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
            recreate_fork_on_reinit: false,
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
