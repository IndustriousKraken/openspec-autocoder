//! Unix-domain control socket for live daemon interaction. The daemon
//! exposes a `0600`-perm socket at `<system-temp>/autocoder/control/control.sock`
//! and accepts JSON line-delimited requests. The only registered action
//! today is `reload`, which re-reads the YAML config and hot-applies
//! changes to the `github`, `reviewer`, and `chatops` sections.

use crate::chatops::ChatOpsBackend;
use crate::code_reviewer::CodeReviewer;
use crate::config::{
    ChatOpsConfig, Config, GithubConfig, NotificationsConfig, ReviewerConfig,
};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
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

    // --- repositories (restart-required) ---
    if yaml_repr(&current.repositories) != yaml_repr(&new_cfg.repositories) {
        requires_restart.push("repositories".to_string());
    } else {
        unchanged.push("repositories".to_string());
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

    fn empty_state(config_path: PathBuf, cfg: &Config) -> ControlState {
        ControlState {
            github: Arc::new(ArcSwap::from_pointee(cfg.github.clone())),
            reviewer: Arc::new(ArcSwap::from_pointee(None)),
            chatops: Arc::new(ArcSwap::from_pointee(None)),
            last_config: Arc::new(ArcSwap::from_pointee(cfg.clone())),
            config_path,
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
        let state = empty_state(cfg_path.clone(), &cfg);
        let socket = dir.path().join("control.sock");
        let cancel = CancellationToken::new();
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
}
