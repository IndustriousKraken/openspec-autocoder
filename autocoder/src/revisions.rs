//! PR-comment revision loop.
//!
//! Polls open PRs whose head branch matches `repositories[].agent_branch`,
//! parses operator comments for the `@<bot> revise <text>` trigger pattern,
//! and dispatches in-place revisions through the executor.
//!
//! State per PR lives at `<workspace>/.autocoder/revisions/<pr-number>.json`.

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

const REVISIONS_DIR: &str = ".autocoder/revisions";

/// Per-PR state file persisted under
/// `<workspace>/.autocoder/revisions/<pr_number>.json`. The dispatcher
/// reads it on each iteration to know which comments are already
/// processed and how many revisions have been applied so far.
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
}

/// Return the path to a PR's state file inside `workspace`.
pub fn state_path(workspace: &Path, pr_number: u64) -> PathBuf {
    workspace
        .join(REVISIONS_DIR)
        .join(format!("{pr_number}.json"))
}

/// Return the directory under which all per-PR state files live.
fn revisions_dir(workspace: &Path) -> PathBuf {
    workspace.join(REVISIONS_DIR)
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
/// `Some(revision_text)` when the body begins with `@<bot_username>` (case-
/// insensitive on the mention) followed by `revise` (case-insensitive) and
/// at least one non-whitespace character of revision text. Otherwise
/// `None`. The returned text has leading/trailing whitespace trimmed but
/// preserves any internal newlines.
pub fn parse_revision_trigger(comment_body: &str, bot_username: &str) -> Option<String> {
    let trimmed = comment_body.trim_start();
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
    let open_prs =
        github::list_open_prs_for_head(api_base, &token, &owner, &repo_name, &repo.agent_branch)
            .await
            .with_context(|| {
                format!(
                    "listing open PRs for {owner}/{repo_name} head {}",
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
        // Bot-authored comments are filtered out before parsing.
        if comment.user_login().eq_ignore_ascii_case(bot_username) {
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
        // target per PR.
        let change_name = extract_change_list_from_pr_body(pr.body.as_deref())
            .into_iter()
            .next()
            .unwrap_or_else(|| repo.agent_branch.clone());
        let outcome =
            execute_revision(workspace, repo, executor, &change_name, &revision_text).await;
        match outcome {
            Ok(ExecutorOutcome::Completed) => {
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
async fn execute_revision(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    change_name: &str,
    revision_text: &str,
) -> Result<ExecutorOutcome> {
    let pr_diff = crate::git::diff_three_dot(workspace, &repo.base_branch, &repo.agent_branch)
        .unwrap_or_default();
    let ctx = RevisionContext {
        change_name: change_name.to_string(),
        pr_diff,
        revision_text: revision_text.to_string(),
    };
    executor.run_revision(workspace, change_name, &ctx).await
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
        let dir = tmp.path().join(REVISIONS_DIR);
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
        let dir = tmp.path().join(REVISIONS_DIR);
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
        let dir = tmp.path().join(REVISIONS_DIR);
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
            Ok(ExecutorOutcome::Completed)
        }
        async fn resume(
            &self,
            _handle: ResumeHandle,
            _answer: &str,
        ) -> Result<ExecutorOutcome> {
            Ok(ExecutorOutcome::Completed)
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
                ExecutorOutcome::Completed
            } else {
                guard.remove(0)
            };
            // Simulate the executor writing a file so the `git add -A`
            // path in the dispatcher's Completed branch has something to
            // commit.
            if matches!(outcome, ExecutorOutcome::Completed) {
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
        let executor = StubExecutor::new(vec![ExecutorOutcome::Completed]);
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
}
