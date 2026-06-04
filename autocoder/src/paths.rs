//! Standard-layout daemon path resolution.
//!
//! The daemon writes four categories of data:
//!
//! - `state` — persistent state (audit cadence, failure counters,
//!   revision state, alert throttles). Survives reboot.
//! - `cache` — re-creatable but kept (repo workspaces). Survives reboot.
//! - `logs` — per-change run logs + audit logs. Survives reboot.
//! - `runtime` — control socket, transient pid/lock files. Cleared on
//!   reboot (by design — these are per-process artefacts).
//!
//! Each category has its own resolved directory. Resolution priority:
//!   1. `config.paths.<field>` if set AND non-empty.
//!   2. `AUTOCODER_STATE_DIR` / `AUTOCODER_CACHE_DIR` / `AUTOCODER_LOGS_DIR`
//!      / `AUTOCODER_RUNTIME_DIR` env vars.
//!   3. systemd-set `$STATE_DIRECTORY` / `$CACHE_DIRECTORY` /
//!      `$LOGS_DIRECTORY` / `$RUNTIME_DIRECTORY`.
//!   4. XDG defaults under `$HOME` (dev mode).
//!   5. Hard fallback to `/var/lib/autocoder` etc.
//!
//! # Threading convention
//!
//! `Arc<DaemonPaths>` is constructed once at daemon startup AND
//! threaded explicitly via constructor fields OR function parameters.
//! No process-global cell — there is no `OnceLock<DaemonPaths>` for
//! production code to fall back on. The four legacy accessors
//! (`current()`, `install_global()`, `get_global()`, `test_fallback()`)
//! have been removed; their reintroduction is blocked by the
//! `path_literals_audit` integration test.
//!
//! Production code receives `DaemonPaths` via one of two patterns:
//!
//! - **Constructor-field pattern** — struct-shaped consumers
//!   (`ClaudeCliExecutor`, `AuditScheduler`, etc.) gain a
//!   `paths: Arc<DaemonPaths>` field on their constructor AND keep
//!   the value alive for the life of the struct.
//! - **Function-parameter pattern** — free-function modules
//!   (`alert_state`, `workspace`, `audits::threads`, etc.) gain a
//!   `paths: &DaemonPaths` parameter on every public function that
//!   needs to resolve a state/cache/logs/runtime path. The polling
//!   loop's top-level orchestrator owns the `Arc<DaemonPaths>` AND
//!   hands `&*paths` to each helper at the call boundary.
//!
//! Tests construct their own `DaemonPaths` via
//! [`crate::testing::test_daemon_paths`] (which hands out a tempdir-
//! scoped instance) AND pass it explicitly into the production APIs
//! they exercise. Two concurrent tests' fixtures live under DISJOINT
//! tempdirs, so they cannot collide on disk.
//!
//! See the canonical `orchestrator-cli` spec's "Production paths SHALL
//! be threaded through APIs, NOT read from a process-global"
//! requirement for the full contract AND its acceptance scenarios.

use crate::config::Config;
use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

/// The four standard daemon paths, resolved at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    pub state: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
    pub runtime: PathBuf,
}

impl DaemonPaths {
    /// Construct a `DaemonPaths` for use in tests where every root is a
    /// subdirectory of one tempdir. The directories are NOT created;
    /// callers that need them on disk run `mkdir_all`.
    #[allow(dead_code)]
    pub fn under_root(root: &Path) -> Self {
        Self {
            state: root.join("state"),
            cache: root.join("cache"),
            logs: root.join("logs"),
            runtime: root.join("runtime"),
        }
    }

    // ----- Per-state-shape helpers -----
    //
    // Every daemon-side state-file read OR write must route through one
    // of these helpers (or through the four bare `state`/`cache`/`logs`/
    // `runtime` fields when no shape-specific helper exists). The
    // `path_literals_audit` integration test enforces this rule against
    // the legacy hard-coded path prefix — adding a new state-file shape
    // means adding a helper here, not hard-coding a path at the call
    // site.
    //
    // `#[allow(dead_code)]` on the helpers that no production callsite
    // exercises yet: they're forward-looking API surface for state-shape
    // modules whose existing `(state_dir_root: &Path)` APIs will route
    // through DaemonPaths in a follow-up refactor. The unit tests in
    // this file exercise every helper.

    /// `<state>/audit-threads/` — per-`thread_ts` state files for the
    /// `audit-reply-acts` (send-it) flow.
    #[allow(dead_code)]
    pub fn audit_threads_dir(&self) -> PathBuf {
        self.state.join("audit-threads")
    }

    /// `<runtime>/busy/` — per-workspace busy-marker JSON files and
    /// their subprocess sidecar PIDs.
    pub fn busy_markers_dir(&self) -> PathBuf {
        self.runtime.join("busy")
    }

    /// `<state>/proposal-requests/` — per-`request_id` state files for
    /// the `chat-request-triage` (propose) flow.
    #[allow(dead_code)]
    pub fn proposal_requests_dir(&self) -> PathBuf {
        self.state.join("proposal-requests")
    }

    /// `<state>/changelog-requests/` — per-`request_id` state files for
    /// the changelog-stylist flow.
    #[allow(dead_code)]
    pub fn changelog_requests_dir(&self) -> PathBuf {
        self.state.join("changelog-requests")
    }

    /// `<state>/failure-state/` — per-repo failure counters keyed by
    /// workspace basename.
    #[allow(dead_code)]
    pub fn failure_state_dir(&self) -> PathBuf {
        self.state.join("failure-state")
    }

    /// `<state>/revisions/` — per-PR reviewer-revision state keyed by
    /// workspace basename.
    #[allow(dead_code)]
    pub fn revisions_dir(&self) -> PathBuf {
        self.state.join("revisions")
    }

    /// `<state>/audit-state/` — per-audit-type cadence + last-run state.
    #[allow(dead_code)]
    pub fn audit_state_dir(&self) -> PathBuf {
        self.state.join("audit-state")
    }

    /// `<state>/alert-state/` — per-workspace alert-throttle state. One
    /// file per workspace, named `<workspace-basename>.json`. The
    /// daemon-wide migration marker `.migration-from-workspace-done`
    /// also lives directly under this directory.
    pub fn alert_state_dir(&self) -> PathBuf {
        self.state.join("alert-state")
    }

    /// `<state>/alert-state/<workspace_basename>.json` — alert-throttle
    /// state file for the named workspace.
    pub fn alert_state_path(&self, workspace_basename: &str) -> PathBuf {
        self.alert_state_dir().join(format!("{workspace_basename}.json"))
    }

    /// `<logs>/runs/<basename>/` — per-change run logs for the named
    /// workspace.
    pub fn run_logs_dir(&self, workspace_basename: &str) -> PathBuf {
        self.logs.join("runs").join(workspace_basename)
    }

    /// `<logs>/runs/<basename>/audits/` — per-invocation audit logs
    /// for the named workspace.
    pub fn audit_logs_dir(&self, workspace_basename: &str) -> PathBuf {
        self.run_logs_dir(workspace_basename).join("audits")
    }

    /// `<state>/iteration-pending/` — root for per-change
    /// iteration-request markers (a27a1). The marker is pure daemon
    /// bookkeeping (written by the polling-loop's `IterationRequested`
    /// arm, read by the prompt-builder + classifier + queue engine,
    /// never operator-edited), so it lives under `<state>/` per a16's
    /// "daemon bookkeeping never appears in the managed repo's working
    /// tree" rule. Earlier a27a1 implementations put the marker at
    /// `<workspace>/openspec/changes/<change>/.iteration-pending.json`;
    /// that location caused `git clean -fd` to wipe the marker on the
    /// next iteration's dirty-workspace recovery, breaking the cap +
    /// continuation-context mechanics. The state-dir location avoids
    /// every git-interaction surface.
    pub fn iteration_pending_dir(&self) -> PathBuf {
        self.state.join("iteration-pending")
    }

    /// `<state>/iteration-pending/<workspace_basename>/` — per-workspace
    /// directory holding one `<change>.json` file per iteration-pending
    /// change in that workspace.
    pub fn iteration_pending_basename_dir(&self, workspace_basename: &str) -> PathBuf {
        self.iteration_pending_dir().join(workspace_basename)
    }

    /// `<state>/iteration-pending/<workspace_basename>/<change>.json` —
    /// the per-change iteration-request marker for `change` in the
    /// workspace identified by `workspace_basename`.
    pub fn iteration_pending_path(
        &self,
        workspace_basename: &str,
        change: &str,
    ) -> PathBuf {
        self.iteration_pending_basename_dir(workspace_basename)
            .join(format!("{change}.json"))
    }

    /// `<cache>/workspaces/` — per-repo cloned workspaces, keyed by
    /// URL-sanitized basename.
    pub fn workspaces_dir(&self) -> PathBuf {
        self.cache.join("workspaces")
    }

    /// `<runtime>/control.sock` — the daemon's control socket.
    pub fn control_socket_path(&self) -> PathBuf {
        self.runtime.join("control.sock")
    }
}

/// Resolve the four daemon paths from the (possibly partial) config
/// override, falling back through env vars → systemd-set vars → XDG
/// defaults → hard fallback. Validates that every path is absolute and
/// that no two paths resolve to the same directory.
pub fn resolve_daemon_paths(config: &Config) -> Result<DaemonPaths> {
    let state = resolve_one(
        "state_dir",
        config.paths.state_dir.as_deref(),
        "AUTOCODER_STATE_DIR",
        "STATE_DIRECTORY",
        xdg_state_default,
        || PathBuf::from("/var/lib/autocoder"),
    )?;
    let cache = resolve_one(
        "cache_dir",
        config.paths.cache_dir.as_deref(),
        "AUTOCODER_CACHE_DIR",
        "CACHE_DIRECTORY",
        xdg_cache_default,
        || PathBuf::from("/var/cache/autocoder"),
    )?;
    let logs = resolve_one(
        "logs_dir",
        config.paths.logs_dir.as_deref(),
        "AUTOCODER_LOGS_DIR",
        "LOGS_DIRECTORY",
        xdg_logs_default,
        || PathBuf::from("/var/log/autocoder"),
    )?;
    let runtime = resolve_one(
        "runtime_dir",
        config.paths.runtime_dir.as_deref(),
        "AUTOCODER_RUNTIME_DIR",
        "RUNTIME_DIRECTORY",
        xdg_runtime_default,
        || PathBuf::from("/run/autocoder"),
    )?;

    let resolved = DaemonPaths {
        state,
        cache,
        logs,
        runtime,
    };

    validate_no_collisions(&resolved)?;
    Ok(resolved)
}

/// Resolve one field through the priority order. Returns the first
/// non-empty value found, rejected with an error if the chosen path is
/// not absolute.
fn resolve_one(
    field_label: &str,
    config_override: Option<&Path>,
    autocoder_env: &str,
    systemd_env: &str,
    xdg_default: impl Fn() -> Option<PathBuf>,
    hard_fallback: impl Fn() -> PathBuf,
) -> Result<PathBuf> {
    // 1. Explicit config override.
    if let Some(p) = config_override
        && !p.as_os_str().is_empty()
    {
        return ensure_absolute(field_label, p.to_path_buf());
    }
    // 2. AUTOCODER_*_DIR env var.
    if let Ok(v) = std::env::var(autocoder_env)
        && !v.is_empty()
    {
        return ensure_absolute(field_label, PathBuf::from(v));
    }
    // 3. systemd-set env var.
    if let Ok(v) = std::env::var(systemd_env)
        && !v.is_empty()
    {
        return ensure_absolute(field_label, PathBuf::from(v));
    }
    // 4. XDG-derived dev-mode default.
    if let Some(p) = xdg_default() {
        return ensure_absolute(field_label, p);
    }
    // 5. Hard fallback.
    let p = hard_fallback();
    tracing::warn!(
        field = field_label,
        path = %p.display(),
        "paths: falling back to hard default; no config, env var, systemd dir, or $HOME found"
    );
    ensure_absolute(field_label, p)
}

fn ensure_absolute(field_label: &str, p: PathBuf) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(anyhow!(
            "paths.{field_label}: path `{}` is not absolute; only absolute paths are accepted",
            p.display()
        ))
    }
}

fn validate_no_collisions(paths: &DaemonPaths) -> Result<()> {
    let labelled: [(&str, &Path); 4] = [
        ("state", &paths.state),
        ("cache", &paths.cache),
        ("logs", &paths.logs),
        ("runtime", &paths.runtime),
    ];
    for i in 0..labelled.len() {
        for j in (i + 1)..labelled.len() {
            if labelled[i].1 == labelled[j].1 {
                return Err(anyhow!(
                    "paths: `{}` and `{}` resolve to the same directory `{}`; \
                     each role needs its own path",
                    labelled[i].0,
                    labelled[j].0,
                    labelled[i].1.display()
                ));
            }
        }
    }
    Ok(())
}

fn xdg_state_default() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("XDG_STATE_HOME")
        && !v.is_empty()
    {
        return Some(PathBuf::from(v).join("autocoder"));
    }
    home_dir().map(|h| h.join(".local").join("state").join("autocoder"))
}

fn xdg_cache_default() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("XDG_CACHE_HOME")
        && !v.is_empty()
    {
        return Some(PathBuf::from(v).join("autocoder"));
    }
    home_dir().map(|h| h.join(".cache").join("autocoder"))
}

fn xdg_logs_default() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("XDG_STATE_HOME")
        && !v.is_empty()
    {
        return Some(PathBuf::from(v).join("autocoder").join("logs"));
    }
    home_dir().map(|h| h.join(".local").join("state").join("autocoder").join("logs"))
}

fn xdg_runtime_default() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("XDG_RUNTIME_DIR")
        && !v.is_empty()
    {
        return Some(PathBuf::from(v).join("autocoder"));
    }
    // Per XDG: when $XDG_RUNTIME_DIR is unset, fall back to a per-UID
    // directory under the system temp. This keeps dev mode functional
    // on hosts that don't run user-level systemd.
    let uid = unsafe { libc::getuid() };
    Some(std::env::temp_dir().join(format!("{uid}-runtime")).join("autocoder"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Create the four daemon directories (mkdir-p, mode 0750). Errors are
/// surfaced — startup should abort if the daemon cannot create its own
/// state/cache/logs/runtime directories.
pub fn ensure_directories(paths: &DaemonPaths) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for (label, dir) in [
        ("state", &paths.state),
        ("cache", &paths.cache),
        ("logs", &paths.logs),
        ("runtime", &paths.runtime),
    ] {
        std::fs::create_dir_all(dir).map_err(|e| {
            anyhow!(
                "paths: failed to create {label} directory `{}`: {e}",
                dir.display()
            )
        })?;
        let mut perms = std::fs::metadata(dir)
            .map_err(|e| {
                anyhow!(
                    "paths: failed to stat {label} directory `{}` after create: {e}",
                    dir.display()
                )
            })?
            .permissions();
        // Tolerate a permissions-set failure (e.g. NFS, exotic FS) by
        // logging — the daemon can still proceed; the mode was a
        // best-effort hardening.
        perms.set_mode(0o750);
        if let Err(e) = std::fs::set_permissions(dir, perms) {
            tracing::warn!(
                label,
                path = %dir.display(),
                "paths: could not set 0750 mode on directory: {e}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Cadence, Config, DaemonPathsConfig, ExecutorConfig, ExecutorKind, GithubConfig,
    };
    use std::sync::Mutex;

    /// Env-var mutation is global; serialize the env-var-touching tests
    /// so concurrent runs do not race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn base_config(paths: DaemonPathsConfig) -> Config {
        let _ = Cadence::Disabled; // touch import for compilers
        Config {
            repositories: vec![],
            executor: ExecutorConfig {
                kind: ExecutorKind::ClaudeCli,
                command: "claude".into(),
                timeout_secs: 60,
                sandbox: None,
                implementer_prompt_path: None,
                changelog_stylist_prompt_path: None,
                perma_stuck_after_failures: None,
                max_changes_per_pr: None,
                startup_jitter_max_secs: None,
                inter_iteration_jitter_pct: None,
                max_auto_revisions_per_pr: 5,
                max_revise_triggers_per_pr: 10,
                wipe_drain_timeout_secs: 30,
                output_format: crate::config::default_output_format(),
                log_retention_days: crate::config::default_log_retention_days(),
                busy_marker_stale_threshold_secs: None,
                change_internal_contradiction_check:
                    crate::config::ContradictionCheckMode::Disabled,
                change_internal_contradiction_check_prompt_path: None,
                change_internal_contradiction_check_llm: None,
                implementer: None,
                changelog_stylist: None,
                implementer_revision: None,
                audit_triage: None,
                chat_request_triage: None,
            },
            github: GithubConfig {
                token_env: "GITHUB_TOKEN".into(),
                token: None,
                owner_tokens: None,
                fork_owner: None,
                recreate_fork_on_reinit: false,
                command_authorization: Default::default(),
            },
            reviewer: None,
            chatops: None,
            audits: None,
            paths,
            features: crate::config::FeaturesConfig::default(),
            canonical_rag: None,
            models: None,
        }
    }

    fn clear_env_vars() {
        for v in [
            "AUTOCODER_STATE_DIR",
            "AUTOCODER_CACHE_DIR",
            "AUTOCODER_LOGS_DIR",
            "AUTOCODER_RUNTIME_DIR",
            "STATE_DIRECTORY",
            "CACHE_DIRECTORY",
            "LOGS_DIRECTORY",
            "RUNTIME_DIRECTORY",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            unsafe { std::env::remove_var(v) };
        }
    }

    #[test]
    fn config_override_wins_over_env_vars() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env_vars();
        unsafe {
            std::env::set_var("AUTOCODER_STATE_DIR", "/env/state");
            std::env::set_var("STATE_DIRECTORY", "/systemd/state");
        }
        let cfg = base_config(DaemonPathsConfig {
            state_dir: Some(PathBuf::from("/cfg/state")),
            cache_dir: Some(PathBuf::from("/cfg/cache")),
            logs_dir: Some(PathBuf::from("/cfg/logs")),
            runtime_dir: Some(PathBuf::from("/cfg/runtime")),
        });
        let p = resolve_daemon_paths(&cfg).unwrap();
        assert_eq!(p.state, PathBuf::from("/cfg/state"));
        assert_eq!(p.cache, PathBuf::from("/cfg/cache"));
        assert_eq!(p.logs, PathBuf::from("/cfg/logs"));
        assert_eq!(p.runtime, PathBuf::from("/cfg/runtime"));
        clear_env_vars();
    }

    #[test]
    fn env_var_wins_over_systemd_var() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env_vars();
        unsafe {
            std::env::set_var("AUTOCODER_STATE_DIR", "/env/state");
            std::env::set_var("AUTOCODER_CACHE_DIR", "/env/cache");
            std::env::set_var("AUTOCODER_LOGS_DIR", "/env/logs");
            std::env::set_var("AUTOCODER_RUNTIME_DIR", "/env/runtime");
            std::env::set_var("STATE_DIRECTORY", "/systemd/state");
            std::env::set_var("CACHE_DIRECTORY", "/systemd/cache");
            std::env::set_var("LOGS_DIRECTORY", "/systemd/logs");
            std::env::set_var("RUNTIME_DIRECTORY", "/systemd/runtime");
        }
        let cfg = base_config(DaemonPathsConfig::default());
        let p = resolve_daemon_paths(&cfg).unwrap();
        assert_eq!(p.state, PathBuf::from("/env/state"));
        assert_eq!(p.cache, PathBuf::from("/env/cache"));
        assert_eq!(p.logs, PathBuf::from("/env/logs"));
        assert_eq!(p.runtime, PathBuf::from("/env/runtime"));
        clear_env_vars();
    }

    #[test]
    fn systemd_var_used_when_no_config_or_env() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env_vars();
        unsafe {
            std::env::set_var("STATE_DIRECTORY", "/var/lib/autocoder");
            std::env::set_var("CACHE_DIRECTORY", "/var/cache/autocoder");
            std::env::set_var("LOGS_DIRECTORY", "/var/log/autocoder");
            std::env::set_var("RUNTIME_DIRECTORY", "/run/autocoder");
        }
        let cfg = base_config(DaemonPathsConfig::default());
        let p = resolve_daemon_paths(&cfg).unwrap();
        assert_eq!(p.state, PathBuf::from("/var/lib/autocoder"));
        assert_eq!(p.cache, PathBuf::from("/var/cache/autocoder"));
        assert_eq!(p.logs, PathBuf::from("/var/log/autocoder"));
        assert_eq!(p.runtime, PathBuf::from("/run/autocoder"));
        clear_env_vars();
    }

    #[test]
    fn xdg_defaults_used_in_dev_mode() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env_vars();
        unsafe {
            std::env::set_var("HOME", "/home/dev");
        }
        let cfg = base_config(DaemonPathsConfig::default());
        let p = resolve_daemon_paths(&cfg).unwrap();
        assert_eq!(p.state, PathBuf::from("/home/dev/.local/state/autocoder"));
        assert_eq!(p.cache, PathBuf::from("/home/dev/.cache/autocoder"));
        assert_eq!(p.logs, PathBuf::from("/home/dev/.local/state/autocoder/logs"));
        // Runtime falls back to system tempdir per-UID even when HOME is
        // set, because $XDG_RUNTIME_DIR is the only XDG var without a
        // user-derived fallback.
        let uid = unsafe { libc::getuid() };
        let expected_runtime = std::env::temp_dir()
            .join(format!("{uid}-runtime"))
            .join("autocoder");
        assert_eq!(p.runtime, expected_runtime);
        clear_env_vars();
    }

    #[test]
    fn relative_config_path_is_rejected() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env_vars();
        let cfg = base_config(DaemonPathsConfig {
            state_dir: Some(PathBuf::from("relative/path")),
            ..Default::default()
        });
        let err = resolve_daemon_paths(&cfg)
            .expect_err("relative path should be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("state_dir"), "error should name the field: {msg}");
        assert!(msg.contains("absolute"), "error should mention absolute: {msg}");
    }

    #[test]
    fn same_path_for_two_roles_is_rejected() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env_vars();
        let cfg = base_config(DaemonPathsConfig {
            state_dir: Some(PathBuf::from("/shared")),
            cache_dir: Some(PathBuf::from("/shared")),
            logs_dir: Some(PathBuf::from("/elsewhere")),
            runtime_dir: Some(PathBuf::from("/somewhere")),
        });
        let err = resolve_daemon_paths(&cfg)
            .expect_err("colliding paths should be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("state"), "error names state: {msg}");
        assert!(msg.contains("cache"), "error names cache: {msg}");
        assert!(msg.contains("/shared"), "error names path: {msg}");
    }

    #[test]
    fn ensure_directories_creates_all_four() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = DaemonPaths::under_root(dir.path());
        ensure_directories(&paths).unwrap();
        for d in [&paths.state, &paths.cache, &paths.logs, &paths.runtime] {
            assert!(d.is_dir(), "{} should be a directory", d.display());
        }
    }

    #[test]
    fn under_root_helper_assembles_paths() {
        let p = DaemonPaths::under_root(Path::new("/tmp/x"));
        assert_eq!(p.state, PathBuf::from("/tmp/x/state"));
        assert_eq!(p.cache, PathBuf::from("/tmp/x/cache"));
        assert_eq!(p.logs, PathBuf::from("/tmp/x/logs"));
        assert_eq!(p.runtime, PathBuf::from("/tmp/x/runtime"));
    }

    /// Every per-state-shape helper composes a fixed subdirectory off
    /// the appropriate root. Regression guard: if a helper is moved to
    /// a different root or renamed, this test flags the change so the
    /// matching docs (STATE-LAYOUT.md) and the audit test's allowlist
    /// can be updated in lock-step.
    #[test]
    fn per_shape_helpers_resolve_under_expected_roots() {
        let p = DaemonPaths {
            state: PathBuf::from("/srv/state"),
            cache: PathBuf::from("/srv/cache"),
            logs: PathBuf::from("/srv/logs"),
            runtime: PathBuf::from("/srv/runtime"),
        };
        assert_eq!(p.audit_threads_dir(), PathBuf::from("/srv/state/audit-threads"));
        assert_eq!(p.busy_markers_dir(), PathBuf::from("/srv/runtime/busy"));
        assert_eq!(
            p.proposal_requests_dir(),
            PathBuf::from("/srv/state/proposal-requests")
        );
        assert_eq!(
            p.changelog_requests_dir(),
            PathBuf::from("/srv/state/changelog-requests")
        );
        assert_eq!(
            p.failure_state_dir(),
            PathBuf::from("/srv/state/failure-state")
        );
        assert_eq!(p.revisions_dir(), PathBuf::from("/srv/state/revisions"));
        assert_eq!(p.audit_state_dir(), PathBuf::from("/srv/state/audit-state"));
        assert_eq!(p.alert_state_dir(), PathBuf::from("/srv/state/alert-state"));
        assert_eq!(
            p.alert_state_path("github_com_owner_repo"),
            PathBuf::from("/srv/state/alert-state/github_com_owner_repo.json")
        );
        assert_eq!(
            p.run_logs_dir("github_com_owner_repo"),
            PathBuf::from("/srv/logs/runs/github_com_owner_repo")
        );
        assert_eq!(
            p.audit_logs_dir("github_com_owner_repo"),
            PathBuf::from("/srv/logs/runs/github_com_owner_repo/audits")
        );
        assert_eq!(
            p.workspaces_dir(),
            PathBuf::from("/srv/cache/workspaces")
        );
        assert_eq!(
            p.control_socket_path(),
            PathBuf::from("/srv/runtime/control.sock")
        );
    }
}
