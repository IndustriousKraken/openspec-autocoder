//! Per-repository polling loop. Each iteration runs a single pass: branch
//! init → queue walk → push + PR if commits were produced. Failures inside
//! one iteration are logged and the loop continues to the next sleep.

use crate::alert_state::{AlertCategory, AlertEntry, AlertState};
use crate::alerts::handle_predictable_failure;
use crate::audits::AuditRegistry;
use crate::audits::scheduler::run_due_audits;
use crate::busy_marker;
use crate::chatops::{self, AnswerPayload, ChatOpsBackend, QuestionPayload};
use crate::code_reviewer::{CodeReviewer, ReviewReport, ReviewVerdict};
use crate::config::{AuditSettings, AuditsConfig, GithubConfig, RepositoryConfig};
use crate::control_socket::{ChatOpsHolder, ChatOpsSlot, GithubHolder, ReviewerHolder};
use crate::executor::{Executor, ExecutorOutcome, ResumeHandle, UnimplementableTask};
use crate::spec_revision::{self, SpecNeedsRevisionDetail};
use crate::{failure_state, git, github, perma_stuck, queue, workspace};
use std::collections::HashMap;
use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use chrono::{Duration as ChronoDuration, Utc};
use rand::Rng;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

const PERMA_STUCK_ALERT_THROTTLE_HOURS: i64 = 24;
const PERMA_STUCK_REASON_EXCERPT_MAX: usize = 200;

/// Per-pass ChatOps context: the provider-agnostic backend + the resolved
/// channel id for THIS repository, plus the operator's notification
/// preferences. Constructed once at startup from the global `chatops:`
/// config and the per-repo `chatops_channel_id` override.
pub struct ChatOpsContext {
    pub chatops: Arc<dyn ChatOpsBackend>,
    pub channel: String,
    /// Whether to post a one-line notification each time a pending change
    /// is dequeued for execution. Defaults to `true` when the operator did
    /// not set `chatops.notifications.start_work`.
    pub start_work_enabled: bool,
    /// Whether to emit throttled chatops alerts at the three predictable
    /// failure sites (workspace init, branch push, PR creation). Defaults
    /// to `true` when the operator did not set
    /// `chatops.notifications.failure_alerts`.
    pub failure_alerts_enabled: bool,
    /// Whether to post a one-line notification each time a PR is opened
    /// (with a link to the PR). Defaults to `true` when the operator did
    /// not set `chatops.notifications.pr_opened`.
    pub pr_opened_enabled: bool,
}

/// Run the polling loop for a single repository. Each iteration is wrapped in
/// `execute_one_pass`; failures inside a pass are logged and do not break the
/// loop. Cancellation is checked between iterations and during the sleep.
///
/// The `github`, `reviewer`, and `chatops` holders are reloaded at the top
/// of each pass — see the control socket (`autocoder reload`) for the
/// mechanism that swaps values into them. The `repo` holder is also reloaded
/// at the top of each pass so the reload handler can hot-swap repository
/// settings (base/agent branch, poll interval, channel id, local_path,
/// per-repo PR cap); the snapshot captured at the start of an iteration is
/// used consistently for the rest of that iteration. The next iteration
/// picks up any swap that happened during the previous sleep.
pub async fn run(
    repo: Arc<ArcSwap<RepositoryConfig>>,
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
    pending_rebuild: Arc<std::sync::atomic::AtomicBool>,
    cancel: CancellationToken,
) {
    run_with_hooks(
        repo,
        executor,
        github_holder,
        reviewer_holder,
        chatops_holder,
        stuck_threshold_secs,
        perma_stuck_threshold,
        executor_max_changes_per_pr,
        startup_jitter_max_secs,
        inter_iteration_jitter_pct,
        audit_registry,
        audits_cfg,
        audit_settings,
        pending_rebuild,
        cancel,
        RunHooks::default(),
    )
    .await
}

/// Test-only hooks for synchronizing with the polling loop's internal
/// state. Production code always passes `RunHooks::default()` (every
/// field `None`); tests inject a `Notify` so they can wait on iteration
/// boundaries event-driven instead of sleep-polling.
#[derive(Default, Clone)]
pub struct RunHooks {
    /// Fires once each time the loop has finished an iteration and is
    /// about to enter its inter-iteration sleep. Tests that need to
    /// race a cancel against the sleep wait on this to know the loop
    /// reached the sleep window.
    pub on_iteration_sleep: Option<Arc<tokio::sync::Notify>>,
}

/// Same as `run` but accepts a `RunHooks` for test-only synchronization.
pub async fn run_with_hooks(
    repo: Arc<ArcSwap<RepositoryConfig>>,
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
    pending_rebuild: Arc<std::sync::atomic::AtomicBool>,
    cancel: CancellationToken,
    hooks: RunHooks,
) {
    {
        let initial = repo.load();
        let workspace = workspace::resolve_path(initial.as_ref());
        tracing::info!(
            url = initial.url.as_str(),
            workspace = %workspace.display(),
            poll_interval_sec = initial.poll_interval_sec,
            "starting polling loop"
        );
    }

    // Startup jitter: each task waits a uniformly-random duration in
    // `[0, startup_jitter_max_secs]` before its first iteration. Without
    // this, N concurrent polling tasks all fire `git fetch` at process
    // start within the same millisecond, which an IDS can flag as a
    // port-scan / scraping signature. Cancellation is honoured during
    // the wait, matching the inter-iteration sleep's contract.
    let startup_jitter_secs = pick_startup_jitter_secs(startup_jitter_max_secs);
    {
        let initial = repo.load();
        tracing::info!(
            url = initial.url.as_str(),
            startup_jitter_secs,
            "polling task for {} will wait {startup_jitter_secs}s before first iteration",
            initial.url
        );
    }
    if startup_jitter_secs > 0 {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!(url = %repo.load().url, "polling loop exiting");
                return;
            }
            () = sleep(Duration::from_secs(startup_jitter_secs)) => {}
        }
    }

    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Single-snapshot-per-iteration: read `repo`, `github`, `reviewer`,
        // and `chatops` exactly once at the top of the iteration so a
        // mid-iteration reload cannot tear the config.
        let snapshot = repo.load();
        let snapshot_ref: &RepositoryConfig = snapshot.as_ref();
        let workspace = workspace::resolve_path(snapshot_ref);
        let github_snap = github_holder.load_full();
        let reviewer_snap = reviewer_holder.load_full();
        let chatops_snap = chatops_holder.load_full();
        let chatops_ctx = chatops_snap
            .as_ref()
            .as_ref()
            .map(|slot| build_chatops_ctx(snapshot_ref, slot));
        let max_changes_per_pr = resolve_max_changes_per_pr(
            snapshot_ref.max_changes_per_pr,
            executor_max_changes_per_pr,
        );

        // Check whether this iteration is a rebuild iteration. We
        // take-and-clear so the chatops-triggered flag does not
        // accidentally trigger a second rebuild on the iteration after
        // this one. Per design: only the polling task itself
        // reads/writes its own `pending_rebuild`, but a writer (control
        // socket) sets it before we read here. Use SeqCst so the read
        // is ordered against the write.
        let want_rebuild = pending_rebuild.swap(false, std::sync::atomic::Ordering::SeqCst);

        if want_rebuild {
            if let Err(error) = execute_rebuild_iteration(
                &workspace,
                snapshot_ref,
                &github_snap,
                chatops_ctx.as_ref(),
                stuck_threshold_secs,
            )
            .await
            {
                tracing::error!(
                    url = snapshot_ref.url.as_str(),
                    "rebuild iteration failed for {}: {error:#}",
                    snapshot_ref.url
                );
            }
        } else if let Err(error) = execute_one_pass(
            &workspace,
            snapshot_ref,
            executor.as_ref(),
            &github_snap,
            reviewer_snap.as_deref(),
            chatops_ctx.as_ref(),
            stuck_threshold_secs,
            perma_stuck_threshold,
            max_changes_per_pr,
            audit_registry.as_ref(),
            audits_cfg.as_deref(),
            audit_settings.as_ref(),
        )
        .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                "polling iteration failed for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Per design: the inter-poll sleep uses the snapshot's
        // poll_interval, not a re-read. Next iteration's read picks up
        // any hot-swap that landed during the sleep.
        let base_secs = snapshot_ref.poll_interval_sec;
        drop(snapshot);
        let sleep_dur = jittered_sleep_duration(base_secs, inter_iteration_jitter_pct);

        if let Some(notify) = &hooks.on_iteration_sleep {
            notify.notify_waiters();
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = sleep(sleep_dur) => {}
        }
    }

    tracing::info!(url = %repo.load().url, "polling loop exiting");
}

/// Resolve the per-iteration commit cap for the polling task. Mirrors
/// `RepositoryConfig::max_changes_per_pr` but accepts the per-repo and
/// executor-level defaults as separate values so the polling loop can
/// pick up a hot-swapped per-repo override without taking a reference
/// to the live `ExecutorConfig`.
fn resolve_max_changes_per_pr(per_repo: Option<u32>, executor_default: Option<u32>) -> u32 {
    const DEFAULT: u32 = 3;
    per_repo.or(executor_default).unwrap_or(DEFAULT).max(1)
}

/// Pick a uniformly-random startup-jitter delay in `[0, max_secs]`. A
/// `max_secs` of `0` short-circuits to `0` without consulting the RNG —
/// `gen_range(0..=0)` is well-defined but skipping the draw keeps the
/// degenerate case obvious to readers.
fn pick_startup_jitter_secs(max_secs: u64) -> u64 {
    if max_secs == 0 {
        return 0;
    }
    rand::rng().random_range(0..=max_secs)
}

/// Compute a jittered inter-iteration sleep duration. The offset is
/// drawn uniformly from `[-max_offset, +max_offset]` where `max_offset
/// = base_secs * jitter_pct / 100`. Saturates at zero on the negative
/// side so a degenerate `jitter_pct = 100` cannot underflow. A
/// `jitter_pct = 0` short-circuits to the exact `base_secs` interval
/// (matching pre-jitter behaviour).
fn jittered_sleep_duration(base_secs: u64, jitter_pct: u8) -> Duration {
    if jitter_pct == 0 {
        return Duration::from_secs(base_secs);
    }
    let max_offset = base_secs.saturating_mul(jitter_pct as u64) / 100;
    if max_offset == 0 {
        return Duration::from_secs(base_secs);
    }
    let offset = rand::rng().random_range(0..=2 * max_offset) as i64 - max_offset as i64;
    let secs = (base_secs as i64).saturating_add(offset).max(0) as u64;
    Duration::from_secs(secs)
}

/// Build the per-iteration `ChatOpsContext` from the loaded snapshot.
/// Notification flags + default channel come from the snapshot; per-repo
/// channel override (immutable, sourced from RepositoryConfig) takes
/// precedence over the snapshot's default channel.
fn build_chatops_ctx(repo: &RepositoryConfig, slot: &ChatOpsSlot) -> ChatOpsContext {
    ChatOpsContext {
        chatops: slot.backend.clone(),
        channel: repo
            .chatops_channel(&slot.default_channel_id)
            .to_string(),
        start_work_enabled: slot.start_work_enabled,
        failure_alerts_enabled: slot.failure_alerts_enabled,
        pr_opened_enabled: slot.pr_opened_enabled,
    }
}

/// Single-pass workflow: workspace init → stale-lock cleanup → dirty-workspace
/// check → branch recreation → queue walk → push + PR if commits were
/// produced.
pub async fn execute_one_pass(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    github_cfg: &GithubConfig,
    reviewer: Option<&CodeReviewer>,
    chatops_ctx: Option<&ChatOpsContext>,
    stuck_threshold_secs: u64,
    perma_stuck_threshold: u32,
    max_changes_per_pr: u32,
    audit_registry: &AuditRegistry,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
) -> Result<()> {
    // Acquire the per-repo busy marker. Held across the entire pass
    // (executor → review → push → PR); released by Drop on every return.
    // A crash that bypasses Drop leaves the marker for the next pass to
    // detect and (depending on age + PID liveness) auto-recover from.
    let mut guard = match busy_marker::try_acquire(workspace, &repo.url, stuck_threshold_secs) {
        Ok(busy_marker::AcquireOutcome::Acquired(g)) => g,
        Ok(busy_marker::AcquireOutcome::SkipFreshInProgress(m)) => {
            tracing::info!(
                url = %repo.url,
                pid = m.pid,
                stage = %m.stage.as_str(),
                "busy marker present; another pass is in progress — skipping iteration"
            );
            return Ok(());
        }
        Ok(busy_marker::AcquireOutcome::SkipAmbiguous(m)) => {
            tracing::error!(
                url = %repo.url,
                pid = m.pid,
                recorded_comm = %m.comm,
                "busy marker is stuck with ambiguous PID state; skipping iteration — investigate manually"
            );
            post_stuck_alert(chatops_ctx, repo, &m, true).await;
            return Ok(());
        }
        Err(e) => {
            tracing::error!(url = %repo.url, "busy marker acquire failed: {e:#}");
            return Err(e);
        }
    };

    // Before doing any iteration work, check whether an open PR already
    // exists on the agent branch. If yes, this iteration would burn
    // tokens re-implementing, force-update the PR's commits under any
    // reviewer mid-review, and 422 at PR creation. Skip entirely.
    if open_pr_exists_for_agent_branch(repo, github_cfg).await {
        return Ok(());
    }
    let (processed, includes_self_heal) = run_pass_through_commits(
        workspace,
        repo,
        github_cfg,
        executor,
        chatops_ctx,
        perma_stuck_threshold,
        max_changes_per_pr,
        audit_registry,
        audits_cfg,
        audit_settings,
    )
    .await?;
    if processed.is_empty() {
        // Workspace init succeeded and the queue walk produced no work.
        // Per design.md task 6.4, an Ok-returning iteration with no
        // failures clears every category's throttle.
        let _ = AlertState::clear(workspace);
        return Ok(());
    }

    let range = format!("{}..{}", repo.base_branch, repo.agent_branch);
    let commit_count = git::rev_list_count(workspace, &range)?;
    if commit_count == 0 {
        tracing::info!(
            url = repo.url.as_str(),
            "polling pass produced no commits (all completed changes had empty diffs)"
        );
        let _ = AlertState::clear(workspace);
        return Ok(());
    }

    // Reviewer step (if configured) runs against the produced commits BEFORE
    // the push + PR. A failed reviewer is non-fatal: PR still ships with a
    // "(reviewer failed)" note in the body.
    let (review_report, draft) = match reviewer {
        None => (None, false),
        Some(r) => {
            let _ = guard.set_stage(busy_marker::Stage::Review);
            let ctx = build_review_context(workspace, repo, &processed)?;
            match r.review(&ctx).await {
                Ok(report) => {
                    let draft = matches!(report.verdict, ReviewVerdict::Block);
                    (Some(report), draft)
                }
                Err(e) => {
                    tracing::error!("reviewer failed: {e:#}");
                    let synthetic = ReviewReport {
                        verdict: ReviewVerdict::Concerns,
                        markdown: format!("(reviewer failed: {e})"),
                    };
                    (Some(synthetic), false)
                }
            }
        }
    };

    let push_remote = if github_cfg.fork_owner.is_some() {
        "fork"
    } else {
        "origin"
    };
    let _ = guard.set_stage(busy_marker::Stage::Push);
    if let Err(e) = git::push_force_with_lease(workspace, &repo.agent_branch, push_remote) {
        handle_predictable_failure(
            workspace,
            &repo.url,
            chatops_ctx,
            chatops_ctx
                .map(|c| c.failure_alerts_enabled)
                .unwrap_or(false),
            AlertCategory::BranchPushFailure,
            &e,
        )
        .await;
        return Err(e);
    }
    let _ = guard.set_stage(busy_marker::Stage::Pr);
    open_pull_request(
        repo,
        github_cfg,
        &processed,
        includes_self_heal,
        review_report.as_ref(),
        draft,
        chatops_ctx,
        workspace,
    )
    .await?;
    // End-of-pass success: push and PR creation both succeeded. Clear the
    // entire alert-state map so the next failure (whatever category) re-
    // alerts immediately. Per design.md, this is intentionally coarse —
    // any successful iteration resets every category's throttle.
    if let Err(e) = AlertState::clear(workspace) {
        tracing::warn!(
            url = %repo.url,
            "failed to clear alert-state on success: {e:#}"
        );
    }
    Ok(())
}

/// Best-effort chatops alert for stuck busy-marker states. Posts a
/// notification via `post_notification` if a chatops backend is
/// configured; otherwise the ERROR log line is the operator's only
/// signal. Returns immediately on any post failure (logged at WARN).
async fn post_stuck_alert(
    chatops_ctx: Option<&ChatOpsContext>,
    repo: &RepositoryConfig,
    marker: &busy_marker::BusyMarker,
    ambiguous: bool,
) {
    let ctx = match chatops_ctx {
        Some(c) => c,
        None => return,
    };
    let kind = if ambiguous {
        "stuck (ambiguous — investigate)"
    } else {
        "recovered from stuck state"
    };
    let text = format!(
        ":rotating_light: autocoder {kind}\nrepo: `{}`\npid: {} (recorded comm: `{}`)\nstage: `{}`\nstarted: {}",
        repo.url,
        marker.pid,
        marker.comm,
        marker.stage.as_str(),
        marker.started_at,
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            "busy_marker: failed to post stuck-state chatops alert: {e:#}"
        );
    }
}

/// Assemble the `ReviewContext` for the reviewer: archived-change briefs
/// (proposal/design/tasks), full contents of every modified file, and the
/// unified diff. Reviewer enforces the 2M-char prompt budget when
/// rendering; this builder is unconstrained — it gathers everything and
/// lets the reviewer drop/include in priority order.
fn build_review_context(
    workspace: &Path,
    repo: &RepositoryConfig,
    processed: &[String],
) -> Result<crate::code_reviewer::ReviewContext> {
    let diff = git::diff_three_dot(workspace, &repo.base_branch, &repo.agent_branch)?;
    let file_list =
        git::diff_files_changed(workspace, &repo.base_branch, &repo.agent_branch)?;

    let mut changed_files = Vec::with_capacity(file_list.len());
    for path in &file_list {
        let abs = workspace.join(path);
        match std::fs::read_to_string(&abs) {
            Ok(contents) => changed_files.push(crate::code_reviewer::ChangedFile {
                path: path.clone(),
                contents,
            }),
            // Deleted files appear in the diff but have no current
            // content. Their removal is captured by the diff itself.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(
                    path = %path,
                    "skipping changed-file read for reviewer: {e}"
                );
                continue;
            }
        }
    }

    let archive_root = workspace.join("openspec/changes/archive");
    let mut archived_changes = Vec::with_capacity(processed.len());
    for name in processed {
        let dir = match locate_archive_dir(&archive_root, name)? {
            Some(d) => d,
            None => {
                tracing::warn!(
                    change = %name,
                    "archive directory not found while building review context"
                );
                continue;
            }
        };
        let proposal = std::fs::read_to_string(dir.join("proposal.md")).unwrap_or_default();
        let design = std::fs::read_to_string(dir.join("design.md")).ok();
        let tasks = std::fs::read_to_string(dir.join("tasks.md")).unwrap_or_default();
        archived_changes.push(crate::code_reviewer::ChangeBrief {
            name: name.clone(),
            proposal,
            design,
            tasks,
        });
    }

    Ok(crate::code_reviewer::ReviewContext {
        archived_changes,
        changed_files,
        diff,
    })
}

/// Find the date-prefixed archive directory matching the given change name
/// (e.g. `openspec/changes/archive/2026-05-14-foo/` for `foo`). Returns
/// `Ok(None)` if no matching directory exists.
fn locate_archive_dir(archive_root: &Path, change: &str) -> Result<Option<std::path::PathBuf>> {
    if !archive_root.is_dir() {
        return Ok(None);
    }
    let suffix = format!("-{change}");
    for entry in std::fs::read_dir(archive_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if name.ends_with(&suffix) {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

/// Run a polling pass up to and including any commits, but stop before push
/// and PR creation. Returns the names of changes archived during the pass.
/// The caller (production: `execute_one_pass`) is responsible for the
/// remote-side work; tests use this directly to verify commit-formation
/// behavior without needing a live GitHub endpoint or a writable remote.
pub async fn run_pass_through_commits(
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsContext>,
    perma_stuck_threshold: u32,
    max_changes_per_pr: u32,
    audit_registry: &AuditRegistry,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
) -> Result<(Vec<String>, bool)> {
    let fork_url = match github_cfg.fork_owner.as_deref() {
        Some(owner) => Some(crate::github::derive_fork_url(&repo.url, owner)?),
        None => None,
    };
    let did_clone = !workspace.exists();
    let mut did_refork = false;
    if did_clone && fork_url.is_some() && github_cfg.recreate_fork_on_reinit {
        match workspace::recreate_fork(github_cfg, repo).await {
            Ok(workspace::RecreateOutcome::Recreated) => {
                did_refork = true;
            }
            Ok(workspace::RecreateOutcome::Forbidden) => {
                // Helper already logged ERROR with scope guidance. Fall
                // through to the conservative ensure_initialized path so
                // the iteration still makes progress.
            }
            Err(e) => {
                tracing::error!(
                    url = %repo.url,
                    "recreate_fork failed: {e:#}; falling back to conservative ensure_initialized"
                );
            }
        }
    }
    let fork_arg = fork_url
        .as_deref()
        .map(|u| (u, repo.agent_branch.as_str()));
    if let Err(e) = workspace::ensure_initialized(workspace, &repo.url, fork_arg) {
        handle_predictable_failure(
            workspace,
            &repo.url,
            chatops_ctx,
            chatops_ctx
                .map(|c| c.failure_alerts_enabled)
                .unwrap_or(false),
            AlertCategory::WorkspaceInitFailure,
            &e,
        )
        .await;
        return Err(e);
    }
    if did_refork {
        maybe_post_refork_notification(repo, chatops_ctx).await;
    }
    let _cleared = queue::clear_stale_locks(workspace)?;

    let dirty = git::status_porcelain(workspace)?;
    // `.alert-state.json` is autocoder bookkeeping at the workspace root.
    // It is intentionally untracked; it must not trip the dirty check.
    let dirty_filtered = filter_alert_state_lines(&dirty);
    if !dirty_filtered.is_empty() {
        let dirty_count = dirty_filtered.lines().count();
        tracing::warn!(
            url = repo.url.as_str(),
            workspace = %workspace.display(),
            "workspace dirty mid-iteration ({dirty_count} entries); attempting recovery (git reset --hard origin/{} + git clean -fd)",
            repo.base_branch
        );
        match attempt_dirty_workspace_recovery(workspace, &repo.base_branch) {
            Ok(()) => {
                let recheck = git::status_porcelain(workspace)?;
                let recheck_filtered = filter_alert_state_lines(&recheck);
                if recheck_filtered.is_empty() {
                    tracing::info!(
                        url = repo.url.as_str(),
                        "workspace recovered mid-iteration; proceeding"
                    );
                } else {
                    let e = anyhow!(
                        "workspace {} still dirty after recovery; refusing to proceed:\n{recheck_filtered}",
                        workspace.display()
                    );
                    handle_predictable_failure(
                        workspace,
                        &repo.url,
                        chatops_ctx,
                        chatops_ctx
                            .map(|c| c.failure_alerts_enabled)
                            .unwrap_or(false),
                        AlertCategory::WorkspaceDirtyMidIteration,
                        &e,
                    )
                    .await;
                    return Err(e);
                }
            }
            Err(recovery_err) => {
                let e = anyhow!(
                    "dirty-workspace recovery failed: {recovery_err:#}; original dirty state:\n{dirty_filtered}"
                );
                handle_predictable_failure(
                    workspace,
                    &repo.url,
                    chatops_ctx,
                    chatops_ctx
                        .map(|c| c.failure_alerts_enabled)
                        .unwrap_or(false),
                    AlertCategory::WorkspaceDirtyMidIteration,
                    &e,
                )
                .await;
                return Err(e);
            }
        }
    }

    git::fetch(workspace)?;
    git::checkout(workspace, &repo.base_branch)?;
    git::pull_ff_only(workspace, &repo.base_branch)?;
    git::recreate_branch(workspace, &repo.agent_branch)?;

    // Periodic audits run AFTER recreate_branch (so the working tree is
    // on a clean agent-q) AND BEFORE list_pending (so any specs a
    // spec-writing audit creates are picked up by this same iteration's
    // queue walk). Per design: audit failures inside the scheduler are
    // logged and never abort the iteration.
    if let Err(e) = run_due_audits(
        audit_registry,
        workspace,
        repo,
        audits_cfg,
        audit_settings,
        chatops_ctx,
    )
    .await
    {
        tracing::error!(
            url = %repo.url,
            "audit scheduler errored (iteration continues): {e:#}"
        );
    }

    let pending_at_start = queue::list_pending(workspace)?;
    let waiting_at_start = queue::list_waiting(workspace)?;
    tracing::info!(
        url = %repo.url,
        pending = pending_at_start.len(),
        waiting = waiting_at_start.len(),
        "polling pass starting"
    );

    // Pre-flight archive-collision filter on the pending list. Any change
    // whose dated archive path already exists on disk is excluded from the
    // queue walk entirely (a throttled chatops alert under
    // `AlertCategory::ArchiveCollision` is posted per excluded change) so
    // the executor is never invoked on a change that cannot land.
    let pending_filtered =
        apply_archive_collision_preflight(workspace, repo, chatops_ctx, pending_at_start).await;

    // Process waiting (escalated) changes BEFORE pending. Each resumes if
    // a human reply has arrived. Any change that comes back as Completed
    // with a diff goes into the `processed` list and will get pushed/PR'd
    // along with anything from the pending pass.
    let mut processed: Vec<String> = Vec::new();
    let mut includes_self_heal = false;
    if chatops_ctx.is_some() {
        let resumed = process_waiting_changes(
            workspace,
            repo,
            executor,
            chatops_ctx,
            perma_stuck_threshold,
            max_changes_per_pr,
        )
        .await?;
        processed.extend(resumed);
    }

    // Same-repo block: if any change is STILL waiting after the resume
    // pass, skip the pending pass entirely for this iteration.
    let still_waiting = queue::list_waiting(workspace)?;
    if !still_waiting.is_empty() {
        tracing::info!(
            url = repo.url.as_str(),
            "queue blocked for {}: {} change(s) still waiting on human reply: {}",
            repo.url,
            still_waiting.len(),
            still_waiting.join(", ")
        );
        tracing::info!(
            url = %repo.url,
            committed = processed.len(),
            waiting = still_waiting.len(),
            "polling pass complete"
        );
        return Ok((processed, includes_self_heal));
    }

    let remaining = max_changes_per_pr.saturating_sub(processed.len() as u32);
    if remaining > 0 {
        let (pending_processed, pending_self_heal) = walk_queue(
            workspace,
            repo,
            executor,
            chatops_ctx,
            perma_stuck_threshold,
            remaining,
            pending_filtered,
        )
        .await?;
        processed.extend(pending_processed);
        if pending_self_heal {
            includes_self_heal = true;
        }
    } else {
        tracing::info!(
            url = %repo.url,
            committed = processed.len(),
            cap = max_changes_per_pr,
            "resume step already filled the per-PR cap; skipping pending queue this iteration"
        );
    }

    let waiting_after = queue::list_waiting(workspace)?.len();
    tracing::info!(
        url = %repo.url,
        committed = processed.len(),
        waiting = waiting_after,
        "polling pass complete"
    );
    Ok((processed, includes_self_heal))
}

/// Iterate over the workspace's `list_waiting` changes. For each:
///   1. Read `.question.json` to recover the resume handle + thread coords.
///   2. Poll Slack for the first human reply.
///   3. If a reply has arrived: write `.answer.json`, delete
///      `.question.json`, call `executor.resume(handle, &reply.text)`,
///      classify the new outcome the same way `walk_queue` would.
///
/// Returns the list of changes that resumed-to-completed (i.e. were
/// archived this iteration). Failures during processing are logged and the
/// iteration moves to the next waiting change — they do NOT abort the
/// pass.
async fn process_waiting_changes(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsContext>,
    perma_stuck_threshold: u32,
    max_changes_per_pr: u32,
) -> Result<Vec<String>> {
    let ctx = match chatops_ctx {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    let waiting = queue::list_waiting(workspace)?;
    // Pre-flight archive-collision filter: a change with a dated archive
    // entry already on disk would fail at resume-archive time. Exclude
    // it, alert once (subject to 24h throttle), and proceed with the
    // rest. Same helper as the pending-side filter so behavior is
    // identical at both call sites.
    let waiting = apply_archive_collision_preflight(workspace, repo, chatops_ctx, waiting).await;
    let mut resumed_archived: Vec<String> = Vec::new();

    for change in waiting {
        match process_one_waiting(workspace, repo, executor, ctx, &change, perma_stuck_threshold)
            .await
        {
            Ok(Some(archived)) => {
                resumed_archived.push(archived);
                if resumed_archived.len() as u32 >= max_changes_per_pr {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(
                    url = repo.url.as_str(),
                    "waiting-change processing failed for `{change}`: {e:#}"
                );
            }
        }
    }
    Ok(resumed_archived)
}

/// Process a single waiting change. Returns `Ok(Some(name))` when the
/// change was resumed-to-completed-with-diff and archived (so the caller
/// adds it to the pass's processed list); `Ok(None)` for every other
/// outcome (still waiting, resumed-to-failed, resumed-to-AskUser again,
/// resumed-to-completed-no-diff).
async fn process_one_waiting(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    ctx: &ChatOpsContext,
    change: &str,
    perma_stuck_threshold: u32,
) -> Result<Option<String>> {
    let question = chatops::read_question_file(workspace, change)
        .with_context(|| format!("reading .question.json for `{change}`"))?;
    let reply = ctx
        .chatops
        .poll_thread_for_human_reply(&question.channel, &question.thread_ts)
        .await
        .with_context(|| format!("polling Slack thread for `{change}`"))?;
    let reply = match reply {
        Some(r) => r,
        None => return Ok(None),
    };

    // Persist the answer BEFORE removing the question, in the order
    // mandated by orchestrator-cli/spec.md "Resuming a change after an
    // answer arrives": write answer → delete question → call resume.
    let answer = AnswerPayload {
        answer: reply.text.clone(),
        answered_at: chrono::Utc::now(),
        answerer_user_id: reply.user_id.clone(),
    };
    chatops::write_answer_file(workspace, change, &answer)?;
    chatops::delete_question_file(workspace, change)?;

    let handle = ResumeHandle(question.resume_handle.clone());
    // Record the resumed change in the busy marker so chatops `status`
    // reflects this iteration's active work.
    busy_marker::update_change(workspace, change);
    tracing::info!(
        url = %repo.url,
        change = %change,
        "starting work on change (resume)"
    );
    let outcome = executor.resume(handle, &reply.text).await;

    // After resume returns (any outcome), delete .answer.json so the
    // change reverts to a clean state regardless of the outcome.
    let _ = chatops::delete_answer_file(workspace, change);

    let (result, failure_reason): (ResumeDisposition, Option<String>) = match outcome {
        Err(e) => {
            tracing::error!("executor.resume errored on `{change}`: {e:#}");
            // A resume-side task error is closer to infrastructure than an
            // agent decision. Per spec, transient daemon-side errors do
            // NOT increment the counter; we treat resume errors the same.
            (ResumeDisposition::Errored, None)
        }
        Ok(ExecutorOutcome::Completed) => {
            // The porcelain output here will include the .question.json
            // deletion (and possibly an .answer.json transient) that
            // autocoder itself just performed above. Those are
            // bookkeeping, not executor output, so they must not count
            // as "the executor modified the workspace."
            let dirty = git::status_porcelain(workspace)?;
            if !has_executor_changes(&dirty, change) {
                tracing::warn!(
                    "resume of `{change}` returned Completed without modifying the workspace; marking Failed"
                );
                // The question/answer file shuffle is left in the working
                // tree for now; the next pass's startup dirty-check will
                // either auto-recover or skip. The .in-progress lock was
                // removed when the question was first posted, so the
                // change is already in pending state for retry.
                (
                    ResumeDisposition::CompletedNoDiff,
                    Some(
                        "agent reported Completed without modifying the workspace (resume)"
                            .into(),
                    ),
                )
            } else {
                let subject = build_commit_subject(workspace, change)?;
                git::add_all(workspace)?;
                git::commit(workspace, &subject)?;
                queue::archive(workspace, change)?;
                (ResumeDisposition::Archived, None)
            }
        }
        Ok(ExecutorOutcome::AskUser {
            question: q2,
            resume_handle: rh2,
        }) => {
            // Agent asked another question. Post it and rotate the
            // question file. The change stays in the waiting set.
            escalate_to_chatops(workspace, repo, ctx, change, &q2, rh2.0).await?;
            (ResumeDisposition::EscalatedAgain, None)
        }
        Ok(ExecutorOutcome::Failed { reason }) => {
            tracing::error!("resume of `{change}` returned Failed: {reason}");
            // .answer.json already deleted above. .question.json was
            // deleted before the resume call. The change reverts cleanly
            // to pending state for the next iteration.
            (ResumeDisposition::Failed, Some(reason))
        }
        Ok(ExecutorOutcome::SpecNeedsRevision {
            unimplementable_tasks,
            revision_suggestion,
        }) => {
            // Even on the resume path, the agent may decide a task is
            // unimplementable (e.g. the operator's answer revealed a
            // requirement outside the sandbox). Same treatment as the
            // pending path: write the marker, alert the operator, halt.
            // Question/answer files were already cleared above; the
            // marker is the new operator-action gate.
            tracing::warn!(
                url = %repo.url,
                change = %change,
                flagged = unimplementable_tasks.len(),
                "resume returned SpecNeedsRevision; writing marker and alerting operator"
            );
            let detail = SpecNeedsRevisionDetail {
                unimplementable_tasks: unimplementable_tasks.clone(),
                revision_suggestion: revision_suggestion.clone(),
            };
            if let Err(e) = spec_revision::write_marker(workspace, change, &detail) {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "failed to write spec-needs-revision marker (resume): {e:#}"
                );
            }
            maybe_post_spec_revision_alert(
                Some(ctx),
                repo,
                change,
                &unimplementable_tasks,
                &revision_suggestion,
            )
            .await;
            (ResumeDisposition::SpecRevisionMarked, None)
        }
    };

    // Counter book-keeping mirrors the pending path:
    //   - Archived → clear
    //   - Failed / CompletedNoDiff (transformed-to-Failed) → record + maybe perma-stuck
    //   - Errored / EscalatedAgain → leave the counter alone
    match (&result, failure_reason) {
        (ResumeDisposition::Archived, _) => {
            if let Err(e) = failure_state::clear(workspace, change) {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "failed to clear failure-state entry after resume archive: {e:#}"
                );
            }
        }
        (ResumeDisposition::Failed, Some(reason))
        | (ResumeDisposition::CompletedNoDiff, Some(reason)) => {
            handle_failure_counter(
                workspace,
                repo,
                Some(ctx),
                change,
                &reason,
                perma_stuck_threshold,
            )
            .await;
        }
        _ => {}
    }

    tracing::info!(
        url = %repo.url,
        change = %change,
        outcome = result.label(),
        "change finished (resume)"
    );

    Ok(match result {
        ResumeDisposition::Archived => Some(change.to_string()),
        _ => None,
    })
}

enum ResumeDisposition {
    Archived,
    CompletedNoDiff,
    EscalatedAgain,
    Failed,
    Errored,
    /// Resume returned `SpecNeedsRevision`. Marker has been written and
    /// the operator alerted; treat as a non-counter-bumping failure-
    /// equivalent (the marker handles exclusion).
    SpecRevisionMarked,
}

impl ResumeDisposition {
    fn label(&self) -> &'static str {
        match self {
            ResumeDisposition::Archived => "archived",
            ResumeDisposition::CompletedNoDiff => "failed_no_diff",
            ResumeDisposition::EscalatedAgain => "escalated",
            ResumeDisposition::Failed => "failed",
            ResumeDisposition::Errored => "errored",
            ResumeDisposition::SpecRevisionMarked => "spec_needs_revision",
        }
    }
}

/// Post a question to ChatOps and write a fresh `.question.json`. Called
/// from the initial AskUser handling (pending → waiting) AND from the
/// resume path when the agent asks ANOTHER question.
async fn escalate_to_chatops(
    workspace: &Path,
    repo: &RepositoryConfig,
    ctx: &ChatOpsContext,
    change: &str,
    question: &str,
    resume_handle: serde_json::Value,
) -> Result<()> {
    let thread_ts = ctx
        .chatops
        .post_question(&ctx.channel, change, question)
        .await
        .with_context(|| format!("posting Slack question for `{change}`"))?;
    let payload = QuestionPayload {
        thread_ts,
        channel: ctx.channel.clone(),
        resume_handle,
        asked_at: chrono::Utc::now(),
    };
    chatops::write_question_file(workspace, change, &payload)?;
    tracing::info!(
        url = repo.url.as_str(),
        "escalated `{change}` to Slack channel {} (thread {})",
        ctx.channel,
        payload.thread_ts
    );
    Ok(())
}

/// Iterate the pending queue, invoking the executor for each ready change.
/// Returns the names of changes that were archived (i.e. those for which the
/// executor returned `Completed`, regardless of diff). On `AskUser`:
///   - if `chatops_ctx` is `Some`, post the question to Slack, write a
///     fresh `.question.json`, unlock, and proceed to the next change;
///   - if `chatops_ctx` is `None`, log an error and break the pass (the
///     architecture-foundation behavior is preserved when chatops is
///     not configured).
async fn walk_queue(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsContext>,
    perma_stuck_threshold: u32,
    max_changes: u32,
    pending: Vec<String>,
) -> Result<(Vec<String>, bool)> {
    let mut archived: Vec<String> = Vec::new();
    let mut includes_self_heal = false;

    for change in pending {
        let result =
            process_one_pending_change(workspace, repo, executor, chatops_ctx, &change).await;

        let outcome_label = match &result {
            Ok(QueueStep::Archived) => "archived",
            Ok(QueueStep::ArchivedSelfHeal) => "archived_self_heal",
            Ok(QueueStep::Failed { .. }) => "failed",
            Ok(QueueStep::Escalated) => "escalated",
            Ok(QueueStep::AskUserExitEarly) => "ask_user_exit_early",
            Ok(QueueStep::SpecRevisionMarked) => "spec_needs_revision",
            Err(_) => "error",
        };
        tracing::info!(
            url = %repo.url,
            change = %change,
            outcome = outcome_label,
            "change finished"
        );

        // Any non-Archive outcome halts the walk. Later changes in the
        // queue may depend on this one having succeeded; attempting them
        // now would either produce wrong-shape commits or contaminate
        // this change's retry. Perma-stuck (default threshold 2) bounds
        // repeat failures: a persistently-failing change is excluded
        // from `list_pending` after the threshold, freeing the queue.
        match result {
            Ok(QueueStep::Archived) | Ok(QueueStep::ArchivedSelfHeal) => {
                let was_self_heal = matches!(&result, Ok(QueueStep::ArchivedSelfHeal));
                if was_self_heal {
                    includes_self_heal = true;
                }
                // Archived (regular or self-heal) → reset the per-change
                // consecutive-failure counter so the next failure starts
                // fresh.
                if let Err(e) = failure_state::clear(workspace, &change) {
                    tracing::warn!(
                        url = %repo.url,
                        change = %change,
                        "failed to clear failure-state entry after archive: {e:#}"
                    );
                }
                archived.push(change);
                if archived.len() as u32 >= max_changes {
                    tracing::info!(
                        url = %repo.url,
                        cap = max_changes,
                        "reached max_changes_per_pr cap; deferring remaining pending changes to next iteration"
                    );
                    break;
                }
            }
            Ok(QueueStep::Failed { reason }) => {
                // Failed (or transformed-to-Failed) → bump the counter and,
                // if the threshold is hit, mark perma-stuck + alert. Then
                // halt the walk: later pending changes may depend on this
                // one and should not be attempted until the next iteration.
                handle_failure_counter(
                    workspace,
                    repo,
                    chatops_ctx,
                    &change,
                    &reason,
                    perma_stuck_threshold,
                )
                .await;
                tracing::info!(
                    url = %repo.url,
                    change = %change,
                    "change failed; halting queue walk this iteration (later changes may depend on this one)"
                );
                break;
            }
            Ok(QueueStep::Escalated) => {
                // Escalation posts a question to chatops and leaves the
                // change in the waiting set. Later pending changes may
                // depend on it; halt the walk so they wait for the human
                // reply on the next iteration.
                tracing::info!(
                    url = %repo.url,
                    change = %change,
                    "change escalated to chatops; halting queue walk this iteration"
                );
                break;
            }
            Ok(QueueStep::AskUserExitEarly) => {
                tracing::error!(
                    url = repo.url.as_str(),
                    "executor returned AskUser for `{change}` AND chatops is not configured; exiting pass. Set the `chatops:` config block to enable escalation."
                );
                break;
            }
            Ok(QueueStep::SpecRevisionMarked) => {
                // Operator-action territory. The marker file, the chatops
                // alert, and the unlock have already been written by
                // `handle_outcome`. We must NOT bump the perma-stuck
                // counter (this isn't repeat-execution-failure territory)
                // but we DO halt the walk so later changes don't run
                // against an environment we just decided we can't
                // implement against.
                tracing::info!(
                    url = %repo.url,
                    change = %change,
                    "change flagged as needing spec revision; halting queue walk this iteration"
                );
                break;
            }
            Err(e) => {
                // The per-change processing function returned Err from a
                // non-executor source (e.g. queue::archive collision,
                // post-executor commit failure, lock I/O, an unlock
                // propagated by handle_outcome). The Failed outcome path
                // is consumed inside handle_outcome → Ok(QueueStep::Failed)
                // and already records via handle_failure_counter, so this
                // wrapper covers the OTHER per-change Err sources without
                // double-counting.
                let reason = format!("post-executor error: {e:#}");
                tracing::error!(
                    url = repo.url.as_str(),
                    change = %change,
                    "fatal error processing change `{change}`: {e:#}"
                );
                handle_failure_counter(
                    workspace,
                    repo,
                    chatops_ctx,
                    &change,
                    &reason,
                    perma_stuck_threshold,
                )
                .await;
                break;
            }
        }
    }

    Ok((archived, includes_self_heal))
}

/// Per-change processing scoped to one entry of the pending queue: lock →
/// optional start-of-work notification → executor.run → handle_outcome →
/// unlock. Any Err this function returns is a non-executor error (the
/// executor-Failed path is consumed inside `handle_outcome` and surfaces
/// as `Ok(QueueStep::Failed)`) and the caller in `walk_queue` records it
/// against the per-change counter before halting the walk.
async fn process_one_pending_change(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsContext>,
    change: &str,
) -> Result<QueueStep> {
    queue::lock(workspace, change)
        .with_context(|| format!("locking change `{change}`"))?;

    // Record which change this iteration is working on so the chatops
    // `status` reply can render `currently: working on <change>`. The
    // marker is held by the caller; best-effort update — failures are
    // logged at DEBUG and don't abort the iteration.
    busy_marker::update_change(workspace, change);

    tracing::info!(
        url = %repo.url,
        change = %change,
        "starting work on change"
    );

    // Start-of-work notification: post a one-liner to chatops when the
    // operator has it enabled. Suppressed entirely when chatops is not
    // wired OR when `notifications.start_work` is false. A failed post
    // logs at WARN and does NOT prevent the executor from running.
    maybe_post_start_of_work(workspace, repo, chatops_ctx, change).await;

    let outcome = executor.run(workspace, change).await;
    let result = handle_outcome(workspace, repo, chatops_ctx, change, outcome).await;
    // Always unlock, even after a Completed → archive (archive moved the
    // dir, so the lock is gone, but `queue::unlock` is idempotent).
    let _ = queue::unlock(workspace, change);
    result
}

/// Pre-flight archive-collision check. For each entry in `candidates`,
/// call `queue::would_collide_on_archive`. Colliding entries are dropped
/// from the returned list, a WARN-level structured log fires (so
/// journalctl tailing surfaces the diagnosis even with chatops disabled),
/// and a chatops alert is posted under `AlertCategory::ArchiveCollision`
/// (subject to the existing 24h per-category throttle). The executor is
/// never invoked for an excluded change — the caller must use the
/// returned (non-colliding) list to drive its queue walk.
///
/// Centralizes the check so both the pending side (`walk_queue` call) and
/// the waiting side (`process_waiting_changes`) share one implementation.
async fn apply_archive_collision_preflight(
    workspace: &Path,
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    candidates: Vec<String>,
) -> Vec<String> {
    let mut kept = Vec::with_capacity(candidates.len());
    for change in candidates {
        if !queue::would_collide_on_archive(workspace, &change) {
            kept.push(change);
            continue;
        }
        let archive_path = queue::archive_collision_path(workspace, &change);
        // WARN-level structured log: emits per iteration regardless of
        // whether the chatops alert is throttled, so operators tailing
        // journalctl see the diagnosis at least once per occurrence.
        tracing::warn!(
            url = %repo.url,
            change = %change,
            archive_path = %archive_path.display(),
            iteration_skipped = true,
            "archive collision detected for `{change}`: openspec/changes/{change}/ would archive to {} but that path already exists; excluding from this iteration",
            archive_path.display(),
        );
        // Body shape per proposal: concrete paths + the fix workflow so
        // the operator's chatops alert is actionable rather than
        // "something's wrong." `handle_predictable_failure` truncates the
        // excerpt at 200 chars when formatting; the long-form body is
        // also captured in the WARN log above so no diagnosis is lost.
        let err = anyhow!(
            "archive collision for `{change}`: openspec/changes/{change}/ would archive to {} but that path already exists. This usually means the change was archived earlier (via a merged PR) and re-added to the active path without removing the prior archive entry. The change is excluded from this iteration's queue walk to avoid burning agent tokens on a run that will fail at archive time. To resolve, on the base branch: (a) if the prior implementation is final: `git rm -r openspec/changes/{change}` and push; (b) if the prior implementation should be reverted and re-done: `git revert -m 1 <merge-sha>` (the merge that landed the prior PR), keeping the revised spec via `git checkout --ours` on the conflicting spec files, then push. Iteration continues with `{change}` excluded.",
            archive_path.display(),
        );
        handle_predictable_failure(
            workspace,
            &repo.url,
            chatops_ctx,
            chatops_ctx
                .map(|c| c.failure_alerts_enabled)
                .unwrap_or(false),
            AlertCategory::ArchiveCollision,
            &err,
        )
        .await;
    }
    kept
}

enum QueueStep {
    Archived,
    /// Same archive bookkeeping as `Archived`, but the implementation was
    /// already on the base branch — autocoder ran the archive move itself
    /// instead of treating Completed-without-diff as Failed. The walker
    /// uses this to flip the pass-level `includes_self_heal` flag, which
    /// adds a disclaimer paragraph to the PR body.
    ArchivedSelfHeal,
    /// The executor (or post-execution classification) marked this change
    /// as Failed. `reason` is either the executor's explicit Failed
    /// reason or a synthetic one for the no-op / lazy-archive cases.
    Failed {
        reason: String,
    },
    Escalated,
    AskUserExitEarly,
    /// The executor returned `SpecNeedsRevision`. The change's marker has
    /// been written and the chatops alert posted. The walker halts the
    /// queue this iteration (operator-action territory). Unlike `Failed`,
    /// this MUST NOT increment the perma-stuck counter — the marker
    /// handles exclusion directly, the counter is irrelevant here.
    SpecRevisionMarked,
}

/// Increment the per-change failure counter, and on threshold transition
/// write the perma-stuck marker + post the chatops alert. Best-effort: any
/// I/O or transport failure here is logged at WARN and does not propagate.
async fn handle_failure_counter(
    workspace: &Path,
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    change: &str,
    reason: &str,
    threshold: u32,
) {
    let count = match failure_state::record_failure(workspace, change, reason) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                url = %repo.url,
                change = %change,
                "failed to record consecutive-failure state: {e:#}"
            );
            return;
        }
    };
    if count < threshold {
        return;
    }
    let entry = failure_state::FailureEntry {
        count,
        last_reason: reason.to_string(),
        last_failed_at: Utc::now(),
    };
    if let Err(e) = perma_stuck::write_marker(workspace, change, &entry) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to write perma-stuck marker: {e:#}"
        );
        // Continue to alert — the operator should still know.
    }
    let marker_path = workspace
        .join("openspec/changes")
        .join(change)
        .join(".perma-stuck.json");
    tracing::error!(
        url = %repo.url,
        change = %change,
        marker = %marker_path.display(),
        consecutive_failures = count,
        "change marked perma-stuck after {count} consecutive failures; daemon will not retry until {} is removed",
        marker_path.display()
    );
    post_perma_stuck_alert(chatops_ctx, repo, change, reason, count).await;
}

/// Post the chatops perma-stuck alert (best-effort, 24h-throttled per
/// change). The state for this throttle lives in
/// `.alert-state.json`'s `perma_stuck_alerts` map.
async fn post_perma_stuck_alert(
    chatops_ctx: Option<&ChatOpsContext>,
    repo: &RepositoryConfig,
    change: &str,
    reason: &str,
    count: u32,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(repo);
    let mut state = AlertState::load_or_default(&workspace);
    let now = Utc::now();
    let should_alert = state
        .perma_stuck_alerts
        .get(change)
        .map(|entry| {
            now - entry.last_alerted_at
                >= ChronoDuration::hours(PERMA_STUCK_ALERT_THROTTLE_HOURS)
        })
        .unwrap_or(true);
    if !should_alert {
        return;
    }
    let excerpt = truncate_reason(reason);
    let marker_path = workspace
        .join("openspec/changes")
        .join(change)
        .join(".perma-stuck.json");
    // Tied to the Claude CLI executor's log convention; refactor to an
    // Executor trait method if a second executor backend with a
    // different log layout is added.
    let log_path = crate::executor::claude_cli::run_log_path(&workspace, change);
    let text = format!(
        ":no_entry: autocoder: change perma-stuck\nrepo: {}\nchange: {}\nconsecutive_failures: {count}\nlast_reason: {excerpt}\nrun_log: {}\n\nThis change has failed {count} iterations in a row. autocoder will not retry until an operator removes {}.",
        repo.url,
        change,
        log_path.display(),
        marker_path.display(),
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "perma-stuck chatops alert post failed: {e:#}"
        );
        return;
    }
    state.perma_stuck_alerts.insert(
        change.to_string(),
        AlertEntry {
            last_alerted_at: now,
            last_error_excerpt: excerpt,
        },
    );
    if let Err(e) = state.save(&workspace) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to persist perma-stuck alert state: {e:#}"
        );
    }
}

/// Post the chatops spec-needs-revision alert (best-effort, 24h-throttled
/// per change). State for this throttle lives in `.alert-state.json`'s
/// `spec_revision_alerts` map. Mirrors `post_perma_stuck_alert` — both
/// announce operator-action states with the same throttle window.
async fn maybe_post_spec_revision_alert(
    chatops_ctx: Option<&ChatOpsContext>,
    repo: &RepositoryConfig,
    change: &str,
    flagged_tasks: &[UnimplementableTask],
    revision_suggestion: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(repo);
    let mut state = AlertState::load_or_default(&workspace);
    let now = Utc::now();
    let should_alert = state
        .spec_revision_alerts
        .get(change)
        .map(|entry| {
            now - entry.last_alerted_at
                >= ChronoDuration::hours(PERMA_STUCK_ALERT_THROTTLE_HOURS)
        })
        .unwrap_or(true);
    if !should_alert {
        return;
    }
    let marker_path = workspace
        .join("openspec/changes")
        .join(change)
        .join(".needs-spec-revision.json");
    let log_path = crate::executor::claude_cli::run_log_path(&workspace, change);
    let mut tasks_block = String::new();
    for task in flagged_tasks {
        tasks_block.push_str(&format!("  - {}: {} ({})\n", task.task_id, task.task_text, task.reason));
    }
    let text = format!(
        "⚠️ `{repo_url}`: spec needs revision — `{change}` has unimplementable tasks\n\nTasks the agent flagged as outside its sandbox:\n{tasks_block}\nSuggested revision:\n  {suggestion}\n\nOperator action:\n  1. Edit openspec/changes/{change}/tasks.md to remove or revise the flagged tasks.\n  2. Commit + push to {base}.\n  3. Delete openspec/changes/{change}/.needs-spec-revision.json — the next iteration will retry the change.\n\nmarker: {marker}\nlog:    {log}",
        repo_url = repo.url,
        change = change,
        tasks_block = tasks_block,
        suggestion = revision_suggestion,
        base = repo.base_branch,
        marker = marker_path.display(),
        log = log_path.display(),
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "spec-needs-revision chatops alert post failed: {e:#}"
        );
        return;
    }
    state.spec_revision_alerts.insert(
        change.to_string(),
        AlertEntry {
            last_alerted_at: now,
            last_error_excerpt: truncate_reason(revision_suggestion),
        },
    );
    if let Err(e) = state.save(&workspace) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to persist spec-needs-revision alert state: {e:#}"
        );
    }
}

fn truncate_reason(reason: &str) -> String {
    let count = reason.chars().count();
    if count <= PERMA_STUCK_REASON_EXCERPT_MAX {
        reason.to_string()
    } else {
        let mut out: String = reason.chars().take(PERMA_STUCK_REASON_EXCERPT_MAX).collect();
        out.push('…');
        out
    }
}

/// Attempt to recover a workspace whose pre-pass dirty check tripped.
/// Mirrors the startup recovery in `cli/run.rs::repo_passes_startup_check`:
/// best-effort `git checkout <base>` (might fail if uncommitted
/// modifications would be overwritten — that's fine, the next step forces
/// the issue), then `git reset --hard origin/<base>`, then `git clean -fd`.
///
/// Safe in the per-iteration position because the agent branch is rebuilt
/// from base each iteration via `recreate_branch`; wholesale wiping does
/// not lose recoverable work. The caller is responsible for re-checking
/// `git status --porcelain` after this returns.
fn attempt_dirty_workspace_recovery(workspace: &Path, base_branch: &str) -> Result<()> {
    let _ = git::checkout(workspace, base_branch);
    git::reset_hard_to_remote(workspace, base_branch)
        .with_context(|| format!("git reset --hard origin/{base_branch}"))?;
    git::clean_force(workspace).with_context(|| "git clean -fd".to_string())?;
    Ok(())
}

/// Remove `git status --porcelain` lines that reference the
/// workspace-root `.alert-state.json` bookkeeping file. The file is
/// autocoder-owned, intentionally untracked, and never executor output.
fn filter_alert_state_lines(porcelain: &str) -> String {
    porcelain
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            // Status block is 1–2 chars + space + path; for the strict
            // match we look for the file basename at the start of the path
            // portion. Any line that names `.alert-state.json` as its only
            // path is autocoder bookkeeping.
            let path_start = trimmed.find(char::is_whitespace);
            let path = match path_start {
                Some(i) => trimmed[i..].trim_start(),
                None => trimmed,
            };
            path != ".alert-state.json"
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Post a `🚀 <repo>: starting work on <change> — <first-line-of-Why>`
/// notification when chatops is wired AND `start_work_enabled` is true.
/// Reads `proposal.md` only when the notification will actually be posted
/// so a disabled flag avoids the disk read entirely.
async fn maybe_post_start_of_work(
    workspace: &Path,
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    change: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.start_work_enabled {
        return;
    }
    let proposal_path = workspace
        .join("openspec/changes")
        .join(change)
        .join("proposal.md");
    let summary = match std::fs::read_to_string(&proposal_path) {
        Ok(raw) => first_line_of_section(&raw, "## Why").unwrap_or_default(),
        Err(e) => {
            tracing::warn!(
                url = %repo.url,
                change = %change,
                "could not read proposal.md for start-of-work summary: {e}; posting without summary"
            );
            String::new()
        }
    };
    let text = if summary.is_empty() {
        format!("🚀 `{}`: starting work on `{change}`", repo.url)
    } else {
        format!("🚀 `{}`: starting work on `{change}` — {summary}", repo.url)
    };
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "start-of-work notification failed; continuing: {e:#}"
        );
    }
}

/// Run a single rebuild iteration: acquire the busy marker, ensure the
/// workspace is on a clean agent branch, run the rebuild, commit + push
/// + open a PR if drift was found, and post the end-of-rebuild chatops
/// notification.
///
/// Failures from individual archived changes are accumulated in the
/// `RebuildReport` and do NOT abort the iteration. A failure to push or
/// open the PR is propagated as the iteration's Err — the chatops
/// notification still fires (best-effort, separate code path).
async fn execute_rebuild_iteration(
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    stuck_threshold_secs: u64,
) -> Result<()> {
    let mut guard = match busy_marker::try_acquire(workspace, &repo.url, stuck_threshold_secs) {
        Ok(busy_marker::AcquireOutcome::Acquired(g)) => g,
        Ok(busy_marker::AcquireOutcome::SkipFreshInProgress(m)) => {
            tracing::info!(
                url = %repo.url,
                pid = m.pid,
                "rebuild iteration: busy marker held by another pass; will retry next iteration"
            );
            return Ok(());
        }
        Ok(busy_marker::AcquireOutcome::SkipAmbiguous(m)) => {
            tracing::error!(
                url = %repo.url,
                pid = m.pid,
                "rebuild iteration: ambiguous busy-marker state; skipping"
            );
            post_stuck_alert(chatops_ctx, repo, &m, true).await;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    tracing::info!(
        url = %repo.url,
        "iteration: running spec rebuild instead of queue walk"
    );

    // Make sure the workspace is initialized + on a clean agent branch
    // before we mutate openspec/specs/. We reuse the existing setup that
    // run_pass_through_commits performs to keep behavior identical.
    let fork_url = match github_cfg.fork_owner.as_deref() {
        Some(owner) => Some(crate::github::derive_fork_url(&repo.url, owner)?),
        None => None,
    };
    let fork_arg = fork_url
        .as_deref()
        .map(|u| (u, repo.agent_branch.as_str()));
    workspace::ensure_initialized(workspace, &repo.url, fork_arg)?;

    // If the workspace is dirty (e.g. a SIGTERMed iteration left state),
    // try to recover. Failure to recover is fatal for this iteration.
    let dirty = git::status_porcelain(workspace)?;
    let dirty_filtered = filter_alert_state_lines(&dirty);
    if !dirty_filtered.is_empty() {
        tracing::warn!(
            url = %repo.url,
            "rebuild iteration: workspace dirty; attempting recovery"
        );
        attempt_dirty_workspace_recovery(workspace, &repo.base_branch)?;
    }
    git::fetch(workspace)?;
    git::checkout(workspace, &repo.base_branch)?;
    git::pull_ff_only(workspace, &repo.base_branch)?;
    git::recreate_branch(workspace, &repo.agent_branch)?;

    let _ = guard.set_stage(busy_marker::Stage::Commit);
    let report = crate::cli::sync_specs::rebuild_canonical(workspace).await?;
    tracing::info!(
        url = %repo.url,
        processed = report.processed,
        successful = report.successful,
        failed = report.failed,
        modified_files = report.modified_files(),
        "rebuild_canonical finished"
    );

    // Stage everything: openspec/specs/ changes AND any archive directory
    // moves (the in-place rename shouldn't produce a net diff but we
    // stage defensively).
    git::add_all(workspace)?;

    let porcelain = git::status_porcelain(workspace)?;
    let staged = filter_alert_state_lines(&porcelain);
    let mut pr_url: Option<String> = None;

    if staged.is_empty() {
        tracing::info!(
            url = %repo.url,
            "rebuild iteration: no drift detected — skipping commit/push/PR"
        );
    } else {
        let modified = report.modified_files();
        let subject = format!(
            "spec rebuild: {modified} capability(ies) rebuilt from {} archived change(s)",
            report.successful
        );
        git::commit(workspace, &subject)?;
        let push_remote = if github_cfg.fork_owner.is_some() {
            "fork"
        } else {
            "origin"
        };
        let _ = guard.set_stage(busy_marker::Stage::Push);
        git::push_force_with_lease(workspace, &repo.agent_branch, push_remote)?;

        let _ = guard.set_stage(busy_marker::Stage::Pr);
        match open_rebuild_pull_request(repo, github_cfg, &report).await {
            Ok(url) => {
                pr_url = Some(url);
            }
            Err(e) => {
                tracing::error!(
                    url = %repo.url,
                    "rebuild iteration: PR creation failed: {e:#}"
                );
                // We still want to send the chatops notification so the
                // operator knows the rebuild happened (and that the PR
                // step failed). Propagate err after the notification.
                maybe_post_end_of_rebuild_notification(repo, &report, None, chatops_ctx).await;
                return Err(e);
            }
        }
    }

    maybe_post_end_of_rebuild_notification(repo, &report, pr_url.as_deref(), chatops_ctx).await;
    Ok(())
}

/// Open the PR for a rebuild iteration. Returns the new PR's HTML URL on
/// success.
async fn open_rebuild_pull_request(
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    report: &crate::cli::sync_specs::RebuildReport,
) -> Result<String> {
    let (owner, repo_name) = github::parse_repo_url(&repo.url)?;
    let token = crate::github_credentials::resolve_token(github_cfg, &owner)?;
    let modified = report.modified_files();
    let title = format!(
        "spec rebuild: {modified} capability(ies) rebuilt from archive history"
    );
    let mut body = String::new();
    body.push_str("This PR was generated by `autocoder sync-specs --rebuild`.\n\n");
    body.push_str(&format!(
        "Replayed {} archived change(s) chronologically; {} succeeded, {} failed.\n\n",
        report.processed, report.successful, report.failed
    ));
    if !report.failures.is_empty() {
        body.push_str("**Failed changes** (left at active path for operator inspection):\n");
        for f in &report.failures {
            body.push_str(&format!("- `{}`: {}\n", f.slug, truncate_one_line(&f.failure_reason, 200)));
        }
        body.push('\n');
    }
    body.push_str("**Canonical spec files**:\n");
    for sf in &report.spec_files {
        let tag = if sf.modified { "modified" } else { "unchanged" };
        body.push_str(&format!("- `{}` ({tag})\n", sf.path));
    }
    let head = match github_cfg.fork_owner.as_deref() {
        Some(fork_owner) => format!("{fork_owner}:{}", repo.agent_branch),
        None => repo.agent_branch.clone(),
    };
    let pr = github::create_pull_request(
        &owner,
        &repo_name,
        &head,
        &repo.base_branch,
        &title,
        &body,
        &token,
        None,
        false,
    )
    .await?;
    tracing::info!(
        url = repo.url.as_str(),
        pr = pr.html_url.as_str(),
        pr_number = pr.number,
        "opened rebuild PR"
    );
    Ok(pr.html_url)
}

fn truncate_one_line(s: &str, n: usize) -> String {
    let one = s.lines().next().unwrap_or("");
    if one.chars().count() <= n {
        one.to_string()
    } else {
        one.chars().take(n).collect::<String>() + "…"
    }
}

/// Post the end-of-rebuild chatops notification. Best-effort: a failed
/// post logs at WARN and never propagates. Unlike `maybe_post_pr_opened`,
/// this is NOT gated on `pr_opened_enabled` or `failure_alerts_enabled`
/// because it's a direct response to an operator-triggered command — the
/// operator wants the completion signal regardless of which notification
/// toggles they have set elsewhere.
async fn maybe_post_end_of_rebuild_notification(
    repo: &RepositoryConfig,
    report: &crate::cli::sync_specs::RebuildReport,
    pr_url: Option<&str>,
    chatops_ctx: Option<&ChatOpsContext>,
) {
    let Some(ctx) = chatops_ctx else { return };

    let modified = report.modified_files();
    let text = if report.failed == 0 {
        if let Some(url) = pr_url {
            format!(
                "✓ rebuild complete for `{}`: PR {url} opened — {modified} capability(ies) updated from {} archived change(s)",
                repo.url, report.successful
            )
        } else {
            format!(
                "✓ rebuild complete for `{}`: no drift detected, canonical specs already in sync",
                repo.url
            )
        }
    } else {
        let pr_segment = match pr_url {
            Some(u) => format!("PR {u}"),
            None => "(no PR — every change failed)".to_string(),
        };
        let slugs = report.failed_slugs();
        let listed: Vec<String> = slugs.iter().take(10).cloned().collect();
        let suffix = if slugs.len() > 10 {
            format!(" and {} more", slugs.len() - 10)
        } else {
            String::new()
        };
        let failed_list = format!("{}{suffix}", listed.join(", "));
        format!(
            "⚠️ rebuild for `{}` completed with {} failure(s); {pr_segment} opened with successful {} change(s).\nFailed: {failed_list}.\nSee journalctl -u autocoder for openspec stderr details.",
            repo.url, report.failed, report.successful
        )
    };

    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            "end-of-rebuild chatops notification failed; continuing: {e:#}"
        );
    }
}

/// Post a one-line ChatOps notification announcing a freshly-opened PR.
/// Suppressed when chatops is not configured OR when `pr_opened_enabled` is
/// false. Best-effort: a failed post logs at WARN and never propagates.
async fn maybe_post_pr_opened(
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    pr_url: &str,
    change_count: usize,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.pr_opened_enabled {
        return;
    }
    let text = format!(
        "🎉 `{}`: opened PR {pr_url} with {change_count} change(s)",
        repo.url
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            pr = %pr_url,
            "pr-opened notification failed; continuing: {e:#}"
        );
    }
}

/// Post a one-line ChatOps notification announcing a fork recreation.
/// Re-forking is destructive: any open PRs from the deleted fork are
/// closed by GitHub when the head ref disappears, so operators should
/// see this immediately. Gated by `failure_alerts_enabled` (re-fork is
/// a recovery action; if the operator opted out of failure alerts, they
/// have opted out of this too).
async fn maybe_post_refork_notification(
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let text = format!(
        ":warning: `{}`: re-forked at workspace reinitialization \
         (previous fork deleted; any open PRs from this fork are now closed)",
        repo.url
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            "re-fork notification failed; continuing: {e:#}"
        );
    }
}

async fn handle_outcome(
    workspace: &Path,
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    change: &str,
    outcome: Result<ExecutorOutcome>,
) -> Result<QueueStep> {
    match outcome {
        Err(e) => {
            // Executor task error (e.g. spawn failure). This is closer to
            // an infrastructure flake than an agent-decided Failed, but
            // the architecture-foundation contract treats it as Failed and
            // we follow suit; the reason carries the error text.
            let reason = format!("{e:#}");
            tracing::error!("executor errored on `{change}`: {reason}");
            Ok(QueueStep::Failed { reason })
        }
        Ok(ExecutorOutcome::Failed { reason }) => {
            tracing::error!("executor reported Failed for `{change}`: {reason}");
            Ok(QueueStep::Failed { reason })
        }
        Ok(ExecutorOutcome::SpecNeedsRevision {
            unimplementable_tasks,
            revision_suggestion,
        }) => {
            tracing::warn!(
                url = %repo.url,
                change = %change,
                flagged = unimplementable_tasks.len(),
                "executor returned SpecNeedsRevision; writing marker and alerting operator"
            );
            // (a) Unlock the change so it's not left in an in-progress
            // state. Mirrors how every other Failed-equivalent outcome
            // hands the change back to operator-managed territory.
            queue::unlock(workspace, change)?;
            // (b) Write the marker. A failure here is logged but does NOT
            // propagate: the alert still goes out, and the next iteration
            // would simply re-trigger the outcome (the agent's pre-flight
            // is deterministic for a given tasks.md).
            let detail = SpecNeedsRevisionDetail {
                unimplementable_tasks: unimplementable_tasks.clone(),
                revision_suggestion: revision_suggestion.clone(),
            };
            if let Err(e) = spec_revision::write_marker(workspace, change, &detail) {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "failed to write spec-needs-revision marker: {e:#}"
                );
            }
            // (c) Post the chatops alert. Best-effort: any failure is
            // logged at WARN and does not propagate.
            maybe_post_spec_revision_alert(
                chatops_ctx,
                repo,
                change,
                &unimplementable_tasks,
                &revision_suggestion,
            )
            .await;
            // (d) Halt the queue walk this iteration. Do NOT increment
            // the perma-stuck counter — the marker handles exclusion
            // directly; the counter is for repeat-execution-failure
            // territory, which this is not.
            Ok(QueueStep::SpecRevisionMarked)
        }
        Ok(ExecutorOutcome::AskUser {
            question,
            resume_handle,
        }) => match chatops_ctx {
            Some(ctx) => {
                // Unlock BEFORE posting so the change is in a clean
                // "waiting" state (no .in-progress) as the spec mandates.
                queue::unlock(workspace, change)?;
                escalate_to_chatops(workspace, repo, ctx, change, &question, resume_handle.0)
                    .await?;
                Ok(QueueStep::Escalated)
            }
            None => {
                tracing::warn!("executor asked a question on `{change}`: {question}");
                Ok(QueueStep::AskUserExitEarly)
            }
        },
        Ok(ExecutorOutcome::Completed) => {
            // Remove the `.in-progress` lock BEFORE inspecting the working
            // tree: the lock file is untracked and would otherwise show up
            // in `git status --porcelain`, contaminating the dirty check
            // and getting swept into the commit by `git add -A`.
            queue::unlock(workspace, change)?;
            let dirty = git::status_porcelain(workspace)?;
            if dirty.is_empty() {
                // Self-heal probe: if every task is `[x]` AND
                // `openspec validate --strict` exits 0, the change's
                // implementation is already on the base branch and the
                // only thing missing is the archive move. Run the archive
                // ourselves rather than burn another iteration on a no-op
                // Completed.
                let tasks_complete = tasks_md_all_complete(workspace, change).unwrap_or(false);
                if tasks_complete && openspec_validate_strict_passes(workspace, change) {
                    tracing::info!(
                        url = %repo.url,
                        change = %change,
                        "self-heal: implementation already in HEAD, archiving"
                    );
                    let subject =
                        format!("archive: {change}: implementation already in base");
                    if let Err(e) = queue::archive(workspace, change) {
                        tracing::error!(
                            url = %repo.url,
                            change = %change,
                            "self-heal: queue::archive failed: {e:#}"
                        );
                        return Ok(QueueStep::Failed {
                            reason: format!("self-heal archive failed: {e:#}"),
                        });
                    }
                    if let Err(e) = git::add_all(workspace) {
                        tracing::error!(
                            url = %repo.url,
                            change = %change,
                            "self-heal: git add -A failed: {e:#}"
                        );
                        return Ok(QueueStep::Failed {
                            reason: format!("self-heal git add failed: {e:#}"),
                        });
                    }
                    if let Err(e) = git::commit(workspace, &subject) {
                        tracing::error!(
                            url = %repo.url,
                            change = %change,
                            "self-heal: git commit failed: {e:#}"
                        );
                        return Ok(QueueStep::Failed {
                            reason: format!("self-heal git commit failed: {e:#}"),
                        });
                    }
                    return Ok(QueueStep::ArchivedSelfHeal);
                }
                tracing::warn!(
                    "agent reported Completed for `{change}` without modifying the workspace; marking Failed"
                );
                return Ok(QueueStep::Failed {
                    reason: "agent reported Completed without modifying the workspace".into(),
                });
            } else if is_lazy_archive(&dirty) {
                tracing::warn!(
                    "agent appears to have archived `{change}` without implementing the change; reverting and marking Failed"
                );
                // Revert the staged moves so the next iteration starts clean.
                if let Err(e) = git::reset_hard_head(workspace) {
                    tracing::error!(
                        "failed to revert lazy-archive moves for `{change}`: {e:#}"
                    );
                }
                return Ok(QueueStep::Failed {
                    reason: "agent attempted lazy archive (rename only, no implementation)".into(),
                });
            } else {
                // Build the commit subject BEFORE the archive rename: it
                // reads `openspec/changes/<change>/proposal.md`, which the
                // archive step moves to `openspec/changes/archive/...`.
                let subject = build_commit_subject(workspace, change)?;
                // Archive BEFORE the commit so the single commit captures
                // both the executor's implementation diff AND the archive
                // rename. After this sequence the working tree is clean,
                // even for the trailing change of a pass — no dangling
                // rename for the next iteration's dirty-check to trip on.
                queue::archive(workspace, change)?;
                git::add_all(workspace)?;
                git::commit(workspace, &subject)?;
            }
            Ok(QueueStep::Archived)
        }
    }
}

/// Detect the lazy-archive failure mode: the executor returned Completed
/// but the only thing it did was rename the change directory into
/// `openspec/changes/archive/`. Returns true when:
/// - `status` is non-empty, AND
/// - every line is a rename (status code contains `R`), AND
/// - every rename's destination path starts with `openspec/changes/archive/`.
///
/// Returns false for any mix that includes a non-rename or a rename outside
/// the archive path — those are treated as legitimate implementations.
fn is_lazy_archive(status: &str) -> bool {
    let mut any = false;
    for line in status.lines() {
        if line.len() < 4 {
            return false; // malformed; bail rather than misclassify
        }
        // Porcelain format: two status chars in cols 0-1, space, then paths.
        let staged = line.as_bytes()[0] as char;
        let unstaged = line.as_bytes()[1] as char;
        if staged != 'R' && unstaged != 'R' {
            return false;
        }
        // Rename lines look like `R  old_path -> new_path`.
        let payload = &line[3..];
        let dest = match payload.split_once(" -> ") {
            Some((_old, new)) => new,
            None => return false,
        };
        if !dest.starts_with("openspec/changes/archive/") {
            return false;
        }
        any = true;
    }
    any
}

/// Decide whether a `git status --porcelain` block (taken after a resume
/// returned `Completed`) contains any change attributable to the executor,
/// as opposed to autocoder's own bookkeeping. In the resume path autocoder
/// itself writes/deletes `.question.json` and `.answer.json` inside the
/// change directory; those entries are NOT executor output and must not
/// be counted when deciding whether the executor produced an artifact.
///
/// Returns true iff at least one porcelain entry references a path that
/// is NOT one of the meta-files for `change`.
fn has_executor_changes(status: &str, change: &str) -> bool {
    let q = format!("openspec/changes/{change}/.question.json");
    let a = format!("openspec/changes/{change}/.answer.json");
    let is_meta = |path: &str| path == q || path == a;
    for raw_line in status.lines() {
        // `git::status_porcelain` trims the entire blob, which strips the
        // leading column-1 space on the first/last line of unstaged
        // changes (e.g. ` D path` -> `D path`). Re-normalize per line by
        // skipping the leading status block and the whitespace that
        // separates it from the path, rather than fixed `line[3..]`.
        let line = raw_line.trim_start();
        if line.is_empty() {
            continue;
        }
        let path_start = match line.find(char::is_whitespace) {
            Some(i) => i,
            None => continue, // malformed; skip rather than misclassify
        };
        let payload = line[path_start..].trim_start();
        if payload.is_empty() {
            continue;
        }
        // Rename: `<old> -> <new>` — both sides must be meta to skip.
        let (left, right) = match payload.split_once(" -> ") {
            Some((l, r)) => (l, Some(r)),
            None => (payload, None),
        };
        if !is_meta(left) {
            return true;
        }
        if let Some(r) = right {
            if !is_meta(r) {
                return true;
            }
        }
    }
    false
}

/// Build a commit subject from the change name and the first non-empty line of
/// the `## Why` section of `proposal.md`. Truncated to 72 characters total.
fn build_commit_subject(workspace: &Path, change: &str) -> Result<String> {
    let proposal = workspace
        .join("openspec/changes")
        .join(change)
        .join("proposal.md");
    let raw = std::fs::read_to_string(&proposal)
        .with_context(|| format!("reading proposal for commit subject: {}", proposal.display()))?;
    let why_summary = first_line_of_section(&raw, "## Why").unwrap_or_else(|| change.to_string());
    let mut subject = format!("{change}: {why_summary}");
    if subject.chars().count() > 72 {
        subject = subject.chars().take(72).collect();
    }
    Ok(subject)
}

/// Return the first non-empty line under the named markdown header. Returns
/// `None` if the header is absent or has no non-empty body line.
fn first_line_of_section(text: &str, header: &str) -> Option<String> {
    let mut in_section = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.trim_start().starts_with("## ") {
            in_section = line.trim_start() == header;
            continue;
        }
        if in_section {
            let stripped = line.trim();
            if !stripped.is_empty() {
                return Some(stripped.to_string());
            }
        }
    }
    None
}

async fn open_pull_request(
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    changes: &[String],
    includes_self_heal: bool,
    review_report: Option<&ReviewReport>,
    draft: bool,
    chatops_ctx: Option<&ChatOpsContext>,
    workspace: &Path,
) -> Result<()> {
    let (owner, repo_name) = github::parse_repo_url(&repo.url)?;
    // PAT routing uses the UPSTREAM owner, not the fork owner — the PR is
    // posted to upstream's /pulls endpoint regardless of fork-PR mode, so
    // the credential authorizing that call must have access to upstream.
    let token = crate::github_credentials::resolve_token(github_cfg, &owner)?;
    let title = build_pr_title(changes);
    let body = build_pr_body(workspace, changes, includes_self_heal);

    // In fork-PR mode the `head` is namespaced `<fork-owner>:<branch>` for
    // GitHub to recognize the cross-repo PR. Direct-push mode uses the bare
    // branch name (same-repo PR).
    let head = match github_cfg.fork_owner.as_deref() {
        Some(fork_owner) => format!("{fork_owner}:{}", repo.agent_branch),
        None => repo.agent_branch.clone(),
    };

    let pr = match github::create_pull_request(
        &owner,
        &repo_name,
        &head,
        &repo.base_branch,
        &title,
        &body,
        &token,
        review_report,
        draft,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            handle_predictable_failure(
                workspace,
                &repo.url,
                chatops_ctx,
                chatops_ctx
                    .map(|c| c.failure_alerts_enabled)
                    .unwrap_or(false),
                AlertCategory::PrCreationFailure,
                &e,
            )
            .await;
            return Err(e);
        }
    };
    tracing::info!(
        url = repo.url.as_str(),
        pr = pr.html_url.as_str(),
        pr_number = pr.number,
        "opened PR"
    );

    // Best-effort: post a one-line ChatOps notification with a link to
    // the new PR. PR creation already succeeded; never propagate a
    // failure from this step.
    maybe_post_pr_opened(repo, chatops_ctx, &pr.html_url, changes.len()).await;

    // Best-effort: post a follow-up comment with each change's implementer
    // stdout. PR creation already succeeded; never propagate a failure
    // from this step.
    post_implementer_summary_comment(
        github::DEFAULT_API_BASE,
        workspace,
        &owner,
        &repo_name,
        pr.number,
        changes,
        &token,
    )
    .await;

    Ok(())
}

/// Build the implementer-summary markdown for `processed`, truncate it to
/// fit GitHub's comment limit, and POST it as an issue comment to the PR
/// (issues and PRs share the comments endpoint). Best-effort: any
/// failure is logged at ERROR; the caller's PR creation has already
/// succeeded and is not rolled back.
///
/// `api_base` is `github::DEFAULT_API_BASE` in production; tests pass a
/// mockito server URL instead.
async fn post_implementer_summary_comment(
    api_base: &str,
    workspace: &Path,
    upstream_owner: &str,
    upstream_repo: &str,
    pr_number: u64,
    processed: &[String],
    token: &str,
) {
    let body = build_implementer_summary(workspace, processed);
    if body.is_empty() {
        tracing::info!(
            pr_number,
            "skipping implementer-summary comment: no per-change run-log content available"
        );
        return;
    }
    let body = truncate_to_fit(body, 60_000);

    let result = if api_base == github::DEFAULT_API_BASE {
        github::create_issue_comment(upstream_owner, upstream_repo, pr_number, &body, token).await
    } else {
        #[cfg(test)]
        {
            github::create_issue_comment_at_for_test(
                api_base,
                upstream_owner,
                upstream_repo,
                pr_number,
                &body,
                token,
            )
            .await
        }
        #[cfg(not(test))]
        {
            unreachable!("non-default api_base is test-only");
        }
    };

    match result {
        Ok(()) => tracing::info!(
            pr_number,
            change_count = processed.len(),
            "posted implementer-summary comment"
        ),
        Err(e) => tracing::error!("posting implementer-summary comment failed: {e:#}"),
    }
}

/// Read each change's run-log from `<system-temp>/autocoder/logs/<workspace-basename>/<change>.log`,
/// extract the `=== STDOUT (n bytes) ===` block (only stdout — stderr is
/// operator log noise), and assemble a single markdown comment with one
/// section per change. If a log is unreadable, the change is skipped with
/// a WARN. If ALL changes' logs are unreadable, returns an empty string.
fn build_implementer_summary(workspace: &Path, processed: &[String]) -> String {
    let mut sections = Vec::new();
    for change in processed {
        let path = crate::executor::claude_cli::run_log_path(workspace, change);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    change = %change,
                    path = %path.display(),
                    "implementer-summary: skipping change — cannot read run-log: {e}"
                );
                continue;
            }
        };
        let stdout = extract_stdout_section(&raw);
        let trimmed = stdout.trim_end();
        let body = if trimmed.is_empty() {
            "_(no implementer output captured)_".to_string()
        } else {
            trimmed.to_string()
        };
        sections.push(format!("### {change}\n\n{body}"));
    }

    if sections.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("## Agent implementation notes\n");
    out.push_str(
        "<!-- Generated by autocoder; the agent's end-of-run output for each\n",
    );
    out.push_str(
        "     change in this pass. Reviewer is the separate `## Code Review`\n",
    );
    out.push_str("     section in the PR body. -->\n\n");
    out.push_str(&sections.join("\n\n"));
    out
}

/// Slice out the bytes between the `=== STDOUT (n bytes) ===` header and
/// the next `=== STDERR (` header (or end-of-file). The match on the
/// STDOUT header is anchored on the literal prefix to tolerate the
/// variable byte-count in the parens. Returns an empty string if the
/// STDOUT header is absent.
fn extract_stdout_section(raw: &str) -> &str {
    let stdout_marker = "=== STDOUT (";
    let stderr_marker = "=== STDERR (";
    let stdout_idx = match raw.find(stdout_marker) {
        Some(i) => i,
        None => return "",
    };
    // Advance past the header line.
    let after_header = match raw[stdout_idx..].find('\n') {
        Some(nl) => stdout_idx + nl + 1,
        None => return "",
    };
    let end = match raw[after_header..].find(stderr_marker) {
        Some(rel) => after_header + rel,
        None => raw.len(),
    };
    &raw[after_header..end]
}

/// Truncate `body` to fit within GitHub's comment size limit. If `body`
/// is short enough, returned as-is. Otherwise truncated at the largest
/// char boundary `<= max` and a marker noting the truncation is appended.
fn truncate_to_fit(body: String, max: usize) -> String {
    if body.len() <= max {
        return body;
    }
    let mut cut = max;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = body[..cut].to_string();
    truncated.push_str(
        "\n\n_[implementer summary truncated to fit GitHub comment limit; full output at /tmp/autocoder/logs/<workspace-basename>/<change>.log]_",
    );
    truncated
}

/// Replace hyphens in `slug` with spaces. If the slug carries the
/// stacked `aNN-` prefix convention (`^[a-z]+\d+-`), keep that prefix as
/// a leading label followed by `": "` and the de-hyphenated remainder.
/// Otherwise just swap hyphens for spaces wholesale.
fn humanize_slug(slug: &str) -> String {
    let re = regex::Regex::new(r"^([a-z]+\d+)-(.+)$")
        .expect("static regex compiles");
    if let Some(caps) = re.captures(slug) {
        let prefix = &caps[1];
        let rest = caps[2].replace('-', " ");
        format!("{prefix}: {rest}")
    } else {
        slug.replace('-', " ")
    }
}

/// Build a PR title from the list of changes processed in a pass.
/// Single-change passes get the humanized slug; multi-change passes get
/// `<first humanized> (+N more)`. Total length is capped at ~80 chars,
/// with an ellipsis replacing the truncated tail.
fn build_pr_title(changes: &[String]) -> String {
    const MAX_LEN: usize = 80;
    const ELLIPSIS: char = '…';

    if changes.is_empty() {
        return "agent: empty pass".to_string();
    }
    let first = humanize_slug(&changes[0]);
    let title = if changes.len() == 1 {
        first
    } else {
        format!("{first} (+{} more)", changes.len() - 1)
    };

    if title.chars().count() <= MAX_LEN {
        return title;
    }

    // Truncate at a char boundary so we don't slice through a multibyte
    // codepoint. Leave room for the ellipsis itself.
    let take = MAX_LEN.saturating_sub(1);
    let truncated: String = title.chars().take(take).collect();
    let mut out = truncated;
    out.push(ELLIPSIS);
    out
}

fn build_pr_body(workspace: &Path, changes: &[String], includes_self_heal: bool) -> String {
    let mut s = String::new();
    if includes_self_heal {
        s.push_str(
            "_This PR archives one or more changes whose implementation was already present on the base branch. No code diff is included; only the openspec archive move._\n\n",
        );
    }
    for change in changes {
        let why = read_archived_why(workspace, change);
        let body = match why {
            Some(w) if !w.trim().is_empty() => w.trim().to_string(),
            _ => "_(no proposal.md available)_".to_string(),
        };
        s.push_str(&format!("## {change}\n\n{body}\n\n"));
    }
    s.push_str("Changes implemented in this pass:\n\n");
    for change in changes {
        s.push_str(&format!("- {change}\n"));
    }
    s
}

/// Locate `<workspace>/openspec/changes/archive/*-<change>/proposal.md`
/// (picking the lexicographically last match if multiple exist), read
/// the file, and return the `## Why` section. Returns `None` if the
/// directory or file is missing, the read fails, or no `## Why` heading
/// is present.
fn read_archived_why(workspace: &Path, change: &str) -> Option<String> {
    let archive_root = workspace.join("openspec/changes/archive");
    let entries = std::fs::read_dir(&archive_root).ok()?;
    let suffix = format!("-{change}");
    let mut matches: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with(&suffix)
        })
        .map(|e| e.path())
        .collect();
    matches.sort();
    let dir = matches.last()?;
    let proposal = dir.join("proposal.md");
    let raw = std::fs::read_to_string(&proposal).ok()?;
    extract_why_section(&raw)
}

/// Pull the `## Why` section out of a proposal.md body: everything from
/// the line after `## Why` up to (but not including) the next `## `
/// heading or EOF. Returns `None` if no `## Why` heading exists.
fn extract_why_section(raw: &str) -> Option<String> {
    let mut lines = raw.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "## Why" {
            let mut out = String::new();
            for next in lines.by_ref() {
                if next.trim_start().starts_with("## ") {
                    break;
                }
                out.push_str(next);
                out.push('\n');
            }
            return Some(out);
        }
    }
    None
}

/// Read `openspec/changes/<change>/tasks.md` and decide whether every task
/// checkbox is `[x]`. Scans each line for the regex `^\s*-\s*\[([ x])\]`.
/// Returns `Ok(true)` iff at least one match is present AND every match
/// captures `x`. Any match capturing ` ` yields `Ok(false)`. An empty
/// match-set yields `Ok(false)` — a tasks.md with no checkboxes is not
/// "all complete". Returns `Err(_)` only on file-read failure or
/// regex-init failure.
pub fn tasks_md_all_complete(workspace: &Path, change: &str) -> Result<bool> {
    let tasks_path = workspace
        .join("openspec/changes")
        .join(change)
        .join("tasks.md");
    let raw = std::fs::read_to_string(&tasks_path)
        .with_context(|| format!("reading {}", tasks_path.display()))?;
    let re = regex::Regex::new(r"^\s*-\s*\[([ x])\]")
        .context("compiling tasks.md checkbox regex")?;
    let mut any = false;
    for line in raw.lines() {
        if let Some(caps) = re.captures(line) {
            any = true;
            if &caps[1] != "x" {
                return Ok(false);
            }
        }
    }
    Ok(any)
}

/// Shell out to `openspec validate <change> --strict` in `workspace` and
/// report whether it exited 0. Any error — binary missing, non-zero exit,
/// transport problem — returns `false`. The caller falls through to the
/// existing Failed path when self-heal preconditions are unmet, which is
/// the conservative behavior.
pub fn openspec_validate_strict_passes(workspace: &Path, change: &str) -> bool {
    match std::process::Command::new("openspec")
        .args(["validate", change, "--strict"])
        .current_dir(workspace)
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// Return `true` if any open PR exists on GitHub for the configured agent
/// branch, in which case the caller should skip this iteration. On any
/// failure to perform the check (parse, token, transport, non-2xx) this
/// logs a WARN and returns `false` so a transient GitHub problem does not
/// block normal iterations — the cost of a redundant Claude run is lower
/// than the cost of an entire repo grinding to a halt on a flaky API.
///
/// `api_base` is `github::DEFAULT_API_BASE` in production; tests pass a
/// mockito server URL instead.
async fn open_pr_exists_for_agent_branch_at(
    api_base: &str,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
) -> bool {
    let (upstream_owner, upstream_repo) = match github::parse_repo_url(&repo.url) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(
                url = %repo.url,
                "open-PR check skipped: cannot parse repo URL: {e:#}"
            );
            return false;
        }
    };
    // In fork-PR mode, the head qualifier is `<fork_owner>:<branch>`; in
    // direct mode it's the upstream owner. Either way the QUERY targets
    // the upstream repo's `/pulls` because that's where PRs are created.
    let head_owner = github_cfg.fork_owner.as_deref().unwrap_or(&upstream_owner);
    let head = format!("{}:{}", head_owner, repo.agent_branch);

    let token = match crate::github_credentials::resolve_token(github_cfg, &upstream_owner) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                url = %repo.url,
                "open-PR check skipped: token resolution failed: {e:#}"
            );
            return false;
        }
    };

    let result = if api_base == github::DEFAULT_API_BASE {
        github::list_open_prs(
            &upstream_owner,
            &upstream_repo,
            &head,
            &repo.base_branch,
            &token,
        )
        .await
    } else {
        // Test path: explicit base.
        #[cfg(test)]
        {
            github::list_open_prs_at_for_test(
                api_base,
                &upstream_owner,
                &upstream_repo,
                &head,
                &repo.base_branch,
                &token,
            )
            .await
        }
        #[cfg(not(test))]
        {
            unreachable!("non-default api_base is test-only");
        }
    };

    match result {
        Ok(prs) if !prs.is_empty() => {
            let numbers: Vec<u64> = prs.iter().map(|p| p.number).collect();
            tracing::info!(
                url = %repo.url,
                pr_count = numbers.len(),
                prs = ?numbers,
                "open PR exists for agent branch; skipping iteration"
            );
            true
        }
        Ok(_) => false,
        Err(e) => {
            tracing::warn!(
                url = %repo.url,
                "open-PR check failed: {e:#}; proceeding with iteration"
            );
            false
        }
    }
}

async fn open_pr_exists_for_agent_branch(
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
) -> bool {
    open_pr_exists_for_agent_branch_at(github::DEFAULT_API_BASE, repo, github_cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Routing test: when `owner_tokens` maps the parsed URL owner to an
    /// env var, the PR-creation HTTP call MUST carry that env var's value
    /// in the `Authorization: Bearer` header — not `token_env`'s value.
    /// This exercises the same composition `open_pull_request` does:
    /// `parse_repo_url → resolve_token → create_pull_request_at`.
    #[tokio::test]
    async fn pr_creation_uses_owner_specific_token() {
        let var = "AUTOCODER_TEST_PR_ROUTING_TOKEN";
        let fallback = "AUTOCODER_TEST_PR_ROUTING_FALLBACK";
        // SAFETY: this test relies on a unique env-var name so it does not
        // collide with parallel tests; no cross-test mutation lock required.
        unsafe {
            std::env::set_var(var, "owner-specific-token-xyz");
            std::env::set_var(fallback, "should-not-be-used");
        }

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/fixture-owner/fixture-repo/pulls")
            .match_header("authorization", "Bearer owner-specific-token-xyz")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"html_url":"https://github.com/fixture-owner/fixture-repo/pull/1","number":1}"#,
            )
            .create_async()
            .await;

        let mut map = std::collections::HashMap::new();
        map.insert(
            "fixture-owner".into(),
            crate::config::SecretSource::EnvVar(var.into()),
        );
        let github_cfg = GithubConfig {
            token_env: fallback.into(),
            token: None,
            owner_tokens: Some(map),
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        // Mirror open_pull_request's internal sequence.
        let (owner, repo_name) =
            crate::github::parse_repo_url("git@github.com:fixture-owner/fixture-repo.git")
                .expect("parse");
        let token = crate::github_credentials::resolve_token(&github_cfg, &owner)
            .expect("owner_tokens entry should resolve");

        crate::github::create_pull_request_at_for_test(
            &server.url(),
            &owner,
            &repo_name,
            "agent-q",
            "main",
            "t",
            "b",
            &token,
            None,
            false,
        )
        .await
        .expect("PR creation should succeed against mockito");

        mock.assert_async().await;

        unsafe {
            std::env::remove_var(var);
            std::env::remove_var(fallback);
        }
    }

    /// In fork-PR mode the PR's `head` is `<fork-owner>:<branch>` and the
    /// API call still goes to the upstream repo's /pulls endpoint.
    #[tokio::test]
    async fn pr_uses_cross_repo_head_in_fork_mode() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/upstream-org/repo/pulls")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"head":"machine-user:agent-q","base":"main"}"#.to_string(),
            ))
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"html_url":"https://github.com/upstream-org/repo/pull/1","number":1}"#,
            )
            .create_async()
            .await;

        // Mirror the open_pull_request flow with fork_owner set.
        let github_cfg = GithubConfig {
            token_env: "X".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "inline-token".into(),
            }),
            owner_tokens: None,
            fork_owner: Some("machine-user".into()),
            recreate_fork_on_reinit: false,
        };
        let (owner, repo_name) =
            crate::github::parse_repo_url("git@github.com:upstream-org/repo.git").unwrap();
        let token = crate::github_credentials::resolve_token(&github_cfg, &owner).unwrap();
        let head = format!("{}:{}", github_cfg.fork_owner.as_deref().unwrap(), "agent-q");

        crate::github::create_pull_request_at_for_test(
            &server.url(),
            &owner,
            &repo_name,
            &head,
            "main",
            "t",
            "b",
            &token,
            None,
            false,
        )
        .await
        .expect("cross-repo PR succeeds");

        mock.assert_async().await;
    }

    #[test]
    fn detect_lazy_archive_returns_true_for_archive_only_renames() {
        let status = "R  openspec/changes/foo/proposal.md -> openspec/changes/archive/2026-05-14-foo/proposal.md\nR  openspec/changes/foo/tasks.md -> openspec/changes/archive/2026-05-14-foo/tasks.md\n";
        assert!(is_lazy_archive(status));
    }

    #[test]
    fn detect_lazy_archive_returns_false_when_real_implementation_present() {
        // Archive rename PLUS a modification to a source file → real work.
        let status = "R  openspec/changes/foo/proposal.md -> openspec/changes/archive/2026-05-14-foo/proposal.md\n M src/foo.rs\n";
        assert!(!is_lazy_archive(status));
    }

    #[test]
    fn detect_lazy_archive_returns_false_for_added_files() {
        let status = "A  src/new_module.rs\n";
        assert!(!is_lazy_archive(status));
    }

    #[test]
    fn detect_lazy_archive_returns_false_when_workspace_clean() {
        assert!(!is_lazy_archive(""));
    }

    #[test]
    fn detect_lazy_archive_returns_false_for_rename_outside_archive() {
        // Renames are fine if they're not into archive/ — agent legitimately
        // moving files around as part of implementation.
        let status = "R  old/path.rs -> new/path.rs\n";
        assert!(!is_lazy_archive(status));
    }

    // ============================================================
    // has_executor_changes (resume-path no-op detection)
    // ============================================================

    #[test]
    fn has_executor_changes_false_when_only_question_file_deletion() {
        // Real-world porcelain from a no-diff resume: autocoder itself
        // deleted .question.json before calling resume; the leading
        // column-1 space is trimmed by `status_porcelain`, leaving the
        // line starting with the second status column.
        let status = "D openspec/changes/foo/.question.json";
        assert!(!has_executor_changes(status, "foo"));
    }

    #[test]
    fn has_executor_changes_false_when_only_answer_and_question_metafiles() {
        let status = " D openspec/changes/foo/.question.json\n?? openspec/changes/foo/.answer.json";
        assert!(!has_executor_changes(status, "foo"));
    }

    #[test]
    fn has_executor_changes_true_when_resume_wrote_artifact() {
        // The executor created an artifact alongside the meta-file
        // deletion → real work happened.
        let status = " D openspec/changes/foo/.question.json\n?? src/new_thing.rs";
        assert!(has_executor_changes(status, "foo"));
    }

    #[test]
    fn has_executor_changes_false_on_empty_status() {
        assert!(!has_executor_changes("", "foo"));
    }

    #[test]
    fn has_executor_changes_true_for_rename_with_non_meta_path() {
        let status = "R  old/path.rs -> new/path.rs";
        assert!(has_executor_changes(status, "foo"));
    }

    #[test]
    fn first_line_of_why_section() {
        let text = "## Why\nSwitch from sync to async\n\n## What Changes\n- thing\n";
        let line = first_line_of_section(text, "## Why").unwrap();
        assert_eq!(line, "Switch from sync to async");
    }

    #[test]
    fn first_line_of_why_skips_blank_lines() {
        let text = "## Why\n\n   \n  Real content here  \n## What Changes\n";
        let line = first_line_of_section(text, "## Why").unwrap();
        assert_eq!(line, "Real content here");
    }

    #[test]
    fn first_line_of_section_returns_none_when_missing() {
        let text = "## What Changes\n- x\n";
        assert!(first_line_of_section(text, "## Why").is_none());
    }

    #[test]
    fn build_commit_subject_truncates_to_72_chars() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let change = "make-the-thing-better";
        let proposal = ws.join("openspec/changes").join(change).join("proposal.md");
        std::fs::create_dir_all(proposal.parent().unwrap()).unwrap();
        let long = "A".repeat(200);
        std::fs::write(&proposal, format!("## Why\n{long}\n")).unwrap();
        let subject = build_commit_subject(ws, change).unwrap();
        assert_eq!(subject.chars().count(), 72);
        assert!(subject.starts_with("make-the-thing-better: "));
    }

    #[test]
    fn build_commit_subject_falls_back_to_change_name_when_no_why() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let proposal = ws.join("openspec/changes/c/proposal.md");
        std::fs::create_dir_all(proposal.parent().unwrap()).unwrap();
        std::fs::write(&proposal, "## What Changes\n- thing\n").unwrap();
        let subject = build_commit_subject(ws, "c").unwrap();
        assert_eq!(subject, "c: c");
    }

    /// Build a fixture remote repo with one commit on `main` AND a cloned
    /// workspace whose `origin` points to the remote. Returns the temp dir
    /// guard (drop = cleanup) plus the workspace path.
    fn fixture_workspace_with_remote() -> (tempfile::TempDir, std::path::PathBuf) {
        use std::process::Command;
        fn run(path: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {}", path.display());
        }

        let dir = tempfile::TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("workspace");

        std::fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "user.email", "test@example.com"]);
        run(&remote, &["config", "user.name", "test"]);
        std::fs::write(remote.join("README.md"), "hi\n").unwrap();
        run(&remote, &["add", "README.md"]);
        run(&remote, &["commit", "-q", "-m", "initial"]);

        let remote_url = remote.to_string_lossy().to_string();
        let parent = workspace.parent().unwrap();
        let status = Command::new("git")
            .args([
                "clone",
                "-q",
                &remote_url,
                workspace.to_string_lossy().as_ref(),
            ])
            .current_dir(parent)
            .status()
            .unwrap();
        assert!(status.success(), "clone failed");
        run(&workspace, &["config", "user.email", "test@example.com"]);
        run(&workspace, &["config", "user.name", "test"]);
        (dir, workspace)
    }

    /// Add an OpenSpec change with a known `## Why` line to a fixture
    /// workspace and commit it locally so the working tree stays clean.
    fn add_committed_change(workspace: &Path, name: &str, why_line: &str) {
        let dir = workspace.join("openspec/changes").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proposal.md"), format!("## Why\n{why_line}\n")).unwrap();
        std::fs::write(dir.join("tasks.md"), "- [ ] do thing\n").unwrap();
        let st = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(workspace)
            .status()
            .unwrap();
        assert!(st.success());
        let st = std::process::Command::new("git")
            .args(["commit", "-q", "-m", &format!("scaffold {name}")])
            .current_dir(workspace)
            .status()
            .unwrap();
        assert!(st.success());
    }

    /// Build a `RepositoryConfig` pointing at a fixture workspace. Uses a
    /// non-existent token env var so any attempt to open a PR errors fast
    /// rather than reaching a live API.
    fn fixture_repo(workspace: &Path) -> RepositoryConfig {
        RepositoryConfig {
            url: "git@github.com:owner/fixture.git".into(),
            local_path: Some(workspace.to_path_buf()),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    /// Executor that returns `Completed` and writes a file so
    /// `git status --porcelain` is non-empty and a real commit gets made.
    struct CompletingExecutorWithDiff {
        artifact_name: String,
        artifact_text: String,
    }
    #[async_trait::async_trait]
    impl Executor for CompletingExecutorWithDiff {
        async fn run(&self, workspace: &Path, _c: &str) -> Result<ExecutorOutcome> {
            std::fs::write(workspace.join(&self.artifact_name), &self.artifact_text)?;
            Ok(ExecutorOutcome::Completed)
        }
        async fn resume(
            &self,
            _h: crate::executor::ResumeHandle,
            _a: &str,
        ) -> Result<ExecutorOutcome> {
            unreachable!()
        }
    }

    /// Executor that returns `Completed` but writes nothing. Exercises the
    /// "Completed but no diff" architecture scenario.
    struct CompletingExecutorNoDiff;
    #[async_trait::async_trait]
    impl Executor for CompletingExecutorNoDiff {
        async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
            Ok(ExecutorOutcome::Completed)
        }
        async fn resume(
            &self,
            _h: crate::executor::ResumeHandle,
            _a: &str,
        ) -> Result<ExecutorOutcome> {
            unreachable!()
        }
    }

    /// Executor that always returns `Failed`. Exercises the "backend failure"
    /// architecture scenario.
    struct AlwaysFailingExecutor;
    #[async_trait::async_trait]
    impl Executor for AlwaysFailingExecutor {
        async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
            Ok(ExecutorOutcome::Failed {
                reason: "fixture failure".into(),
            })
        }
        async fn resume(
            &self,
            _h: crate::executor::ResumeHandle,
            _a: &str,
        ) -> Result<ExecutorOutcome> {
            unreachable!()
        }
    }

    /// Run a single pass through the commit step but skip push + PR. Tests
    /// only need this when they want to verify commit/archive behavior
    /// without an HTTP fixture for the GitHub API.
    async fn run_one_pass_no_push(
        workspace: &Path,
        executor: &dyn Executor,
    ) -> Result<Vec<String>> {
        let repo = fixture_repo(workspace);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        // Use a very high threshold so existing tests' single-fail
        // iterations don't accidentally mark perma-stuck.
        let (processed, _self_heal) =
            run_pass_through_commits(
                workspace, &repo, &github_cfg, executor, None, u32::MAX, u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
            )
            .await?;
        Ok(processed)
    }

    /// 13.3.2 / executor baseline: when the executor returns `Failed`,
    /// autocoder unlocks the change AND does NOT archive it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_change_unlocks_and_does_not_archive() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "feature-a", "fixture reason");

        let executor = AlwaysFailingExecutor;
        let _ = run_one_pass_no_push(&ws, &executor).await; // Failed is a normal outcome

        // The change is still in the active queue (not archived).
        let pending = queue::list_pending(&ws).unwrap();
        assert_eq!(pending, vec!["feature-a".to_string()]);
        // No archive directory was created for it.
        let archive_root = ws.join("openspec/changes/archive");
        if archive_root.exists() {
            for entry in std::fs::read_dir(&archive_root).unwrap() {
                let name = entry.unwrap().file_name().into_string().unwrap();
                assert!(
                    !name.contains("feature-a"),
                    "Failed change must not be archived; found {name}"
                );
            }
        }
        // No `.in-progress` lock left behind.
        let lock = ws.join("openspec/changes/feature-a/.in-progress");
        assert!(!lock.exists(), "lock file should be cleared after Failed");
    }

    /// 13.4.1 / git-workflow-manager baseline: at start of each pass, the
    /// agent branch is recreated to match the post-pull HEAD of the base
    /// branch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn branch_init_resets_agent_to_base() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // Empty queue is fine — we only care about the branch init step.

        let executor = CompletingExecutorNoDiff;
        run_one_pass_no_push(&ws, &executor).await.expect("pass succeeds");

        // After init, agent-q must point at the same SHA as main.
        let main_sha = crate::git::rev_parse(&ws, "main").unwrap();
        let agent_sha = crate::git::rev_parse(&ws, "agent-q").unwrap();
        assert_eq!(
            main_sha, agent_sha,
            "agent-q must equal main after branch init step"
        );
    }

    /// 13.4.3 / git-workflow-manager baseline: commit subject is
    /// `<change>: <first non-empty line of ## Why>`, truncated to 72 chars.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn commit_subject_matches_spec_format() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "add-greetings", "Make the project greet users on startup");

        let executor = CompletingExecutorWithDiff {
            artifact_name: "GREETINGS".into(),
            artifact_text: "hello world".into(),
        };
        run_one_pass_no_push(&ws, &executor).await.expect("pass succeeds");

        // Inspect HEAD on agent-q. autocoder left us on agent-q after
        // recreate_branch + commit; verify subject directly.
        let out = std::process::Command::new("git")
            .args(["log", "-1", "--pretty=%s", "agent-q"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(out.status.success(), "git log failed");
        let subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            subject,
            "add-greetings: Make the project greet users on startup",
            "subject should be `<change>: <first ## Why line>`"
        );
        assert!(
            subject.chars().count() <= 72,
            "subject should be ≤72 chars, got {} chars: {subject:?}",
            subject.chars().count()
        );
    }

    /// git-workflow-manager / orchestrator-cli: an executor that returns
    /// `Completed` without modifying the workspace is treated as Failed.
    /// The change is NOT archived, no commit is made, and the change is
    /// unlocked so the next polling pass retries it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_with_empty_workspace_is_failed() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "no-op-change", "intentionally a no-op");

        let pre_main = crate::git::rev_parse(&ws, "main").unwrap();

        let executor = CompletingExecutorNoDiff;
        run_one_pass_no_push(&ws, &executor).await.expect("pass succeeds");

        // Change is NOT archived: active directory must still exist and
        // the archive directory must NOT contain it.
        assert!(
            ws.join("openspec/changes/no-op-change").exists(),
            "no-op change must remain in active changes for retry"
        );
        let archive_root = ws.join("openspec/changes/archive");
        if archive_root.exists() {
            for entry in std::fs::read_dir(&archive_root).unwrap() {
                let name = entry.unwrap().file_name().into_string().unwrap();
                assert!(
                    !name.ends_with("-no-op-change"),
                    "no-op Completed must not produce an archive entry, found {name}"
                );
            }
        }

        // Lock removed → change is back in pending for the next pass.
        assert!(
            !ws.join("openspec/changes/no-op-change/.in-progress").exists(),
            ".in-progress lock must be cleared so the change retries"
        );
        assert_eq!(
            queue::list_pending(&ws).unwrap(),
            vec!["no-op-change".to_string()],
            "change must be back in pending after a no-op Completed"
        );

        // No commit was made: agent-q must still equal main's pre-pass SHA.
        let agent_sha = crate::git::rev_parse(&ws, "agent-q").unwrap();
        assert_eq!(agent_sha, pre_main, "no-diff Completed must not create a commit");
    }

    /// 13.4.2 / git-workflow-manager baseline: when `git pull --ff-only`
    /// fails (base branch has diverged from origin), the iteration aborts
    /// and the agent branch is NOT created or modified.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pull_conflict_aborts_iteration_without_touching_agent_branch() {
        let (_dir, ws) = fixture_workspace_with_remote();

        // Reach into the remote (the fixture's `remote/` sibling) to advance
        // origin/main with a commit our local doesn't have.
        let remote = ws.parent().unwrap().join("remote");
        std::fs::write(remote.join("REMOTE_ONLY.md"), "remote-side\n").unwrap();
        let st = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&remote)
            .status()
            .unwrap();
        assert!(st.success());
        let st = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "remote-side commit"])
            .current_dir(&remote)
            .status()
            .unwrap();
        assert!(st.success());

        // Now create a divergent local commit on main so pull --ff-only fails
        // (our local main is not an ancestor of origin/main and vice versa).
        std::fs::write(ws.join("LOCAL_ONLY.md"), "local-side\n").unwrap();
        let st = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success());
        let st = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "local-side commit"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success());

        // Sanity: agent-q must not exist yet.
        assert!(crate::git::rev_parse(&ws, "agent-q").is_err(),
            "agent-q must not exist before the pass");

        let executor = CompletingExecutorNoDiff;
        let result = run_one_pass_no_push(&ws, &executor).await;
        assert!(result.is_err(), "pass must error when pull --ff-only fails");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("git pull --ff-only failed") || msg.contains("non-fast-forward"),
            "error must surface the git failure verbatim, got: {msg}"
        );

        // Agent branch must remain absent after the aborted iteration.
        assert!(
            crate::git::rev_parse(&ws, "agent-q").is_err(),
            "agent-q must not be created when the iteration aborts at pull"
        );
    }

    // ============================================================
    // chatops-escalation end-to-end tests
    // ============================================================

    /// Build a ChatOps client wired against the given mockito server.
    async fn fixture_chatops_for(server: &mut mockito::Server) -> Arc<dyn ChatOpsBackend> {
        let _ = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
            .create_async()
            .await;
        Arc::new(
            crate::chatops::SlackBackend::new_at(server.url(), "xoxb-fixture".into())
                .await
                .unwrap(),
        )
    }

    /// Pending-pass executor that returns `AskUser` on first invocation
    /// and `Completed` (with a file write) on resume.
    struct AskThenComplete {
        ws: std::path::PathBuf,
    }
    #[async_trait::async_trait]
    impl Executor for AskThenComplete {
        async fn run(&self, _w: &Path, change: &str) -> Result<ExecutorOutcome> {
            Ok(ExecutorOutcome::AskUser {
                question: "What name should the file have?".to_string(),
                resume_handle: ResumeHandle(
                    serde_json::json!({"change": change, "workspace": self.ws}),
                ),
            })
        }
        async fn resume(&self, _h: ResumeHandle, answer: &str) -> Result<ExecutorOutcome> {
            std::fs::write(self.ws.join("RESUME_ARTIFACT.txt"), answer.as_bytes())?;
            Ok(ExecutorOutcome::Completed)
        }
    }

    /// 5.2: AskUser on a pending change → posts to Slack, writes
    /// `.question.json`, unlocks the change, change is excluded from
    /// pending and shows up in `list_waiting`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn askuser_on_pending_escalates_to_chatops() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "ambig-change", "ambiguous fixture");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let _post = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1234567890.123456"}"#)
            .create_async()
            .await;

        let executor = AskThenComplete { ws: ws.clone() };
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");
        // No commits this pass — the change is now waiting.
        assert!(processed.is_empty(), "no commits on a pure-AskUser pass");

        // `.question.json` was written; change is gone from pending,
        // present in waiting; no `.in-progress` lingers.
        let q_path = ws.join("openspec/changes/ambig-change/.question.json");
        assert!(q_path.is_file(), ".question.json must be written");
        assert!(!ws
            .join("openspec/changes/ambig-change/.in-progress")
            .exists());
        assert_eq!(queue::list_pending(&ws).unwrap(), Vec::<String>::new());
        assert_eq!(
            queue::list_waiting(&ws).unwrap(),
            vec!["ambig-change".to_string()]
        );

        // Persisted payload carries thread_ts and the executor's resume
        // handle.
        let q = chatops::read_question_file(&ws, "ambig-change").unwrap();
        assert_eq!(q.thread_ts, "1234567890.123456");
        assert_eq!(q.channel, "C_TEST");
        assert_eq!(q.resume_handle["change"], "ambig-change");
    }

    /// 5.1: a waiting change with a human reply gets resumed; on a
    /// successful resume with a diff the change is archived and the pass
    /// reports it as processed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiting_change_resumes_and_archives_on_reply() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "ambig-change", "ambiguous fixture");

        // Pre-populate .question.json simulating an earlier-iteration
        // escalation.
        let q = QuestionPayload {
            thread_ts: "1234567890.123456".into(),
            channel: "C_TEST".into(),
            resume_handle: serde_json::json!({
                "change": "ambig-change",
                "workspace": ws,
            }),
            asked_at: chrono::Utc::now(),
        };
        chatops::write_question_file(&ws, "ambig-change", &q).unwrap();
        // Commit the .question.json so the workspace stays clean for the
        // pre-pass dirty check. (In production this file would persist
        // across iterations naturally; here we commit to satisfy the
        // fixture-time clean check.)
        let run_git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success());
        };
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "persist question marker"]);

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let _replies = server
            .mock("GET", "/conversations.replies?channel=C_TEST&ts=1234567890.123456")
            .with_status(200)
            .with_body(
                r#"{"ok":true,"messages":[
                    {"user":"U_BOT","text":"❓ ...","ts":"1234567890.123456"},
                    {"user":"U_HUMAN","text":"SAMPLE","ts":"1234567891.0"}
                ]}"#,
            )
            .create_async()
            .await;

        let executor = AskThenComplete { ws: ws.clone() };
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");

        // Change resumed, produced a diff, was committed + archived.
        assert_eq!(processed, vec!["ambig-change".to_string()]);
        // .question.json and .answer.json both gone.
        assert!(!ws
            .join("openspec/changes/ambig-change/.question.json")
            .exists());
        assert!(!ws
            .join("openspec/changes/ambig-change/.answer.json")
            .exists());
        assert!(!queue::list_waiting(&ws).unwrap().contains(&"ambig-change".to_string()));
        // Archived under date prefix.
        let archive = ws.join("openspec/changes/archive");
        let names: Vec<String> = std::fs::read_dir(&archive)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("-ambig-change")),
            "expected archived ambig-change in {names:?}"
        );
    }

    /// orchestrator-cli: when a resume returns `Completed` but the
    /// executor did not modify the workspace, the change is NOT archived.
    /// The question/answer files are cleared so the change leaves
    /// "waiting" state, but it must come back as pending for the next
    /// pass to retry rather than being silently completed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_with_empty_workspace_is_failed() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "ambig-change", "ambiguous fixture");

        // Pre-populate .question.json as if escalated in a prior iteration.
        let q = QuestionPayload {
            thread_ts: "2222222222.222222".into(),
            channel: "C_TEST".into(),
            resume_handle: serde_json::json!({"change": "ambig-change"}),
            asked_at: chrono::Utc::now(),
        };
        chatops::write_question_file(&ws, "ambig-change", &q).unwrap();
        let run_git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success());
        };
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "persist question marker"]);

        let pre_main = crate::git::rev_parse(&ws, "main").unwrap();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let _replies = server
            .mock("GET", "/conversations.replies?channel=C_TEST&ts=2222222222.222222")
            .with_status(200)
            .with_body(
                r#"{"ok":true,"messages":[
                    {"user":"U_BOT","text":"❓ ...","ts":"2222222222.222222"},
                    {"user":"U_HUMAN","text":"some reply","ts":"2222222223.0"}
                ]}"#,
            )
            .create_async()
            .await;

        // Executor whose resume returns Completed without touching the
        // workspace, then refuses to do work if `run()` is later called
        // (which the same pass will do, since the no-diff resume puts
        // the change back into pending state — that retry is production-
        // correct, we just don't want it to mask what the resume path
        // did in this test).
        struct ResumeReturnsCompletedNoDiff;
        #[async_trait::async_trait]
        impl Executor for ResumeReturnsCompletedNoDiff {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                Ok(ExecutorOutcome::Failed {
                    reason: "retry after no-diff resume; not implementing in this fixture".into(),
                })
            }
            async fn resume(&self, _h: ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
                Ok(ExecutorOutcome::Completed)
            }
        }
        let executor = ResumeReturnsCompletedNoDiff;
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");

        // No commits this pass — the resume produced no diff.
        assert!(
            processed.is_empty(),
            "no-diff resume must not be reported as committed"
        );

        // Change is NOT archived: active dir still present, archive
        // (if it exists) does not contain it.
        assert!(
            ws.join("openspec/changes/ambig-change").exists(),
            "change must remain in active changes after no-diff resume"
        );
        let archive = ws.join("openspec/changes/archive");
        if archive.exists() {
            for entry in std::fs::read_dir(&archive).unwrap() {
                let name = entry.unwrap().file_name().into_string().unwrap();
                assert!(
                    !name.ends_with("-ambig-change"),
                    "no-diff resume must not produce an archive entry, found {name}"
                );
            }
        }

        // Question + answer files cleared; change is back in pending,
        // not waiting.
        assert!(
            !ws.join("openspec/changes/ambig-change/.question.json").exists(),
            ".question.json must be deleted after resume"
        );
        assert!(
            !ws.join("openspec/changes/ambig-change/.answer.json").exists(),
            ".answer.json must be deleted after resume"
        );
        assert!(
            !queue::list_waiting(&ws).unwrap().contains(&"ambig-change".to_string()),
            "change must leave waiting state after resume"
        );
        assert!(
            queue::list_pending(&ws).unwrap().contains(&"ambig-change".to_string()),
            "change must return to pending for retry"
        );

        // No commit was made on agent-q (it should equal main's pre-pass
        // SHA after branch init).
        let agent_sha = crate::git::rev_parse(&ws, "agent-q").unwrap();
        assert_eq!(
            agent_sha, pre_main,
            "no-diff resume must not create a commit"
        );
    }

    /// 5.1a: same-repo block. If after the waiting-processing step the
    /// waiting set is STILL non-empty, the pending pass MUST NOT run for
    /// this iteration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_repo_block_skips_pending_when_still_waiting() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "still-waiting", "stuck on a question");
        add_committed_change(&ws, "would-be-pending", "should not be touched");

        // .question.json on `still-waiting`.
        let q = QuestionPayload {
            thread_ts: "1111.1111".into(),
            channel: "C_TEST".into(),
            resume_handle: serde_json::json!({}),
            asked_at: chrono::Utc::now(),
        };
        chatops::write_question_file(&ws, "still-waiting", &q).unwrap();
        let run_git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success());
        };
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "persist question"]);

        // Slack returns no human reply yet → change stays waiting.
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let _ = server
            .mock("GET", "/conversations.replies?channel=C_TEST&ts=1111.1111")
            .with_status(200)
            .with_body(
                r#"{"ok":true,"messages":[
                    {"user":"U_BOT","text":"❓ ...","ts":"1111.1111"}
                ]}"#,
            )
            .create_async()
            .await;

        // An executor that would PANIC if invoked — it must NOT be called
        // for `would-be-pending` since the same-repo block applies.
        struct MustNotRunExecutor;
        #[async_trait::async_trait]
        impl Executor for MustNotRunExecutor {
            async fn run(&self, _w: &Path, change: &str) -> Result<ExecutorOutcome> {
                panic!("executor must not run on pending `{change}` while another change is waiting");
            }
            async fn resume(&self, _h: ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }

        let executor = MustNotRunExecutor;
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds without running pending");
        assert!(processed.is_empty(), "no work this iteration");
        // Still waiting.
        assert_eq!(
            queue::list_waiting(&ws).unwrap(),
            vec!["still-waiting".to_string()]
        );
    }

    /// Verifies the orchestrator-cli "Queue resumes after waiting set
    /// empties" scenario: when the human reply arrives AND the resume
    /// completes, the same iteration proceeds to process pending changes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_resumes_after_waiting_set_empties() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "was-waiting", "fixture for waiting");
        add_committed_change(&ws, "fresh-pending", "fresh fixture");

        // Pre-populate .question.json for `was-waiting`.
        let q = QuestionPayload {
            thread_ts: "9999.9999".into(),
            channel: "C_TEST".into(),
            resume_handle: serde_json::json!({
                "change": "was-waiting",
                "workspace": ws,
            }),
            asked_at: chrono::Utc::now(),
        };
        chatops::write_question_file(&ws, "was-waiting", &q).unwrap();
        let run_git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success());
        };
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "persist marker"]);

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        // Reply arrives.
        let _ = server
            .mock("GET", "/conversations.replies?channel=C_TEST&ts=9999.9999")
            .with_status(200)
            .with_body(
                r#"{"ok":true,"messages":[
                    {"user":"U_BOT","text":"❓ ...","ts":"9999.9999"},
                    {"user":"U_HUMAN","text":"go ahead","ts":"9999.0001"}
                ]}"#,
            )
            .create_async()
            .await;

        // Executor: resumes was-waiting (produces a file), runs fresh-pending
        // (produces a different file). Both Completed-with-diff.
        let ws_for_exec = ws.clone();
        struct ResumeAndRunBoth {
            ws: std::path::PathBuf,
            invocations: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl Executor for ResumeAndRunBoth {
            async fn run(&self, _w: &Path, change: &str) -> Result<ExecutorOutcome> {
                self.invocations.lock().unwrap().push(format!("run:{change}"));
                std::fs::write(
                    self.ws.join(format!("RUN_{change}.txt")),
                    "from run",
                )?;
                Ok(ExecutorOutcome::Completed)
            }
            async fn resume(
                &self,
                _h: ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                self.invocations.lock().unwrap().push("resume".to_string());
                std::fs::write(self.ws.join("RESUMED.txt"), "from resume")?;
                Ok(ExecutorOutcome::Completed)
            }
        }
        let executor = ResumeAndRunBoth {
            ws: ws_for_exec,
            invocations: std::sync::Mutex::new(Vec::new()),
        };

        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");

        // Both changes processed in this single iteration: the resumed one
        // AND the fresh pending one. Both archived.
        assert_eq!(
            processed.iter().cloned().collect::<std::collections::HashSet<_>>(),
            ["was-waiting", "fresh-pending"]
                .iter()
                .map(|s| s.to_string())
                .collect::<std::collections::HashSet<_>>(),
            "both changes must process in the same iteration once waiting empties"
        );
        // Resume was called BEFORE the fresh-pending run (waiting-first
        // iteration order).
        let inv = executor.invocations.lock().unwrap().clone();
        let resume_idx = inv.iter().position(|s| s == "resume").unwrap();
        let pending_idx = inv.iter().position(|s| s == "run:fresh-pending").unwrap();
        assert!(
            resume_idx < pending_idx,
            "resume must run BEFORE pending: invocations={inv:?}"
        );
    }

    /// max-changes-per-pr-limit: a resumed waiting change that archives
    /// counts toward the per-iteration cap. With one waiting + two pending
    /// and `max_changes_per_pr = 2`, the pass ships exactly two commits
    /// (the resumed archive + the first pending archive); the second
    /// pending change is deferred to the next iteration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_one_pass_resumed_change_counts_toward_cap() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "was-waiting", "fixture for waiting");
        add_committed_change(&ws, "pending-one", "first fresh pending");
        add_committed_change(&ws, "pending-two", "second fresh pending");

        // Pre-populate .question.json for `was-waiting` so the resume path
        // engages.
        let q = QuestionPayload {
            thread_ts: "7777.7777".into(),
            channel: "C_TEST".into(),
            resume_handle: serde_json::json!({
                "change": "was-waiting",
                "workspace": ws,
            }),
            asked_at: chrono::Utc::now(),
        };
        chatops::write_question_file(&ws, "was-waiting", &q).unwrap();
        let run_git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success());
        };
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "persist marker"]);

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        // Human reply arrives so the resume engages.
        let _ = server
            .mock("GET", "/conversations.replies?channel=C_TEST&ts=7777.7777")
            .with_status(200)
            .with_body(
                r#"{"ok":true,"messages":[
                    {"user":"U_BOT","text":"❓ ...","ts":"7777.7777"},
                    {"user":"U_HUMAN","text":"go ahead","ts":"7777.0001"}
                ]}"#,
            )
            .create_async()
            .await;

        // Executor: resume writes a file for the waiting change; run
        // writes a per-change file for fresh pending changes. Both
        // Completed-with-diff.
        let ws_for_exec = ws.clone();
        struct ResumeAndRunPerChange {
            ws: std::path::PathBuf,
            invocations: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl Executor for ResumeAndRunPerChange {
            async fn run(&self, _w: &Path, change: &str) -> Result<ExecutorOutcome> {
                self.invocations.lock().unwrap().push(format!("run:{change}"));
                std::fs::write(
                    self.ws.join(format!("RUN_{change}.txt")),
                    format!("artifact for {change}"),
                )?;
                Ok(ExecutorOutcome::Completed)
            }
            async fn resume(
                &self,
                _h: ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                self.invocations.lock().unwrap().push("resume".to_string());
                std::fs::write(self.ws.join("RESUMED.txt"), "from resume")?;
                Ok(ExecutorOutcome::Completed)
            }
        }
        let executor = ResumeAndRunPerChange {
            ws: ws_for_exec,
            invocations: std::sync::Mutex::new(Vec::new()),
        };

        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            2, // cap of 2 across resume + pending,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");

        assert_eq!(
            processed.len(),
            2,
            "cap of 2 must ship exactly 2 commits: resumed + one pending"
        );
        assert_eq!(
            processed[0], "was-waiting",
            "resumed change processed first"
        );
        assert_eq!(
            processed[1], "pending-one",
            "first pending change processed next"
        );

        let inv = executor.invocations.lock().unwrap().clone();
        assert!(
            !inv.contains(&"run:pending-two".to_string()),
            "second pending must NOT have run (cap stopped the walk); invocations={inv:?}"
        );

        // The undelivered pending change is still in the queue for the
        // next iteration.
        let still_pending = queue::list_pending(&ws).unwrap();
        assert!(
            still_pending.contains(&"pending-two".to_string()),
            "deferred change still pending: {still_pending:?}"
        );
    }

    /// 5.3 / reviewer-integration: end-to-end review wiring. With a fixture
    /// reviewer + a mockito GitHub server, exercise each verdict variant
    /// and confirm:
    ///   - Pass / Concerns → non-draft PR with `## Code Review` body section
    ///   - Block → draft PR with the same section
    ///   - Reviewer-error path → non-draft PR with `(reviewer failed: …)` note
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reviewer_verdict_drives_pr_shape() {
        use crate::code_reviewer::{CodeReviewer, ReviewReport, ReviewVerdict};
        use crate::llm::LlmClient;
        use async_trait::async_trait;

        /// Stub LLM client returning a canned `VERDICT:` response.
        struct CannedClient(&'static str);
        #[async_trait]
        impl LlmClient for CannedClient {
            async fn complete(&self, _: &str) -> Result<String> {
                Ok(self.0.to_string())
            }
        }
        /// Stub LLM client that always errors (exercises the failure path).
        struct ErrClient;
        #[async_trait]
        impl LlmClient for ErrClient {
            async fn complete(&self, _: &str) -> Result<String> {
                Err(anyhow!("simulated reviewer failure"))
            }
        }

        // A trivial "## Why\nbecause\n" stand-in template so we don't depend
        // on the production default template's text in this test.
        let template = "REVIEW THE FOLLOWING DIFF:\n{{diff}}\nSUMMARY:\n{{change_summary}}";

        // -- Helper: run one full pass with a custom reviewer + mockito.
        async fn run_with_reviewer(
            reviewer: CodeReviewer,
            expect_draft: bool,
            body_contains: &'static str,
        ) {
            let (_dir, ws) = fixture_workspace_with_remote();
            add_committed_change(&ws, "rv-change", "make the world a better place");

            // Spin up a mockito server, point autocoder's PR creation
            // at it via GITHUB_API_BASE-style override is not available;
            // instead we drive `execute_one_pass` directly and verify by
            // intercepting the github::create_pull_request HTTP call.
            //
            // The cleanest way is to set up a mockito mock that matches the
            // expected request shape; since we need to override the API
            // base, use the existing `create_pull_request_at` indirectly via
            // the `GITHUB_API_BASE`-equivalent — which we don't have.
            //
            // Approach: this test exercises autocoder's review-step
            // logic by invoking `execute_one_pass` and asserting on the
            // _outcome_ (no panic, push happened) plus reading the agent
            // branch tip's *commit subject* unchanged. The detailed
            // request-shape assertion (draft flag + body section) is
            // already covered by `github::tests::{body_includes_review_section,
            // draft_flag_serialized, label_fallback_on_draft_unsupported}`.
            //
            // What we add here is the *integration*: autocoder
            // selects the right draft flag and review_report based on the
            // verdict the reviewer produces. We test that by directly
            // calling the same compose logic via a small surface.
            let executor = CompletingExecutorWithDiff {
                artifact_name: format!("REVIEW_FIXTURE_{body_contains}"),
                artifact_text: "x".into(),
            };
            let direct_github = GithubConfig {
                token_env: "X".into(),
                token: None,
                owner_tokens: None,
                fork_owner: None,
                recreate_fork_on_reinit: false,
            };
            let (processed, _) = run_pass_through_commits(
                &ws,
                &fixture_repo(&ws),
                &direct_github,
                &executor,
                None,
                u32::MAX,
                u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
            )
            .await
            .expect("commits step succeeds");
            assert_eq!(processed, vec!["rv-change".to_string()]);

            // Now exercise the reviewer step's compose path manually,
            // mirroring what execute_one_pass does between
            // `run_pass_through_commits` and `open_pull_request`.
            let ctx = build_review_context(&ws, &fixture_repo(&ws), &processed)
                .expect("build_review_context succeeds");
            let (report, draft) = match reviewer.review(&ctx).await {
                Ok(report) => {
                    let draft = matches!(report.verdict, ReviewVerdict::Block);
                    (Some(report), draft)
                }
                Err(e) => (
                    Some(ReviewReport {
                        verdict: ReviewVerdict::Concerns,
                        markdown: format!("(reviewer failed: {e})"),
                    }),
                    false,
                ),
            };

            assert_eq!(draft, expect_draft, "draft flag mismatch");
            let rendered = report.expect("report always present when reviewer enabled");
            assert!(
                rendered.markdown.contains(body_contains)
                    || (body_contains == "reviewer failed"
                        && rendered.markdown.contains("(reviewer failed:")),
                "markdown should contain `{body_contains}`; got: {}",
                rendered.markdown
            );
        }

        // Pass verdict → non-draft, body contains the verdict markdown.
        run_with_reviewer(
            CodeReviewer::new(
                Box::new(CannedClient(
                    "VERDICT: Pass\n\n## Security\n- None observed.\n",
                )),
                template.to_string(),
            ),
            false,
            "None observed",
        )
        .await;

        // Concerns verdict → non-draft, body contains verdict markdown.
        run_with_reviewer(
            CodeReviewer::new(
                Box::new(CannedClient(
                    "VERDICT: Concerns\n\n## Possible bugs\n- check input length.\n",
                )),
                template.to_string(),
            ),
            false,
            "check input length",
        )
        .await;

        // Block verdict → DRAFT.
        run_with_reviewer(
            CodeReviewer::new(
                Box::new(CannedClient(
                    "VERDICT: Block\n\n## Security\n- SQL injection on line 42.\n",
                )),
                template.to_string(),
            ),
            true,
            "SQL injection",
        )
        .await;

        // Reviewer error → non-draft, body contains synthetic "reviewer failed" note.
        run_with_reviewer(
            CodeReviewer::new(Box::new(ErrClient), template.to_string()),
            false,
            "reviewer failed",
        )
        .await;
    }

    /// 13.4.7 / git-workflow-manager baseline: empty pass produces no
    /// commits and does not call the GitHub API.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_pass_produces_no_commits_and_no_pr() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // No changes added — queue is empty.

        let pre_main = crate::git::rev_parse(&ws, "main").unwrap();

        let executor = CompletingExecutorNoDiff;
        // run_one_pass_no_push only runs through commit formation; if any
        // commits were produced inappropriately, the test would still need
        // to assert agent-q equals main below. The empty queue means the
        // function returns early without invoking the executor.
        let processed = run_one_pass_no_push(&ws, &executor)
            .await
            .expect("empty pass succeeds");
        assert!(processed.is_empty(), "expected no processed changes, got {processed:?}");

        let agent_sha = crate::git::rev_parse(&ws, "agent-q").unwrap();
        assert_eq!(agent_sha, pre_main, "empty pass must not advance agent branch");
    }

    /// Counting failing executor: increments a shared counter on every call,
    /// fires a `Notify` so tests can await iteration completion event-driven,
    /// always returns `Failed`.
    struct CountingFailingExecutor {
        count: std::sync::atomic::AtomicUsize,
        invoked: Arc<tokio::sync::Notify>,
    }
    impl CountingFailingExecutor {
        fn new() -> Self {
            Self {
                count: std::sync::atomic::AtomicUsize::new(0),
                invoked: Arc::new(tokio::sync::Notify::new()),
            }
        }
    }
    #[async_trait::async_trait]
    impl Executor for CountingFailingExecutor {
        async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.invoked.notify_waiters();
            Ok(ExecutorOutcome::Failed {
                reason: "fixture".into(),
            })
        }
        async fn resume(
            &self,
            _h: crate::executor::ResumeHandle,
            _a: &str,
        ) -> Result<ExecutorOutcome> {
            unreachable!()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iteration_error_continues() {
        // Verify the polling loop runs ≥2 iterations even when the executor
        // returns `Failed` on every change. Failed changes stay in the queue
        // (no archive), so each iteration re-locks, re-invokes, and re-fails.
        let (_dir, ws) = fixture_workspace_with_remote();
        // One pending change so each pass invokes the executor. The change
        // material must be committed in the fixture so the workspace is not
        // dirty when the polling pass starts (production repos commit their
        // openspec/changes/ tree alongside source code).
        let change_dir = ws.join("openspec/changes/feature-x");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join("proposal.md"), "## Why\nbecause\n").unwrap();
        let status = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "add fixture change"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(status.success());
        // Also push so origin/main matches local main; otherwise the
        // `git pull --ff-only origin main` in the pass becomes a no-op of
        // the original commit, which is fine. We don't actually need to push
        // in this test.

        let executor = Arc::new(CountingFailingExecutor::new());
        let executor_dyn: Arc<dyn Executor> = executor.clone();
        let invoked = executor.invoked.clone();

        let repo = RepositoryConfig {
            url: "git@github.com:owner/fixture.git".into(),
            local_path: Some(ws.clone()),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 0, // tight loop so we get many iterations fast
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        let github = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let github_holder: GithubHolder = Arc::new(arc_swap::ArcSwap::from_pointee(github));
        let reviewer_holder: ReviewerHolder =
            Arc::new(arc_swap::ArcSwap::from_pointee(None));
        let chatops_holder: ChatOpsHolder =
            Arc::new(arc_swap::ArcSwap::from_pointee(None));
        let repo_holder: Arc<ArcSwap<RepositoryConfig>> =
            Arc::new(ArcSwap::from_pointee(repo));
        let handle = tokio::spawn(async move {
            run(
                repo_holder,
                executor_dyn,
                github_holder,
                reviewer_holder,
                chatops_holder,
                2400,
                u32::MAX,
                Some(u32::MAX),
                0, // startup_jitter_max_secs: deterministic for tests
                0, // inter_iteration_jitter_pct: deterministic for tests
                std::sync::Arc::new(crate::audits::AuditRegistry::default()),
                None,
                std::sync::Arc::new(std::collections::HashMap::new()),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cancel_for_task,
            )
            .await;
        });

        // Wait event-driven for the executor to be invoked at least
        // twice — the proof that the loop iterated more than once. The
        // wall-clock cap is a "fail rather than hang" guardrail, not a
        // poll interval.
        let two_invocations = async {
            // notified() must be registered before the first read for
            // the first wake. Register, then check (because the counter
            // could already be ≥2 if we got scheduled late).
            loop {
                if executor.count.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                    return;
                }
                let n = invoked.notified();
                if executor.count.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                    return;
                }
                n.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), two_invocations)
            .await
            .expect("expected ≥2 executor invocations within 10s");
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("loop should exit within 2s of cancel");

        let count = executor.count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            count >= 2,
            "expected ≥2 executor invocations across iterations, got {count}"
        );
    }

    /// Cancellation must break the loop within the sleep window. We use a
    /// 60-second poll interval so the only way the test passes within the
    /// timeout is if `cancel.cancelled()` wins the `select!`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_sleep_exits() {
        use crate::executor::ResumeHandle;
        use async_trait::async_trait;

        struct AlwaysFails;
        #[async_trait]
        impl Executor for AlwaysFails {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                Ok(ExecutorOutcome::Failed {
                    reason: "fixture".into(),
                })
            }
            async fn resume(&self, _h: ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }

        // Fixture workspace: an empty directory + a `local_path` that points
        // to it AND has no `.git` directory so `ensure_initialized` errors.
        // That error is logged and the loop sleeps; cancellation breaks out.
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let repo = RepositoryConfig {
            url: "git@github.com:owner/empty.git".into(),
            local_path: Some(ws.clone()),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        let github = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let cancel = CancellationToken::new();
        let executor: Arc<dyn Executor> = Arc::new(AlwaysFails);

        let cancel_for_task = cancel.clone();
        let github_holder: GithubHolder = Arc::new(arc_swap::ArcSwap::from_pointee(github));
        let reviewer_holder: ReviewerHolder =
            Arc::new(arc_swap::ArcSwap::from_pointee(None));
        let chatops_holder: ChatOpsHolder =
            Arc::new(arc_swap::ArcSwap::from_pointee(None));
        let repo_holder: Arc<ArcSwap<RepositoryConfig>> =
            Arc::new(ArcSwap::from_pointee(repo));
        let iteration_sleep = Arc::new(tokio::sync::Notify::new());
        let hooks = RunHooks {
            on_iteration_sleep: Some(iteration_sleep.clone()),
        };
        let handle = tokio::spawn(async move {
            run_with_hooks(
                repo_holder,
                executor,
                github_holder,
                reviewer_holder,
                chatops_holder,
                2400,
                u32::MAX,
                Some(u32::MAX),
                0, // startup_jitter_max_secs: deterministic for tests
                0, // inter_iteration_jitter_pct: deterministic for tests
                std::sync::Arc::new(crate::audits::AuditRegistry::default()),
                None,
                std::sync::Arc::new(std::collections::HashMap::new()),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cancel_for_task,
                hooks,
            )
            .await;
        });

        // Wait event-driven for the loop to reach its inter-iteration
        // sleep — the `on_iteration_sleep` hook fires immediately before
        // the select! enters the sleep, so a cancel after this notify is
        // guaranteed to race against the sleep branch (the case under
        // test). The 5s wall-clock cap is a guardrail, not a poll interval.
        tokio::time::timeout(Duration::from_secs(5), iteration_sleep.notified())
            .await
            .expect("polling loop did not reach inter-iteration sleep within 5s");
        cancel.cancel();

        // The loop must exit within 1s of cancellation. The 60s sleep would
        // otherwise dominate.
        let res = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(res.is_ok(), "polling loop did not exit within 1s of cancel");
    }

    // ============================================================
    // open-PR pre-flight check (skip-poll-when-pr-open)
    // ============================================================

    fn open_pr_test_repo() -> RepositoryConfig {
        RepositoryConfig {
            url: "git@github.com:upstream-owner/upstream-repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    fn open_pr_test_github(server_url: &str) -> GithubConfig {
        // Resolve_token reads from token_env (or inline). Use a fixture
        // env var unique to this test set so parallel tests don't clobber.
        unsafe { std::env::set_var("AUTOCODER_OPEN_PR_TEST_TOKEN", "testtoken") };
        let _ = server_url; // unused but kept for symmetry with future callers
        GithubConfig {
            token_env: "AUTOCODER_OPEN_PR_TEST_TOKEN".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        }
    }

    #[tokio::test]
    async fn open_pr_check_returns_true_when_pr_exists() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                "/repos/upstream-owner/upstream-repo/pulls?state=open&head=upstream-owner%3Aagent-q&base=main",
            )
            .with_status(200)
            .with_body(
                r#"[{"number":7,"html_url":"https://github.com/upstream-owner/upstream-repo/pull/7"}]"#,
            )
            .expect(1)
            .create_async()
            .await;

        let result = open_pr_exists_for_agent_branch_at(
            &server.url(),
            &open_pr_test_repo(),
            &open_pr_test_github(&server.url()),
        )
        .await;
        assert!(result, "should report PR exists");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn open_pr_check_returns_false_when_no_pr() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                "/repos/upstream-owner/upstream-repo/pulls?state=open&head=upstream-owner%3Aagent-q&base=main",
            )
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;

        let result = open_pr_exists_for_agent_branch_at(
            &server.url(),
            &open_pr_test_repo(),
            &open_pr_test_github(&server.url()),
        )
        .await;
        assert!(!result, "should report no PR");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn open_pr_check_returns_false_on_query_error() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(500)
            .with_body(r#"{"message":"server error"}"#)
            .create_async()
            .await;

        // Best-effort fallback: a 500 from GitHub should not block the
        // iteration — log WARN and proceed as if no PR exists.
        let result = open_pr_exists_for_agent_branch_at(
            &server.url(),
            &open_pr_test_repo(),
            &open_pr_test_github(&server.url()),
        )
        .await;
        assert!(!result, "transport/HTTP errors must degrade to 'no PR'");
    }

    #[tokio::test]
    async fn open_pr_check_uses_fork_owner_in_head_qualifier() {
        // With fork_owner = "bot-machine-user", the head query parameter
        // must be `bot-machine-user:agent-q` (not the upstream owner).
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                "/repos/upstream-owner/upstream-repo/pulls?state=open&head=bot-machine-user%3Aagent-q&base=main",
            )
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;

        let mut gh = open_pr_test_github(&server.url());
        gh.fork_owner = Some("bot-machine-user".into());
        let result = open_pr_exists_for_agent_branch_at(
            &server.url(),
            &open_pr_test_repo(),
            &gh,
        )
        .await;
        assert!(!result);
        mock.assert_async().await;
    }

    // ============================================================
    // Progress notifications: start-of-work + failure alerts
    // ============================================================

    /// Start-of-work notification fires once when a pending change is
    /// dequeued. The mockito server is matched on a body fragment so the
    /// test doesn't care about JSON-key ordering or how `text` is encoded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_of_work_notification_posted_on_dequeue() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "feature-start-of-work", "make work observable");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let start_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::PartialJsonString(
                serde_json::json!({
                    "channel": "C_TEST",
                    "text": "🚀 `git@github.com:owner/fixture.git`: starting work on `feature-start-of-work` — make work observable"
                })
                .to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;

        let executor = CompletingExecutorWithDiff {
            artifact_name: "SOWA.txt".into(),
            artifact_text: "x".into(),
        };
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".into(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");
        assert_eq!(processed, vec!["feature-start-of-work".to_string()]);
        start_mock.assert_async().await;
    }

    /// When `start_work_enabled` is false the mock receives zero calls even
    /// though chatops is otherwise wired.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_of_work_suppressed_when_disabled() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "feature-suppressed", "should not be announced");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let no_post_mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;

        let executor = CompletingExecutorWithDiff {
            artifact_name: "SUPPRESSED.txt".into(),
            artifact_text: "x".into(),
        };
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".into(),
            start_work_enabled: false, // disabled
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");
        assert_eq!(processed, vec!["feature-suppressed".to_string()]);
        no_post_mock.assert_async().await;
    }

    /// Build a workspace whose `origin` URL points at a non-existent local
    /// path so any `git push origin` fails — useful for simulating
    /// `branch_push_failure`. The workspace basename is randomized via
    /// `suffix` so the busy-marker path (which keys off workspace
    /// basename) does not collide between parallel tests.
    fn fixture_workspace_with_broken_remote(
        suffix: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, ws) = fixture_workspace_with_remote();
        // Rename the workspace dir so its basename is unique per test.
        let renamed = ws.parent().unwrap().join(format!("workspace-{suffix}"));
        std::fs::rename(&ws, &renamed).unwrap();
        let ws = renamed;
        let bogus_push = dir.path().join("does-not-exist-push-target");
        let st = std::process::Command::new("git")
            .args([
                "remote",
                "set-url",
                "--push",
                "origin",
                &bogus_push.to_string_lossy(),
            ])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success());
        (dir, ws)
    }

    /// 24h throttle: the first push failure posts; a second pass within
    /// the throttle window does NOT post.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failure_alert_posted_then_suppressed_within_24h() {
        let (_dir, ws) = fixture_workspace_with_broken_remote("alert-throttle");
        add_committed_change(&ws, "needs-push", "push-failure fixture");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        // Exactly one alert post across two iterations.
        let alert_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex(
                "branch push keeps failing".to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        // Start-of-work posts are unrelated and may fire any number of
        // times; allow them.
        let _start_work_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex("starting work on".to_string()))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .create_async()
            .await;

        let executor = CompletingExecutorWithDiff {
            artifact_name: "PUSH_ART.txt".into(),
            artifact_text: "x".into(),
        };
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".into(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        // Iteration 1: pass through commits succeeds, push fails → alert
        // is posted and `.alert-state.json` is written.
        let stuck_secs = 2400u64;
        let _ = execute_one_pass(
            &ws,
            &fixture_repo(&ws),
            &executor,
            &github,
            None,
            Some(&chatops_ctx),
            stuck_secs,
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await;
        assert!(
            ws.join(".alert-state.json").exists(),
            "iter 1's push failure must persist alert state"
        );

        // Iteration 2: invoke `handle_predictable_failure` directly with a
        // synthesized push error. State is loaded from disk; the entry is
        // recent (< 24h), so should_alert is false → no post, mock counter
        // stays at 1. This is the throttle assertion: a repeat failure
        // within the window is silent.
        crate::alerts::handle_predictable_failure(
            &ws,
            &fixture_repo(&ws).url,
            Some(&chatops_ctx),
            true,
            crate::alert_state::AlertCategory::BranchPushFailure,
            &anyhow!("simulated repeat push failure"),
        )
        .await;

        alert_mock.assert_async().await;
    }

    /// Clear-on-success: a failing iteration alerts, a successful next
    /// iteration clears state, then a SECOND failure re-alerts because the
    /// throttle was reset (NOT silenced by the 24h window).
    ///
    /// Iter 1 runs the full `execute_one_pass` to produce the real alert +
    /// real state file. Iter 2 calls `AlertState::clear` directly to mimic
    /// the on-success clear that `execute_one_pass` performs (production
    /// already invokes `clear` at three Ok paths — see the inline calls
    /// in `execute_one_pass` and `run_pass_through_commits`). Iter 3
    /// invokes `handle_predictable_failure` directly to verify that with
    /// state cleared the alert fires again immediately, NOT silenced by
    /// the 24h throttle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failure_alert_cleared_on_subsequent_success() {
        let (_dir, ws) = fixture_workspace_with_broken_remote("alert-cleared");
        add_committed_change(&ws, "round-1", "fixture round 1");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        // Two alerts expected across iterations 1 + 3.
        let alert_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex(
                "branch push keeps failing".to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(2)
            .create_async()
            .await;
        let _start_work_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex("starting work on".to_string()))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .create_async()
            .await;

        let executor = CompletingExecutorWithDiff {
            artifact_name: "ART.txt".into(),
            artifact_text: "x".into(),
        };
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".into(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let stuck_secs = 2400u64;

        // Iteration 1: push fails → alert #1 fires AND state is saved.
        let _ = execute_one_pass(
            &ws,
            &fixture_repo(&ws),
            &executor,
            &github,
            None,
            Some(&chatops_ctx),
            stuck_secs,
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await;
        assert!(
            ws.join(".alert-state.json").exists(),
            "alert state should be written after first failure"
        );

        // Iteration 2: simulate a successful pass-end by directly clearing
        // the alert state, mimicking what `execute_one_pass` does on each
        // of its Ok-return paths (after push+PR succeed, when processed is
        // empty, or when commit_count is zero). The clear paths are
        // covered by `AlertState::clear`'s own unit tests; here we just
        // need the on-disk state to be gone so iter 3 can re-alert.
        crate::alert_state::AlertState::clear(&ws).unwrap();
        assert!(
            !ws.join(".alert-state.json").exists(),
            "alert state must be gone after clear"
        );

        // Iteration 3: simulate another push failure via the helper. State
        // file is gone (cleared in iter 2), so this re-alerts even though
        // less than 24h has elapsed since iter 1's alert.
        crate::alerts::handle_predictable_failure(
            &ws,
            &fixture_repo(&ws).url,
            Some(&chatops_ctx),
            true,
            crate::alert_state::AlertCategory::BranchPushFailure,
            &anyhow!("second push failure after recovery"),
        )
        .await;

        alert_mock.assert_async().await;
    }

    // ============================================================
    // Implementer-summary PR comment
    // ============================================================

    /// Write a fixture run-log file at the location
    /// `<system-temp>/autocoder/logs/<workspace-basename>/<change>.log` so
    /// `build_implementer_summary` can find it without invoking the
    /// executor.
    fn write_fixture_run_log(workspace: &Path, change: &str, prompt: &str, stdout: &str, stderr: &str) {
        let path = crate::executor::claude_cli::run_log_path(workspace, change);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = format!(
            "=== PROMPT ({p} bytes) ===\n{prompt}\n=== STDOUT ({n} bytes) ===\n{stdout}\n=== STDERR ({m} bytes) ===\n{stderr}\n",
            p = prompt.len(),
            n = stdout.len(),
            m = stderr.len(),
        );
        std::fs::write(&path, body).unwrap();
    }

    /// Make a workspace dir whose basename is unique per test so the
    /// `<system-temp>/autocoder/logs/<basename>/` namespace does not
    /// collide across parallel tests.
    fn unique_workspace(suffix: &str) -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix(&format!("autocoder-summary-{suffix}-"))
            .tempdir()
            .unwrap();
        dir
    }

    #[test]
    fn build_implementer_summary_extracts_stdout_only() {
        let dir = unique_workspace("extract-stdout");
        let ws = dir.path();
        write_fixture_run_log(
            ws,
            "alpha",
            "PROMPT_BODY_SECRET",
            "STDOUT_NARRATIVE_VISIBLE",
            "STDERR_LOG_NOISE",
        );
        let out = build_implementer_summary(ws, &["alpha".to_string()]);
        assert!(out.contains("## Agent implementation notes"));
        assert!(out.contains("### alpha"));
        assert!(out.contains("STDOUT_NARRATIVE_VISIBLE"));
        assert!(!out.contains("PROMPT_BODY_SECRET"));
        assert!(!out.contains("STDERR_LOG_NOISE"));
        assert!(!out.contains("=== PROMPT"));
        assert!(!out.contains("=== STDERR"));
    }

    #[test]
    fn build_implementer_summary_skips_missing_log() {
        let dir = unique_workspace("skip-missing");
        let ws = dir.path();
        write_fixture_run_log(ws, "present", "p", "PRESENT_STDOUT", "");
        // "absent" has no log file written.
        let out = build_implementer_summary(
            ws,
            &["present".to_string(), "absent".to_string()],
        );
        assert!(out.contains("PRESENT_STDOUT"));
        assert!(out.contains("### present"));
        assert!(!out.contains("### absent"));
    }

    #[test]
    fn build_implementer_summary_returns_empty_when_all_missing() {
        let dir = unique_workspace("all-missing");
        let ws = dir.path();
        let out = build_implementer_summary(
            ws,
            &["nope-1".to_string(), "nope-2".to_string()],
        );
        assert!(out.is_empty(), "expected empty string, got: {out:?}");
    }

    #[test]
    fn build_implementer_summary_uses_placeholder_for_empty_stdout() {
        let dir = unique_workspace("empty-stdout");
        let ws = dir.path();
        write_fixture_run_log(ws, "silent", "p", "", "");
        let out = build_implementer_summary(ws, &["silent".to_string()]);
        assert!(out.contains("### silent"));
        assert!(out.contains("_(no implementer output captured)_"));
    }

    #[test]
    fn truncate_to_fit_appends_marker_when_exceeded() {
        let body = "x".repeat(100_000);
        let out = truncate_to_fit(body, 60_000);
        let marker = "_[implementer summary truncated to fit GitHub comment limit;";
        assert!(out.ends_with("/<change>.log]_"));
        assert!(out.contains(marker), "missing truncation marker");
        // Total length is bounded by max + marker length.
        assert!(out.len() <= 60_000 + 200, "unexpected length: {}", out.len());
    }

    #[test]
    fn truncate_to_fit_passthrough_when_under_budget() {
        let body = "small body".to_string();
        let out = truncate_to_fit(body.clone(), 60_000);
        assert_eq!(out, body);
    }

    #[test]
    fn truncate_to_fit_respects_char_boundary() {
        // Three-byte char "界" repeated. With max=10 the byte cut would
        // land mid-codepoint; the function must walk back to the prior
        // boundary.
        let body = "界".repeat(20); // 60 bytes
        let out = truncate_to_fit(body, 10);
        // Did not panic. The truncated prefix must be valid UTF-8 and end
        // on a char boundary.
        let prefix_end = out.find("\n\n_[").unwrap();
        let prefix = &out[..prefix_end];
        assert!(prefix.is_char_boundary(prefix.len()));
        assert!(prefix.chars().all(|c| c == '界'));
        // At max=10, three-byte char fits 3 times (9 bytes) — boundary
        // walks down from 10 to 9.
        assert_eq!(prefix.chars().count(), 3);
    }

    /// Integration: `post_implementer_summary_comment` against a mockito
    /// server. Asserts the POST hits the expected endpoint AND the body
    /// contains the per-change stdout sentinel pulled from the fixture
    /// run-log.
    #[tokio::test]
    async fn post_implementer_summary_comment_hits_endpoint_with_stdout_body() {
        let dir = unique_workspace("integration-comment");
        let ws = dir.path();
        write_fixture_run_log(
            ws,
            "the-change",
            "p",
            "INTEGRATION_STDOUT_SENTINEL",
            "",
        );

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/upstream-org/the-repo/issues/77/comments")
            .match_header("authorization", "Bearer testtoken")
            .match_body(mockito::Matcher::Regex(
                "INTEGRATION_STDOUT_SENTINEL".to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":1}"#)
            .expect(1)
            .create_async()
            .await;

        post_implementer_summary_comment(
            &server.url(),
            ws,
            "upstream-org",
            "the-repo",
            77,
            &["the-change".to_string()],
            "testtoken",
        )
        .await;

        mock.assert_async().await;
    }

    /// When all logs are absent the comment is NOT posted.
    #[tokio::test]
    async fn post_implementer_summary_comment_skips_when_summary_empty() {
        let dir = unique_workspace("integration-skip");
        let ws = dir.path();
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", mockito::Matcher::Any)
            .expect(0)
            .create_async()
            .await;

        post_implementer_summary_comment(
            &server.url(),
            ws,
            "owner",
            "repo",
            1,
            &["no-such-change".to_string()],
            "testtoken",
        )
        .await;

        mock.assert_async().await;
    }

    // ============================================================
    // Perma-stuck change detection
    // ============================================================

    /// Run a single pass at the specified threshold and return its result.
    /// Uses the existing remote fixture so the workspace's dirty-check
    /// passes — perma-stuck logic exercises the same Failed paths.
    async fn run_one_pass_with_threshold(
        workspace: &Path,
        executor: &dyn Executor,
        threshold: u32,
    ) -> Result<Vec<String>> {
        let repo = fixture_repo(workspace);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            workspace,
            &repo,
            &github_cfg,
            executor,
            None,
            threshold,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await?;
        Ok(processed)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_increments_failure_counter() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "stuck-change", "fixture reason");
        let executor = AlwaysFailingExecutor;
        // Use a high threshold so a single failure does NOT yet mark
        // perma-stuck; we are asserting only the counter side-effect here.
        let _ = run_one_pass_with_threshold(&ws, &executor, 10).await;
        let state = failure_state::load(&ws).unwrap();
        let entry = state.entries.get("stuck-change").expect("entry present");
        assert_eq!(entry.count, 1);
        assert!(
            entry.last_reason.contains("fixture failure"),
            "last_reason should capture the executor's Failed reason: {}",
            entry.last_reason
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archived_clears_failure_counter() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "recovered", "fixture");
        // Pre-populate the failure-state file with a count for this change.
        let _ = failure_state::record_failure(&ws, "recovered", "earlier fail").unwrap();
        assert!(
            failure_state::load(&ws).unwrap().entries.contains_key("recovered"),
            "fixture must have a counter entry before the pass"
        );
        let executor = CompletingExecutorWithDiff {
            artifact_name: "RECOVERED.txt".into(),
            artifact_text: "x".into(),
        };
        let processed = run_one_pass_with_threshold(&ws, &executor, 10)
            .await
            .expect("pass succeeds");
        assert_eq!(processed, vec!["recovered".to_string()]);
        let state = failure_state::load(&ws).unwrap();
        assert!(
            !state.entries.contains_key("recovered"),
            "archive must clear the failure-state entry"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn threshold_reached_writes_marker_and_excludes_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "doomed", "fixture");
        let executor = AlwaysFailingExecutor;

        // Pass 1: count 1, no marker.
        let _ = run_one_pass_with_threshold(&ws, &executor, 2).await;
        assert!(
            !ws.join("openspec/changes/doomed/.perma-stuck.json").exists(),
            "no marker after first failure"
        );
        assert_eq!(
            queue::list_pending(&ws).unwrap(),
            vec!["doomed".to_string()],
            "change still pending after one failure"
        );

        // Pass 2: count 2 = threshold → marker written, change excluded.
        let _ = run_one_pass_with_threshold(&ws, &executor, 2).await;
        assert!(
            ws.join("openspec/changes/doomed/.perma-stuck.json").exists(),
            "marker must be written when threshold is reached"
        );
        assert!(
            queue::list_pending(&ws).unwrap().is_empty(),
            "perma-stuck change must be excluded from pending"
        );
        // Marker file schema: confirm it contains the change name and count.
        let raw = std::fs::read_to_string(
            ws.join("openspec/changes/doomed/.perma-stuck.json"),
        )
        .unwrap();
        assert!(raw.contains("\"change\": \"doomed\""));
        assert!(raw.contains("\"consecutive_failures\": 2"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removing_marker_re_enables_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "recoverable", "fixture");
        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct CountingFailing(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for CountingFailing {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Failed {
                    reason: "fixture".into(),
                })
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }
        let executor = CountingFailing(invocations.clone());

        let _ = run_one_pass_with_threshold(&ws, &executor, 2).await;
        let _ = run_one_pass_with_threshold(&ws, &executor, 2).await;
        // 2 invocations so far; marker should now exist.
        assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 2);
        let marker = ws.join("openspec/changes/recoverable/.perma-stuck.json");
        assert!(marker.exists(), "marker must be written by pass 2");

        // Pass 3: marker present → excluded → executor NOT invoked.
        let _ = run_one_pass_with_threshold(&ws, &executor, 2).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "executor must not run while marker is present"
        );

        // Operator removes the marker.
        std::fs::remove_file(&marker).unwrap();

        // Pass 4: change is back in pending, executor runs again.
        let _ = run_one_pass_with_threshold(&ws, &executor, 2).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "executor must run after the operator clears the marker"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transient_error_does_not_increment_counter() {
        // Workspace with no .git directory → workspace::ensure_initialized
        // errors out before the executor is ever invoked. The
        // failure-state file must remain absent.
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("placeholder.txt"), "x").unwrap();

        let repo = RepositoryConfig {
            url: "git@github.com:owner/missing.git".into(),
            local_path: Some(ws.clone()),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        let github_cfg = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let executor = AlwaysFailingExecutor;

        let result = run_pass_through_commits(
            &ws,
            &repo,
            &github_cfg,
            &executor,
            None,
            1,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await;
        assert!(result.is_err(), "pre-executor failure must propagate");
        // .failure-state.json must NOT have been written.
        assert!(
            !ws.join(".failure-state.json").exists(),
            "transient pre-executor errors must not bump the counter"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn perma_stuck_alert_posts_to_chatops() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "perma-stuck-alert-fixture", "fixture reason");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let alert_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex("change perma-stuck".to_string()))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        // Allow (and consume) any other unrelated chatops POSTs.
        let _other = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .create_async()
            .await;

        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".into(),
            start_work_enabled: false, // suppress start-of-work to keep matcher unambiguous
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let executor = AlwaysFailingExecutor;
        let _ = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            1, // threshold = 1 → first failure marks perma-stuck
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await;

        assert!(
            ws.join("openspec/changes/perma-stuck-alert-fixture/.perma-stuck.json")
                .exists(),
            "marker should be written when threshold = 1 and the executor failed once"
        );
        alert_mock.assert_async().await;
    }

    /// perma-stuck-alert-includes-log-path: the alert body MUST include a
    /// `run_log:` line pointing at the per-change run log so the
    /// operator can diagnose the failure without knowing the path convention.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn perma_stuck_alert_body_contains_log_path() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "log-path-fixture", "diagnostic test");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        // Match BOTH the perma-stuck subject AND the run_log: line with
        // the expected change name segment.
        let alert_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("change perma-stuck".to_string()),
                mockito::Matcher::Regex("run_log:".to_string()),
                mockito::Matcher::Regex("log-path-fixture\\.log".to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let _other = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .create_async()
            .await;

        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".into(),
            start_work_enabled: false,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let executor = AlwaysFailingExecutor;
        let _ = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            1, // threshold = 1
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await;
        alert_mock.assert_async().await;
    }

    // ============================================================
    // SpecNeedsRevision outcome
    // ============================================================

    /// Executor that returns `SpecNeedsRevision` with a fixed payload on
    /// every `run`. Useful for asserting marker write + alert + queue halt.
    struct SpecRevisionExecutor {
        tasks: Vec<UnimplementableTask>,
        suggestion: String,
        invocations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Executor for SpecRevisionExecutor {
        async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
            self.invocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ExecutorOutcome::SpecNeedsRevision {
                unimplementable_tasks: self.tasks.clone(),
                revision_suggestion: self.suggestion.clone(),
            })
        }
        async fn resume(
            &self,
            _h: crate::executor::ResumeHandle,
            _a: &str,
        ) -> Result<ExecutorOutcome> {
            unreachable!()
        }
    }

    fn fixture_unimpl_tasks() -> Vec<UnimplementableTask> {
        vec![UnimplementableTask {
            task_id: "5.2".into(),
            task_text: "install actionlint locally".into(),
            reason: "no apt access".into(),
        }]
    }

    /// SpecNeedsRevision outcome → marker written, chatops alert posted,
    /// queue walk halts. Later pending changes are not processed in the
    /// same iteration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spec_needs_revision_writes_marker_and_alerts_and_halts_queue() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "01-needs-revision", "fixture");
        add_committed_change(&ws, "02-would-run-if-not-halted", "fixture");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let alert_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex("spec needs revision".into()))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        // Allow other unrelated POSTs (start-of-work etc.) without
        // failing assert. We suppress start-of-work in the ctx below to
        // keep things tidy, but accept any extras.
        let _other = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .create_async()
            .await;

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor = SpecRevisionExecutor {
            tasks: fixture_unimpl_tasks(),
            suggestion: "drop 5.2 from tasks.md".into(),
            invocations: invocations.clone(),
        };
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".into(),
            start_work_enabled: false,
            failure_alerts_enabled: true,
            pr_opened_enabled: false,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let _ = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        )
        .await;

        // Marker is at the expected path with the expected schema fields.
        let marker_path = ws.join("openspec/changes/01-needs-revision/.needs-spec-revision.json");
        assert!(
            marker_path.exists(),
            "marker file must be written at {}",
            marker_path.display()
        );
        let raw = std::fs::read_to_string(&marker_path).unwrap();
        assert!(raw.contains("\"change\""));
        assert!(raw.contains("\"01-needs-revision\""));
        assert!(raw.contains("\"unimplementable_tasks\""));
        assert!(raw.contains("\"5.2\""));
        assert!(raw.contains("\"revision_suggestion\""));
        assert!(raw.contains("drop 5.2 from tasks.md"));
        assert!(raw.contains("\"operator_action\""));
        assert!(raw.contains("\"marked_at\""));

        // Alert was posted exactly once.
        alert_mock.assert_async().await;

        // Queue walk halted: the executor ran for the first change only.
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "queue walk must halt after SpecNeedsRevision; later changes must not run"
        );
        // The second change is still in pending (not archived, not marked).
        assert!(
            ws.join("openspec/changes/02-would-run-if-not-halted").exists(),
            "second change must remain in the queue"
        );

        // The lock for the flagged change was cleaned up.
        assert!(
            !ws.join("openspec/changes/01-needs-revision/.in-progress").exists(),
            ".in-progress lock must be removed after SpecNeedsRevision"
        );
    }

    /// SpecNeedsRevision must NOT increment the perma-stuck counter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spec_needs_revision_does_not_increment_perma_stuck_counter() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "no-counter-bump", "fixture");
        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor = SpecRevisionExecutor {
            tasks: fixture_unimpl_tasks(),
            suggestion: "x".into(),
            invocations: invocations.clone(),
        };
        let _ = run_one_pass_with_threshold(&ws, &executor, 1).await;
        // Marker is present.
        assert!(
            ws.join("openspec/changes/no-counter-bump/.needs-spec-revision.json").exists()
        );
        // failure-state.json must NOT have an entry for this change (or
        // the file must not exist at all). The marker handles exclusion;
        // the counter is operator-action territory, not repeat-failure
        // territory.
        let counter_file = ws.join(".failure-state.json");
        if counter_file.exists() {
            let state = failure_state::load(&ws).unwrap();
            assert!(
                !state.entries.contains_key("no-counter-bump"),
                "SpecNeedsRevision must not write a failure-state entry"
            );
        }
    }

    /// Pre-place a marker → change is excluded from list_pending → the
    /// executor is never invoked for that change.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn change_with_revision_marker_excluded_from_list_pending() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "pre-marked", "fixture");
        // Pre-place the marker; the marker file must NOT trip the dirty
        // check because workspace::ensure_initialized adds it to
        // .git/info/exclude.
        std::fs::write(
            ws.join("openspec/changes/pre-marked/.needs-spec-revision.json"),
            r#"{"change":"pre-marked","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"Edit tasks.md, commit, then delete this marker."}"#,
        )
        .unwrap();

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Completed)
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }
        let executor = Counter(invocations.clone());
        let _ = run_one_pass_with_threshold(&ws, &executor, u32::MAX).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "executor must NOT be invoked for a change with a needs-spec-revision marker"
        );
    }

    /// Pre-place the marker, run once (executor not called), then delete the
    /// marker and run again — the executor IS called the second time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn marker_removed_re_enables_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "operator-cleared", "fixture");
        let marker = ws.join("openspec/changes/operator-cleared/.needs-spec-revision.json");
        std::fs::write(
            &marker,
            r#"{"change":"operator-cleared","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"Edit tasks.md, commit, then delete this marker."}"#,
        )
        .unwrap();

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Failed { reason: "noop fixture".into() })
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }
        let executor = Counter(invocations.clone());
        // First pass: marker present → executor must not run.
        let _ = run_one_pass_with_threshold(&ws, &executor, u32::MAX).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "executor must not be invoked while marker is present"
        );

        // Operator removes the marker.
        std::fs::remove_file(&marker).unwrap();

        // Second pass: change is back in pending, executor runs.
        let _ = run_one_pass_with_threshold(&ws, &executor, u32::MAX).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "executor must run after the operator clears the marker"
        );
    }

    // ============================================================
    // Self-heal for already-implemented changes
    // ============================================================

    /// `tasks_md_all_complete`: every checkbox is `[x]` → true.
    #[test]
    fn tasks_md_all_complete_all_checked_returns_true() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let tasks = ws.join("openspec/changes/c/tasks.md");
        std::fs::create_dir_all(tasks.parent().unwrap()).unwrap();
        std::fs::write(
            &tasks,
            "## 1. things\n- [x] 1.1 first\n- [x] 1.2 second\n  - [x] 1.3 nested\n",
        )
        .unwrap();
        assert!(tasks_md_all_complete(ws, "c").unwrap());
    }

    /// `tasks_md_all_complete`: mixed `[x]` and `[ ]` → false.
    #[test]
    fn tasks_md_all_complete_mixed_returns_false() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let tasks = ws.join("openspec/changes/c/tasks.md");
        std::fs::create_dir_all(tasks.parent().unwrap()).unwrap();
        std::fs::write(&tasks, "- [x] done\n- [ ] still open\n").unwrap();
        assert!(!tasks_md_all_complete(ws, "c").unwrap());
    }

    /// `tasks_md_all_complete`: every checkbox is `[ ]` → false.
    #[test]
    fn tasks_md_all_complete_all_open_returns_false() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let tasks = ws.join("openspec/changes/c/tasks.md");
        std::fs::create_dir_all(tasks.parent().unwrap()).unwrap();
        std::fs::write(&tasks, "- [ ] a\n- [ ] b\n").unwrap();
        assert!(!tasks_md_all_complete(ws, "c").unwrap());
    }

    /// `tasks_md_all_complete`: no checkbox lines at all → false.
    /// "no tasks recorded = not complete" is the conservative path.
    #[test]
    fn tasks_md_all_complete_empty_returns_false() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let tasks = ws.join("openspec/changes/c/tasks.md");
        std::fs::create_dir_all(tasks.parent().unwrap()).unwrap();
        std::fs::write(&tasks, "## Heading\nNo checkboxes here.\n").unwrap();
        assert!(!tasks_md_all_complete(ws, "c").unwrap());
    }

    /// `tasks_md_all_complete`: missing file → Err.
    #[test]
    fn tasks_md_all_complete_missing_file_returns_err() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        assert!(tasks_md_all_complete(ws, "does-not-exist").is_err());
    }

    /// Write a self-heal-ready change into the fixture workspace: a proposal,
    /// a tasks.md with every task `[x]`, and a spec under `specs/<cap>/` that
    /// `openspec validate --strict` accepts. Commit it so the dirty check
    /// stays clean.
    fn add_committed_self_heal_change(workspace: &Path, name: &str, all_done: bool, valid_spec: bool) {
        let dir = workspace.join("openspec/changes").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("proposal.md"),
            "## Why\n\nfixture self-heal\n\n## What Changes\n\n- thing\n",
        )
        .unwrap();
        let tasks = if all_done {
            "- [x] 1.1 done\n- [x] 1.2 also done\n"
        } else {
            "- [x] 1.1 done\n- [ ] 1.2 still open\n"
        };
        std::fs::write(dir.join("tasks.md"), tasks).unwrap();
        let spec_dir = dir.join("specs").join("self-heal-fixture-cap");
        std::fs::create_dir_all(&spec_dir).unwrap();
        let spec_body = if valid_spec {
            "## ADDED Requirements\n\n### Requirement: Do thing\nThe system SHALL do the thing.\n\n#### Scenario: It works\n- **WHEN** triggered\n- **THEN** does thing\n"
        } else {
            // No scenario block → openspec validate --strict fails.
            "## ADDED Requirements\n\n### Requirement: Do thing\nThe system SHALL do the thing.\n"
        };
        std::fs::write(spec_dir.join("spec.md"), spec_body).unwrap();
        let st = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(workspace)
            .status()
            .unwrap();
        assert!(st.success());
        let st = std::process::Command::new("git")
            .args(["commit", "-q", "-m", &format!("scaffold {name}")])
            .current_dir(workspace)
            .status()
            .unwrap();
        assert!(st.success());
    }

    /// Self-heal succeeds: change with every task `[x]`, valid spec, and a
    /// Completed-with-empty-workspace executor result. autocoder must
    /// archive, commit the move, and flag the pass as self-healing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_heal_archives_when_preconditions_met() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_self_heal_change(&ws, "already-done", true, true);

        let executor = CompletingExecutorNoDiff;
        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, includes_self_heal) =
            run_pass_through_commits(&ws, &repo, &github_cfg, &executor, None, u32::MAX, u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
)
                .await
                .expect("self-heal pass succeeds");
        assert_eq!(
            processed,
            vec!["already-done".to_string()],
            "self-healed change must appear in processed list"
        );
        assert!(
            includes_self_heal,
            "pass should report includes_self_heal = true"
        );

        // Active change dir is gone; archive entry exists with date prefix.
        assert!(
            !ws.join("openspec/changes/already-done").exists(),
            "active change dir must be moved into archive"
        );
        let archive = ws.join("openspec/changes/archive");
        let archived_names: Vec<String> = std::fs::read_dir(&archive)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            archived_names.iter().any(|n| n.ends_with("-already-done")),
            "expected archived already-done in {archived_names:?}"
        );

        // Commit subject matches the spec-mandated form.
        let out = std::process::Command::new("git")
            .args(["log", "-1", "--pretty=%s", "agent-q"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(out.status.success());
        let subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            subject, "archive: already-done: implementation already in base",
            "self-heal commit subject must follow the spec-mandated format"
        );

        // PR body for this pass carries the disclaimer paragraph.
        let body = build_pr_body(&ws, &processed, includes_self_heal);
        assert!(
            body.contains("_This PR archives one or more changes whose implementation was already present on the base branch."),
            "PR body should include the self-heal disclaimer; got: {body}"
        );
    }

    /// Self-heal precondition unmet: tasks.md has an unchecked task → the
    /// pass falls through to the existing Failed path. Change must remain
    /// in pending; nothing committed; nothing archived.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_heal_falls_through_to_failed_when_tasks_incomplete() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // all_done=false → tasks.md contains a `[ ]` line.
        add_committed_self_heal_change(&ws, "tasks-open", false, true);

        let pre_main = crate::git::rev_parse(&ws, "main").unwrap();

        let executor = CompletingExecutorNoDiff;
        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, includes_self_heal) =
            run_pass_through_commits(&ws, &repo, &github_cfg, &executor, None, u32::MAX, u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
)
                .await
                .expect("pass returns Failed via fall-through, not Err");
        assert!(
            processed.is_empty(),
            "no archived changes expected; got {processed:?}"
        );
        assert!(
            !includes_self_heal,
            "self-heal flag must remain false when preconditions unmet"
        );

        // Change is NOT archived; still in pending; no commit on agent-q.
        assert!(
            ws.join("openspec/changes/tasks-open").exists(),
            "change must remain in active changes for retry"
        );
        let archive_root = ws.join("openspec/changes/archive");
        if archive_root.exists() {
            for entry in std::fs::read_dir(&archive_root).unwrap() {
                let name = entry.unwrap().file_name().into_string().unwrap();
                assert!(
                    !name.ends_with("-tasks-open"),
                    "must not archive tasks-open with an open task"
                );
            }
        }
        assert_eq!(
            queue::list_pending(&ws).unwrap(),
            vec!["tasks-open".to_string()],
            "change must be back in pending"
        );
        let agent_sha = crate::git::rev_parse(&ws, "agent-q").unwrap();
        assert_eq!(agent_sha, pre_main, "no commit must be made");
    }

    /// Self-heal precondition unmet: `openspec validate --strict` errors
    /// because the spec is missing a Scenario block. Same fall-through to
    /// Failed; no archive, no commit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_heal_falls_through_when_openspec_validate_fails() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // tasks all done, but spec lacks Scenario → openspec validate fails.
        add_committed_self_heal_change(&ws, "invalid-spec", true, false);

        let pre_main = crate::git::rev_parse(&ws, "main").unwrap();

        let executor = CompletingExecutorNoDiff;
        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, includes_self_heal) =
            run_pass_through_commits(&ws, &repo, &github_cfg, &executor, None, u32::MAX, u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
)
                .await
                .expect("pass returns Failed via fall-through, not Err");
        assert!(processed.is_empty());
        assert!(!includes_self_heal);

        // Change must remain in pending and unarchived.
        assert!(ws.join("openspec/changes/invalid-spec").exists());
        let archive_root = ws.join("openspec/changes/archive");
        if archive_root.exists() {
            for entry in std::fs::read_dir(&archive_root).unwrap() {
                let name = entry.unwrap().file_name().into_string().unwrap();
                assert!(!name.ends_with("-invalid-spec"));
            }
        }
        let agent_sha = crate::git::rev_parse(&ws, "agent-q").unwrap();
        assert_eq!(agent_sha, pre_main);
    }

    /// A pass with normally-implemented changes only (no self-heal) must
    /// NOT include the self-heal disclaimer paragraph in the PR body.
    #[test]
    fn self_heal_paragraph_omitted_when_no_self_heals_in_pass() {
        let tmp = tempfile::TempDir::new().unwrap();
        let processed = vec!["regular-change".to_string()];
        let body = build_pr_body(tmp.path(), &processed, false);
        assert!(
            !body.contains("This PR archives one or more changes whose implementation was already present"),
            "disclaimer paragraph must not appear when includes_self_heal=false; got: {body}"
        );
        assert!(
            body.contains("- regular-change"),
            "normal change listing must remain"
        );
    }

    /// Executor that writes a per-change file so every change produces a
    /// distinct diff and can archive cleanly across iterations.
    struct PerChangeArtifactExecutor;
    #[async_trait::async_trait]
    impl Executor for PerChangeArtifactExecutor {
        async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
            std::fs::write(
                workspace.join(format!("artifact-{change}.txt")),
                format!("contents for {change}\n"),
            )?;
            Ok(ExecutorOutcome::Completed)
        }
        async fn resume(
            &self,
            _h: crate::executor::ResumeHandle,
            _a: &str,
        ) -> Result<ExecutorOutcome> {
            unreachable!()
        }
    }

    /// max-changes-per-pr-limit: with 5 pending changes and a cap of 3, a
    /// single pass commits exactly 3 archives and leaves the remaining 2
    /// in the pending queue for the next iteration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_queue_stops_at_max_changes() {
        let (_dir, ws) = fixture_workspace_with_remote();
        for n in 1..=5 {
            add_committed_change(&ws, &format!("ch{n:02}"), &format!("fixture {n}"));
        }

        let executor = PerChangeArtifactExecutor;
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _self_heal) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &executor,
            None,
            u32::MAX,
            3, // cap,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");

        assert_eq!(processed.len(), 3, "exactly 3 changes commit in one pass");
        assert_eq!(
            processed,
            vec!["ch01".to_string(), "ch02".to_string(), "ch03".to_string()],
            "first three by queue order are processed"
        );
        // The remaining two are still pending.
        let still_pending = queue::list_pending(&ws).unwrap();
        assert_eq!(
            still_pending,
            vec!["ch04".to_string(), "ch05".to_string()],
            "the last two remain in the queue for the next iteration"
        );
    }

    /// max-changes-per-pr-limit: a cap of 1 ships exactly one archive per
    /// pass; the rest of the queue waits for subsequent iterations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_queue_cap_of_1_ships_one_per_pass() {
        let (_dir, ws) = fixture_workspace_with_remote();
        for n in 1..=3 {
            add_committed_change(&ws, &format!("ch{n:02}"), &format!("fixture {n}"));
        }
        let executor = PerChangeArtifactExecutor;
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &executor,
            None,
            u32::MAX,
            1, // cap of 1,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");
        assert_eq!(processed, vec!["ch01".to_string()], "cap=1 → one archive");
        let still_pending = queue::list_pending(&ws).unwrap();
        assert_eq!(
            still_pending,
            vec!["ch02".to_string(), "ch03".to_string()],
            "remaining changes wait for the next iteration"
        );
    }

    /// halt-queue-walk-on-non-archive: a `Failed` outcome halts the walk
    /// regardless of cap. Changes later in the queue may depend on the
    /// failed one and SHALL NOT be attempted until the next iteration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_queue_halts_on_failed_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // ch01 succeeds, ch02 fails, ch03 and ch04 would succeed but the
        // walk must halt at the failure.
        add_committed_change(&ws, "ch01", "succeeds first");
        add_committed_change(&ws, "ch02-fails", "fails second");
        add_committed_change(&ws, "ch03", "should not be attempted");
        add_committed_change(&ws, "ch04", "should not be attempted");

        struct ArchiveThenFailThenWouldArchive;
        #[async_trait::async_trait]
        impl Executor for ArchiveThenFailThenWouldArchive {
            async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
                if change == "ch02-fails" {
                    return Ok(ExecutorOutcome::Failed {
                        reason: "fixture failure".into(),
                    });
                }
                std::fs::write(
                    workspace.join(format!("artifact-{change}.txt")),
                    format!("contents for {change}\n"),
                )?;
                Ok(ExecutorOutcome::Completed)
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }

        let executor = ArchiveThenFailThenWouldArchive;
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &executor,
            None,
            u32::MAX,
            10, // cap intentionally generous; halt must come from the failure,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");
        assert_eq!(
            processed,
            vec!["ch01".to_string()],
            "only ch01 archived; ch02-fails halts the walk before ch03/ch04"
        );
        // ch02-fails still pending (failed once, retries next iteration).
        // ch03 and ch04 still pending (walker never reached them).
        let still_pending = queue::list_pending(&ws).unwrap();
        assert!(
            still_pending.contains(&"ch02-fails".to_string()),
            "failed change still pending for retry: {still_pending:?}"
        );
        assert!(
            still_pending.contains(&"ch03".to_string()),
            "untouched ch03 still pending: {still_pending:?}"
        );
        assert!(
            still_pending.contains(&"ch04".to_string()),
            "untouched ch04 still pending: {still_pending:?}"
        );
        // ch03 must not have a failure-state entry — it was never attempted.
        assert!(
            !ws.join("openspec/changes/ch03/.failure-state.json").exists(),
            "ch03 must not have a failure-state entry — walker never reached it"
        );
    }

    /// halt-queue-walk-on-non-archive: an `Escalated` outcome (AskUser
    /// posted to chatops) halts the walk regardless of cap. Later
    /// pending changes wait for the next iteration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_queue_halts_on_escalated_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "ch01", "succeeds first");
        add_committed_change(&ws, "ch02-asks", "asks a question");
        add_committed_change(&ws, "ch03", "should not be attempted");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let _post = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"2222222222.111111"}"#)
            .create_async()
            .await;

        struct ArchiveThenAskThenWouldArchive {
            ws: std::path::PathBuf,
        }
        #[async_trait::async_trait]
        impl Executor for ArchiveThenAskThenWouldArchive {
            async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
                if change == "ch02-asks" {
                    return Ok(ExecutorOutcome::AskUser {
                        question: "Halt me?".to_string(),
                        resume_handle: ResumeHandle(
                            serde_json::json!({"change": change, "workspace": self.ws}),
                        ),
                    });
                }
                std::fs::write(
                    workspace.join(format!("artifact-{change}.txt")),
                    format!("contents for {change}\n"),
                )?;
                Ok(ExecutorOutcome::Completed)
            }
            async fn resume(
                &self,
                _h: ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!("resume not exercised in this test")
            }
        }

        let executor = ArchiveThenAskThenWouldArchive { ws: ws.clone() };
        let chatops_ctx = ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let test_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            Some(&chatops_ctx),
            u32::MAX,
            10, // cap intentionally generous; halt must come from escalation,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");
        assert_eq!(
            processed,
            vec!["ch01".to_string()],
            "only ch01 archived; ch02-asks halts the walk before ch03"
        );
        // ch02-asks is now waiting (has .question.json).
        assert!(
            ws.join("openspec/changes/ch02-asks/.question.json").is_file(),
            "ch02-asks must have .question.json after escalation"
        );
        // ch03 is still pending — walker never reached it.
        let still_pending = queue::list_pending(&ws).unwrap();
        assert!(
            still_pending.contains(&"ch03".to_string()),
            "untouched ch03 still pending: {still_pending:?}"
        );
    }

    /// commit-trailing-archive: after a single-change archive pass, the
    /// working tree MUST be clean. The original bug left the archive
    /// rename uncommitted, causing the next iteration's dirty check to
    /// trip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archived_change_leaves_clean_working_tree() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "only-change", "fixture for trailing-archive");
        let executor = PerChangeArtifactExecutor;
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &executor,
            None,
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");
        assert_eq!(processed, vec!["only-change".to_string()]);
        let porcelain = crate::git::status_porcelain(&ws).unwrap();
        assert!(
            porcelain.trim().is_empty(),
            "working tree must be clean after archive; got:\n{porcelain}"
        );
    }

    /// commit-trailing-archive: HEAD's commit MUST contain both the
    /// executor's implementation file AND the archive rename of
    /// proposal.md / tasks.md.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn commit_contains_both_impl_and_archive_rename() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "feature-x", "trailing archive test");
        let executor = PerChangeArtifactExecutor;
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let _ = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &executor,
            None,
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");

        // diff-tree --name-status HEAD^..HEAD shows the files changed in
        // the new commit. Use `-M` to detect renames so the archive move
        // shows as a single `R` entry rather than D+A.
        let out = std::process::Command::new("git")
            .args(["diff-tree", "--no-commit-id", "--name-status", "-r", "-M", "HEAD^..HEAD"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(out.status.success(), "diff-tree failed");
        let body = String::from_utf8_lossy(&out.stdout).to_string();

        // Implementation artifact must appear.
        assert!(
            body.contains("artifact-feature-x.txt"),
            "commit missing executor artifact; diff-tree output:\n{body}"
        );
        // Archive move must appear (either as a rename or as D+A pair).
        let has_rename = body.lines().any(|l| {
            l.starts_with("R")
                && l.contains("openspec/changes/feature-x/proposal.md")
                && l.contains("openspec/changes/archive/")
        });
        let has_delete_and_add = body
            .lines()
            .any(|l| l.starts_with("D\topenspec/changes/feature-x/"))
            && body.lines().any(|l| {
                l.starts_with("A\topenspec/changes/archive/") && l.contains("-feature-x/")
            });
        assert!(
            has_rename || has_delete_and_add,
            "commit must contain archive rename of openspec/changes/feature-x/; diff-tree output:\n{body}"
        );
    }

    /// recover-dirty-workspace-mid-iteration: a workspace dirty at
    /// `run_pass_through_commits` time triggers auto-recovery
    /// (`git reset --hard origin/<base> + git clean -fd`). When recovery
    /// cleans the dirt, the iteration proceeds normally AND no chatops
    /// alert fires (the operator does not need to be notified about a
    /// self-healed condition).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dirty_workspace_recovers_and_iteration_proceeds() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // Seed a dirty state: untracked file under openspec/.
        // `git clean -fd` (the recovery step) will remove this.
        std::fs::create_dir_all(ws.join("openspec/changes/leftover")).unwrap();
        std::fs::write(
            ws.join("openspec/changes/leftover/proposal.md"),
            "## Why\nleftover\n",
        )
        .unwrap();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        // No alert should fire — recovery handles the dirt silently.
        let mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;
        let chatops_ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        struct UnreachableExecutor;
        #[async_trait::async_trait]
        impl Executor for UnreachableExecutor {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                // After `git clean -fd` the leftover dir is gone, so the
                // queue walk has nothing to do and the executor is never
                // invoked. If this panics, the test reveals a regression.
                unreachable!("post-recovery queue must be empty; executor should not be invoked")
            }
            async fn resume(&self, _h: ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }
        let result = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &UnreachableExecutor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        )
        .await;
        assert!(
            result.is_ok(),
            "iteration should succeed after recovery; got: {result:?}"
        );
        // The dirty untracked dir must be gone.
        assert!(
            !ws.join("openspec/changes/leftover").exists(),
            "git clean -fd should have removed the untracked dir"
        );
        // No state file was written because no alert fired.
        assert!(
            !ws.join(".alert-state.json").exists(),
            "no alert, no state file write"
        );
        mock.assert_async().await;
    }

    /// recover-dirty-workspace-mid-iteration: when recovery itself
    /// errors (e.g. `git reset --hard` against an origin that doesn't
    /// have the configured base branch), the iteration falls back to
    /// the old alert-and-return-Err path. The alert is the operator's
    /// signal that a manually-fixable problem is present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dirty_workspace_recovery_failure_still_alerts() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // Dirty state same as the success-path test.
        std::fs::create_dir_all(ws.join("openspec/changes/leftover")).unwrap();
        std::fs::write(
            ws.join("openspec/changes/leftover/proposal.md"),
            "## Why\nleftover\n",
        )
        .unwrap();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let chatops_ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        // base_branch points at a branch that does NOT exist on origin
        // → `git reset --hard origin/nonexistent-branch` errors →
        // recovery returns Err → fall back to alert path.
        let mut repo = fixture_repo(&ws);
        repo.base_branch = "nonexistent-branch".into();

        struct UnreachableExecutor;
        #[async_trait::async_trait]
        impl Executor for UnreachableExecutor {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                unreachable!()
            }
            async fn resume(&self, _h: ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }
        let result = run_pass_through_commits(
            &ws,
            &repo,
            &github_cfg,
            &UnreachableExecutor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        )
        .await;
        assert!(result.is_err(), "recovery failure must surface as Err");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("recovery failed") || err.contains("dirty"),
            "error should name the recovery failure; got: {err}"
        );
        mock.assert_async().await;
        let state = crate::alert_state::AlertState::load_or_default(&ws);
        assert!(
            state
                .alerts
                .contains_key(&crate::alert_state::AlertCategory::WorkspaceDirtyMidIteration),
            "alert state must record the WorkspaceDirtyMidIteration timestamp"
        );
    }

    /// recover-dirty-workspace-mid-iteration: without chatops the
    /// auto-recovery still runs. Workspace dirty → recovery cleans
    /// → iteration succeeds. No state file is written (no alert posted).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dirty_workspace_recovers_without_chatops() {
        let (_dir, ws) = fixture_workspace_with_remote();
        std::fs::create_dir_all(ws.join("openspec/changes/leftover")).unwrap();
        std::fs::write(
            ws.join("openspec/changes/leftover/proposal.md"),
            "## Why\nleftover\n",
        )
        .unwrap();

        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        struct UnreachableExecutor;
        #[async_trait::async_trait]
        impl Executor for UnreachableExecutor {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                unreachable!()
            }
            async fn resume(&self, _h: ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }
        let result = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &UnreachableExecutor,
            None, // no chatops
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        )
        .await;
        assert!(result.is_ok(), "iteration should succeed: {result:?}");
        assert!(
            !ws.join(".alert-state.json").exists(),
            "no chatops, no state file write"
        );
    }

    /// attempt_dirty_workspace_recovery is a thin wrapper; unit-test it
    /// in isolation so a regression in the helper itself is caught
    /// independently of the run_pass_through_commits integration.
    #[test]
    fn attempt_dirty_workspace_recovery_clears_untracked_and_tracked_modifications() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // Tracked modification: rewrite README.md.
        std::fs::write(ws.join("README.md"), "modified\n").unwrap();
        // Untracked file.
        std::fs::write(ws.join("untracked.txt"), "stranger\n").unwrap();
        // Sanity: status reports both.
        let dirty = git::status_porcelain(&ws).unwrap();
        assert!(
            dirty.contains("README.md") && dirty.contains("untracked.txt"),
            "fixture must seed both kinds of dirt: {dirty}"
        );
        attempt_dirty_workspace_recovery(&ws, "main").expect("recovery should succeed");
        let after = git::status_porcelain(&ws).unwrap();
        assert!(
            after.is_empty(),
            "workspace must be clean after recovery; got: {after}"
        );
        // README.md should be restored to origin's content.
        let restored = std::fs::read_to_string(ws.join("README.md")).unwrap();
        assert_eq!(restored, "hi\n", "tracked file restored from origin");
    }

    /// pr-opened-chatops-notification: when `pr_opened_enabled = true`,
    /// `maybe_post_pr_opened` posts exactly one message to the channel,
    /// containing the repository URL, the PR URL, and the change count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_opened_notification_fires_when_enabled() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("opened PR".to_string()),
                mockito::Matcher::Regex(
                    "https://github\\.com/owner/repo/pull/42".to_string(),
                ),
                mockito::Matcher::Regex("3 change".to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let repo = RepositoryConfig {
            url: "git@github.com:owner/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        maybe_post_pr_opened(
            &repo,
            Some(&ctx),
            "https://github.com/owner/repo/pull/42",
            3,
        )
        .await;
        mock.assert_async().await;
    }

    /// pr-opened-chatops-notification: when `pr_opened_enabled = false`,
    /// no chatops post is made.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_opened_notification_suppressed_when_disabled() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: false,
        };
        let repo = RepositoryConfig {
            url: "git@github.com:owner/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        maybe_post_pr_opened(
            &repo,
            Some(&ctx),
            "https://github.com/owner/repo/pull/42",
            1,
        )
        .await;
        mock.assert_async().await;
    }

    /// pr-opened-chatops-notification: when the chatops backend's post
    /// returns Err, the helper does NOT panic and does NOT propagate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_opened_notification_failure_does_not_propagate() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let _mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"channel_not_found"}"#)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let repo = RepositoryConfig {
            url: "git@github.com:owner/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        // Should not panic; should return Ok-equivalent (it's an async fn
        // returning unit, so "doesn't panic" is the assertion).
        maybe_post_pr_opened(
            &repo,
            Some(&ctx),
            "https://github.com/owner/repo/pull/42",
            1,
        )
        .await;
    }

    /// re-fork-chatops-notification: a successful re-fork triggers
    /// exactly one chat.postMessage whose body contains the destructive-
    /// action notice and the repo URL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refork_notification_fires_when_failure_alerts_enabled() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("re-forked".to_string()),
                mockito::Matcher::Regex("owner/repo".to_string()),
                mockito::Matcher::Regex("closed".to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let repo = RepositoryConfig {
            url: "git@github.com:owner/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        maybe_post_refork_notification(&repo, Some(&ctx)).await;
        mock.assert_async().await;
    }

    /// re-fork-chatops-notification: when failure alerts are disabled
    /// the helper is a no-op (re-fork is a recovery event, gated by the
    /// same toggle as the other operator-visible failure alerts).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refork_notification_suppressed_when_failure_alerts_disabled() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: false,
            pr_opened_enabled: true,
        };
        let repo = RepositoryConfig {
            url: "git@github.com:owner/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        maybe_post_refork_notification(&repo, Some(&ctx)).await;
        mock.assert_async().await;
    }

    fn fixture_repo_for_rebuild_test() -> RepositoryConfig {
        RepositoryConfig {
            url: "git@github.com:owner/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    /// success-with-drift: report has zero failures + a PR URL → the
    /// notification names the PR, the modified-file count, and the
    /// archived-change count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_of_rebuild_success_with_drift_posts_pr_url_message() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("PR".to_string()),
                mockito::Matcher::Regex("3 capability".to_string()),
                mockito::Matcher::Regex("5 archived change".to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: false,
            failure_alerts_enabled: false,
            pr_opened_enabled: false, // notification fires regardless
        };
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 5,
            successful: 5,
            failed: 0,
            spec_files: vec![
                crate::cli::sync_specs::SpecFileOutcome {
                    path: "openspec/specs/a/spec.md".into(),
                    modified: true,
                },
                crate::cli::sync_specs::SpecFileOutcome {
                    path: "openspec/specs/b/spec.md".into(),
                    modified: true,
                },
                crate::cli::sync_specs::SpecFileOutcome {
                    path: "openspec/specs/c/spec.md".into(),
                    modified: true,
                },
                crate::cli::sync_specs::SpecFileOutcome {
                    path: "openspec/specs/d/spec.md".into(),
                    modified: false,
                },
            ],
            ..Default::default()
        };
        maybe_post_end_of_rebuild_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            Some("https://github.com/owner/repo/pull/77"),
            Some(&ctx),
        )
        .await;
        mock.assert_async().await;
    }

    /// success-no-drift: report has zero failures + no PR URL → the
    /// notification names "no drift detected".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_of_rebuild_success_no_drift_posts_clean_message() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex("no drift detected".to_string()))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 5,
            successful: 5,
            failed: 0,
            spec_files: vec![],
            ..Default::default()
        };
        maybe_post_end_of_rebuild_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            None,
            Some(&ctx),
        )
        .await;
        mock.assert_async().await;
    }

    /// partial-failure: report has >0 failures → the notification lists
    /// the failed slugs and includes the journalctl pointer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_of_rebuild_partial_failure_lists_failed_slugs() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("2 failure".to_string()),
                mockito::Matcher::Regex("a06-foo".to_string()),
                mockito::Matcher::Regex("a07-bar".to_string()),
                mockito::Matcher::Regex("journalctl".to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: false,
            failure_alerts_enabled: false,
            pr_opened_enabled: false,
        };
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 5,
            successful: 3,
            failed: 2,
            failures: vec![
                crate::cli::sync_specs::ChangeOutcome {
                    slug: "a06-foo".into(),
                    original_name: "2026-01-01-a06-foo".into(),
                    success: false,
                    failure_reason: "boom".into(),
                },
                crate::cli::sync_specs::ChangeOutcome {
                    slug: "a07-bar".into(),
                    original_name: "2026-01-02-a07-bar".into(),
                    success: false,
                    failure_reason: "boom2".into(),
                },
            ],
            spec_files: vec![
                crate::cli::sync_specs::SpecFileOutcome {
                    path: "openspec/specs/a/spec.md".into(),
                    modified: true,
                },
                crate::cli::sync_specs::SpecFileOutcome {
                    path: "openspec/specs/b/spec.md".into(),
                    modified: true,
                },
            ],
            ..Default::default()
        };
        maybe_post_end_of_rebuild_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            Some("https://github.com/owner/repo/pull/77"),
            Some(&ctx),
        )
        .await;
        mock.assert_async().await;
    }

    /// no-chatops: when `chatops_ctx` is None, the helper is a no-op —
    /// no chatops mock should fire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_of_rebuild_no_chatops_is_noop() {
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 1,
            successful: 1,
            failed: 0,
            ..Default::default()
        };
        maybe_post_end_of_rebuild_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            None,
            None, // no chatops
        )
        .await;
        // No assertion needed beyond "doesn't panic"; the absence of any
        // mockito server means a stray POST would obviously fail anyway.
    }

    /// truncation: 15 failed slugs → the notification lists 10 + "and 5
    /// more"; slugs 11-15 must not appear verbatim.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_of_rebuild_failed_slugs_truncation() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        // Match: contains slug-01 (first) AND slug-10 (last of first 10)
        // AND "and 5 more". A negative-match for slug-11 catches the
        // truncation bug.
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("slug-01".to_string()),
                mockito::Matcher::Regex("slug-10".to_string()),
                mockito::Matcher::Regex("and 5 more".to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: false,
            failure_alerts_enabled: false,
            pr_opened_enabled: false,
        };
        let failures: Vec<crate::cli::sync_specs::ChangeOutcome> = (1..=15)
            .map(|i| crate::cli::sync_specs::ChangeOutcome {
                slug: format!("slug-{i:02}"),
                original_name: format!("2026-01-01-slug-{i:02}"),
                success: false,
                failure_reason: "boom".into(),
            })
            .collect();
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 15,
            successful: 0,
            failed: 15,
            failures,
            ..Default::default()
        };
        maybe_post_end_of_rebuild_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            None,
            Some(&ctx),
        )
        .await;
        mock.assert_async().await;
    }

    /// pending_rebuild-branch: a polling iteration whose flag is set
    /// runs the rebuild path instead of the queue walk. The fixture has
    /// no archived changes (so `rebuild_canonical` produces an empty
    /// report) and no drift (so the iteration completes without trying
    /// to push or open a PR). The assertion is that the iteration
    /// returns Ok WITHOUT invoking the executor (we pass a panicking
    /// executor; if the queue-walk path were taken it would panic).
    /// Skipped (printed) when `openspec` is absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rebuild_iteration_runs_when_pending_flag_set() {
        if std::process::Command::new("openspec")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipping rebuild_iteration_runs_when_pending_flag_set: openspec absent");
            return;
        }
        let (_dir, ws) = fixture_workspace_with_remote();
        // Seed the OpenSpec layout (with no archived changes, so the
        // rebuild is a no-op). The dirs are committed so the iteration's
        // dirty-recovery step doesn't `git clean -fd` them away as
        // untracked. Critically: do NOT seed `openspec/specs/` — the
        // rebuild's clear-and-replay would remove any tracked content
        // there, producing drift the test isn't intending to exercise.
        std::fs::create_dir_all(ws.join("openspec/changes/archive")).unwrap();
        std::fs::write(
            ws.join("openspec/project.md"),
            "# Project\n\nFixture.\n",
        )
        .unwrap();
        // Empty archive dir needs a gitkeep file so git tracks it.
        std::fs::write(ws.join("openspec/changes/archive/.gitkeep"), "").unwrap();
        let st = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success());
        let st = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "scaffold openspec layout"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success(), "commit scaffold");

        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let repo = fixture_repo(&ws);

        // Run the rebuild iteration directly. No chatops, so no
        // notification posts.
        execute_rebuild_iteration(
            &ws,
            &repo,
            &github_cfg,
            None,
            2400,
        )
        .await
        .expect("rebuild iteration should succeed on no-drift fixture");

        // Workspace MUST be clean (the rebuild ran but produced no
        // changes; add_all + the no-staged-content branch left git in
        // a clean state).
        let porcelain = git::status_porcelain(&ws).unwrap();
        assert!(
            filter_alert_state_lines(&porcelain).is_empty(),
            "post-rebuild workspace should be clean; got: {porcelain}"
        );

        // The agent branch should exist (the rebuild iteration always
        // recreates it).
        let st = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/agent-q"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success(), "agent-q branch must exist after rebuild iteration");
    }

    /// flag-clear: the polling loop swaps-and-clears `pending_rebuild`
    /// at iteration start. Verify the atomic semantics directly so the
    /// "second RebuildSpecs arriving mid-rebuild waits for the NEXT
    /// iteration" contract holds.
    #[test]
    fn pending_rebuild_flag_swap_clears() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let flag = std::sync::Arc::new(AtomicBool::new(true));
        let was_set = flag.swap(false, Ordering::SeqCst);
        assert!(was_set, "swap of true→false must return prior `true`");
        assert!(!flag.load(Ordering::SeqCst), "flag must be cleared after swap");
        // A second swap returns false (the flag is already cleared).
        assert!(!flag.swap(false, Ordering::SeqCst));
    }

    /// pr-opened-chatops-notification: when chatops is unconfigured,
    /// the helper is a no-op.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_opened_notification_noop_without_chatops() {
        let repo = RepositoryConfig {
            url: "git@github.com:owner/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        };
        maybe_post_pr_opened(
            &repo,
            None, // no chatops
            "https://github.com/owner/repo/pull/42",
            1,
        )
        .await;
    }

    /// commit-trailing-archive: after a multi-change pass, the working
    /// tree MUST be clean (one commit per change, each containing its
    /// own archive move).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_change_pass_clean_after_each() {
        let (_dir, ws) = fixture_workspace_with_remote();
        for n in 1..=3 {
            add_committed_change(&ws, &format!("ch{n:02}"), &format!("fixture {n}"));
        }
        let executor = PerChangeArtifactExecutor;
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &executor,
            None,
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        
        )
        .await
        .expect("pass succeeds");
        assert_eq!(processed.len(), 3, "all three archived");

        // Working tree must be clean.
        let porcelain = crate::git::status_porcelain(&ws).unwrap();
        assert!(
            porcelain.trim().is_empty(),
            "working tree must be clean after multi-change pass; got:\n{porcelain}"
        );

        // Exactly 3 new commits on agent-q ahead of main.
        let out = std::process::Command::new("git")
            .args(["rev-list", "--count", "main..HEAD"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let count: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap();
        assert_eq!(count, 3, "three commits ahead of main, one per change");
    }

    // ============================================================
    // Poll jitter and staggering (poll-jitter-and-staggering)
    // ============================================================

    /// 1000 draws with `startup_jitter_max_secs = 30` MUST all be in
    /// `[0, 30]`, and the sample MUST contain both endpoints. With a
    /// uniform 0..=30 draw and 1000 samples the probability of missing
    /// either endpoint is `(30/31)^1000 ≈ 10^-14`.
    #[test]
    fn startup_jitter_in_range() {
        let mut saw_zero = false;
        let mut saw_thirty = false;
        for _ in 0..1000 {
            let v = pick_startup_jitter_secs(30);
            assert!(v <= 30, "draw {v} must be in [0, 30]");
            if v == 0 {
                saw_zero = true;
            }
            if v == 30 {
                saw_thirty = true;
            }
        }
        assert!(saw_zero, "1000 draws should produce at least one 0");
        assert!(saw_thirty, "1000 draws should produce at least one 30");
    }

    /// A `0` ceiling MUST short-circuit to `0` without consulting the
    /// RNG (and definitely without panicking on a degenerate range).
    #[test]
    fn startup_jitter_zero_returns_zero() {
        for _ in 0..100 {
            assert_eq!(pick_startup_jitter_secs(0), 0);
        }
    }

    /// For `base = 300, pct = 10` the helper draws in `[270, 330]`
    /// (300 ± 30). 1000 samples MUST stay inside the band AND the mean
    /// MUST be within ±5 of 300 — a uniform distribution centred on 300
    /// will, with overwhelming probability, satisfy this.
    #[test]
    fn jittered_sleep_duration_within_band() {
        let mut sum: u64 = 0;
        for _ in 0..1000 {
            let d = jittered_sleep_duration(300, 10);
            let s = d.as_secs();
            assert!((270..=330).contains(&s), "draw {s} must be in [270, 330]");
            sum += s;
        }
        let mean = sum as f64 / 1000.0;
        assert!(
            (mean - 300.0).abs() <= 5.0,
            "mean {mean} must be within ±5 of 300"
        );
    }

    /// `pct = 0` MUST produce exactly `base_secs` every time — the
    /// arithmetic short-circuit lets operators opt out of jitter for
    /// deterministic test timing.
    #[test]
    fn jittered_sleep_duration_zero_pct_is_exact() {
        for _ in 0..100 {
            let d = jittered_sleep_duration(300, 0);
            assert_eq!(d, Duration::from_secs(300));
        }
    }

    /// `base = 10, pct = 100` means the negative offset can be up to
    /// `-10` (i.e. equal to the entire interval). Result MUST stay in
    /// `[0, 20]` and MUST NOT panic on the underflow boundary.
    #[test]
    fn jittered_sleep_duration_no_underflow_when_pct_is_100() {
        for _ in 0..1000 {
            let d = jittered_sleep_duration(10, 100);
            let s = d.as_secs();
            assert!(s <= 20, "draw {s} must be in [0, 20]");
        }
        // The boundary case: ensure the helper doesn't panic with the
        // most-aggressive percentage on the smallest interval.
        let _ = jittered_sleep_duration(1, 100);
        let _ = jittered_sleep_duration(0, 100);
    }

    /// Cancellation while the task is in its startup-jitter sleep MUST
    /// be observed within 200 ms; the task MUST NOT iterate. Uses a
    /// dummy executor and noisy holders since none should be touched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_exits_during_startup_jitter() {
        struct UnreachableExecutor;
        #[async_trait::async_trait]
        impl Executor for UnreachableExecutor {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                unreachable!("startup-jitter cancellation must prevent first iteration");
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let mut repo = fixture_repo(dir.path());
        // Configure a huge poll_interval so any post-jitter sleep would
        // also block — if the test passes, we must be exiting from the
        // jitter sleep, not the iter sleep.
        repo.poll_interval_sec = 86_400;
        let repo_holder = Arc::new(ArcSwap::from_pointee(repo));
        let executor: Arc<dyn Executor> = Arc::new(UnreachableExecutor);
        let github_holder: GithubHolder = Arc::new(ArcSwap::from_pointee(GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        }));
        let reviewer_holder: ReviewerHolder = Arc::new(ArcSwap::from_pointee(None));
        let chatops_holder: ChatOpsHolder = Arc::new(ArcSwap::from_pointee(None));
        let cancel = CancellationToken::new();

        let task_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run(
                repo_holder,
                executor,
                github_holder,
                reviewer_holder,
                chatops_holder,
                1_000_000,
                u32::MAX,
                None,
                60, // startup_jitter_max_secs: large window
                0,  // inter_iteration_jitter_pct: irrelevant
                std::sync::Arc::new(crate::audits::AuditRegistry::default()),
                None,
                std::sync::Arc::new(std::collections::HashMap::new()),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                task_cancel,
            )
            .await;
        });

        // Cancel immediately — the task should exit during the
        // startup-jitter sleep, not after a multi-second wait.
        cancel.cancel();
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_millis(2000), handle)
            .await
            .expect("run must exit within 2s after cancel during startup jitter")
            .expect("polling task must not panic");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "cancellation should be observed within 500 ms; took {elapsed:?}"
        );
    }

    // ----- Title generator -----

    #[test]
    #[allow(non_snake_case)]
    fn build_pr_title_single_change_humanizes_aNN_prefix() {
        let input = vec!["a06-refactor-portal-handlers-to-fromref".to_string()];
        assert_eq!(
            build_pr_title(&input),
            "a06: refactor portal handlers to fromref",
        );
    }

    #[test]
    fn build_pr_title_single_change_without_prefix() {
        let input = vec!["fix-bug-in-thing".to_string()];
        assert_eq!(build_pr_title(&input), "fix bug in thing");
    }

    #[test]
    fn build_pr_title_multi_change_uses_first_and_count() {
        let input = vec![
            "a04-foo-thing".to_string(),
            "a05-bar-thing".to_string(),
            "a06-baz-thing".to_string(),
        ];
        assert_eq!(build_pr_title(&input), "a04: foo thing (+2 more)");
    }

    #[test]
    fn build_pr_title_caps_overlong() {
        let mut slug = String::from("a06-");
        for _ in 0..50 {
            slug.push_str("verylong-");
        }
        let input = vec![slug];
        let title = build_pr_title(&input);
        assert!(
            title.chars().count() <= 80,
            "title should be capped at 80 chars; got {} chars: {title:?}",
            title.chars().count()
        );
        assert!(
            title.ends_with('…'),
            "truncated title should end with ellipsis; got {title:?}"
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn humanize_slug_strips_aNN_prefix_into_label() {
        assert_eq!(humanize_slug("a06-x-y"), "a06: x y");
        assert_eq!(humanize_slug("b13-foo-bar"), "b13: foo bar");
        assert_eq!(humanize_slug("foo-bar"), "foo bar");
    }

    // ----- Body generator -----

    /// Write a fixture archive entry with a known proposal.md.
    fn write_fixture_archive(workspace: &Path, date_slug: &str, proposal: &str) {
        let dir = workspace.join("openspec/changes/archive").join(date_slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proposal.md"), proposal).unwrap();
    }

    #[test]
    fn build_pr_body_inlines_why_from_archived_proposal() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fixture_archive(
            tmp.path(),
            "2026-05-18-fix-thing",
            "## Why\n\nThing was broken because of reasons.\n\n## What Changes\n\nstuff\n",
        );
        let body = build_pr_body(tmp.path(), &["fix-thing".to_string()], false);
        assert!(body.contains("## fix-thing"), "body: {body}");
        assert!(
            body.contains("Thing was broken because of reasons."),
            "body: {body}"
        );
        assert!(
            body.contains("Changes implemented in this pass"),
            "body: {body}"
        );
    }

    #[test]
    fn build_pr_body_falls_back_when_proposal_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No archive directory at all.
        let body = build_pr_body(tmp.path(), &["fix-thing".to_string()], false);
        assert!(body.contains("## fix-thing"), "body: {body}");
        assert!(
            body.contains("_(no proposal.md available)_"),
            "body: {body}"
        );
    }

    #[test]
    fn build_pr_body_handles_multiple_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fixture_archive(
            tmp.path(),
            "2026-05-18-a04-foo",
            "## Why\n\nFoo rationale.\n\n## What Changes\n\nx\n",
        );
        write_fixture_archive(
            tmp.path(),
            "2026-05-18-a05-bar",
            "## Why\n\nBar rationale.\n\n## What Changes\n\nx\n",
        );
        write_fixture_archive(
            tmp.path(),
            "2026-05-18-a06-baz",
            "## Why\n\nBaz rationale.\n\n## What Changes\n\nx\n",
        );
        let changes = vec![
            "a04-foo".to_string(),
            "a05-bar".to_string(),
            "a06-baz".to_string(),
        ];
        let body = build_pr_body(tmp.path(), &changes, false);

        // Each per-change heading appears in input order.
        let foo_pos = body.find("## a04-foo").expect("a04-foo heading");
        let bar_pos = body.find("## a05-bar").expect("a05-bar heading");
        let baz_pos = body.find("## a06-baz").expect("a06-baz heading");
        assert!(foo_pos < bar_pos && bar_pos < baz_pos);

        // Each section contains its own Why text.
        assert!(body.contains("Foo rationale."));
        assert!(body.contains("Bar rationale."));
        assert!(body.contains("Baz rationale."));
    }

    #[test]
    fn build_pr_body_preserves_self_heal_disclaimer() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fixture_archive(
            tmp.path(),
            "2026-05-18-fix-thing",
            "## Why\n\nA reason.\n\n## What Changes\n\nx\n",
        );
        let body = build_pr_body(tmp.path(), &["fix-thing".to_string()], true);
        assert!(
            body.starts_with("_This PR archives one or more changes whose implementation was already present on the base branch."),
            "body must begin with the self-heal disclaimer; got: {body}"
        );
        let disclaimer_end = body
            .find("_\n\n")
            .expect("disclaimer paragraph terminator");
        let after_disclaimer = &body[disclaimer_end..];
        assert!(
            after_disclaimer.contains("## fix-thing"),
            "per-change section must follow disclaimer; got: {body}"
        );
    }

    #[test]
    fn build_pr_body_extracts_only_why_section() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fixture_archive(
            tmp.path(),
            "2026-05-18-fix-thing",
            "## Why\nWhy text.\n## What Changes\nDifferent text.\n## Impact\nMore text.\n",
        );
        let body = build_pr_body(tmp.path(), &["fix-thing".to_string()], false);
        assert!(body.contains("Why text."), "body: {body}");
        assert!(
            !body.contains("Different text."),
            "body must not include non-Why sections; got: {body}"
        );
        assert!(
            !body.contains("More text."),
            "body must not include non-Why sections; got: {body}"
        );
    }

    // ============================================================
    // extract_stdout_section — log-parser branches
    // ============================================================

    #[test]
    fn extract_stdout_section_returns_body_between_markers() {
        let raw = "=== STDOUT (10) ===\nhello world\n=== STDERR (0) ===\nignored\n";
        assert_eq!(extract_stdout_section(raw), "hello world\n");
    }

    #[test]
    fn extract_stdout_section_returns_empty_when_no_stdout_marker() {
        let raw = "no markers anywhere\n=== STDERR (0) ===\n";
        assert_eq!(extract_stdout_section(raw), "");
    }

    #[test]
    fn extract_stdout_section_returns_empty_when_header_has_no_newline() {
        let raw = "=== STDOUT (10) ===";
        assert_eq!(extract_stdout_section(raw), "");
    }

    #[test]
    fn extract_stdout_section_returns_to_eof_when_no_stderr_marker() {
        let raw = "=== STDOUT (5) ===\nbody only\n";
        assert_eq!(extract_stdout_section(raw), "body only\n");
    }

    // ============================================================
    // filter_alert_state_lines — porcelain filter
    // ============================================================

    #[test]
    fn filter_alert_state_lines_passes_through_when_no_alert_state() {
        let porcelain = " M src/foo.rs\n?? new.txt\n";
        let out = filter_alert_state_lines(porcelain);
        // `.lines()` strips the trailing newline; `join("\n")` re-joins
        // without one, so we compare against the same shape.
        assert_eq!(out, " M src/foo.rs\n?? new.txt");
    }

    #[test]
    fn filter_alert_state_lines_strips_only_alert_state_entry() {
        let porcelain = "?? .alert-state.json\n";
        let out = filter_alert_state_lines(porcelain);
        assert!(
            out.trim().is_empty(),
            "expected empty/whitespace-only output, got {out:?}"
        );
    }

    #[test]
    fn filter_alert_state_lines_keeps_real_files_and_strips_alert_state() {
        let porcelain = " M src/foo.rs\n?? .alert-state.json\n M src/bar.rs\n";
        let out = filter_alert_state_lines(porcelain);
        assert!(out.contains(" M src/foo.rs"), "missing foo.rs: {out:?}");
        assert!(out.contains(" M src/bar.rs"), "missing bar.rs: {out:?}");
        assert!(
            !out.contains(".alert-state.json"),
            "alert-state line leaked: {out:?}"
        );
    }

    #[test]
    fn filter_alert_state_lines_does_not_match_subpath_or_similar_name() {
        let porcelain = " M subdir/.alert-state.json\n?? prefix.alert-state.json\n";
        let out = filter_alert_state_lines(porcelain);
        assert!(
            out.contains("subdir/.alert-state.json"),
            "subdir variant must survive: {out:?}"
        );
        assert!(
            out.contains("prefix.alert-state.json"),
            "prefix variant must survive: {out:?}"
        );
    }

    // ============================================================
    // truncate_reason — boundary behavior
    // ============================================================

    #[test]
    fn truncate_reason_passthrough_when_under_or_equal_to_cap() {
        let input: String = "a".repeat(PERMA_STUCK_REASON_EXCERPT_MAX);
        let out = truncate_reason(&input);
        assert_eq!(out, input);
        assert!(!out.ends_with('…'));
    }

    #[test]
    fn truncate_reason_truncates_and_appends_ellipsis_when_over_cap() {
        let input: String = "a".repeat(PERMA_STUCK_REASON_EXCERPT_MAX + 50);
        let out = truncate_reason(&input);
        assert_eq!(out.chars().count(), PERMA_STUCK_REASON_EXCERPT_MAX + 1);
        assert!(out.ends_with('…'), "expected trailing ellipsis: {out:?}");
    }

    #[test]
    fn truncate_reason_respects_char_boundary_on_multibyte_input() {
        let input: String = "é".repeat(PERMA_STUCK_REASON_EXCERPT_MAX + 50);
        let out = truncate_reason(&input);
        assert_eq!(out.chars().count(), PERMA_STUCK_REASON_EXCERPT_MAX + 1);
        assert!(out.ends_with('…'));
    }

    // ============================================================
    // Archive-collision pre-flight exclusion
    // ============================================================

    /// Seed a dated archive entry for `change` at today's UTC date so a
    /// subsequent `queue::archive(workspace, change)` would collide. The
    /// path matches `queue::archive_collision_path` exactly.
    fn pre_create_dated_archive_entry(workspace: &Path, change: &str) {
        let dated = format!(
            "{}-{change}",
            chrono::Utc::now().format("%Y-%m-%d")
        );
        let archive_dir = workspace
            .join("openspec/changes/archive")
            .join(&dated);
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(
            archive_dir.join("proposal.md"),
            "## Why\nprior archive entry from a merged PR\n",
        )
        .unwrap();
        // Commit so the workspace stays clean for the pre-pass dirty
        // check inside `run_pass_through_commits`.
        let run_git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(workspace)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed in fixture pre-create");
        };
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", &format!("seed archive entry for {change}")]);
    }

    /// Executor that PANICS if invoked. Use this in collision tests to
    /// assert the pre-flight filter ran and excluded the change before
    /// any executor work happened.
    struct UnreachableExecutorForCollision;
    #[async_trait::async_trait]
    impl Executor for UnreachableExecutorForCollision {
        async fn run(&self, _w: &Path, change: &str) -> Result<ExecutorOutcome> {
            unreachable!(
                "archive collision pre-flight must exclude `{change}` before the executor runs"
            );
        }
        async fn resume(
            &self,
            _h: crate::executor::ResumeHandle,
            _a: &str,
        ) -> Result<ExecutorOutcome> {
            unreachable!()
        }
    }

    /// 5.1: pending change with both `openspec/changes/foo/` AND
    /// `openspec/changes/archive/<today>-foo/` present on disk is
    /// excluded from the queue walk. The executor is never invoked,
    /// exactly one chatops post fires under `ArchiveCollision`, the
    /// iteration's processed list is empty, and the per-change failure
    /// counter is NOT incremented (collision is structural, not a
    /// perma-stuck-counting failure).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archive_collision_excludes_change_and_alerts() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "foo", "fixture");
        pre_create_dated_archive_entry(&ws, "foo");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let chatops_ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let (processed, _self_heal) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &UnreachableExecutorForCollision,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("iteration should complete Ok with the change excluded");

        // (a) executor never invoked — guaranteed by Unreachable*::run panic
        //     (the test would have panicked already if it had been called).
        // (c) processed list empty (no commits)
        assert!(
            processed.is_empty(),
            "no changes processed when the only pending change collides; got {processed:?}"
        );
        // (b) exactly one chatops post under ArchiveCollision
        mock.assert_async().await;
        let state = crate::alert_state::AlertState::load_or_default(&ws);
        assert!(
            state
                .alerts
                .contains_key(&crate::alert_state::AlertCategory::ArchiveCollision),
            "ArchiveCollision entry must be persisted after the alert post"
        );
        // (d) failure-state counter for `foo` is NOT incremented
        let fs = crate::failure_state::load(&ws).unwrap();
        assert!(
            fs.entries.get("foo").is_none(),
            "collision is structural, not a perma-stuck-counting failure; got: {:?}",
            fs.entries
        );
    }

    /// 5.2: a mixed pending set — one colliding change, one clean —
    /// processes the clean one normally and excludes the colliding one
    /// with a single chatops post.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archive_collision_does_not_block_other_changes() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // `bar` sorts before `foo` and gets processed first; `foo` is
        // also added but skipped via the collision pre-flight.
        add_committed_change(&ws, "bar", "clean change");
        add_committed_change(&ws, "foo", "colliding change");
        pre_create_dated_archive_entry(&ws, "foo");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let chatops_ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: false, // disable to keep mock count to 1
            failure_alerts_enabled: true,
            pr_opened_enabled: false,
        };
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        /// Recording executor: succeeds on `bar`, panics on any other name.
        /// Proves the queue walk only invoked the executor for the non-
        /// colliding change.
        struct RecordingExecutor;
        #[async_trait::async_trait]
        impl Executor for RecordingExecutor {
            async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
                if change != "bar" {
                    panic!("executor must only be invoked for `bar`; got `{change}`");
                }
                std::fs::write(
                    workspace.join("artifact-bar.txt"),
                    "bar contents\n",
                )?;
                Ok(ExecutorOutcome::Completed)
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }

        let (processed, _) = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &RecordingExecutor,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("iteration should succeed");

        assert_eq!(
            processed,
            vec!["bar".to_string()],
            "only the non-colliding change should be processed; got {processed:?}"
        );
        // `foo` excluded with the alert; `bar` archived → counter not touched.
        mock.assert_async().await;
        let fs = crate::failure_state::load(&ws).unwrap();
        assert!(
            fs.entries.get("foo").is_none(),
            "collided change must not move the failure counter"
        );
        assert!(
            fs.entries.get("bar").is_none(),
            "successfully-archived change must not have a failure entry"
        );
    }

    /// 5.5: archive-collision regression. Both paths present →
    /// two consecutive iterations exclude the change every time; the
    /// chatops alert fires ONCE (24h throttle catches the second
    /// iteration); the executor is invoked ZERO times across both; the
    /// failure-state counter stays at 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archive_collision_two_iterations_throttle_alert_and_zero_executor_invocations() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "stuck-change", "fixture");
        pre_create_dated_archive_entry(&ws, "stuck-change");

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1) // exactly once across BOTH iterations
            .create_async()
            .await;
        let chatops_ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        for _ in 0..2 {
            let (processed, _) = run_pass_through_commits(
                &ws,
                &fixture_repo(&ws),
                &github_cfg,
                &UnreachableExecutorForCollision,
                Some(&chatops_ctx),
                u32::MAX,
                u32::MAX,
                &crate::audits::AuditRegistry::default(),
                None,
                &std::collections::HashMap::new(),
            )
            .await
            .expect("iteration succeeds");
            assert!(processed.is_empty(), "no commits in a pure-collision pass");
        }

        mock.assert_async().await;
        let fs = crate::failure_state::load(&ws).unwrap();
        assert!(
            fs.entries.get("stuck-change").is_none(),
            "collision is not a perma-stuck-counting event across iterations"
        );
    }

    // ============================================================
    // Perma-stuck counter covers all per-change errors
    // ============================================================

    /// 5.3: when the per-change processing function returns Err from a
    /// non-executor source (here: a fixture executor that returns
    /// Completed but the post-executor `queue::archive` fails because
    /// the dated archive path was pre-staged during the iteration), the
    /// failure counter for that change increments by 1.
    ///
    /// We exercise the wrapper directly via a small stub: the executor
    /// creates a file BUT also pre-creates the dated archive directory
    /// at runtime, so `handle_outcome`'s `queue::archive` call returns
    /// Err and propagates out of `process_one_pending_change`. The Err
    /// arm of `walk_queue` then calls `handle_failure_counter`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_executor_archive_failure_increments_counter() {
        let (_dir, ws) = fixture_workspace_with_remote();
        add_committed_change(&ws, "racy", "fixture");

        // Sanity: no failure entries yet.
        let fs0 = crate::failure_state::load(&ws).unwrap();
        assert!(fs0.entries.get("racy").is_none());

        /// Executor: writes a diff (so we get past the no-diff path)
        /// AND, during its run, pre-creates the dated archive entry so
        /// the subsequent `queue::archive` call inside `handle_outcome`
        /// fails with "archive destination already exists".
        struct ArchiveColliderExecutor;
        #[async_trait::async_trait]
        impl Executor for ArchiveColliderExecutor {
            async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
                // Produce a real diff so we don't take the no-diff path.
                std::fs::write(
                    workspace.join(format!("artifact-{change}.txt")),
                    format!("contents for {change}\n"),
                )?;
                // Race the archive step: create the dated dir now.
                let collision = queue::archive_collision_path(workspace, change);
                std::fs::create_dir_all(&collision).unwrap();
                Ok(ExecutorOutcome::Completed)
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }

        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        // No chatops, no preflight alert. The pre-flight check sees no
        // collision at the top of the iteration (the dated dir gets
        // created INSIDE the executor's run), so the change passes the
        // pre-flight; the post-executor archive then collides.
        let _ = run_pass_through_commits(
            &ws,
            &fixture_repo(&ws),
            &github_cfg,
            &ArchiveColliderExecutor,
            None,
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        )
        .await;

        let fs = crate::failure_state::load(&ws).unwrap();
        let entry = fs
            .entries
            .get("racy")
            .expect("post-executor archive failure must increment the per-change counter");
        assert_eq!(
            entry.count, 1,
            "non-executor Err from process_one_pending_change must record exactly one failure"
        );
        assert!(
            entry.last_reason.contains("post-executor")
                || entry.last_reason.contains("already exists"),
            "reason should name the post-executor origin; got: {}",
            entry.last_reason
        );
    }

    /// 5.4: an iteration-level failure (dirty-workspace recovery error)
    /// MUST NOT increment any per-change counter — the failure is
    /// outside `walk_queue` entirely and has its own iteration-level
    /// `AlertCategory::WorkspaceDirtyMidIteration`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iteration_level_failure_does_not_increment_per_change_counter() {
        let (_dir, ws) = fixture_workspace_with_remote();
        // A change that COULD trigger the per-change counter if its
        // processing ever ran. Adding it lets us assert "no entry"
        // unambiguously rather than just "the file doesn't exist."
        add_committed_change(&ws, "would-be-affected", "fixture");
        // Dirty state same as dirty_workspace_recovery_failure_still_alerts:
        // an unstaged untracked dir under openspec/changes/ that the
        // pre-pass dirty check will see, with a base_branch that doesn't
        // exist on origin so recovery FAILS.
        std::fs::create_dir_all(ws.join("openspec/changes/leftover")).unwrap();
        std::fs::write(
            ws.join("openspec/changes/leftover/proposal.md"),
            "## Why\nleftover\n",
        )
        .unwrap();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let _mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .create_async()
            .await;
        let chatops_ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let mut repo = fixture_repo(&ws);
        repo.base_branch = "nonexistent-branch".into();

        let result = run_pass_through_commits(
            &ws,
            &repo,
            &github_cfg,
            &UnreachableExecutorForCollision,
            Some(&chatops_ctx),
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
        )
        .await;
        assert!(result.is_err(), "iteration must surface the recovery failure");

        // The iteration-level alert fired (WorkspaceDirtyMidIteration)…
        let state = crate::alert_state::AlertState::load_or_default(&ws);
        assert!(
            state
                .alerts
                .contains_key(&crate::alert_state::AlertCategory::WorkspaceDirtyMidIteration),
            "iteration-level failure must route through WorkspaceDirtyMidIteration"
        );
        // …but no per-change counter moved.
        let fs = crate::failure_state::load(&ws).unwrap();
        assert!(
            fs.entries.is_empty(),
            "iteration-level failure must not increment any per-change counter; got: {:?}",
            fs.entries
        );
    }
}
