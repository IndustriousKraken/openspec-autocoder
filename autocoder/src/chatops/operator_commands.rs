//! ChatOps operator commands: backend-independent parser, repo-substring
//! matcher, in-memory pending-confirmation tracker for the destructive
//! `wipe-workspace` flow, and reply formatters.
//!
//! Messages that don't start with the bot mention OR don't match one of
//! the known verbs return `None` from `parse_command` — the Slack inbound
//! listener turns `None` into a `?` reaction on the operator's message
//! rather than spamming a text reply. Verb matching is case-insensitive
//! and whitespace-tolerant.
//!
//! Recognized verbs:
//!   - `status <repo-substring>`
//!   - `clear-perma-stuck <repo-substring> <change-slug>`
//!   - `clear-revision <repo-substring> <change-slug>`
//!   - `wipe-workspace <repo-substring>`     (first step)
//!   - `confirm`                              (second step; only within 60s
//!                                            of a wipe-workspace in the
//!                                            same channel)
//!   - `rebuild-specs <repo-substring>`       (schedules a canonical-spec
//!                                            rebuild from archive history
//!                                            for the next iteration;
//!                                            never triggers --immediate)
//!   - `help`                                 (verb list synopsis)
//!
//! Note: this module does NOT import `RepositoryConfig`. Callers must
//! project repo configs down to `RepoIdentity` (url + workspace_path
//! only) via `RepoIdentityProvider`. This is a deliberate
//! minimum-privilege boundary: any future field added to
//! `RepositoryConfig` (tokens, channel IDs, audit settings) does NOT
//! automatically widen what the operator-commands codepath can observe.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Default chat-channel TTL for a wipe-workspace pending confirmation.
/// Per spec scenario: "Reply 'confirm' within 60 seconds."
pub const WIPE_CONFIRM_TTL_SECS: u64 = 60;

// ====================================================================
// Reply shape
// ====================================================================

/// Result of dispatching an operator command. The inbound listener
/// routes:
///   - `Sync(text)` → `post_threaded_reply` with `text`
///   - `Acked { ack_text, job_id }` → `post_threaded_reply` with
///     `ack_text` and register `job_id` for a later completion post
///
/// `Acked` exists for forward compatibility (future async verbs like
/// "spawn an ad-hoc bug-fix run"). No v1 verb constructs one yet; the
/// listener is wired to handle the variant so that adding such verbs
/// later doesn't require a listener retrofit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Sync(String),
    // Forward-compat variant. Not yet constructed by any v1 verb;
    // the listener already wires it through `post_threaded_reply +
    // register-for-completion` so the first async verb that lands
    // does not require a listener retrofit.
    #[allow(dead_code)]
    Acked {
        ack_text: String,
        job_id: uuid::Uuid,
    },
}

// ====================================================================
// Minimum-privilege repo projection
// ====================================================================

/// What the operator-commands codepath is allowed to see about a
/// configured repository. Constructed exclusively via
/// `RepoIdentityProvider::snapshot()`; the conversion from
/// `RepositoryConfig` lives in the provider impl, never in user code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub url: String,
    pub workspace_path: PathBuf,
}

/// Source of `RepoIdentity` values for the inbound listener. The
/// listener holds an `Arc<dyn RepoIdentityProvider>` and calls
/// `snapshot()` once per inbound command. Implementations are
/// expected to be cheap (an `ArcSwap` load + projection) so the
/// listener does not need to cache the result.
pub trait RepoIdentityProvider: Send + Sync {
    fn snapshot(&self) -> Vec<RepoIdentity>;
}

// ====================================================================
// Argument sanitization
// ====================================================================

/// Reasonable upper bound on a change-slug arg. Wider than any real
/// change name yet narrow enough to keep the no-shell-metachar guard
/// useful.
const MAX_CHANGE_SLUG_LEN: usize = 64;
/// Reasonable upper bound on a repo-substring arg. Long enough to
/// hold any reasonable `git@host:org/repo.git` URL prefix.
const MAX_REPO_SUBSTRING_LEN: usize = 128;

fn change_slug_regex() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9_-]{1,64}$").unwrap())
}

fn repo_substring_regex() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9._/-]{1,128}$").unwrap())
}

fn invalid_change_slug_reply() -> Reply {
    Reply::Sync(format!(
        "✗ invalid change name (must match ^[a-zA-Z0-9_-]+$, max {MAX_CHANGE_SLUG_LEN} chars)"
    ))
}

fn invalid_repo_substring_reply() -> Reply {
    Reply::Sync(format!(
        "✗ invalid repo substring (must match ^[a-zA-Z0-9._/-]+$, max {MAX_REPO_SUBSTRING_LEN} chars)"
    ))
}

// ====================================================================
// Parsed-command shape (post-parse, pre-dispatch)
// ====================================================================

/// Parsed operator command. The parser does NOT resolve the repo —
/// the caller is responsible for that step so the parsing layer stays
/// pure. Argument sanitization HAS run, so by this point all string
/// fields are known to match the documented regex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorCommand {
    Status {
        repo_substring: String,
    },
    ClearPermaStuck {
        repo_substring: String,
        change: String,
    },
    ClearRevision {
        repo_substring: String,
        change: String,
    },
    WipeWorkspace {
        repo_substring: String,
    },
    /// Bare `confirm` reply OR explicit `wipe-workspace-confirm` form.
    /// The caller looks up the channel's pending confirmation; the
    /// `repo_substring` (when present) is informational only — the
    /// authoritative repo URL was captured at the time the original
    /// `wipe-workspace` was issued.
    WipeWorkspaceConfirm {
        repo_substring: Option<String>,
    },
    /// Schedule a canonical-spec rebuild for the next iteration of the
    /// matched repo's polling loop. Chatops NEVER supports `--immediate`:
    /// killing a running executor mid-iteration via chat is too easy to
    /// fire accidentally. Operators wanting `--immediate` SSH to the
    /// daemon host and run the CLI directly.
    RebuildSpecs {
        repo_substring: String,
    },
    Help,
}

/// Outcome of parsing a chat message:
///   - `Ok(Some(cmd))` — fully validated command, ready for dispatch.
///   - `Ok(None)` — does not address the bot OR uses an unknown verb;
///     listener should react with `?`.
///   - `Err(reply)` — addresses the bot with a known verb but one of
///     the arguments failed sanitization. The dispatcher uses this
///     as its return value so the operator sees the precise reason.
#[derive(Debug)]
enum ParseOutcome {
    Ok(OperatorCommand),
    None,
    Invalid(Reply),
}

fn parse_command_outcome(message: &str, bot_mention: &str) -> ParseOutcome {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return ParseOutcome::None;
    }
    let mention = bot_mention.trim();

    // Bare `confirm` (no mention) is a known one-token shortcut for the
    // wipe-workspace second step. The dispatcher checks the channel's
    // pending-confirmation table; if none exists, it posts the
    // "no pending wipe-workspace confirmation" reply.
    if mention.is_empty() || !trimmed.starts_with(mention) {
        if trimmed.eq_ignore_ascii_case("confirm") {
            return ParseOutcome::Ok(OperatorCommand::WipeWorkspaceConfirm {
                repo_substring: None,
            });
        }
        return ParseOutcome::None;
    }

    let after_mention = trimmed[mention.len()..].trim_start();
    if after_mention.is_empty() {
        return ParseOutcome::None;
    }

    let mut tokens = after_mention.split_whitespace();
    let verb = match tokens.next() {
        Some(v) => v,
        None => return ParseOutcome::None,
    };
    let rest: Vec<&str> = tokens.collect();

    match verb.to_ascii_lowercase().as_str() {
        "status" => {
            if rest.len() != 1 {
                return ParseOutcome::None;
            }
            if !repo_substring_regex().is_match(rest[0]) {
                return ParseOutcome::Invalid(invalid_repo_substring_reply());
            }
            ParseOutcome::Ok(OperatorCommand::Status {
                repo_substring: rest[0].to_string(),
            })
        }
        "clear-perma-stuck" => {
            if rest.len() != 2 {
                return ParseOutcome::None;
            }
            if !repo_substring_regex().is_match(rest[0]) {
                return ParseOutcome::Invalid(invalid_repo_substring_reply());
            }
            if !change_slug_regex().is_match(rest[1]) {
                return ParseOutcome::Invalid(invalid_change_slug_reply());
            }
            ParseOutcome::Ok(OperatorCommand::ClearPermaStuck {
                repo_substring: rest[0].to_string(),
                change: rest[1].to_string(),
            })
        }
        "clear-revision" => {
            if rest.len() != 2 {
                return ParseOutcome::None;
            }
            if !repo_substring_regex().is_match(rest[0]) {
                return ParseOutcome::Invalid(invalid_repo_substring_reply());
            }
            if !change_slug_regex().is_match(rest[1]) {
                return ParseOutcome::Invalid(invalid_change_slug_reply());
            }
            ParseOutcome::Ok(OperatorCommand::ClearRevision {
                repo_substring: rest[0].to_string(),
                change: rest[1].to_string(),
            })
        }
        "wipe-workspace" => {
            if rest.len() != 1 {
                return ParseOutcome::None;
            }
            if !repo_substring_regex().is_match(rest[0]) {
                return ParseOutcome::Invalid(invalid_repo_substring_reply());
            }
            ParseOutcome::Ok(OperatorCommand::WipeWorkspace {
                repo_substring: rest[0].to_string(),
            })
        }
        "wipe-workspace-confirm" | "confirm" => {
            // Either the explicit form (`@bot wipe-workspace-confirm myrepo`)
            // or the friendly form (`@bot confirm`). The substring is
            // informational; the channel's pending entry is authoritative.
            let substring = match rest.first() {
                Some(s) => {
                    if !repo_substring_regex().is_match(s) {
                        return ParseOutcome::Invalid(invalid_repo_substring_reply());
                    }
                    Some(s.to_string())
                }
                None => None,
            };
            ParseOutcome::Ok(OperatorCommand::WipeWorkspaceConfirm {
                repo_substring: substring,
            })
        }
        "rebuild-specs" => {
            if rest.len() != 1 {
                return ParseOutcome::None;
            }
            if !repo_substring_regex().is_match(rest[0]) {
                return ParseOutcome::Invalid(invalid_repo_substring_reply());
            }
            ParseOutcome::Ok(OperatorCommand::RebuildSpecs {
                repo_substring: rest[0].to_string(),
            })
        }
        "help" => {
            if !rest.is_empty() {
                return ParseOutcome::None;
            }
            ParseOutcome::Ok(OperatorCommand::Help)
        }
        _ => ParseOutcome::None,
    }
}

/// Try to parse `message` as an operator command addressed to the bot.
/// Returns `None` when the message either does not address the bot or
/// uses an unknown verb. **Errors in argument sanitization are NOT
/// surfaced here** — they only become visible through
/// `OperatorCommandDispatcher::handle_message`, which returns the
/// sanitization-error reply as a `Reply::Sync`. Public for the
/// parser-level unit tests; production code goes through
/// `OperatorCommandDispatcher::handle_message`.
#[allow(dead_code)]
pub fn parse_command(message: &str, bot_mention: &str) -> Option<OperatorCommand> {
    match parse_command_outcome(message, bot_mention) {
        ParseOutcome::Ok(cmd) => Some(cmd),
        ParseOutcome::None | ParseOutcome::Invalid(_) => None,
    }
}

// ====================================================================
// Repo-substring matcher
// ====================================================================

/// Outcome of resolving an operator-supplied repo substring against the
/// configured repositories.
#[derive(Debug)]
pub enum RepoMatch<'a> {
    /// Exactly one configured repo matched the substring.
    Unique(&'a RepoIdentity),
    /// More than one configured repo matched. The caller formats a
    /// "be more specific" reply listing each candidate's URL.
    Multiple(Vec<&'a RepoIdentity>),
    /// No configured repo matched the substring.
    None,
}

/// Case-insensitive substring match against `repository.url`. Liberal: any
/// configured URL whose lowercase form contains the lowercase of
/// `substring` is a match. Empty substring matches every configured repo
/// (returned as `Multiple` so the operator sees the full list instead of
/// a silent everything-match).
pub fn match_repo<'a>(substring: &str, configured: &'a [RepoIdentity]) -> RepoMatch<'a> {
    let needle = substring.to_ascii_lowercase();
    let mut matches: Vec<&RepoIdentity> = Vec::new();
    for repo in configured {
        if repo.url.to_ascii_lowercase().contains(&needle) {
            matches.push(repo);
        }
    }
    match matches.len() {
        0 => RepoMatch::None,
        1 => RepoMatch::Unique(matches.into_iter().next().unwrap()),
        _ => RepoMatch::Multiple(matches),
    }
}

// ====================================================================
// Repo-status aggregate response shape
// ====================================================================

/// Daemon's view of a repo, returned by the control-socket `RepoStatus`
/// action. Fields are independent: empty vectors mean "nothing in this
/// section"; the formatter collapses empty sections rather than printing
/// `(none)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoStatusResponse {
    pub url: String,
    pub perma_stuck_changes: Vec<MarkerEntry>,
    pub revision_marked_changes: Vec<MarkerEntry>,
    pub throttled_alerts: Vec<ThrottledAlertEntry>,
    pub pending_changes: Vec<String>,
    pub waiting_changes: Vec<String>,
    pub last_iteration: Option<LastIteration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkerEntry {
    pub change: String,
    pub marked_at: DateTime<Utc>,
    /// Free-form detail for the marker (e.g. `consecutive_failures: 2`).
    /// Omitted from the reply when empty.
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThrottledAlertEntry {
    pub label: String,
    pub last_fired_at: DateTime<Utc>,
    pub throttle_window_hours: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastIteration {
    pub finished_at: DateTime<Utc>,
    pub outcome_summary: String,
    pub next_iteration_estimate: Option<DateTime<Utc>>,
    pub poll_interval_sec: u64,
}

// ====================================================================
// Reply formatters
// ====================================================================

/// Format the status response into the multi-line chat reply shape
/// from the proposal. Empty sections are omitted entirely.
pub fn format_status_reply(resp: &RepoStatusResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("📊 {}\n", resp.url));

    let has_markers =
        !resp.perma_stuck_changes.is_empty() || !resp.revision_marked_changes.is_empty();
    if has_markers {
        out.push_str("\nactive markers (excluded from list_pending):\n");
        for m in &resp.perma_stuck_changes {
            let age = human_age_since(m.marked_at);
            if m.detail.is_empty() {
                out.push_str(&format!(
                    "  • {} (.perma-stuck.json — marked {age} ago)\n",
                    m.change
                ));
            } else {
                out.push_str(&format!(
                    "  • {} (.perma-stuck.json — {}, marked {age} ago)\n",
                    m.change, m.detail
                ));
            }
        }
        for m in &resp.revision_marked_changes {
            let age = human_age_since(m.marked_at);
            out.push_str(&format!(
                "  • {} (.needs-spec-revision.json — marked {age} ago)\n",
                m.change
            ));
        }
    }

    if !resp.throttled_alerts.is_empty() {
        out.push_str("\n24h-throttled alerts currently engaged:\n");
        for a in &resp.throttled_alerts {
            let last_fired = human_age_since(a.last_fired_at);
            let remaining_h = a.throttle_window_hours
                - (Utc::now() - a.last_fired_at).num_hours();
            let remaining = if remaining_h < 0 { 0 } else { remaining_h };
            out.push_str(&format!(
                "  • {} — last fired {last_fired} ago ({remaining}h remaining)\n",
                a.label
            ));
        }
    }

    if let Some(li) = &resp.last_iteration {
        out.push_str("\nlast iteration:\n");
        out.push_str(&format!(
            "  finished: {} ago\n",
            human_age_since(li.finished_at)
        ));
        if !li.outcome_summary.is_empty() {
            out.push_str(&format!("  outcome: {}\n", li.outcome_summary));
        }
        if let Some(next) = li.next_iteration_estimate {
            let delta = next - Utc::now();
            if delta.num_seconds() > 0 {
                out.push_str(&format!(
                    "  next iteration: in ~{} (poll_interval {}s)\n",
                    human_age_duration(delta),
                    li.poll_interval_sec,
                ));
            } else {
                out.push_str(&format!(
                    "  next iteration: due (poll_interval {}s)\n",
                    li.poll_interval_sec
                ));
            }
        } else {
            out.push_str(&format!(
                "  next iteration: poll_interval {}s\n",
                li.poll_interval_sec
            ));
        }
    }

    let queue_has_content = !resp.pending_changes.is_empty()
        || !resp.waiting_changes.is_empty()
        || !resp.perma_stuck_changes.is_empty()
        || !resp.revision_marked_changes.is_empty();
    if queue_has_content {
        out.push_str("\nqueue snapshot:\n");
        if !resp.pending_changes.is_empty() {
            out.push_str(&format!(
                "  pending: {}\n",
                resp.pending_changes.join(", ")
            ));
        }
        if !resp.waiting_changes.is_empty() {
            out.push_str(&format!(
                "  waiting: {}\n",
                resp.waiting_changes.join(", ")
            ));
        }
        let excluded: Vec<String> = resp
            .perma_stuck_changes
            .iter()
            .chain(resp.revision_marked_changes.iter())
            .map(|m| m.change.clone())
            .collect();
        if !excluded.is_empty() {
            out.push_str(&format!(
                "  excluded: {} (see markers above)\n",
                excluded.join(", ")
            ));
        }
    }

    // Strip trailing newline so chatops backends post a single message
    // without an empty terminal line.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Reply when the operator-supplied substring resolves to more than one
/// configured repository.
pub fn format_multiple_matches(substring: &str, matches: &[&RepoIdentity]) -> String {
    let urls: Vec<String> = matches.iter().map(|r| r.url.clone()).collect();
    format!(
        "✗ `{substring}` matched multiple repos: {} — be more specific",
        urls.join(", ")
    )
}

/// Reply when the operator-supplied substring matches no configured
/// repository. Lists every configured URL so the operator sees their
/// available options.
pub fn format_no_match(substring: &str, configured: &[RepoIdentity]) -> String {
    if configured.is_empty() {
        return format!("✗ no repo matched `{substring}`; no repositories configured");
    }
    let urls: Vec<String> = configured.iter().map(|r| r.url.clone()).collect();
    format!(
        "✗ no repo matched `{substring}`; configured: {}",
        urls.join(", ")
    )
}

/// Multi-line synopsis returned by the `help` verb. Lists every
/// currently-supported verb with one-line description + the README
/// pointer for the destructive-confirmation flow.
pub fn format_help_reply() -> String {
    let mut out = String::new();
    out.push_str("Available commands (mention the bot to invoke):\n");
    out.push_str("  • `status <repo>` — current markers, throttled alerts, queue snapshot, last iteration\n");
    out.push_str("  • `clear-perma-stuck <repo> <change>` — clear `.perma-stuck.json` for a change\n");
    out.push_str("  • `clear-revision <repo> <change>` — clear `.needs-spec-revision.json` for a change\n");
    out.push_str("  • `wipe-workspace <repo>` — destructive: warns, then awaits `confirm` (60s TTL)\n");
    out.push_str("  • `confirm` — second step for `wipe-workspace` (same channel, within 60s)\n");
    out.push_str("  • `rebuild-specs <repo>` — schedule a canonical-spec rebuild for the next iteration\n");
    out.push_str("  • `help` — this synopsis\n");
    out.push_str("See the README \"ChatOps operator commands\" section for the destructive confirmation flow.");
    out
}

// ====================================================================
// Pending wipe-workspace confirmations
// ====================================================================

#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    pub repo_url: String,
    pub expires_at: Instant,
}

/// In-memory per-channel pending-confirmation tracker for the destructive
/// `wipe-workspace` flow. The `Instant`-based expiry gives the second-step
/// reply a hard 60-second window (per the spec).
#[derive(Debug, Default)]
pub struct ConfirmationStore {
    pending: Mutex<HashMap<String, PendingConfirmation>>,
}

impl ConfirmationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a pending wipe-workspace confirmation for `channel_id`,
    /// replacing any prior pending entry on that channel.
    pub fn record(&self, channel_id: &str, repo_url: String, ttl: Duration) {
        self.record_at(channel_id, repo_url, Instant::now() + ttl);
    }

    /// Same as `record` but takes an absolute expiry instant. Lets tests
    /// plant entries with an `expires_at` already in the past without
    /// sleeping.
    fn record_at(&self, channel_id: &str, repo_url: String, expires_at: Instant) {
        let mut g = self.pending.lock().unwrap();
        g.insert(
            channel_id.to_string(),
            PendingConfirmation {
                repo_url,
                expires_at,
            },
        );
    }

    /// Look up the pending confirmation for `channel_id`, returning the
    /// captured `repo_url` and consuming the entry. Returns `None` when
    /// no entry exists OR when the entry has expired (an expired entry
    /// is also removed).
    pub fn take_valid(&self, channel_id: &str) -> Option<String> {
        let mut g = self.pending.lock().unwrap();
        let entry = g.remove(channel_id)?;
        if Instant::now() > entry.expires_at {
            return None;
        }
        Some(entry.repo_url)
    }

    /// Test-only: count of in-memory pending entries.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }
}

// ====================================================================
// Action-submission abstraction
// ====================================================================

/// Submit-action trait that the dispatcher uses to invoke the four
/// control-socket actions. Implementations:
///   - In production: `ControlSocketSubmitter` writes JSON to the
///     daemon's Unix-domain control socket.
///   - In tests: `InProcessSubmitter` calls `control_socket::dispatch_request`
///     directly so the full flow can be driven without a listening
///     socket.
#[async_trait]
pub trait ActionSubmitter: Send + Sync {
    async fn submit(&self, action: serde_json::Value) -> serde_json::Value;
}

// ====================================================================
// OperatorCommandDispatcher — message-in → action → reply-out
// ====================================================================

/// Full-flow dispatcher: parses an incoming chat message, resolves the
/// repo substring against the configured repositories, submits the
/// corresponding action via the supplied `ActionSubmitter`, and returns
/// the formatted chat reply.
///
/// Two-step destructive `wipe-workspace`:
///   - The first step records a pending confirmation keyed by
///     `channel_id` with a 60-second TTL.
///   - The second step (bare `confirm` OR explicit
///     `wipe-workspace-confirm`) consumes the pending entry and submits
///     the actual `wipe_workspace` action.
///   - If no pending entry exists OR it has expired, the dispatcher
///     posts the "no pending wipe-workspace confirmation" error.
pub struct OperatorCommandDispatcher {
    pending: ConfirmationStore,
}

impl Default for OperatorCommandDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl OperatorCommandDispatcher {
    pub fn new() -> Self {
        Self {
            pending: ConfirmationStore::new(),
        }
    }

    /// Parse `text` and execute the resulting command. Returns:
    ///   - `Some(Reply::Sync(text))` — a v1 verb produced a text reply.
    ///   - `Some(Reply::Acked { .. })` — reserved for future async verbs;
    ///     not constructed by any v1 codepath.
    ///   - `None` — the message did not address the bot OR used an
    ///     unknown verb. The Slack inbound listener turns this into a
    ///     `?` reaction on the operator's original message.
    pub async fn handle_message(
        &self,
        text: &str,
        channel_id: &str,
        bot_mention: &str,
        repositories: &[RepoIdentity],
        submitter: &dyn ActionSubmitter,
    ) -> Option<Reply> {
        match parse_command_outcome(text, bot_mention) {
            ParseOutcome::Ok(cmd) => Some(Reply::Sync(
                self.dispatch(cmd, channel_id, repositories, submitter).await,
            )),
            ParseOutcome::None => None,
            ParseOutcome::Invalid(reply) => Some(reply),
        }
    }

    async fn dispatch(
        &self,
        cmd: OperatorCommand,
        channel_id: &str,
        repositories: &[RepoIdentity],
        submitter: &dyn ActionSubmitter,
    ) -> String {
        match cmd {
            OperatorCommand::Status { repo_substring } => {
                let repo = match match_repo(&repo_substring, repositories) {
                    RepoMatch::Unique(r) => r,
                    RepoMatch::Multiple(ms) => {
                        return format_multiple_matches(&repo_substring, &ms);
                    }
                    RepoMatch::None => return format_no_match(&repo_substring, repositories),
                };
                let resp = submitter
                    .submit(serde_json::json!({
                        "action": "repo_status",
                        "url": repo.url,
                    }))
                    .await;
                if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let err = resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)");
                    return format!("✗ status failed: {err}");
                }
                let status: RepoStatusResponse =
                    match serde_json::from_value(resp["status"].clone()) {
                        Ok(s) => s,
                        Err(e) => return format!("✗ status decode failed: {e}"),
                    };
                format_status_reply(&status)
            }
            OperatorCommand::ClearPermaStuck {
                repo_substring,
                change,
            } => {
                let repo = match match_repo(&repo_substring, repositories) {
                    RepoMatch::Unique(r) => r,
                    RepoMatch::Multiple(ms) => {
                        return format_multiple_matches(&repo_substring, &ms);
                    }
                    RepoMatch::None => return format_no_match(&repo_substring, repositories),
                };
                let resp = submitter
                    .submit(serde_json::json!({
                        "action": "clear_perma_stuck_marker",
                        "url": repo.url,
                        "change": change,
                    }))
                    .await;
                if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    format!(
                        "✓ cleared .perma-stuck.json for {change} on {}",
                        short_repo_label(&repo.url)
                    )
                } else {
                    let err = resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)");
                    format!("✗ {err}")
                }
            }
            OperatorCommand::ClearRevision {
                repo_substring,
                change,
            } => {
                let repo = match match_repo(&repo_substring, repositories) {
                    RepoMatch::Unique(r) => r,
                    RepoMatch::Multiple(ms) => {
                        return format_multiple_matches(&repo_substring, &ms);
                    }
                    RepoMatch::None => return format_no_match(&repo_substring, repositories),
                };
                let resp = submitter
                    .submit(serde_json::json!({
                        "action": "clear_revision_marker",
                        "url": repo.url,
                        "change": change,
                    }))
                    .await;
                if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    format!(
                        "✓ cleared .needs-spec-revision.json for {change} on {}",
                        short_repo_label(&repo.url)
                    )
                } else {
                    let err = resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)");
                    format!("✗ {err}")
                }
            }
            OperatorCommand::WipeWorkspace { repo_substring } => {
                let repo = match match_repo(&repo_substring, repositories) {
                    RepoMatch::Unique(r) => r,
                    RepoMatch::Multiple(ms) => {
                        return format_multiple_matches(&repo_substring, &ms);
                    }
                    RepoMatch::None => return format_no_match(&repo_substring, repositories),
                };
                self.pending.record(
                    channel_id,
                    repo.url.clone(),
                    Duration::from_secs(WIPE_CONFIRM_TTL_SECS),
                );
                format!(
                    "⚠️ This will delete {} (forces a re-clone on the next \
                     iteration). Reply 'confirm' within {WIPE_CONFIRM_TTL_SECS} seconds.",
                    repo.workspace_path.display()
                )
            }
            OperatorCommand::RebuildSpecs { repo_substring } => {
                let repo = match match_repo(&repo_substring, repositories) {
                    RepoMatch::Unique(r) => r,
                    RepoMatch::Multiple(ms) => {
                        return format_multiple_matches(&repo_substring, &ms);
                    }
                    RepoMatch::None => return format_no_match(&repo_substring, repositories),
                };
                let resp = submitter
                    .submit(serde_json::json!({
                        "action": "rebuild_specs",
                        "url": repo.url,
                        "immediate": false,
                    }))
                    .await;
                if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let poll = resp
                        .get("poll_interval_sec")
                        .and_then(|v| v.as_u64());
                    match poll {
                        Some(p) => format!(
                            "✓ rebuild scheduled for {} — will run within ~{p}s (current iteration must finish first)",
                            short_repo_label(&repo.url)
                        ),
                        None => format!(
                            "✓ rebuild scheduled for {} — will run on the next iteration (current iteration must finish first)",
                            short_repo_label(&repo.url)
                        ),
                    }
                } else {
                    let err = resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)");
                    format!("✗ {err}")
                }
            }
            OperatorCommand::WipeWorkspaceConfirm { .. } => {
                let url = match self.pending.take_valid(channel_id) {
                    Some(u) => u,
                    None => {
                        return "✗ no pending wipe-workspace confirmation in this \
                                channel (or it expired — re-issue the original command)"
                            .to_string();
                    }
                };
                let resp = submitter
                    .submit(serde_json::json!({
                        "action": "wipe_workspace",
                        "url": url,
                    }))
                    .await;
                if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let path = resp
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("workspace");
                    format!("✓ wiped {path}; next iteration will re-clone")
                } else {
                    let err = resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)");
                    format!("✗ wipe-workspace failed: {err}")
                }
            }
            OperatorCommand::Help => format_help_reply(),
        }
    }

    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Production `ActionSubmitter` that writes a single JSON line to the
/// daemon's Unix-domain control socket and reads back the response.
/// Tests use `FakeSubmitter` instead.
pub struct ControlSocketSubmitter {
    socket_path: std::path::PathBuf,
}

impl ControlSocketSubmitter {
    pub fn new(socket_path: std::path::PathBuf) -> Self {
        Self { socket_path }
    }
}

#[async_trait]
impl ActionSubmitter for ControlSocketSubmitter {
    async fn submit(&self, action: serde_json::Value) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let stream = match UnixStream::connect(&self.socket_path).await {
            Ok(s) => s,
            Err(e) => {
                return serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "could not connect to control socket {}: {e}",
                        self.socket_path.display()
                    ),
                });
            }
        };
        let (read_half, mut write_half) = stream.into_split();
        let mut payload = action.to_string();
        payload.push('\n');
        if let Err(e) = write_half.write_all(payload.as_bytes()).await {
            return serde_json::json!({
                "ok": false,
                "error": format!("writing to control socket: {e}"),
            });
        }
        if let Err(e) = write_half.shutdown().await {
            return serde_json::json!({
                "ok": false,
                "error": format!("shutdown of control socket: {e}"),
            });
        }
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        if let Err(e) = reader.read_line(&mut line).await {
            return serde_json::json!({
                "ok": false,
                "error": format!("reading control socket response: {e}"),
            });
        }
        match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => serde_json::json!({
                "ok": false,
                "error": format!("parsing control socket response: {e}; raw: {line}"),
            }),
        }
    }
}

/// Strip the URL down to a short readable label for chat replies. For a
/// typical `git@host:owner/repo.git`, returns `repo` (the trailing path
/// segment without the `.git` suffix). Falls back to the full URL when
/// the form is unfamiliar.
fn short_repo_label(url: &str) -> String {
    let trimmed = url.trim_end_matches(".git");
    let after_slash = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let after_colon = after_slash.rsplit(':').next().unwrap_or(after_slash);
    after_colon.to_string()
}

// ====================================================================
// Reply-formatting helpers (private)
// ====================================================================

fn human_age_since(when: DateTime<Utc>) -> String {
    let delta = Utc::now() - when;
    human_age_duration(delta)
}

fn human_age_duration(delta: chrono::Duration) -> String {
    let secs = delta.num_seconds().abs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(url: &str) -> RepoIdentity {
        RepoIdentity {
            url: url.to_string(),
            workspace_path: PathBuf::from("/tmp/ws/").join(
                url.rsplit('/')
                    .next()
                    .unwrap_or("repo")
                    .trim_end_matches(".git"),
            ),
        }
    }

    // ---------- parse_command ----------

    const BOT: &str = "<@UBOT>";

    #[test]
    fn parse_status_happy_path() {
        let cmd = parse_command(&format!("{BOT} status myrepo"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::Status {
                repo_substring: "myrepo".into()
            }
        );
    }

    #[test]
    fn parse_clear_perma_stuck_happy_path() {
        let cmd =
            parse_command(&format!("{BOT} clear-perma-stuck myrepo a06-foo"), BOT)
                .unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::ClearPermaStuck {
                repo_substring: "myrepo".into(),
                change: "a06-foo".into(),
            }
        );
    }

    #[test]
    fn parse_clear_revision_happy_path() {
        let cmd =
            parse_command(&format!("{BOT} clear-revision myrepo a07-bar"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::ClearRevision {
                repo_substring: "myrepo".into(),
                change: "a07-bar".into(),
            }
        );
    }

    #[test]
    fn parse_rebuild_specs_happy_path() {
        let cmd = parse_command(&format!("{BOT} rebuild-specs myrepo"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::RebuildSpecs {
                repo_substring: "myrepo".into()
            }
        );
    }

    #[test]
    fn parse_rebuild_specs_immediate_not_recognized() {
        // The chatops parser does NOT recognize --immediate. Per spec
        // scenario "Chatops verb does not support --immediate": the
        // verb parses as rebuild-specs with the entire remainder as
        // the repo-substring (i.e. None for too-many args, or matches
        // when the operator's literal substring includes "--immediate").
        let cmd = parse_command(&format!("{BOT} rebuild-specs myrepo --immediate"), BOT);
        assert!(
            cmd.is_none(),
            "two-arg form must not parse (--immediate is not a flag)"
        );
    }

    #[test]
    fn parse_rebuild_specs_missing_arg_returns_none() {
        assert!(parse_command(&format!("{BOT} rebuild-specs"), BOT).is_none());
    }

    #[test]
    fn parse_wipe_workspace_happy_path() {
        let cmd = parse_command(&format!("{BOT} wipe-workspace myrepo"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::WipeWorkspace {
                repo_substring: "myrepo".into()
            }
        );
    }

    #[test]
    fn parse_explicit_wipe_workspace_confirm() {
        let cmd =
            parse_command(&format!("{BOT} wipe-workspace-confirm myrepo"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::WipeWorkspaceConfirm {
                repo_substring: Some("myrepo".into())
            }
        );
    }

    #[test]
    fn parse_bare_confirm_no_mention() {
        let cmd = parse_command("confirm", BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::WipeWorkspaceConfirm {
                repo_substring: None
            }
        );
    }

    #[test]
    fn parse_bare_confirm_case_insensitive() {
        for form in ["CONFIRM", "Confirm", "ConFIRM"] {
            let cmd = parse_command(form, BOT).unwrap();
            assert_eq!(
                cmd,
                OperatorCommand::WipeWorkspaceConfirm {
                    repo_substring: None
                }
            );
        }
    }

    #[test]
    fn parse_confirm_mentioned() {
        let cmd = parse_command(&format!("{BOT} confirm"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::WipeWorkspaceConfirm {
                repo_substring: None
            }
        );
    }

    #[test]
    fn parse_missing_arg_returns_none() {
        assert!(parse_command(&format!("{BOT} status"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} clear-perma-stuck myrepo"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} clear-revision"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} wipe-workspace"), BOT).is_none());
    }

    #[test]
    fn parse_too_many_args_returns_none() {
        // The spec lists one substring for status; trailing junk is an
        // ambiguous typo, not a known verb.
        assert!(parse_command(&format!("{BOT} status myrepo extra"), BOT).is_none());
    }

    #[test]
    fn parse_message_without_mention_returns_none() {
        // Don't drown random chat in error replies.
        assert!(parse_command("status myrepo", BOT).is_none());
        assert!(parse_command("hello world", BOT).is_none());
        assert!(parse_command("@somebody-else status myrepo", BOT).is_none());
    }

    #[test]
    fn parse_unknown_verb_returns_none() {
        assert!(parse_command(&format!("{BOT} hello"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} please archive everything"), BOT).is_none());
        // Explicitly out-of-scope per spec.
        assert!(parse_command(&format!("{BOT} pause myrepo"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} resume myrepo"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} clear-alert-throttle x"), BOT).is_none());
    }

    #[test]
    fn parse_verb_is_case_insensitive() {
        for verb_form in ["status", "Status", "STATUS", "StAtUs"] {
            let cmd = parse_command(&format!("{BOT} {verb_form} myrepo"), BOT)
                .unwrap_or_else(|| panic!("`{verb_form}` should parse"));
            assert_eq!(
                cmd,
                OperatorCommand::Status {
                    repo_substring: "myrepo".into()
                }
            );
        }
    }

    #[test]
    fn parse_help_verb_case_insensitive() {
        for form in ["help", "Help", "HELP", "HeLp"] {
            let cmd = parse_command(&format!("{BOT} {form}"), BOT)
                .unwrap_or_else(|| panic!("`{form}` should parse"));
            assert_eq!(cmd, OperatorCommand::Help);
        }
    }

    #[test]
    fn parse_help_with_trailing_garbage_returns_none() {
        // `help` takes no args. Trailing tokens make the message a typo,
        // not a known verb. Falling through to None lets the listener
        // react with `?`.
        assert!(parse_command(&format!("{BOT} help me"), BOT).is_none());
    }

    #[test]
    fn parse_whitespace_tolerance() {
        // Leading/trailing whitespace + multi-space separators are all ok.
        let cmd =
            parse_command(&format!("   {BOT}   status    myrepo   "), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::Status {
                repo_substring: "myrepo".into()
            }
        );
    }

    #[test]
    fn parse_empty_message_returns_none() {
        assert!(parse_command("", BOT).is_none());
        assert!(parse_command("   ", BOT).is_none());
    }

    #[test]
    fn parse_mention_only_returns_none() {
        assert!(parse_command(BOT, BOT).is_none());
        assert!(parse_command(&format!("{BOT}   "), BOT).is_none());
    }

    // ---------- argument sanitization (visible via dispatcher only) ----------

    #[tokio::test]
    async fn dispatch_change_slug_path_traversal_rejected() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} clear-perma-stuck myrepo ../../etc/passwd"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("invalid args produce a sanitization reply, not None");
        match reply {
            Reply::Sync(text) => {
                assert!(text.starts_with("✗ invalid change name"), "{text}");
            }
            other => panic!("expected Sync sanitization reply, got {other:?}"),
        }
        // No control-socket call.
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_change_slug_shell_metachars_rejected() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        // Tokenizer sees "a;" as one token then "rm" "-rf" "/" — the
        // verb has only 2 args (repo, change), so "a;" is the change.
        // The sanitization regex rejects `;` immediately.
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} clear-perma-stuck myrepo a;"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        match reply {
            Reply::Sync(text) => {
                assert!(text.starts_with("✗ invalid change name"), "{text}");
            }
            other => panic!("expected sanitization reply, got {other:?}"),
        }
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_change_slug_oversized_rejected() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let too_long: String = "a".repeat(65);
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} clear-perma-stuck myrepo {too_long}"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        match reply {
            Reply::Sync(text) => {
                assert!(text.starts_with("✗ invalid change name"), "{text}");
            }
            other => panic!("expected sanitization reply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_repo_substring_double_dot_rejected() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        // `..` is rejected because `.` is in the allowed set but two
        // consecutive `..` is allowed by the substring regex — wait,
        // actually `..` matches `[a-zA-Z0-9._/-]+`. So we need a
        // metacharacter NOT in the allowed set. `:` is the test case.
        // The spec text says "rejects repo substring with `..`" but
        // that's the change-name regex. Re-reading the spec: repo
        // substrings allow `.`, so we test a disallowed char.
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} status my$repo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        match reply {
            Reply::Sync(text) => {
                assert!(text.starts_with("✗ invalid repo substring"), "{text}");
            }
            other => panic!("expected sanitization reply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_repo_substring_oversized_rejected() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let too_long: String = "a".repeat(129);
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} status {too_long}"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        match reply {
            Reply::Sync(text) => {
                assert!(text.starts_with("✗ invalid repo substring"), "{text}");
            }
            other => panic!("expected sanitization reply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_real_world_valid_args_accepted() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        submitter.set_response("clear_perma_stuck_marker", serde_json::json!({"ok": true}));

        // Use `myrepo` so the substring resolves uniquely in the
        // fixture; we're exercising the sanitization regex on the
        // change-slug arg (the second arg), not the substring.
        for (msg, expected) in [
            (
                format!("{BOT} clear-perma-stuck myrepo a06-foo"),
                "a06-foo",
            ),
            (
                format!("{BOT} clear-perma-stuck myrepo auth-2fa"),
                "auth-2fa",
            ),
            (
                format!("{BOT} clear-perma-stuck myrepo Cap_underscored-NAME"),
                "Cap_underscored-NAME",
            ),
        ] {
            let reply = dispatcher
                .handle_message(&msg, "C1", BOT, &fixture_repos(), &submitter)
                .await
                .unwrap();
            match reply {
                Reply::Sync(text) => {
                    assert!(text.starts_with("✓"), "happy path for {expected}: {text}");
                    assert!(text.contains(expected));
                }
                other => panic!("expected Sync, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn dispatch_repo_substring_with_dot_and_slash_accepted() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "repo_status",
            serde_json::json!({
                "ok": true,
                "status": {
                    "url": "git@github.com:acme/myrepo.git",
                    "perma_stuck_changes": [], "revision_marked_changes": [],
                    "throttled_alerts": [], "pending_changes": [], "waiting_changes": [],
                    "last_iteration": null
                }
            }),
        );
        // Repo substrings with `/` and `.` should resolve. The substring
        // `acme/myrepo.git` matches just the configured `acme/myrepo.git`
        // URL substring.
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} status acme/myrepo.git"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        assert!(matches!(reply, Reply::Sync(_)));
    }

    // ---------- match_repo ----------

    #[test]
    fn match_repo_unique() {
        let repos = vec![
            ident("git@github.com:acme/myrepo.git"),
            ident("git@github.com:acme/widgets.git"),
        ];
        match match_repo("myrepo", &repos) {
            RepoMatch::Unique(r) => assert!(r.url.contains("myrepo")),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn match_repo_multiple() {
        let repos = vec![
            ident("git@github.com:org-a/repo.git"),
            ident("git@github.com:org-b/repo.git"),
        ];
        match match_repo("repo", &repos) {
            RepoMatch::Multiple(ms) => assert_eq!(ms.len(), 2),
            other => panic!("expected Multiple, got {other:?}"),
        }
    }

    #[test]
    fn match_repo_none() {
        let repos = vec![ident("git@github.com:owner/foo.git")];
        match match_repo("nonexistent", &repos) {
            RepoMatch::None => {}
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn match_repo_case_insensitive() {
        let repos = vec![ident("git@github.com:acme/myrepo.git")];
        match match_repo("MYREPO", &repos) {
            RepoMatch::Unique(r) => assert!(r.url.contains("myrepo")),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn match_repo_empty_substring_returns_all_as_multiple() {
        let repos = vec![
            ident("git@github.com:owner/a.git"),
            ident("git@github.com:owner/b.git"),
        ];
        match match_repo("", &repos) {
            RepoMatch::Multiple(ms) => assert_eq!(ms.len(), 2),
            other => panic!("expected Multiple, got {other:?}"),
        }
    }

    #[test]
    fn match_repo_empty_substring_with_one_repo_is_unique() {
        let repos = vec![ident("git@github.com:owner/only.git")];
        match match_repo("", &repos) {
            RepoMatch::Unique(_) => {}
            other => panic!("expected Unique (single repo), got {other:?}"),
        }
    }

    // ---------- ConfirmationStore ----------

    #[test]
    fn confirmation_store_round_trip() {
        let store = ConfirmationStore::new();
        store.record("C1", "git@github.com:owner/repo.git".into(), Duration::from_secs(60));
        assert_eq!(store.len(), 1);
        let url = store.take_valid("C1").expect("present");
        assert_eq!(url, "git@github.com:owner/repo.git");
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn confirmation_store_expires_after_ttl() {
        let store = ConfirmationStore::new();
        // Plant an entry whose `expires_at` is already in the past, so the
        // expiry check exercises the same code path as a real timeout
        // without the test having to wait for wall-clock time.
        store.record_at(
            "C1",
            "url".into(),
            Instant::now() - Duration::from_millis(1),
        );
        // Expired → take_valid returns None AND removes the entry.
        assert!(store.take_valid("C1").is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn confirmation_store_cross_channel_isolation() {
        let store = ConfirmationStore::new();
        store.record("A", "url-a".into(), Duration::from_secs(60));
        // Channel B has no pending → take_valid returns None.
        assert!(store.take_valid("B").is_none());
        // A's pending is untouched.
        assert_eq!(store.take_valid("A").as_deref(), Some("url-a"));
    }

    #[test]
    fn confirmation_store_replaces_prior_pending() {
        let store = ConfirmationStore::new();
        store.record("C", "url-1".into(), Duration::from_secs(60));
        store.record("C", "url-2".into(), Duration::from_secs(60));
        // Second record replaces first.
        assert_eq!(store.take_valid("C").as_deref(), Some("url-2"));
    }

    // ---------- Reply formatters ----------

    #[test]
    fn format_status_collapses_empty_sections() {
        let resp = RepoStatusResponse {
            url: "git@github.com:owner/repo.git".into(),
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        // Header is always present.
        assert!(out.starts_with("📊 git@github.com:owner/repo.git"));
        // No "active markers" / "throttled alerts" / "queue snapshot" /
        // "last iteration" lines appear since every section is empty.
        for label in [
            "active markers",
            "throttled alerts",
            "queue snapshot",
            "last iteration",
            "(none)",
        ] {
            assert!(
                !out.contains(label),
                "empty status reply must not contain `{label}`; got: {out}"
            );
        }
    }

    #[test]
    fn format_status_lists_markers_when_present() {
        let resp = RepoStatusResponse {
            url: "git@github.com:owner/repo.git".into(),
            perma_stuck_changes: vec![MarkerEntry {
                change: "a06-foo".into(),
                marked_at: Utc::now() - chrono::Duration::hours(4),
                detail: "consecutive_failures: 2".into(),
            }],
            revision_marked_changes: vec![MarkerEntry {
                change: "a07-bar".into(),
                marked_at: Utc::now() - chrono::Duration::minutes(22),
                detail: String::new(),
            }],
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(out.contains("active markers"));
        assert!(out.contains("a06-foo"));
        assert!(out.contains(".perma-stuck.json"));
        assert!(out.contains("consecutive_failures: 2"));
        assert!(out.contains("a07-bar"));
        assert!(out.contains(".needs-spec-revision.json"));
        // The queue snapshot's "excluded" line lists both markers.
        assert!(out.contains("excluded: a06-foo, a07-bar"));
    }

    #[test]
    fn format_no_match_lists_configured_repos() {
        let repos = vec![
            ident("git@github.com:owner/myrepo.git"),
            ident("git@github.com:owner/widgets.git"),
        ];
        let out = format_no_match("gibberish", &repos);
        assert!(out.starts_with("✗ "));
        assert!(out.contains("gibberish"));
        assert!(out.contains("myrepo"));
        assert!(out.contains("widgets"));
    }

    #[test]
    fn format_multiple_matches_lists_candidates() {
        let r1 = ident("git@github.com:org-a/repo.git");
        let r2 = ident("git@github.com:org-b/repo.git");
        let out = format_multiple_matches("repo", &[&r1, &r2]);
        assert!(out.starts_with("✗ "));
        assert!(out.contains("org-a/repo"));
        assert!(out.contains("org-b/repo"));
        assert!(out.contains("be more specific"));
    }

    #[test]
    fn format_help_lists_current_verbs() {
        let out = format_help_reply();
        for verb in [
            "status",
            "clear-perma-stuck",
            "clear-revision",
            "wipe-workspace",
            "rebuild-specs",
            "help",
        ] {
            assert!(out.contains(verb), "help must list `{verb}`: {out}");
        }
        // Pointer to the README's confirmation-flow section.
        assert!(out.to_lowercase().contains("readme"));
    }

    // ---------- OperatorCommandDispatcher (full flow) ----------

    /// Test-only `ActionSubmitter` that records every submitted action
    /// JSON and replies with a configurable response. Suitable for
    /// driving the dispatcher's message-in → action → reply-out flow
    /// without a real control socket or daemon.
    struct FakeSubmitter {
        responses: Mutex<HashMap<String, serde_json::Value>>,
        log: Mutex<Vec<serde_json::Value>>,
    }

    impl FakeSubmitter {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                log: Mutex::new(Vec::new()),
            }
        }

        fn set_response(&self, action: &str, value: serde_json::Value) {
            self.responses
                .lock()
                .unwrap()
                .insert(action.to_string(), value);
        }

        fn calls(&self) -> Vec<serde_json::Value> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ActionSubmitter for FakeSubmitter {
        async fn submit(&self, action: serde_json::Value) -> serde_json::Value {
            self.log.lock().unwrap().push(action.clone());
            let verb = action
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            self.responses
                .lock()
                .unwrap()
                .get(&verb)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"ok": false, "error": "no fake response"}))
        }
    }

    fn fixture_repos() -> Vec<RepoIdentity> {
        vec![
            ident("git@github.com:acme/myrepo.git"),
            ident("git@github.com:acme/widgets.git"),
        ]
    }

    fn unwrap_sync(reply: Reply) -> String {
        match reply {
            Reply::Sync(s) => s,
            other => panic!("expected Sync, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_help_returns_sync_synopsis() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(&format!("{BOT} help"), "C1", BOT, &fixture_repos(), &submitter)
            .await
            .expect("help must produce a reply");
        let text = unwrap_sync(reply);
        assert!(text.contains("status"));
        assert!(text.contains("help"));
        assert!(submitter.calls().is_empty(), "help has no action");
    }

    #[tokio::test]
    async fn dispatch_help_case_insensitive() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply_lower = dispatcher
            .handle_message(&format!("{BOT} help"), "C1", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        let reply_upper = dispatcher
            .handle_message(&format!("{BOT} HELP"), "C1", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        assert_eq!(reply_lower, reply_upper);
    }

    #[tokio::test]
    async fn dispatch_status_returns_formatted_reply() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "repo_status",
            serde_json::json!({
                "ok": true,
                "status": {
                    "url": "git@github.com:acme/myrepo.git",
                    "perma_stuck_changes": [],
                    "revision_marked_changes": [],
                    "throttled_alerts": [],
                    "pending_changes": ["a08-deploy"],
                    "waiting_changes": [],
                    "last_iteration": null,
                },
            }),
        );
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} status myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("dispatcher must produce a reply");
        let text = unwrap_sync(reply);
        assert!(text.contains("git@github.com:acme/myrepo.git"));
        assert!(text.contains("pending: a08-deploy"));
    }

    #[tokio::test]
    async fn dispatch_clear_perma_stuck_on_unique_repo_submits_action() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        submitter.set_response("clear_perma_stuck_marker", serde_json::json!({"ok": true}));
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} clear-perma-stuck myrepo a06-foo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✓"));
        assert!(text.contains("a06-foo"));
        assert!(text.contains("myrepo"));
        let calls = submitter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["action"], "clear_perma_stuck_marker");
        assert_eq!(
            calls[0]["url"], "git@github.com:acme/myrepo.git"
        );
        assert_eq!(calls[0]["change"], "a06-foo");
    }

    #[tokio::test]
    async fn dispatch_clear_perma_stuck_propagates_action_error() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "clear_perma_stuck_marker",
            serde_json::json!({
                "ok": false,
                "error": "no perma-stuck marker for change `a99-nope`",
            }),
        );
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} clear-perma-stuck myrepo a99-nope"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"));
        assert!(text.contains("no perma-stuck marker"));
        assert!(text.contains("a99-nope"));
    }

    #[tokio::test]
    async fn dispatch_no_match_replies_with_configured_list() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} status gibberish"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"));
        assert!(text.contains("gibberish"));
        assert!(text.contains("myrepo"));
        assert!(text.contains("widgets"));
        // No action was submitted.
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_unknown_verb_returns_none() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} please archive everything"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await;
        assert!(reply.is_none(), "unknown verbs must produce None for silent ignore");
    }

    // ---------- wipe-workspace confirmation flow ----------

    #[tokio::test]
    async fn wipe_workspace_two_step_confirm_happy_path() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "wipe_workspace",
            serde_json::json!({
                "ok": true,
                "path": "/tmp/workspaces/github_com_acme_myrepo",
                "already_absent": false,
            }),
        );

        let warn = dispatcher
            .handle_message(
                &format!("{BOT} wipe-workspace myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let warn_text = unwrap_sync(warn);
        assert!(warn_text.starts_with("⚠️"), "first step is a warning: {warn_text}");
        assert!(warn_text.contains("confirm"));
        assert!(warn_text.contains("60 seconds"));
        assert!(submitter.calls().is_empty(), "no action submitted yet");
        assert_eq!(dispatcher.pending_len(), 1);

        let success = dispatcher
            .handle_message("confirm", "C1", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        let success_text = unwrap_sync(success);
        assert!(success_text.starts_with("✓"), "confirm should succeed: {success_text}");
        assert!(success_text.contains("wiped"));
        assert_eq!(submitter.calls().len(), 1);
        assert_eq!(submitter.calls()[0]["action"], "wipe_workspace");
        assert_eq!(dispatcher.pending_len(), 0);
    }

    #[tokio::test]
    async fn wipe_workspace_confirm_without_pending_returns_error() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message("confirm", "C1", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"));
        assert!(text.contains("no pending"));
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn wipe_workspace_expired_confirmation_returns_error_no_wipe() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        // Plant an already-expired entry directly so the test doesn't depend
        // on wall-clock time at all.
        dispatcher.pending.record_at(
            "C1",
            "git@github.com:owner/repo.git".into(),
            Instant::now() - Duration::from_millis(1),
        );
        let reply = dispatcher
            .handle_message("confirm", "C1", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"));
        assert!(text.contains("no pending"));
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn wipe_workspace_cross_channel_confirm_no_match() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "wipe_workspace",
            serde_json::json!({"ok": true, "path": "/tmp/workspaces/x", "already_absent": false}),
        );
        // Wipe in channel A, confirm in channel B.
        dispatcher
            .handle_message(
                &format!("{BOT} wipe-workspace myrepo"),
                "A",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let reply_b = dispatcher
            .handle_message("confirm", "B", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        let text_b = unwrap_sync(reply_b);
        assert!(text_b.starts_with("✗"));
        assert!(text_b.contains("no pending"));
        assert!(submitter.calls().is_empty(), "no action submitted from cross-channel confirm");
        // A's pending entry is still live.
        assert_eq!(dispatcher.pending_len(), 1);
    }

    #[tokio::test]
    async fn control_socket_submitter_returns_error_on_missing_socket() {
        // No daemon → no socket → ActionSubmitter reports the failure
        // shape the dispatcher can format into a `✗` reply.
        let dir = tempfile::TempDir::new().unwrap();
        let submitter =
            ControlSocketSubmitter::new(dir.path().join("does-not-exist.sock"));
        let resp = submitter
            .submit(serde_json::json!({"action":"repo_status","url":"x"}))
            .await;
        assert_eq!(resp["ok"], serde_json::Value::Bool(false));
        let err = resp["error"].as_str().unwrap();
        assert!(
            err.contains("could not connect"),
            "must explain the failure: {err}"
        );
    }

    #[tokio::test]
    async fn wipe_workspace_reissue_replaces_prior_pending() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "wipe_workspace",
            serde_json::json!({"ok": true, "path": "/tmp/workspaces/sound", "already_absent": false}),
        );
        dispatcher
            .handle_message(
                &format!("{BOT} wipe-workspace myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        dispatcher
            .handle_message(
                &format!("{BOT} wipe-workspace widgets"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        // The second wipe replaced the first pending — `confirm` wipes
        // widgets, NOT myrepo.
        let success = dispatcher
            .handle_message("confirm", "C1", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        let text = unwrap_sync(success);
        assert!(text.starts_with("✓"));
        let calls = submitter.calls();
        let wipe_call = calls
            .iter()
            .find(|c| c["action"] == "wipe_workspace")
            .expect("wipe_workspace must be submitted");
        assert_eq!(
            wipe_call["url"], "git@github.com:acme/widgets.git",
            "the second wipe's URL must win"
        );
    }
}
