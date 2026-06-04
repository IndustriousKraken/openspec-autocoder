//! Polling-iteration handler for chat-driven changelog requests.
//!
//! The chatops `changelog` verb writes a `ChangelogRequestState` to disk
//! AND pushes a `ChangelogRequest` onto the per-repo queue. The polling
//! loop drains the queue once per iteration and calls
//! `process_changelog_requests`. For each request the handler:
//!
//! 1. Runs the deterministic `a05` extractor against the workspace's
//!    archive (no subprocess — the extractor's data-producing helpers
//!    are called directly).
//! 2. Builds the stylist prompt from the JSON output AND invokes the
//!    executor's `run_changelog` method.
//! 3. Validates the resulting diff's path scope: must touch only
//!    `CHANGELOG.md` AND/OR `openspec/changes/archive/<slug>/proposal.md`
//!    paths. Reject otherwise.
//! 4. Commits the diff to a `changelog-<short-hash>` branch, pushes,
//!    AND opens a single PR.
//! 5. Posts a threaded reply in the lifecycle thread naming the PR URL.

use anyhow::{Context, Result, anyhow};
use std::path::Path;

use crate::changelog_requests::{
    self, ChangelogRequestState, ChangelogStatus,
};
use crate::chatops::operator_commands::{ParsedChangelogArgs, parse_changelog_args};
use crate::cli::changelog::{
    self as cli_changelog, ArchiveEntry, ArchiveMetadataRaw, SkippedEntry, render_json,
    resolve_tag_range,
};
use crate::config::{GithubConfig, RepositoryConfig};
use crate::executor::{ChangelogContext, Executor, ExecutorOutcome};
use crate::{git, github};

/// Per-request branch-name prefix. The full branch name appends a short
/// hash of the request_id so concurrent runs cannot collide.
const CHANGELOG_BRANCH_PREFIX: &str = "changelog-";

/// Path-scope validation: a diff entry is accepted iff it touches
/// `CHANGELOG.md` (at the workspace root) OR
/// `openspec/changes/archive/<slug>/proposal.md` (any depth, any slug).
fn is_in_scope(path: &str) -> bool {
    if path == "CHANGELOG.md" {
        return true;
    }
    if let Some(rest) = path.strip_prefix("openspec/changes/archive/")
        && let Some(idx) = rest.find('/')
    {
        let after = &rest[idx + 1..];
        if after == "proposal.md" {
            return true;
        }
    }
    false
}

/// Run the deterministic `a05` extractor against the workspace's
/// archive AND return the rendered JSON payload. Calls the extractor's
/// data-producing helpers directly (no subprocess).
fn extract_changelog_json(workspace: &Path, args: &ParsedChangelogArgs) -> Result<String> {
    let mut stderr_buf: Vec<u8> = Vec::new();
    let range = resolve_tag_range(
        workspace,
        args.since.as_deref(),
        args.to.as_deref().unwrap_or("HEAD"),
        &mut stderr_buf,
    )
    .with_context(|| "changelog-stylist: resolving tag range".to_string())?;
    let discovered = cli_changelog::find_archives_in_range(workspace, &range)
        .with_context(|| "changelog-stylist: discovering archives".to_string())?;

    let mut entries: Vec<ArchiveEntry> = Vec::new();
    let mut skipped: Vec<SkippedEntry> = Vec::new();
    for raw in discovered {
        let slug = raw.slug.clone();
        let metadata = match cli_changelog::read_archive_metadata(
            workspace,
            &raw.archive_dir,
            &mut stderr_buf,
        ) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    "changelog-stylist: skipping `{slug}`: failed to read proposal.md: {e:#}"
                );
                continue;
            }
        };
        match metadata {
            ArchiveMetadataRaw::Entry { summary } => entries.push(ArchiveEntry {
                summary,
                ..raw
            }),
            ArchiveMetadataRaw::Skip { reason } => {
                skipped.push(SkippedEntry { slug, reason })
            }
        }
    }
    let version = range.to_label.clone();
    render_json(&version, &range, &entries, &skipped)
        .map_err(|e| anyhow!("rendering changelog JSON: {e}"))
}

/// Drain handler for chat-driven changelog requests. The polling loop's
/// `run` calls this once per iteration with the per-iteration drained
/// queue snapshot. Each entry loads its `ChangelogRequestState`, runs
/// the deterministic extractor, invokes the stylist via the executor,
/// validates the diff's path scope, commits + pushes to a
/// `changelog-<short-hash>` branch, AND opens a single PR.
pub async fn process_changelog_requests(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&crate::polling_loop::ChatOpsContext>,
    requests: &[crate::control_socket::ChangelogRequest],
) -> Result<()> {
    let fork_url = match github_cfg.fork_owner.as_deref() {
        Some(owner) => Some(crate::github::derive_fork_url(&repo.url, owner)?),
        None => None,
    };
    let fork_arg = fork_url.as_deref().map(|u| (u, repo.agent_branch.as_str()));
    crate::workspace::ensure_initialized(paths, workspace, &repo.url, fork_arg)
        .with_context(|| "changelog-stylist: workspace ensure_initialized".to_string())?;
    let _ = crate::queue::clear_stale_locks(workspace);
    let _ = git::reset_hard_head(workspace);
    let _ = git::clean_force(workspace);
    git::fetch(workspace).with_context(|| "changelog-stylist: git fetch".to_string())?;
    git::checkout(workspace, &repo.base_branch)
        .with_context(|| format!("changelog-stylist: checkout `{}`", repo.base_branch))?;
    git::pull_ff_only(workspace, &repo.base_branch).with_context(|| {
        format!("changelog-stylist: pull --ff-only `{}`", repo.base_branch)
    })?;

    let state_root = changelog_requests::default_state_root(paths);
    for request in requests {
        let mut state = match changelog_requests::read_state(
            &state_root,
            &repo.url,
            &request.request_id,
        ) {
            Ok(Some(s)) => s,
            Ok(None) => {
                tracing::warn!(
                    request_id = %request.request_id,
                    "changelog-stylist: no state file (entry pruned between enqueue and processing); skipping"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request.request_id,
                    "changelog-stylist: state read failed: {e:#}"
                );
                continue;
            }
        };

        state.status = ChangelogStatus::InFlight;
        let _ = changelog_requests::write_state(&state_root, &state);

        if let Err(e) = process_one_request(
            workspace, repo, executor, github_cfg, chatops_ctx, &state_root, &mut state,
        )
        .await
        {
            tracing::error!(
                url = %repo.url,
                request_id = %state.request_id,
                "changelog-stylist: processing failed: {e:#}"
            );
            mark_failed(&state_root, &mut state, format!("{e:#}"), chatops_ctx).await;
        }

        if let Err(e) = git::reset_hard_head(workspace) {
            tracing::warn!(
                url = %repo.url,
                "changelog-stylist: post-run reset_hard_head failed: {e:#}"
            );
        }
        let _ = git::clean_force(workspace);
        let _ = git::checkout(workspace, &repo.base_branch);
    }
    Ok(())
}

async fn process_one_request(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&crate::polling_loop::ChatOpsContext>,
    state_root: &Path,
    state: &mut ChangelogRequestState,
) -> Result<()> {
    let parsed = parse_changelog_args(&state.raw_args)
        .map_err(|e| anyhow!("parsing changelog args `{}`: {e}", state.raw_args))?;
    if parsed.workspace_override.is_some() {
        return Err(anyhow!(
            "refusing changelog: --workspace override arrived via chatops"
        ));
    }
    let changelog_json = extract_changelog_json(workspace, &parsed)?;
    let ctx = ChangelogContext {
        changelog_json,
        repo_url: state.repo_url.clone(),
        revision_text: String::new(),
    };
    tracing::info!(
        url = %repo.url,
        request_id = %state.request_id,
        "changelog-stylist: invoking executor"
    );
    let outcome = executor.run_changelog(workspace, &ctx).await?;
    match outcome {
        ExecutorOutcome::Completed { .. } => {
            commit_and_open_pr(workspace, repo, github_cfg, chatops_ctx, state_root, state).await
        }
        ExecutorOutcome::Failed { reason } => Err(anyhow!("executor failed: {reason}")),
        ExecutorOutcome::AskUser { .. } => Err(anyhow!(
            "executor returned AskUser; changelog flow does not support clarification"
        )),
        ExecutorOutcome::SpecNeedsRevision { .. } => Err(anyhow!(
            "executor flagged SpecNeedsRevision during changelog run"
        )),
        ExecutorOutcome::IterationRequested { .. } => Err(anyhow!(
            "executor returned IterationRequested during changelog run (iteration sequences not applicable)"
        )),
        ExecutorOutcome::Aborted { reason } => {
            // a39: subprocess killed by the daemon's own SIGTERM
            // cascade. Return Ok(()) so the changelog request is not
            // marked as a failure; the next iteration after restart
            // will retry from a clean state.
            tracing::info!(
                url = %repo.url,
                request_id = %state.request_id,
                "changelog-stylist: executor aborted by daemon shutdown: {reason}"
            );
            Ok(())
        }
    }
}

async fn commit_and_open_pr(
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&crate::polling_loop::ChatOpsContext>,
    state_root: &Path,
    state: &mut ChangelogRequestState,
) -> Result<()> {
    // Read the workspace's git status to discover what the stylist
    // changed.
    let porcelain = git::status_porcelain(workspace)
        .with_context(|| "changelog-stylist: reading post-Completed git status".to_string())?;
    let changed: Vec<String> = porcelain
        .lines()
        .filter_map(extract_porcelain_path)
        .filter(|p| !p.is_empty())
        .collect();

    if changed.is_empty() {
        if let Some(ctx) = chatops_ctx {
            let body = format!(
                "ℹ️ Changelog run for `{repo_url}` completed with no changes.",
                repo_url = state.repo_url,
            );
            let _ = ctx
                .chatops
                .post_threaded_reply(&state.channel, &state.lifecycle_thread_ts, &body)
                .await;
        }
        state.status = ChangelogStatus::Acted;
        let _ = changelog_requests::write_state(state_root, state);
        return Ok(());
    }

    // Path-scope validation. Out-of-scope diffs are refused; the
    // workspace is reset clean below.
    let out_of_scope: Vec<String> = changed
        .iter()
        .filter(|p| !is_in_scope(p))
        .cloned()
        .collect();
    if !out_of_scope.is_empty() {
        let log_pointer = format!(
            "journalctl -u autocoder | grep request_id={}",
            state.request_id
        );
        let body = format!(
            "✗ changelog: LLM produced out-of-scope diff; refusing to commit. See {log_pointer}."
        );
        if let Some(ctx) = chatops_ctx {
            let _ = ctx
                .chatops
                .post_threaded_reply(&state.channel, &state.lifecycle_thread_ts, &body)
                .await;
        }
        tracing::warn!(
            request_id = %state.request_id,
            out_of_scope = ?out_of_scope,
            "changelog-stylist: rejecting out-of-scope diff"
        );
        state.status = ChangelogStatus::Failed;
        state.reason = Some(format!("out-of-scope diff: {out_of_scope:?}"));
        let _ = changelog_requests::write_state(state_root, state);
        return Ok(());
    }

    // Build the branch name from a short hash of the request_id. Stable
    // per request, unique across concurrent runs.
    let short_hash = short_id_hash(&state.request_id);
    let branch = format!("{CHANGELOG_BRANCH_PREFIX}{short_hash}");

    git::recreate_branch(workspace, &branch)
        .with_context(|| format!("changelog-stylist: recreate `{branch}`"))?;
    for p in &changed {
        let _ = std::process::Command::new("git")
            .args(["add", "--", p])
            .current_dir(workspace)
            .status();
    }
    let subject = format!("changelog: stylist draft (request {})", state.request_id);
    git::commit(workspace, &subject)
        .with_context(|| "changelog-stylist: commit changelog branch".to_string())?;
    let push_remote = if github_cfg.fork_owner.is_some() {
        "fork"
    } else {
        "origin"
    };
    git::push_force_with_lease(workspace, &branch, push_remote)
        .with_context(|| "changelog-stylist: pushing changelog branch".to_string())?;

    let pr_title = format!("changelog: stylist draft ({short_hash})");
    let pr_body = format!(
        "This PR carries the LLM-styled CHANGELOG.md draft for `{repo_url}` (request `{request_id}`).\n\n\
         Reviewers: read the diff on GitHub. To iterate, post `@<bot> revise <instruction>` on this PR and the stylist will re-run with your instruction applied.",
        repo_url = state.repo_url,
        request_id = state.request_id,
    );
    let pr_url = open_changelog_pull_request(
        repo,
        github_cfg,
        &branch,
        &repo.base_branch,
        &pr_title,
        &pr_body,
    )
    .await
    .with_context(|| "changelog-stylist: opening PR".to_string())?;

    if let Some(ctx) = chatops_ctx {
        let body = format!(
            "✓ Changelog draft ready at {pr_url}. Review on GitHub; revise via @<bot> revise <text>."
        );
        let _ = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.lifecycle_thread_ts, &body)
            .await;
    }

    state.status = ChangelogStatus::Acted;
    let _ = changelog_requests::write_state(state_root, state);
    Ok(())
}

async fn mark_failed(
    state_root: &Path,
    state: &mut ChangelogRequestState,
    reason: String,
    chatops_ctx: Option<&crate::polling_loop::ChatOpsContext>,
) {
    state.status = ChangelogStatus::Failed;
    state.reason = Some(reason.clone());
    if let Err(e) = changelog_requests::write_state(state_root, state) {
        tracing::warn!(
            request_id = %state.request_id,
            "changelog-stylist: recording Failed state failed: {e:#}"
        );
    }
    if let Some(ctx) = chatops_ctx {
        let body = format!(
            "✗ Changelog run for `{repo_url}` failed: {reason}",
            repo_url = state.repo_url,
        );
        let _ = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.lifecycle_thread_ts, &body)
            .await;
    }
}

/// Helper: extract the post-status path from a `git status --porcelain`
/// line. Mirrors the existing helper in `polling_loop` but kept local so
/// `changelog_triage` is self-contained.
fn extract_porcelain_path(line: &str) -> Option<String> {
    // Porcelain v1 lines: `XY <path>` where XY is two status chars.
    // Renames look like `R  old -> new`; we return the new path.
    if line.len() < 4 {
        return None;
    }
    let body = &line[3..];
    if let Some(idx) = body.find(" -> ") {
        Some(body[idx + 4..].to_string())
    } else {
        Some(body.to_string())
    }
}

/// 8-char hex hash of `request_id`. Stable + URL-safe.
fn short_id_hash(request_id: &str) -> String {
    let mut state: u64 = 0xcbf29ce484222325;
    for b in request_id.as_bytes() {
        state ^= *b as u64;
        state = state.wrapping_mul(0x100000001b3);
    }
    format!("{state:016x}")[..8].to_string()
}

/// Walk every open PR in the repo whose head branch starts with
/// `changelog-` AND drive the PR-comment revision loop against it.
/// Mirrors the shape of `revisions::process_revision_requests` but is
/// purpose-built for the changelog flow: on a revision trigger, the
/// stylist re-runs with the operator's revision text injected, the
/// diff scope is validated, AND the new commit is force-pushed to the
/// PR's existing branch (no PR close/re-open).
pub async fn process_changelog_revision_requests(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&crate::polling_loop::ChatOpsContext>,
) -> Result<()> {
    let (owner, repo_name) = github::parse_repo_url(&repo.url)?;
    let token = crate::github_credentials::resolve_token(github_cfg, &owner)?;
    let bot_username = github::self_bot_username(github::DEFAULT_API_BASE, &token)
        .await
        .with_context(|| "changelog-revision: resolving bot username")?;
    let open_prs = github::list_open_prs_all(
        github::DEFAULT_API_BASE,
        &token,
        &owner,
        &repo_name,
    )
    .await
    .with_context(|| {
        format!("changelog-revision: listing open PRs for {owner}/{repo_name}")
    })?;
    let changelog_prs: Vec<&github::PrSummary> = open_prs
        .iter()
        .filter(|p| p.head.ref_.starts_with(CHANGELOG_BRANCH_PREFIX))
        .collect();
    if changelog_prs.is_empty() {
        return Ok(());
    }
    for pr in &changelog_prs {
        if let Err(e) = process_one_changelog_pr_revision(
            paths,
            workspace,
            repo,
            github_cfg,
            executor,
            chatops_ctx,
            pr,
            &owner,
            &repo_name,
            &token,
            &bot_username,
        )
        .await
        {
            tracing::warn!(
                url = %repo.url,
                pr_number = pr.number,
                "changelog-revision processing for PR failed (iteration continues): {e:#}"
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_one_changelog_pr_revision(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&crate::polling_loop::ChatOpsContext>,
    pr: &github::PrSummary,
    owner: &str,
    repo_name: &str,
    token: &str,
    bot_username: &str,
) -> Result<()> {
    // Per-PR state lives under the existing `revisions/` directory so
    // both flows share the same prune-on-close machinery.
    let mut state = match crate::revisions::read_state(paths, workspace, pr.number)? {
        Some(s) => s,
        None => crate::revisions::RevisionState {
            pr_number: pr.number,
            agent_branch: pr.head.ref_.clone(),
            last_seen_comment_at: pr.created_at,
            auto_revisions_applied: 0,
            revision_cap: u32::MAX,
            cap_decline_posted: false,
            code_reviews_applied: 0,
            code_review_cap: Some(5),
            cap_decline_posted_for_code_review: false,
            last_suggested_rereview_at_revisions_count: None,
            original_review_head_sha: None,
        },
    };
    let comments = github::list_issue_comments_since(
        github::DEFAULT_API_BASE,
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
    let mut latest_seen: Option<chrono::DateTime<chrono::Utc>> = None;
    for comment in comments {
        if comment.user_login().eq_ignore_ascii_case(bot_username)
            && !comment
                .body
                .trim_start()
                .starts_with(crate::revisions::REVIEWER_REVISION_MARKER)
        {
            advance_seen(&mut latest_seen, comment.created_at);
            continue;
        }
        let revision_text = match crate::revisions::parse_revision_trigger(&comment.body, bot_username)
        {
            Some(t) => t,
            None => {
                advance_seen(&mut latest_seen, comment.created_at);
                continue;
            }
        };
        // Re-run the stylist with the revision text injected; force-push
        // to the existing changelog branch.
        if let Err(e) = re_run_stylist_and_force_push(
            workspace,
            repo,
            github_cfg,
            executor,
            chatops_ctx,
            &pr.head.ref_,
            &revision_text,
            pr.number,
            owner,
            repo_name,
            token,
        )
        .await
        {
            tracing::warn!(
                url = %repo.url,
                pr_number = pr.number,
                "changelog-revision re-run failed: {e:#}"
            );
            let body = format!(
                "✗ Changelog revision failed: {e}. The PR is unchanged."
            );
            let _ = github::post_issue_comment(
                github::DEFAULT_API_BASE,
                token,
                owner,
                repo_name,
                pr.number,
                &body,
            )
            .await;
        } else {
            state.auto_revisions_applied = state.auto_revisions_applied.saturating_add(1);
            let body = format!(
                "✅ Changelog revision applied. Total revisions on this PR: {}.",
                state.auto_revisions_applied
            );
            let _ = github::post_issue_comment(
                github::DEFAULT_API_BASE,
                token,
                owner,
                repo_name,
                pr.number,
                &body,
            )
            .await;
        }
        advance_seen(&mut latest_seen, comment.created_at);
        crate::revisions::write_state(paths, workspace, &state)?;
    }
    if let Some(t) = latest_seen
        && t > state.last_seen_comment_at
    {
        state.last_seen_comment_at = t;
        crate::revisions::write_state(paths, workspace, &state)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn re_run_stylist_and_force_push(
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    executor: &dyn Executor,
    _chatops_ctx: Option<&crate::polling_loop::ChatOpsContext>,
    branch: &str,
    revision_text: &str,
    _pr_number: u64,
    _owner: &str,
    _repo_name: &str,
    _token: &str,
) -> Result<()> {
    git::fetch(workspace).with_context(|| "changelog-revision: git fetch")?;
    git::checkout(workspace, &repo.base_branch)
        .with_context(|| format!("changelog-revision: checkout `{}`", repo.base_branch))?;
    git::pull_ff_only(workspace, &repo.base_branch).with_context(|| {
        format!("changelog-revision: pull --ff-only `{}`", repo.base_branch)
    })?;
    let parsed = ParsedChangelogArgs::default();
    let changelog_json = extract_changelog_json(workspace, &parsed)?;
    let ctx = ChangelogContext {
        changelog_json,
        repo_url: repo.url.clone(),
        revision_text: revision_text.to_string(),
    };
    let outcome = executor.run_changelog(workspace, &ctx).await?;
    match outcome {
        ExecutorOutcome::Completed { .. } => {}
        ExecutorOutcome::Failed { reason } => {
            return Err(anyhow!("executor failed: {reason}"));
        }
        ExecutorOutcome::AskUser { .. } => {
            return Err(anyhow!("executor returned AskUser; not supported here"));
        }
        ExecutorOutcome::SpecNeedsRevision { .. } => {
            return Err(anyhow!("executor returned SpecNeedsRevision; not supported here"));
        }
        ExecutorOutcome::IterationRequested { .. } => {
            return Err(anyhow!(
                "executor returned IterationRequested; not supported here"
            ));
        }
        ExecutorOutcome::Aborted { reason } => {
            // a39: subprocess killed by the daemon's own SIGTERM
            // cascade. Return Ok(()) — the revise loop will retry on
            // the next iteration after restart.
            tracing::info!(
                url = %repo.url,
                "changelog-revision: executor aborted by daemon shutdown: {reason}"
            );
            return Ok(());
        }
    }
    let porcelain = git::status_porcelain(workspace)
        .with_context(|| "changelog-revision: post-Completed git status")?;
    let changed: Vec<String> = porcelain
        .lines()
        .filter_map(extract_porcelain_path)
        .filter(|p| !p.is_empty())
        .collect();
    let out_of_scope: Vec<String> = changed
        .iter()
        .filter(|p| !is_in_scope(p))
        .cloned()
        .collect();
    if !out_of_scope.is_empty() {
        let _ = git::reset_hard_head(workspace);
        let _ = git::clean_force(workspace);
        return Err(anyhow!(
            "out-of-scope diff: {out_of_scope:?}; refusing to commit"
        ));
    }
    if changed.is_empty() {
        return Err(anyhow!("revision produced no diff"));
    }
    // Force-recreate the changelog branch from base AND commit the new
    // stylist output. This preserves the branch name (so the PR's head
    // does not change) but rewrites the single commit on it.
    git::recreate_branch(workspace, branch)
        .with_context(|| format!("changelog-revision: recreate `{branch}`"))?;
    for p in &changed {
        let _ = std::process::Command::new("git")
            .args(["add", "--", p])
            .current_dir(workspace)
            .status();
    }
    git::commit(workspace, "changelog: stylist revision")
        .with_context(|| "changelog-revision: commit")?;
    let push_remote = if github_cfg.fork_owner.is_some() {
        "fork"
    } else {
        "origin"
    };
    git::push_force_with_lease(workspace, branch, push_remote)
        .with_context(|| "changelog-revision: pushing revised branch")?;
    Ok(())
}

fn advance_seen(
    latest: &mut Option<chrono::DateTime<chrono::Utc>>,
    candidate: chrono::DateTime<chrono::Utc>,
) {
    match latest {
        Some(curr) if *curr >= candidate => {}
        _ => *latest = Some(candidate),
    }
}

/// Open the changelog PR. Mirrors `open_triage_pull_request` from
/// `polling_loop` but lives here so the changelog flow can change PR
/// shape independently.
async fn open_changelog_pull_request(
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    head_branch: &str,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let (owner, name) = github::parse_repo_url(&repo.url)
        .with_context(|| "changelog-stylist: parsing repo URL".to_string())?;
    let token = crate::github_credentials::resolve_token(github_cfg, &owner)?;
    let head = if let Some(fork_owner) = github_cfg.fork_owner.as_deref() {
        format!("{fork_owner}:{head_branch}")
    } else {
        head_branch.to_string()
    };
    let pr = github::create_pull_request(
        &owner,
        &name,
        &head,
        base_branch,
        title,
        body,
        &token,
        None,
        false,
    )
    .await?;
    Ok(pr.html_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_scope_accepts_root_changelog_and_proposal_files() {
        assert!(is_in_scope("CHANGELOG.md"));
        assert!(is_in_scope(
            "openspec/changes/archive/2026-05-22-foo/proposal.md"
        ));
        assert!(is_in_scope(
            "openspec/changes/archive/2026-05-22-foo-bar-baz/proposal.md"
        ));
    }

    #[test]
    fn in_scope_rejects_arbitrary_paths() {
        assert!(!is_in_scope("src/foo.rs"));
        assert!(!is_in_scope("README.md"));
        assert!(!is_in_scope("openspec/changes/active/foo/proposal.md"));
        assert!(!is_in_scope(
            "openspec/changes/archive/2026-05-22-foo/tasks.md"
        ));
        assert!(!is_in_scope("CHANGELOG.md.bak"));
    }

    #[test]
    fn short_id_hash_is_deterministic_and_8_chars() {
        let a = short_id_hash("req-1");
        let b = short_id_hash("req-1");
        let c = short_id_hash("req-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 8);
    }

    #[test]
    fn extract_porcelain_path_handles_rename_and_simple() {
        assert_eq!(
            extract_porcelain_path(" M CHANGELOG.md"),
            Some("CHANGELOG.md".to_string())
        );
        assert_eq!(
            extract_porcelain_path("R  old.md -> new.md"),
            Some("new.md".to_string())
        );
    }
}
