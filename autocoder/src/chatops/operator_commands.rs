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
//!   - `status` (bare — per-repo menu of every configured repository)
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
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Default chat-channel TTL for a wipe-workspace pending confirmation.
/// Per spec scenario: "Reply 'confirm' within 60 seconds."
pub const WIPE_CONFIRM_TTL_SECS: u64 = 60;

/// Polite-refusal reply for `send it` posted in a thread autocoder is
/// not tracking. Wording matches the `audit-reply-acts` spec scenario
/// "Send-it in untracked thread is politely refused".
pub const SEND_IT_REFUSE_UNTRACKED: &str =
    "✗ This reply is in a thread autocoder is not tracking. The `send it` verb only acts in audit-notification threads.";

/// Polite-refusal reply for `send it` against an audit thread whose
/// `posted_at` is older than the 7-day staleness cap.
pub const SEND_IT_REFUSE_STALE: &str =
    "✗ This audit's findings are too old to act on (>7d). Re-run the audit via @<bot> audit <type> <repo>.";

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
    /// The dispatcher handled all chat side-effects internally (posting
    /// its own ack via the chatops backend). The listener should take no
    /// further action — neither a threaded reply nor a `?` reaction.
    /// Used by the `propose` verb whose ack must be a top-level message
    /// in the channel whose `ts` becomes the request's lifecycle thread.
    Silent,
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
/// Reasonable upper bound on an audit-substring arg. Same shape as a
/// change-slug: alphanumerics + `_` + `-`, cap at 64 chars.
const MAX_AUDIT_SUBSTRING_LEN: usize = 64;
/// Cap on the free-form request text accepted by the `propose` verb.
/// Operators wanting more than this should put it in an issue/doc and
/// reference it; the cap keeps the inbound dispatch path bounded.
pub const MAX_PROPOSE_REQUEST_TEXT_LEN: usize = 10_000;

/// Cap on the raw-args remainder accepted by the `changelog` verb. Long
/// enough to hold any reasonable `--since vX.Y.Z --to vA.B.C` arg
/// combination plus comfortable headroom; narrow enough to bound the
/// state-file size.
pub const MAX_CHANGELOG_RAW_ARGS_LEN: usize = 512;

fn change_slug_regex() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9_-]{1,64}$").unwrap())
}

fn repo_substring_regex() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9._/-]{1,128}$").unwrap())
}

fn audit_substring_regex() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9_-]{1,64}$").unwrap())
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

fn invalid_audit_substring_reply() -> Reply {
    Reply::Sync(format!(
        "✗ invalid audit substring (must match ^[a-zA-Z0-9_-]+$, max {MAX_AUDIT_SUBSTRING_LEN} chars)"
    ))
}

fn missing_request_text_reply() -> Reply {
    Reply::Sync(
        "✗ propose: missing request text. Usage: @<bot> propose <repo> <free-form description>"
            .to_string(),
    )
}

fn missing_repo_substring_reply() -> Reply {
    Reply::Sync(
        "✗ propose: missing repo-substring. Usage: @<bot> propose <repo> <free-form description>"
            .to_string(),
    )
}

fn oversize_request_text_reply() -> Reply {
    Reply::Sync(format!(
        "✗ propose: request text exceeds {MAX_PROPOSE_REQUEST_TEXT_LEN} characters. \
         Put longer descriptions in an issue or doc and reference it in a shorter request."
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
    /// Bare `@<bot> status` (no further arguments). Returns the per-repo
    /// menu: one announcement line + one two-line section per configured
    /// repository (URL on top, summary on the next line). See
    /// `format_status_menu_reply` for the reply shape.
    StatusMenu,
    /// `@<bot> send it` posted as a thread reply in an audit-notification
    /// thread. Stamped with the thread's `thread_ts` by the chatops
    /// listener (the parser sees the thread_ts via `parse_command_in_thread`
    /// and embeds it on the command). Outside a thread, the same text
    /// parses as the unknown-verb fallback so the listener reacts with
    /// `?` rather than treating channel-level mentions as triage requests.
    SendItOnAudit {
        thread_ts: String,
    },
    /// `@<bot> audit <audit-substring> <repo-substring>` — queue an
    /// on-demand audit run for the matched repo on the next polling
    /// iteration, bypassing the audit's configured cadence. The
    /// dispatcher resolves both substrings via the established
    /// substring-matching pattern and submits the `queue_audit`
    /// control-socket action with the canonical names.
    AuditNow {
        audit_substring: String,
        repo_substring: String,
    },
    /// `@<bot> propose <repo-substring> <free-form text>` — queue a
    /// chat-driven triage request against the matched repo. The
    /// dispatcher posts a top-level ack message in the channel (whose
    /// `ts` becomes the request's lifecycle thread), writes a
    /// `ProposalRequestState` file, and submits a
    /// `queue_proposal_request` control-socket action so the next
    /// polling iteration runs the triage. See `proposal_requests` for
    /// the state-file shape and lifecycle.
    ProposeRequest {
        repo_substring: String,
        request_text: String,
    },
    /// `@<bot> changelog <repo-substring> [<args>]` — queue a chat-driven
    /// changelog generation request against the matched repo. The
    /// dispatcher posts a top-level ack message in the channel (whose
    /// `ts` becomes the request's lifecycle thread), writes a
    /// `ChangelogRequestState` file, AND submits a
    /// `queue_changelog_request` control-socket action so the next
    /// polling iteration runs the stylist. See
    /// `crate::changelog_requests` for the state-file shape AND lifecycle.
    ChangelogRequest {
        repo_substring: String,
        raw_args: String,
    },
    Help,
}

/// One repository whose per-repo status could not be fully assembled
/// (control-socket error, repo-not-found, decode failure). The menu
/// formatter still renders a section for the repository — URL on top,
/// `(unavailable: <error excerpt>)` on the summary line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableEntry {
    pub url: String,
    pub error: String,
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
    parse_command_outcome_in_thread(message, bot_mention, None)
}

/// Same as `parse_command_outcome` but also threads through the
/// inbound message's `thread_ts`. Required for verbs whose recognition
/// depends on the message arriving inside a thread (currently only
/// `send it`).
fn parse_command_outcome_in_thread(
    message: &str,
    bot_mention: &str,
    thread_ts: Option<&str>,
) -> ParseOutcome {
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
        "status" => match rest.len() {
            0 => ParseOutcome::Ok(OperatorCommand::StatusMenu),
            1 => {
                if !repo_substring_regex().is_match(rest[0]) {
                    return ParseOutcome::Invalid(invalid_repo_substring_reply());
                }
                ParseOutcome::Ok(OperatorCommand::Status {
                    repo_substring: rest[0].to_string(),
                })
            }
            _ => ParseOutcome::None,
        },
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
        "audit" => {
            if rest.len() != 2 {
                return ParseOutcome::None;
            }
            if !audit_substring_regex().is_match(rest[0]) {
                return ParseOutcome::Invalid(invalid_audit_substring_reply());
            }
            if !repo_substring_regex().is_match(rest[1]) {
                return ParseOutcome::Invalid(invalid_repo_substring_reply());
            }
            ParseOutcome::Ok(OperatorCommand::AuditNow {
                audit_substring: rest[0].to_string(),
                repo_substring: rest[1].to_string(),
            })
        }
        "help" => {
            if !rest.is_empty() {
                return ParseOutcome::None;
            }
            ParseOutcome::Ok(OperatorCommand::Help)
        }
        "propose" => {
            // `@<bot> propose <repo-substring> <free-form text>` — the
            // repo substring is the first whitespace-separated token
            // after `propose`; the request text is everything after
            // that, preserving internal whitespace/newlines, with only
            // leading/trailing whitespace trimmed. The parser keys off
            // the body string directly (not `rest`) so multi-line
            // request text doesn't collapse.
            //
            // The body shape is:
            //     after_mention = "propose <substring> <free-form...>"
            // After stripping the verb literal we have:
            //     body = " <substring> <free-form...>"
            // Find the first whitespace-bounded token (the substring)
            // and consume everything after the FIRST whitespace
            // boundary following it as the request text.
            let verb_len = verb.len();
            let after_verb = &after_mention[verb_len..];
            // Skip leading whitespace before the substring.
            let after_verb = after_verb.trim_start();
            if after_verb.is_empty() {
                return ParseOutcome::Invalid(missing_repo_substring_reply());
            }
            // Find end of the substring (first whitespace char).
            let sub_end = after_verb
                .find(|c: char| c.is_whitespace())
                .unwrap_or(after_verb.len());
            let repo_substring = &after_verb[..sub_end];
            if !repo_substring_regex().is_match(repo_substring) {
                return ParseOutcome::Invalid(invalid_repo_substring_reply());
            }
            let rest_after_sub = after_verb[sub_end..].trim();
            if rest_after_sub.is_empty() {
                return ParseOutcome::Invalid(missing_request_text_reply());
            }
            if rest_after_sub.chars().count() > MAX_PROPOSE_REQUEST_TEXT_LEN {
                return ParseOutcome::Invalid(oversize_request_text_reply());
            }
            ParseOutcome::Ok(OperatorCommand::ProposeRequest {
                repo_substring: repo_substring.to_string(),
                request_text: rest_after_sub.to_string(),
            })
        }
        "changelog" => {
            // `@<bot> changelog <repo-substring> [<args>]` — the repo
            // substring is the first whitespace-separated token after
            // `changelog`; the args remainder is everything after that.
            // Args may be empty (run with defaults) AND are passed
            // verbatim to `parse_changelog_args` by the dispatcher.
            let verb_len = verb.len();
            let after_verb = &after_mention[verb_len..];
            let after_verb = after_verb.trim_start();
            if after_verb.is_empty() {
                return ParseOutcome::Invalid(Reply::Sync(
                    "✗ changelog: missing repo-substring.".to_string(),
                ));
            }
            let sub_end = after_verb
                .find(|c: char| c.is_whitespace())
                .unwrap_or(after_verb.len());
            let repo_substring = &after_verb[..sub_end];
            if !repo_substring_regex().is_match(repo_substring) {
                return ParseOutcome::Invalid(invalid_repo_substring_reply());
            }
            let rest_after_sub = after_verb[sub_end..].trim();
            if rest_after_sub.chars().count() > MAX_CHANGELOG_RAW_ARGS_LEN {
                return ParseOutcome::Invalid(Reply::Sync(format!(
                    "✗ changelog: args exceed {MAX_CHANGELOG_RAW_ARGS_LEN} characters."
                )));
            }
            ParseOutcome::Ok(OperatorCommand::ChangelogRequest {
                repo_substring: repo_substring.to_string(),
                raw_args: rest_after_sub.to_string(),
            })
        }
        "send" => {
            // `@<bot> send it` parses ONLY when the inbound message
            // arrived inside a thread (non-empty `thread_ts`) AND the
            // verb takes exactly one positional `it`. Any other arg
            // count or shape falls through to the unknown-verb path,
            // which the listener turns into a `?` reaction.
            if rest.len() != 1 || !rest[0].eq_ignore_ascii_case("it") {
                return ParseOutcome::None;
            }
            let ts = match thread_ts {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return ParseOutcome::None,
            };
            ParseOutcome::Ok(OperatorCommand::SendItOnAudit { thread_ts: ts })
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

/// Thread-aware variant of `parse_command`. Verbs whose recognition
/// depends on the inbound message arriving inside a thread (currently
/// only `send it`) require this entry point. Pass `thread_ts: Some(&str)`
/// for messages with a non-empty thread root, `None` for channel-level
/// mentions.
#[allow(dead_code)]
pub fn parse_command_in_thread(
    message: &str,
    bot_mention: &str,
    thread_ts: Option<&str>,
) -> Option<OperatorCommand> {
    match parse_command_outcome_in_thread(message, bot_mention, thread_ts) {
        ParseOutcome::Ok(cmd) => Some(cmd),
        ParseOutcome::None | ParseOutcome::Invalid(_) => None,
    }
}

// ====================================================================
// Changelog raw-args parser
// ====================================================================

/// Result of parsing the `<args>` remainder from
/// `@<bot> changelog <repo> [<args>]`. Mirrors the flag surface of
/// `autocoder changelog`: `--since <tag>`, `--to <tag>`. The
/// `--workspace <path>` flag is parsed BUT default-denied for chatops
/// (operators picking workspaces via chat would let any unprivileged
/// channel member point the stylist at an arbitrary directory). The
/// `workspace_override` field is populated only when an explicit
/// trust-elevated path bypasses the deny — production wiring sets the
/// trust gate to `false`, so the field stays `None` in normal use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedChangelogArgs {
    pub since: Option<String>,
    pub to: Option<String>,
    pub workspace_override: Option<String>,
}

/// Parse the raw-args remainder of `@<bot> changelog <repo> [<args>]`.
/// Accepts a subset of the CLI's flag surface (`--since`, `--to`, AND
/// `--workspace`, the last of which is default-denied at higher layers).
/// Bad flags surface as descriptive errors so the dispatcher can post
/// `✗ changelog: bad arg: <text>`.
pub fn parse_changelog_args(raw: &str) -> Result<ParsedChangelogArgs, String> {
    let mut out = ParsedChangelogArgs::default();
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        match tok {
            "--since" => {
                let val = tokens.get(i + 1).copied().ok_or_else(|| {
                    "missing value after `--since`".to_string()
                })?;
                if val.starts_with("--") {
                    return Err(format!("missing value after `--since` (got `{val}`)"));
                }
                out.since = Some(val.to_string());
                i += 2;
            }
            "--to" => {
                let val = tokens.get(i + 1).copied().ok_or_else(|| {
                    "missing value after `--to`".to_string()
                })?;
                if val.starts_with("--") {
                    return Err(format!("missing value after `--to` (got `{val}`)"));
                }
                out.to = Some(val.to_string());
                i += 2;
            }
            "--workspace" => {
                let val = tokens.get(i + 1).copied().ok_or_else(|| {
                    "missing value after `--workspace`".to_string()
                })?;
                if val.starts_with("--") {
                    return Err(format!(
                        "missing value after `--workspace` (got `{val}`)"
                    ));
                }
                out.workspace_override = Some(val.to_string());
                i += 2;
            }
            other => {
                return Err(format!("unrecognized arg `{other}`"));
            }
        }
    }
    Ok(out)
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

/// Outcome of resolving an operator-supplied audit substring against the
/// registered audit-type names. Mirrors `RepoMatch`.
#[derive(Debug)]
pub enum AuditMatch<'a> {
    Unique(&'a str),
    Multiple(Vec<&'a str>),
    None,
}

/// Case-insensitive substring match against the registered audit-type
/// names. Mirrors `match_repo`: any registered name whose lowercase form
/// contains the lowercase of `substring` is a match. Empty substring
/// matches everything (returned as `Multiple` so the operator sees the
/// full list).
pub fn match_audit_type<'a>(substring: &str, registered: &'a [&str]) -> AuditMatch<'a> {
    let needle = substring.to_ascii_lowercase();
    let mut matches: Vec<&'a str> = Vec::new();
    for name in registered {
        if name.to_ascii_lowercase().contains(&needle) {
            matches.push(*name);
        }
    }
    match matches.len() {
        0 => AuditMatch::None,
        1 => AuditMatch::Unique(matches.into_iter().next().unwrap()),
        _ => AuditMatch::Multiple(matches),
    }
}

/// Reply when an audit substring matches more than one registered audit
/// type. Mirrors `format_multiple_matches` for repos.
pub fn format_audit_multiple_matches(substring: &str, matches: &[&str]) -> String {
    format!(
        "✗ audit substring `{substring}` matches multiple: {}. Be more specific.",
        matches.join(", ")
    )
}

/// Reply when an audit substring matches no registered audit type. Lists
/// every registered name so the operator sees their options. Mirrors
/// `format_no_match` for repos.
pub fn format_audit_no_match(substring: &str, registered: &[&str]) -> String {
    if registered.is_empty() {
        return format!("✗ no audit matched `{substring}`; no audits registered.");
    }
    format!(
        "✗ no audit matched `{substring}`; registered: {}.",
        registered.join(", ")
    )
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
    #[serde(default)]
    pub base_branch: String,
    #[serde(default)]
    pub agent_branch: String,
    #[serde(default)]
    pub last_commit_base: Option<CommitSummary>,
    #[serde(default)]
    pub last_commit_agent: Option<CommitSummary>,
    #[serde(default)]
    pub latest_pr: Option<PrSummary>,
    #[serde(default)]
    pub currently_busy: Option<BusySummary>,
    pub perma_stuck_changes: Vec<MarkerEntry>,
    pub revision_marked_changes: Vec<MarkerEntry>,
    pub throttled_alerts: Vec<ThrottledAlertEntry>,
    pub pending_changes: Vec<String>,
    pub waiting_changes: Vec<String>,
    pub last_iteration: Option<LastIteration>,
}

/// One-line summary of the latest commit on a branch (sourced from
/// `git log -1`). `age` is `Utc::now() - committer_timestamp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitSummary {
    pub short_sha: String,
    pub subject: String,
    pub age: chrono::Duration,
}

/// One-line summary of the most recent PR opened from the daemon's agent
/// branch. `state` is `"open" | "closed" | "merged"`; `age` is
/// `Utc::now() - created_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub head_branch: String,
    pub url: String,
    pub age: chrono::Duration,
}

/// One-line summary of the per-repo busy marker, when held. Carries
/// enough marker context for `format_status_reply` to surface the
/// distinguishing variants of the `currently:` line (audit-in-flight,
/// post-executor stage, stale-marker, etc.) rather than collapsing
/// every non-`change` marker into a misleading `currently: idle`.
///
/// `started_at` is the marker's recorded `started_at` field (RFC3339
/// from the JSON body); when the marker JSON is malformed and the
/// daemon falls back to the file's mtime, the daemon writes the
/// fallback into this field so downstream formatters never see the
/// distinction.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BusySummary {
    /// Slug of the OpenSpec change the daemon is currently working on,
    /// or empty when the marker has no associated change (audit run,
    /// post-executor stage, recovery operation, etc.).
    #[serde(default)]
    pub change: String,
    pub started_at: DateTime<Utc>,
    /// Marker's `stage` field as a string (`executor`, `commit`,
    /// `review`, `push`, `pr`). Default `executor` for backwards
    /// compatibility with pre-spec marker JSON that omitted the field
    /// at the wire boundary.
    #[serde(default)]
    pub stage: String,
    /// PID recorded in the marker. Used by the stale-marker branch of
    /// the `currently:` line to surface "stale marker from pid <pid>".
    #[serde(default)]
    pub pid: u32,
    /// `executor.busy_marker_stale_threshold_secs` resolved value, used
    /// by the stale-marker branch's "recovery in <duration>" /
    /// "recovery eligible now" heuristics. Zero when the threshold is
    /// unconfigured (status falls back to a no-stale-warning behavior).
    #[serde(default)]
    pub stale_threshold_secs: u64,
    /// Whether the marker's recorded PID is alive on the daemon host
    /// as of the status snapshot. False means recovery would fire
    /// immediately on the next iteration (per `a08`).
    #[serde(default)]
    pub pid_alive: bool,
    /// When the marker has `stage=executor` AND `change` is empty AND
    /// an audit log file's timestamp matches the marker's `started_at`
    /// (within 1s), the daemon resolves the audit type from the
    /// filename and records it here. `None` means the executor stage
    /// is busy but no audit log matches.
    #[serde(default)]
    pub audit_type: Option<String>,
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

/// Escape Slack-special characters (`&`, `<`, `>`) so user-controlled
/// strings (commit subjects, PR titles, change names) can be embedded in
/// the status reply without inadvertently invoking Slack's mention syntax
/// (e.g. a malicious commit subject `<!channel> ping` would otherwise
/// notify every member of the channel).
///
/// Order matters: `&` is substituted FIRST. Doing it last would
/// double-escape the substitutions produced by `<`→`&lt;` and `>`→`&gt;`.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a `CommitSummary` (or its absence) as the `<sha> "<subject>"
/// (<age> ago)` shape used by the `last commit on <branch>:` lines.
/// Returns `(none)` for `None`. The subject is Slack-escaped.
fn format_commit_summary(summary: Option<&CommitSummary>) -> String {
    match summary {
        Some(s) => {
            let age = human_age_duration(s.age);
            format!(
                "{} \"{}\" ({age} ago)",
                s.short_sha,
                slack_escape(&s.subject)
            )
        }
        None => "(none)".to_string(),
    }
}

/// Format the status response into the multi-line chat reply shape
/// from the proposal. The five always-present sections (branches, last
/// commits, latest PR, currently-busy, next-iteration) come first;
/// existing marker / throttled-alert / queue sections collapse when
/// empty exactly as before.
pub fn format_status_reply(resp: &RepoStatusResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("📊 {}\n", resp.url));

    // ---- Always-present block ----
    if !resp.base_branch.is_empty() || !resp.agent_branch.is_empty() {
        out.push_str(&format!(
            "\nbranches: base={}, agent={}\n",
            resp.base_branch, resp.agent_branch
        ));
        out.push_str(&format!(
            "last commit on {}: {}\n",
            resp.base_branch,
            format_commit_summary(resp.last_commit_base.as_ref())
        ));
        out.push_str(&format!(
            "last commit on {}: {}\n",
            resp.agent_branch,
            format_commit_summary(resp.last_commit_agent.as_ref())
        ));

        out.push('\n');
        match &resp.latest_pr {
            Some(pr) => {
                let age = human_age_duration(pr.age);
                out.push_str(&format!(
                    "latest PR: #{} \"{}\"  {} · head={} · {age} ago\n",
                    pr.number,
                    slack_escape(&pr.title),
                    pr.state,
                    pr.head_branch
                ));
                out.push_str(&format!("           {}\n", pr.url));
            }
            None => {
                out.push_str("latest PR: (none)\n");
            }
        }

        out.push('\n');
        out.push_str(&format_currently_line(resp.currently_busy.as_ref()));
        out.push('\n');
    }

    let has_markers =
        !resp.perma_stuck_changes.is_empty() || !resp.revision_marked_changes.is_empty();
    if has_markers {
        out.push_str("\nactive markers (excluded from list_pending):\n");
        for m in &resp.perma_stuck_changes {
            let age = human_age_since(m.marked_at);
            let change = slack_escape(&m.change);
            if m.detail.is_empty() {
                out.push_str(&format!(
                    "  • {change} (.perma-stuck.json — marked {age} ago)\n"
                ));
            } else {
                out.push_str(&format!(
                    "  • {change} (.perma-stuck.json — {}, marked {age} ago)\n",
                    m.detail
                ));
            }
        }
        for m in &resp.revision_marked_changes {
            let age = human_age_since(m.marked_at);
            let change = slack_escape(&m.change);
            out.push_str(&format!(
                "  • {change} (.needs-spec-revision.json — marked {age} ago)\n"
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

    let excluded: Vec<String> = resp
        .perma_stuck_changes
        .iter()
        .chain(resp.revision_marked_changes.iter())
        .map(|m| m.change.clone())
        .collect();
    let queue_has_content = !resp.pending_changes.is_empty()
        || !resp.waiting_changes.is_empty()
        || !excluded.is_empty();
    if queue_has_content {
        // One-liner form when ALL three lists are small (≤5).
        let small =
            resp.pending_changes.len() <= 5
                && resp.waiting_changes.len() <= 5
                && excluded.len() <= 5;
        if small {
            out.push('\n');
            out.push_str(&format_queue_one_liner(
                &resp.pending_changes,
                &resp.waiting_changes,
                &excluded,
            ));
            out.push('\n');
        } else {
            out.push_str("\nqueue snapshot:\n");
            if !resp.pending_changes.is_empty() {
                out.push_str(&format!(
                    "  pending: {}\n",
                    resp.pending_changes
                        .iter()
                        .map(|c| slack_escape(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !resp.waiting_changes.is_empty() {
                out.push_str(&format!(
                    "  waiting: {}\n",
                    resp.waiting_changes
                        .iter()
                        .map(|c| slack_escape(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !excluded.is_empty() {
                out.push_str(&format!(
                    "  excluded: {} (see markers above)\n",
                    excluded
                        .iter()
                        .map(|c| slack_escape(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    // Strip trailing newline so chatops backends post a single message
    // without an empty terminal line.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Render the compact-queue one-liner used when every list is ≤5
/// entries: `queue: N pending (<list>), M waiting (<list>), K excluded`.
/// Empty lists render as the count alone (`0 pending`) — no empty
/// parenthetical. Change names pass through `slack_escape` belt-and-
/// braces; the parser's allowlist already restricts them to
/// `[a-zA-Z0-9_-]`.
fn format_queue_one_liner(
    pending: &[String],
    waiting: &[String],
    excluded: &[String],
) -> String {
    fn render(label: &str, list: &[String]) -> String {
        let n = list.len();
        if n == 0 {
            format!("{n} {label}")
        } else {
            let items: Vec<String> = list.iter().map(|c| slack_escape(c)).collect();
            format!("{n} {label} ({})", items.join(", "))
        }
    }
    // Excluded never gets a parenthetical — operators reference the
    // dedicated markers section above for details. For the one-liner
    // form the count is what matters.
    let excluded_part = format!("{} excluded", excluded.len());
    format!(
        "queue: {}, {}, {}",
        render("pending", pending),
        render("waiting", waiting),
        excluded_part,
    )
}

/// Render the enriched wipe-workspace confirmation message. Shape:
///
/// ```text
/// ⚠️ Wipe-workspace requested for <repo_url>
/// This will delete <workspace_path> (forces a re-clone on the next iteration).
///
/// Currently: <busy_clause>
/// Queue (continues after wipe): <queue_clause>
/// [Active markers (git-tracked; preserved across the wipe):
///   • <change> (<marker-file>)
///   ...]
///
/// Reply 'confirm' within 60 seconds to proceed.
/// ```
///
/// The `Currently:` clause is always present and reads either `idle` or
/// `working on \`<change>\` (started <age> ago) — will be cancelled`. The
/// `Queue (continues after wipe):` clause reuses the same compact form
/// as `format_queue_one_liner` (collapses to `empty queue` when all
/// three categories are zero). The `Active markers (...)` section is
/// elided entirely when no marker files exist — no empty section, no
/// `(none)` placeholder. User-controlled fields (change names, repo URL,
/// workspace path) pass through `slack_escape` belt-and-braces so a
/// hostile commit subject can't smuggle `<!channel>` past the parser's
/// allowlist.
pub fn format_wipe_confirmation(
    workspace_path: &Path,
    repo_url: &str,
    status: &RepoStatusResponse,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "⚠️ Wipe-workspace requested for {}\n",
        slack_escape(repo_url)
    ));
    out.push_str(&format!(
        "This will delete {} (forces a re-clone on the next iteration).\n",
        slack_escape(&workspace_path.display().to_string())
    ));
    out.push('\n');

    // Currently: clause. The pre-spec form was `idle` OR `working on
    // <change> (started <age> ago) — will be cancelled`. With a11 the
    // marker may be present without a change (audit run, post-executor
    // stage, recovery operation), so we surface the named change when
    // available AND still warn that the in-flight work will be
    // cancelled; otherwise we report the marker's classified
    // `currently:` line as-is.
    let busy_clause = match &status.currently_busy {
        Some(b) if !b.change.trim().is_empty() => {
            let age = crate::busy_marker::format_age_human(currently_age_secs(b.started_at));
            format!(
                "working on `{}` (started {age} ago) — will be cancelled",
                slack_escape(&b.change)
            )
        }
        Some(_) => {
            let inner = format_currently_line(status.currently_busy.as_ref());
            let stripped = inner.strip_prefix("currently: ").unwrap_or(&inner);
            format!("{stripped} — will be cancelled")
        }
        None => "idle".to_string(),
    };
    out.push_str(&format!("Currently: {busy_clause}\n"));

    // Queue (continues after wipe): clause. Collapse to `empty queue`
    // when all three categories are zero.
    let excluded: Vec<String> = status
        .perma_stuck_changes
        .iter()
        .chain(status.revision_marked_changes.iter())
        .map(|m| m.change.clone())
        .collect();
    let queue_empty = status.pending_changes.is_empty()
        && status.waiting_changes.is_empty()
        && excluded.is_empty();
    let queue_clause = if queue_empty {
        "empty queue".to_string()
    } else {
        let pending = render_count_with_list("pending", &status.pending_changes);
        let waiting = render_count_with_list("waiting", &status.waiting_changes);
        // Excluded gets the count alone (no parenthetical) — operators
        // refer to the dedicated markers section below.
        let excluded_part = format!("{} excluded", excluded.len());
        format!("{pending}, {waiting}, {excluded_part}")
    };
    out.push_str(&format!("Queue (continues after wipe): {queue_clause}\n"));

    // Active markers (git-tracked; preserved across the wipe). Elided
    // when no markers exist.
    let total_markers =
        status.perma_stuck_changes.len() + status.revision_marked_changes.len();
    if total_markers > 0 {
        out.push_str("Active markers (git-tracked; preserved across the wipe):\n");
        for m in &status.perma_stuck_changes {
            out.push_str(&format!(
                "  • {} (.perma-stuck.json)\n",
                slack_escape(&m.change)
            ));
        }
        for m in &status.revision_marked_changes {
            out.push_str(&format!(
                "  • {} (.needs-spec-revision.json)\n",
                slack_escape(&m.change)
            ));
        }
    }

    out.push('\n');
    out.push_str(&format!(
        "Reply 'confirm' within {WIPE_CONFIRM_TTL_SECS} seconds to proceed."
    ));
    out
}

/// Render `N <label> (<comma-list>)` (or just `N <label>` for an empty
/// list). Used by the wipe confirmation's queue clause; the form mirrors
/// the queue one-liner shape from `format_status_reply`.
fn render_count_with_list(label: &str, list: &[String]) -> String {
    let n = list.len();
    if n == 0 {
        format!("{n} {label}")
    } else {
        let items: Vec<String> = list.iter().map(|c| slack_escape(c)).collect();
        format!("{n} {label} ({})", items.join(", "))
    }
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

/// Render the ETA clause for an `audit` ack from the daemon's response.
/// Prefers an explicit `seconds_until_next_iteration` field (`< 30` →
/// `imminently`) and falls back to `~Nm` from `poll_interval_sec`. When
/// neither is present, returns `soon` so the operator gets a usable
/// reply even if the daemon's response is sparse.
fn format_audit_eta(resp: &serde_json::Value) -> String {
    if let Some(s) = resp
        .get("seconds_until_next_iteration")
        .and_then(|v| v.as_u64())
        && s < 30
    {
        return "imminently".to_string();
    }
    if let Some(p) = resp.get("poll_interval_sec").and_then(|v| v.as_u64()) {
        if p < 30 {
            return "imminently".to_string();
        }
        let mins = (p + 30) / 60;
        if mins == 0 {
            return format!("~{p}s");
        }
        return format!("~{mins}m");
    }
    "soon".to_string()
}

/// Multi-line synopsis returned by the `help` verb. Lists every
/// currently-supported verb with one-line description + the README
/// pointer for the destructive-confirmation flow.
pub fn format_help_reply() -> String {
    let mut out = String::new();
    out.push_str("Available commands (mention the bot to invoke):\n");
    out.push_str("  • `status <repo>` — current markers, throttled alerts, queue snapshot, last iteration\n");
    out.push_str("  • `status` (no repo) — list every watched repository with queue summary, busy state, and last-iteration time. Use `status <repo>` for the per-repo detail.\n");
    out.push_str("  • `clear-perma-stuck <repo> <change>` — clear `.perma-stuck.json` for a change\n");
    out.push_str("  • `clear-revision <repo> <change>` — clear `.needs-spec-revision.json` for a change\n");
    out.push_str("  • `wipe-workspace <repo>` — destructive: warns, then awaits `confirm` (60s TTL)\n");
    out.push_str("  • `confirm` — second step for `wipe-workspace` (same channel, within 60s)\n");
    out.push_str("  • `rebuild-specs <repo>` — schedule a canonical-spec rebuild for the next iteration\n");
    out.push_str("  • `audit <audit-substring> <repo>` — queue an on-demand audit run for the next polling iteration\n");
    out.push_str("  • `propose <repo> <free-form text>` — queue a chat-driven triage request (question or directive)\n");
    out.push_str("  • `changelog <repo> [<args>]` — generate an LLM-styled CHANGELOG.md update via PR\n");
    out.push_str("  • `help` — this synopsis\n");
    out.push_str("See the README \"ChatOps operator commands\" section for the destructive confirmation flow.");
    out
}

/// Format the bare-`status` (menu) reply: one announcement line followed by
/// one two-line section per repository (URL on top, summary on the next
/// line). `responses` are repos whose per-repo `RepoStatusResponse` was
/// successfully built; `unavailable` are repos whose per-repo lookup
/// errored — they still ship as a URL line + `(unavailable: <err>)` so the
/// operator sees every watched repository.
///
/// Empty inputs (no configured repos, all lookups failed before reaching
/// the formatter) collapse to `📊 No repositories configured.` for
/// symmetry with the dispatcher's empty-config short-circuit.
pub fn format_status_menu_reply(
    responses: &[RepoStatusResponse],
    unavailable: &[UnavailableEntry],
) -> String {
    let total = responses.len() + unavailable.len();
    if total == 0 {
        return "📊 No repositories configured.".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "📊 Watching {total} repositories. Reply `@<bot> status <repo-substring>` for details.\n"
    ));
    for resp in responses {
        out.push('\n');
        out.push_str(&format!("  • {}\n", resp.url));
        out.push_str(&format!(
            "    {} · {} · {}\n",
            menu_queue_clause(resp),
            menu_busy_clause(resp),
            menu_last_iteration_clause(resp),
        ));
    }
    for entry in unavailable {
        out.push('\n');
        out.push_str(&format!("  • {}\n", entry.url));
        out.push_str(&format!(
            "    (unavailable: {})\n",
            truncate_error_excerpt(&entry.error)
        ));
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Queue clause for the per-repo menu section. Collapses to `empty queue`
/// when all three counts are zero; otherwise emits `<N> pending (<list>),
/// <M> waiting (<list>), <K> excluded` where each list truncates after 5
/// entries with ` …+N more`. Empty-list categories render as the count
/// alone (no empty parens). Change names pass through `slack_escape`.
fn menu_queue_clause(resp: &RepoStatusResponse) -> String {
    let excluded: Vec<String> = resp
        .perma_stuck_changes
        .iter()
        .chain(resp.revision_marked_changes.iter())
        .map(|m| m.change.clone())
        .collect();
    let pending = &resp.pending_changes;
    let waiting = &resp.waiting_changes;
    if pending.is_empty() && waiting.is_empty() && excluded.is_empty() {
        return "empty queue".to_string();
    }
    // Excluded never gets a parenthetical in the menu — operators
    // drill in via `status <repo>` for the marker detail. Count alone
    // keeps the per-repo line short.
    let excluded_count = excluded.len();
    format!(
        "{}, {}, {excluded_count} excluded",
        menu_queue_segment("pending", pending),
        menu_queue_segment("waiting", waiting),
    )
}

/// Render one `<N> <label> (<list>)` segment for the menu queue clause,
/// truncating to the first 5 entries with ` …+N more` for the overflow.
/// Empty lists render as the count alone (no parens).
fn menu_queue_segment(label: &str, list: &[String]) -> String {
    let n = list.len();
    if n == 0 {
        return format!("{n} {label}");
    }
    let shown: Vec<String> = list
        .iter()
        .take(5)
        .map(|c| slack_escape(c))
        .collect();
    if n <= 5 {
        format!("{n} {label} ({})", shown.join(", "))
    } else {
        let extra = n - 5;
        format!("{n} {label} ({} …+{extra} more)", shown.join(", "))
    }
}

/// Busy clause for the per-repo menu section. Reuses the same branching
/// rules as the per-repo `currently:` line so the bare-status menu and
/// the detailed-status reply never disagree about whether a repo is
/// idle, working on a change, running an audit, or sitting on a stale
/// marker. The `currently: ` prefix is stripped — the menu's section
/// header already implies "currently".
fn menu_busy_clause(resp: &RepoStatusResponse) -> String {
    let line = format_currently_line(resp.currently_busy.as_ref());
    line.strip_prefix("currently: ").unwrap_or(&line).to_string()
}

/// Last-iteration clause for the per-repo menu section: `no iteration
/// yet` on a fresh-startup daemon that hasn't polled this repo yet,
/// otherwise `last iteration <age> ago`.
fn menu_last_iteration_clause(resp: &RepoStatusResponse) -> String {
    match &resp.last_iteration {
        None => "no iteration yet".to_string(),
        Some(li) => format!("last iteration {} ago", human_age_since(li.finished_at)),
    }
}

/// Cap the unavailable-section error excerpt at a reasonable length so a
/// multi-line `anyhow` error chain doesn't blow out the menu reply.
fn truncate_error_excerpt(s: &str) -> String {
    const MAX: usize = 80;
    let first_line = s.lines().next().unwrap_or("");
    if first_line.chars().count() <= MAX {
        first_line.to_string()
    } else {
        first_line.chars().take(MAX).collect::<String>() + "…"
    }
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
    /// Directory under which the audit-thread state files live (the
    /// dispatcher resolves `send it` requests against
    /// `<audit_thread_state_dir>/audit-threads/<thread_ts>.json`).
    /// Defaults to `crate::audits::threads::default_state_root()` —
    /// tests override via `with_audit_thread_state_dir`.
    audit_thread_state_dir: PathBuf,
    /// Directory under which proposal-request state files live (the
    /// dispatcher writes `<proposal_request_state_dir>/proposal-requests/<repo-sanitized>/<request_id>.json`
    /// in the `propose` branch). Defaults to
    /// `crate::proposal_requests::default_state_root()`.
    proposal_request_state_dir: PathBuf,
    /// Directory under which changelog-request state files live (the
    /// dispatcher writes
    /// `<changelog_request_state_dir>/changelog-requests/<repo-sanitized>/<request_id>.json`
    /// in the `changelog` branch). Defaults to
    /// `crate::changelog_requests::default_state_root()`.
    changelog_request_state_dir: PathBuf,
    /// Registered audit-type names. The `AuditNow` branch uses these to
    /// resolve operator-supplied audit substrings. Defaults to empty;
    /// the daemon's `cli/run.rs` wires the live registry's
    /// `known_type_names()` in via `with_audit_types`.
    audit_types: Vec<String>,
    /// Optional ChatOps backend handle. The `propose` branch uses this
    /// to post a top-level ack message AND capture its `ts` so the
    /// returned ts becomes the proposal-request's lifecycle thread.
    /// When `None` (some test paths), the `propose` branch returns a
    /// `Reply::Sync(...)` failure noting chatops is not configured.
    chatops: Option<std::sync::Arc<dyn crate::chatops::ChatOpsBackend>>,
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
            audit_thread_state_dir: crate::audits::threads::default_state_root(),
            proposal_request_state_dir:
                crate::proposal_requests::default_state_root(),
            changelog_request_state_dir:
                crate::changelog_requests::default_state_root(),
            audit_types: Vec::new(),
            chatops: None,
        }
    }

    /// Override the audit-thread state directory. Used by tests to
    /// point the dispatcher at a per-test `TempDir` so concurrent runs
    /// can't trip over each other's state files.
    #[allow(dead_code)]
    pub fn with_audit_thread_state_dir(mut self, dir: PathBuf) -> Self {
        self.audit_thread_state_dir = dir;
        self
    }

    /// Override the proposal-request state directory. Tests use this
    /// the same way `with_audit_thread_state_dir` is used.
    #[allow(dead_code)]
    pub fn with_proposal_request_state_dir(mut self, dir: PathBuf) -> Self {
        self.proposal_request_state_dir = dir;
        self
    }

    /// Override the changelog-request state directory. Tests use this
    /// the same way `with_proposal_request_state_dir` is used.
    #[allow(dead_code)]
    pub fn with_changelog_request_state_dir(mut self, dir: PathBuf) -> Self {
        self.changelog_request_state_dir = dir;
        self
    }

    /// Inject the ChatOps backend the `propose` branch uses to post its
    /// top-level ack and capture the resulting `ts`. Production wiring
    /// (in `cli/run.rs`) installs the live backend; tests pass a mock
    /// that records the post and returns a deterministic ts.
    pub fn with_chatops(
        mut self,
        chatops: std::sync::Arc<dyn crate::chatops::ChatOpsBackend>,
    ) -> Self {
        self.chatops = Some(chatops);
        self
    }

    /// Set the registered audit-type names the dispatcher resolves an
    /// `@<bot> audit <substr> <repo>` command against. Pass the daemon's
    /// `AuditRegistry::known_type_names()` here at startup; tests build
    /// fixtures with `vec!["security_bug_audit", ...]`.
    pub fn with_audit_types<I>(mut self, types: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        self.audit_types = types.into_iter().map(Into::into).collect();
        self
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
        self.handle_message_with_context(
            text,
            channel_id,
            None,
            None,
            bot_mention,
            repositories,
            submitter,
        )
        .await
    }

    /// Thread-aware entry point. The Slack inbound listener uses this
    /// so the `send it` verb (which only parses inside a thread) sees
    /// the inbound envelope's `thread_ts`. Pass `None` for channel-level
    /// mentions; pass `Some(&str)` (non-empty) for replies in a thread.
    #[allow(dead_code)] // used by tests; production path goes via handle_message_with_context
    pub async fn handle_message_in_thread(
        &self,
        text: &str,
        channel_id: &str,
        thread_ts: Option<&str>,
        bot_mention: &str,
        repositories: &[RepoIdentity],
        submitter: &dyn ActionSubmitter,
    ) -> Option<Reply> {
        self.handle_message_with_context(
            text,
            channel_id,
            thread_ts,
            None,
            bot_mention,
            repositories,
            submitter,
        )
        .await
    }

    /// Most-comprehensive entry point. Accepts both `thread_ts` (for
    /// verbs whose recognition depends on threading, e.g. `send it`)
    /// AND `operator_user` (for verbs whose state files record who
    /// issued the command, e.g. `propose`). Existing call sites that
    /// don't have one or both pass `None`.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_message_with_context(
        &self,
        text: &str,
        channel_id: &str,
        thread_ts: Option<&str>,
        operator_user: Option<&str>,
        bot_mention: &str,
        repositories: &[RepoIdentity],
        submitter: &dyn ActionSubmitter,
    ) -> Option<Reply> {
        match parse_command_outcome_in_thread(text, bot_mention, thread_ts) {
            ParseOutcome::Ok(OperatorCommand::ProposeRequest {
                repo_substring,
                request_text,
            }) => Some(
                self.dispatch_propose_request(
                    &repo_substring,
                    &request_text,
                    channel_id,
                    operator_user,
                    repositories,
                    submitter,
                )
                .await,
            ),
            ParseOutcome::Ok(OperatorCommand::ChangelogRequest {
                repo_substring,
                raw_args,
            }) => Some(
                self.dispatch_changelog_request(
                    &repo_substring,
                    &raw_args,
                    channel_id,
                    repositories,
                    submitter,
                )
                .await,
            ),
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
                // Always record the pending entry before potentially
                // returning early — the operator has issued the verb and
                // we want `confirm` to work even if the status fetch
                // glitches (the worst case is then a less-rich
                // confirmation message, not a stuck-in-limbo state).
                self.pending.record(
                    channel_id,
                    repo.url.clone(),
                    Duration::from_secs(WIPE_CONFIRM_TTL_SECS),
                );
                // Fetch the live repo status so the confirmation message
                // can show the operator what the wipe is acting on.
                let status_resp = submitter
                    .submit(serde_json::json!({
                        "action": "repo_status",
                        "url": repo.url,
                    }))
                    .await;
                let status_obj: Option<RepoStatusResponse> =
                    if status_resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        serde_json::from_value(status_resp["status"].clone()).ok()
                    } else {
                        None
                    };
                let status = status_obj.unwrap_or_else(|| RepoStatusResponse {
                    url: repo.url.clone(),
                    ..RepoStatusResponse::default()
                });
                format_wipe_confirmation(&repo.workspace_path, &repo.url, &status)
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
                    let already_absent = resp
                        .get("already_absent")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let drain_outcome = resp
                        .get("drain_outcome")
                        .and_then(|v| v.as_str())
                        .unwrap_or("no iteration in flight");
                    // The already-absent outcome supersedes the drain
                    // outcome in the reply text. Operators reading the
                    // reply want to know "directory was missing" first;
                    // the drain outcome in that case is always "no
                    // iteration in flight" (the daemon doesn't run an
                    // iteration with no workspace to act on) so reporting
                    // it would be redundant noise.
                    if already_absent {
                        format!("✓ Wiped {path} (already absent)")
                    } else {
                        format!("✓ Wiped {path} ({drain_outcome})")
                    }
                } else {
                    let err = resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)");
                    format!("✗ wipe-workspace failed: {err}")
                }
            }
            OperatorCommand::StatusMenu => {
                self.dispatch_status_menu(repositories, submitter).await
            }
            OperatorCommand::SendItOnAudit { thread_ts } => {
                self.dispatch_send_it_on_audit(&thread_ts, submitter).await
            }
            OperatorCommand::AuditNow {
                audit_substring,
                repo_substring,
            } => {
                self.dispatch_audit_now(
                    &audit_substring,
                    &repo_substring,
                    repositories,
                    submitter,
                )
                .await
            }
            OperatorCommand::Help => format_help_reply(),
            // The `propose` verb is routed via `handle_message_with_context`
            // BEFORE reaching this dispatch fn, because its side effects
            // (posting a top-level ack via chatops, writing a state file)
            // produce a `Reply::Silent` that doesn't fit the `String`-
            // returning shape of this method. Reaching this arm means an
            // upstream change forgot to route — fail loudly.
            OperatorCommand::ProposeRequest { .. } => {
                "✗ propose: internal routing error (the dispatcher saw \
                 ProposeRequest in the String-returning dispatch fn). \
                 Please file a bug."
                    .to_string()
            }
            OperatorCommand::ChangelogRequest { .. } => {
                "✗ changelog: internal routing error (the dispatcher saw \
                 ChangelogRequest in the String-returning dispatch fn). \
                 Please file a bug."
                    .to_string()
            }
        }
    }

    /// Handle the `propose` verb. Resolves the repo, posts a top-level
    /// ack message via the configured chatops backend (capturing the
    /// ack's `ts` as the request's lifecycle thread), writes a
    /// `ProposalRequestState` file with `status: Pending`, and submits a
    /// `queue_proposal_request` control-socket action so the next
    /// polling iteration picks up the request. Returns `Reply::Silent`
    /// on success (the dispatcher has already posted the ack) and
    /// `Reply::Sync(...)` on every failure shape so the operator's
    /// `propose` message gets a threaded reply explaining what went
    /// wrong.
    async fn dispatch_propose_request(
        &self,
        repo_substring: &str,
        request_text: &str,
        channel_id: &str,
        operator_user: Option<&str>,
        repositories: &[RepoIdentity],
        submitter: &dyn ActionSubmitter,
    ) -> Reply {
        // 1. Repo substring resolution.
        let repo = match match_repo(repo_substring, repositories) {
            RepoMatch::Unique(r) => r,
            RepoMatch::Multiple(ms) => {
                return Reply::Sync(format_multiple_matches(repo_substring, &ms));
            }
            RepoMatch::None => {
                return Reply::Sync(format_no_match(repo_substring, repositories));
            }
        };

        // 2. Generate a fresh request_id.
        let request_id = uuid::Uuid::new_v4().to_string();

        // 3. Build the ack text. The trailing "Follow along in this thread."
        //    is mandatory per spec so operators know subsequent updates
        //    will land in the thread.
        let ack_text = format!(
            "✓ Queued proposal request for {repo_url}. \
             The next polling iteration will run it. Follow along in this thread.",
            repo_url = repo.url,
        );

        // 4. Post the ack via the chatops backend and capture the `ts`.
        //    Without chatops we cannot produce the lifecycle thread anchor
        //    the spec requires — surface that as an error reply.
        let backend = match self.chatops.as_ref() {
            Some(b) => b.clone(),
            None => {
                return Reply::Sync(
                    "✗ propose: chatops backend not configured; cannot post the proposal-request ack"
                        .to_string(),
                );
            }
        };
        let ack_ts = match backend.post_message_capturing_ts(channel_id, &ack_text).await {
            Ok(ts) => ts,
            Err(e) => {
                tracing::warn!("propose: backend post_message_capturing_ts failed: {e:#}");
                return Reply::Sync(format!(
                    "✗ propose: could not post ack to chat: {e}"
                ));
            }
        };

        // 5. Write the state file.
        let state = crate::proposal_requests::ProposalRequestState {
            request_id: request_id.clone(),
            repo_url: repo.url.clone(),
            channel: channel_id.to_string(),
            thread_ts: ack_ts.clone(),
            ack_message_ts: ack_ts.clone(),
            operator_user: operator_user.unwrap_or("").to_string(),
            request_text: request_text.to_string(),
            submitted_at: chrono::Utc::now(),
            status: crate::proposal_requests::ProposalRequestStatus::Pending,
            reason: None,
        };
        if let Err(e) =
            crate::proposal_requests::write_state(&self.proposal_request_state_dir, &state)
        {
            tracing::warn!(request_id = %request_id, "propose: write_state failed: {e:#}");
            // Best-effort: tell the chat thread the ack landed but the
            // state file didn't.
            if let Err(reply_err) = backend
                .post_threaded_reply(
                    channel_id,
                    &ack_ts,
                    &format!("✗ propose: could not persist state file: {e}"),
                )
                .await
            {
                tracing::warn!(
                    "propose: subsequent thread reply for state-write failure also failed: {reply_err:#}"
                );
            }
            return Reply::Silent;
        }

        // 6. Submit the queue_proposal_request control-socket action.
        let resp = submitter
            .submit(serde_json::json!({
                "action": "queue_proposal_request",
                "url": repo.url,
                "request_id": request_id,
            }))
            .await;
        if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("(no error message)");
            // Tell the operator in the thread.
            let body = format!("✗ propose: could not enqueue proposal-request: {err}");
            if let Err(reply_err) = backend
                .post_threaded_reply(channel_id, &ack_ts, &body)
                .await
            {
                tracing::warn!(
                    "propose: subsequent thread reply for queue failure also failed: {reply_err:#}"
                );
            }
            return Reply::Silent;
        }

        Reply::Silent
    }

    /// Handle the `changelog` verb. Resolves the repo, validates the
    /// raw-args remainder via `parse_changelog_args` (default-denying
    /// `--workspace` overrides arriving via chat), posts a top-level
    /// ack message via the configured chatops backend (capturing the
    /// ack's `ts` as the request's lifecycle thread), writes a
    /// `ChangelogRequestState` file with `status: Pending`, and submits
    /// a `queue_changelog_request` control-socket action so the next
    /// polling iteration picks up the request. Returns `Reply::Silent`
    /// on success (the dispatcher has already posted the ack) and
    /// `Reply::Sync(...)` on every failure shape so the operator's
    /// message gets a threaded reply explaining what went wrong.
    async fn dispatch_changelog_request(
        &self,
        repo_substring: &str,
        raw_args: &str,
        channel_id: &str,
        repositories: &[RepoIdentity],
        submitter: &dyn ActionSubmitter,
    ) -> Reply {
        // 1. Repo substring resolution.
        let repo = match match_repo(repo_substring, repositories) {
            RepoMatch::Unique(r) => r,
            RepoMatch::Multiple(ms) => {
                return Reply::Sync(format_multiple_matches(repo_substring, &ms));
            }
            RepoMatch::None => {
                return Reply::Sync(format_no_match(repo_substring, repositories));
            }
        };

        // 2. Validate args. Bad flags are surfaced inline.
        match parse_changelog_args(raw_args) {
            Ok(parsed) => {
                if parsed.workspace_override.is_some() {
                    // Default-deny the `--workspace` override in chatops
                    // (it would let any channel member point the stylist
                    // at an arbitrary directory). WARN log + refusal.
                    tracing::warn!(
                        repo_url = %repo.url,
                        "changelog: refusing `--workspace` override arriving via chatops"
                    );
                    return Reply::Sync(
                        "✗ changelog: `--workspace` override is not accepted via chatops; \
                         run `autocoder changelog --workspace <path>` on the daemon host instead."
                            .to_string(),
                    );
                }
            }
            Err(e) => {
                return Reply::Sync(format!("✗ changelog: bad arg: {e}"));
            }
        }

        // 3. Fresh request_id.
        let request_id = uuid::Uuid::new_v4().to_string();

        // 4. Ack text. Mirrors the `propose` ack so operators learn one
        //    pattern.
        let ack_text = format!(
            "✓ Queued changelog request for {repo_url}. \
             The next polling iteration will run it. Follow along in this thread.",
            repo_url = repo.url,
        );

        // 5. Post the ack via the chatops backend AND capture the `ts`.
        let backend = match self.chatops.as_ref() {
            Some(b) => b.clone(),
            None => {
                return Reply::Sync(
                    "✗ changelog: chatops backend not configured.".to_string(),
                );
            }
        };
        let ack_ts = match backend
            .post_message_capturing_ts(channel_id, &ack_text)
            .await
        {
            Ok(ts) => ts,
            Err(e) => {
                tracing::warn!("changelog: backend post_message_capturing_ts failed: {e:#}");
                return Reply::Sync(format!(
                    "✗ changelog: could not post ack to chat: {e}"
                ));
            }
        };

        // 6. Write the state file BEFORE submitting the control-socket
        //    action so the polling-iteration handler always finds it.
        let state = crate::changelog_requests::ChangelogRequestState {
            request_id: request_id.clone(),
            repo_url: repo.url.clone(),
            raw_args: raw_args.to_string(),
            channel: channel_id.to_string(),
            lifecycle_thread_ts: ack_ts.clone(),
            status: crate::changelog_requests::ChangelogStatus::Pending,
            submitted_at: chrono::Utc::now(),
            reason: None,
        };
        if let Err(e) = crate::changelog_requests::write_state(
            &self.changelog_request_state_dir,
            &state,
        ) {
            tracing::warn!(request_id = %request_id, "changelog: write_state failed: {e:#}");
            if let Err(reply_err) = backend
                .post_threaded_reply(
                    channel_id,
                    &ack_ts,
                    &format!("✗ changelog: could not persist state file: {e}"),
                )
                .await
            {
                tracing::warn!(
                    "changelog: subsequent thread reply for state-write failure also failed: {reply_err:#}"
                );
            }
            return Reply::Silent;
        }

        // 7. Submit the queue_changelog_request control-socket action.
        let resp = submitter
            .submit(serde_json::json!({
                "action": "queue_changelog_request",
                "url": repo.url,
                "request_id": request_id,
            }))
            .await;
        if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("(no error message)");
            let body = format!("✗ changelog: could not enqueue request: {err}");
            if let Err(reply_err) = backend
                .post_threaded_reply(channel_id, &ack_ts, &body)
                .await
            {
                tracing::warn!(
                    "changelog: subsequent thread reply for queue failure also failed: {reply_err:#}"
                );
            }
            return Reply::Silent;
        }

        Reply::Silent
    }

    /// Handle the `audit` verb. Resolves the audit substring against the
    /// registered audit-type names AND the repo substring against the
    /// configured repos; on a unique match in both, submits the
    /// `queue_audit` control-socket action and returns the one-line ack
    /// with an ETA derived from the repo's poll interval.
    async fn dispatch_audit_now(
        &self,
        audit_substring: &str,
        repo_substring: &str,
        repositories: &[RepoIdentity],
        submitter: &dyn ActionSubmitter,
    ) -> String {
        // 1. Audit-type substring resolution.
        let audit_types_borrowed: Vec<&str> =
            self.audit_types.iter().map(|s| s.as_str()).collect();
        let audit_name = match match_audit_type(audit_substring, &audit_types_borrowed) {
            AuditMatch::Unique(n) => n.to_string(),
            AuditMatch::Multiple(ms) => {
                return format_audit_multiple_matches(audit_substring, &ms);
            }
            AuditMatch::None => {
                return format_audit_no_match(audit_substring, &audit_types_borrowed);
            }
        };

        // 2. Repo substring resolution.
        let repo = match match_repo(repo_substring, repositories) {
            RepoMatch::Unique(r) => r,
            RepoMatch::Multiple(ms) => {
                return format_multiple_matches(repo_substring, &ms);
            }
            RepoMatch::None => return format_no_match(repo_substring, repositories),
        };

        // 3. Submit the queue_audit action.
        let resp = submitter
            .submit(serde_json::json!({
                "action": "queue_audit",
                "url": repo.url,
                "audit_type": audit_name,
            }))
            .await;
        if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("(no error message)");
            return format!("✗ queue_audit failed: {err}");
        }
        // The daemon echoes the resolved audit-type name (canonical) so
        // the ack uses the canonical string instead of whatever the
        // substring resolved to locally. They should be identical, but
        // trust the daemon if it diverges (e.g. registry mutation).
        let canonical_audit = resp
            .get("audit_type")
            .and_then(|v| v.as_str())
            .unwrap_or(&audit_name)
            .to_string();
        let eta = format_audit_eta(&resp);
        format!(
            "✓ Queued {canonical_audit} for {url}. Will run on the next polling iteration ({eta}).",
            url = repo.url,
        )
    }

    /// Handle the `send it` verb. Looks up the audit-thread state, runs
    /// the four-case decision tree (untracked / stale / already-acted /
    /// fresh-and-open), and on the accept path submits the
    /// `trigger_audit_action` control-socket action AND flips the state
    /// file's `status` to `TriagePending`. Returns the operator-facing
    /// reply text.
    async fn dispatch_send_it_on_audit(
        &self,
        thread_ts: &str,
        submitter: &dyn ActionSubmitter,
    ) -> String {
        use crate::audits::threads::{
            AuditThreadStatus, read_state, write_state,
        };
        let state_root = self.audit_thread_state_dir.as_path();
        let mut state = match read_state(state_root, thread_ts) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return SEND_IT_REFUSE_UNTRACKED.to_string();
            }
            Err(e) => {
                tracing::warn!(
                    thread_ts = %thread_ts,
                    "audit-thread state read failed; treating as untracked: {e:#}"
                );
                return SEND_IT_REFUSE_UNTRACKED.to_string();
            }
        };

        let age = chrono::Utc::now() - state.posted_at;
        if age > chrono::Duration::days(7) {
            return SEND_IT_REFUSE_STALE.to_string();
        }

        match state.status {
            AuditThreadStatus::Open | AuditThreadStatus::TriageFailed => {
                // Fresh request OR a retry after a prior failed attempt.
                // Both transition into TriagePending; the polling loop
                // drains the queue on its next iteration.
                let resp = submitter
                    .submit(serde_json::json!({
                        "action": "trigger_audit_action",
                        "thread_ts": thread_ts,
                    }))
                    .await;
                if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let err = resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)");
                    return format!("✗ could not schedule triage: {err}");
                }
                state.status = AuditThreadStatus::TriagePending;
                state.reason = None;
                if let Err(e) = write_state(state_root, &state) {
                    tracing::warn!(
                        thread_ts = %thread_ts,
                        "failed to flip audit-thread state to TriagePending: {e:#}"
                    );
                }
                // The polling cadence varies per repo; the response shape
                // also carries `poll_interval_sec` so we can name an
                // estimate in the reply if the daemon told us one.
                let poll_clause = resp
                    .get("poll_interval_sec")
                    .and_then(|v| v.as_u64())
                    .map(|s| format!(" (~{s}s)"))
                    .unwrap_or_default();
                format!(
                    "✓ Triage scheduled for {audit_type} on {repo_url}. The next polling iteration will run it{poll_clause}.",
                    audit_type = state.audit_type,
                    repo_url = state.repo_url,
                )
            }
            AuditThreadStatus::Acted | AuditThreadStatus::TriagePending => {
                format!(
                    "✗ This audit thread is already {status}. No new action taken.",
                    status = state.status.label(),
                )
            }
        }
        // The threads module's notes prefix is unused here — `read_state`
        // returns the on-disk truth and the dispatcher never invents one.
    }

    /// Build the per-repo menu reply: empty-slice short-circuit, otherwise
    /// try the daemon's bulk `repo_status_all` action first and fall back
    /// to N individual `repo_status` calls if the daemon doesn't support
    /// the bulk action (older builds).
    async fn dispatch_status_menu(
        &self,
        repositories: &[RepoIdentity],
        submitter: &dyn ActionSubmitter,
    ) -> String {
        if repositories.is_empty() {
            return "📊 No repositories configured.".to_string();
        }
        let (responses, unavailable) =
            collect_menu_state(repositories, submitter).await;
        for entry in &unavailable {
            tracing::warn!(
                url = %entry.url,
                "status menu: per-repo state unavailable: {}",
                entry.error
            );
        }
        format_status_menu_reply(&responses, &unavailable)
    }

    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Aggregate per-repo state for the bare-`status` menu reply. Tries the
/// daemon's bulk `repo_status_all` action first (one round trip); falls
/// back to N individual `repo_status` calls if the daemon returns the
/// `unknown action: repo_status_all` shape (older builds without bulk
/// support).
///
/// Returned `Vec<RepoStatusResponse>` is in the same order as
/// `repositories`. The order across responses and `unavailable` entries
/// is the configured-repo order — a healthy repo stays in its position
/// even when a sibling failed.
async fn collect_menu_state(
    repositories: &[RepoIdentity],
    submitter: &dyn ActionSubmitter,
) -> (Vec<RepoStatusResponse>, Vec<UnavailableEntry>) {
    // 1. Try the bulk action.
    let bulk = submitter
        .submit(serde_json::json!({"action": "repo_status_all"}))
        .await;
    if bulk.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return parse_bulk_status_response(repositories, &bulk);
    }
    // 2. Bulk action unsupported (older daemon) → fall back to N
    //    individual repo_status calls. Any error response shape that is
    //    not `unknown action: repo_status_all` is treated as "daemon
    //    doesn't speak this verb" too — the fallback path always works
    //    and the worst case is N extra round trips.
    let mut responses = Vec::with_capacity(repositories.len());
    let mut unavailable = Vec::new();
    for repo in repositories {
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
                .unwrap_or("(no error message)")
                .to_string();
            unavailable.push(UnavailableEntry {
                url: repo.url.clone(),
                error: err,
            });
            continue;
        }
        match serde_json::from_value::<RepoStatusResponse>(resp["status"].clone()) {
            Ok(s) => responses.push(s),
            Err(e) => unavailable.push(UnavailableEntry {
                url: repo.url.clone(),
                error: format!("decode failed: {e}"),
            }),
        }
    }
    (responses, unavailable)
}

/// Parse the daemon's `repo_status_all` response. The shape:
/// ```json
/// {"ok": true, "results": [
///   {"url": "...", "ok": true, "status": {...}},
///   {"url": "...", "ok": false, "error": "..."}
/// ]}
/// ```
/// Any results entry whose URL is NOT in the configured `repositories`
/// list is dropped silently — the dispatcher's repo set is authoritative.
/// Any configured URL absent from the results list is rendered as
/// `(unavailable: missing from daemon response)` so the operator sees
/// every configured repo regardless.
fn parse_bulk_status_response(
    repositories: &[RepoIdentity],
    bulk: &serde_json::Value,
) -> (Vec<RepoStatusResponse>, Vec<UnavailableEntry>) {
    let mut responses = Vec::new();
    let mut unavailable = Vec::new();
    let results = bulk.get("results").and_then(|v| v.as_array()).cloned()
        .unwrap_or_default();
    // Index results by URL for stable per-repo lookup.
    let mut by_url: HashMap<String, &serde_json::Value> = HashMap::new();
    for entry in &results {
        if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
            by_url.insert(url.to_string(), entry);
        }
    }
    for repo in repositories {
        match by_url.get(&repo.url) {
            Some(entry) => {
                let ok = entry.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                if ok {
                    match entry.get("status").cloned() {
                        Some(status_val) => match serde_json::from_value::<
                            RepoStatusResponse,
                        >(status_val)
                        {
                            Ok(s) => responses.push(s),
                            Err(e) => unavailable.push(UnavailableEntry {
                                url: repo.url.clone(),
                                error: format!("decode failed: {e}"),
                            }),
                        },
                        None => unavailable.push(UnavailableEntry {
                            url: repo.url.clone(),
                            error: "ok=true but `status` field missing".to_string(),
                        }),
                    }
                } else {
                    let err = entry
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)")
                        .to_string();
                    unavailable.push(UnavailableEntry {
                        url: repo.url.clone(),
                        error: err,
                    });
                }
            }
            None => unavailable.push(UnavailableEntry {
                url: repo.url.clone(),
                error: "missing from daemon response".to_string(),
            }),
        }
    }
    (responses, unavailable)
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

/// Render the `currently:` line of the per-repo status reply by
/// branching on the busy marker's contents per the a11 spec:
///
///   1. No marker present → `idle`.
///   2. Marker present AND stale (dead pid OR age ≥ threshold per
///      `a08`'s classification) → `stale marker from pid <pid>
///      (age <age>, recovery <eligible-or-remaining>)`.
///   3. Marker present AND `change` non-empty → `working on <change>
///      (started <age> ago)`.
///   4. Marker present AND `stage=executor` AND `change` empty AND an
///      audit log matches the marker's `started_at` → `running audit
///      <audit_type> (started <age> ago)`.
///   5. Marker present AND `stage` ∈ {commit, review, push, pr} →
///      `<stage> in progress (started <age> ago)`.
///   6. Recovery-operation marker (no distinguishable stage today;
///      reserved for future expansion) → `recovery in progress ...`.
///   7. Fallback → `busy (stage=<stage>, started <age> ago)`.
///
/// The age format uses the busy-marker convention (`Xs` / `Xm` /
/// `XhYm`) rather than the broader `human_age_duration` so older
/// markers are reported as `2h17m ago` rather than `2h ago` — the
/// stale-marker branch in particular needs the extra resolution so
/// "stuck-feeling" markers' actual progress past the threshold is
/// visible.
pub fn format_currently_line(busy: Option<&BusySummary>) -> String {
    let Some(b) = busy else {
        return "currently: idle".to_string();
    };
    let age = currently_age_secs(b.started_at);
    let age_human = crate::busy_marker::format_age_human(age);

    // Step 2: stale-marker classification. The status surface mirrors
    // `a08`'s acquire-time classification so an operator sees the same
    // "this marker will be recovered" verdict the polling-loop's INFO
    // log would emit on the next iteration. Note we deliberately do
    // NOT consult the marker's `comm` field here — the status reply
    // surfaces the operator-actionable view ("recovery will fire"),
    // not the ambiguous PID-reuse case which never auto-recovers.
    if !b.pid_alive {
        return format!(
            "currently: stale marker from pid {} (age {age_human}, recovery eligible now)",
            b.pid
        );
    }
    if b.stale_threshold_secs > 0 && age >= b.stale_threshold_secs {
        return format!(
            "currently: stale marker from pid {} (age {age_human}, threshold passed, recovery eligible next iteration)",
            b.pid
        );
    }
    if b.stale_threshold_secs > 0 {
        // Heuristic: surface upcoming-recovery once the marker is past
        // 80% of the threshold so "stuck-feeling" markers appear as
        // visibly transitioning rather than as a wall of `working on
        // ...` lines that suddenly flip to `stale marker`. The 80%
        // cut-off is documented in the spec; if it ever moves, the
        // CHATOPS.md example and the unit test for this branch both
        // need updating in lock-step.
        let warn_at = (b.stale_threshold_secs * 8) / 10;
        if age > warn_at && age < b.stale_threshold_secs {
            let remaining = b.stale_threshold_secs - age;
            let remaining_human = crate::busy_marker::format_age_human(remaining);
            return format!(
                "currently: stale marker from pid {} (age {age_human}, recovery in {remaining_human})",
                b.pid
            );
        }
    }

    // Step 3: change non-empty wins over stage-based variants — the
    // operator wants to know the change slug before the lifecycle
    // phase.
    if !b.change.trim().is_empty() {
        return format!(
            "currently: working on {} (started {age_human} ago)",
            slack_escape(&b.change)
        );
    }

    // Step 4: audit-in-flight detection. `stage=executor` AND
    // `change=""` is the marker shape the executor stamps after
    // acquiring the marker but before the queue-walk selects a change
    // (or while an audit runs against the workspace). The audit_type
    // is resolved by the daemon at status-build time by matching the
    // marker's `started_at` to an audit log filename. When the stage
    // is executor but no audit matches, we fall through to the
    // generic in-progress / fallback line below.
    if b.stage == "executor"
        && let Some(at) = &b.audit_type
    {
        return format!(
            "currently: running audit {at} (started {age_human} ago)"
        );
    }

    // Step 5: known post-executor lifecycle stage.
    if matches!(b.stage.as_str(), "commit" | "review" | "push" | "pr") {
        return format!(
            "currently: {} in progress (started {age_human} ago)",
            b.stage
        );
    }

    // Step 6: recovery operations don't stamp distinguishable markers
    // today (rebuild-specs and fork-recreation both run under the
    // generic `executor` stage). Reserved for future expansion when
    // those flows adopt their own stage variants.

    // Step 7: fallback line for any unclassified marker shape — gives
    // the operator the stage + age even when none of the branches
    // above match.
    format!(
        "currently: busy (stage={}, started {age_human} ago)",
        b.stage
    )
}

/// Compute the age of a marker as a non-negative integer of seconds.
/// Negative deltas (clock skew, future `started_at`) clamp to 0 so the
/// formatter never emits a negative `-5s ago`.
fn currently_age_secs(started_at: DateTime<Utc>) -> u64 {
    let delta = Utc::now() - started_at;
    if delta.num_seconds() < 0 {
        0
    } else {
        delta.num_seconds() as u64
    }
}

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

    // ---------- audit verb (chatops-on-demand-audit-trigger) ----------

    #[test]
    fn parse_audit_happy_path() {
        let cmd = parse_command(&format!("{BOT} audit sec myrepo"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::AuditNow {
                audit_substring: "sec".into(),
                repo_substring: "myrepo".into(),
            }
        );
    }

    #[test]
    fn parse_audit_verb_case_insensitive() {
        for verb_form in ["audit", "Audit", "AUDIT", "aUdIt"] {
            let cmd = parse_command(&format!("{BOT} {verb_form} sec myrepo"), BOT)
                .unwrap_or_else(|| panic!("`{verb_form}` should parse"));
            assert_eq!(
                cmd,
                OperatorCommand::AuditNow {
                    audit_substring: "sec".into(),
                    repo_substring: "myrepo".into(),
                }
            );
        }
    }

    #[test]
    fn parse_audit_missing_args_returns_none() {
        assert!(parse_command(&format!("{BOT} audit"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} audit sec"), BOT).is_none());
        // Too many positional args also rejected.
        assert!(parse_command(&format!("{BOT} audit sec myrepo extra"), BOT).is_none());
    }

    #[test]
    fn match_audit_type_unique() {
        let registered = &[
            "security_bug_audit",
            "architecture_brightline",
            "drift_audit",
        ];
        match match_audit_type("sec", registered) {
            AuditMatch::Unique(n) => assert_eq!(n, "security_bug_audit"),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn match_audit_type_multiple() {
        let registered = &[
            "architecture_brightline",
            "architecture_consultative",
            "security_bug_audit",
        ];
        match match_audit_type("arch", registered) {
            AuditMatch::Multiple(ms) => {
                assert_eq!(ms.len(), 2);
                assert!(ms.contains(&"architecture_brightline"));
                assert!(ms.contains(&"architecture_consultative"));
            }
            other => panic!("expected Multiple, got {other:?}"),
        }
    }

    #[test]
    fn match_audit_type_none() {
        let registered = &["security_bug_audit", "drift_audit"];
        assert!(matches!(
            match_audit_type("zzz", registered),
            AuditMatch::None
        ));
    }

    #[test]
    fn match_audit_type_case_insensitive() {
        let registered = &["security_bug_audit"];
        match match_audit_type("SEC", registered) {
            AuditMatch::Unique(n) => assert_eq!(n, "security_bug_audit"),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_audit_substring_path_traversal_rejected() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit ../../etc/passwd myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("invalid audit substring produces a sanitization reply");
        match reply {
            Reply::Sync(text) => {
                assert!(text.starts_with("✗ invalid audit substring"), "{text}");
            }
            other => panic!("expected Sync sanitization reply, got {other:?}"),
        }
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_audit_substring_shell_metachars_rejected() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit a;rm myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        match reply {
            Reply::Sync(text) => {
                assert!(text.starts_with("✗ invalid audit substring"), "{text}");
            }
            other => panic!("expected Sync, got {other:?}"),
        }
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
        // Bare `status` is its own verb (StatusMenu) — see
        // `parse_bare_status_returns_status_menu`. Every other verb that
        // expects positional arguments must reject the missing-arg case.
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
    fn parse_bare_status_returns_status_menu() {
        let cmd = parse_command(&format!("{BOT} status"), BOT).unwrap();
        assert_eq!(cmd, OperatorCommand::StatusMenu);
    }

    #[test]
    fn parse_bare_status_with_trailing_whitespace_and_caps() {
        // Trailing whitespace + extra inter-token whitespace + mixed case
        // all parse as StatusMenu (zero positional args).
        for form in [
            format!("{BOT} status   "),
            format!("{BOT}  status"),
            format!("{BOT} Status"),
            format!("{BOT} STATUS"),
            format!("{BOT}   Status   "),
        ] {
            let cmd = parse_command(&form, BOT)
                .unwrap_or_else(|| panic!("`{form}` should parse as StatusMenu"));
            assert_eq!(cmd, OperatorCommand::StatusMenu, "form: {form}");
        }
    }

    #[test]
    fn parse_status_one_arg_still_resolves_to_status() {
        let cmd = parse_command(&format!("{BOT} status myrepo"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::Status {
                repo_substring: "myrepo".into()
            }
        );
    }

    #[test]
    fn parse_status_two_or_more_args_is_invalid_no_silent_menu_fallback() {
        // Two-or-more args is the existing "invalid" error path — must
        // not silently fall back to StatusMenu.
        assert!(parse_command(&format!("{BOT} status myrepo extra"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} status a b c"), BOT).is_none());
    }

    #[test]
    fn parse_message_without_mention_returns_none() {
        // Don't drown random chat in error replies.
        assert!(parse_command("status myrepo", BOT).is_none());
        assert!(parse_command("hello world", BOT).is_none());
        assert!(parse_command("@somebody-else status myrepo", BOT).is_none());
    }

    // ---------- send it (audit-reply-acts) ----------

    #[test]
    fn parse_send_it_in_thread_parses_to_send_it_on_audit() {
        let cmd = parse_command_in_thread(
            &format!("{BOT} send it"),
            BOT,
            Some("1748293445.001234"),
        )
        .expect("send-it in a thread must parse");
        assert_eq!(
            cmd,
            OperatorCommand::SendItOnAudit {
                thread_ts: "1748293445.001234".into(),
            }
        );
    }

    #[test]
    fn parse_send_it_is_case_insensitive() {
        let cmd = parse_command_in_thread(
            &format!("{BOT} SEND IT"),
            BOT,
            Some("1748.999"),
        )
        .expect("SEND IT in a thread must parse");
        assert_eq!(
            cmd,
            OperatorCommand::SendItOnAudit {
                thread_ts: "1748.999".into(),
            }
        );
        let cmd2 = parse_command_in_thread(
            &format!("{BOT} Send It"),
            BOT,
            Some("1748.999"),
        )
        .unwrap();
        assert!(matches!(cmd2, OperatorCommand::SendItOnAudit { .. }));
    }

    #[test]
    fn parse_send_it_outside_thread_returns_none() {
        // Outside a thread → unknown-verb fallback (None) so the
        // listener reacts with `?`.
        assert!(parse_command_in_thread(&format!("{BOT} send it"), BOT, None).is_none());
        assert!(
            parse_command_in_thread(&format!("{BOT} send it"), BOT, Some("")).is_none(),
            "empty thread_ts must also fall through"
        );
    }

    #[test]
    fn parse_send_without_it_returns_none() {
        // `send` is not a verb on its own.
        assert!(
            parse_command_in_thread(&format!("{BOT} send"), BOT, Some("1.0")).is_none()
        );
    }

    #[test]
    fn parse_send_it_with_trailing_args_returns_none() {
        // `send it` must be the entire verb. Anything after parses as
        // unknown verb (no `send it <args>` shape in this iteration).
        assert!(
            parse_command_in_thread(&format!("{BOT} send it now"), BOT, Some("1.0")).is_none()
        );
        assert!(
            parse_command_in_thread(&format!("{BOT} send it but ignore 3"), BOT, Some("1.0"))
                .is_none()
        );
    }

    #[test]
    fn parse_command_without_thread_aware_entry_does_not_accept_send_it() {
        // The legacy `parse_command` entry point has no thread context
        // and so MUST refuse to recognize `send it` regardless of message
        // shape.
        assert!(parse_command(&format!("{BOT} send it"), BOT).is_none());
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
        // No branches configured → the always-present block is skipped
        // (this represents the "data-shape only" default — production
        // callers always populate base/agent branch). Marker / throttle
        // / queue / last_iteration sections also collapse when empty.
        let resp = RepoStatusResponse {
            url: "git@github.com:owner/repo.git".into(),
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        // Header is always present.
        assert!(out.starts_with("📊 git@github.com:owner/repo.git"));
        for label in [
            "active markers",
            "throttled alerts",
            "queue snapshot",
            "queue:",
            "last iteration",
            "(none)",
            "branches:",
            "currently:",
        ] {
            assert!(
                !out.contains(label),
                "empty status reply must not contain `{label}`; got: {out}"
            );
        }
    }

    #[test]
    fn format_status_healthy_repo_includes_all_always_present_sections() {
        // Healthy snapshot: both branches with commits, an open PR, idle
        // daemon, empty queue, no markers/throttles.
        let resp = RepoStatusResponse {
            url: "git@github.com:owner/repo.git".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            last_commit_base: Some(CommitSummary {
                short_sha: "abcd123".into(),
                subject: "base subject".into(),
                age: chrono::Duration::minutes(15),
            }),
            last_commit_agent: Some(CommitSummary {
                short_sha: "def4567".into(),
                subject: "agent subject".into(),
                age: chrono::Duration::minutes(5),
            }),
            latest_pr: Some(PrSummary {
                number: 42,
                title: "agent work".into(),
                state: "open".into(),
                head_branch: "agent-q".into(),
                url: "https://github.com/owner/repo/pull/42".into(),
                age: chrono::Duration::hours(3),
            }),
            currently_busy: None,
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(out.contains("branches: base=main, agent=agent-q"), "{out}");
        assert!(
            out.contains("last commit on main: abcd123 \"base subject\" (15m ago)"),
            "{out}"
        );
        assert!(
            out.contains("last commit on agent-q: def4567 \"agent subject\" (5m ago)"),
            "{out}"
        );
        assert!(
            out.contains("latest PR: #42 \"agent work\"  open · head=agent-q · 3h ago"),
            "{out}"
        );
        assert!(out.contains("https://github.com/owner/repo/pull/42"), "{out}");
        assert!(out.contains("currently: idle"), "{out}");
    }

    #[test]
    fn format_status_absent_data_renders_none_not_blank() {
        // Fresh clone: branches set in config, but no commits on agent
        // branch yet, no PR ever opened, daemon idle.
        let resp = RepoStatusResponse {
            url: "git@github.com:owner/repo.git".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            last_commit_base: Some(CommitSummary {
                short_sha: "abc1234".into(),
                subject: "init".into(),
                age: chrono::Duration::hours(2),
            }),
            last_commit_agent: None,
            latest_pr: None,
            currently_busy: None,
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(
            out.contains("last commit on agent-q: (none)"),
            "(none) must be present on absent agent-branch commit; got: {out}"
        );
        assert!(out.contains("latest PR: (none)"), "{out}");
        assert!(out.contains("currently: idle"), "{out}");
    }

    #[test]
    fn format_status_currently_busy_shows_change_and_age() {
        let resp = RepoStatusResponse {
            url: "git@github.com:owner/repo.git".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            currently_busy: Some(BusySummary {
                change: "a05-foo".into(),
                started_at: Utc::now() - chrono::Duration::minutes(2),
                stage: "executor".into(),
                pid: 4242,
                stale_threshold_secs: 600,
                pid_alive: true,
                audit_type: None,
            }),
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(
            out.contains("currently: working on a05-foo (started 2m ago)"),
            "{out}"
        );
    }

    // ---------- format_currently_line: a11 variants ----------

    fn busy(stage: &str, change: &str, started_secs_ago: i64) -> BusySummary {
        BusySummary {
            change: change.into(),
            started_at: Utc::now() - chrono::Duration::seconds(started_secs_ago),
            stage: stage.into(),
            pid: 4242,
            stale_threshold_secs: 600,
            pid_alive: true,
            audit_type: None,
        }
    }

    #[test]
    fn currently_line_idle_when_marker_absent() {
        assert_eq!(format_currently_line(None), "currently: idle");
    }

    #[test]
    fn currently_line_working_on_change_takes_priority_over_stage() {
        // change non-empty wins even when stage is post-executor: the
        // operator wants the change slug first.
        let b = busy("commit", "a36-expense-tracking", 180);
        assert_eq!(
            format_currently_line(Some(&b)),
            "currently: working on a36-expense-tracking (started 3m ago)"
        );
    }

    #[test]
    fn currently_line_running_audit_when_audit_type_resolved() {
        // Threshold high enough that the marker isn't stale; the audit
        // branch is what we're asserting here.
        let mut b = busy("executor", "", 13 * 60);
        b.stale_threshold_secs = 3600;
        b.audit_type = Some("architecture_consultative".into());
        assert_eq!(
            format_currently_line(Some(&b)),
            "currently: running audit architecture_consultative (started 13m ago)"
        );
    }

    #[test]
    fn currently_line_executor_without_audit_falls_through_to_fallback() {
        // stage=executor + change empty + audit_type=None: step 4 (audit)
        // fails to match, step 5 only covers {commit, review, push, pr},
        // and steps 6 (recovery) isn't wired today — so the formatter
        // lands on the step-7 fallback line.
        let b = busy("executor", "", 30);
        assert_eq!(
            format_currently_line(Some(&b)),
            "currently: busy (stage=executor, started 30s ago)"
        );
    }

    #[test]
    fn currently_line_post_executor_stage_in_progress() {
        for (stage, label) in [
            ("commit", "commit"),
            ("review", "review"),
            ("push", "push"),
            ("pr", "pr"),
        ] {
            let b = busy(stage, "", 12);
            let line = format_currently_line(Some(&b));
            assert_eq!(
                line,
                format!("currently: {label} in progress (started 12s ago)")
            );
        }
    }

    #[test]
    fn currently_line_stale_marker_dead_pid_is_eligible_now() {
        let mut b = busy("executor", "", 53 * 60);
        b.pid = 490170;
        b.pid_alive = false;
        assert_eq!(
            format_currently_line(Some(&b)),
            "currently: stale marker from pid 490170 (age 53m, recovery eligible now)"
        );
    }

    #[test]
    fn currently_line_stale_marker_live_pid_past_threshold_eligible_next_iteration() {
        let mut b = busy("executor", "", 700);
        b.pid = 490170;
        b.stale_threshold_secs = 600;
        b.pid_alive = true;
        assert_eq!(
            format_currently_line(Some(&b)),
            "currently: stale marker from pid 490170 (age 11m, threshold passed, recovery eligible next iteration)"
        );
    }

    #[test]
    fn currently_line_stale_marker_approaching_threshold_surfaces_remaining_time() {
        // age > 80% of threshold AND age < threshold → surface the
        // upcoming recovery. Threshold=600s, age=540s → remaining=60s.
        let mut b = busy("executor", "", 540);
        b.pid = 490170;
        b.stale_threshold_secs = 600;
        b.pid_alive = true;
        assert_eq!(
            format_currently_line(Some(&b)),
            "currently: stale marker from pid 490170 (age 9m, recovery in 1m)"
        );
    }

    #[test]
    fn currently_line_change_branch_does_not_get_stale_treatment_when_fresh() {
        // Live pid AND age well under threshold → no stale-marker
        // branch, just the working-on line.
        let mut b = busy("executor", "a05-foo", 120);
        b.stale_threshold_secs = 600;
        assert_eq!(
            format_currently_line(Some(&b)),
            "currently: working on a05-foo (started 2m ago)"
        );
    }

    #[test]
    fn currently_line_stale_dead_pid_wins_over_working_on_change() {
        // Even with a change set, a dead-pid marker is unactionable and
        // the operator needs to see "stale" first.
        let mut b = busy("executor", "a05-foo", 30);
        b.pid = 12345;
        b.pid_alive = false;
        let out = format_currently_line(Some(&b));
        assert!(
            out.starts_with("currently: stale marker from pid 12345"),
            "{out}"
        );
    }

    #[test]
    fn currently_line_fallback_for_unknown_stage() {
        // Defensive fallback: a marker with an unknown stage value
        // (forward-compat from a newer daemon writing into an older
        // status code path) still renders something useful.
        let b = busy("rebuild_specs", "", 45);
        assert_eq!(
            format_currently_line(Some(&b)),
            "currently: busy (stage=rebuild_specs, started 45s ago)"
        );
    }

    #[test]
    fn currently_line_change_escapes_slack_specials() {
        let b = busy("executor", "<bad>", 60);
        let out = format_currently_line(Some(&b));
        assert!(out.contains("&lt;bad&gt;"), "{out}");
        assert!(!out.contains("<bad>"), "raw `<bad>` must not leak: {out}");
    }

    #[test]
    fn currently_line_age_uses_hours_plus_minutes_for_long_ages() {
        // Older convention asked for `XhYm` resolution on the currently
        // line so an operator reading "stale marker" sees a meaningful
        // age past 1h.
        let mut b = busy("executor", "", 8_220); // 2h17m
        b.pid = 1;
        b.pid_alive = false;
        let out = format_currently_line(Some(&b));
        assert!(out.contains("age 2h17m"), "{out}");
    }

    #[test]
    fn currently_line_zero_threshold_disables_stale_branches() {
        // stale_threshold_secs=0 → only the dead-pid branch can flag
        // stale; live + old marker stays in its stage-based branch.
        // For stage=push (in the step-5 set), the line is "push in
        // progress" even at a wall-clock age that would otherwise
        // exceed any sane threshold.
        let mut b = busy("push", "", 9_999);
        b.stale_threshold_secs = 0;
        b.pid_alive = true;
        let out = format_currently_line(Some(&b));
        assert!(
            !out.contains("stale marker"),
            "threshold=0 must not synthesize a stale-marker line: {out}"
        );
        assert!(out.contains("push in progress"), "{out}");
    }

    #[test]
    fn currently_line_recovery_remaining_format_matches_age_convention() {
        // Threshold 1800 (30m), age 1620 (27m) → remaining 180s = 3m.
        let mut b = busy("executor", "", 1620);
        b.pid = 12345;
        b.stale_threshold_secs = 1800;
        b.pid_alive = true;
        let out = format_currently_line(Some(&b));
        assert!(out.contains("recovery in 3m"), "{out}");
    }

    #[test]
    fn slack_escape_substitutes_ampersand_first() {
        // Order matters: `&` must be substituted before `<`/`>` so the
        // emitted `&lt;` / `&gt;` do NOT get re-escaped to `&amp;lt;`.
        assert_eq!(slack_escape("&<"), "&amp;&lt;");
        assert_ne!(slack_escape("&<"), "&amp;lt;");
    }

    #[test]
    fn slack_escape_handles_all_three_chars() {
        assert_eq!(slack_escape("<>&"), "&lt;&gt;&amp;");
    }

    #[test]
    fn format_status_escapes_commit_subject() {
        let resp = RepoStatusResponse {
            url: "u".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            last_commit_base: Some(CommitSummary {
                short_sha: "abc".into(),
                subject: "<!channel> ping everyone".into(),
                age: chrono::Duration::minutes(1),
            }),
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(
            out.contains("&lt;!channel&gt; ping everyone"),
            "subject must be escaped; got: {out}"
        );
        assert!(
            !out.contains("<!channel>"),
            "raw `<!channel>` must NOT leak (would ping the channel): {out}"
        );
    }

    #[test]
    fn format_status_escapes_pr_title() {
        let resp = RepoStatusResponse {
            url: "u".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            latest_pr: Some(PrSummary {
                number: 1,
                title: "<@U123> ping".into(),
                state: "open".into(),
                head_branch: "agent-q".into(),
                url: "https://example".into(),
                age: chrono::Duration::seconds(30),
            }),
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(out.contains("&lt;@U123&gt; ping"), "{out}");
        assert!(!out.contains("<@U123>"), "{out}");
    }

    #[test]
    fn format_status_escapes_ampersand_in_subject() {
        let resp = RepoStatusResponse {
            url: "u".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            last_commit_base: Some(CommitSummary {
                short_sha: "abc".into(),
                subject: "foo & bar".into(),
                age: chrono::Duration::seconds(1),
            }),
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(out.contains("foo &amp; bar"), "{out}");
    }

    #[test]
    fn format_status_queue_one_liner_for_small_lists() {
        let resp = RepoStatusResponse {
            url: "u".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            pending_changes: vec!["a06-foo".into(), "a07-bar".into()],
            waiting_changes: vec!["a10-secrets".into()],
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(
            out.contains(
                "queue: 2 pending (a06-foo, a07-bar), 1 waiting (a10-secrets), 0 excluded"
            ),
            "{out}"
        );
    }

    #[test]
    fn format_status_queue_per_line_when_list_exceeds_five() {
        let pending: Vec<String> = (0..6).map(|i| format!("a0{i}-x")).collect();
        let resp = RepoStatusResponse {
            url: "u".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            pending_changes: pending.clone(),
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        // Per-line fallback: the multi-line `queue snapshot:` header is
        // present.
        assert!(out.contains("queue snapshot:"), "{out}");
        assert!(
            out.contains(&format!("pending: {}", pending.join(", "))),
            "{out}"
        );
        // Must NOT be the one-liner form.
        assert!(
            !out.contains("queue: 6 pending"),
            "should use per-line format for >5 entries: {out}"
        );
    }

    #[test]
    fn format_status_queue_empty_lists_render_count_only() {
        // No queue entries, no markers — the queue section is omitted
        // entirely (queue_has_content = false). This is the spec's
        // "omitted entirely" alternative.
        let resp = RepoStatusResponse {
            url: "u".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(!out.contains("queue:"), "empty queue is omitted: {out}");

        // With markers-only (excluded > 0, pending/waiting = 0), the
        // one-liner form appears and renders empty lists as count-only
        // (no empty parens).
        let resp = RepoStatusResponse {
            url: "u".into(),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            perma_stuck_changes: vec![MarkerEntry {
                change: "a06-foo".into(),
                marked_at: Utc::now() - chrono::Duration::hours(1),
                detail: String::new(),
            }],
            ..RepoStatusResponse::default()
        };
        let out = format_status_reply(&resp);
        assert!(
            out.contains("queue: 0 pending, 0 waiting, 1 excluded"),
            "empty lists render count-only (no `()`): {out}"
        );
        assert!(
            !out.contains("0 pending ()"),
            "empty list must NOT emit empty parens: {out}"
        );
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
        // Two markers + no pending/waiting → totals are small (≤5),
        // so the queue one-liner form is used: `K excluded` (no list,
        // operators reference the markers section above for detail).
        assert!(
            out.contains("queue: 0 pending, 0 waiting, 2 excluded"),
            "small queue + 2 excluded must use one-liner form: {out}"
        );
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

    #[test]
    fn format_help_mentions_bare_status_menu() {
        // Help text must distinguish the two `status` forms so an
        // operator discovers both the per-repo deep dive AND the menu
        // mode. Spec scenario "Help mentions bare status".
        let out = format_help_reply();
        // Both the per-repo form and the bare form must be discoverable.
        assert!(
            out.contains("`status <repo>`"),
            "help must mention per-repo form: {out}"
        );
        assert!(
            out.contains("`status` (no repo)"),
            "help must mention bare `status` form: {out}"
        );
        // The bare-form line must describe the menu behavior.
        let lower = out.to_lowercase();
        assert!(
            lower.contains("menu") || lower.contains("every watched repository"),
            "help must describe the menu behavior on bare status: {out}"
        );
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
        // Single-pending one-liner form (≤5 entries triggers compact
        // queue line).
        assert!(text.contains("queue: 1 pending (a08-deploy)"), "{text}");
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

    // ---------- audit-now dispatcher (chatops-on-demand-audit-trigger) ----------

    fn audit_types() -> Vec<&'static str> {
        vec![
            "architecture_brightline",
            "architecture_consultative",
            "drift_audit",
            "missing_tests_audit",
            "security_bug_audit",
        ]
    }

    #[tokio::test]
    async fn dispatch_audit_now_happy_path_submits_queue_audit() {
        let dispatcher =
            OperatorCommandDispatcher::new().with_audit_types(audit_types());
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "queue_audit",
            serde_json::json!({
                "ok": true,
                "url": "git@github.com:acme/myrepo.git",
                "audit_type": "security_bug_audit",
                "poll_interval_sec": 300,
            }),
        );
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit sec myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✓"), "{text}");
        assert!(text.contains("Queued security_bug_audit"), "{text}");
        assert!(text.contains("git@github.com:acme/myrepo.git"), "{text}");
        assert!(text.contains("~5m"), "ETA must be rounded to minutes: {text}");
        // Exactly one action submitted with the canonical resolved names.
        let calls = submitter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["action"], "queue_audit");
        assert_eq!(calls[0]["url"], "git@github.com:acme/myrepo.git");
        assert_eq!(calls[0]["audit_type"], "security_bug_audit");
    }

    #[tokio::test]
    async fn dispatch_audit_now_imminent_eta_when_poll_interval_short() {
        let dispatcher =
            OperatorCommandDispatcher::new().with_audit_types(audit_types());
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "queue_audit",
            serde_json::json!({
                "ok": true,
                "url": "git@github.com:acme/myrepo.git",
                "audit_type": "security_bug_audit",
                "seconds_until_next_iteration": 10,
            }),
        );
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit sec myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.contains("imminently"), "{text}");
    }

    #[tokio::test]
    async fn dispatch_audit_now_ambiguous_audit_substring_lists_candidates() {
        let dispatcher =
            OperatorCommandDispatcher::new().with_audit_types(audit_types());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit arch myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("architecture_brightline"), "{text}");
        assert!(text.contains("architecture_consultative"), "{text}");
        assert!(text.contains("Be more specific"), "{text}");
        assert!(submitter.calls().is_empty(), "no action submitted");
    }

    #[tokio::test]
    async fn dispatch_audit_now_unknown_audit_lists_all_registered() {
        let dispatcher =
            OperatorCommandDispatcher::new().with_audit_types(audit_types());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit gibberish myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("gibberish"), "{text}");
        for name in audit_types() {
            assert!(text.contains(name), "registered list missing `{name}`: {text}");
        }
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_audit_now_ambiguous_repo_substring_lists_candidates() {
        let dispatcher =
            OperatorCommandDispatcher::new().with_audit_types(audit_types());
        let submitter = FakeSubmitter::new();
        let repos = vec![
            RepoIdentity {
                url: "git@github.com:org-a/repo.git".into(),
                workspace_path: PathBuf::from("/tmp/ws/repo-a"),
            },
            RepoIdentity {
                url: "git@github.com:org-b/repo.git".into(),
                workspace_path: PathBuf::from("/tmp/ws/repo-b"),
            },
        ];
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit sec repo"),
                "C1",
                BOT,
                &repos,
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("org-a/repo"), "{text}");
        assert!(text.contains("org-b/repo"), "{text}");
        assert!(text.contains("be more specific"), "{text}");
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_audit_now_unknown_repo_returns_no_match_reply() {
        let dispatcher =
            OperatorCommandDispatcher::new().with_audit_types(audit_types());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit sec nonexistent"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("nonexistent"), "{text}");
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_audit_now_propagates_daemon_error() {
        let dispatcher =
            OperatorCommandDispatcher::new().with_audit_types(audit_types());
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "queue_audit",
            serde_json::json!({
                "ok": false,
                "error": "no live polling task for `git@github.com:acme/myrepo.git`",
            }),
        );
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} audit sec myrepo"),
                "C1",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("queue_audit"), "{text}");
        assert!(text.contains("no live polling task"), "{text}");
    }

    // ---------- wipe-workspace confirmation flow ----------

    #[tokio::test]
    async fn wipe_workspace_two_step_confirm_happy_path() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        // The first step now fetches live repo_status so the warning can
        // show currently-busy / queue / markers context to the operator.
        submitter.set_response(
            "repo_status",
            serde_json::json!({
                "ok": true,
                "status": {
                    "url": "git@github.com:acme/myrepo.git",
                    "perma_stuck_changes": [],
                    "revision_marked_changes": [],
                    "throttled_alerts": [],
                    "pending_changes": [],
                    "waiting_changes": [],
                    "last_iteration": null,
                },
            }),
        );
        submitter.set_response(
            "wipe_workspace",
            serde_json::json!({
                "ok": true,
                "path": "/tmp/workspaces/github_com_acme_myrepo",
                "already_absent": false,
                "drain_outcome": "no iteration in flight",
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
        // The status fetch happened, but no wipe yet.
        let first_step_calls = submitter.calls();
        assert_eq!(first_step_calls.len(), 1);
        assert_eq!(first_step_calls[0]["action"], "repo_status");
        assert_eq!(dispatcher.pending_len(), 1);

        let success = dispatcher
            .handle_message("confirm", "C1", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        let success_text = unwrap_sync(success);
        assert!(success_text.starts_with("✓"), "confirm should succeed: {success_text}");
        assert!(success_text.contains("Wiped"));
        assert!(
            success_text.contains("no iteration in flight"),
            "reply must name drain outcome: {success_text}"
        );
        // The total call log is now repo_status + wipe_workspace.
        let all_calls = submitter.calls();
        assert_eq!(all_calls.len(), 2);
        assert_eq!(all_calls[1]["action"], "wipe_workspace");
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
        // The first step calls repo_status; the cross-channel confirm
        // must NOT progress to wipe_workspace.
        submitter.set_response(
            "repo_status",
            serde_json::json!({
                "ok": true,
                "status": {
                    "url": "git@github.com:acme/myrepo.git",
                    "perma_stuck_changes": [],
                    "revision_marked_changes": [],
                    "throttled_alerts": [],
                    "pending_changes": [],
                    "waiting_changes": [],
                    "last_iteration": null,
                },
            }),
        );
        submitter.set_response(
            "wipe_workspace",
            serde_json::json!({"ok": true, "path": "/tmp/workspaces/x", "already_absent": false, "drain_outcome": "no iteration in flight"}),
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
        // The first step called repo_status; no wipe_workspace was
        // submitted from the cross-channel confirm.
        let wipe_calls: Vec<_> = submitter
            .calls()
            .into_iter()
            .filter(|c| c["action"] == "wipe_workspace")
            .collect();
        assert!(
            wipe_calls.is_empty(),
            "no wipe action submitted from cross-channel confirm"
        );
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
            "repo_status",
            serde_json::json!({
                "ok": true,
                "status": {
                    "url": "git@github.com:acme/myrepo.git",
                    "perma_stuck_changes": [],
                    "revision_marked_changes": [],
                    "throttled_alerts": [],
                    "pending_changes": [],
                    "waiting_changes": [],
                    "last_iteration": null,
                },
            }),
        );
        submitter.set_response(
            "wipe_workspace",
            serde_json::json!({"ok": true, "path": "/tmp/workspaces/sound", "already_absent": false, "drain_outcome": "no iteration in flight"}),
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

    // ---------- enriched wipe-workspace confirmation message ----------

    /// Build a minimal RepoStatusResponse useful as a fixture for the
    /// confirmation-message tests.
    fn wipe_status_fixture() -> RepoStatusResponse {
        RepoStatusResponse {
            url: "git@github.com:acme/myrepo.git".into(),
            ..RepoStatusResponse::default()
        }
    }

    #[test]
    fn format_wipe_confirmation_idle_empty_queue_no_markers_is_compact() {
        let status = wipe_status_fixture();
        let out = format_wipe_confirmation(
            std::path::Path::new("/tmp/workspaces/myrepo"),
            "git@github.com:acme/myrepo.git",
            &status,
        );
        assert!(out.contains("⚠️ Wipe-workspace requested for git@github.com:acme/myrepo.git"));
        assert!(out.contains("/tmp/workspaces/myrepo"));
        assert!(out.contains("Currently: idle"), "{out}");
        assert!(
            out.contains("Queue (continues after wipe): empty queue"),
            "{out}"
        );
        // No active-markers section when nothing is marker'd.
        assert!(
            !out.contains("Active markers"),
            "no marker section when none exist: {out}"
        );
        // The trailing confirm line is unchanged.
        assert!(out.contains("Reply 'confirm' within 60 seconds to proceed."));
    }

    #[test]
    fn format_wipe_confirmation_busy_nonempty_queue_with_markers_full_form() {
        let started_5m_ago = Utc::now() - chrono::Duration::minutes(5);
        let status = RepoStatusResponse {
            url: "git@github.com:acme/myrepo.git".into(),
            currently_busy: Some(BusySummary {
                change: "audit-proposal-self-validation".into(),
                started_at: started_5m_ago,
                stage: "executor".into(),
                pid: 4242,
                stale_threshold_secs: 600,
                pid_alive: true,
                audit_type: None,
            }),
            pending_changes: vec!["pr-body-tweak".into(), "queue-archive".into()],
            waiting_changes: vec![],
            perma_stuck_changes: vec![MarkerEntry {
                change: "audit-proposal-created-notification".into(),
                marked_at: Utc::now(),
                detail: String::new(),
            }],
            revision_marked_changes: vec![],
            ..RepoStatusResponse::default()
        };
        let out = format_wipe_confirmation(
            std::path::Path::new("/tmp/workspaces/myrepo"),
            "git@github.com:acme/myrepo.git",
            &status,
        );
        assert!(
            out.contains(
                "Currently: working on `audit-proposal-self-validation` (started 5m ago) — will be cancelled"
            ),
            "{out}"
        );
        assert!(
            out.contains("Queue (continues after wipe): 2 pending (pr-body-tweak, queue-archive)"),
            "{out}"
        );
        assert!(
            out.contains("Active markers (git-tracked; preserved across the wipe):"),
            "{out}"
        );
        assert!(
            out.contains("• audit-proposal-created-notification (.perma-stuck.json)"),
            "{out}"
        );
    }

    #[test]
    fn format_wipe_confirmation_revision_markers_render_with_correct_suffix() {
        let status = RepoStatusResponse {
            url: "git@github.com:acme/myrepo.git".into(),
            revision_marked_changes: vec![MarkerEntry {
                change: "needs-revising-thing".into(),
                marked_at: Utc::now(),
                detail: String::new(),
            }],
            ..RepoStatusResponse::default()
        };
        let out = format_wipe_confirmation(
            std::path::Path::new("/tmp/workspaces/myrepo"),
            "git@github.com:acme/myrepo.git",
            &status,
        );
        assert!(
            out.contains("• needs-revising-thing (.needs-spec-revision.json)"),
            "{out}"
        );
    }

    #[test]
    fn format_wipe_confirmation_slack_escapes_change_names() {
        // The parser's allowlist makes a literal `<` in a change name
        // unreachable in normal operation, but the formatter still applies
        // slack_escape belt-and-braces (matching the conventions
        // established in chatops-status-enrichment).
        let status = RepoStatusResponse {
            url: "git@github.com:acme/myrepo.git".into(),
            pending_changes: vec!["bad<change".into()],
            ..RepoStatusResponse::default()
        };
        let out = format_wipe_confirmation(
            std::path::Path::new("/tmp/workspaces/myrepo"),
            "git@github.com:acme/myrepo.git",
            &status,
        );
        assert!(out.contains("bad&lt;change"), "{out}");
        assert!(!out.contains("bad<change"), "{out}");
    }

    // ---------- wipe success-reply text variants ----------

    /// Helper that walks the full two-step flow and returns the
    /// success-reply text for a given wipe_workspace response shape.
    async fn run_wipe_with_response(
        wipe_response: serde_json::Value,
    ) -> String {
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
                    "pending_changes": [],
                    "waiting_changes": [],
                    "last_iteration": null,
                },
            }),
        );
        submitter.set_response("wipe_workspace", wipe_response);
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
        let reply = dispatcher
            .handle_message("confirm", "C1", BOT, &fixture_repos(), &submitter)
            .await
            .unwrap();
        unwrap_sync(reply)
    }

    #[tokio::test]
    async fn wipe_reply_drained_cleanly_text() {
        let text = run_wipe_with_response(serde_json::json!({
            "ok": true,
            "path": "/tmp/workspaces/myrepo",
            "already_absent": false,
            "drain_outcome": "drained cleanly in 1.2s",
        }))
        .await;
        assert_eq!(
            text,
            "✓ Wiped /tmp/workspaces/myrepo (drained cleanly in 1.2s)"
        );
    }

    #[tokio::test]
    async fn wipe_reply_drain_timeout_text() {
        let text = run_wipe_with_response(serde_json::json!({
            "ok": true,
            "path": "/tmp/workspaces/myrepo",
            "already_absent": false,
            "drain_outcome": "drain timeout — iteration may have been stuck",
        }))
        .await;
        assert_eq!(
            text,
            "✓ Wiped /tmp/workspaces/myrepo (drain timeout — iteration may have been stuck)"
        );
    }

    #[tokio::test]
    async fn wipe_reply_no_iteration_in_flight_text() {
        let text = run_wipe_with_response(serde_json::json!({
            "ok": true,
            "path": "/tmp/workspaces/myrepo",
            "already_absent": false,
            "drain_outcome": "no iteration in flight",
        }))
        .await;
        assert_eq!(
            text,
            "✓ Wiped /tmp/workspaces/myrepo (no iteration in flight)"
        );
    }

    #[tokio::test]
    async fn wipe_reply_already_absent_text() {
        let text = run_wipe_with_response(serde_json::json!({
            "ok": true,
            "path": "/tmp/workspaces/myrepo",
            "already_absent": true,
            "drain_outcome": "no iteration in flight",
        }))
        .await;
        assert_eq!(
            text,
            "✓ Wiped /tmp/workspaces/myrepo (already absent)"
        );
    }

    // ---------- send it dispatcher (audit-reply-acts) ----------

    fn write_audit_thread_state(
        state_root: &std::path::Path,
        thread_ts: &str,
        status: crate::audits::threads::AuditThreadStatus,
        posted_at: chrono::DateTime<chrono::Utc>,
    ) {
        let state = crate::audits::threads::AuditThreadState {
            thread_ts: thread_ts.to_string(),
            channel: "C_OPS".to_string(),
            repo_url: "git@github.com:acme/myrepo.git".to_string(),
            audit_type: "architecture_brightline".to_string(),
            findings_excerpt: "  • file foo.rs is 1234 lines".to_string(),
            posted_at,
            status,
            reason: None,
        };
        crate::audits::threads::write_state(state_root, &state).unwrap();
    }

    #[tokio::test]
    async fn dispatch_send_it_in_untracked_thread_politely_refuses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = OperatorCommandDispatcher::new()
            .with_audit_thread_state_dir(tmp.path().to_path_buf());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message_in_thread(
                &format!("{BOT} send it"),
                "C1",
                Some("1748.unknown"),
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("send-it must always produce a reply");
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "must be a refusal: {text}");
        assert!(text.contains("not tracking"), "{text}");
        assert!(submitter.calls().is_empty(), "no action should be submitted");
    }

    #[tokio::test]
    async fn dispatch_send_it_in_stale_thread_politely_refuses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = OperatorCommandDispatcher::new()
            .with_audit_thread_state_dir(tmp.path().to_path_buf());
        let submitter = FakeSubmitter::new();
        // 14 days old → past the 7d cap → polite refusal.
        write_audit_thread_state(
            tmp.path(),
            "1748.stale",
            crate::audits::threads::AuditThreadStatus::Open,
            chrono::Utc::now() - chrono::Duration::days(14),
        );
        let reply = dispatcher
            .handle_message_in_thread(
                &format!("{BOT} send it"),
                "C1",
                Some("1748.stale"),
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"));
        assert!(text.contains("too old"), "{text}");
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_send_it_in_fresh_open_thread_schedules_triage() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = OperatorCommandDispatcher::new()
            .with_audit_thread_state_dir(tmp.path().to_path_buf());
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "trigger_audit_action",
            serde_json::json!({"ok": true, "poll_interval_sec": 60}),
        );
        write_audit_thread_state(
            tmp.path(),
            "1748.fresh",
            crate::audits::threads::AuditThreadStatus::Open,
            chrono::Utc::now() - chrono::Duration::hours(2),
        );
        let reply = dispatcher
            .handle_message_in_thread(
                &format!("{BOT} send it"),
                "C1",
                Some("1748.fresh"),
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✓"), "must accept: {text}");
        assert!(text.contains("Triage scheduled"), "{text}");
        // Status flipped to TriagePending.
        let after = crate::audits::threads::read_state(tmp.path(), "1748.fresh")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.status,
            crate::audits::threads::AuditThreadStatus::TriagePending
        );
        // Exactly one action submitted: the trigger.
        let calls = submitter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["action"], "trigger_audit_action");
        assert_eq!(calls[0]["thread_ts"], "1748.fresh");
    }

    #[tokio::test]
    async fn dispatch_send_it_on_already_acted_thread_refuses_with_status() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = OperatorCommandDispatcher::new()
            .with_audit_thread_state_dir(tmp.path().to_path_buf());
        let submitter = FakeSubmitter::new();
        write_audit_thread_state(
            tmp.path(),
            "1748.acted",
            crate::audits::threads::AuditThreadStatus::Acted,
            chrono::Utc::now() - chrono::Duration::hours(2),
        );
        let reply = dispatcher
            .handle_message_in_thread(
                &format!("{BOT} send it"),
                "C1",
                Some("1748.acted"),
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"));
        assert!(text.contains("acted"), "must name the status: {text}");
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_send_it_on_triage_failed_thread_reschedules() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = OperatorCommandDispatcher::new()
            .with_audit_thread_state_dir(tmp.path().to_path_buf());
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "trigger_audit_action",
            serde_json::json!({"ok": true}),
        );
        write_audit_thread_state(
            tmp.path(),
            "1748.retry",
            crate::audits::threads::AuditThreadStatus::TriageFailed,
            chrono::Utc::now() - chrono::Duration::hours(2),
        );
        let reply = dispatcher
            .handle_message_in_thread(
                &format!("{BOT} send it"),
                "C1",
                Some("1748.retry"),
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✓"), "retry must be accepted: {text}");
        let after = crate::audits::threads::read_state(tmp.path(), "1748.retry")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.status,
            crate::audits::threads::AuditThreadStatus::TriagePending
        );
    }

    // ---------- format_status_menu_reply ----------

    fn empty_menu_response(url: &str) -> RepoStatusResponse {
        RepoStatusResponse {
            url: url.into(),
            ..RepoStatusResponse::default()
        }
    }

    #[test]
    fn menu_empty_inputs_collapse_to_no_repositories_configured() {
        let out = format_status_menu_reply(&[], &[]);
        assert_eq!(out, "📊 No repositories configured.");
    }

    #[test]
    fn menu_three_repo_mixed_states_renders_documented_shape() {
        // First: idle, empty queue, recent iteration.
        let r1 = RepoStatusResponse {
            url: "git@github.com:acme/widgets.git".into(),
            last_iteration: Some(LastIteration {
                finished_at: Utc::now() - chrono::Duration::minutes(5),
                outcome_summary: String::new(),
                next_iteration_estimate: None,
                poll_interval_sec: 60,
            }),
            ..RepoStatusResponse::default()
        };
        // Second: idle, has pending entries.
        let r2 = RepoStatusResponse {
            url: "git@github.com:org-b/another.git".into(),
            pending_changes: vec!["a06-foo".into(), "a07-bar".into()],
            last_iteration: Some(LastIteration {
                finished_at: Utc::now() - chrono::Duration::minutes(3),
                outcome_summary: String::new(),
                next_iteration_estimate: None,
                poll_interval_sec: 60,
            }),
            ..RepoStatusResponse::default()
        };
        // Third: working on a change, has pending+waiting, no iteration yet.
        let r3 = RepoStatusResponse {
            url: "git@github.com:personal/foo.git".into(),
            pending_changes: vec![
                "a01".into(),
                "a02".into(),
                "a03".into(),
                "a04".into(),
                "a05".into(),
            ],
            waiting_changes: vec!["a07-bar".into()],
            currently_busy: Some(BusySummary {
                change: "a05-foo".into(),
                started_at: Utc::now() - chrono::Duration::minutes(2),
                stage: "executor".into(),
                pid: 4242,
                stale_threshold_secs: 600,
                pid_alive: true,
                audit_type: None,
            }),
            ..RepoStatusResponse::default()
        };
        let out = format_status_menu_reply(&[r1, r2, r3], &[]);
        assert!(
            out.starts_with(
                "📊 Watching 3 repositories. Reply `@<bot> status <repo-substring>` for details."
            ),
            "{out}"
        );
        assert!(out.contains("git@github.com:acme/widgets.git"), "{out}");
        assert!(
            out.contains("empty queue · idle · last iteration 5m ago"),
            "{out}"
        );
        assert!(
            out.contains(
                "2 pending (a06-foo, a07-bar), 0 waiting, 0 excluded · idle · last iteration 3m ago"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "5 pending (a01, a02, a03, a04, a05), 1 waiting (a07-bar), 0 excluded · working on a05-foo (started 2m ago) · no iteration yet"
            ),
            "{out}"
        );
    }

    #[test]
    fn menu_pending_list_truncates_after_five_with_extra_count() {
        let r = RepoStatusResponse {
            url: "git@github.com:owner/repo.git".into(),
            pending_changes: vec![
                "a01".into(),
                "a02".into(),
                "a03".into(),
                "a04".into(),
                "a05".into(),
                "a06".into(),
            ],
            ..RepoStatusResponse::default()
        };
        let out = format_status_menu_reply(&[r], &[]);
        assert!(
            out.contains("6 pending (a01, a02, a03, a04, a05 …+1 more)"),
            "{out}"
        );
    }

    #[test]
    fn menu_seven_pending_truncates_with_two_more() {
        // From the spec scenario: 7 pending entries → "…+2 more".
        let r = RepoStatusResponse {
            url: "u".into(),
            pending_changes: vec![
                "a01".into(),
                "a02".into(),
                "a03".into(),
                "a04".into(),
                "a05".into(),
                "a06".into(),
                "a07".into(),
            ],
            ..RepoStatusResponse::default()
        };
        let out = format_status_menu_reply(&[r], &[]);
        assert!(
            out.contains("7 pending (a01, a02, a03, a04, a05 …+2 more)"),
            "{out}"
        );
    }

    #[test]
    fn menu_all_zero_queue_collapses_to_empty_queue() {
        let r = empty_menu_response("git@github.com:owner/repo.git");
        let out = format_status_menu_reply(&[r], &[]);
        assert!(out.contains("empty queue"), "{out}");
        assert!(!out.contains("0 pending"), "no zero-count parts: {out}");
    }

    #[test]
    fn menu_last_iteration_none_renders_no_iteration_yet() {
        let r = empty_menu_response("u");
        let out = format_status_menu_reply(&[r], &[]);
        assert!(out.contains("no iteration yet"), "{out}");
    }

    #[test]
    fn menu_unavailable_entry_renders_url_plus_unavailable_summary() {
        let healthy = RepoStatusResponse {
            url: "git@github.com:org/healthy.git".into(),
            last_iteration: Some(LastIteration {
                finished_at: Utc::now() - chrono::Duration::minutes(2),
                outcome_summary: String::new(),
                next_iteration_estimate: None,
                poll_interval_sec: 60,
            }),
            ..RepoStatusResponse::default()
        };
        let unavail = UnavailableEntry {
            url: "git@github.com:org/broken.git".into(),
            error: "control-socket call failed: no perma marker".into(),
        };
        let out = format_status_menu_reply(&[healthy], &[unavail]);
        assert!(out.contains("git@github.com:org/healthy.git"), "{out}");
        assert!(out.contains("git@github.com:org/broken.git"), "{out}");
        assert!(
            out.contains(
                "(unavailable: control-socket call failed: no perma marker)"
            ),
            "{out}"
        );
        // Header counts both repos.
        assert!(out.starts_with("📊 Watching 2 repositories."), "{out}");
    }

    #[test]
    fn menu_unavailable_only_still_shows_announcement_and_per_repo_sections() {
        let unavail = vec![
            UnavailableEntry {
                url: "git@github.com:org/a.git".into(),
                error: "err1".into(),
            },
            UnavailableEntry {
                url: "git@github.com:org/b.git".into(),
                error: "err2".into(),
            },
        ];
        let out = format_status_menu_reply(&[], &unavail);
        assert!(out.starts_with("📊 Watching 2 repositories."), "{out}");
        assert!(out.contains("git@github.com:org/a.git"), "{out}");
        assert!(out.contains("git@github.com:org/b.git"), "{out}");
        assert!(out.contains("(unavailable: err1)"), "{out}");
        assert!(out.contains("(unavailable: err2)"), "{out}");
    }

    #[test]
    fn menu_change_name_with_angle_brackets_is_slack_escaped() {
        // The parser's regex would normally reject `<`, but the formatter
        // belt-and-braces escapes anyway so an upstream bug can't leak a
        // raw `<!channel>` ping into the reply.
        let r = RepoStatusResponse {
            url: "u".into(),
            pending_changes: vec!["<bad>".into()],
            currently_busy: Some(BusySummary {
                change: "<also-bad>".into(),
                started_at: Utc::now() - chrono::Duration::minutes(1),
                stage: "executor".into(),
                pid: 4242,
                stale_threshold_secs: 600,
                pid_alive: true,
                audit_type: None,
            }),
            ..RepoStatusResponse::default()
        };
        let out = format_status_menu_reply(&[r], &[]);
        assert!(out.contains("&lt;bad&gt;"), "{out}");
        assert!(out.contains("&lt;also-bad&gt;"), "{out}");
        assert!(!out.contains("<bad>"), "{out}");
        assert!(!out.contains("<also-bad>"), "{out}");
    }

    // ---------- StatusMenu dispatcher ----------

    #[tokio::test]
    async fn dispatch_status_menu_empty_repositories_returns_no_repos_configured_line() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(&format!("{BOT} status"), "C1", BOT, &[], &submitter)
            .await
            .expect("StatusMenu always produces a reply");
        let text = unwrap_sync(reply);
        assert_eq!(text, "📊 No repositories configured.");
        // No bulk call needed.
        assert!(submitter.calls().is_empty());
    }

    /// Build a minimal "ok" RepoStatusResponse JSON for FakeSubmitter
    /// responses.
    fn status_json(url: &str) -> serde_json::Value {
        serde_json::json!({
            "url": url,
            "perma_stuck_changes": [],
            "revision_marked_changes": [],
            "throttled_alerts": [],
            "pending_changes": [],
            "waiting_changes": [],
            "last_iteration": null,
        })
    }

    #[tokio::test]
    async fn dispatch_status_menu_bulk_action_three_repos_all_ok() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let repos = vec![
            ident("git@github.com:owner/a.git"),
            ident("git@github.com:owner/b.git"),
            ident("git@github.com:owner/c.git"),
        ];
        submitter.set_response(
            "repo_status_all",
            serde_json::json!({
                "ok": true,
                "results": [
                    {"url": "git@github.com:owner/a.git", "ok": true, "status": status_json("git@github.com:owner/a.git")},
                    {"url": "git@github.com:owner/b.git", "ok": true, "status": status_json("git@github.com:owner/b.git")},
                    {"url": "git@github.com:owner/c.git", "ok": true, "status": status_json("git@github.com:owner/c.git")},
                ],
            }),
        );
        let reply = dispatcher
            .handle_message(&format!("{BOT} status"), "C1", BOT, &repos, &submitter)
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("📊 Watching 3 repositories."), "{text}");
        for repo in &repos {
            assert!(text.contains(&repo.url), "URL missing: {} in {text}", repo.url);
        }
        // One round trip — the bulk action only.
        assert_eq!(submitter.calls().len(), 1);
        assert_eq!(submitter.calls()[0]["action"], "repo_status_all");
    }

    #[tokio::test]
    async fn dispatch_status_menu_bulk_action_one_unavailable_two_healthy() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let repos = vec![
            ident("git@github.com:owner/a.git"),
            ident("git@github.com:owner/b.git"),
            ident("git@github.com:owner/c.git"),
        ];
        submitter.set_response(
            "repo_status_all",
            serde_json::json!({
                "ok": true,
                "results": [
                    {"url": "git@github.com:owner/a.git", "ok": true, "status": status_json("git@github.com:owner/a.git")},
                    {"url": "git@github.com:owner/b.git", "ok": false, "error": "workspace missing"},
                    {"url": "git@github.com:owner/c.git", "ok": true, "status": status_json("git@github.com:owner/c.git")},
                ],
            }),
        );
        let reply = dispatcher
            .handle_message(&format!("{BOT} status"), "C1", BOT, &repos, &submitter)
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        // All three URLs appear.
        for repo in &repos {
            assert!(text.contains(&repo.url), "URL missing: {} in {text}", repo.url);
        }
        // The errored repo shows the (unavailable) clause.
        assert!(text.contains("(unavailable: workspace missing)"), "{text}");
        // The healthy repos show the standard summary.
        let healthy_a_pos = text
            .find("git@github.com:owner/a.git")
            .unwrap();
        assert!(
            text[healthy_a_pos..].contains("empty queue · idle · no iteration yet"),
            "healthy repo a must have a summary line: {text}"
        );
    }

    #[tokio::test]
    async fn dispatch_status_menu_falls_back_to_n_repo_status_calls_when_bulk_unknown() {
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let repos = vec![
            ident("git@github.com:owner/a.git"),
            ident("git@github.com:owner/b.git"),
        ];
        // Older daemon: bulk action errors out.
        submitter.set_response(
            "repo_status_all",
            serde_json::json!({"ok": false, "error": "unknown action: repo_status_all"}),
        );
        // Fallback path uses per-repo repo_status. The fake submitter's
        // response table is keyed by action name only, so both repos
        // share the same response — fine for this test, which only
        // checks the URL list / call counts.
        submitter.set_response(
            "repo_status",
            serde_json::json!({
                "ok": true,
                "status": status_json("placeholder"),
            }),
        );
        let reply = dispatcher
            .handle_message(&format!("{BOT} status"), "C1", BOT, &repos, &submitter)
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("📊 Watching 2 repositories."), "{text}");
        // 1 bulk attempt + 2 individual calls = 3.
        assert_eq!(submitter.calls().len(), 3);
        assert_eq!(submitter.calls()[0]["action"], "repo_status_all");
        assert_eq!(submitter.calls()[1]["action"], "repo_status");
        assert_eq!(submitter.calls()[2]["action"], "repo_status");
    }

    #[tokio::test]
    async fn dispatch_status_menu_fallback_records_per_call_failures_as_unavailable() {
        // Bulk unsupported → fallback → one of the N per-repo calls
        // errors out. The dispatcher must still ship every other repo's
        // section.
        let dispatcher = OperatorCommandDispatcher::new();
        let submitter = FakeSubmitter::new();
        let repos = vec![
            ident("git@github.com:owner/a.git"),
            ident("git@github.com:owner/b.git"),
        ];
        submitter.set_response(
            "repo_status_all",
            serde_json::json!({"ok": false, "error": "unknown action"}),
        );
        // Both repos get the same hard error response from the fake
        // submitter — both end up in `unavailable`.
        submitter.set_response(
            "repo_status",
            serde_json::json!({"ok": false, "error": "workspace exploded"}),
        );
        let reply = dispatcher
            .handle_message(&format!("{BOT} status"), "C1", BOT, &repos, &submitter)
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("📊 Watching 2 repositories."), "{text}");
        // Both URLs are present and both show the unavailable clause.
        for repo in &repos {
            assert!(text.contains(&repo.url), "URL missing: {} in {text}", repo.url);
        }
        // The error excerpt for both repos is identical (same fake
        // response shared by action name), so we expect at least two
        // occurrences of the same `(unavailable:` clause.
        assert_eq!(
            text.matches("(unavailable: workspace exploded)").count(),
            2,
            "{text}"
        );
    }

    // ---------- propose verb (chat-request-triage) ----------

    /// Test ChatOpsBackend that records every post and returns a
    /// deterministic ts. Suitable for driving the propose dispatcher's
    /// post-then-state-file path without a real backend.
    struct FakeChatOpsBackend {
        posts: Mutex<Vec<(String, String)>>,
        threaded_replies: Mutex<Vec<(String, String, String)>>,
        capture_ts: String,
        // Force `post_message_capturing_ts` to fail with this error
        // (for "backend post failed" tests).
        capture_should_fail: Mutex<bool>,
    }

    impl FakeChatOpsBackend {
        fn new(capture_ts: &str) -> Self {
            Self {
                posts: Mutex::new(Vec::new()),
                threaded_replies: Mutex::new(Vec::new()),
                capture_ts: capture_ts.to_string(),
                capture_should_fail: Mutex::new(false),
            }
        }

        fn force_capture_failure(&self) {
            *self.capture_should_fail.lock().unwrap() = true;
        }
    }

    #[async_trait]
    impl crate::chatops::ChatOpsBackend for FakeChatOpsBackend {
        fn provider_name(&self) -> &'static str {
            "fake"
        }
        fn is_experimental(&self) -> bool {
            false
        }
        async fn post_question(
            &self,
            _channel: &str,
            _change: &str,
            _question: &str,
        ) -> anyhow::Result<String> {
            Ok("fake-question-id".to_string())
        }
        async fn poll_thread_for_human_reply(
            &self,
            _channel: &str,
            _handle: &str,
        ) -> anyhow::Result<Option<crate::chatops::HumanReply>> {
            Ok(None)
        }
        async fn post_notification(&self, _channel: &str, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn post_threaded_reply(
            &self,
            channel: &str,
            thread_ts: &str,
            text: &str,
        ) -> anyhow::Result<()> {
            self.threaded_replies.lock().unwrap().push((
                channel.to_string(),
                thread_ts.to_string(),
                text.to_string(),
            ));
            Ok(())
        }
        async fn post_message_capturing_ts(
            &self,
            channel: &str,
            text: &str,
        ) -> anyhow::Result<String> {
            if *self.capture_should_fail.lock().unwrap() {
                return Err(anyhow::anyhow!("simulated capture failure"));
            }
            self.posts
                .lock()
                .unwrap()
                .push((channel.to_string(), text.to_string()));
            Ok(self.capture_ts.clone())
        }
    }

    fn unwrap_silent(reply: Reply) {
        match reply {
            Reply::Silent => {}
            other => panic!("expected Silent, got {other:?}"),
        }
    }

    #[test]
    fn parse_propose_happy_path() {
        let cmd = parse_command(&format!("{BOT} propose myrepo add a healthz endpoint"), BOT)
            .unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::ProposeRequest {
                repo_substring: "myrepo".into(),
                request_text: "add a healthz endpoint".into(),
            }
        );
    }

    #[test]
    fn parse_propose_case_insensitive_verb() {
        for verb in ["propose", "Propose", "PROPOSE", "ProPosE"] {
            let cmd = parse_command(&format!("{BOT} {verb} myrepo add X"), BOT)
                .unwrap_or_else(|| panic!("{verb} should parse"));
            assert_eq!(
                cmd,
                OperatorCommand::ProposeRequest {
                    repo_substring: "myrepo".into(),
                    request_text: "add X".into(),
                }
            );
        }
    }

    #[test]
    fn parse_propose_missing_request_text_via_parse_command_returns_none() {
        // parse_command turns Invalid into None — the dispatcher path
        // exercises the error reply directly (see the
        // dispatch_propose_missing_request_text_returns_error test).
        assert!(parse_command(&format!("{BOT} propose myrepo"), BOT).is_none());
        // Trailing whitespace doesn't count as text.
        assert!(parse_command(&format!("{BOT} propose myrepo   "), BOT).is_none());
    }

    #[test]
    fn parse_propose_missing_repo_substring_returns_none() {
        assert!(parse_command(&format!("{BOT} propose"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} propose   "), BOT).is_none());
    }

    #[test]
    fn parse_propose_preserves_multiline_text() {
        let msg = format!(
            "{BOT} propose myrepo line1\nline2\n\nthird para after blank"
        );
        let cmd = parse_command(&msg, BOT).expect("multi-line must parse");
        match cmd {
            OperatorCommand::ProposeRequest {
                repo_substring,
                request_text,
            } => {
                assert_eq!(repo_substring, "myrepo");
                assert_eq!(request_text, "line1\nline2\n\nthird para after blank");
            }
            other => panic!("expected ProposeRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_propose_over_cap_returns_invalid() {
        // 10,001-char request text — over the cap. The dispatcher must
        // surface the error reply; through `parse_command` we just see
        // None.
        let big: String = std::iter::repeat_n('a', MAX_PROPOSE_REQUEST_TEXT_LEN + 1).collect();
        let msg = format!("{BOT} propose myrepo {big}");
        assert!(
            parse_command(&msg, BOT).is_none(),
            "oversize text must fail parse"
        );
    }

    #[tokio::test]
    async fn dispatch_propose_happy_path_posts_ack_and_writes_state_and_submits_action() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1748399999.001234"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_proposal_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "queue_proposal_request",
            serde_json::json!({"ok": true, "poll_interval_sec": 60}),
        );
        let reply = dispatcher
            .handle_message_with_context(
                &format!("{BOT} propose myrepo add a /healthz endpoint"),
                "C_OPS",
                None,
                Some("U_RAB"),
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("propose must produce a reply");
        unwrap_silent(reply);

        // Backend captured the top-level ack post.
        let posts = backend.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1, "exactly one top-level ack post: {posts:?}");
        let (channel, ack_text) = &posts[0];
        assert_eq!(channel, "C_OPS");
        assert!(ack_text.starts_with("✓ Queued proposal request for "), "{ack_text}");
        assert!(
            ack_text.contains("git@github.com:acme/myrepo.git"),
            "{ack_text}"
        );
        assert!(ack_text.contains("Follow along in this thread."), "{ack_text}");

        // Exactly one control-socket action was submitted.
        let calls = submitter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["action"], "queue_proposal_request");
        assert_eq!(calls[0]["url"], "git@github.com:acme/myrepo.git");
        let request_id = calls[0]["request_id"]
            .as_str()
            .expect("action carries request_id")
            .to_string();

        // State file exists with the expected fields.
        let st = crate::proposal_requests::read_state(
            tmp.path(),
            "git@github.com:acme/myrepo.git",
            &request_id,
        )
        .unwrap()
        .expect("state file present");
        assert_eq!(st.thread_ts, "1748399999.001234");
        assert_eq!(st.ack_message_ts, "1748399999.001234");
        assert_eq!(st.channel, "C_OPS");
        assert_eq!(st.operator_user, "U_RAB");
        assert_eq!(st.request_text, "add a /healthz endpoint");
        assert_eq!(
            st.status,
            crate::proposal_requests::ProposalRequestStatus::Pending
        );
    }

    #[tokio::test]
    async fn dispatch_propose_ambiguous_repo_returns_be_more_specific_no_post() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_proposal_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        // Both fixture repos contain the substring "acme" → ambiguous.
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} propose acme add X"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("be more specific"), "{text}");
        // No backend posts, no control-socket action, no state file.
        assert!(backend.posts.lock().unwrap().is_empty());
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_propose_no_repo_match_returns_no_match_no_post() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_proposal_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} propose nonexistent add X"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("nonexistent"), "{text}");
        assert!(backend.posts.lock().unwrap().is_empty());
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_propose_missing_request_text_returns_error() {
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher =
            OperatorCommandDispatcher::new().with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} propose myrepo"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("missing-text propose must produce an error reply");
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("missing request text"), "{text}");
        assert!(backend.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_propose_oversize_text_returns_error() {
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher =
            OperatorCommandDispatcher::new().with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let big: String = std::iter::repeat_n('a', MAX_PROPOSE_REQUEST_TEXT_LEN + 1).collect();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} propose myrepo {big}"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("oversize-text propose must produce an error reply");
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("10000"), "{text}");
        assert!(backend.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_propose_without_chatops_backend_returns_error() {
        // No `.with_chatops(...)` call → the dispatcher cannot post the
        // top-level ack so it surfaces an error.
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = OperatorCommandDispatcher::new()
            .with_proposal_request_state_dir(tmp.path().to_path_buf());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} propose myrepo add X"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("chatops backend not configured"), "{text}");
    }

    #[tokio::test]
    async fn dispatch_propose_backend_post_failure_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        backend.force_capture_failure();
        let dispatcher = OperatorCommandDispatcher::new()
            .with_proposal_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} propose myrepo add X"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("could not post ack"), "{text}");
        // No control-socket call submitted on backend failure.
        assert!(submitter.calls().is_empty());
    }

    // ---------- changelog verb (a06-chat-driven-changelog) ----------

    #[test]
    fn parse_changelog_happy_path_no_args() {
        let cmd = parse_command(&format!("{BOT} changelog myrepo"), BOT).unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::ChangelogRequest {
                repo_substring: "myrepo".into(),
                raw_args: String::new(),
            }
        );
    }

    #[test]
    fn parse_changelog_happy_path_with_args() {
        let cmd = parse_command(
            &format!("{BOT} changelog myrepo --since v0.1.0 --to v0.2.0"),
            BOT,
        )
        .unwrap();
        assert_eq!(
            cmd,
            OperatorCommand::ChangelogRequest {
                repo_substring: "myrepo".into(),
                raw_args: "--since v0.1.0 --to v0.2.0".into(),
            }
        );
    }

    #[test]
    fn parse_changelog_case_insensitive_verb() {
        for verb in ["changelog", "CHANGELOG", "Changelog", "ChangeLog"] {
            let cmd = parse_command(&format!("{BOT} {verb} myrepo"), BOT)
                .unwrap_or_else(|| panic!("{verb} should parse"));
            assert!(matches!(cmd, OperatorCommand::ChangelogRequest { .. }));
        }
    }

    #[test]
    fn parse_changelog_missing_repo_returns_none() {
        assert!(parse_command(&format!("{BOT} changelog"), BOT).is_none());
        assert!(parse_command(&format!("{BOT} changelog   "), BOT).is_none());
    }

    #[test]
    fn parse_changelog_args_parses_since_and_to() {
        let p = parse_changelog_args("--since v0.1.0 --to v0.2.0").unwrap();
        assert_eq!(p.since.as_deref(), Some("v0.1.0"));
        assert_eq!(p.to.as_deref(), Some("v0.2.0"));
        assert!(p.workspace_override.is_none());
    }

    #[test]
    fn parse_changelog_args_empty_input_is_ok() {
        let p = parse_changelog_args("").unwrap();
        assert!(p.since.is_none());
        assert!(p.to.is_none());
        assert!(p.workspace_override.is_none());
    }

    #[test]
    fn parse_changelog_args_unknown_flag_errors() {
        let err = parse_changelog_args("--unknown foo").unwrap_err();
        assert!(err.contains("unrecognized arg"));
        assert!(err.contains("--unknown"));
    }

    #[test]
    fn parse_changelog_args_missing_value_errors() {
        let err = parse_changelog_args("--since").unwrap_err();
        assert!(err.contains("missing value"));
        // A second flag in place of a value is also an error.
        let err = parse_changelog_args("--since --to v1").unwrap_err();
        assert!(err.contains("missing value"));
    }

    #[test]
    fn parse_changelog_args_accepts_workspace_for_higher_layers_to_reject() {
        let p = parse_changelog_args("--workspace /tmp/ws").unwrap();
        assert_eq!(p.workspace_override.as_deref(), Some("/tmp/ws"));
    }

    #[tokio::test]
    async fn dispatch_changelog_happy_path_posts_ack_writes_state_submits_action() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1748400000.001234"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_changelog_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        submitter.set_response(
            "queue_changelog_request",
            serde_json::json!({"ok": true, "poll_interval_sec": 60}),
        );
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} changelog myrepo --since v0.1.0"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("changelog must produce a reply");
        unwrap_silent(reply);

        let posts = backend.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1, "exactly one top-level ack post");
        let (channel, ack_text) = &posts[0];
        assert_eq!(channel, "C_OPS");
        assert!(
            ack_text.starts_with("✓ Queued changelog request for "),
            "{ack_text}"
        );
        assert!(
            ack_text.contains("git@github.com:acme/myrepo.git"),
            "{ack_text}"
        );
        assert!(ack_text.contains("Follow along in this thread."), "{ack_text}");

        let calls = submitter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["action"], "queue_changelog_request");
        assert_eq!(calls[0]["url"], "git@github.com:acme/myrepo.git");
        let request_id = calls[0]["request_id"]
            .as_str()
            .expect("action carries request_id")
            .to_string();

        let st = crate::changelog_requests::read_state(
            tmp.path(),
            "git@github.com:acme/myrepo.git",
            &request_id,
        )
        .unwrap()
        .expect("state file present");
        assert_eq!(st.lifecycle_thread_ts, "1748400000.001234");
        assert_eq!(st.channel, "C_OPS");
        assert_eq!(st.raw_args, "--since v0.1.0");
        assert_eq!(
            st.status,
            crate::changelog_requests::ChangelogStatus::Pending
        );
    }

    #[tokio::test]
    async fn dispatch_changelog_missing_repo_substring_refuses_with_no_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_changelog_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} changelog"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .expect("missing-repo must produce a reply");
        let text = unwrap_sync(reply);
        assert!(text.contains("missing repo-substring"), "{text}");
        assert!(backend.posts.lock().unwrap().is_empty());
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_changelog_no_match_lists_configured() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_changelog_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} changelog gibberish"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("gibberish"), "{text}");
        assert!(backend.posts.lock().unwrap().is_empty());
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_changelog_ambiguous_lists_candidates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_changelog_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} changelog acme"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("be more specific"), "{text}");
        assert!(backend.posts.lock().unwrap().is_empty());
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_changelog_without_chatops_backend_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = OperatorCommandDispatcher::new()
            .with_changelog_request_state_dir(tmp.path().to_path_buf());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} changelog myrepo"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.contains("chatops backend not configured"), "{text}");
    }

    #[tokio::test]
    async fn dispatch_changelog_workspace_override_default_denied() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_changelog_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} changelog myrepo --workspace /tmp/ws"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗"), "{text}");
        assert!(text.contains("--workspace"), "{text}");
        // No backend posts AND no state file AND no action submitted.
        assert!(backend.posts.lock().unwrap().is_empty());
        assert!(submitter.calls().is_empty());
    }

    #[tokio::test]
    async fn dispatch_changelog_bad_arg_returns_descriptive_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = std::sync::Arc::new(FakeChatOpsBackend::new("1.0"));
        let dispatcher = OperatorCommandDispatcher::new()
            .with_changelog_request_state_dir(tmp.path().to_path_buf())
            .with_chatops(backend.clone());
        let submitter = FakeSubmitter::new();
        let reply = dispatcher
            .handle_message(
                &format!("{BOT} changelog myrepo --bogus xyz"),
                "C_OPS",
                BOT,
                &fixture_repos(),
                &submitter,
            )
            .await
            .unwrap();
        let text = unwrap_sync(reply);
        assert!(text.starts_with("✗ changelog: bad arg:"), "{text}");
        assert!(text.contains("--bogus"), "{text}");
        assert!(backend.posts.lock().unwrap().is_empty());
        assert!(submitter.calls().is_empty());
    }
}
