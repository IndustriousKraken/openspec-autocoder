//! Unix-domain control socket for live daemon interaction. The daemon
//! exposes a `0600`-perm socket at `<system-temp>/autocoder/control/control.sock`
//! and accepts JSON line-delimited requests. The only registered action
//! today is `reload`, which re-reads the YAML config and hot-applies
//! changes to the `github`, `reviewer`, `chatops`, and `repositories`
//! sections. Only the `executor` section requires a process restart.

use crate::alert_state::AlertState;
use crate::busy_marker;
use crate::chatops::ChatOpsBackend;
use crate::chatops::operator_commands::{
    LastIteration, MarkerEntry, RepoStatusResponse, ThrottledAlertEntry,
};
use crate::git;
use crate::github;
use crate::github_credentials;
use crate::code_reviewer::CodeReviewer;
use crate::config::{
    ChatOpsConfig, Config, GithubConfig, NotificationsConfig, RepositoryConfig, ReviewerConfig,
};
use crate::failure_state;
use crate::{queue, workspace};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
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
    pub pr_opened_enabled: bool,
}

pub type GithubHolder = Arc<ArcSwap<GithubConfig>>;
pub type ReviewerHolder = Arc<ArcSwap<Option<Arc<CodeReviewer>>>>;
pub type ChatOpsHolder = Arc<ArcSwap<Option<ChatOpsSlot>>>;
pub type ConfigHolder = Arc<ArcSwap<Config>>;

/// One in-flight chat-driven proposal-request awaiting triage. The
/// chatops dispatcher's `propose` verb appends to
/// `RepoTaskHandle::pending_proposal_requests`; the polling loop drains
/// it at iteration start (alongside the existing revision-request queue,
/// audit-thread `send it` queue, and on-demand audit queue). The full
/// `ProposalRequestState` lives on disk; this in-memory shape carries
/// only the minimum the polling loop needs to look the state up.
///
/// Most fields mirror the on-disk state file so a caller that only has
/// the in-memory queue entry (no disk read) still has the full request
/// context. The polling loop today only reads `request_id` and then
/// loads the state file for the rest — keeping the other fields here
/// is forward-compat shape, not current consumption.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProposalRequest {
    pub request_id: String,
    pub channel: String,
    /// Bot's ack-message ts; the request's lifecycle thread.
    pub thread_ts: String,
    pub operator_user: String,
    pub request_text: String,
    pub submitted_at: chrono::DateTime<chrono::Utc>,
}

/// One in-flight chat-driven changelog-request awaiting stylist run. The
/// chatops dispatcher's `changelog` verb appends to
/// `RepoTaskHandle::pending_changelog_requests`; the polling loop drains
/// it at iteration start. The full `ChangelogRequestState` lives on
/// disk; this in-memory shape carries only what the polling loop needs.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ChangelogRequest {
    pub request_id: String,
    pub repo_url: String,
    pub raw_args: String,
    pub channel: String,
    /// Bot's ack-message ts; the request's lifecycle thread.
    pub lifecycle_thread_ts: String,
    pub submitted_at: chrono::DateTime<chrono::Utc>,
}

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
    /// "Run a canonical-spec rebuild at the next iteration" flag. The
    /// control socket's `rebuild_specs` action sets this to `true`; the
    /// polling loop checks + clears it at iteration start (see
    /// `polling_loop::run`).
    pub pending_rebuild: Arc<std::sync::atomic::AtomicBool>,
    /// Queue of `thread_ts` values awaiting audit-triage execution
    /// (`audit-reply-acts`). The chatops dispatcher's `trigger_audit_action`
    /// handler pushes here when an operator posts `@<bot> send it`; the
    /// polling loop drains the queue at the start of each iteration.
    pub pending_triages: Arc<Mutex<Vec<String>>>,
    /// Queue of audit-type names awaiting on-demand execution
    /// (`chatops-on-demand-audit-trigger`). The chatops `audit` verb and
    /// the CLI `audit run` subcommand push canonical audit-type names
    /// onto this list via the `queue_audit` control-socket action; the
    /// polling loop drains it at the start of each iteration's audit
    /// phase and runs each one unconditionally (bypassing cadence). The
    /// queue is de-duplicated on insert so multiple `audit` commands
    /// before a single iteration collapse to one run.
    pub pending_audit_runs: Arc<Mutex<Vec<String>>>,
    /// Queue of chat-driven proposal requests awaiting triage
    /// (`chat-request-triage`). The chatops dispatcher's `propose` verb
    /// pushes here via the `queue_proposal_request` control-socket
    /// action; the polling loop drains the queue at the start of each
    /// iteration AFTER the revision-loop processing AND the on-demand
    /// audit processing AND BEFORE the pending-change walk. Each entry
    /// keys into the on-disk `ProposalRequestState` file via
    /// `request_id` so a daemon restart between enqueue and drain does
    /// not lose the operator's request.
    pub pending_proposal_requests: Arc<Mutex<Vec<ProposalRequest>>>,
    /// Queue of chat-driven changelog requests awaiting stylist run
    /// (`@<bot> changelog`). The chatops dispatcher's `changelog` verb
    /// pushes here via the `queue_changelog_request` control-socket
    /// action; the polling loop drains the queue at the start of each
    /// iteration AFTER the proposal-request drain AND BEFORE the
    /// pending-change walk. Each entry keys into the on-disk
    /// `ChangelogRequestState` file via `request_id`.
    pub pending_changelog_requests: Arc<Mutex<Vec<ChangelogRequest>>>,
    /// Per-iteration cancel token. The polling loop populates this with
    /// a child of the global cancel at iteration start and clears it
    /// back to `None` at iteration end (via an `IterationGuard` drop).
    /// The `wipe_workspace` control-socket handler fires it to ask the
    /// in-flight iteration to drain cleanly before the workspace is
    /// deleted; firing it does NOT cancel the per-repo polling task —
    /// only the current iteration body sees the cancellation.
    pub iteration_cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// Per-repo `Notify` that fires every time the iteration's per-
    /// iteration cleanup runs. The `wipe_workspace` handler awaits this
    /// after firing `iteration_cancel` so the wipe can run on a quiet
    /// workspace instead of yanking the directory out from under a
    /// live executor subprocess.
    pub iteration_drained: Arc<Notify>,
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
    /// Notify that fires every time `repo_tasks` is mutated (insert /
    /// remove). Both the production spawn closure and the test fixtures
    /// notify after their map writes so consumers can wait on map state
    /// changes without sleep-polling.
    pub repo_tasks_changed: Arc<Notify>,
    /// Factory the reload handler uses to spawn a polling task for a
    /// newly-added repository. Captured at daemon startup so the reload
    /// handler doesn't need direct access to executor/holders.
    pub spawn_repo: SpawnRepoFn,
}

/// Canonical control-socket path: `<runtime_dir>/control.sock`. The
/// runtime dir is resolved from the daemon's `DaemonPaths` (typically
/// `/run/autocoder/` under systemd or `${XDG_RUNTIME_DIR}/autocoder/`
/// in dev mode); reboot-cleared tmpfs is the correct location for a
/// socket that should never outlive the process that owns it.
pub fn socket_path() -> PathBuf {
    crate::paths::current().control_socket_path()
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
    let listener = bind_at(&path)?;
    serve(listener, path, state, cancel).await
}

/// Bind a `UnixListener` at `path` (creating the parent directory and
/// removing any stale socket file first). Returns synchronously once the
/// listener is ready to accept, so test callers can spawn `serve` and
/// know — without polling — that the socket is live.
pub fn bind_at(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating control-socket directory {}", parent.display())
        })?;
    }
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding control socket at {}", path.display()))?;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(
            "could not chmod control socket {} to 0600: {e}",
            path.display()
        );
    }
    tracing::info!("control socket listening at {}", path.display());
    Ok(listener)
}

/// Run the accept loop against an already-bound `listener` until `cancel`
/// fires. Removes the socket file on shutdown.
pub async fn serve(
    listener: UnixListener,
    path: PathBuf,
    state: ControlState,
    cancel: CancellationToken,
) -> Result<()> {
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
        "repo_status" => handle_repo_status(&parsed, state).await,
        "repo_status_all" => handle_repo_status_all(state).await,
        "clear_perma_stuck_marker" => handle_clear_perma_stuck(&parsed, state),
        "clear_revision_marker" => handle_clear_revision(&parsed, state),
        "wipe_workspace" => handle_wipe_workspace(&parsed, state).await,
        "rebuild_specs" => handle_rebuild_specs(&parsed, state).await,
        "trigger_audit_action" => handle_trigger_audit_action(&parsed, state).await,
        "queue_audit" => handle_queue_audit(&parsed, state),
        "queue_proposal_request" => handle_queue_proposal_request(&parsed, state),
        "queue_changelog_request" => handle_queue_changelog_request(&parsed, state),
        other => json!({"ok": false, "error": format!("unknown action: {other}")}),
    }
}

// =====================================================================
// Operator-command action handlers
// =====================================================================

/// Look up the configured repository whose `url` matches `url_arg`. Errors
/// when the URL is unknown to the daemon.
fn find_repo(state: &ControlState, url_arg: &str) -> std::result::Result<RepositoryConfig, String> {
    let cfg = state.last_config.load_full();
    cfg.repositories
        .iter()
        .find(|r| r.url == url_arg)
        .cloned()
        .ok_or_else(|| format!("no repository configured with url `{url_arg}`"))
}

/// Look up the configured repository whose resolved workspace path
/// matches `target` (after canonicalisation when both paths exist). Used
/// by the `queue_audit` action's CLI path so an operator can pass
/// `--workspace <path>` instead of the upstream URL.
fn find_repo_by_workspace(state: &ControlState, target: &Path) -> Option<String> {
    let cfg = state.last_config.load_full();
    let target_canon = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    for repo in cfg.repositories.iter() {
        let ws = workspace::resolve_path(repo);
        if ws == target {
            return Some(repo.url.clone());
        }
        let ws_canon = std::fs::canonicalize(&ws).unwrap_or_else(|_| ws);
        if ws_canon == target_canon {
            return Some(repo.url.clone());
        }
    }
    None
}

/// Render the comma-separated list of `url@workspace_path` pairs the
/// daemon is currently managing. Used in error replies so the operator
/// sees their configured repos.
fn managed_repo_list_for_error(state: &ControlState) -> String {
    let cfg = state.last_config.load_full();
    if cfg.repositories.is_empty() {
        return "(none)".to_string();
    }
    cfg.repositories
        .iter()
        .map(|r| {
            format!(
                "`{}` @ `{}`",
                r.url,
                workspace::resolve_path(r).display()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn require_str(parsed: &Value, field: &str) -> std::result::Result<String, String> {
    parsed
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing `{field}` field"))
}

async fn handle_repo_status(parsed: &Value, state: &ControlState) -> Value {
    let url = match require_str(parsed, "url") {
        Ok(u) => u,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let repo = match find_repo(state, &url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let workspace_path = workspace::resolve_path(&repo);
    let github_cfg = state.github.load_full();
    match build_repo_status(&workspace_path, &repo, &github_cfg).await {
        Ok(resp) => match serde_json::to_value(&resp) {
            Ok(body) => json!({"ok": true, "status": body}),
            Err(e) => json!({"ok": false, "error": format!("serializing status: {e}")}),
        },
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
    }
}

/// Aggregate `repo_status` for every repository currently in the live
/// `repo_tasks` registry — one round trip instead of N. Per-repo failures
/// are caught and recorded in the per-entry `ok` field rather than
/// failing the whole call; the bare-status menu always ships every repo
/// section even if one repo's workspace is mid-failure.
async fn handle_repo_status_all(state: &ControlState) -> Value {
    let repos: Vec<RepositoryConfig> = {
        // Snapshot URLs from the live task registry, then look up each
        // URL in the current config holder so the per-repo
        // RepositoryConfig is the one polling tasks see.
        let urls: Vec<String> = {
            let guard = state.repo_tasks.lock().unwrap();
            guard.keys().cloned().collect()
        };
        let cfg = state.last_config.load_full();
        urls.into_iter()
            .filter_map(|url| {
                cfg.repositories.iter().find(|r| r.url == url).cloned()
            })
            .collect()
    };
    let github_cfg = state.github.load_full();
    let mut results = Vec::with_capacity(repos.len());
    for repo in repos {
        let workspace_path = workspace::resolve_path(&repo);
        let url = repo.url.clone();
        let entry = match build_repo_status(&workspace_path, &repo, &github_cfg).await {
            Ok(resp) => match serde_json::to_value(&resp) {
                Ok(body) => json!({"url": url, "ok": true, "status": body}),
                Err(e) => json!({
                    "url": url,
                    "ok": false,
                    "error": format!("serializing status: {e}"),
                }),
            },
            Err(e) => json!({
                "url": url,
                "ok": false,
                "error": format!("{e:#}"),
            }),
        };
        results.push(entry);
    }
    json!({"ok": true, "results": results})
}

/// Build the `RepoStatusResponse` for one repo by reading the workspace's
/// failure-state, alert-state, marker files, and queue state. Pure
/// filesystem reads + config snapshot, plus one outbound GitHub API call
/// for the "latest PR by the daemon" line. Does not interrogate the live
/// polling-task map for `last_iteration` (no central record exists yet);
/// the field is populated from the most recent failure-state timestamp
/// when available.
///
/// Per the status-enrichment spec, a GitHub or local-git failure is
/// log-and-degrade: the affected field becomes `None` and the reply
/// still ships every other section. An operator hitting `status <repo>`
/// during a GitHub incident still gets the local-state half.
async fn build_repo_status(
    workspace_path: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
) -> Result<RepoStatusResponse> {
    let mut resp = RepoStatusResponse {
        url: repo.url.clone(),
        base_branch: repo.base_branch.clone(),
        agent_branch: repo.agent_branch.clone(),
        ..RepoStatusResponse::default()
    };

    // Currently-busy peek is workspace-relative but does not require the
    // workspace dir to exist (the marker lives under <tempdir>/autocoder/busy),
    // so populate it before the early-return.
    resp.currently_busy = busy_marker::current(workspace_path);

    // Workspace may not exist yet (e.g. a freshly added repo whose initial
    // clone hasn't run). Treat that as "everything empty for the
    // workspace-derived fields" — the URL header + branches + busy-marker
    // peek are still useful, and operators won't see a false error.
    if !workspace_path.is_dir() {
        // Try the GitHub PR call anyway — it does not depend on the local
        // workspace.
        resp.latest_pr = fetch_latest_pr(repo, github_cfg).await;
        return Ok(resp);
    }

    // Last-commit lines: best-effort. On error, log and keep the field
    // as None so the formatter renders `(none)`.
    match git::last_commit_summary(workspace_path, &repo.base_branch) {
        Ok(s) => resp.last_commit_base = s,
        Err(e) => {
            tracing::warn!(
                url = %repo.url,
                branch = %repo.base_branch,
                "status: last_commit_summary failed: {e:#}"
            );
        }
    }
    match git::last_commit_summary(workspace_path, &repo.agent_branch) {
        Ok(s) => resp.last_commit_agent = s,
        Err(e) => {
            tracing::warn!(
                url = %repo.url,
                branch = %repo.agent_branch,
                "status: last_commit_summary failed: {e:#}"
            );
        }
    }

    // Latest PR by the daemon (one outbound GitHub call). Failure is
    // log-and-degrade.
    resp.latest_pr = fetch_latest_pr(repo, github_cfg).await;

    // Marker-excluded changes — pull marked_at + detail from the marker
    // JSON files where possible.
    let (perma_changes, revision_changes) = queue::list_marker_excluded(workspace_path)?;
    for change in perma_changes {
        let marker_path = workspace_path
            .join("openspec/changes")
            .join(&change)
            .join(".perma-stuck.json");
        let (marked_at, detail) = read_perma_marker(&marker_path);
        resp.perma_stuck_changes.push(MarkerEntry {
            change,
            marked_at,
            detail,
        });
    }
    for change in revision_changes {
        let marker_path = workspace_path
            .join("openspec/changes")
            .join(&change)
            .join(".needs-spec-revision.json");
        let marked_at = read_revision_marker(&marker_path);
        resp.revision_marked_changes.push(MarkerEntry {
            change,
            marked_at,
            detail: String::new(),
        });
    }

    // Throttled alerts (category-level + per-change perma-stuck +
    // per-change spec-revision).
    let alert_state = AlertState::load_or_default(workspace_path);
    for (category, entry) in &alert_state.alerts {
        resp.throttled_alerts.push(ThrottledAlertEntry {
            label: category.label().to_string(),
            last_fired_at: entry.last_alerted_at,
            throttle_window_hours: 24,
        });
    }
    for (change, entry) in &alert_state.perma_stuck_alerts {
        resp.throttled_alerts.push(ThrottledAlertEntry {
            label: format!("perma_stuck:{change}"),
            last_fired_at: entry.last_alerted_at,
            throttle_window_hours: 24,
        });
    }
    for (change, entry) in &alert_state.spec_revision_alerts {
        resp.throttled_alerts.push(ThrottledAlertEntry {
            label: format!("spec_revision:{change}"),
            last_fired_at: entry.last_alerted_at,
            throttle_window_hours: 24,
        });
    }

    // Queue snapshot.
    resp.pending_changes = queue::list_pending(workspace_path).unwrap_or_default();
    resp.waiting_changes = queue::list_waiting(workspace_path).unwrap_or_default();

    // Best-effort last-iteration: failure-state's most recent entry
    // gives us a timestamp for "something happened recently"; without a
    // central iteration log there's no archive-vs-failure outcome to
    // report. Skip when there are no failure-state entries (a healthy
    // workspace).
    if let Ok(state) = failure_state::load(workspace_path) {
        if let Some(latest_entry) = state
            .entries
            .values()
            .max_by_key(|e| e.last_failed_at)
        {
            resp.last_iteration = Some(LastIteration {
                finished_at: latest_entry.last_failed_at,
                outcome_summary: format!(
                    "last failure: {}",
                    truncate(&latest_entry.last_reason, 80)
                ),
                next_iteration_estimate: Some(
                    latest_entry.last_failed_at
                        + chrono::Duration::seconds(repo.poll_interval_sec as i64),
                ),
                poll_interval_sec: repo.poll_interval_sec,
            });
        }
    }

    Ok(resp)
}

/// Resolve owner / repo / token for `repo` and call
/// `github::latest_pr_for_head`. Any failure (parse, token-resolve, HTTP)
/// is logged at WARN and converted to `None`. Per spec: the status reply
/// MUST NOT fail because GitHub is rate-limited or briefly down.
async fn fetch_latest_pr(
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
) -> Option<crate::chatops::operator_commands::PrSummary> {
    let (owner, repo_name) = match github::parse_repo_url(&repo.url) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url = %repo.url, "status: parse_repo_url failed: {e:#}");
            return None;
        }
    };
    let token = match github_credentials::resolve_token(github_cfg, &owner) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(url = %repo.url, "status: github token resolve failed: {e:#}");
            return None;
        }
    };
    match github::latest_pr_for_head(
        github::DEFAULT_API_BASE,
        &token,
        &owner,
        &repo_name,
        &repo.agent_branch,
    )
    .await
    {
        Ok(pr) => pr,
        Err(e) => {
            tracing::warn!(url = %repo.url, "status: latest_pr_for_head failed: {e:#}");
            None
        }
    }
}

fn read_perma_marker(path: &Path) -> (DateTime<Utc>, String) {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let marked_at = parsed
        .get("marked_stuck_at")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);
    let count = parsed
        .get("consecutive_failures")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let detail = if count > 0 {
        format!("consecutive_failures: {count}")
    } else {
        String::new()
    };
    (marked_at, detail)
}

fn read_revision_marker(path: &Path) -> DateTime<Utc> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    parsed
        .get("marked_at")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

fn handle_clear_perma_stuck(parsed: &Value, state: &ControlState) -> Value {
    let url = match require_str(parsed, "url") {
        Ok(u) => u,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let change = match require_str(parsed, "change") {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let repo = match find_repo(state, &url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let workspace_path = workspace::resolve_path(&repo);
    match queue::remove_perma_stuck_marker(&workspace_path, &change) {
        Ok(()) => json!({"ok": true, "change": change, "url": url}),
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
    }
}

fn handle_clear_revision(parsed: &Value, state: &ControlState) -> Value {
    let url = match require_str(parsed, "url") {
        Ok(u) => u,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let change = match require_str(parsed, "change") {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let repo = match find_repo(state, &url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let workspace_path = workspace::resolve_path(&repo);
    match queue::remove_revision_marker(&workspace_path, &change) {
        Ok(()) => json!({"ok": true, "change": change, "url": url}),
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
    }
}

/// Idempotent — a missing workspace directory is success (the user wanted
/// it gone, it's gone). Returns the path that was (or would have been)
/// removed in the success response so the chatops reply names a concrete
/// thing.
///
/// Before deleting, the handler signals the per-repo polling task's
/// per-iteration cancel token (when set) and awaits the `iteration_drained`
/// `Notify` so the in-flight executor subprocess exits cleanly instead of
/// losing its CWD to a `remove_dir_all`. The wait is capped by
/// `executor.wipe_drain_timeout_secs`; the deletion runs regardless of
/// whether the drain completed, since the directory is going to be gone
/// either way.
async fn handle_wipe_workspace(parsed: &Value, state: &ControlState) -> Value {
    let url = match require_str(parsed, "url") {
        Ok(u) => u,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let repo = match find_repo(state, &url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let workspace_path = workspace::resolve_path(&repo);
    let display = workspace_path.display().to_string();

    // Look up the per-repo handle's iteration_cancel handle + drained
    // Notify under the briefest possible lock so the lookup never blocks
    // the chatops listener for longer than a hashmap probe + Arc clone.
    let (iter_token, drained_notify): (Option<CancellationToken>, Option<Arc<Notify>>) = {
        let guard = state.repo_tasks.lock().unwrap();
        match guard.get(&url) {
            Some(h) => {
                let token = h.iteration_cancel.lock().unwrap().clone();
                (token, Some(h.iteration_drained.clone()))
            }
            None => (None, None),
        }
    };

    // Drain coordination. The four-outcome decision tree (per the
    // wipe-workspace spec): drained-cleanly / drain-timeout / no-iteration /
    // already-absent. The first two require an in-flight iteration; the
    // third is the "between iterations, just delete" short-circuit; the
    // fourth is the idempotent no-op.
    let drain_timeout_secs = state
        .last_config
        .load_full()
        .executor
        .wipe_drain_timeout_secs_clamped();
    let drain_outcome: String = if let (Some(token), Some(notify)) = (iter_token, drained_notify) {
        let start = std::time::Instant::now();
        // Register interest in the Notify BEFORE firing the cancel so we
        // don't miss the wake. `Notify::notified()` only observes events
        // that fire after the future is created.
        let notified = notify.notified();
        tokio::pin!(notified);
        token.cancel();
        if drain_timeout_secs == 0 {
            // Special case: skip the await entirely. The wipe runs
            // immediately whether the iteration responded or not. Treat
            // as a drain-timeout outcome so the operator's chatops reply
            // still signals "we did not wait" rather than misleadingly
            // claiming a clean drain.
            "drain timeout — iteration may have been stuck".to_string()
        } else {
            match tokio::time::timeout(
                std::time::Duration::from_secs(drain_timeout_secs),
                notified.as_mut(),
            )
            .await
            {
                Ok(()) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    format!("drained cleanly in {elapsed:.1}s")
                }
                Err(_) => {
                    tracing::warn!(
                        url = %url,
                        timeout_secs = drain_timeout_secs,
                        "wipe-workspace drain timeout: the in-flight iteration for `{url}` did not exit \
                         within {drain_timeout_secs}s of the per-iteration cancel signal; \
                         proceeding with the workspace deletion regardless"
                    );
                    "drain timeout — iteration may have been stuck".to_string()
                }
            }
        }
    } else {
        "no iteration in flight".to_string()
    };

    if !workspace_path.exists() {
        // Existing already-absent shape preserved (no behaviour change for
        // operators who scripted against the prior `ok=true,
        // already_absent=true` payload). The drain_outcome is appended.
        return json!({
            "ok": true,
            "path": display,
            "url": url,
            "already_absent": true,
            "drain_outcome": drain_outcome,
        });
    }
    match std::fs::remove_dir_all(&workspace_path) {
        Ok(()) => json!({
            "ok": true,
            "path": display,
            "url": url,
            "already_absent": false,
            "drain_outcome": drain_outcome,
        }),
        Err(e) => json!({
            "ok": false,
            "error": format!("removing {display}: {e}"),
        }),
    }
}

/// Rebuild canonical specs from archive history. Two modes:
///   - `immediate: true`: SIGTERM the running executor (via the busy
///     marker's subprocess sidecar), wait up to 30s, then run the
///     rebuild synchronously and return the report in the response.
///   - `immediate: false`: set `pending_rebuild = true` on the named
///     repo's polling task state and return immediately. The next
///     polling iteration picks up the flag and runs the rebuild
///     instead of the normal queue walk.
async fn handle_rebuild_specs(parsed: &Value, state: &ControlState) -> Value {
    let url = match require_str(parsed, "url") {
        Ok(u) => u,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let immediate = parsed
        .get("immediate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let repo = match find_repo(state, &url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let workspace = workspace::resolve_path(&repo);

    if immediate {
        if let Err(e) =
            crate::cli::sync_specs::coordinate_with_daemon(&workspace, true).await
        {
            return json!({
                "ok": false,
                "error": format!("--immediate coordination failed: {e:#}"),
            });
        }
        match crate::cli::sync_specs::rebuild_canonical(&workspace).await {
            Ok(report) => {
                let report_val = serde_json::to_value(&report).unwrap_or(Value::Null);
                json!({
                    "ok": true,
                    "url": url,
                    "immediate": true,
                    "report": report_val,
                })
            }
            Err(e) => json!({
                "ok": false,
                "error": format!("rebuild failed: {e:#}"),
            }),
        }
    } else {
        // Set the per-repo task's pending_rebuild flag.
        let flag = {
            let guard = state.repo_tasks.lock().unwrap();
            guard.get(&url).map(|h| h.pending_rebuild.clone())
        };
        match flag {
            Some(f) => {
                f.store(true, std::sync::atomic::Ordering::SeqCst);
                json!({
                    "ok": true,
                    "url": url,
                    "immediate": false,
                    "scheduled": true,
                    "poll_interval_sec": repo.poll_interval_sec,
                })
            }
            None => json!({
                "ok": false,
                "error": format!(
                    "no live polling task for `{url}` (daemon may not have spawned it yet)"
                ),
            }),
        }
    }
}

/// Queue an audit-triage run for the repo whose audit produced the
/// thread named by `thread_ts`. Reads the audit-thread state to resolve
/// repo URL + audit type, pushes the `thread_ts` onto the matching
/// `RepoTaskHandle::pending_triages`, and returns the repo's poll
/// interval so the chatops reply can name an ETA.
async fn handle_trigger_audit_action(parsed: &Value, state: &ControlState) -> Value {
    let thread_ts = match require_str(parsed, "thread_ts") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let state_root = crate::audits::threads::default_state_root();
    let audit_state = match crate::audits::threads::read_state(&state_root, &thread_ts) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return json!({
                "ok": false,
                "error": format!(
                    "no audit-thread state for thread_ts `{thread_ts}` (the chatops dispatcher should have caught this earlier)"
                ),
            });
        }
        Err(e) => {
            return json!({"ok": false, "error": format!("reading audit-thread state: {e:#}")});
        }
    };

    let repo = match find_repo(state, &audit_state.repo_url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };

    let queue_slot = {
        let guard = state.repo_tasks.lock().unwrap();
        guard.get(&audit_state.repo_url).map(|h| h.pending_triages.clone())
    };
    let queue = match queue_slot {
        Some(q) => q,
        None => {
            return json!({
                "ok": false,
                "error": format!(
                    "no live polling task for `{}` (daemon may not have spawned it yet)",
                    audit_state.repo_url
                ),
            });
        }
    };
    {
        let mut g = queue.lock().unwrap();
        // De-dup: if the same thread_ts is already queued (e.g. the
        // operator double-clicked `send it`), keep just the one entry.
        if !g.iter().any(|t| t == &thread_ts) {
            g.push(thread_ts.clone());
        }
    }

    json!({
        "ok": true,
        "thread_ts": thread_ts,
        "url": audit_state.repo_url,
        "audit_type": audit_state.audit_type,
        "poll_interval_sec": repo.poll_interval_sec,
    })
}

/// Append `audit_type` to the named repo's `pending_audit_runs` queue
/// so the next polling iteration's audit phase runs it unconditionally
/// (bypassing cadence). De-duplicated: appending a value already in the
/// queue is a no-op (the response still reports success). The request
/// identifies the repo by `url` (chatops verb path) OR by `workspace`
/// (CLI `audit run` path — the daemon does the workspace-to-URL
/// resolution against its configured repo list). The response echoes
/// the canonical `audit_type` and resolved `url` so the chatops/CLI
/// caller can build an ack with the daemon's authoritative names;
/// `poll_interval_sec` lets the caller compute the ETA clause.
fn handle_queue_audit(parsed: &Value, state: &ControlState) -> Value {
    let audit_type = match require_str(parsed, "audit_type") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    // Resolve target URL: explicit `url` wins; otherwise look up by
    // `workspace` path (matched against each configured repo's
    // `workspace::resolve_path`).
    let url = if let Some(u) = parsed.get("url").and_then(|v| v.as_str()) {
        u.to_string()
    } else if let Some(ws) = parsed.get("workspace").and_then(|v| v.as_str()) {
        match find_repo_by_workspace(state, std::path::Path::new(ws)) {
            Some(u) => u,
            None => {
                return json!({
                    "ok": false,
                    "error": format!(
                        "no managed repository found for workspace path `{ws}`; the daemon is managing: {}",
                        managed_repo_list_for_error(state)
                    ),
                });
            }
        }
    } else {
        return json!({"ok": false, "error": "missing `url` or `workspace` field"});
    };
    let repo = match find_repo(state, &url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let queue_slot = {
        let guard = state.repo_tasks.lock().unwrap();
        guard.get(&url).map(|h| h.pending_audit_runs.clone())
    };
    let queue = match queue_slot {
        Some(q) => q,
        None => {
            return json!({
                "ok": false,
                "error": format!(
                    "no live polling task for `{url}` (daemon may not have spawned it yet)"
                ),
            });
        }
    };
    {
        let mut g = queue.lock().unwrap();
        if !g.iter().any(|a| a == &audit_type) {
            g.push(audit_type.clone());
        }
    }
    json!({
        "ok": true,
        "url": url,
        "audit_type": audit_type,
        "poll_interval_sec": repo.poll_interval_sec,
    })
}

/// Queue a chat-driven proposal-request for the repo's next polling
/// iteration. The request was already persisted to disk as a
/// `ProposalRequestState` file by the chatops dispatcher; this handler's
/// job is to look up the repo's live polling-task handle, load the
/// state from disk, and push a `ProposalRequest` onto the handle's
/// `pending_proposal_requests` queue so the polling loop drains it.
///
/// On success returns `{ok: true, url, request_id, poll_interval_sec}`.
/// On any failure (unknown repo, missing state file, etc.) returns
/// `{ok: false, error}` and does NOT enqueue.
fn handle_queue_proposal_request(parsed: &Value, state: &ControlState) -> Value {
    let url = match require_str(parsed, "url") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let request_id = match require_str(parsed, "request_id") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let repo = match find_repo(state, &url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    // Load the on-disk state file the chatops dispatcher just wrote.
    let state_root = crate::proposal_requests::default_state_root();
    let proposal_state =
        match crate::proposal_requests::read_state(&state_root, &url, &request_id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return json!({
                    "ok": false,
                    "error": format!(
                        "no proposal-request state file found for request_id `{request_id}` under repo `{url}`"
                    ),
                });
            }
            Err(e) => {
                return json!({
                    "ok": false,
                    "error": format!("reading proposal-request state: {e:#}")
                });
            }
        };
    let queue_slot = {
        let guard = state.repo_tasks.lock().unwrap();
        guard
            .get(&url)
            .map(|h| h.pending_proposal_requests.clone())
    };
    let queue = match queue_slot {
        Some(q) => q,
        None => {
            return json!({
                "ok": false,
                "error": format!(
                    "no live polling task for `{url}` (daemon may not have spawned it yet)"
                ),
            });
        }
    };
    {
        let mut g = queue.lock().unwrap();
        // De-dup: if the same request_id is somehow queued twice (e.g.
        // chatops retried), keep only one entry.
        if !g.iter().any(|r| r.request_id == request_id) {
            g.push(ProposalRequest {
                request_id: proposal_state.request_id.clone(),
                channel: proposal_state.channel.clone(),
                thread_ts: proposal_state.thread_ts.clone(),
                operator_user: proposal_state.operator_user.clone(),
                request_text: proposal_state.request_text.clone(),
                submitted_at: proposal_state.submitted_at,
            });
        }
    }
    json!({
        "ok": true,
        "url": url,
        "request_id": request_id,
        "poll_interval_sec": repo.poll_interval_sec,
    })
}

/// Queue a chat-driven changelog request for the repo's next polling
/// iteration. The request was already persisted to disk as a
/// `ChangelogRequestState` file by the chatops dispatcher; this
/// handler's job is to look up the repo's live polling-task handle,
/// load the state from disk, and push a `ChangelogRequest` onto the
/// handle's `pending_changelog_requests` queue.
fn handle_queue_changelog_request(parsed: &Value, state: &ControlState) -> Value {
    let url = match require_str(parsed, "url") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let request_id = match require_str(parsed, "request_id") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let repo = match find_repo(state, &url) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let state_root = crate::changelog_requests::default_state_root();
    let changelog_state =
        match crate::changelog_requests::read_state(&state_root, &url, &request_id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return json!({
                    "ok": false,
                    "error": format!(
                        "no changelog-request state file found for request_id `{request_id}` under repo `{url}`"
                    ),
                });
            }
            Err(e) => {
                return json!({
                    "ok": false,
                    "error": format!("reading changelog-request state: {e:#}")
                });
            }
        };
    let queue_slot = {
        let guard = state.repo_tasks.lock().unwrap();
        guard
            .get(&url)
            .map(|h| h.pending_changelog_requests.clone())
    };
    let queue = match queue_slot {
        Some(q) => q,
        None => {
            return json!({
                "ok": false,
                "error": format!(
                    "no live polling task for `{url}` (daemon may not have spawned it yet)"
                ),
            });
        }
    };
    {
        let mut g = queue.lock().unwrap();
        if !g.iter().any(|r| r.request_id == request_id) {
            g.push(ChangelogRequest {
                request_id: changelog_state.request_id.clone(),
                repo_url: changelog_state.repo_url.clone(),
                raw_args: changelog_state.raw_args.clone(),
                channel: changelog_state.channel.clone(),
                lifecycle_thread_ts: changelog_state.lifecycle_thread_ts.clone(),
                submitted_at: changelog_state.submitted_at,
            });
        }
    }
    json!({
        "ok": true,
        "url": url,
        "request_id": request_id,
        "poll_interval_sec": repo.poll_interval_sec,
    })
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
        pr_opened_enabled: NotificationsConfig::pr_opened_enabled(Some(co)),
    }))
}

/// Structural-equality diff via YAML serialization. Catches changes to
/// nested values (e.g. `SecretSource`) that raw equality would miss.
fn yaml_repr<T: serde::Serialize>(value: &T) -> String {
    serde_yml::to_string(value).unwrap_or_default()
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
    fn fake_spawn(
        task_map: RepoTaskMap,
        task_map_changed: Arc<Notify>,
        parent_cancel: CancellationToken,
    ) -> SpawnRepoFn {
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
            let map_changed_for_task = task_map_changed.clone();
            let url_for_task = url.clone();
            let join: JoinHandle<()> = tokio::spawn(async move {
                cancel_for_task.cancelled().await;
                {
                    let mut g = map_for_task.lock().unwrap();
                    g.remove(&url_for_task);
                }
                map_changed_for_task.notify_waiters();
            });
            guard.insert(
                url,
                RepoTaskHandle {
                    cancel: child_cancel,
                    config,
                    join,
                    pending_rebuild: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    pending_triages: Arc::new(Mutex::new(Vec::new())),
                    pending_audit_runs: Arc::new(Mutex::new(Vec::new())),
                    pending_proposal_requests: Arc::new(Mutex::new(Vec::new())),
                    pending_changelog_requests: Arc::new(Mutex::new(Vec::new())),
                    iteration_cancel: Arc::new(Mutex::new(None)),
                    iteration_drained: Arc::new(Notify::new()),
                },
            );
            drop(guard);
            task_map_changed.notify_waiters();
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
        let task_map_changed: Arc<Notify> = Arc::new(Notify::new());
        let spawn = fake_spawn(task_map.clone(), task_map_changed.clone(), cancel);
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
            repo_tasks_changed: task_map_changed,
            spawn_repo: spawn,
        }
    }

    #[test]
    fn socket_path_is_under_runtime_dir() {
        let p = socket_path();
        let s = p.to_string_lossy().to_string();
        assert!(s.contains("autocoder"), "expected `autocoder` in path: {s}");
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
        // Bind synchronously so the test knows — without polling — that the
        // socket is ready to accept connections by the time fixture_listener
        // returns. Spawn only the accept loop.
        let listener = bind_at(&socket).expect("bind control socket");
        let listener_state = state.clone();
        let listener_socket = socket.clone();
        let listener_cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = serve(listener, listener_socket, listener_state, listener_cancel).await;
        });
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

    /// Mode-toggle reload: writing a config that flips
    /// `reviewer.mode` from bundled to per_change rebuilds the live
    /// reviewer slot. The seeded test fixture starts with an empty
    /// reviewer slot (the daemon initializes it at startup outside
    /// `handle_reload`), so the assertion is that the reload-driven
    /// hot-swap populates the slot with the new mode + budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_applies_reviewer_mode_change() {
        let base_with_reviewer = r#"
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
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key:
    value: "sk-fixture"
"#;
        let (_dir, socket, state, cfg_path, cancel) = fixture_listener(base_with_reviewer).await;
        // Operator edits the config to flip mode + raise budget.
        let new_yaml = base_with_reviewer.replace(
            "  api_key:\n    value: \"sk-fixture\"\n",
            "  api_key:\n    value: \"sk-fixture\"\n  mode: per_change\n  prompt_budget_chars: 4000000\n",
        );
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
            applied.contains(&"reviewer".to_string()),
            "reviewer must be in applied: {applied:?}"
        );
        // The hot-swapped reviewer slot sees the new mode + budget.
        {
            let r = state.reviewer.load_full();
            let inner = r
                .as_ref()
                .as_ref()
                .expect("reviewer slot populated by reload");
            assert_eq!(inner.mode(), crate::config::ReviewerMode::PerChange);
            assert_eq!(inner.prompt_budget(), 4_000_000);
        }
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_returns_error_on_unparseable_json() {
        let (_dir, socket, _state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let resp = send_request(&socket, "not-json").await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        let err = resp["error"].as_str().unwrap();
        assert!(
            err.contains("malformed JSON"),
            "error must contain `malformed JSON`: {err}"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_returns_error_when_action_field_missing() {
        let (_dir, socket, _state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        for body in ["{}", r#"{"unrelated":"x"}"#] {
            let resp = send_request(&socket, body).await;
            assert_eq!(
                resp["ok"],
                serde_json::Value::Bool(false),
                "resp for {body}: {resp}"
            );
            let err = resp["error"].as_str().unwrap();
            assert!(
                err.contains("missing"),
                "error must contain `missing` for body {body}: {err}"
            );
            assert!(
                err.contains("action"),
                "error must contain `action` for body {body}: {err}"
            );
            assert!(
                !err.contains("malformed JSON"),
                "missing-action error must be distinguishable from `malformed JSON` for body {body}: {err}"
            );
        }
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

    /// Wait up to `timeout_ms` for `pred` to return true, driven by
    /// `notify`. The caller passes a `Notify` that fires whenever the
    /// underlying state changes; this function only re-evaluates `pred`
    /// in response to a notify, so the test stays event-driven instead of
    /// sleep-polling. The `timeout` is a hard wall-clock cap (the
    /// legitimate "I'd rather fail than hang" use of a timer), not a poll
    /// interval.
    async fn wait_for(
        timeout_ms: u64,
        notify: Arc<Notify>,
        mut pred: impl FnMut() -> bool,
    ) -> bool {
        // Fast path — predicate is already true.
        if pred() {
            return true;
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                return pred();
            }
            // Register interest BEFORE evaluating the predicate so a notify
            // racing with the check is not lost.
            let notified = notify.notified();
            if pred() {
                return true;
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return pred();
            }
        }
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
        // makes it exit and remove its map entry. The fixture fires
        // `repo_tasks_changed` on every map mutation, so we can wait
        // event-driven instead of sleep-polling.
        let state_ref = state.clone();
        let observed = wait_for(1000, state.repo_tasks_changed.clone(), move || {
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

    /// Build YAML for a workspace at an explicit `local_path` so the
    /// operator-command tests don't try to look under /tmp/workspaces.
    fn local_path_yaml(local_path: &Path) -> String {
        format!(
            r#"
repositories:
  - url: "git@github.com:owner/myrepo.git"
    local_path: "{}"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#,
            local_path.display()
        )
    }

    /// Create a workspace fixture with an openspec/changes/<name>/proposal.md
    /// file so `queue::list_pending` includes it.
    fn make_change(workspace: &Path, change: &str) {
        let dir = workspace.join("openspec/changes").join(change);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proposal.md"), "## Why\nfixture\n").unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_perma_stuck_removes_marker_and_returns_ok() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        make_change(&workspace, "a06-foo");
        std::fs::write(
            workspace.join("openspec/changes/a06-foo/.perma-stuck.json"),
            r#"{"change":"a06-foo","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "clear_perma_stuck_marker",
            "url": "git@github.com:owner/myrepo.git",
            "change": "a06-foo",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        assert!(
            !workspace
                .join("openspec/changes/a06-foo/.perma-stuck.json")
                .exists(),
            "marker file should be gone"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_perma_stuck_errors_when_marker_absent() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        make_change(&workspace, "a06-foo");
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "clear_perma_stuck_marker",
            "url": "git@github.com:owner/myrepo.git",
            "change": "a06-foo",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        let err = resp["error"].as_str().unwrap();
        assert!(
            err.contains("no perma-stuck marker"),
            "error must name marker: {err}"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_revision_removes_marker_and_returns_ok() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        make_change(&workspace, "a07-bar");
        std::fs::write(
            workspace.join("openspec/changes/a07-bar/.needs-spec-revision.json"),
            r#"{"change":"a07-bar","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"x"}"#,
        )
        .unwrap();
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "clear_revision_marker",
            "url": "git@github.com:owner/myrepo.git",
            "change": "a07-bar",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        assert!(
            !workspace
                .join("openspec/changes/a07-bar/.needs-spec-revision.json")
                .exists()
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_revision_errors_when_marker_absent() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        make_change(&workspace, "a07-bar");
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "clear_revision_marker",
            "url": "git@github.com:owner/myrepo.git",
            "change": "a07-bar",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_url_returns_error_for_marker_clear() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "clear_perma_stuck_marker",
            "url": "git@github.com:owner/UNKNOWN.git",
            "change": "x",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        let err = resp["error"].as_str().unwrap();
        assert!(err.contains("no repository configured"), "got: {err}");
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wipe_workspace_removes_directory_and_returns_path() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(workspace.join("openspec/changes")).unwrap();
        std::fs::write(workspace.join("dummy.txt"), "x").unwrap();
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        assert!(workspace.exists());
        let req = serde_json::json!({
            "action": "wipe_workspace",
            "url": "git@github.com:owner/myrepo.git",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        assert!(!workspace.exists(), "workspace should have been removed");
        assert_eq!(
            resp["path"].as_str().unwrap(),
            workspace.display().to_string(),
            "response must echo the removed path"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wipe_workspace_is_idempotent_when_directory_absent() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        // Intentionally do NOT create the workspace directory.
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "wipe_workspace",
            "url": "git@github.com:owner/myrepo.git",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(
            resp["ok"],
            serde_json::Value::Bool(true),
            "missing dir must be Ok: {resp}"
        );
        assert_eq!(resp["already_absent"], serde_json::Value::Bool(true));
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wipe_workspace_reports_no_iteration_in_flight_when_handle_unset() {
        // The seeded fixture's per-repo handle has `iteration_cancel: None`
        // (the fake polling task isn't running an actual iteration loop).
        // The wipe handler must short-circuit straight to the deletion and
        // report the "no iteration in flight" outcome.
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "wipe_workspace",
            "url": "git@github.com:owner/myrepo.git",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        assert_eq!(
            resp["drain_outcome"].as_str().unwrap(),
            "no iteration in flight",
            "resp: {resp}"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wipe_workspace_drains_cleanly_when_iteration_responds_quickly() {
        // Plant a per-iteration cancel handle on the fake polling task and
        // arrange for the iteration_drained Notify to fire as soon as the
        // cancel is observed. The wipe handler should record a
        // "drained cleanly in <Xs>" outcome and proceed with the deletion.
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let (_dir, socket, state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let url = "git@github.com:owner/myrepo.git";

        // Install a per-iteration cancel + spawn a tiny task that fires
        // the Notify when the cancel observes a cancellation.
        let (iter_cancel, drained) = {
            let guard = state.repo_tasks.lock().unwrap();
            let h = guard.get(url).expect("seeded handle");
            let token = CancellationToken::new();
            *h.iteration_cancel.lock().unwrap() = Some(token.clone());
            (token, h.iteration_drained.clone())
        };
        let watcher = tokio::spawn(async move {
            iter_cancel.cancelled().await;
            drained.notify_waiters();
        });

        let req = serde_json::json!({
            "action": "wipe_workspace",
            "url": url,
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let outcome = resp["drain_outcome"].as_str().unwrap();
        assert!(
            outcome.starts_with("drained cleanly in "),
            "expected drained-cleanly outcome, got: {outcome}"
        );
        assert!(!workspace.exists(), "workspace must be deleted after drain");

        let _ = watcher.await;
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wipe_workspace_reports_drain_timeout_when_iteration_ignores_cancel() {
        // Plant a per-iteration cancel handle BUT do NOT fire the
        // iteration_drained Notify. The drain must time out and the wipe
        // must run anyway.
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        // Set wipe_drain_timeout_secs: 1 so the timeout fires fast.
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/myrepo.git"
    local_path: "{}"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  wipe_drain_timeout_secs: 1
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#,
            workspace.display()
        );
        let (_dir, socket, state, _cfg_path, cancel) = fixture_listener(&yaml).await;
        let url = "git@github.com:owner/myrepo.git";
        {
            let guard = state.repo_tasks.lock().unwrap();
            let h = guard.get(url).expect("seeded handle");
            *h.iteration_cancel.lock().unwrap() = Some(CancellationToken::new());
        }
        let req = serde_json::json!({
            "action": "wipe_workspace",
            "url": url,
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        assert_eq!(
            resp["drain_outcome"].as_str().unwrap(),
            "drain timeout — iteration may have been stuck",
            "resp: {resp}"
        );
        assert!(!workspace.exists(), "wipe must run regardless of drain");
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wipe_workspace_already_absent_includes_drain_outcome() {
        // Idempotent no-op case: the workspace is already gone AND no
        // iteration is in flight. The response must still carry the
        // (no-iteration) drain_outcome for the chatops formatter.
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("never-created");
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "wipe_workspace",
            "url": "git@github.com:owner/myrepo.git",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        assert_eq!(resp["already_absent"], serde_json::Value::Bool(true));
        assert_eq!(
            resp["drain_outcome"].as_str().unwrap(),
            "no iteration in flight"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repo_status_assembles_marker_alert_and_queue_snapshot() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        make_change(&workspace, "a06-foo");
        make_change(&workspace, "a07-bar");
        make_change(&workspace, "a08-ready");
        // Marker on a06 + a07.
        std::fs::write(
            workspace.join("openspec/changes/a06-foo/.perma-stuck.json"),
            r#"{"change":"a06-foo","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();
        std::fs::write(
            workspace.join("openspec/changes/a07-bar/.needs-spec-revision.json"),
            r#"{"change":"a07-bar","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"x"}"#,
        )
        .unwrap();
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "repo_status",
            "url": "git@github.com:owner/myrepo.git",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let status = &resp["status"];
        assert_eq!(status["url"], "git@github.com:owner/myrepo.git");
        let perma: Vec<String> = status["perma_stuck_changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["change"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(perma, vec!["a06-foo".to_string()]);
        let revision: Vec<String> = status["revision_marked_changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["change"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(revision, vec!["a07-bar".to_string()]);
        // Pending = a08-ready (the others are marker-excluded).
        let pending: Vec<String> = status["pending_changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(pending, vec!["a08-ready".to_string()]);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repo_status_all_aggregates_one_round_trip_per_repo() {
        // Two-repo fixture: the daemon should bundle both per-repo
        // statuses into a single response so the chatops menu only
        // pays one round trip.
        let dir = TempDir::new().unwrap();
        let ws_a = dir.path().join("ws-a");
        let ws_b = dir.path().join("ws-b");
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();
        make_change(&ws_a, "a06-foo");
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/aaa.git"
    local_path: "{}"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
  - url: "git@github.com:owner/bbb.git"
    local_path: "{}"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  token:
    value: "ghp_fixture"
"#,
            ws_a.display(),
            ws_b.display()
        );
        let (_dir, socket, _state, _cfg_path, cancel) = fixture_listener(&yaml).await;
        let resp = send_request(&socket, r#"{"action":"repo_status_all"}"#).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let results = resp["results"]
            .as_array()
            .expect("results must be an array");
        assert_eq!(results.len(), 2, "two repos → two results");
        let urls: Vec<String> = results
            .iter()
            .map(|e| e["url"].as_str().unwrap().to_string())
            .collect();
        assert!(urls.contains(&"git@github.com:owner/aaa.git".to_string()));
        assert!(urls.contains(&"git@github.com:owner/bbb.git".to_string()));
        // Every per-repo entry is ok=true and ships a status payload.
        for entry in results {
            assert_eq!(
                entry["ok"], serde_json::Value::Bool(true),
                "every entry must be ok=true: {entry}"
            );
            assert!(
                entry.get("status").is_some(),
                "every entry must ship `status`: {entry}"
            );
        }
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repo_status_handles_missing_workspace_gracefully() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("never-created");
        let (_dir, socket, _state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let req = serde_json::json!({
            "action": "repo_status",
            "url": "git@github.com:owner/myrepo.git",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        let status = &resp["status"];
        assert!(status["perma_stuck_changes"].as_array().unwrap().is_empty());
        assert!(status["pending_changes"].as_array().unwrap().is_empty());
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_dispatcher_drives_full_flow_through_real_socket() {
        use crate::chatops::operator_commands::{
            ControlSocketSubmitter, OperatorCommandDispatcher, RepoIdentity, Reply,
        };

        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        make_change(&workspace, "a06-foo");
        std::fs::write(
            workspace.join("openspec/changes/a06-foo/.perma-stuck.json"),
            r#"{"change":"a06-foo","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();

        let (_dir, socket, state, _cfg_path, cancel) =
            fixture_listener(&local_path_yaml(&workspace)).await;
        let submitter = ControlSocketSubmitter::new(socket.clone());
        let dispatcher = OperatorCommandDispatcher::new();
        let repos: Vec<RepoIdentity> = state
            .last_config
            .load_full()
            .repositories
            .iter()
            .map(|r| RepoIdentity {
                url: r.url.clone(),
                workspace_path: crate::workspace::resolve_path(r),
            })
            .collect();
        let bot = "<@UBOT>";
        let reply = dispatcher
            .handle_message(
                &format!("{bot} clear-perma-stuck myrepo a06-foo"),
                "C1",
                bot,
                &repos,
                &submitter,
            )
            .await
            .expect("dispatcher must produce a reply");
        let reply_text = match reply {
            Reply::Sync(s) => s,
            other => panic!("expected Sync reply, got {other:?}"),
        };
        assert!(reply_text.starts_with("✓"), "expected success reply: {reply_text}");
        assert!(
            !workspace
                .join("openspec/changes/a06-foo/.perma-stuck.json")
                .exists(),
            "marker must have been removed"
        );
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
            audits: None,
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
                    pending_rebuild: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    pending_triages: Arc::new(Mutex::new(Vec::new())),
                    pending_audit_runs: Arc::new(Mutex::new(Vec::new())),
                    pending_proposal_requests: Arc::new(Mutex::new(Vec::new())),
                    pending_changelog_requests: Arc::new(Mutex::new(Vec::new())),
                    iteration_cancel: Arc::new(Mutex::new(None)),
                    iteration_drained: Arc::new(Notify::new()),
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

    // ---------- queue_audit (chatops-on-demand-audit-trigger) ----------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_audit_appends_to_pending_audit_runs() {
        let (_dir, socket, state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let req = serde_json::json!({
            "action": "queue_audit",
            "url": "git@github.com:owner/repo.git",
            "audit_type": "security_bug_audit",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        assert_eq!(resp["audit_type"], "security_bug_audit");
        assert_eq!(resp["url"], "git@github.com:owner/repo.git");
        assert!(resp["poll_interval_sec"].is_u64(), "poll interval echoed: {resp}");

        // The handle's queue now contains the audit-type name.
        let guard = state.repo_tasks.lock().unwrap();
        let handle = guard.get("git@github.com:owner/repo.git").expect("repo present");
        let q = handle.pending_audit_runs.lock().unwrap();
        assert_eq!(*q, vec!["security_bug_audit".to_string()]);
        drop(q);
        drop(guard);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_audit_is_deduplicated_per_repo() {
        let (_dir, socket, state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let req = serde_json::json!({
            "action": "queue_audit",
            "url": "git@github.com:owner/repo.git",
            "audit_type": "security_bug_audit",
        });
        // First submit.
        let resp1 = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp1["ok"], serde_json::Value::Bool(true));
        // Second submit with the same audit_type → success but no
        // duplicate entry.
        let resp2 = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp2["ok"], serde_json::Value::Bool(true));

        let guard = state.repo_tasks.lock().unwrap();
        let handle = guard.get("git@github.com:owner/repo.git").expect("repo present");
        let q = handle.pending_audit_runs.lock().unwrap();
        assert_eq!(
            *q,
            vec!["security_bug_audit".to_string()],
            "duplicate audit_type must collapse to one entry"
        );
        drop(q);
        drop(guard);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_audit_distinct_types_both_recorded() {
        let (_dir, socket, state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        for at in ["security_bug_audit", "drift_audit"] {
            let req = serde_json::json!({
                "action": "queue_audit",
                "url": "git@github.com:owner/repo.git",
                "audit_type": at,
            });
            let resp = send_request(&socket, &req.to_string()).await;
            assert_eq!(resp["ok"], serde_json::Value::Bool(true), "resp: {resp}");
        }
        let guard = state.repo_tasks.lock().unwrap();
        let handle = guard.get("git@github.com:owner/repo.git").expect("repo present");
        let q = handle.pending_audit_runs.lock().unwrap();
        assert!(q.contains(&"security_bug_audit".to_string()));
        assert!(q.contains(&"drift_audit".to_string()));
        assert_eq!(q.len(), 2);
        drop(q);
        drop(guard);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_audit_unknown_url_returns_error() {
        let (_dir, socket, _state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let req = serde_json::json!({
            "action": "queue_audit",
            "url": "git@github.com:owner/UNKNOWN.git",
            "audit_type": "security_bug_audit",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        let err = resp["error"].as_str().unwrap();
        assert!(err.contains("no repository configured"), "got: {err}");
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_audit_missing_audit_type_field_returns_error() {
        let (_dir, socket, _state, _cfg_path, cancel) = fixture_listener(BASE_YAML).await;
        let req = serde_json::json!({
            "action": "queue_audit",
            "url": "git@github.com:owner/repo.git",
        });
        let resp = send_request(&socket, &req.to_string()).await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false), "resp: {resp}");
        let err = resp["error"].as_str().unwrap();
        assert!(err.contains("audit_type"), "got: {err}");
        cancel.cancel();
    }
}
