//! Unix-domain control socket for live daemon interaction. The daemon
//! exposes a `0600`-perm socket at `<system-temp>/autocoder/control/control.sock`
//! and accepts JSON line-delimited requests. The only registered action
//! today is `reload`, which re-reads the YAML config and hot-applies
//! changes to the `github`, `reviewer`, `chatops`, and `repositories`
//! sections. Only the `executor` section requires a process restart.

use crate::chatops::ChatOpsBackend;
use crate::code_reviewer::CodeReviewer;
use crate::config::{
    ChatOpsConfig, Config, GithubConfig, NotificationsConfig, RepositoryConfig, ReviewerConfig,
};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Snapshot of ChatOps runtime state (backend + notification flags +
/// default channel). Held inside an `ArcSwap` so a reload can hot-swap
/// the whole thing atomically; consumers `.load()` once per iteration
/// to read a stable snapshot.
#[derive(Clone)]
pub struct ChatOpsSlot {
    pub backend: Arc<dyn ChatOpsBackend>,
    pub default_channel_id: String,
    pub start_work_enabled: bool,
    pub failure_alerts_enabled: bool,
}

pub type GithubHolder = Arc<ArcSwap<GithubConfig>>;
pub type ReviewerHolder = Arc<ArcSwap<Option<Arc<CodeReviewer>>>>;
pub type ChatOpsHolder = Arc<ArcSwap<Option<ChatOpsSlot>>>;
pub type ConfigHolder = Arc<ArcSwap<Config>>;

/// Handle for a per-repository polling task. The reload handler uses
/// `cancel` to ask one task to exit (without affecting siblings), and
/// `config` to hot-swap the `RepositoryConfig` so a still-running task
/// picks up the new values on its next iteration. `join` is the spawned
/// task's `JoinHandle`; it lets the daemon shutdown path await every
/// per-repo task before exiting.
pub struct RepoTaskHandle {
    pub cancel: CancellationToken,
    pub config: Arc<ArcSwap<RepositoryConfig>>,
    pub join: JoinHandle<()>,
}

/// Daemon-level task registry keyed by repository URL. Mutated only by
/// the reload handler (add/cancel/swap-in-place) and by each polling
/// task's exit hook (remove-self).
pub type RepoTaskMap = Arc<Mutex<HashMap<String, RepoTaskHandle>>>;

/// Outcome of asking the runtime to spawn a polling task for a new repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// A new task was spawned and inserted into the map.
    Spawned,
    /// The URL is already present in the task map (either a live task or
    /// a still-shutting-down one). Caller treats this as "unchanged" so
    /// the response shape stays accurate.
    AlreadyPresent,
    /// The repository's startup check failed (workspace init, dirty
    /// recovery, etc.). The task was not spawned.
    StartupCheckFailed,
}

/// Closure-style hook for spawning a polling task at reload time. The
/// daemon's `execute` function constructs one of these in `cli/run.rs`,
/// capturing the executor, holders, cancellation parent, and thresholds.
/// The reload handler invokes it for every URL it has decided to add.
pub type SpawnRepoFn =
    Arc<dyn Fn(RepositoryConfig) -> SpawnOutcome + Send + Sync>;

/// Handles the control socket task needs to mutate live config + read
/// disk. Constructed once at startup and shared with the listener task.
#[derive(Clone)]
pub struct ControlState {
    pub github: GithubHolder,
    pub reviewer: ReviewerHolder,
    pub chatops: ChatOpsHolder,
    /// The most recently parsed-and-applied `Config`. Reload diffs
    /// against this snapshot; on a successful reload, the snapshot is
    /// swapped to the new value.
    pub last_config: ConfigHolder,
    pub config_path: PathBuf,
    /// Registry of running per-repo polling tasks, keyed by URL.
    pub repo_tasks: RepoTaskMap,
    /// Factory the reload handler uses to spawn a polling task for a
    /// newly-added repository. Captured at daemon startup so the reload
    /// handler doesn't need direct access to executor/holders.
    pub spawn_repo: SpawnRepoFn,
}

/// Canonical control-socket path:
/// `<system-temp>/autocoder/control/control.sock`.
pub fn socket_path() -> PathBuf {
    std::env::temp_dir()
        .join("autocoder")
        .join("control")
        .join("control.sock")
}

/// Bind the listener at the canonical socket path and accept connections
/// until `cancel` fires. Removes the socket file on shutdown.
pub async fn listen(state: ControlState, cancel: CancellationToken) -> Result<()> {
    listen_at(socket_path(), state, cancel).await
}

/// Same as `listen` but binds at an explicit path. Used by tests so
/// parallel runs don't collide on the canonical path.
pub async fn listen_at(
    path: PathBuf,
    state: ControlState,
    cancel: CancellationToken,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating control-socket directory {}", parent.display())
        })?;
    }
    // Stale socket from a previous run is not a startup failure.
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding control socket at {}", path.display()))?;
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(
            "could not chmod control socket {} to 0600: {e}",
            path.display()
        );
    }
    tracing::info!("control socket listening at {}", path.display());

    let state = Arc::new(state);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("control socket: cancellation received; shutting down");
                break;
            }
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, state).await {
                                tracing::warn!("control-socket connection failed: {e:#}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("control-socket accept failed: {e}");
                    }
                }
            }
        }
    }
    if let Err(e) = std::fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                "failed to remove control socket {} on shutdown: {e}",
                path.display()
            );
        }
    }
    Ok(())
}

async fn handle_connection(stream: UnixStream, state: Arc<ControlState>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => return Ok(()),
        Ok(_) => {}
        Err(e) => {
            let resp = json!({"ok": false, "error": format!("read failed: {e}")});
            let _ = write_response(&mut write_half, &resp).await;
            return Ok(());
        }
    }
    let response = dispatch_request(&line, state.as_ref()).await;
    write_response(&mut write_half, &response).await?;
    Ok(())
}

async fn write_response(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    response: &Value,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(response).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    write_half.write_all(&bytes).await?;
    write_half.shutdown().await?;
    Ok(())
}

pub async fn dispatch_request(line: &str, state: &ControlState) -> Value {
    let parsed: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            return json!({"ok": false, "error": format!("malformed JSON: {e}")});
        }
    };
    let action = match parsed.get("action").and_then(|a| a.as_str()) {
        Some(a) => a.to_string(),
        None => {
            return json!({"ok": false, "error": "malformed request: missing `action` field"});
        }
    };
    match action.as_str() {
        "reload" => handle_reload(state).await,
        other => json!({"ok": false, "error": format!("unknown action: {other}")}),
    }
}

/// Read the daemon's config path, parse + validate, diff against the
/// last-applied snapshot, hot-apply safe sections, and return the result.
pub async fn handle_reload(state: &ControlState) -> Value {
    let path = &state.config_path;
    let new_cfg = match Config::load_from(path) {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "ok": false,
                "error": format!("config file {}: {e:#}", path.display()),
            });
        }
    };
    if let Err(e) = crate::workspace::detect_collisions(&new_cfg.repositories) {
        return json!({"ok": false, "error": format!("{e:#}")});
    }
    if let Err(e) = crate::cli::run::validate_github_token_routes(
        &new_cfg.github,
        &new_cfg.repositories,
    ) {
        return json!({"ok": false, "error": format!("{e:#}")});
    }

    let current = state.last_config.load_full();

    let mut applied: Vec<String> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();
    let mut requires_restart: Vec<String> = Vec::new();
    let mut section_errors: Vec<(String, String)> = Vec::new();

    // --- github ---
    if yaml_repr(&current.github) != yaml_repr(&new_cfg.github) {
        state.github.store(Arc::new(new_cfg.github.clone()));
        applied.push("github".to_string());
    } else {
        unchanged.push("github".to_string());
    }

    // --- reviewer ---
    if yaml_repr(&current.reviewer) != yaml_repr(&new_cfg.reviewer) {
        match build_reviewer(new_cfg.reviewer.as_ref()) {
            Ok(slot) => {
                state.reviewer.store(Arc::new(slot));
                applied.push("reviewer".to_string());
            }
            Err(e) => {
                tracing::error!("reload: reviewer reconstruction failed: {e:#}");
                section_errors.push(("reviewer".to_string(), format!("{e:#}")));
            }
        }
    } else {
        unchanged.push("reviewer".to_string());
    }

    // --- chatops ---
    if yaml_repr(&current.chatops) != yaml_repr(&new_cfg.chatops) {
        match build_chatops_slot(new_cfg.chatops.as_ref()).await {
            Ok(slot) => {
                state.chatops.store(Arc::new(slot));
                applied.push("chatops".to_string());
            }
            Err(e) => {
                tracing::error!("reload: chatops reconstruction failed: {e:#}");
                section_errors.push(("chatops".to_string(), format!("{e:#}")));
            }
        }
    } else {
        unchanged.push("chatops".to_string());
    }

    // --- repositories (hot-applied) ---
    // Diff by URL: added/removed are computed from the URL sets; for URLs
    // present in both, compare the full RepositoryConfig (URLs already
    // match, so any difference is in another field).
    let delta = apply_repository_changes(state, &new_cfg.repositories);
    if delta.added.is_empty() && delta.removed.is_empty() && delta.changed.is_empty() {
        unchanged.push("repositories".to_string());
    } else {
        applied.push("repositories".to_string());
    }

    // --- executor (restart-required) ---
    if yaml_repr(&current.executor) != yaml_repr(&new_cfg.executor) {
        requires_restart.push("executor".to_string());
    } else {
        unchanged.push("executor".to_string());
    }

    // Persist the new config snapshot so the next reload diffs against
    // current state.
    state.last_config.store(Arc::new(new_cfg));

    let mut resp = json!({
        "ok": true,
        "applied": applied,
        "requires_restart": requires_restart,
        "unchanged": unchanged,
        "repositories_delta": {
            "added": delta.added,
            "removed": delta.removed,
            "changed": delta.changed,
        },
    });
    if !section_errors.is_empty() {
        let mut errors = serde_json::Map::new();
        for (section, msg) in section_errors {
            errors.insert(section, Value::String(msg));
        }
        resp.as_object_mut()
            .unwrap()
            .insert("section_errors".to_string(), Value::Object(errors));
    }
    resp
}

#[derive(Default, Debug, Clone)]
struct RepositoriesDelta {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
}

/// Apply the repositories-section diff against the live task map.
/// Returns the set of URLs added, removed, and changed-in-place.
///
/// Semantics:
///   - URL in current but not new → `removed`: cancel the per-repo
///     token. The task's exit path removes the map entry.
///   - URL in new but not current → `added`: spawn a fresh polling task.
///     If the URL is somehow still in the map (transient state from a
///     recently-cancelled task that hasn't exited), log WARN and treat
///     as `unchanged` for the response — the next reload (after the
///     in-flight iteration completes) will pick it up cleanly.
///   - URL in both → compare configs (URLs already match, so any
///     difference is in another field). If different AND the existing
///     handle's token is NOT already cancelled, swap the live config
///     holder via `store(Arc::new(new))` so the next iteration picks up
///     the new values. If the existing handle's token IS already
///     cancelled (transient mid-shutdown state), log WARN and skip —
///     report as `unchanged`.
fn apply_repository_changes(
    state: &ControlState,
    new_repos: &[RepositoryConfig],
) -> RepositoriesDelta {
    let mut delta = RepositoriesDelta::default();
    let new_by_url: HashMap<String, &RepositoryConfig> =
        new_repos.iter().map(|r| (r.url.clone(), r)).collect();
    let new_urls: HashSet<String> = new_by_url.keys().cloned().collect();

    // Snapshot current URLs (+ a structural fingerprint per URL for the
    // change-in-place diff). We do this under the lock, then drop the
    // lock before any spawn calls so the spawn closure can re-take it.
    let current_state: Vec<(String, bool, Arc<RepositoryConfig>)> = {
        let guard = state.repo_tasks.lock().unwrap();
        guard
            .iter()
            .map(|(url, handle)| {
                (
                    url.clone(),
                    handle.cancel.is_cancelled(),
                    handle.config.load_full(),
                )
            })
            .collect()
    };
    let current_urls: HashSet<String> =
        current_state.iter().map(|(u, _, _)| u.clone()).collect();

    // 1. Removed: cancel the existing per-repo token. The task exit
    //    path removes the map entry; we do NOT remove it here.
    let mut removed_sorted: Vec<&String> = current_urls.difference(&new_urls).collect();
    removed_sorted.sort();
    for url in removed_sorted {
        let cancel_token = {
            let guard = state.repo_tasks.lock().unwrap();
            guard.get(url).map(|h| h.cancel.clone())
        };
        if let Some(token) = cancel_token {
            tracing::info!(url = %url, "reload: cancelling polling task for removed repository");
            token.cancel();
            delta.removed.push(url.clone());
        }
    }

    // 2. Changed in place: URL still present, other fields differ.
    //    Skip with a WARN if the existing handle's token is already
    //    cancelled (transient mid-shutdown state).
    let mut existing_sorted: Vec<&String> = new_urls.intersection(&current_urls).collect();
    existing_sorted.sort();
    for url in existing_sorted {
        let (_, was_cancelled, current_cfg) = current_state
            .iter()
            .find(|(u, _, _)| *u == **url)
            .cloned()
            .expect("URL came from current_urls intersected with new_urls");
        let new_cfg = new_by_url
            .get(url)
            .copied()
            .expect("URL came from new_urls intersection");
        if yaml_repr(current_cfg.as_ref()) == yaml_repr(new_cfg) {
            // No structural difference. Nothing to do.
            continue;
        }
        if was_cancelled {
            tracing::warn!(
                url = %url,
                "reload: repository is still in the task map but its per-repo cancellation token is set; \
                 in-flight iteration is shutting down — skipping hot-swap on this reload, \
                 retry after the task exits"
            );
            continue;
        }
        // Take the lock just long enough to issue the store. If the
        // task exited between our snapshot and the store, the swap is
        // harmless (the holder's Arc strong references will drop with
        // the task).
        let guard = state.repo_tasks.lock().unwrap();
        if let Some(handle) = guard.get(url) {
            handle.config.store(Arc::new(new_cfg.clone()));
            tracing::info!(url = %url, "reload: hot-swapped repository config");
            delta.changed.push(url.clone());
        }
    }

    // 3. Added: spawn a new task per URL. If the URL is already in the
    //    map (e.g. mid-shutdown of a previously-cancelled task), log
    //    WARN and skip — count as unchanged in the response.
    let mut added_sorted: Vec<&String> = new_urls.difference(&current_urls).collect();
    added_sorted.sort();
    for url in added_sorted {
        let new_cfg = new_by_url
            .get(url)
            .copied()
            .expect("URL came from new_urls difference")
            .clone();
        match (state.spawn_repo)(new_cfg) {
            SpawnOutcome::Spawned => {
                tracing::info!(url = %url, "reload: spawned polling task for added repository");
                delta.added.push(url.clone());
            }
            SpawnOutcome::AlreadyPresent => {
                tracing::warn!(
                    url = %url,
                    "reload: repository is in the new config but already present in the task map; \
                     skipping spawn — likely a transient mid-shutdown state, retry after the prior task exits"
                );
            }
            SpawnOutcome::StartupCheckFailed => {
                tracing::error!(
                    url = %url,
                    "reload: repository startup check failed; not spawning a polling task — \
                     edit YAML and reload again after fixing the workspace"
                );
            }
        }
    }

    delta
}

fn build_reviewer(cfg: Option<&ReviewerConfig>) -> Result<Option<Arc<CodeReviewer>>> {
    match cfg {
        Some(rcfg) if rcfg.enabled => {
            let r = CodeReviewer::from_config(rcfg)
                .context("initializing code reviewer from new config")?;
            Ok(Some(Arc::new(r)))
        }
        _ => Ok(None),
    }
}

async fn build_chatops_slot(cfg: Option<&ChatOpsConfig>) -> Result<Option<ChatOpsSlot>> {
    let Some(co) = cfg else { return Ok(None) };
    let backend = crate::chatops::from_config(co)
        .await
        .context("initializing chatops backend from new config")?;
    Ok(Some(ChatOpsSlot {
        backend,
        default_channel_id: co.default_channel_id.clone(),
        start_work_enabled: NotificationsConfig::start_work_enabled(Some(co)),
        failure_alerts_enabled: NotificationsConfig::failure_alerts_enabled(Some(co)),
    }))
}

/// Structural-equality diff via YAML serialization. Catches changes to
/// nested values (e.g. `SecretSource`) that raw equality would miss.
fn yaml_repr<T: serde::Serialize>(value: &T) -> String {
    serde_yaml::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn write_yaml(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("config.yaml");
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Test-only spawn closure that pretends to start a polling task.
    /// The "task" just parks on its cancellation token and removes its
    /// own map entry on exit, mirroring the production spawn helper's
    /// lifecycle without doing any real work. Lets the reload-handler
    /// tests inspect the task map without depending on real workspaces,
    /// executors, or filesystem state.
    fn fake_spawn(task_map: RepoTaskMap, parent_cancel: CancellationToken) -> SpawnRepoFn {
        Arc::new(move |repo: RepositoryConfig| {
            let url = repo.url.clone();
            let mut guard = task_map.lock().unwrap();
            if guard.contains_key(&url) {
                return SpawnOutcome::AlreadyPresent;
            }
            let child_cancel = parent_cancel.child_token();
            let config: Arc<ArcSwap<RepositoryConfig>> =
                Arc::new(ArcSwap::from_pointee(repo));
            let cancel_for_task = child_cancel.clone();
            let map_for_task = task_map.clone();
            let url_for_task = url.clone();
            let join: JoinHandle<()> = tokio::spawn(async move {
                cancel_for_task.cancelled().await;
                let mut g = map_for_task.lock().unwrap();
                g.remove(&url_for_task);
            });
            guard.insert(
                url,
                RepoTaskHandle {
                    cancel: child_cancel,
                    config,
                    join,
                },
            );
            SpawnOutcome::Spawned
        })
    }

    /// Build a `ControlState` whose task map is seeded with a fake
    /// handle for every repository in `cfg`. The `cancel` token is the
    /// parent of every fake task's child token, so cancelling it tears
    /// down the whole fixture cleanly.
    fn seeded_state(
        config_path: PathBuf,
        cfg: &Config,
        cancel: CancellationToken,
    ) -> ControlState {
        let task_map: RepoTaskMap = Arc::new(Mutex::new(HashMap::new()));
        let spawn = fake_spawn(task_map.clone(), cancel);
        for repo in &cfg.repositories {
            let _ = (spawn)(repo.clone());
        }
        ControlState {
            github: Arc::new(ArcSwap::from_pointee(cfg.github.clone())),
            reviewer: Arc::new(ArcSwap::from_pointee(None)),
            chatops: Arc::new(ArcSwap::from_pointee(None)),
            last_config: Arc::new(ArcSwap::from_pointee(cfg.clone())),
            config_path,
            repo_tasks: task_map,
            spawn_repo: spawn,
        }
    }

    #[test]
    fn socket_path_is_under_temp_autocoder_control() {
        let p = socket_path();
        let s = p.to_string_lossy().to_string();
        assert!(s.contains("autocoder"), "expected `autocoder` in path: {s}");
        assert!(s.contains("control"), "expected `control` in path: {s}");
        assert!(
            s.ends_with("control.sock"),
            "expected `control.sock` suffix: {s}"
        );
    }

    async fn send_request(socket: &Path, action_json: &str) -> serde_json::Value {
        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(action_json.as_bytes()).await.unwrap();
        if !action_json.ends_with('\n') {
            write_half.write_all(b"\n").await.unwrap();
        }
        write_half.shutdown().await.unwrap();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    async fn fixture_listener(
        initial_yaml: &str,
    ) -> (TempDir, PathBuf, ControlState, PathBuf, CancellationToken) {
        let dir = TempDir::new().unwrap();
        let cfg_path = write_yaml(dir.path(), initial_yaml);
        let cfg = Config::load_from(&cfg_path).expect("fixture yaml parses");
        let cancel = CancellationToken::new();
        let state = seeded_state(cfg_path.clone(), &cfg, cancel.clone());
        let socket = dir.path().join("control.sock");
        let listener_state = state.clone();
        let listener_socket = socket.clone();
        let listener_cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = listen_at(listener_socket, listener_state, listener_cancel).await;
        });
        // Wait briefly for the listener to bind.
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (dir, socket, state, cfg_path, cancel)
    }

    /// Inline token in the github block so semantic validation
    /// (`validate_github_token_routes`) succeeds without depending on
    /// process env vars.
    const BASE_YAML: &str = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_with_no_changes_responds_unchanged() {
        let (_dir, socket, _state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        assert!(
            resp["applied"].as_array().unwrap().is_empty(),
            "applied must be empty: {resp}"
        );
        assert!(
            resp["requires_restart"].as_array().unwrap().is_empty(),
            "requires_restart must be empty: {resp}"
        );
        let unchanged: Vec<String> = resp["unchanged"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        for section in ["github", "reviewer", "chatops", "repositories", "executor"] {
            assert!(
                unchanged.contains(&section.to_string()),
                "section `{section}` missing from unchanged: {unchanged:?}"
            );
        }
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_applies_github_changes() {
        let (_dir, socket, state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let new_yaml = BASE_YAML.replace("token_env: GITHUB_TOKEN", "token_env: NEW_TOKEN_VAR");
        std::fs::write(&cfg_path, new_yaml).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let applied: Vec<String> = resp["applied"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            applied.contains(&"github".to_string()),
            "github must be in applied: {applied:?}"
        );
        let now = state.github.load_full();
        assert_eq!(now.token_env, "NEW_TOKEN_VAR");
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_reports_requires_restart_for_executor_change() {
        let (_dir, socket, state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let new_yaml = BASE_YAML.replace(
            "executor:\n  kind: claude_cli",
            "executor:\n  kind: claude_cli\n  timeout_secs: 600",
        );
        std::fs::write(&cfg_path, new_yaml).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let requires_restart: Vec<String> = resp["requires_restart"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            requires_restart.contains(&"executor".to_string()),
            "executor must be in requires_restart: {requires_restart:?}"
        );
        // last_config now reflects the new timeout, but the in-memory
        // executor shared with polling tasks is NOT swapped.
        let snap = state.last_config.load_full();
        assert_eq!(snap.executor.timeout_secs, 600);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_rejected_on_invalid_yaml() {
        let (_dir, socket, _state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        std::fs::write(&cfg_path, "::: not [valid: yaml [[ {{").unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        let err = resp["error"].as_str().unwrap();
        assert!(
            err.to_lowercase().contains("parsing")
                || err.to_lowercase().contains("yaml")
                || err.to_lowercase().contains("expected")
                || err.to_lowercase().contains("did not find"),
            "error must hint at parse failure: {err}"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_rejected_on_validation_failure() {
        let (dir, socket, _state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let collision_path = dir.path().join("shared-ws");
        let new_yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo-a.git"
    local_path: "{shared}"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
  - url: "git@github.com:owner/repo-b.git"
    local_path: "{shared}"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#,
            shared = collision_path.display(),
        );
        std::fs::write(&cfg_path, new_yaml).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        let err = resp["error"].as_str().unwrap();
        assert!(
            err.contains("collision") || err.contains("resolve to"),
            "error must name workspace collision: {err}"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_action_returns_error() {
        let (_dir, socket, _state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let resp = send_request(&socket, r#"{"action":"nonsense"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        let err = resp["error"].as_str().unwrap();
        assert!(err.contains("nonsense"), "error must name action: {err}");
        assert!(
            err.to_lowercase().contains("unknown"),
            "error must say `unknown`: {err}"
        );
        cancel.cancel();
    }

    /// Helper: copy the task map's current URLs into a sorted Vec for
    /// stable assertions.
    fn task_map_urls(state: &ControlState) -> Vec<String> {
        let guard = state.repo_tasks.lock().unwrap();
        let mut urls: Vec<String> = guard.keys().cloned().collect();
        urls.sort();
        urls
    }

    /// Wait up to `timeout_ms` for `pred` to return true, polling every
    /// 10ms. Returns `true` if the predicate became true within the
    /// timeout, otherwise `false`.
    async fn wait_for(timeout_ms: u64, mut pred: impl FnMut() -> bool) -> bool {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if pred() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        pred()
    }

    fn delta_urls(resp: &serde_json::Value, key: &str) -> Vec<String> {
        resp["repositories_delta"][key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn applied_list(resp: &serde_json::Value) -> Vec<String> {
        resp["applied"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn unchanged_list(resp: &serde_json::Value) -> Vec<String> {
        resp["unchanged"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_adds_repository_spawns_task() {
        let (_dir, socket, state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let new_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
  - url: "git@github.com:owner/repo-added.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#;
        std::fs::write(&cfg_path, new_yaml).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let applied = applied_list(&resp);
        assert!(
            applied.contains(&"repositories".to_string()),
            "`repositories` must be in applied: {applied:?}"
        );
        let added = delta_urls(&resp, "added");
        assert_eq!(
            added,
            vec!["git@github.com:owner/repo-added.git".to_string()]
        );
        assert!(
            delta_urls(&resp, "removed").is_empty(),
            "removed must be empty: {resp}"
        );
        assert!(
            delta_urls(&resp, "changed").is_empty(),
            "changed must be empty: {resp}"
        );
        // The new URL must be present in the task map.
        let urls = task_map_urls(&state);
        assert!(
            urls.contains(&"git@github.com:owner/repo-added.git".to_string()),
            "task map must contain added URL: {urls:?}"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_removes_repository_cancels_task() {
        // Start with two repos.
        let two_repo_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo-a.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
  - url: "git@github.com:owner/repo-b.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#;
        let (_dir, socket, state, cfg_path, cancel) = fixture_listener(two_repo_yaml).await;
        // Drop repo-b.
        let new_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo-a.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#;
        std::fs::write(&cfg_path, new_yaml).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let removed = delta_urls(&resp, "removed");
        assert_eq!(
            removed,
            vec!["git@github.com:owner/repo-b.git".to_string()],
            "removed must be exactly repo-b: {resp}"
        );
        // The fake task is parked on its own child token; cancelling
        // makes it exit and remove its map entry. Wait for that.
        let state_ref = state.clone();
        let observed = wait_for(1000, move || {
            !state_ref
                .repo_tasks
                .lock()
                .unwrap()
                .contains_key("git@github.com:owner/repo-b.git")
        })
        .await;
        assert!(
            observed,
            "removed URL's task should have exited and removed its map entry within 1s"
        );
        // repo-a still present.
        let urls = task_map_urls(&state);
        assert_eq!(urls, vec!["git@github.com:owner/repo-a.git".to_string()]);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_changes_repository_settings_in_place() {
        let (_dir, socket, state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        // Change base_branch from main → dev. URL unchanged.
        let new_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: dev
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#;
        std::fs::write(&cfg_path, new_yaml).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let changed = delta_urls(&resp, "changed");
        assert_eq!(
            changed,
            vec!["git@github.com:owner/repo.git".to_string()],
            "changed must contain URL: {resp}"
        );
        assert!(
            delta_urls(&resp, "added").is_empty(),
            "added must be empty: {resp}"
        );
        assert!(
            delta_urls(&resp, "removed").is_empty(),
            "removed must be empty: {resp}"
        );
        // Verify the swap holder now contains base_branch = dev.
        let url = "git@github.com:owner/repo.git";
        let guard = state.repo_tasks.lock().unwrap();
        let handle = guard.get(url).expect("URL still present");
        let snapshot = handle.config.load();
        assert_eq!(snapshot.base_branch, "dev");
        drop(guard);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_repo_url_change_is_remove_plus_add() {
        let (_dir, socket, _state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        // Swap URL X for URL Y.
        let new_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo-new-url.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#;
        std::fs::write(&cfg_path, new_yaml).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let added = delta_urls(&resp, "added");
        let removed = delta_urls(&resp, "removed");
        assert_eq!(
            added,
            vec!["git@github.com:owner/repo-new-url.git".to_string()]
        );
        assert_eq!(
            removed,
            vec!["git@github.com:owner/repo.git".to_string()]
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_executor_change_still_requires_restart() {
        let (_dir, socket, state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let new_yaml = BASE_YAML.replace(
            "executor:\n  kind: claude_cli",
            "executor:\n  kind: claude_cli\n  timeout_secs: 600",
        );
        std::fs::write(&cfg_path, new_yaml).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let requires_restart: Vec<String> = resp["requires_restart"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            requires_restart.contains(&"executor".to_string()),
            "executor must be in requires_restart: {requires_restart:?}"
        );
        // Repositories section unchanged AND it is NOT in requires_restart.
        assert!(
            !requires_restart.contains(&"repositories".to_string()),
            "`repositories` must no longer be in requires_restart after \
             hot-reload-repositories-list lands: {requires_restart:?}"
        );
        let unchanged = unchanged_list(&resp);
        assert!(
            unchanged.contains(&"repositories".to_string()),
            "repositories must be in unchanged since the YAML edit only touched executor: {unchanged:?}"
        );
        let snap = state.last_config.load_full();
        assert_eq!(snap.executor.timeout_secs, 600);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_transient_cancelled_url_is_not_respawned() {
        let (_dir, socket, state, cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let url = "git@github.com:owner/repo.git";
        // Simulate the transient state: the URL is in the task map but
        // its cancellation token is already cancelled (the task is
        // mid-shutdown — finishing its in-flight iteration). The fake
        // spawn helper auto-removes its map entry when cancelled, so to
        // hold the URL in the "cancelled-but-present" state we replace
        // the seeded handle with one whose backing task is parked
        // forever.
        let parked_repo = RepositoryConfig {
            url: url.to_string(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
        };
        {
            let mut guard = state.repo_tasks.lock().unwrap();
            if let Some(prev) = guard.remove(url) {
                // Cancel the auto-spawned task so its wrapper finishes
                // and drops its references. We don't care about its
                // map-removal because we just removed the entry under
                // the lock.
                prev.cancel.cancel();
                prev.join.abort();
            }
            let pre_cancelled = CancellationToken::new();
            pre_cancelled.cancel();
            let parked = tokio::spawn(async {
                std::future::pending::<()>().await;
            });
            guard.insert(
                url.to_string(),
                RepoTaskHandle {
                    cancel: pre_cancelled,
                    config: Arc::new(ArcSwap::from_pointee(parked_repo)),
                    join: parked,
                },
            );
        }
        // Re-write the SAME YAML (URL unchanged). The reload sees the
        // URL in `existing`, but its per-repo token is cancelled →
        // WARN + skip. The URL should NOT appear in added/changed/removed.
        std::fs::write(&cfg_path, BASE_YAML).unwrap();
        let resp = send_request(&socket, r#"{"action":"reload"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let added = delta_urls(&resp, "added");
        let changed = delta_urls(&resp, "changed");
        let removed = delta_urls(&resp, "removed");
        assert!(
            !added.contains(&url.to_string()),
            "transient cancelled URL must not be in added: {resp}"
        );
        assert!(
            !changed.contains(&url.to_string()),
            "transient cancelled URL must not be in changed: {resp}"
        );
        assert!(
            !removed.contains(&url.to_string()),
            "transient cancelled URL must not be in removed: {resp}"
        );
        // No second task was spawned: the map still has exactly one
        // entry (the parked transient one).
        let urls = task_map_urls(&state);
        assert_eq!(
            urls,
            vec![url.to_string()],
            "no second task should have been spawned"
        );
        // Manual teardown: abort the parked task before the runtime
        // shuts down so it doesn't leak.
        {
            let mut guard = state.repo_tasks.lock().unwrap();
            if let Some(h) = guard.remove(url) {
                h.join.abort();
            }
        }
        cancel.cancel();
    }
}
