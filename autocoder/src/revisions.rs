//! PR-comment revision loop.
//!
//! Polls open PRs whose head branch matches `repositories[].agent_branch`,
//! parses operator comments for the `@<bot> revise <text>` trigger pattern,
//! and dispatches in-place revisions through the executor.
//!
//! State per PR lives at
//! `<state_dir>/revisions/<repo-sanitized>/<pr-number>.json`, where
//! `<repo-sanitized>` is the workspace's basename (already
//! URL-sanitized per `workspace::derive_path`'s rules). Daemon-global
//! accounting that survives reboot but does NOT live inside the git
//! working tree.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::chatops::ChatOpsBackend;
use crate::config::{GithubConfig, RepositoryConfig};
use crate::executor::{Executor, ExecutorOutcome};
use crate::github;

/// HTML-comment marker the reviewer-initiated revision flow writes at the
/// top of every PR comment it posts. The dispatcher's self-author filter
/// (which normally drops bot-authored comments to avoid recursion on its
/// own replies) checks this marker as an explicit bypass — comments the
/// reviewer pipeline posts on the bot's behalf are parsed normally even
/// though their author is `bot_username`.
pub const REVIEWER_REVISION_MARKER: &str = "<!-- reviewer-revision -->";

/// Per-PR state file persisted under
/// `<state_dir>/revisions/<repo-sanitized>/<pr_number>.json`. The
/// dispatcher reads it on each iteration to know which comments are
/// already processed and how many revisions have been applied so far.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionState {
    pub pr_number: u64,
    pub agent_branch: String,
    pub last_seen_comment_at: DateTime<Utc>,
    pub revisions_applied: u32,
    pub revision_cap: u32,
    #[serde(default)]
    pub cap_decline_posted: bool,
}

/// Bundle of context passed to the executor when it runs in revision mode.
/// Lives in this module (rather than `executor::mod`) because only the
/// revision dispatcher constructs it; the executor consumes it through a
/// thin trait method that takes the bundle as a parameter.
#[derive(Debug, Clone)]
pub struct RevisionContext {
    /// Name of the archived change being revised — used to locate the
    /// original change material via the same archive-lookup helper the
    /// reviewer uses (`locate_archive_dir`). Carried in the context so
    /// callers that consume the bundle have it without re-deriving from
    /// the surrounding executor parameters.
    #[allow(dead_code)]
    pub change_name: String,
    /// `git diff <base>..<agent>` against the workspace state at the time
    /// the revision was dispatched.
    pub pr_diff: String,
    /// The operator's revision text verbatim (after `parse_revision_trigger`
    /// strips the mention and the verb).
    pub revision_text: String,
    /// PR body verbatim, per a20a5. Contains the `## Code Review`
    /// section (when the reviewer is enabled) AND any other PR-rendered
    /// context the human reviewer saw.
    pub pr_body: String,
    /// Newline-separated change slugs extracted from the PR body via
    /// `extract_change_list_from_pr_body`. Lets the LLM resolve which
    /// change(s) the operator's revision targets (per a20a5's
    /// multi-change resolution rule).
    pub pr_change_list: String,
    /// Concatenated `## Agent implementation notes` issue-comment bodies
    /// from the PR, in posted order, separated by `\n\n---\n\n` between
    /// entries. One section per change in multi-change passes. Per
    /// a20a5: the original implementer's narrative — what the agent
    /// claimed to do, which is the gap the operator's revision closes.
    pub agent_implementation_notes: String,
}

/// Legacy per-workspace directory used as a fallback when the
/// daemon-paths global is not installed (i.e. tests that build their
/// workspace without going through `cli::run`). Preserves
/// pre-`DaemonPaths` test-fixture expectations.
const LEGACY_REVISIONS_DIR: &str = ".autocoder/revisions";

/// Return the path to a PR's state file for `workspace`. In
/// production, lives at
/// `<state_dir>/revisions/<repo-sanitized>/<pr_number>.json`. In tests
/// where the daemon-paths global has not been installed, falls back to
/// `<workspace>/.autocoder/revisions/<pr_number>.json`.
pub fn state_path(workspace: &Path, pr_number: u64) -> PathBuf {
    revisions_dir(workspace).join(format!("{pr_number}.json"))
}

/// Return the directory under which all per-PR state files for one
/// repo live. Resolved to `<state_dir>/revisions/<repo-sanitized>/` in
/// production, or `<workspace>/.autocoder/revisions/` in the
/// global-paths-not-installed fallback.
fn revisions_dir(workspace: &Path) -> PathBuf {
    if crate::paths::get_global().is_some() {
        let basename = workspace
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        crate::paths::current()
            .state
            .join("revisions")
            .join(basename)
    } else {
        workspace.join(LEGACY_REVISIONS_DIR)
    }
}

/// Read the state file for `pr_number`. A missing file returns
/// `Ok(None)`; a corrupt file returns `Err`.
pub fn read_state(workspace: &Path, pr_number: u64) -> Result<Option<RevisionState>> {
    let path = state_path(workspace, pr_number);
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let parsed: RevisionState = serde_json::from_str(&raw).with_context(|| {
                format!("parsing revision-state file {}", path.display())
            })?;
            Ok(Some(parsed))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading revision-state file {}", path.display())),
    }
}

/// Atomically write `state` to its per-PR file via temp-file-then-rename
/// in the same directory. Matches the daemon's other state-file writes.
pub fn write_state(workspace: &Path, state: &RevisionState) -> Result<()> {
    let path = state_path(workspace, state.pr_number);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("revision-state path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating revisions dir {}", parent.display()))?;
    let tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    serde_json::to_writer_pretty(&tmp, state)
        .with_context(|| format!("serializing revision state for {}", path.display()))?;
    tmp.persist(&path)
        .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
    Ok(())
}

/// Idempotently remove the state file for `pr_number`. A missing file is
/// a success, not an error. Exposed for callers (and tests) that need to
/// drop state explicitly outside the prune-on-close path.
#[allow(dead_code)]
pub fn remove_state(workspace: &Path, pr_number: u64) -> Result<()> {
    let path = state_path(workspace, pr_number);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Remove every state file whose PR number is not in `open_pr_numbers`.
/// Returns the number of files removed. A missing revisions directory is
/// not an error — it returns `0`.
pub fn prune_closed_prs(workspace: &Path, open_pr_numbers: &HashSet<u64>) -> Result<usize> {
    let dir = revisions_dir(workspace);
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("reading revisions dir {}", dir.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().into_owned();
        let stem = match name.strip_suffix(".json") {
            Some(s) => s,
            None => continue,
        };
        let pr_number: u64 = match stem.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !open_pr_numbers.contains(&pr_number) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                tracing::warn!(
                    path = %entry.path().display(),
                    "failed to prune closed-PR revision state: {e}"
                );
            } else {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Parse a PR comment body for the revision trigger pattern. Returns
/// `Some(revision_text)` when the body's first non-whitespace,
/// non-HTML-comment line begins with `@<bot_username>` (case-insensitive
/// on the mention) followed by `revise` (case-insensitive) and at least
/// one non-whitespace character of revision text. Otherwise `None`. The
/// returned text has leading/trailing whitespace trimmed but preserves
/// any internal newlines.
///
/// Leading lines that are entirely an HTML comment (`<!-- ... -->`) are
/// skipped before the mention search. This lets the reviewer-initiated
/// revision pipeline prefix its comments with `<!-- reviewer-revision -->`
/// (the dispatcher's self-author-filter bypass marker) without the parser
/// needing to understand the marker's semantics — it just tolerates the
/// leading metadata line.
pub fn parse_revision_trigger(comment_body: &str, bot_username: &str) -> Option<String> {
    let body = strip_leading_html_comment_lines(comment_body);
    let trimmed = body.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let expected_mention = format!("@{bot_username}");
    // First non-whitespace token = up to next whitespace
    let mention_end = trimmed
        .find(char::is_whitespace)
        .unwrap_or(trimmed.len());
    let mention = &trimmed[..mention_end];
    if !mention.eq_ignore_ascii_case(&expected_mention) {
        return None;
    }
    let rest = trimmed[mention_end..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let verb_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let verb = &rest[..verb_end];
    if !verb.eq_ignore_ascii_case("revise") {
        return None;
    }
    let after_verb = &rest[verb_end..];
    let revision_text = after_verb.trim();
    if revision_text.is_empty() {
        return None;
    }
    Some(revision_text.to_string())
}

/// Strip leading whole-line HTML comments (e.g. `<!-- reviewer-revision -->`)
/// from `body`, returning the remainder. A line counts as "HTML comment
/// only" when its trimmed contents start with `<!--` and end with `-->`.
/// Non-comment lines (including blank lines that follow the comment) end
/// the strip and are returned verbatim. Used by `parse_revision_trigger`
/// to make the reviewer-revision marker invisible to the mention search.
fn strip_leading_html_comment_lines(body: &str) -> &str {
    let mut cursor = 0usize;
    while cursor < body.len() {
        // Skip leading whitespace within the current line slice.
        let rest = &body[cursor..];
        let line_end = rest.find('\n').map(|n| cursor + n + 1).unwrap_or(body.len());
        let line = &body[cursor..line_end];
        let line_trim = line.trim();
        if line_trim.is_empty() {
            // Blank line — stop stripping; let downstream `trim_start`
            // handle it.
            break;
        }
        if line_trim.starts_with("<!--") && line_trim.ends_with("-->") {
            cursor = line_end;
            continue;
        }
        break;
    }
    &body[cursor..]
}

/// Best-effort: extract the list of change names from the PR body's
/// "Changes implemented in this pass:" section (the format produced by
/// `polling_loop::build_pr_body`). Returns an empty vec if the section is
/// absent or `body` is `None`.
pub fn extract_change_list_from_pr_body(body: Option<&str>) -> Vec<String> {
    let body = match body {
        Some(b) => b,
        None => return Vec::new(),
    };
    let marker = "Changes implemented in this pass:";
    let idx = match body.find(marker) {
        Some(i) => i + marker.len(),
        None => return Vec::new(),
    };
    let mut changes = Vec::new();
    for line in body[idx..].lines() {
        let trimmed = line.trim();
        if let Some(stripped) = trimmed.strip_prefix("- ") {
            let name = stripped.trim();
            if !name.is_empty() {
                changes.push(name.to_string());
            }
        } else if !trimmed.is_empty() && !changes.is_empty() {
            // Stop at the first non-bullet, non-blank line after we've
            // started collecting (preserves the "first list only" intent).
            break;
        }
    }
    changes
}

/// Per-pass ChatOps context borrowed from the polling loop. The
/// dispatcher uses it to post cap-decline + AskUser notifications.
pub struct ChatOpsCtx<'a> {
    pub chatops: &'a dyn ChatOpsBackend,
    pub channel: &'a str,
}

/// Walk the set of open PRs on `repo.agent_branch`, prune closed-PR state
/// files, and process any revision-trigger comments. Returns Ok on
/// completion (per-PR errors are logged at WARN and do not abort the
/// walk).
///
/// `revision_cap` is the resolved `executor.max_revisions_per_pr` (already
/// clamped at config load); it is stamped into freshly-initialized
/// per-PR state files. PRs whose state file pre-dates a config change
/// continue to use the cap stored in their state file.
pub async fn process_revision_requests(
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<ChatOpsCtx<'_>>,
    revision_cap: u32,
    cancel: CancellationToken,
) -> Result<()> {
    process_revision_requests_at(
        workspace,
        repo,
        github_cfg,
        executor,
        chatops_ctx,
        revision_cap,
        cancel,
        github::DEFAULT_API_BASE,
    )
    .await
}

/// Test-injectable form of `process_revision_requests` that accepts an
/// explicit GitHub API base URL (so mockito-driven tests can intercept
/// HTTP calls).
#[allow(clippy::too_many_arguments)]
pub async fn process_revision_requests_at(
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<ChatOpsCtx<'_>>,
    revision_cap: u32,
    cancel: CancellationToken,
    api_base: &str,
) -> Result<()> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let (owner, repo_name) = github::parse_repo_url(&repo.url)?;
    let token = crate::github_credentials::resolve_token(github_cfg, &owner)?;
    if cancel.is_cancelled() {
        return Ok(());
    }
    let bot_username = github::self_bot_username(api_base, &token)
        .await
        .with_context(|| "resolving bot username via GET /user")?;
    if cancel.is_cancelled() {
        return Ok(());
    }
    // Per a20a4: the head qualifier owner is the fork owner in fork-PR
    // mode, the upstream owner in direct-push mode. Pre-fix code used
    // the upstream owner unconditionally — which never matched a
    // fork-headed PR, leaving fork-PR-mode operators without working
    // `@<bot> revise <text>` since fork-PR mode shipped.
    let head_owner = github_cfg.fork_owner.as_deref().unwrap_or(&owner);
    let open_prs = github::list_open_prs_for_head(
        api_base,
        &token,
        &owner,
        &repo_name,
        head_owner,
        &repo.agent_branch,
    )
    .await
    .with_context(|| {
        format!(
            "listing open PRs for {owner}/{repo_name} head {head_owner}:{}",
            repo.agent_branch
        )
    })?;
    let open_numbers: HashSet<u64> = open_prs.iter().map(|p| p.number).collect();
    let _pruned = prune_closed_prs(workspace, &open_numbers)?;
    let push_remote = if github_cfg.fork_owner.is_some() {
        "fork"
    } else {
        "origin"
    };
    for pr in &open_prs {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let pr_result = process_one_pr(
            workspace,
            repo,
            pr,
            &owner,
            &repo_name,
            &token,
            &bot_username,
            executor,
            chatops_ctx.as_ref(),
            revision_cap,
            push_remote,
            api_base,
            cancel.clone(),
        )
        .await;
        if let Err(e) = pr_result {
            tracing::warn!(
                url = %repo.url,
                pr_number = pr.number,
                "revision processing for PR failed (iteration continues): {e:#}"
            );
        }
    }
    Ok(())
}

/// Process all new comments on a single PR. Returns Ok on success;
/// errors propagate (the caller logs at WARN and proceeds to the next PR).
#[allow(clippy::too_many_arguments)]
async fn process_one_pr(
    workspace: &Path,
    repo: &RepositoryConfig,
    pr: &github::PrSummary,
    owner: &str,
    repo_name: &str,
    token: &str,
    bot_username: &str,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsCtx<'_>>,
    revision_cap: u32,
    push_remote: &str,
    api_base: &str,
    cancel: CancellationToken,
) -> Result<()> {
    // Load or initialize per-PR state. The revision_cap stored in state
    // reflects the cap in effect when the PR was first observed; callers
    // that change `executor.max_revisions_per_pr` mid-PR live with the
    // older cap until the PR closes (matches the chatops-channel
    // hot-reload contract: changes apply to new work, not in-flight).
    let mut state = match read_state(workspace, pr.number)? {
        Some(s) => s,
        None => RevisionState {
            pr_number: pr.number,
            agent_branch: repo.agent_branch.clone(),
            last_seen_comment_at: pr.created_at,
            revisions_applied: 0,
            revision_cap,
            cap_decline_posted: false,
        },
    };

    // If we've already posted the decline AND we're still at-or-above the
    // cap, fast-skip the PR entirely so subsequent triggering comments are
    // silently ignored. Still advance the timestamp so they're not
    // re-processed if state is rewritten by other means.
    if state.cap_decline_posted && state.revisions_applied >= state.revision_cap {
        // Fetch comments only to advance the timestamp, so we don't keep
        // re-reading the same set forever.
        let comments = github::list_issue_comments_since(
            api_base,
            token,
            owner,
            repo_name,
            pr.number,
            state.last_seen_comment_at,
        )
        .await?;
        if let Some(latest) = comments.iter().map(|c| c.created_at).max() {
            state.last_seen_comment_at = latest;
            write_state(workspace, &state)?;
        }
        return Ok(());
    }

    let comments = github::list_issue_comments_since(
        api_base,
        token,
        owner,
        repo_name,
        pr.number,
        state.last_seen_comment_at,
    )
    .await?;
    if comments.is_empty() {
        return Ok(());
    }
    let mut latest_seen: Option<DateTime<Utc>> = None;
    for comment in comments {
        if cancel.is_cancelled() {
            // Persist whatever progress we made and return.
            if let Some(t) = latest_seen {
                state.last_seen_comment_at = t;
                write_state(workspace, &state)?;
            }
            return Ok(());
        }
        // Strict-since client-side guard. GitHub's `since` filter on
        // `/issues/<num>/comments` is inclusive at the sub-second boundary
        // (it compares against the comment's full-precision `updated_at`),
        // so a marker truncated to seconds — OR a marker advanced exactly
        // to a comment's creation time — can produce a re-fetch of an
        // already-processed comment. Skip any comment at OR before the
        // marker; the corresponding `advance_seen` is a no-op (the local
        // `latest_seen` is only used to advance the persisted marker, and
        // the value is already at or behind it).
        if comment.created_at <= state.last_seen_comment_at {
            advance_seen(&mut latest_seen, comment.created_at);
            continue;
        }
        // Bot-authored comments are filtered out before parsing — UNLESS
        // the body starts with the reviewer-revision HTML-comment marker,
        // which is the one sanctioned bypass. The reviewer pipeline posts
        // comments on the bot's behalf; without the bypass the dispatcher
        // would (correctly) treat them as the bot's own replies and drop
        // them. All other bot-authored comments (the dispatcher's own
        // success/failure/cap-decline replies, any future bot content)
        // continue to be filtered.
        if comment.user_login().eq_ignore_ascii_case(bot_username)
            && !comment
                .body
                .trim_start()
                .starts_with(REVIEWER_REVISION_MARKER)
        {
            advance_seen(&mut latest_seen, comment.created_at);
            continue;
        }
        let revision_text = match parse_revision_trigger(&comment.body, bot_username) {
            Some(t) => t,
            None => {
                advance_seen(&mut latest_seen, comment.created_at);
                continue;
            }
        };
        if state.revisions_applied >= state.revision_cap {
            // Cap hit. Post the one-time decline (if not posted) and
            // break — subsequent triggering comments on this PR are
            // silently ignored. We still advance `latest_seen` to the
            // decline-triggering comment so re-running the iteration
            // doesn't loop on the same comment.
            advance_seen(&mut latest_seen, comment.created_at);
            if !state.cap_decline_posted {
                let pr_text = format!(
                    "🛑 Revision cap reached ({} revisions). Further `@{} revise` requests on this PR will be ignored. Close + re-open or merge as-is.",
                    state.revision_cap, bot_username,
                );
                if let Err(e) = github::post_issue_comment(
                    api_base,
                    token,
                    owner,
                    repo_name,
                    pr.number,
                    &pr_text,
                )
                .await
                {
                    tracing::warn!(
                        url = %repo.url,
                        pr_number = pr.number,
                        "failed to post cap-decline PR comment: {e:#}"
                    );
                }
                if let Some(ctx) = chatops_ctx {
                    let chat_text = format!(
                        "🛑 {}: PR #{} hit the revision cap of {}. Further revision requests ignored.",
                        repo.url, pr.number, state.revision_cap,
                    );
                    if let Err(e) = ctx.chatops.post_notification(ctx.channel, &chat_text).await {
                        tracing::warn!(
                            url = %repo.url,
                            pr_number = pr.number,
                            "failed to post cap-decline chatops notification: {e:#}"
                        );
                    }
                }
                state.cap_decline_posted = true;
                write_state(workspace, &state)?;
            }
            break;
        }
        // Apply the revision. The change name is derived from the PR's
        // body (the first change listed); v1 supports a single revision
        // target per PR. (Multi-change resolution is delegated to the
        // LLM via a20a5's pr_change_list field — the dispatcher still
        // uses the first slug for state-file naming + log routing.)
        let change_list = extract_change_list_from_pr_body(pr.body.as_deref());
        let change_name = change_list
            .first()
            .cloned()
            .unwrap_or_else(|| repo.agent_branch.clone());

        // Per a20a5: assemble the executor's revision context from
        // PR-sourced material. Fetch all-time PR comments to extract
        // the original implementer's `## Agent implementation notes`.
        // If the fetch fails, post a clear failure comment AND DO NOT
        // advance the comment-seen marker — the next iteration's
        // dispatcher pass re-attempts the assembly so transient API
        // errors don't lose the operator's revise comment.
        let all_comments = match github::list_issue_comments_since(
            api_base,
            token,
            owner,
            repo_name,
            pr.number,
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        )
        .await
        {
            Ok(cs) => cs,
            Err(e) => {
                tracing::warn!(
                    url = %repo.url,
                    pr_number = pr.number,
                    "revise: PR-context assembly failed (comments fetch): {e:#}; refusing without advancing the seen-marker"
                );
                let truncated_err: String =
                    format!("{e:#}").chars().take(200).collect();
                let body = format!(
                    "✗ Cannot revise: failed to fetch PR context: {truncated_err}. The daemon will retry on the next polling iteration. If this persists, check journalctl for the daemon's GitHub API errors AND verify the bot's token has Read access on this repo."
                );
                let _ = github::post_issue_comment(
                    api_base, token, owner, repo_name, pr.number, &body,
                )
                .await;
                // CRITICAL: do NOT advance latest_seen — re-attempt
                // on the next iteration. Break out of the trigger
                // loop; subsequent triggers (if any) also get
                // re-fetched on the next iteration.
                break;
            }
        };
        let agent_implementation_notes =
            extract_agent_implementation_notes(&all_comments);
        let pr_body = pr.body.clone().unwrap_or_default();
        let pr_change_list_str = change_list.join("\n");

        let outcome = execute_revision(
            workspace,
            repo,
            executor,
            &change_name,
            &revision_text,
            pr_body,
            pr_change_list_str,
            agent_implementation_notes,
        )
        .await;
        match outcome {
            Ok(ExecutorOutcome::Completed { .. }) => {
                let commit_subject = build_commit_subject(&change_name, &revision_text);
                if let Err(e) = apply_revision_commit(workspace, repo, push_remote, &commit_subject)
                {
                    tracing::warn!(
                        url = %repo.url,
                        pr_number = pr.number,
                        "revision commit/push failed; reporting as failed: {e:#}"
                    );
                    let body = format!(
                        "✗ Revision attempt failed: commit/push failed: {e}. The PR is unchanged. Reply with another `@{} revise ...` to retry, or close the PR if the request cannot be satisfied.",
                        bot_username
                    );
                    let _ = github::post_issue_comment(
                        api_base, token, owner, repo_name, pr.number, &body,
                    )
                    .await;
                    state.revisions_applied = state.revisions_applied.saturating_add(1);
                    advance_seen(&mut latest_seen, comment.created_at);
                    write_state(workspace, &state)?;
                    continue;
                }
                state.revisions_applied = state.revisions_applied.saturating_add(1);
                let reply = format!(
                    "✅ Revision applied: {}. Revision count: {} of {}.",
                    commit_subject, state.revisions_applied, state.revision_cap,
                );
                if let Err(e) = github::post_issue_comment(
                    api_base, token, owner, repo_name, pr.number, &reply,
                )
                .await
                {
                    tracing::warn!(
                        url = %repo.url,
                        pr_number = pr.number,
                        "failed to post success PR comment: {e:#}"
                    );
                }
                advance_seen(&mut latest_seen, comment.created_at);
                write_state(workspace, &state)?;
            }
            Ok(ExecutorOutcome::AskUser { question, resume_handle }) => {
                // AskUser → existing chatops escalation. No commit, no
                // count increment, no PR reply. `last_seen_comment_at`
                // is NOT advanced past this comment so the next iteration
                // can resume against it.
                let _handle = resume_handle;
                if let Some(ctx) = chatops_ctx {
                    let chat_text = format!(
                        "❓ Revision on {} PR #{} needs clarification: {}",
                        repo.url, pr.number, question,
                    );
                    if let Err(e) = ctx.chatops.post_notification(ctx.channel, &chat_text).await {
                        tracing::warn!(
                            url = %repo.url,
                            pr_number = pr.number,
                            "failed to post AskUser chatops notification: {e:#}"
                        );
                    }
                }
                // Persist progress on prior comments only — do NOT advance
                // past the current (unresolved) comment.
                if let Some(t) = latest_seen {
                    state.last_seen_comment_at = t;
                    write_state(workspace, &state)?;
                }
                return Ok(());
            }
            Ok(ExecutorOutcome::Failed { reason }) => {
                let body = format!(
                    "✗ Revision attempt failed: {}. The PR is unchanged. Reply with another `@{} revise ...` to retry, or close the PR if the request cannot be satisfied.",
                    reason, bot_username
                );
                if let Err(e) = github::post_issue_comment(
                    api_base, token, owner, repo_name, pr.number, &body,
                )
                .await
                {
                    tracing::warn!(
                        url = %repo.url,
                        pr_number = pr.number,
                        "failed to post failure PR comment: {e:#}"
                    );
                }
                state.revisions_applied = state.revisions_applied.saturating_add(1);
                advance_seen(&mut latest_seen, comment.created_at);
                write_state(workspace, &state)?;
            }
            Ok(ExecutorOutcome::SpecNeedsRevision { .. }) => {
                let body = "✗ Revision attempt failed: executor reported the original change spec is unimplementable. The PR is unchanged."
                    .to_string();
                let _ = github::post_issue_comment(
                    api_base, token, owner, repo_name, pr.number, &body,
                )
                .await;
                state.revisions_applied = state.revisions_applied.saturating_add(1);
                advance_seen(&mut latest_seen, comment.created_at);
                write_state(workspace, &state)?;
            }
            Ok(ExecutorOutcome::IterationRequested { .. }) => {
                // Revisions are single-shot bug fixes against a merged PR;
                // they don't have the iteration-pending state machine that
                // pending changes do. Treat IterationRequested as a Failed-
                // equivalent so the PR comment surfaces the unhandled case.
                let body = format!(
                    "✗ Revision attempt failed: executor returned IterationRequested (iteration sequences are not supported on the revise path). The PR is unchanged. Reply with another `@{} revise ...` to retry.",
                    bot_username
                );
                let _ = github::post_issue_comment(
                    api_base, token, owner, repo_name, pr.number, &body,
                )
                .await;
                state.revisions_applied = state.revisions_applied.saturating_add(1);
                advance_seen(&mut latest_seen, comment.created_at);
                write_state(workspace, &state)?;
            }
            Err(e) => {
                tracing::warn!(
                    url = %repo.url,
                    pr_number = pr.number,
                    "revision executor invocation errored: {e:#}"
                );
                let body = format!(
                    "✗ Revision attempt failed: {}. The PR is unchanged. Reply with another `@{} revise ...` to retry, or close the PR if the request cannot be satisfied.",
                    e, bot_username
                );
                let _ = github::post_issue_comment(
                    api_base, token, owner, repo_name, pr.number, &body,
                )
                .await;
                state.revisions_applied = state.revisions_applied.saturating_add(1);
                advance_seen(&mut latest_seen, comment.created_at);
                write_state(workspace, &state)?;
            }
        }
    }
    if let Some(t) = latest_seen
        && t > state.last_seen_comment_at
    {
        state.last_seen_comment_at = t;
        write_state(workspace, &state)?;
    }
    Ok(())
}

fn advance_seen(latest: &mut Option<DateTime<Utc>>, candidate: DateTime<Utc>) {
    match latest {
        Some(curr) if *curr >= candidate => {}
        _ => *latest = Some(candidate),
    }
}

fn build_commit_subject(change: &str, revision_text: &str) -> String {
    let mut excerpt: String = revision_text
        .chars()
        .filter(|c| *c != '\n')
        .take(60)
        .collect();
    excerpt = excerpt.trim().to_string();
    format!("revise: {change}: {excerpt}")
}

fn apply_revision_commit(
    workspace: &Path,
    repo: &RepositoryConfig,
    push_remote: &str,
    commit_subject: &str,
) -> Result<()> {
    crate::git::add_all(workspace)?;
    crate::git::commit(workspace, commit_subject)?;
    crate::git::push_force_with_lease(workspace, &repo.agent_branch, push_remote)?;
    Ok(())
}

/// Execute the revision via the executor. Captures the PR diff via
/// `git diff <base>..<agent>` and bundles the context.
#[allow(clippy::too_many_arguments)]
async fn execute_revision(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    change_name: &str,
    revision_text: &str,
    pr_body: String,
    pr_change_list: String,
    agent_implementation_notes: String,
) -> Result<ExecutorOutcome> {
    // a20a5 expanded the RevisionContext to carry PR-sourced material
    // (pr_body, pr_change_list, agent_implementation_notes). The
    // per-arg expansion crosses clippy's seven-argument threshold;
    // bundling into a struct would be cleaner but invasive for one
    // internal call site. Matches the established codebase pattern
    // (see handle_message_with_context in chatops::operator_commands).
    let pr_diff = crate::git::diff_three_dot(workspace, &repo.base_branch, &repo.agent_branch)
        .unwrap_or_default();
    let ctx = RevisionContext {
        change_name: change_name.to_string(),
        pr_diff,
        revision_text: revision_text.to_string(),
        pr_body,
        pr_change_list,
        agent_implementation_notes,
    };
    executor.run_revision(workspace, change_name, &ctx).await
}

/// Per a20a5: extract the original implementer's narrative from PR
/// comments. Matches issue comments whose body starts with the
/// canonical `## Agent implementation notes` heading (per the
/// `Implementer-summary PR comment` requirement). Concatenates matched
/// bodies in posted-order, separated by `\n\n---\n\n`. Returns an
/// empty string when no matching comments exist (e.g., revise was
/// posted within the same iteration the PR was created — the LLM
/// still has the diff + body + revision text to work with).
pub(crate) fn extract_agent_implementation_notes(
    comments: &[github::IssueComment],
) -> String {
    const HEADING: &str = "## Agent implementation notes";
    let matches: Vec<&str> = comments
        .iter()
        .filter(|c| c.body.starts_with(HEADING))
        .map(|c| c.body.as_str())
        .collect();
    matches.join("\n\n---\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn sample_state(pr: u64) -> RevisionState {
        RevisionState {
            pr_number: pr,
            agent_branch: "agent-q".to_string(),
            last_seen_comment_at: ts("2026-05-25T10:00:00Z"),
            revisions_applied: 1,
            revision_cap: 5,
            cap_decline_posted: false,
        }
    }

    // -------- state-file IO --------

    #[test]
    fn read_state_returns_none_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let got = read_state(tmp.path(), 99).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn write_then_read_round_trips_every_field() {
        let tmp = TempDir::new().unwrap();
        let original = RevisionState {
            pr_number: 42,
            agent_branch: "agent-q".to_string(),
            last_seen_comment_at: ts("2026-05-25T10:00:00Z"),
            revisions_applied: 3,
            revision_cap: 5,
            cap_decline_posted: true,
        };
        write_state(tmp.path(), &original).unwrap();
        let got = read_state(tmp.path(), 42).unwrap().expect("file exists");
        assert_eq!(got, original);
    }

    #[test]
    fn prune_removes_state_for_closed_prs() {
        let tmp = TempDir::new().unwrap();
        write_state(tmp.path(), &sample_state(1)).unwrap();
        write_state(tmp.path(), &sample_state(2)).unwrap();
        write_state(tmp.path(), &sample_state(3)).unwrap();

        let mut open = HashSet::new();
        open.insert(2u64);
        let removed = prune_closed_prs(tmp.path(), &open).unwrap();
        assert_eq!(removed, 2);
        assert!(read_state(tmp.path(), 1).unwrap().is_none());
        assert!(read_state(tmp.path(), 2).unwrap().is_some());
        assert!(read_state(tmp.path(), 3).unwrap().is_none());
    }

    #[test]
    fn prune_missing_directory_is_zero() {
        let tmp = TempDir::new().unwrap();
        let mut open = HashSet::new();
        open.insert(1u64);
        let removed = prune_closed_prs(tmp.path(), &open).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn prune_ignores_non_json_files_and_garbage_names() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(LEGACY_REVISIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.txt"), "x").unwrap();
        std::fs::write(dir.join("not-a-number.json"), "x").unwrap();
        write_state(tmp.path(), &sample_state(1)).unwrap();
        let mut open = HashSet::new();
        let removed = prune_closed_prs(tmp.path(), &open).unwrap();
        // Only `1.json` removed; non-numeric stems left alone.
        assert_eq!(removed, 1);
        assert!(dir.join("readme.txt").exists());
        assert!(dir.join("not-a-number.json").exists());
        open.insert(99u64);
        let _ = prune_closed_prs(tmp.path(), &open).unwrap();
        assert!(dir.join("readme.txt").exists());
    }

    /// 2.2: an interrupted write must not leave a partial canonical file
    /// on disk. We simulate by creating a temp file that mimics a torn
    /// write (incomplete JSON) under the revisions dir, then verifying
    /// the canonical state file (`<pr>.json`) does NOT exist.
    #[test]
    fn atomic_write_tolerates_interrupted_partial_temp() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(LEGACY_REVISIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate a temp file left behind by a previous interrupted write.
        std::fs::write(dir.join(".tmpABCDEF"), "{incomplete json").unwrap();
        // The canonical file does NOT exist; read returns None.
        let got = read_state(tmp.path(), 42).unwrap();
        assert!(got.is_none());
        // A successful write then read works as expected.
        write_state(tmp.path(), &sample_state(42)).unwrap();
        assert!(read_state(tmp.path(), 42).unwrap().is_some());
    }

    #[test]
    fn read_state_errors_on_corrupt_json() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(LEGACY_REVISIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("42.json"), "not json").unwrap();
        let err = read_state(tmp.path(), 42).expect_err("corrupt JSON must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("parsing"), "got: {msg}");
    }

    #[test]
    fn remove_state_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        // Removing a never-existing file is Ok.
        remove_state(tmp.path(), 99).unwrap();
        write_state(tmp.path(), &sample_state(42)).unwrap();
        remove_state(tmp.path(), 42).unwrap();
        // Second remove is also Ok.
        remove_state(tmp.path(), 42).unwrap();
    }

    // -------- parser tests --------

    #[test]
    fn parse_basic_revision_trigger() {
        assert_eq!(
            parse_revision_trigger("@bot revise foo", "bot"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn parse_case_insensitive_verb() {
        assert_eq!(
            parse_revision_trigger("@bot REVISE foo", "bot"),
            Some("foo".to_string())
        );
        assert_eq!(
            parse_revision_trigger("@bot Revise foo", "bot"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn parse_case_insensitive_mention() {
        // bot_username is "bot"; mention is `@BOT`.
        assert_eq!(
            parse_revision_trigger("@BOT revise foo", "bot"),
            Some("foo".to_string())
        );
        // bot_username with mixed case is matched case-insensitively too.
        assert_eq!(
            parse_revision_trigger("@MyBot revise foo", "mybot"),
            Some("foo".to_string())
        );
    }

    // ---------- a20a5: agent-notes extraction ----------

    fn ic(body: &str, ts: chrono::DateTime<chrono::Utc>) -> crate::github::IssueComment {
        crate::github::IssueComment {
            id: 1,
            body: body.to_string(),
            user: None,
            created_at: ts,
        }
    }

    #[test]
    fn extract_agent_notes_returns_single_match() {
        let ts = chrono::Utc::now();
        let comments = vec![
            ic("Some unrelated comment", ts),
            ic("## Agent implementation notes\n\nDid X and Y.", ts),
        ];
        let out = extract_agent_implementation_notes(&comments);
        assert_eq!(out, "## Agent implementation notes\n\nDid X and Y.");
    }

    #[test]
    fn extract_agent_notes_concatenates_multiple_with_separator() {
        let ts = chrono::Utc::now();
        let comments = vec![
            ic("## Agent implementation notes\n\nFirst pass.", ts),
            ic("Some revise reply.", ts),
            ic("## Agent implementation notes\n\nSecond pass.", ts),
        ];
        let out = extract_agent_implementation_notes(&comments);
        assert_eq!(
            out,
            "## Agent implementation notes\n\nFirst pass.\n\n---\n\n## Agent implementation notes\n\nSecond pass."
        );
    }

    #[test]
    fn extract_agent_notes_returns_empty_when_no_matches() {
        let ts = chrono::Utc::now();
        let comments = vec![
            ic("Some unrelated comment", ts),
            ic("@bot revise add a thing", ts),
            ic("✅ Revision applied: blah.", ts),
        ];
        let out = extract_agent_implementation_notes(&comments);
        assert!(out.is_empty());
    }

    #[test]
    fn extract_agent_notes_requires_exact_heading_prefix() {
        let ts = chrono::Utc::now();
        let comments = vec![
            // Indented heading does NOT match — must start at column 0.
            ic("  ## Agent implementation notes\n\nIndented.", ts),
            // Wrong case does NOT match.
            ic("## agent implementation notes\n\nLowercase.", ts),
            // Subtle word difference does NOT match.
            ic("## Agent's implementation notes\n\nApostrophe.", ts),
        ];
        let out = extract_agent_implementation_notes(&comments);
        assert!(
            out.is_empty(),
            "false-match in extracted notes:\n{out}"
        );
    }

    #[test]
    fn parse_returns_none_for_non_revise_verb() {
        assert!(parse_revision_trigger("@bot foo", "bot").is_none());
        assert!(parse_revision_trigger("@bot looks good", "bot").is_none());
    }

    #[test]
    fn parse_returns_none_for_no_text_after_verb() {
        assert!(parse_revision_trigger("@bot revise", "bot").is_none());
        assert!(parse_revision_trigger("@bot revise   ", "bot").is_none());
        assert!(parse_revision_trigger("@bot revise\n\n  ", "bot").is_none());
    }

    #[test]
    fn parse_returns_none_when_mention_not_at_start() {
        assert!(parse_revision_trigger("foo @bot revise bar", "bot").is_none());
        assert!(parse_revision_trigger("  prefix @bot revise foo", "bot").is_none());
    }

    #[test]
    fn parse_returns_none_for_wrong_mention() {
        assert!(parse_revision_trigger("@notbot revise foo", "bot").is_none());
        assert!(parse_revision_trigger("@bots revise foo", "bot").is_none());
        assert!(parse_revision_trigger("revise foo", "bot").is_none());
    }

    #[test]
    fn parse_multi_line_revision_text_preserved() {
        let body = "@bot revise line1\n\nline2";
        let got = parse_revision_trigger(body, "bot").expect("trigger");
        assert_eq!(got, "line1\n\nline2");
    }

    #[test]
    fn parse_leading_whitespace_in_body_is_tolerated() {
        // GitHub renders leading whitespace as-is; we trim it for
        // robustness so a user-friendly leading space doesn't kill the
        // trigger.
        let body = "   @bot revise the find_user function";
        let got = parse_revision_trigger(body, "bot").expect("trigger");
        assert_eq!(got, "the find_user function");
    }

    #[test]
    fn parse_empty_body_returns_none() {
        assert!(parse_revision_trigger("", "bot").is_none());
        assert!(parse_revision_trigger("   ", "bot").is_none());
        assert!(parse_revision_trigger("\n\n", "bot").is_none());
    }

    #[test]
    fn parse_skips_leading_reviewer_revision_marker() {
        // The dispatcher passes the marker-bearing body through; the
        // parser must locate the mention on the line below the marker.
        let body = "<!-- reviewer-revision -->\n@bot revise the find_user function";
        let got = parse_revision_trigger(body, "bot").expect("trigger");
        assert_eq!(got, "the find_user function");
    }

    #[test]
    fn parse_skips_arbitrary_leading_html_comments() {
        // Any whole-line HTML comment is stripped before the mention
        // search — the parser doesn't special-case the reviewer-revision
        // marker, it just tolerates leading metadata lines.
        let body = "<!-- arbitrary -->\n<!-- another -->\n@bot revise foo";
        let got = parse_revision_trigger(body, "bot").expect("trigger");
        assert_eq!(got, "foo");
    }

    #[test]
    fn parse_does_not_skip_inline_html_comment() {
        // An HTML comment that shares a line with other content does NOT
        // count as a leading-metadata line; the mention search runs from
        // the start of that line and fails.
        let body = "<!-- inline --> @bot revise foo";
        assert!(parse_revision_trigger(body, "bot").is_none());
    }

    #[test]
    fn extract_change_list_parses_polling_loop_body() {
        let body = "## change-a\n\nbody\n\n## change-b\n\nbody\n\nChanges implemented in this pass:\n\n- change-a\n- change-b\n";
        let changes = extract_change_list_from_pr_body(Some(body));
        assert_eq!(changes, vec!["change-a", "change-b"]);
    }

    #[test]
    fn extract_change_list_handles_missing_section() {
        let changes = extract_change_list_from_pr_body(Some("no marker here"));
        assert!(changes.is_empty());
    }

    #[test]
    fn extract_change_list_handles_none_body() {
        let changes = extract_change_list_from_pr_body(None);
        assert!(changes.is_empty());
    }

    #[test]
    fn build_commit_subject_truncates_long_text() {
        let long: String = "x".repeat(120);
        let s = build_commit_subject("foo", &long);
        // 60 chars of excerpt + the prefix.
        assert_eq!(s.len(), "revise: foo: ".len() + 60);
    }

    #[test]
    fn build_commit_subject_strips_newlines_in_excerpt() {
        let s = build_commit_subject("foo", "line1\nline2");
        assert!(!s.contains('\n'), "newlines should be removed: {s:?}");
        assert!(s.contains("line1"), "got: {s}");
    }

    // -------- dispatcher integration (mockito + stub executor) --------

    use crate::chatops::{ChatOpsBackend, HumanReply};
    use crate::config::{GithubConfig, RepositoryConfig};
    use crate::executor::{Executor, ExecutorOutcome, ResumeHandle};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn make_repo(url: &str) -> RepositoryConfig {
        RepositoryConfig {
            url: url.to_string(),
            local_path: None,
            base_branch: "main".to_string(),
            agent_branch: "agent-q".to_string(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
        }
    }

    fn make_github(token_env: &str) -> GithubConfig {
        GithubConfig {
            token_env: token_env.to_string(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        }
    }

    /// Stub executor that records every `run_revision` call and returns a
    /// scripted outcome. `run`/`resume` are stubbed to `Completed` for
    /// simplicity (the dispatcher only calls `run_revision`).
    struct StubExecutor {
        scripted: Mutex<Vec<ExecutorOutcome>>,
        calls: AtomicUsize,
    }

    impl StubExecutor {
        fn new(outcomes: Vec<ExecutorOutcome>) -> Self {
            Self {
                scripted: Mutex::new(outcomes),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Executor for StubExecutor {
        async fn run(&self, _workspace: &Path, _change: &str) -> Result<ExecutorOutcome> {
            Ok(ExecutorOutcome::Completed { final_answer: None })
        }
        async fn resume(
            &self,
            _handle: ResumeHandle,
            _answer: &str,
        ) -> Result<ExecutorOutcome> {
            Ok(ExecutorOutcome::Completed { final_answer: None })
        }
        async fn run_revision(
            &self,
            workspace: &Path,
            _change: &str,
            _revision_context: &RevisionContext,
        ) -> Result<ExecutorOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.scripted.lock().unwrap();
            let outcome = if guard.is_empty() {
                ExecutorOutcome::Completed { final_answer: None }
            } else {
                guard.remove(0)
            };
            // Simulate the executor writing a file so the `git add -A`
            // path in the dispatcher's Completed branch has something to
            // commit.
            if matches!(outcome, ExecutorOutcome::Completed { .. }) {
                let _ = std::fs::write(workspace.join("rev-marker.txt"), "rev");
            }
            Ok(outcome)
        }
    }

    /// Minimal ChatOpsBackend stub that records every notification posted.
    /// The dispatcher only ever calls `post_notification`; the other
    /// methods are unused so they return defaults.
    struct StubChatOps {
        notifications: Mutex<Vec<String>>,
    }
    impl StubChatOps {
        fn new() -> Self {
            Self {
                notifications: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait]
    impl ChatOpsBackend for StubChatOps {
        fn provider_name(&self) -> &'static str {
            "stub"
        }
        fn is_experimental(&self) -> bool {
            true
        }
        async fn post_question(
            &self,
            _channel: &str,
            _change: &str,
            _question: &str,
        ) -> Result<String> {
            unreachable!("dispatcher does not post questions in v1")
        }
        async fn poll_thread_for_human_reply(
            &self,
            _channel: &str,
            _handle: &str,
        ) -> Result<Option<HumanReply>> {
            Ok(None)
        }
        async fn post_notification(&self, _channel: &str, text: &str) -> Result<()> {
            self.notifications.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn init_git_workspace() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&path)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(path.join("README.md"), "hi\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "initial"]);
        // origin remote pointing to a bare repo so push --force-with-lease
        // succeeds during dispatch.
        let bare = dir.path().with_extension("bare.git");
        let st = std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&bare)
            .status()
            .unwrap();
        assert!(st.success());
        run(&[
            "remote",
            "add",
            "origin",
            bare.to_string_lossy().as_ref(),
        ]);
        run(&["push", "-q", "origin", "main"]);
        run(&["checkout", "-q", "-B", "agent-q"]);
        run(&["push", "-q", "origin", "agent-q"]);
        (dir, path)
    }

    fn token_env_set(name: &str) {
        unsafe { std::env::set_var(name, "test-token") };
    }
    fn token_env_clear(name: &str) {
        unsafe { std::env::remove_var(name) };
    }

    /// Integration: a PR with no comments produces no executor calls and
    /// no PR comments — the dispatcher is a no-op.
    #[tokio::test]
    async fn dispatcher_no_op_when_no_comments() {
        let env_var = "REVISIONS_TOKEN_NO_OP";
        token_env_set(env_var);

        let mut server = mockito::Server::new_async().await;
        // /user → bot username
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        // /pulls → one open PR
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 7,
                    "title": "PR",
                    "html_url": "https://example.invalid/pr/7",
                    "state": "open",
                    "body": "Changes implemented in this pass:\n\n- some-change",
                    "created_at": "2026-05-25T10:00:00Z",
                    "head": {"ref": "agent-q"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .create_async()
            .await;
        // /issues/7/comments → empty
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/7/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(Vec::new());
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);

        token_env_clear(env_var);
    }

    /// Integration: a triggering comment is detected and the executor's
    /// `run_revision` method is invoked once. On `Completed`, a success
    /// PR comment is posted and state is persisted.
    #[tokio::test]
    async fn dispatcher_triggering_comment_runs_revision() {
        let env_var = "REVISIONS_TOKEN_TRIGGER";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 9,
                    "title": "PR",
                    "html_url": "https://example.invalid/pr/9",
                    "state": "open",
                    "body": "Changes implemented in this pass:\n\n- my-change",
                    "created_at": "2026-05-25T10:00:00Z",
                    "head": {"ref": "agent-q"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .create_async()
            .await;
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/9/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "id": 1,
                    "body": "@my-bot revise drop error info",
                    "user": {"login": "operator"},
                    "created_at": "2026-05-25T11:00:00Z"
                }]"#,
            )
            .create_async()
            .await;
        let post_reply = server
            .mock("POST", "/repos/owner/repo/issues/9/comments")
            .match_body(mockito::Matcher::Regex(
                "Revision applied".to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":42}"#)
            .expect(1)
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(vec![ExecutorOutcome::Completed { final_answer: None }]);
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        post_reply.assert_async().await;
        let state = read_state(&ws, 9).unwrap().expect("state persisted");
        assert_eq!(state.revisions_applied, 1);

        token_env_clear(env_var);
    }

    /// Integration: the bot's own comments are filtered before parsing.
    /// A reply with the bot as author does NOT trigger a recursive
    /// revision even if its body looks like a trigger.
    #[tokio::test]
    async fn dispatcher_filters_bot_authored_comments() {
        let env_var = "REVISIONS_TOKEN_BOT_FILTER";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 11,
                    "title": "PR",
                    "html_url": "https://example.invalid/pr/11",
                    "state": "open",
                    "body": "Changes implemented in this pass:\n\n- my-change",
                    "created_at": "2026-05-25T10:00:00Z",
                    "head": {"ref": "agent-q"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .create_async()
            .await;
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/11/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "id": 1,
                    "body": "@my-bot revise nothing to do here",
                    "user": {"login": "my-bot"},
                    "created_at": "2026-05-25T11:00:00Z"
                }]"#,
            )
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(Vec::new());
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        let state = read_state(&ws, 11).unwrap().expect("state persisted");
        // last_seen advanced past the bot comment but no revision applied.
        assert_eq!(state.revisions_applied, 0);

        token_env_clear(env_var);
    }

    /// Integration: a bot-authored comment whose body starts with the
    /// reviewer-revision marker bypasses the self-author filter and is
    /// dispatched normally. This is the only sanctioned bypass; other
    /// bot-authored comments continue to be filtered (covered above).
    #[tokio::test]
    async fn dispatcher_reviewer_revision_marker_bypasses_self_author_filter() {
        let env_var = "REVISIONS_TOKEN_REVIEWER_BYPASS";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 19,
                    "title": "PR",
                    "html_url": "https://example.invalid/pr/19",
                    "state": "open",
                    "body": "Changes implemented in this pass:\n\n- my-change",
                    "created_at": "2026-05-25T10:00:00Z",
                    "head": {"ref": "agent-q"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .create_async()
            .await;
        // Bot-authored comment carrying the reviewer-revision marker.
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/19/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "id": 1,
                    "body": "<!-- reviewer-revision -->\n@my-bot revise fix the find_user helper",
                    "user": {"login": "my-bot"},
                    "created_at": "2026-05-25T11:00:00Z"
                }]"#,
            )
            .create_async()
            .await;
        let post_reply = server
            .mock("POST", "/repos/owner/repo/issues/19/comments")
            .match_body(mockito::Matcher::Regex("Revision applied".to_string()))
            .with_status(201)
            .with_body(r#"{"id":42}"#)
            .expect(1)
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(vec![ExecutorOutcome::Completed { final_answer: None }]);
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        // The marker-bearing bot comment WAS passed through to the
        // executor — proves the bypass works.
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        post_reply.assert_async().await;

        token_env_clear(env_var);
    }

    /// Integration: bot-authored reply comments (which start with
    /// `✅ Revision applied:` / `✗ Revision attempt failed:`, no marker)
    /// continue to be filtered — proves the bypass is gated strictly on
    /// the marker, not on bot-authorship alone.
    #[tokio::test]
    async fn dispatcher_unmarked_bot_reply_still_filtered() {
        let env_var = "REVISIONS_TOKEN_UNMARKED_REPLY";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 21,
                    "title": "PR",
                    "html_url": "https://example.invalid/pr/21",
                    "state": "open",
                    "body": "Changes implemented in this pass:\n\n- my-change",
                    "created_at": "2026-05-25T10:00:00Z",
                    "head": {"ref": "agent-q"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .create_async()
            .await;
        // Bot-authored reply (no marker); even though its body looks
        // trigger-like, the filter must drop it.
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/21/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "id": 1,
                    "body": "@my-bot revise oops should not recurse",
                    "user": {"login": "my-bot"},
                    "created_at": "2026-05-25T11:00:00Z"
                }]"#,
            )
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(Vec::new());
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);

        token_env_clear(env_var);
    }

    /// Integration: when the cap has been reached, the dispatcher posts
    /// the cap-decline comment + chatops notification once, sets the
    /// `cap_decline_posted` flag, and does NOT call the executor.
    #[tokio::test]
    async fn dispatcher_cap_decline_fires_once() {
        let env_var = "REVISIONS_TOKEN_CAP";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 13,
                    "title": "PR",
                    "html_url": "https://example.invalid/pr/13",
                    "state": "open",
                    "body": "Changes implemented in this pass:\n\n- my-change",
                    "created_at": "2026-05-25T10:00:00Z",
                    "head": {"ref": "agent-q"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .create_async()
            .await;
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/13/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "id": 1,
                    "body": "@my-bot revise after cap",
                    "user": {"login": "operator"},
                    "created_at": "2026-05-25T11:00:00Z"
                }]"#,
            )
            .create_async()
            .await;
        let decline = server
            .mock("POST", "/repos/owner/repo/issues/13/comments")
            .match_body(mockito::Matcher::Regex(
                "Revision cap reached".to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":99}"#)
            .expect(1)
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        // Pre-seed the state file so the cap is already reached.
        let pre_state = RevisionState {
            pr_number: 13,
            agent_branch: "agent-q".to_string(),
            last_seen_comment_at: ts("2026-05-25T09:00:00Z"),
            revisions_applied: 5,
            revision_cap: 5,
            cap_decline_posted: false,
        };
        write_state(&ws, &pre_state).unwrap();

        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(Vec::new());
        let chatops = std::sync::Arc::new(StubChatOps::new());
        let ctx = ChatOpsCtx {
            chatops: chatops.as_ref(),
            channel: "C-test",
        };
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, Some(ctx), 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        decline.assert_async().await;
        // The chatops backend received the cap-decline notification.
        let notifs = chatops.notifications.lock().unwrap().clone();
        assert!(!notifs.is_empty(), "chatops notification must fire");
        assert!(
            notifs.iter().any(|n| n.contains("hit the revision cap")),
            "expected cap-decline chatops notification; got: {notifs:?}"
        );
        // State now records the decline was posted.
        let state = read_state(&ws, 13).unwrap().expect("state persisted");
        assert!(state.cap_decline_posted);

        token_env_clear(env_var);
    }

    /// Integration: closed-PR state files are pruned at iteration start.
    #[tokio::test]
    async fn dispatcher_prunes_state_for_closed_prs() {
        let env_var = "REVISIONS_TOKEN_PRUNE";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        // No open PRs at all.
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        write_state(&ws, &sample_state(5)).unwrap();
        write_state(&ws, &sample_state(7)).unwrap();
        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(Vec::new());
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        // Both state files should be gone (no open PR claims them).
        assert!(read_state(&ws, 5).unwrap().is_none());
        assert!(read_state(&ws, 7).unwrap().is_none());

        token_env_clear(env_var);
    }

    /// Regression test for the a20a4 fork-PR-mode head-qualifier bug.
    ///
    /// Configures `github.fork_owner = "fork-acc"` and mocks GitHub
    /// with an exact `head=fork-acc:agent-q` matcher. Pre-fix code
    /// passed `&owner` (the upstream `owner`) to
    /// `list_open_prs_for_head`, which constructed
    /// `head=owner:agent-q` — never matching the mock. The mock's
    /// `.expect(1)` would fail because the request never arrived.
    ///
    /// Post-fix, the dispatcher computes
    /// `head_owner = fork_owner.as_deref().unwrap_or(&owner)` AND passes
    /// it explicitly. The mock matches, the dispatcher fetches the
    /// PR's comments, AND the test asserts the dispatcher proceeded
    /// past the empty-list early-return.
    #[tokio::test]
    async fn dispatcher_finds_pr_in_fork_pr_mode() {
        let env_var = "REVISIONS_TOKEN_FORK_MODE";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        // Strict head matcher — only `fork-acc:agent-q` may match.
        let pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("state".into(), "open".into()),
                mockito::Matcher::UrlEncoded(
                    "head".into(),
                    "fork-acc:agent-q".into(),
                ),
                mockito::Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 99,
                    "title": "PR",
                    "html_url": "https://example.invalid/pr/99",
                    "state": "open",
                    "body": "Changes implemented in this pass:\n\n- my-change",
                    "created_at": "2026-05-25T10:00:00Z",
                    "head": {"ref": "agent-q"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .expect(1)
            .create_async()
            .await;
        // The dispatcher continues into the per-PR processing path; it
        // fetches comments for PR #99. Stub with an empty list so no
        // revision attempt is made — we just need to prove the dispatcher
        // got past the empty-PR-list early-return.
        let comments = server
            .mock("GET", "/repos/owner/repo/issues/99/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        let repo = make_repo("git@github.com:owner/repo.git");
        let mut gh = make_github(env_var);
        gh.fork_owner = Some("fork-acc".to_string());
        let executor = StubExecutor::new(Vec::new());
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");

        // The PR-list mock fired exactly once — the dispatcher used
        // the fork-owner-qualified `head`. Pre-fix code would not have
        // matched and this assertion would fail.
        pulls.assert_async().await;
        // The dispatcher proceeded past the empty-list early-return
        // and fetched comments for the PR.
        comments.assert_async().await;

        token_env_clear(env_var);
    }

    // -------- strict-since dispatcher filter (a2705) --------

    fn operator_comment_body(comment_created_at: &str) -> String {
        format!(
            r#"[{{
                "id": 1,
                "body": "@my-bot revise tweak the helper",
                "user": {{"login": "operator"}},
                "created_at": "{comment_created_at}"
            }}]"#,
        )
    }

    fn pr_summary_body(pr_number: u64, pr_created_at: &str) -> String {
        format!(
            r#"[{{
                "number": {pr_number},
                "title": "PR",
                "html_url": "https://example.invalid/pr/{pr_number}",
                "state": "open",
                "body": "Changes implemented in this pass:\n\n- my-change",
                "created_at": "{pr_created_at}",
                "head": {{"ref": "agent-q"}},
                "base": {{"ref": "main"}}
            }}]"#,
        )
    }

    /// 2.2: a comment whose `created_at` exactly equals the marker is
    /// skipped client-side — neither the bot-author filter nor the
    /// trigger parser runs, AND `run_revision` is not invoked.
    #[tokio::test]
    async fn dispatcher_skips_comment_at_exact_marker_timestamp() {
        let env_var = "REVISIONS_TOKEN_STRICT_SINCE_EQ";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(pr_summary_body(31, "2026-05-25T10:00:00Z"))
            .create_async()
            .await;
        // GitHub returns the comment with created_at == marker (simulating
        // the truncation-induced inclusive `since` behavior we are
        // defending against).
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/31/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(operator_comment_body("2026-05-25T11:00:00Z"))
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        // Pre-seed state with `last_seen_comment_at` exactly equal to the
        // comment's `created_at`.
        write_state(
            &ws,
            &RevisionState {
                pr_number: 31,
                agent_branch: "agent-q".to_string(),
                last_seen_comment_at: ts("2026-05-25T11:00:00Z"),
                revisions_applied: 0,
                revision_cap: 5,
                cap_decline_posted: false,
            },
        )
        .unwrap();

        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(Vec::new());
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            0,
            "comment at exact marker timestamp must not invoke run_revision",
        );
        let state = read_state(&ws, 31).unwrap().expect("state persisted");
        assert_eq!(state.revisions_applied, 0);
        assert_eq!(state.last_seen_comment_at, ts("2026-05-25T11:00:00Z"));

        token_env_clear(env_var);
    }

    /// 2.3: a comment whose `created_at` is strictly less than the marker
    /// (e.g. a replication-lag re-fetch of an older comment) is skipped.
    #[tokio::test]
    async fn dispatcher_skips_comment_before_marker_timestamp() {
        let env_var = "REVISIONS_TOKEN_STRICT_SINCE_LT";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(pr_summary_body(33, "2026-05-25T10:00:00Z"))
            .create_async()
            .await;
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/33/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(operator_comment_body("2026-05-25T10:30:00Z"))
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        write_state(
            &ws,
            &RevisionState {
                pr_number: 33,
                agent_branch: "agent-q".to_string(),
                // Marker is 30 minutes AFTER the comment's created_at.
                last_seen_comment_at: ts("2026-05-25T11:00:00Z"),
                revisions_applied: 1,
                revision_cap: 5,
                cap_decline_posted: false,
            },
        )
        .unwrap();

        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(Vec::new());
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            0,
            "comment older than marker must not invoke run_revision",
        );
        let state = read_state(&ws, 33).unwrap().expect("state persisted");
        assert_eq!(state.revisions_applied, 1);
        assert_eq!(state.last_seen_comment_at, ts("2026-05-25T11:00:00Z"));

        token_env_clear(env_var);
    }

    /// 2.4: a comment whose `created_at` is strictly greater than the
    /// marker IS processed (the happy-path regression — the strict-since
    /// filter must not drop legitimately-new comments).
    #[tokio::test]
    async fn dispatcher_processes_comment_after_marker_timestamp() {
        let env_var = "REVISIONS_TOKEN_STRICT_SINCE_GT";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(pr_summary_body(35, "2026-05-25T10:00:00Z"))
            .create_async()
            .await;
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/35/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(operator_comment_body("2026-05-25T12:00:00Z"))
            .create_async()
            .await;
        // The Completed path posts a "Revision applied" PR reply.
        let post_reply = server
            .mock("POST", "/repos/owner/repo/issues/35/comments")
            .match_body(mockito::Matcher::Regex("Revision applied".to_string()))
            .with_status(201)
            .with_body(r#"{"id":42}"#)
            .expect(1)
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        write_state(
            &ws,
            &RevisionState {
                pr_number: 35,
                agent_branch: "agent-q".to_string(),
                // Marker is one hour BEFORE the comment's created_at.
                last_seen_comment_at: ts("2026-05-25T11:00:00Z"),
                revisions_applied: 0,
                revision_cap: 5,
                cap_decline_posted: false,
            },
        )
        .unwrap();

        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor =
            StubExecutor::new(vec![ExecutorOutcome::Completed { final_answer: None }]);
        let cancel = CancellationToken::new();
        process_revision_requests_at(
            &ws, &repo, &gh, &executor, None, 5, cancel, &server.url(),
        )
        .await
        .expect("dispatcher should succeed");
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            1,
            "comment newer than marker must invoke run_revision exactly once",
        );
        post_reply.assert_async().await;
        let state = read_state(&ws, 35).unwrap().expect("state persisted");
        assert_eq!(state.revisions_applied, 1);
        assert_eq!(state.last_seen_comment_at, ts("2026-05-25T12:00:00Z"));

        token_env_clear(env_var);
    }

    /// 3.1: end-to-end two-iteration regression. The first iteration
    /// processes a comment AND advances the marker to its `created_at`.
    /// The second iteration's GitHub mock returns the SAME comment again
    /// (simulating GitHub's truncation-induced re-fetch). The strict-since
    /// filter skips the duplicate AND `run_revision` is invoked exactly
    /// once across both iterations.
    #[tokio::test]
    async fn dispatcher_same_comment_processed_at_most_once_across_iterations() {
        let env_var = "REVISIONS_TOKEN_STRICT_SINCE_E2E";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        // Both iterations re-fetch the open-PRs list and the comments
        // list; allow each mock to fire any number of times.
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(pr_summary_body(37, "2026-05-25T10:00:00Z"))
            .create_async()
            .await;
        // The comment's created_at is T1 = 11:00:00Z. Both iterations get
        // this same comment in the response (simulating the bug we're
        // defending against).
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/37/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(operator_comment_body("2026-05-25T11:00:00Z"))
            .create_async()
            .await;
        // Iter 1's Failed path posts a "Revision attempt failed" reply.
        // Iter 2's strict-since filter skips the comment so no second
        // reply is posted. Use `expect(1)` to assert the count.
        let post_reply = server
            .mock("POST", "/repos/owner/repo/issues/37/comments")
            .match_body(mockito::Matcher::Regex(
                "Revision attempt failed".to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":42}"#)
            .expect(1)
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        // Pre-seed iter-1 state: T0 = 09:00:00Z (before the comment).
        write_state(
            &ws,
            &RevisionState {
                pr_number: 37,
                agent_branch: "agent-q".to_string(),
                last_seen_comment_at: ts("2026-05-25T09:00:00Z"),
                revisions_applied: 0,
                revision_cap: 5,
                cap_decline_posted: false,
            },
        )
        .unwrap();

        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        // Only iter 1 should call run_revision; script a single Failed
        // outcome. (If iter 2 leaked a call through, the stub would fall
        // back to its empty-script default of Completed, which we'd
        // notice via state.revisions_applied incrementing.)
        let executor = StubExecutor::new(vec![ExecutorOutcome::Failed {
            reason: "timeout".to_string(),
        }]);

        // Iteration 1.
        process_revision_requests_at(
            &ws,
            &repo,
            &gh,
            &executor,
            None,
            5,
            CancellationToken::new(),
            &server.url(),
        )
        .await
        .expect("iter 1 dispatcher should succeed");
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            1,
            "iter 1: comment should be processed",
        );
        let state = read_state(&ws, 37).unwrap().expect("iter 1 state persisted");
        assert_eq!(state.revisions_applied, 1);
        assert_eq!(state.last_seen_comment_at, ts("2026-05-25T11:00:00Z"));

        // Iteration 2: same comment is re-fetched, strict-since filter
        // must skip it.
        process_revision_requests_at(
            &ws,
            &repo,
            &gh,
            &executor,
            None,
            5,
            CancellationToken::new(),
            &server.url(),
        )
        .await
        .expect("iter 2 dispatcher should succeed");
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            1,
            "iter 2: strict-since filter must skip the duplicate",
        );
        post_reply.assert_async().await;
        let state = read_state(&ws, 37).unwrap().expect("iter 2 state persisted");
        assert_eq!(
            state.revisions_applied, 1,
            "counter must not be incremented twice",
        );
        assert_eq!(state.last_seen_comment_at, ts("2026-05-25T11:00:00Z"));

        token_env_clear(env_var);
    }

    /// 3.2: AskUser regression. An AskUser outcome in iteration 1 leaves
    /// the marker UNCHANGED (per the canonical AskUser-preserves-marker
    /// requirement). Iteration 2 receives the SAME comment; because the
    /// marker was held back, `comment.created_at > state.last_seen_comment_at`
    /// holds AND the strict-since filter does NOT skip it. The comment IS
    /// reprocessed in iter 2 — the strict-since filter does not regress
    /// AskUser semantics.
    #[tokio::test]
    async fn dispatcher_askuser_marker_preservation_allows_reprocessing() {
        let env_var = "REVISIONS_TOKEN_STRICT_SINCE_ASKUSER";
        token_env_set(env_var);
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        let _pulls = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(pr_summary_body(39, "2026-05-25T10:00:00Z"))
            .create_async()
            .await;
        let _comments = server
            .mock("GET", "/repos/owner/repo/issues/39/comments")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(operator_comment_body("2026-05-25T11:00:00Z"))
            .create_async()
            .await;
        // Iter 2 (Completed) posts a "Revision applied" reply.
        let post_reply = server
            .mock("POST", "/repos/owner/repo/issues/39/comments")
            .match_body(mockito::Matcher::Regex("Revision applied".to_string()))
            .with_status(201)
            .with_body(r#"{"id":42}"#)
            .expect(1)
            .create_async()
            .await;

        let (_dir, ws) = init_git_workspace();
        // T0 = 09:00:00Z; T1 = 11:00:00Z. Pre-seed iter-1 state.
        write_state(
            &ws,
            &RevisionState {
                pr_number: 39,
                agent_branch: "agent-q".to_string(),
                last_seen_comment_at: ts("2026-05-25T09:00:00Z"),
                revisions_applied: 0,
                revision_cap: 5,
                cap_decline_posted: false,
            },
        )
        .unwrap();

        let repo = make_repo("git@github.com:owner/repo.git");
        let gh = make_github(env_var);
        let executor = StubExecutor::new(vec![
            ExecutorOutcome::AskUser {
                question: "clarify the helper signature?".to_string(),
                resume_handle: ResumeHandle(serde_json::json!({"k": "v"})),
            },
            ExecutorOutcome::Completed { final_answer: None },
        ]);
        let chatops = std::sync::Arc::new(StubChatOps::new());
        let ctx = ChatOpsCtx {
            chatops: chatops.as_ref(),
            channel: "C-test",
        };

        // Iteration 1: AskUser → marker held back, no PR reply.
        process_revision_requests_at(
            &ws,
            &repo,
            &gh,
            &executor,
            Some(ctx),
            5,
            CancellationToken::new(),
            &server.url(),
        )
        .await
        .expect("iter 1 dispatcher should succeed");
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            1,
            "iter 1: AskUser-returning comment was processed",
        );
        let state = read_state(&ws, 39).unwrap().expect("iter 1 state persisted");
        assert_eq!(state.revisions_applied, 0);
        assert_eq!(
            state.last_seen_comment_at,
            ts("2026-05-25T09:00:00Z"),
            "AskUser must NOT advance the marker past the comment",
        );

        // Iteration 2: same comment re-arrives, the strict-since filter
        // does NOT skip it (because the marker is still at T0 < T1).
        let chatops2 = std::sync::Arc::new(StubChatOps::new());
        let ctx2 = ChatOpsCtx {
            chatops: chatops2.as_ref(),
            channel: "C-test",
        };
        process_revision_requests_at(
            &ws,
            &repo,
            &gh,
            &executor,
            Some(ctx2),
            5,
            CancellationToken::new(),
            &server.url(),
        )
        .await
        .expect("iter 2 dispatcher should succeed");
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            2,
            "iter 2: AskUser-preserved comment must be reprocessed",
        );
        post_reply.assert_async().await;
        let state = read_state(&ws, 39).unwrap().expect("iter 2 state persisted");
        assert_eq!(state.revisions_applied, 1);
        assert_eq!(state.last_seen_comment_at, ts("2026-05-25T11:00:00Z"));

        token_env_clear(env_var);
    }
}
