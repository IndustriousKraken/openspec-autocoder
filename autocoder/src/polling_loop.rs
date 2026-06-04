//! Per-repository polling loop. Each iteration runs a single pass: branch
//! init → queue walk → push + PR if commits were produced. Failures inside
//! one iteration are logged and the loop continues to the next sleep.

use crate::alert_state::{AlertCategory, AlertEntry, AlertState};
use crate::alerts::{handle_classified_recovery_failure, handle_predictable_failure};
use crate::audits::AuditRegistry;
use crate::audits::scheduler::run_due_audits;
use crate::busy_marker;
use crate::chatops::{self, AnswerPayload, ChatOpsBackend, QuestionPayload};
use crate::code_reviewer::{
    CodeReviewer, PerChangeContext, PerChangeReview, PerChangeSection, ReviewConcern,
    ReviewReport, ReviewVerdict, build_cross_change_preamble,
};
use crate::config::{AuditSettings, AuditsConfig, GithubConfig, RepositoryConfig};
use crate::control_socket::{ChatOpsHolder, ChatOpsSlot, GithubHolder, ReviewerHolder};
use crate::executor::{Executor, ExecutorOutcome, ResumeHandle, UnimplementableTask};
use crate::paths::DaemonPaths;
use crate::recovery_classification::{RecoveryFailureClass, classify_recovery_failure};
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
#[allow(clippy::too_many_arguments)]
pub async fn run(
    paths: Arc<DaemonPaths>,
    repo: Arc<ArcSwap<RepositoryConfig>>,
    executor: Arc<dyn Executor>,
    github_holder: GithubHolder,
    reviewer_holder: ReviewerHolder,
    chatops_holder: ChatOpsHolder,
    stuck_threshold_secs: u64,
    perma_stuck_threshold: u32,
    executor_max_changes_per_pr: Option<u32>,
    revision_cap: u32,
    startup_jitter_max_secs: u64,
    inter_iteration_jitter_pct: u8,
    audit_registry: Arc<AuditRegistry>,
    audits_cfg: Option<Arc<AuditsConfig>>,
    audit_settings: Arc<HashMap<String, AuditSettings>>,
    pending_rebuild: Arc<std::sync::atomic::AtomicBool>,
    pending_triages: Arc<std::sync::Mutex<Vec<String>>>,
    pending_audit_runs: Arc<std::sync::Mutex<Vec<String>>>,
    pending_proposal_requests: Arc<
        std::sync::Mutex<Vec<crate::control_socket::ProposalRequest>>,
    >,
    pending_changelog_requests: Arc<
        std::sync::Mutex<Vec<crate::control_socket::ChangelogRequest>>,
    >,
    pending_brownfield_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::BrownfieldRequest>,
        >,
    >,
    pending_scout_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::ScoutRequest>,
        >,
    >,
    pending_spec_it_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::SpecItRequest>,
        >,
    >,
    pending_sync_upstream_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::SyncUpstreamRequest>,
        >,
    >,
    pending_brownfield_survey_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::BrownfieldSurveyRequest>,
        >,
    >,
    pending_brownfield_batch_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::BrownfieldBatchRequest>,
        >,
    >,
    iteration_cancel: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    iteration_drained: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
) {
    run_with_hooks(
        paths,
        repo,
        executor,
        github_holder,
        reviewer_holder,
        chatops_holder,
        stuck_threshold_secs,
        perma_stuck_threshold,
        executor_max_changes_per_pr,
        revision_cap,
        startup_jitter_max_secs,
        inter_iteration_jitter_pct,
        audit_registry,
        audits_cfg,
        audit_settings,
        pending_rebuild,
        pending_triages,
        pending_audit_runs,
        pending_proposal_requests,
        pending_changelog_requests,
        pending_brownfield_requests,
        pending_scout_requests,
        pending_spec_it_requests,
        pending_sync_upstream_requests,
        pending_brownfield_survey_requests,
        pending_brownfield_batch_requests,
        iteration_cancel,
        iteration_drained,
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

/// Drops at the end of the iteration body — including the panic-unwind
/// path — so the per-iteration cancel handle is cleared and the
/// `iteration_drained` Notify fires from every exit path without manual
/// repetition. The wipe-workspace handler awaits the Notify after firing
/// the per-iteration cancel; the drop ensures it always wakes.
struct IterationGuard<'a> {
    iteration_cancel: &'a std::sync::Mutex<Option<CancellationToken>>,
    iteration_drained: &'a tokio::sync::Notify,
}

impl Drop for IterationGuard<'_> {
    fn drop(&mut self) {
        *self.iteration_cancel.lock().unwrap() = None;
        self.iteration_drained.notify_waiters();
    }
}

/// Same as `run` but accepts a `RunHooks` for test-only synchronization.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_hooks(
    paths: Arc<DaemonPaths>,
    repo: Arc<ArcSwap<RepositoryConfig>>,
    executor: Arc<dyn Executor>,
    github_holder: GithubHolder,
    reviewer_holder: ReviewerHolder,
    chatops_holder: ChatOpsHolder,
    stuck_threshold_secs: u64,
    perma_stuck_threshold: u32,
    executor_max_changes_per_pr: Option<u32>,
    revision_cap: u32,
    startup_jitter_max_secs: u64,
    inter_iteration_jitter_pct: u8,
    audit_registry: Arc<AuditRegistry>,
    audits_cfg: Option<Arc<AuditsConfig>>,
    audit_settings: Arc<HashMap<String, AuditSettings>>,
    pending_rebuild: Arc<std::sync::atomic::AtomicBool>,
    pending_triages: Arc<std::sync::Mutex<Vec<String>>>,
    pending_audit_runs: Arc<std::sync::Mutex<Vec<String>>>,
    pending_proposal_requests: Arc<
        std::sync::Mutex<Vec<crate::control_socket::ProposalRequest>>,
    >,
    pending_changelog_requests: Arc<
        std::sync::Mutex<Vec<crate::control_socket::ChangelogRequest>>,
    >,
    pending_brownfield_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::BrownfieldRequest>,
        >,
    >,
    pending_scout_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::ScoutRequest>,
        >,
    >,
    pending_spec_it_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::SpecItRequest>,
        >,
    >,
    pending_sync_upstream_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::SyncUpstreamRequest>,
        >,
    >,
    pending_brownfield_survey_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::BrownfieldSurveyRequest>,
        >,
    >,
    pending_brownfield_batch_requests: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<crate::control_socket::BrownfieldBatchRequest>,
        >,
    >,
    iteration_cancel: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    iteration_drained: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
    hooks: RunHooks,
) {
    {
        let initial = repo.load();
        let workspace = workspace::resolve_path(&paths, initial.as_ref());
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

        // Per-iteration cancel token — a child of the global cancel so
        // SIGINT/SIGTERM still propagates. The wipe-workspace control-
        // socket handler fires this token to ask the in-flight iteration
        // to drain cleanly before deleting the workspace. The
        // IterationGuard below clears the slot and fires the
        // iteration_drained Notify on every exit path (normal, error,
        // panic) so the wipe handler always wakes.
        let iter_cancel = cancel.child_token();
        *iteration_cancel.lock().unwrap() = Some(iter_cancel.clone());
        let iter_guard = IterationGuard {
            iteration_cancel: iteration_cancel.as_ref(),
            iteration_drained: iteration_drained.as_ref(),
        };

        // Single-snapshot-per-iteration: read `repo`, `github`, `reviewer`,
        // and `chatops` exactly once at the top of the iteration so a
        // mid-iteration reload cannot tear the config.
        let snapshot = repo.load();
        let snapshot_ref: &RepositoryConfig = snapshot.as_ref();
        let workspace = workspace::resolve_path(&paths, snapshot_ref);
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

        // Audit-thread state housekeeping runs first: prune any audit-
        // thread state files older than 7 days regardless of status, so
        // the audit-threads directory stays bounded. Best-effort; a
        // failure is logged and the iteration continues.
        let audit_state_root = crate::audits::threads::default_state_root(&paths);
        match crate::audits::threads::prune_stale_entries(
            &audit_state_root,
            chrono::Duration::days(7),
        ) {
            Ok(0) => {}
            Ok(n) => tracing::debug!(
                count = n,
                "audit-threads prune removed {n} stale entry(ies)"
            ),
            Err(e) => tracing::warn!(
                "audit-threads prune failed (iteration continues): {e:#}"
            ),
        }

        // Same housekeeping for proposal-request state files (per
        // `chat-request-triage`). Stale entries (>7 days) are removed
        // regardless of status so the directory stays bounded.
        let proposal_state_root = crate::proposal_requests::default_state_root(&paths);
        match crate::proposal_requests::prune_stale_entries(
            &proposal_state_root,
            chrono::Duration::days(7),
        ) {
            Ok(0) => {}
            Ok(n) => tracing::debug!(
                count = n,
                "proposal-requests prune removed {n} stale entry(ies)"
            ),
            Err(e) => tracing::warn!(
                "proposal-requests prune failed (iteration continues): {e:#}"
            ),
        }

        // Same housekeeping for changelog-request state files (per
        // `a06-chat-driven-changelog`). Stale entries (>7 days) are
        // removed regardless of status so the directory stays bounded.
        let changelog_state_root = crate::changelog_requests::default_state_root(&paths);
        match crate::changelog_requests::prune_stale_entries(
            &changelog_state_root,
            chrono::Duration::days(7),
        ) {
            Ok(0) => {}
            Ok(n) => tracing::debug!(
                count = n,
                "changelog-requests prune removed {n} stale entry(ies)"
            ),
            Err(e) => tracing::warn!(
                "changelog-requests prune failed (iteration continues): {e:#}"
            ),
        }

        // Drain the per-repo on-demand audit-run queue
        // (chatops-on-demand-audit-trigger). The HashSet collapses any
        // duplicates that survived the control-socket-level de-dup
        // (defence in depth) so the same audit cannot run twice in one
        // iteration. The queue is emptied unconditionally even when an
        // entry is unknown to the registry — leaving it would cause the
        // unknown name to be re-warned every iteration forever.
        let queued_audit_types: std::collections::HashSet<String> = {
            let mut g = pending_audit_runs.lock().unwrap();
            std::mem::take(&mut *g).into_iter().collect()
        };

        // Drain the per-repo triage queue (audit-reply-acts `send it`).
        // Triage runs BEFORE the rebuild check and the pending-change
        // walk so an operator's `send it` always gets attention this
        // iteration. Failures inside `process_audit_triages` are logged
        // and never abort the surrounding iteration.
        let triage_thread_tses: Vec<String> = {
            let mut g = pending_triages.lock().unwrap();
            std::mem::take(&mut *g)
        };
        if !triage_thread_tses.is_empty()
            && let Err(error) = process_audit_triages(
                &paths,
                &workspace,
                snapshot_ref,
                executor.as_ref(),
                &github_snap,
                chatops_ctx.as_ref(),
                &triage_thread_tses,
            )
            .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                "audit-triage processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Drain the per-repo proposal-request queue (chat-request-triage
        // `propose`). Same placement contract as the audit-triage drain
        // above: runs BEFORE the rebuild check and the pending-change
        // walk so an operator's `propose` always gets attention this
        // iteration. Failures inside `process_proposal_requests` are
        // logged and never abort the surrounding iteration.
        let proposal_requests_batch: Vec<crate::control_socket::ProposalRequest> = {
            let mut g = pending_proposal_requests.lock().unwrap();
            std::mem::take(&mut *g)
        };
        if !proposal_requests_batch.is_empty()
            && let Err(error) = process_proposal_requests(
                &paths,
                &workspace,
                snapshot_ref,
                executor.as_ref(),
                &github_snap,
                chatops_ctx.as_ref(),
                &proposal_requests_batch,
            )
            .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                "chat-triage processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Drain the per-repo changelog-request queue
        // (`a06-chat-driven-changelog`). Runs immediately after the
        // proposal-request drain AND before the pending-change walk so an
        // operator's `@<bot> changelog ...` always gets attention this
        // iteration.
        let changelog_requests_batch: Vec<crate::control_socket::ChangelogRequest> = {
            let mut g = pending_changelog_requests.lock().unwrap();
            std::mem::take(&mut *g)
        };
        if !changelog_requests_batch.is_empty()
            && let Err(error) = crate::changelog_triage::process_changelog_requests(
                &paths,
                &workspace,
                snapshot_ref,
                executor.as_ref(),
                &github_snap,
                chatops_ctx.as_ref(),
                &changelog_requests_batch,
            )
            .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                "changelog-request processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Drain at most ONE brownfield request per iteration (per the
        // a23 spec). The handler reverts the workspace on failure so a
        // sandboxed leak doesn't bleed into the standard change-
        // processing pass that follows. Failures are logged but never
        // abort the surrounding iteration.
        let brownfield_request: Option<crate::control_socket::BrownfieldRequest> = {
            let mut g = pending_brownfield_requests.lock().unwrap();
            g.pop_front()
        };
        if let Some(req) = brownfield_request
            && let Err(error) = crate::polling::brownfield::process_pending_brownfield(
                &paths,
                &workspace,
                snapshot_ref,
                executor.as_ref(),
                &github_snap,
                chatops_ctx.as_ref(),
                &req,
            )
            .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                request_id = req.request_id.as_str(),
                "brownfield-request processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Drain at most ONE scout request per iteration (a25). The
        // handler invokes the executor in scout mode (read-only
        // sandbox) AND persists the result to disk. Failures are
        // logged but never abort the surrounding iteration.
        let scout_request: Option<crate::control_socket::ScoutRequest> = {
            let mut g = pending_scout_requests.lock().unwrap();
            g.pop_front()
        };
        if let Some(req) = scout_request
            && let Err(error) = crate::polling::scout::process_pending_scout(
                &workspace,
                snapshot_ref,
                executor.as_ref(),
                chatops_ctx.as_ref(),
                &req,
            )
            .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                request_id = req.request_id.as_str(),
                "scout-request processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Drain at most ONE spec-it request per iteration (a25). The
        // handler translates the scouted item into a `ProposalRequest`
        // AND pushes it onto the proposal-request queue for the
        // standard propose lifecycle to consume on the next iteration.
        let spec_it_request: Option<crate::control_socket::SpecItRequest> = {
            let mut g = pending_spec_it_requests.lock().unwrap();
            g.pop_front()
        };
        if let Some(req) = spec_it_request
            && let Err(error) = crate::polling::spec_it::process_pending_spec_it(
                &paths,
                &workspace,
                snapshot_ref,
                chatops_ctx.as_ref(),
                pending_proposal_requests.clone(),
                &req,
            )
            .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                scout_request_id = req.scout_request_id.as_str(),
                item_id = req.item_id,
                "spec-it-request processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // OSS-fork support (a26): drain at most ONE sync-upstream
        // request per iteration. The handler fetches the configured
        // upstream remote, rebases the workspace's base branch, AND
        // posts a thread reply summarizing the result OR naming
        // conflicting files. NEVER pushes — the operator decides when
        // to push to their fork.
        let sync_upstream_request: Option<crate::control_socket::SyncUpstreamRequest> = {
            let mut g = pending_sync_upstream_requests.lock().unwrap();
            g.pop_front()
        };
        if let Some(req) = sync_upstream_request
            && let Err(error) = crate::polling::sync_upstream::process_pending_sync_upstream(
                &workspace,
                snapshot_ref,
                chatops_ctx.as_ref(),
                &req,
            )
            .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                request_id = req.request_id.as_str(),
                "sync-upstream-request processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Drain at most ONE brownfield-survey request per iteration
        // (a29). The handler invokes the executor in survey mode
        // (read-only sandbox) AND persists the result to disk.
        let brownfield_survey_request: Option<
            crate::control_socket::BrownfieldSurveyRequest,
        > = {
            let mut g = pending_brownfield_survey_requests.lock().unwrap();
            g.pop_front()
        };
        if let Some(req) = brownfield_survey_request
            && let Err(error) =
                crate::polling::brownfield_survey::process_pending_brownfield_survey(
                    &workspace,
                    snapshot_ref,
                    executor.as_ref(),
                    chatops_ctx.as_ref(),
                    &req,
                )
                .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                request_id = req.request_id.as_str(),
                "brownfield-survey-request processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Drain at most ONE brownfield-batch action per iteration
        // (a29). The action only flips the survey state to InProgress
        // AND posts an ack; the actual item drain happens immediately
        // afterwards in `drain_next_brownfield_batch_item` so the
        // first item starts on the next iteration AS the spec
        // promises.
        let brownfield_batch_request: Option<
            crate::control_socket::BrownfieldBatchRequest,
        > = {
            let mut g = pending_brownfield_batch_requests.lock().unwrap();
            g.pop_front()
        };
        if let Some(req) = brownfield_batch_request
            && let Err(error) =
                crate::polling::brownfield_batch::process_pending_brownfield_batch(
                    &paths,
                    &workspace,
                    snapshot_ref,
                    executor.as_ref(),
                    &github_snap,
                    chatops_ctx.as_ref(),
                    &req,
                )
                .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                survey_request_id = req.survey_request_id.as_str(),
                "brownfield-batch-request processing errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        // Drain one in-progress batch item per iteration (a29). This
        // pass runs every iteration regardless of whether an action
        // arrived — once a survey is `InProgress` it owns the per-
        // iteration item-drain slot.
        if let Err(error) =
            crate::polling::brownfield_batch::drain_next_brownfield_batch_item(
                &paths,
                &workspace,
                snapshot_ref,
                executor.as_ref(),
                &github_snap,
                chatops_ctx.as_ref(),
            )
            .await
        {
            tracing::error!(
                url = snapshot_ref.url.as_str(),
                "brownfield-batch item drain errored for {}: {error:#}",
                snapshot_ref.url
            );
        }

        if want_rebuild {
            if let Err(error) = execute_rebuild_iteration(
                &paths,
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
            &paths,
            &workspace,
            snapshot_ref,
            executor.as_ref(),
            &github_snap,
            reviewer_snap.as_deref(),
            chatops_ctx.as_ref(),
            stuck_threshold_secs,
            perma_stuck_threshold,
            max_changes_per_pr,
            revision_cap,
            audit_registry.as_ref(),
            audits_cfg.as_deref(),
            audit_settings.as_ref(),
            &queued_audit_types,
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
        // Iteration body is done — drop the IterationGuard explicitly so
        // `iteration_cancel` is cleared to `None` (the "no iteration in
        // flight" state) and the `iteration_drained` Notify fires before
        // we enter the inter-iteration sleep. A wipe-workspace handler
        // arriving during the sleep then short-circuits straight to the
        // deletion without waiting on a drain that already happened.
        drop(iter_guard);
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
/// OSS-fork support (a26): opportunistic upstream fetch. When the
/// per-repo `upstream` block is configured, ensure a remote named
/// `<upstream.remote>` exists pointing at `<upstream.url>` AND run
/// `git fetch <remote>` with a 30-second timeout. The fetch is
/// best-effort: any error (remote-add failure, fetch timeout, auth
/// failure) logs a WARN naming the failure AND the function returns
/// without affecting the iteration.
fn opportunistic_upstream_fetch(workspace: &Path, repo: &RepositoryConfig) {
    let Some(upstream) = repo.upstream.as_ref() else {
        return;
    };
    if let Err(e) = git::ensure_remote(workspace, &upstream.remote, &upstream.url) {
        tracing::warn!(
            url = %repo.url,
            remote = %upstream.remote,
            upstream_url = %upstream.url,
            "opportunistic upstream remote-management failed: {e:#}; continuing iteration"
        );
        return;
    }
    if let Err(e) = git::fetch_remote_with_timeout(workspace, &upstream.remote, 30) {
        tracing::warn!(
            url = %repo.url,
            remote = %upstream.remote,
            upstream_url = %upstream.url,
            "opportunistic upstream fetch failed: {e:#}; continuing iteration"
        );
    }
}

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
#[allow(clippy::too_many_arguments)]
pub async fn execute_one_pass(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    github_cfg: &GithubConfig,
    reviewer: Option<&CodeReviewer>,
    chatops_ctx: Option<&ChatOpsContext>,
    stuck_threshold_secs: u64,
    perma_stuck_threshold: u32,
    max_changes_per_pr: u32,
    revision_cap: u32,
    audit_registry: &AuditRegistry,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
    queued_audit_types: &std::collections::HashSet<String>,
) -> Result<()> {
    // Acquire the per-repo busy marker. Held across the entire pass
    // (executor → review → push → PR); released by Drop on every return.
    // A crash that bypasses Drop leaves the marker for the next pass to
    // detect and (depending on age + PID liveness) auto-recover from.
    let mut guard = match busy_marker::try_acquire(paths, workspace, &repo.url, stuck_threshold_secs) {
        Ok(busy_marker::AcquireOutcome::Acquired(g)) => g,
        Ok(busy_marker::AcquireOutcome::SkipFreshInProgress(details)) => {
            tracing::info!(
                url = %repo.url,
                pid = details.marker.pid,
                stage = %details.marker.stage.as_str(),
                age = %busy_marker::format_age_human(details.age_secs),
                threshold = %busy_marker::format_age_human(details.threshold_secs),
                pid_alive = details.pid_alive,
                recovery_eligible = details.recovery_eligible(),
                "busy marker present; skipping iteration"
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

    // Run the PR-comment revision dispatcher BEFORE the open-PR
    // short-circuit so revisions reach open PRs. A v1 simplification:
    // when `revision_cap` is `0`, the feature is disabled entirely.
    if revision_cap > 0 {
        let chatops_ctx_for_revisions = chatops_ctx.map(|c| crate::revisions::ChatOpsCtx {
            chatops: c.chatops.as_ref(),
            channel: c.channel.as_str(),
            failure_alerts_enabled: c.failure_alerts_enabled,
        });
        if let Err(e) = crate::revisions::process_revision_requests(
            paths,
            workspace,
            repo,
            github_cfg,
            reviewer,
            executor,
            chatops_ctx_for_revisions,
            revision_cap,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        {
            tracing::warn!(
                url = %repo.url,
                "revision dispatcher errored (iteration continues): {e:#}"
            );
        }
        // Same dispatcher pattern for chat-driven changelog PRs (per
        // `a06-chat-driven-changelog`): walk open PRs whose head matches
        // `changelog-*` AND re-run the stylist on revision triggers.
        if let Err(e) = crate::changelog_triage::process_changelog_revision_requests(
            paths,
            workspace,
            repo,
            github_cfg,
            executor,
            chatops_ctx,
        )
        .await
        {
            tracing::warn!(
                url = %repo.url,
                "changelog-revision dispatcher errored (iteration continues): {e:#}"
            );
        }
    }

    // Before doing any iteration work, check whether an open PR already
    // exists on the agent branch. If yes, this iteration would burn
    // tokens re-implementing, force-update the PR's commits under any
    // reviewer mid-review, and 422 at PR creation. Skip entirely.
    if open_pr_exists_for_agent_branch(paths, repo, github_cfg).await {
        return Ok(());
    }
    let (processed, includes_self_heal) = run_pass_through_commits(
        paths,
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
        queued_audit_types,
    )
    .await?;

    // a34: "detect working-tree state" prelude per the canonical
    // orchestrator-cli requirement. Before the iteration's commit +
    // push + PR step, classify the iteration's outcome by probing the
    // spec_storage tree's uncommitted state. The workspace's status
    // is implicit: the agent-branch commit count below is the
    // primary signal for "code-only has work". The spec_storage
    // tree's dirty state — populated by brownfield / scout spec-it /
    // archive flows when `spec_storage` is configured — is logged
    // here so operators see which routing branch the iteration is
    // about to take. The full spec-storage commit + push + PR fanout
    // lives in `crate::spec_storage_routing` AND is exercised by the
    // brownfield / scout / archive callers when they route through
    // the new helpers.
    let spec_storage_resolved = repo.resolved_spec_storage_dir(workspace);
    let spec_storage_dirty = match spec_storage_resolved.as_deref() {
        Some(p) => match git::status_porcelain(p) {
            Ok(s) => !s.is_empty(),
            Err(e) => {
                tracing::warn!(
                    url = %repo.url,
                    spec_storage_path = %p.display(),
                    "spec_storage status_porcelain probe failed; treating tree as clean: {e:#}"
                );
                false
            }
        },
        None => false,
    };

    // Termination is gated EXCLUSIVELY on the agent branch's commit count
    // relative to base — see `polling-iteration-termination-is-commit-count
    // -gated`. Using `processed.is_empty()` would miss commits produced by
    // the audit phase that runs AFTER the queue walk, silently dropping
    // them on the next iteration's recreate_branch step.
    let range = format!("{}..{}", repo.base_branch, repo.agent_branch);
    let commit_count = git::rev_list_count(workspace, &range)?;
    if commit_count == 0 {
        if spec_storage_dirty {
            tracing::info!(
                url = %repo.url,
                spec_storage_path = ?spec_storage_resolved.as_ref().map(|p| p.display().to_string()),
                "a34: spec_storage tree dirty AND workspace has no commits — spec-only iteration classified; spec-storage routing handled by the originating brownfield / scout / archive caller"
            );
        } else {
            tracing::info!(
                url = repo.url.as_str(),
                "polling pass produced no commits (all completed changes had empty diffs)"
            );
        }
        let _ = AlertState::clear(paths, workspace);
        return Ok(());
    }
    if spec_storage_dirty {
        tracing::info!(
            url = %repo.url,
            spec_storage_path = ?spec_storage_resolved.as_ref().map(|p| p.display().to_string()),
            workspace_commit_count = commit_count,
            "a34: dual-tree iteration classified — workspace commits push as code-only PR; spec-storage routing handled by the originating brownfield / scout / archive caller"
        );
    }

    // a38: audit-only-PR suppression on iteration-pending state. When
    // any `.iteration-pending.json` marker is present in the workspace,
    // the agent-branch's commits-ahead-of-master include iteration_request
    // WIP that is explicitly not ready to ship (per a27a1). Opening a PR
    // on top of that WIP produces a "0 change(s)" PR that misleads the
    // operator AND, if merged, locks in half-done iteration work.
    // Suppress the push + PR steps for this iteration; audit-produced
    // commits (if any) remain on agent-q AND ship in the next iteration
    // after the iteration-pending change concludes via outcome_success,
    // outcome_spec_needs_revision, OR the a27a1 5-iteration cap.
    let pending_iteration_changes = {
        let basename = workspace
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        crate::iteration_pending::list_pending_changes(paths, basename)
    };
    if !pending_iteration_changes.is_empty() {
        tracing::info!(
            url = %repo.url,
            pending = %pending_iteration_changes.join(","),
            workspace_commit_count = commit_count,
            "a38: audit-only PR path suppressed: iteration-pending markers present for {}; deferring push + PR until iteration sequence concludes",
            pending_iteration_changes.join(", ")
        );
        let _ = AlertState::clear(paths, workspace);
        return Ok(());
    }

    // Reviewer step (if configured) runs against the produced commits BEFORE
    // the push + PR. A failed reviewer is non-fatal: PR still ships with a
    // "(reviewer failed)" note in the body.
    //
    // When `reviewer.auto_revise` is enabled, the per-concern
    // `should_request_revision` records drive the reviewer-initiated
    // revision pipeline regardless of the verdict. Concerns are
    // partitioned against the per-PR cap budget here; the taken set is
    // queued to be posted as `<!-- reviewer-revision -->` PR comments
    // after the PR is created, and the dropped set is annotated into the
    // `## Code Review` PR-body section so the human sees what was skipped.
    // a34 §6: when `reviewer.skip_spec_only_prs: true` AND the PR's
    // diff lives entirely under `openspec/`, skip the reviewer call
    // (cost-optimization knob). The detection mirrors the iteration's
    // commit + push classification — a PR opened from a spec-only
    // iteration's classification is a spec-only PR; a code-only
    // iteration's PR (including dual-tree's code half) is NOT.
    let skip_reviewer_for_spec_only_pr = if let Some(r) = reviewer
        && r.skip_spec_only_prs()
    {
        let diff_paths = git::diff_files_changed(
            workspace,
            &repo.base_branch,
            &repo.agent_branch,
        )
        .unwrap_or_default();
        let spec_only =
            crate::spec_storage_routing::diff_is_spec_only(&diff_paths);
        if spec_only {
            tracing::info!(
                url = %repo.url,
                "reviewer: skipping spec-only PR per skip_spec_only_prs config"
            );
        }
        spec_only
    } else {
        false
    };

    let (review_report, draft, reviewer_revision_concerns) = if processed.is_empty()
        || skip_reviewer_for_spec_only_pr
    {
        // Audit-only iteration: no implementer-touched files to evaluate.
        // The audit's own validation pass already gated each proposal, so
        // the reviewer would either error against an empty `processed`
        // list or produce a meaningless review of mechanical
        // proposal-writing. Skip the reviewer entirely.
        //
        // a34 §6 also skips here when `reviewer.skip_spec_only_prs` AND
        // the iteration's diff is entirely under `openspec/`.
        (None, false, Vec::new())
    } else {
        match reviewer {
            None => (None, false, Vec::new()),
            Some(r) => {
                let _ = guard.set_stage(busy_marker::Stage::Review);
                let outcome = match r.mode() {
                    crate::config::ReviewerMode::Bundled => {
                        let ctx = build_review_context(workspace, repo, &processed)?;
                        r.review(&ctx).await
                    }
                    crate::config::ReviewerMode::PerChange => {
                        let contexts =
                            build_per_change_contexts(workspace, repo, &processed)?;
                        r.review_per_change(&contexts).await.map(|per_change| {
                            synthesize_per_change_report(per_change)
                        })
                    }
                };
                match outcome {
                    Ok(mut report) => {
                        let draft = matches!(report.verdict, ReviewVerdict::Block);
                        let taken = if r.auto_revise() {
                            partition_and_annotate_reviewer_revisions(&mut report, revision_cap)
                        } else {
                            Vec::new()
                        };
                        (Some(report), draft, taken)
                    }
                    Err(e) => {
                        tracing::error!("reviewer failed: {e:#}");
                        let synthetic = ReviewReport {
                            verdict: ReviewVerdict::Concerns,
                            markdown: format!("(reviewer failed: {e})"),
                            concerns: Vec::new(),
                            per_change_sections: Vec::new(),
                        };
                        (Some(synthetic), false, Vec::new())
                    }
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
            paths,
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
        paths,
        repo,
        github_cfg,
        &processed,
        includes_self_heal,
        review_report.as_ref(),
        reviewer,
        revision_cap,
        draft,
        &reviewer_revision_concerns,
        chatops_ctx,
        workspace,
    )
    .await?;
    // End-of-pass success: push and PR creation both succeeded. Clear the
    // entire alert-state map so the next failure (whatever category) re-
    // alerts immediately. Per design.md, this is intentionally coarse —
    // any successful iteration resets every category's throttle.
    if let Err(e) = AlertState::clear(paths, workspace) {
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
pub(crate) fn build_review_context(
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

/// Assemble one `PerChangeContext` per change in `processed`, used by
/// the reviewer's `per_change` mode dispatch. Each per-change context is
/// scoped to:
/// - the change's own brief (proposal/design/tasks),
/// - the diff of the commit(s) for that change (NOT the union diff),
/// - the workspace-state contents of the files touched by those commits,
/// - a cross-change preamble naming the OTHER changes in the same pass.
///
/// Commits are located by subject-prefix (`<change>:`) using
/// `git::commits_for_change`. A change with no matching commit (or whose
/// touched-file list is empty) still produces a context, but with an
/// empty diff/files set — the reviewer's prompt for that change still
/// includes the brief + preamble, so the operator sees a deliberate
/// `## Code Review: <slug>` section instead of a silent skip.
fn build_per_change_contexts(
    workspace: &Path,
    repo: &RepositoryConfig,
    processed: &[String],
) -> Result<Vec<PerChangeContext>> {
    // First pass: gather briefs for all changes. The cross-change
    // preamble for change `i` needs the OTHER changes' briefs in full,
    // so we collect them all first.
    let archive_root = workspace.join("openspec/changes/archive");
    let mut briefs: Vec<crate::code_reviewer::ChangeBrief> =
        Vec::with_capacity(processed.len());
    for name in processed {
        let dir = match locate_archive_dir(&archive_root, name)? {
            Some(d) => d,
            None => {
                tracing::warn!(
                    change = %name,
                    "archive directory not found while building per-change review context"
                );
                continue;
            }
        };
        let proposal = std::fs::read_to_string(dir.join("proposal.md")).unwrap_or_default();
        let design = std::fs::read_to_string(dir.join("design.md")).ok();
        let tasks = std::fs::read_to_string(dir.join("tasks.md")).unwrap_or_default();
        briefs.push(crate::code_reviewer::ChangeBrief {
            name: name.clone(),
            proposal,
            design,
            tasks,
        });
    }

    let mut contexts: Vec<PerChangeContext> = Vec::with_capacity(briefs.len());
    for brief in &briefs {
        let shas = git::commits_for_change(
            workspace,
            &repo.base_branch,
            &repo.agent_branch,
            &brief.name,
        )
        .unwrap_or_else(|e| {
            tracing::warn!(
                change = %brief.name,
                "git log --grep failed locating per-change commits; falling back to empty list: {e:#}"
            );
            Vec::new()
        });
        let diff = if shas.is_empty() {
            String::new()
        } else {
            git::diff_for_commits(workspace, &shas).unwrap_or_default()
        };
        let file_paths = if shas.is_empty() {
            Vec::new()
        } else {
            git::files_for_commits(workspace, &shas).unwrap_or_default()
        };
        let mut changed_files = Vec::with_capacity(file_paths.len());
        for path in &file_paths {
            let abs = workspace.join(path);
            match std::fs::read_to_string(&abs) {
                Ok(contents) => {
                    changed_files.push(crate::code_reviewer::ChangedFile {
                        path: path.clone(),
                        contents,
                    });
                }
                // Deleted files have no current content but still appear
                // in the per-change diff — that's fine, the diff body
                // captures the deletion.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!(
                        path = %path,
                        change = %brief.name,
                        "skipping per-change file read for reviewer: {e}"
                    );
                    continue;
                }
            }
        }

        let context = crate::code_reviewer::ReviewContext {
            archived_changes: vec![brief.clone()],
            changed_files,
            diff,
        };
        let preamble = build_cross_change_preamble(&brief.name, &briefs);
        contexts.push(PerChangeContext {
            change_slug: brief.name.clone(),
            context,
            cross_change_preamble: preamble,
        });
    }
    Ok(contexts)
}

/// Aggregate a `Vec<PerChangeReview>` into one `ReviewReport` whose
/// `per_change_sections` drives the PR-body composer to emit one
/// `## Code Review: <slug>` section per element. The aggregate
/// `verdict` is the worst across sections (`Block` > `Concerns` >
/// `Pass`). The flat `concerns` vec is the union of each per-change
/// report's concerns, used by the auto-revise pipeline.
fn synthesize_per_change_report(per_change: Vec<PerChangeReview>) -> ReviewReport {
    let mut verdict = ReviewVerdict::Pass;
    let mut concerns: Vec<ReviewConcern> = Vec::new();
    let mut sections: Vec<PerChangeSection> = Vec::with_capacity(per_change.len());
    for pcr in per_change {
        verdict = worst_verdict(verdict, pcr.report.verdict);
        for concern in &pcr.report.concerns {
            let mut tagged = concern.clone();
            tagged.change_slug = Some(pcr.change_slug.clone());
            concerns.push(tagged);
        }
        let section_body =
            format!("VERDICT: {}\n\n{}", verdict_label(pcr.report.verdict), pcr.report.markdown);
        sections.push(PerChangeSection {
            change_slug: pcr.change_slug,
            markdown: section_body,
        });
    }
    ReviewReport {
        verdict,
        markdown: String::new(),
        concerns,
        per_change_sections: sections,
    }
}

fn verdict_label(v: ReviewVerdict) -> &'static str {
    match v {
        ReviewVerdict::Pass => "Pass",
        ReviewVerdict::Concerns => "Concerns",
        ReviewVerdict::Block => "Block",
    }
}

fn worst_verdict(a: ReviewVerdict, b: ReviewVerdict) -> ReviewVerdict {
    fn rank(v: ReviewVerdict) -> u8 {
        match v {
            ReviewVerdict::Pass => 0,
            ReviewVerdict::Concerns => 1,
            ReviewVerdict::Block => 2,
        }
    }
    if rank(a) >= rank(b) { a } else { b }
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
#[allow(clippy::too_many_arguments)]
pub async fn run_pass_through_commits(
    paths: &DaemonPaths,
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
    queued_audit_types: &std::collections::HashSet<String>,
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
    if let Err(e) = workspace::ensure_initialized(paths, workspace, &repo.url, fork_arg) {
        let class = classify_recovery_failure(&e);
        log_classified_recovery_failure(&repo.url, "workspace_init", class, &e);
        handle_classified_recovery_failure(
            paths,
            workspace,
            &repo.url,
            chatops_ctx,
            chatops_ctx
                .map(|c| c.failure_alerts_enabled)
                .unwrap_or(false),
            AlertCategory::WorkspaceInitFailure,
            &e,
            class,
        )
        .await;
        return Err(e);
    }
    if did_refork {
        maybe_post_refork_notification(repo, chatops_ctx).await;
    }
    let _cleared = queue::clear_stale_locks(workspace)?;

    let dirty = git::status_porcelain(workspace)?;
    // Post-`a16`, alert-state lives in `<state_dir>/alert-state/...`,
    // outside the workspace, so this filter is a defensive no-op for
    // normal operation. It still runs to catch transient `.alert-state.json`
    // files that linger before the first-startup migration completes
    // (e.g., a fresh re-clone of a repo whose history transiently
    // included it).
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
                    let class = classify_recovery_failure(&e);
                    log_classified_recovery_failure(&repo.url, "dirty_recheck", class, &e);
                    handle_classified_recovery_failure(
                        paths,
                        workspace,
                        &repo.url,
                        chatops_ctx,
                        chatops_ctx
                            .map(|c| c.failure_alerts_enabled)
                            .unwrap_or(false),
                        AlertCategory::WorkspaceDirtyMidIteration,
                        &e,
                        class,
                    )
                    .await;
                    return Err(e);
                }
            }
            Err(recovery_err) => {
                let e = anyhow!(
                    "dirty-workspace recovery failed: {recovery_err:#}; original dirty state:\n{dirty_filtered}"
                );
                let class = classify_recovery_failure(&e);
                log_classified_recovery_failure(&repo.url, "dirty_cleanup", class, &e);
                handle_classified_recovery_failure(
                    paths,
                    workspace,
                    &repo.url,
                    chatops_ctx,
                    chatops_ctx
                        .map(|c| c.failure_alerts_enabled)
                        .unwrap_or(false),
                    AlertCategory::WorkspaceDirtyMidIteration,
                    &e,
                    class,
                )
                .await;
                return Err(e);
            }
        }
    }

    if let Err(e) = git::fetch(workspace) {
        let class = classify_recovery_failure(&e);
        log_classified_recovery_failure(&repo.url, "git_fetch", class, &e);
        handle_classified_recovery_failure(
            paths,
            workspace,
            &repo.url,
            chatops_ctx,
            chatops_ctx
                .map(|c| c.failure_alerts_enabled)
                .unwrap_or(false),
            AlertCategory::WorkspaceInitFailure,
            &e,
            class,
        )
        .await;
        return Err(e);
    }
    // OSS-fork support (a26): opportunistic upstream fetch.
    // Best-effort — failures log a WARN but never block the iteration.
    opportunistic_upstream_fetch(workspace, repo);
    git::checkout(workspace, &repo.base_branch)?;
    git::pull_ff_only(workspace, &repo.base_branch)?;
    git::recreate_branch(workspace, &repo.agent_branch)?;

    // Canonical-spec RAG workspace-init hook (a21). Idempotent: only
    // builds + registers the store on the first iteration of a given
    // workspace (a previously-registered store is left alone). Fail-open
    // — any error logs WARN and the store is omitted from the registry.
    crate::rag::workspace_init_hook(workspace).await;

    let pending_at_start = queue::list_pending(paths, workspace)?;
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
        apply_archive_collision_preflight(paths, workspace, repo, chatops_ctx, pending_at_start).await;

    // Process waiting (escalated) changes BEFORE pending. Each resumes if
    // a human reply has arrived. Any change that comes back as Completed
    // with a diff goes into the `processed` list and will get pushed/PR'd
    // along with anything from the pending pass.
    let mut processed: Vec<String> = Vec::new();
    let mut includes_self_heal = false;
    if chatops_ctx.is_some() {
        let resumed = process_waiting_changes(
            paths,
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
    // pass, skip the pending pass entirely for this iteration. Audits
    // still run after this gate — they are independent of queue state
    // and the operator-visible block is on the queue walk, not on
    // periodic maintenance.
    let still_waiting = queue::list_waiting(workspace)?;
    if !still_waiting.is_empty() {
        tracing::info!(
            url = repo.url.as_str(),
            "queue blocked for {}: {} change(s) still waiting on human reply: {}",
            repo.url,
            still_waiting.len(),
            still_waiting.join(", ")
        );
        run_due_audits_after_queue(
            paths,
            workspace,
            repo,
            audit_registry,
            audits_cfg,
            audit_settings,
            chatops_ctx,
            queued_audit_types,
        )
        .await;
        tracing::info!(
            url = %repo.url,
            committed = processed.len(),
            waiting = still_waiting.len(),
            "polling pass complete"
        );
        return Ok((processed, includes_self_heal));
    }

    // Same-repo block (a18): if any change carries an operator-action
    // marker (`.perma-stuck.json`, `.needs-spec-revision.json`, or
    // `.question.json` AskUser waiting) AND is NOT downgraded by a
    // companion `.ignore-for-queue.json`, halt the pending walk. The
    // operator opts a specific change out of blocking by stamping
    // `.ignore-for-queue.json` alongside the underlying marker.
    let blocking_markers = queue::find_queue_blocking_markers(workspace)?;
    if !blocking_markers.is_empty() {
        for bm in &blocking_markers {
            let marker_path = workspace
                .join("openspec/changes")
                .join(&bm.change)
                .join(&bm.marker);
            tracing::info!(
                url = repo.url.as_str(),
                change = %bm.change,
                marker = %bm.marker,
                path = %marker_path.display(),
                "queue blocked: change `{}` has `{}` (not downgraded by .ignore-for-queue.json)",
                bm.change,
                bm.marker
            );
        }
        run_due_audits_after_queue(
            paths,
            workspace,
            repo,
            audit_registry,
            audits_cfg,
            audit_settings,
            chatops_ctx,
            queued_audit_types,
        )
        .await;
        tracing::info!(
            url = %repo.url,
            committed = processed.len(),
            blocked = blocking_markers.len(),
            "polling pass complete (queue blocked by operator-action markers)"
        );
        return Ok((processed, includes_self_heal));
    }

    let remaining = max_changes_per_pr.saturating_sub(processed.len() as u32);
    if remaining > 0 {
        let (pending_processed, pending_self_heal) = walk_queue(
            paths,
            workspace,
            repo,
            github_cfg,
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

    // Periodic audits run AFTER the pending queue walk completes (was:
    // before list_pending). The reorder prevents an "audit storm" — many
    // audits becoming eligible at once after a HEAD change — from
    // monopolizing the daemon and starving pending changes. The
    // trade-off is that an audit's spec-writing outcome
    // (`AuditOutcome::SpecsWritten`) lands its new pending change
    // directories AFTER this iteration's queue walk has already finished;
    // those changes wait for the NEXT iteration's `list_pending`. The
    // audit's creation commit still ships in this iteration's PR.
    //
    // Iteration-level workspace-validity gate (see
    // `audits-require-valid-workspace`): the audit scheduler is only
    // reached when `ensure_initialized` returned Ok for this iteration.
    // The early `return Err(e)` on init failure above is the gate: if
    // the workspace can't be brought to a valid state at the start of
    // the iteration, this site is unreachable and `run_due_audits` is
    // never called, so audits cannot create broken-state side effects.
    // (Per-audit gates in each `Audit::run` catch the rarer case where
    // the workspace becomes invalid mid-iteration.)
    run_due_audits_after_queue(
        paths,
        workspace,
        repo,
        audit_registry,
        audits_cfg,
        audit_settings,
        chatops_ctx,
        queued_audit_types,
    )
    .await;

    let waiting_after = queue::list_waiting(workspace)?.len();
    tracing::info!(
        url = %repo.url,
        committed = processed.len(),
        waiting = waiting_after,
        "polling pass complete"
    );
    Ok((processed, includes_self_heal))
}

/// Invoke the periodic-audit scheduler at the post-queue-walk position.
/// Audit failures inside the scheduler are logged and never abort the
/// iteration — the caller continues to the push+PR step regardless.
async fn run_due_audits_after_queue(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    audit_registry: &AuditRegistry,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
    chatops_ctx: Option<&ChatOpsContext>,
    queued_audit_types: &std::collections::HashSet<String>,
) {
    if let Err(e) = run_due_audits(
        paths,
        audit_registry,
        workspace,
        repo,
        audits_cfg,
        audit_settings,
        chatops_ctx,
        queued_audit_types,
    )
    .await
    {
        tracing::error!(
            url = %repo.url,
            "audit scheduler errored (iteration continues): {e:#}"
        );
    }
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
    paths: &DaemonPaths,
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
    let waiting = apply_archive_collision_preflight(paths, workspace, repo, chatops_ctx, waiting).await;
    let mut resumed_archived: Vec<String> = Vec::new();

    for change in waiting {
        match process_one_waiting(paths, workspace, repo, executor, ctx, &change, perma_stuck_threshold)
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
    paths: &DaemonPaths,
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
    busy_marker::update_change(paths, workspace, change);
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
        Ok(ExecutorOutcome::Completed { .. }) => {
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
                let spec_root = crate::spec_root::SpecRoot::for_repo(repo, workspace);
                queue::archive_at(&spec_root, change)?;
                (ResumeDisposition::Archived, None)
            }
        }
        Ok(ExecutorOutcome::AskUser {
            question: q2,
            resume_handle: rh2,
        }) => {
            // Agent asked another question. Post it and rotate the
            // question file. The change stays in the waiting set.
            escalate_to_chatops(paths, workspace, repo, ctx, change, &q2, rh2.0).await?;
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
                unarchivable_deltas: Vec::new(),
                revision_suggestion: revision_suggestion.clone(),
            };
            if let Err(e) = spec_revision::write_marker(workspace, change, &detail) {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "failed to write spec-needs-revision marker (resume): {e:#}"
                );
            }
            // a27a1: same lifecycle as the pending path — SpecNeedsRevision
            // terminates the iteration sequence; drop the marker.
            let basename_for_marker = workspace
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            if let Err(e) = crate::iteration_pending::remove_marker(
                paths,
                basename_for_marker,
                change,
            ) {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "failed to remove iteration-pending marker on SpecNeedsRevision (resume): {e:#}"
                );
            }
            maybe_post_spec_revision_alert(
                paths,
                Some(ctx),
                repo,
                change,
                &unimplementable_tasks,
                &revision_suggestion,
            )
            .await;
            (ResumeDisposition::SpecRevisionMarked, None)
        }
        Ok(ExecutorOutcome::IterationRequested { .. }) => {
            // a27a1: resume returning IterationRequested is unusual but
            // possible (e.g. the operator's answer pointed the agent at
            // additional work it can complete in another iteration).
            // Today's resume path doesn't have the WIP-commit + push
            // plumbing the pending arm has, AND the iteration cap is
            // enforced at the classifier which already produced this
            // variant. Treat it as a Failed-equivalent so the operator
            // sees the unhandled case rather than silent loss; the next
            // polling iteration will re-enter the change normally.
            tracing::warn!(
                url = %repo.url,
                change = %change,
                "resume returned IterationRequested; treating as Failed (resume-side iteration sequences not yet supported)"
            );
            (
                ResumeDisposition::Failed,
                Some(
                    "resume returned IterationRequested (unsupported on the resume path)"
                        .to_string(),
                ),
            )
        }
        Ok(ExecutorOutcome::Aborted { reason }) => {
            // a39: the resume's subprocess was killed by the daemon's
            // own SIGTERM cascade. The .question.json was deleted
            // before the resume call (above), so we cannot restore the
            // pre-resume waiting-on-answer state. The change is back
            // in pending state for the next iteration to retry from
            // the agent-q tip. We do NOT increment the failure counter
            // (operator initiated the shutdown) AND do NOT post a
            // chatops alert.
            tracing::info!(
                url = %repo.url,
                change = %change,
                "resume aborted by daemon shutdown: {reason}"
            );
            (ResumeDisposition::Aborted, None)
        }
    };

    // Counter book-keeping mirrors the pending path:
    //   - Archived → clear
    //   - Failed / CompletedNoDiff (transformed-to-Failed) → record + maybe perma-stuck
    //   - Errored / EscalatedAgain → leave the counter alone
    match (&result, failure_reason) {
        (ResumeDisposition::Archived, _) => {
            if let Err(e) = failure_state::clear(paths, workspace, change) {
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
                paths,
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
    /// a39: resume returned `Aborted` (subprocess killed by the daemon's
    /// own SIGTERM cascade). Treat as a non-counter-bumping failure-
    /// equivalent — the failure budget is not the right tool for an
    /// operator-initiated shutdown.
    Aborted,
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
            ResumeDisposition::Aborted => "aborted",
        }
    }
}

/// Post a question to ChatOps and write a fresh `.question.json`. Called
/// from the initial AskUser handling (pending → waiting) AND from the
/// resume path when the agent asks ANOTHER question.
async fn escalate_to_chatops(
    _paths: &DaemonPaths,
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
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsContext>,
    perma_stuck_threshold: u32,
    max_changes: u32,
    pending: Vec<String>,
) -> Result<(Vec<String>, bool)> {
    let mut archived: Vec<String> = Vec::new();
    let mut includes_self_heal = false;

    for change in pending {
        let result = process_one_pending_change(
            paths,
            workspace,
            repo,
            github_cfg,
            executor,
            chatops_ctx,
            &change,
        )
        .await;

        let outcome_label = match &result {
            Ok(QueueStep::Archived) => "archived",
            Ok(QueueStep::ArchivedSelfHeal) => "archived_self_heal",
            Ok(QueueStep::Failed { .. }) => "failed",
            Ok(QueueStep::Escalated) => "escalated",
            Ok(QueueStep::AskUserExitEarly) => "ask_user_exit_early",
            Ok(QueueStep::SpecRevisionMarked) => "spec_needs_revision",
            Ok(QueueStep::IterationPending) => "iteration_pending",
            Ok(QueueStep::Aborted) => "aborted",
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
                if let Err(e) = failure_state::clear(paths, workspace, &change) {
                    tracing::warn!(
                        url = %repo.url,
                        change = %change,
                        "failed to clear failure-state entry after archive: {e:#}"
                    );
                }
                // Canonical-spec RAG post-archive hook (a21). Inspect
                // the just-landed commit (HEAD vs HEAD~1) for canonical
                // spec changes; re-embed affected capabilities. Fail-
                // open via the hook itself.
                let touched_caps =
                    crate::rag::capabilities_touched_between(workspace, "HEAD~1..HEAD");
                if !touched_caps.is_empty() {
                    crate::rag::post_archive_hook(workspace, &touched_caps).await;
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
                    paths,
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
            Ok(QueueStep::IterationPending) => {
                // a27a1: the executor wants another iteration on this
                // change. The WIP has been committed + force-pushed to
                // the agent branch, `.iteration-pending.json` carries the
                // continuation state, AND `.in-progress` has been dropped
                // inside `handle_outcome`. The next polling iteration on
                // this repo will pick the change up first (queue front-
                // insertion via marker preference). Halt the walk now —
                // we do NOT chain a follow-up commit on top of the WIP
                // (PRs are reserved for the FINAL `Completed`).
                tracing::info!(
                    url = %repo.url,
                    change = %change,
                    "change requested another iteration; halting queue walk this iteration"
                );
                break;
            }
            Ok(QueueStep::Aborted) => {
                // a39: the executor's subprocess was killed by the
                // daemon's own SIGTERM cascade. `.in-progress` has been
                // dropped inside `handle_outcome`. We must NOT bump the
                // perma-stuck counter (operator-initiated shutdown is
                // not a repeat-execution-failure) AND we halt the walk
                // — the daemon is shutting down; later changes belong
                // to the next process's iteration.
                tracing::info!(
                    url = %repo.url,
                    change = %change,
                    "change aborted by daemon shutdown; halting queue walk this iteration"
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
                    paths,
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
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsContext>,
    change: &str,
) -> Result<QueueStep> {
    // Spec-delta archivability pre-flight (a17). Catches the a07-style
    // class of failures — a `## MODIFIED Requirements` block whose
    // `### Requirement:` header doesn't exist in canonical, etc. —
    // BEFORE the executor runs. Saves the LLM cost on changes whose
    // deltas would abort `openspec archive` later anyway. No lock is
    // taken on this path: the marker file is the operator-action gate;
    // failing-archivability changes never lock the queue dir.
    match handle_archivability_preflight(paths, workspace, repo, chatops_ctx, change).await {
        Ok(Some(step)) => return Ok(step),
        Ok(None) => {}
        Err(e) => {
            // Pre-flight should never fail (it's filesystem reads against
            // the change's own dir), but if it does we log + proceed to
            // the executor — better to incur a redundant Claude run than
            // halt the queue on an unexpected I/O glitch.
            tracing::warn!(
                url = %repo.url,
                change = %change,
                "spec-archivability pre-flight check errored; proceeding to executor: {e:#}"
            );
        }
    }

    // Change-internal contradiction pre-flight (a19). Opt-in via
    // `executor.change_internal_contradiction_check: enabled`. The
    // global is `None` until daemon startup installs a context, so
    // tests AND default-off operators short-circuit here without
    // touching the LLM. Failures inside the check fail-open (no
    // contradictions reported, executor proceeds).
    if let Some(cc_ctx) = crate::preflight::change_contradiction::current() {
        match handle_contradiction_preflight(paths, workspace, repo, chatops_ctx, change, &cc_ctx)
            .await
        {
            Ok(Some(step)) => return Ok(step),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "change-contradiction pre-flight check errored unexpectedly; proceeding to executor: {e:#}"
                );
            }
        }
    }

    queue::lock(workspace, change)
        .with_context(|| format!("locking change `{change}`"))?;

    // Record which change this iteration is working on so the chatops
    // `status` reply can render `currently: working on <change>`. The
    // marker is held by the caller; best-effort update — failures are
    // logged at DEBUG and don't abort the iteration.
    busy_marker::update_change(paths, workspace, change);

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
    let result =
        handle_outcome(paths, workspace, repo, github_cfg, chatops_ctx, change, outcome).await;
    // Always unlock, even after a Completed → archive (archive moved the
    // dir, so the lock is gone, but `queue::unlock` is idempotent).
    let _ = queue::unlock(workspace, change);
    result
}

/// Run the spec-delta archivability pre-flight (a17) against `change`.
/// On clean result: returns `Ok(None)` and the caller proceeds to the
/// executor. On any violation: writes the `.needs-spec-revision.json`
/// marker with `unarchivable_deltas` populated, posts the existing
/// `AlertCategory::SpecNeedsRevision` chatops alert (subject to the 24h
/// throttle), and returns `Ok(Some(QueueStep::SpecRevisionMarked))` so
/// the caller short-circuits without invoking the executor.
async fn handle_archivability_preflight(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    change: &str,
) -> Result<Option<QueueStep>> {
    let violations = crate::preflight::spec_archivability::check_spec_deltas_archivable(
        workspace, change,
    )
    .with_context(|| format!("spec-delta archivability check for `{change}`"))?;
    if violations.is_empty() {
        return Ok(None);
    }
    let suggestion = build_unarchivable_revision_suggestion(change, &violations);
    tracing::warn!(
        url = %repo.url,
        change = %change,
        violations = violations.len(),
        "spec-delta archivability pre-flight FAILED; skipping executor and writing marker"
    );
    let detail = SpecNeedsRevisionDetail {
        unimplementable_tasks: Vec::new(),
        unarchivable_deltas: violations.clone(),
        revision_suggestion: suggestion.clone(),
    };
    if let Err(e) = spec_revision::write_marker(workspace, change, &detail) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to write spec-needs-revision marker (pre-flight): {e:#}"
        );
    }
    maybe_post_unarchivable_deltas_alert(paths, chatops_ctx, repo, change, &violations, &suggestion)
        .await;
    Ok(Some(QueueStep::SpecRevisionMarked))
}

/// Compose the auto-generated `revision_suggestion` text written into
/// the marker file when the pre-flight catches one or more unarchivable
/// deltas. Names each violation and points the operator at the spec
/// file to edit + the recovery verb.
fn build_unarchivable_revision_suggestion(
    change: &str,
    violations: &[crate::preflight::spec_archivability::UnarchivableDelta],
) -> String {
    let mut out = format!(
        "Pre-flight check found {} unarchivable spec delta{}:\n",
        violations.len(),
        if violations.len() == 1 { "" } else { "s" }
    );
    for v in violations {
        out.push_str(&format!(
            "- capability={cap} kind={kind} header=\"{hdr}\" reason=\"{reason}\"\n",
            cap = v.capability,
            kind = v.kind.as_str(),
            hdr = v.header,
            reason = v.reason,
        ));
    }
    out.push_str(&format!(
        "\nEdit openspec/changes/{change}/specs/<capability>/spec.md to use the\n\
         exact canonical header. After fixing, push the spec change AND clear\n\
         this marker via @<bot> clear-revision <repo> <change>.\n"
    ));
    out
}

/// Run the change-internal contradiction pre-flight (a19) against
/// `change`. On clean result (LLM returned an empty array OR failed
/// open): returns `Ok(None)` and the caller proceeds to the executor.
/// On non-empty findings: writes the `.needs-spec-revision.json`
/// marker with `revision_suggestion` populated from the contradictions
/// narrative, posts the existing `AlertCategory::SpecNeedsRevision`
/// chatops alert (subject to 24h throttle), AND returns
/// `Ok(Some(QueueStep::SpecRevisionMarked))` so the caller halts the
/// queue walk without invoking the executor.
async fn handle_contradiction_preflight(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    change: &str,
    cc_ctx: &crate::preflight::change_contradiction::ContradictionCheckCtx,
) -> Result<Option<QueueStep>> {
    let findings =
        crate::preflight::change_contradiction::check_change_internal_contradictions(
            workspace,
            change,
            cc_ctx.llm.as_ref(),
            &cc_ctx.prompt_template,
        )
        .await
        .with_context(|| format!("contradiction-check pre-flight for `{change}`"))?;
    if findings.is_empty() {
        return Ok(None);
    }
    let suggestion = build_contradiction_revision_suggestion(&findings);
    tracing::warn!(
        url = %repo.url,
        change = %change,
        findings = findings.len(),
        "change-contradiction pre-flight FAILED; skipping executor and writing marker"
    );
    let detail = SpecNeedsRevisionDetail {
        unimplementable_tasks: Vec::new(),
        unarchivable_deltas: Vec::new(),
        revision_suggestion: suggestion.clone(),
    };
    if let Err(e) = spec_revision::write_marker(workspace, change, &detail) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to write spec-needs-revision marker (contradiction pre-flight): {e:#}"
        );
    }
    maybe_post_contradiction_findings_alert(paths, chatops_ctx, repo, change, &findings, &suggestion)
        .await;
    Ok(Some(QueueStep::SpecRevisionMarked))
}

/// Compose the auto-generated `revision_suggestion` text written into
/// the marker file when the contradiction pre-flight catches one or
/// more findings. Numbers each finding 1..N, includes the LLM's
/// `requirement_a`, `requirement_b`, AND `summary`, AND closes with
/// operator-action guidance.
fn build_contradiction_revision_suggestion(
    findings: &[crate::preflight::change_contradiction::ContradictionFinding],
) -> String {
    let n = findings.len();
    let mut out = format!(
        "Pre-flight contradiction check found {n} issue(s) where this change's\n\
         requirements appear to contradict each other:\n\n"
    );
    for (i, f) in findings.iter().enumerate() {
        out.push_str(&format!(
            "{idx}. Requirement A: {a}\n   Requirement B: {b}\n   {summary}\n\n",
            idx = i + 1,
            a = f.requirement_a,
            b = f.requirement_b,
            summary = f.summary,
        ));
    }
    out.push_str(
        "Edit the conflicting requirements so they can hold simultaneously,\n\
         OR REMOVE one of them. Push the spec change AND clear this marker\n\
         via @<bot> clear-revision <repo> <change>.\n",
    );
    out
}

/// Sibling of `maybe_post_unarchivable_deltas_alert` for the a19
/// contradiction pre-flight path. Same throttle state, channel, and
/// gating flag as the existing alert so a single stream of
/// `AlertCategory::SpecNeedsRevision` notifications covers both code
/// paths. Body framing names "contradictions" instead of unarchivable
/// deltas.
async fn maybe_post_contradiction_findings_alert(
    paths: &DaemonPaths,
    chatops_ctx: Option<&ChatOpsContext>,
    repo: &RepositoryConfig,
    change: &str,
    findings: &[crate::preflight::change_contradiction::ContradictionFinding],
    revision_suggestion: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
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
    let mut findings_block = String::new();
    for (i, f) in findings.iter().enumerate() {
        findings_block.push_str(&format!(
            "  {n}. A: \"{a}\" vs B: \"{b}\" — {s}\n",
            n = i + 1,
            a = f.requirement_a,
            b = f.requirement_b,
            s = f.summary,
        ));
    }
    let text = format!(
        "⚠️ `{repo_url}`: spec needs revision — `{change}` has change-internal contradictions (pre-flight)\n\nRequirements within this change cannot all hold simultaneously:\n{findings_block}\nSuggested revision:\n{suggestion}\nOperator action:\n  1. Edit openspec/changes/{change}/specs/<capability>/spec.md so the conflicting requirements can both hold (or remove one).\n  2. Commit + push to {base}.\n  3. `@<bot> clear-revision <repo> <change>` from chat (or delete the marker file).\n\nmarker: {marker}",
        repo_url = repo.url,
        change = change,
        findings_block = findings_block,
        suggestion = revision_suggestion,
        base = repo.base_branch,
        marker = marker_path.display(),
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "contradiction-findings chatops alert post failed: {e:#}"
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
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to persist contradiction-findings alert state: {e:#}"
        );
    }
}

/// Sibling of [`maybe_post_spec_revision_alert`] for the a17 pre-flight
/// path. Body framing names "unarchivable spec deltas" rather than the
/// agent-detected "unimplementable tasks", and lists each violation
/// (`capability`, `kind`, `header`, `reason`). Throttle state, channel,
/// and gating flag are identical to the existing alert so a single
/// stream of `AlertCategory::SpecNeedsRevision` notifications covers
/// both code paths.
async fn maybe_post_unarchivable_deltas_alert(
    paths: &DaemonPaths,
    chatops_ctx: Option<&ChatOpsContext>,
    repo: &RepositoryConfig,
    change: &str,
    violations: &[crate::preflight::spec_archivability::UnarchivableDelta],
    revision_suggestion: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
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
    let mut violations_block = String::new();
    for v in violations {
        violations_block.push_str(&format!(
            "  - {cap} / {kind}: \"{hdr}\" — {reason}\n",
            cap = v.capability,
            kind = v.kind.as_str(),
            hdr = v.header,
            reason = v.reason,
        ));
    }
    let text = format!(
        "⚠️ `{repo_url}`: spec needs revision — `{change}` has unarchivable spec deltas (pre-flight)\n\nDeltas whose preconditions don't match canonical specs (would abort `openspec archive` later):\n{violations_block}\nSuggested revision:\n{suggestion}\nOperator action:\n  1. Edit openspec/changes/{change}/specs/<capability>/spec.md so each delta block's header matches canonical.\n  2. Commit + push to {base}.\n  3. `@<bot> clear-revision <repo> <change>` from chat (or delete the marker file).\n\nmarker: {marker}",
        repo_url = repo.url,
        change = change,
        violations_block = violations_block,
        suggestion = revision_suggestion,
        base = repo.base_branch,
        marker = marker_path.display(),
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "unarchivable-deltas chatops alert post failed: {e:#}"
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
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to persist unarchivable-deltas alert state: {e:#}"
        );
    }
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
    paths: &DaemonPaths,
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
            paths,
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

#[derive(Debug)]
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
    /// a27a1: the executor returned `IterationRequested`. The WIP has
    /// been committed + force-pushed to the agent branch AND the
    /// `.iteration-pending.json` marker has been written. The walker
    /// halts this iteration; the next polling iteration picks the
    /// change up first via the queue's marker-preference ordering.
    /// Unlike `Failed`, this MUST NOT increment the perma-stuck counter
    /// — iteration sequences are part of the normal lifecycle, not a
    /// repeat-execution-failure.
    IterationPending,
    /// a39: the executor returned `Aborted`. The subprocess was killed
    /// by the daemon's own SIGTERM cascade (operator-initiated
    /// shutdown). The `.in-progress` lock has been dropped AND the
    /// `.iteration-pending.json` marker (if any) has been left
    /// untouched. The walker halts this iteration; the next polling
    /// iteration after restart picks the change up fresh. Like
    /// `IterationPending`, this MUST NOT increment the perma-stuck
    /// counter — operator-initiated shutdown is not a repeat-execution-
    /// failure.
    Aborted,
}

/// Increment the per-change failure counter, and on threshold transition
/// write the perma-stuck marker + post the chatops alert. Best-effort: any
/// I/O or transport failure here is logged at WARN and does not propagate.
async fn handle_failure_counter(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    change: &str,
    reason: &str,
    threshold: u32,
) {
    let count = match failure_state::record_failure(paths, workspace, change, reason) {
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
    post_perma_stuck_alert(paths, chatops_ctx, repo, change, reason, count).await;
}

/// Post the chatops perma-stuck alert (best-effort, 24h-throttled per
/// change). The state for this throttle lives in the daemon's
/// alert-state file (`<state_dir>/alert-state/<basename>.json`) under
/// its `perma_stuck_alerts` map.
async fn post_perma_stuck_alert(
    paths: &DaemonPaths,
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
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
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
    let log_path = crate::executor::claude_cli::run_log_path(paths, &workspace, change);
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
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to persist perma-stuck alert state: {e:#}"
        );
    }
}

/// Post the chatops spec-needs-revision alert (best-effort, 24h-throttled
/// per change). State for this throttle lives in the daemon's
/// alert-state file (`<state_dir>/alert-state/<basename>.json`) under
/// its `spec_revision_alerts` map. Mirrors `post_perma_stuck_alert` —
/// both announce operator-action states with the same throttle window.
async fn maybe_post_spec_revision_alert(
    paths: &DaemonPaths,
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
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
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
    let log_path = crate::executor::claude_cli::run_log_path(paths, &workspace, change);
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
    if let Err(e) = state.save(paths, &workspace) {
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

/// Per-canonical-spec character cap on a threaded notification body.
/// Mirrors the audit-findings threading threshold; see
/// `audits::AUDIT_THREAD_BODY_CHAR_CAP`.
const REVISE_FAILED_REASON_THREAD_CAP: usize = 35_000;

/// Render `duration` using the same human-format shape the chatops
/// `status` reply uses for "started Nm ago" — delegates to
/// `busy_marker::format_age_human` so the two stay in lockstep.
fn format_revise_duration(duration: std::time::Duration) -> String {
    busy_marker::format_age_human(duration.as_secs())
}

/// Compose the canonical `change_list_summary` segment for a
/// revise-lifecycle notification: `` `<first_change>` +N more `` (the
/// `+0 more` suffix is omitted; `+1 more` AND higher are included). The
/// caller wraps the result in `(...)` when embedding.
pub(crate) fn format_revise_change_list_summary(change_list: &[String]) -> String {
    if change_list.is_empty() {
        return "(unknown change)".to_string();
    }
    let first = &change_list[0];
    let extras = change_list.len().saturating_sub(1);
    if extras == 0 {
        format!("`{first}`")
    } else {
        format!("`{first}` +{extras} more")
    }
}

/// Truncate `operator_comment` to at most `max_chars` characters,
/// appending `…` when truncated. Used by the picked-up dispatch site to
/// fit the operator's revise text into the 80-char quote slot.
pub(crate) fn truncate_operator_comment(operator_comment: &str, max_chars: usize) -> String {
    let count = operator_comment.chars().count();
    if count <= max_chars {
        return operator_comment.to_string();
    }
    let mut out: String = operator_comment.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Post the chatops "Revise picked up" lifecycle notification (best-
/// effort, deduplicated per-comment via the alert-state file's
/// `revise_notifications` map). Returns silently when the chatops
/// backend is absent, `failure_alerts_enabled` is `false`, OR the
/// notification was already posted for this comment. On post failure,
/// the alert-state file is NOT updated so a subsequent iteration can
/// retry.
pub(crate) async fn maybe_post_revise_picked_up_alert(
    paths: &DaemonPaths,
    chatops_ctx: Option<&crate::revisions::ChatOpsCtx<'_>>,
    repo: &RepositoryConfig,
    pr_number: u64,
    pr_url: &str,
    change_list_summary: &str,
    operator_comment_quote: &str,
    comment_id: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
    if state.revise_notification_already_posted(
        comment_id,
        crate::alert_state::ReviseNotificationKind::PickedUp,
    ) {
        return;
    }
    let text = format!(
        "🔧 `{repo_url}`: revising PR #{pr_number} ({change_list_summary}): \"{quote}\"\n{pr_url}",
        repo_url = repo.url,
        quote = operator_comment_quote,
    );
    if let Err(e) = ctx.chatops.post_notification(ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "revise-picked-up chatops notification post failed: {e:#}"
        );
        return;
    }
    state.record_revise_notification(
        comment_id,
        crate::alert_state::ReviseNotificationKind::PickedUp,
        Utc::now(),
    );
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "failed to persist revise-picked-up notification state: {e:#}"
        );
    }
}

/// Post the chatops "Revise succeeded" lifecycle notification (mirrors
/// [`maybe_post_revise_picked_up_alert`] with the `Succeeded` kind).
/// Posted after the executor returns `Completed` (or `IterationRequested`)
/// AND the commit + force-push step succeeds.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn maybe_post_revise_succeeded_alert(
    paths: &DaemonPaths,
    chatops_ctx: Option<&crate::revisions::ChatOpsCtx<'_>>,
    repo: &RepositoryConfig,
    pr_number: u64,
    pr_url: &str,
    change_list_summary: &str,
    agent_branch: &str,
    duration: std::time::Duration,
    comment_id: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
    if state.revise_notification_already_posted(
        comment_id,
        crate::alert_state::ReviseNotificationKind::Succeeded,
    ) {
        return;
    }
    let text = format!(
        "✓ `{repo_url}`: revision applied to PR #{pr_number} ({change_list_summary}) — force-pushed `{agent_branch}` (took {duration_human})\n{pr_url}",
        repo_url = repo.url,
        duration_human = format_revise_duration(duration),
    );
    if let Err(e) = ctx.chatops.post_notification(ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "revise-succeeded chatops notification post failed: {e:#}"
        );
        return;
    }
    state.record_revise_notification(
        comment_id,
        crate::alert_state::ReviseNotificationKind::Succeeded,
        Utc::now(),
    );
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "failed to persist revise-succeeded notification state: {e:#}"
        );
    }
}

/// Post the chatops "Revise failed" lifecycle notification (mirrors
/// [`maybe_post_revise_picked_up_alert`] with the `Failed` kind). When
/// `reason.len() > REVISE_FAILED_REASON_THREAD_CAP`, the helper switches
/// to the threaded-notification API AND truncates the body at 35,000
/// characters with a pointer-to-daemon-log tail (per the existing
/// canonical "Thread body truncates at 35,000 characters" requirement).
pub(crate) async fn maybe_post_revise_failed_alert(
    paths: &DaemonPaths,
    chatops_ctx: Option<&crate::revisions::ChatOpsCtx<'_>>,
    repo: &RepositoryConfig,
    pr_number: u64,
    pr_url: &str,
    reason: &str,
    comment_id: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
    if state.revise_notification_already_posted(
        comment_id,
        crate::alert_state::ReviseNotificationKind::Failed,
    ) {
        return;
    }
    let reason_chars = reason.chars().count();
    let post_result = if reason_chars > REVISE_FAILED_REASON_THREAD_CAP {
        let top_line = format!(
            "✗ `{repo_url}`: revision failed on PR #{pr_number} (full reason in thread)\n{pr_url}",
            repo_url = repo.url,
        );
        let truncated: String = reason
            .chars()
            .take(REVISE_FAILED_REASON_THREAD_CAP)
            .collect();
        let thread_body = format!(
            "{truncated}\n\n… [truncated; full reason at journalctl -u autocoder | grep pr={pr_number}]"
        );
        ctx.chatops
            .post_notification_with_thread(ctx.channel, &top_line, &thread_body)
            .await
            .map(|_| ())
    } else {
        let text = format!(
            "✗ `{repo_url}`: revision failed on PR #{pr_number}: {reason}\n{pr_url}",
            repo_url = repo.url,
        );
        ctx.chatops.post_notification(ctx.channel, &text).await
    };
    if let Err(e) = post_result {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "revise-failed chatops notification post failed: {e:#}"
        );
        return;
    }
    state.record_revise_notification(
        comment_id,
        crate::alert_state::ReviseNotificationKind::Failed,
        Utc::now(),
    );
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "failed to persist revise-failed notification state: {e:#}"
        );
    }
}

/// Maximum number of chars on the "failed" body's `reason` segment
/// before the helper switches to the threaded-notification path AND
/// truncates per the canonical 35,000-char rule.
const CODE_REVIEW_FAILED_REASON_THREAD_CAP: usize = 35_000;

/// Post the chatops "Code review triggered" lifecycle notification (a33)
/// (best-effort, deduplicated per-comment via the alert-state file's
/// `code_review_notifications` map). Returns silently when the chatops
/// backend is absent, `failure_alerts_enabled` is `false`, OR the
/// notification was already posted for this comment.
pub(crate) async fn maybe_post_code_review_triggered_alert(
    paths: &DaemonPaths,
    chatops_ctx: Option<&crate::revisions::ChatOpsCtx<'_>>,
    repo: &RepositoryConfig,
    pr_number: u64,
    pr_url: &str,
    operator_login: &str,
    comment_id: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
    if state.code_review_notification_already_posted(
        comment_id,
        crate::alert_state::CodeReviewNotificationKind::Triggered,
    ) {
        return;
    }
    let text = format!(
        "🔍 `{repo_url}`: code review triggered on PR #{pr_number} by @{operator_login}\n{pr_url}",
        repo_url = repo.url,
    );
    if let Err(e) = ctx.chatops.post_notification(ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "code-review-triggered chatops notification post failed: {e:#}"
        );
        return;
    }
    state.record_code_review_notification(
        comment_id,
        crate::alert_state::CodeReviewNotificationKind::Triggered,
        Utc::now(),
    );
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "failed to persist code-review-triggered notification state: {e:#}"
        );
    }
}

/// Post the chatops "Code review complete" lifecycle notification (a33).
pub(crate) async fn maybe_post_code_review_complete_alert(
    paths: &DaemonPaths,
    chatops_ctx: Option<&crate::revisions::ChatOpsCtx<'_>>,
    repo: &RepositoryConfig,
    pr_number: u64,
    pr_url: &str,
    verdict_label: &str,
    comment_id: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
    if state.code_review_notification_already_posted(
        comment_id,
        crate::alert_state::CodeReviewNotificationKind::Complete,
    ) {
        return;
    }
    let text = format!(
        "✓ `{repo_url}`: code review complete on PR #{pr_number} — verdict: {verdict_label}\n{pr_url}",
        repo_url = repo.url,
    );
    if let Err(e) = ctx.chatops.post_notification(ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "code-review-complete chatops notification post failed: {e:#}"
        );
        return;
    }
    state.record_code_review_notification(
        comment_id,
        crate::alert_state::CodeReviewNotificationKind::Complete,
        Utc::now(),
    );
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "failed to persist code-review-complete notification state: {e:#}"
        );
    }
}

/// Post the chatops "Code review failed" lifecycle notification (a33).
/// When `reason.len() > CODE_REVIEW_FAILED_REASON_THREAD_CAP`, switches
/// to the threaded-notification path AND truncates per the canonical
/// 35,000-char rule.
pub(crate) async fn maybe_post_code_review_failed_alert(
    paths: &DaemonPaths,
    chatops_ctx: Option<&crate::revisions::ChatOpsCtx<'_>>,
    repo: &RepositoryConfig,
    pr_number: u64,
    pr_url: &str,
    reason: &str,
    comment_id: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.failure_alerts_enabled {
        return;
    }
    let workspace = workspace::resolve_path(paths, repo);
    let mut state = AlertState::load_or_default(paths, &workspace);
    if state.code_review_notification_already_posted(
        comment_id,
        crate::alert_state::CodeReviewNotificationKind::Failed,
    ) {
        return;
    }
    let reason_chars = reason.chars().count();
    let post_result = if reason_chars > CODE_REVIEW_FAILED_REASON_THREAD_CAP {
        let top_line = format!(
            "✗ `{repo_url}`: code review failed on PR #{pr_number} (full reason in thread)\n{pr_url}",
            repo_url = repo.url,
        );
        let truncated: String = reason
            .chars()
            .take(CODE_REVIEW_FAILED_REASON_THREAD_CAP)
            .collect();
        let thread_body = format!(
            "{truncated}\n\n… [truncated; full reason at journalctl -u autocoder | grep pr={pr_number}]"
        );
        ctx.chatops
            .post_notification_with_thread(ctx.channel, &top_line, &thread_body)
            .await
            .map(|_| ())
    } else {
        let text = format!(
            "✗ `{repo_url}`: code review failed on PR #{pr_number}: {reason}\n{pr_url}",
            repo_url = repo.url,
        );
        ctx.chatops.post_notification(ctx.channel, &text).await
    };
    if let Err(e) = post_result {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "code-review-failed chatops notification post failed: {e:#}"
        );
        return;
    }
    state.record_code_review_notification(
        comment_id,
        crate::alert_state::CodeReviewNotificationKind::Failed,
        Utc::now(),
    );
    if let Err(e) = state.save(paths, &workspace) {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            comment_id = %comment_id,
            "failed to persist code-review-failed notification state: {e:#}"
        );
    }
}

/// Post the chatops re-review suggestion (a33). Fires after a revision
/// iteration when the cumulative-since-original-review diff overlap
/// exceeds the operator-configured threshold. Best-effort,
/// `failure_alerts_enabled`-gated, AND deduplicated per-PR per
/// `revisions_count` via the per-PR state file's
/// `last_suggested_rereview_at_revisions_count` field (caller updates
/// the field after a successful post).
pub(crate) async fn maybe_post_rereview_suggestion_alert(
    chatops_ctx: Option<&crate::revisions::ChatOpsCtx<'_>>,
    repo: &RepositoryConfig,
    pr_number: u64,
    pr_url: &str,
    overlap_percent: u32,
    revisions_count: u32,
) -> bool {
    let Some(ctx) = chatops_ctx else { return false };
    if !ctx.failure_alerts_enabled {
        return false;
    }
    let text = format!(
        "💡 `{repo_url}`: PR #{pr_number} has been substantially revised (~{overlap_percent}% of original diff changed across {revisions_count} revisions). Consider `@<bot> code-review` to re-evaluate.\n{pr_url}",
        repo_url = repo.url,
    );
    if let Err(e) = ctx.chatops.post_notification(ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            pr_number = pr_number,
            "re-review suggestion chatops notification post failed: {e:#}"
        );
        return false;
    }
    true
}

/// Log a mid-iteration recovery failure with its classification (transient
/// vs. permanent). Transient → WARN (network blips are noisy but
/// self-recovering); Permanent → ERROR (operator must inspect). The
/// `site` field names the call site (`workspace_init`, `git_fetch`,
/// `dirty_cleanup`, `dirty_recheck`) so journalctl filters can scope to
/// a specific stage.
fn log_classified_recovery_failure(
    repo_url: &str,
    site: &'static str,
    class: RecoveryFailureClass,
    err: &anyhow::Error,
) {
    match class {
        RecoveryFailureClass::Transient => tracing::warn!(
            url = repo_url,
            site,
            class = class.log_tag(),
            "mid-iteration recovery failed (will retry next iteration): {err:#}"
        ),
        RecoveryFailureClass::Permanent => tracing::error!(
            url = repo_url,
            site,
            class = class.log_tag(),
            "mid-iteration recovery failed (operator inspection required): {err:#}"
        ),
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

/// Defensive no-op: remove `git status --porcelain` lines that
/// reference a workspace-root `.alert-state.json` file. Post-`a16` the
/// file lives in `<state_dir>/alert-state/<basename>.json`, so the
/// workspace should never contain it AND the helper returns its input
/// unchanged for normal operation. The helper stays in the polling-
/// loop code path to absorb transient workspace-root `.alert-state.json`
/// files (e.g., a fresh re-clone of a repo whose history transiently
/// committed it before the migration completes). A future spec can
/// remove the helper after a verification window.
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
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    stuck_threshold_secs: u64,
) -> Result<()> {
    let mut guard = match busy_marker::try_acquire(paths, workspace, &repo.url, stuck_threshold_secs) {
        Ok(busy_marker::AcquireOutcome::Acquired(g)) => g,
        Ok(busy_marker::AcquireOutcome::SkipFreshInProgress(details)) => {
            tracing::info!(
                url = %repo.url,
                pid = details.marker.pid,
                stage = %details.marker.stage.as_str(),
                age = %busy_marker::format_age_human(details.age_secs),
                threshold = %busy_marker::format_age_human(details.threshold_secs),
                pid_alive = details.pid_alive,
                recovery_eligible = details.recovery_eligible(),
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
    workspace::ensure_initialized(paths, workspace, &repo.url, fork_arg)?;

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
        prefix_renames = report.prefix_renames.len(),
        aborted = report.abort_reason.is_some(),
        "rebuild_canonical finished"
    );

    // If the dependency pre-pass aborted the rebuild, there is no PR to
    // open and no canonical-spec drift to push. Post the `❌` chatops
    // notification and exit early.
    if report.abort_reason.is_some() {
        maybe_post_rebuild_abort_notification(repo, &report, chatops_ctx).await;
        return Ok(());
    }

    // If the pre-pass applied prefix renames, post the `🔀` chatops
    // notification BEFORE staging/pushing/PR so operators see the
    // renames first. Best-effort: a failed post does not block PR
    // creation.
    if !report.prefix_renames.is_empty() {
        maybe_post_rebuild_renames_notification(repo, &report, chatops_ctx).await;
    }

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
        match open_rebuild_pull_request(paths, repo, github_cfg, &report).await {
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
    _paths: &DaemonPaths,
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
    let body = build_rebuild_pr_body(report);
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

/// Build the markdown body for a rebuild PR. The summary line includes a
/// `(Z rolled back to archive)` parenthetical when `report.rolled_back >
/// 0` so the operator can confirm at a glance that the rollback count
/// matches the failure count; the failures section header describes
/// rollback rather than active-path retention to match the actual
/// behavior enforced by the rebuild's atomicity contract.
fn build_rebuild_pr_body(report: &crate::cli::sync_specs::RebuildReport) -> String {
    let mut body = String::new();
    body.push_str("This PR was generated by `autocoder sync-specs --rebuild`.\n\n");
    if report.rolled_back > 0 {
        body.push_str(&format!(
            "Replayed {} archived change(s) chronologically; {} succeeded, {} failed ({} rolled back to archive).\n\n",
            report.processed, report.successful, report.failed, report.rolled_back
        ));
    } else {
        body.push_str(&format!(
            "Replayed {} archived change(s) chronologically; {} succeeded, {} failed.\n\n",
            report.processed, report.successful, report.failed
        ));
    }
    if !report.failures.is_empty() {
        body.push_str(
            "**Failed changes** (rolled back to archive — see failure reasons below for the openspec output explaining each):\n",
        );
        for f in &report.failures {
            body.push_str(&format!(
                "- `{}`: {}\n",
                f.slug,
                truncate_one_line(&f.failure_reason, 200)
            ));
        }
        body.push('\n');
    }
    if !report.prefix_renames.is_empty() {
        body.push_str("**Applied dependency-prefix renames**:\n\n");
        body.push_str(&render_prefix_renames_markdown(&report.prefix_renames));
        body.push('\n');
    }
    body.push_str("**Canonical spec files**:\n");
    for sf in &report.spec_files {
        let tag = if sf.modified { "modified" } else { "unchanged" };
        body.push_str(&format!("- `{}` ({tag})\n", sf.path));
    }
    body
}

fn truncate_one_line(s: &str, n: usize) -> String {
    let one = s.lines().next().unwrap_or("");
    if one.chars().count() <= n {
        one.to_string()
    } else {
        one.chars().take(n).collect::<String>() + "…"
    }
}

/// Render a list of `RenameRecord`s grouped by day, in the format shared
/// between the `🔀` chatops notification and the PR body's renames
/// section. Each entry: `<from> → <to>` followed by an indented
/// parenthetical `(<dependency_summary>)`.
fn render_prefix_renames_markdown(
    renames: &[crate::cli::sync_specs_deps::RenameRecord],
) -> String {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<&str, Vec<&crate::cli::sync_specs_deps::RenameRecord>> =
        BTreeMap::new();
    for r in renames {
        grouped.entry(r.day.as_str()).or_default().push(r);
    }
    let mut out = String::new();
    for (day, group) in grouped {
        out.push_str(&format!("  {day}:\n"));
        for r in group {
            out.push_str(&format!("    {} → {}\n", r.from, r.to));
            if !r.dependency_summary.is_empty() {
                out.push_str(&format!("      ({})\n", r.dependency_summary));
            }
        }
    }
    out
}

/// Count how many distinct days appear in a list of `RenameRecord`s.
fn count_distinct_days(renames: &[crate::cli::sync_specs_deps::RenameRecord]) -> usize {
    use std::collections::BTreeSet;
    renames.iter().map(|r| r.day.as_str()).collect::<BTreeSet<_>>().len()
}

/// Format the `🔀` chatops notification text announcing applied
/// dependency-prefix renames. Pure function for snapshot-testing.
fn format_rebuild_renames_notification(
    repo_url: &str,
    renames: &[crate::cli::sync_specs_deps::RenameRecord],
) -> String {
    let n_days = count_distinct_days(renames);
    let mut out = format!(
        "🔀 `{repo_url}`: rebuild applied dependency-prefix renames in {n_days} day-group(s)\n"
    );
    out.push_str(&render_prefix_renames_markdown(renames));
    // Trim trailing newline for a cleaner one-message look.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Format the `❌` chatops notification text when the rebuild's
/// dependency pre-pass aborted (cycle, cross-day backward dep, scan
/// failure). Pure function for snapshot-testing.
fn format_rebuild_abort_notification(
    repo_url: &str,
    reason: &crate::cli::sync_specs_deps::RebuildAbortReason,
) -> String {
    format!(
        "❌ `{repo_url}`: rebuild aborted — {}. No archives were renamed; no canonical specs were modified. Operator action required.",
        reason.summary()
    )
}

/// Post the `🔀` rename-list notification. Best-effort: a failed post
/// logs at ERROR and does NOT block PR creation.
async fn maybe_post_rebuild_renames_notification(
    repo: &RepositoryConfig,
    report: &crate::cli::sync_specs::RebuildReport,
    chatops_ctx: Option<&ChatOpsContext>,
) {
    let Some(ctx) = chatops_ctx else { return };
    if report.prefix_renames.is_empty() {
        return;
    }
    let text = format_rebuild_renames_notification(&repo.url, &report.prefix_renames);
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::error!(
            url = %repo.url,
            "rebuild-renames chatops notification failed; continuing: {e:#}"
        );
    }
}

/// Post the `❌` rebuild-aborted notification. Best-effort: a failed
/// post logs at ERROR and does not propagate.
async fn maybe_post_rebuild_abort_notification(
    repo: &RepositoryConfig,
    report: &crate::cli::sync_specs::RebuildReport,
    chatops_ctx: Option<&ChatOpsContext>,
) {
    let Some(ctx) = chatops_ctx else { return };
    let Some(reason) = report.abort_reason.as_ref() else {
        return;
    };
    let text = format_rebuild_abort_notification(&repo.url, reason);
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::error!(
            url = %repo.url,
            "rebuild-abort chatops notification failed; continuing: {e:#}"
        );
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

/// OSS-fork support (a26): post the `📦 Branch pushed` notification
/// when `auto_submit_pr: false` skipped PR creation. Carries the
/// branch URL AND the templated `gh pr create` command the operator
/// can run manually after local review. Gated by the same
/// `pr_opened_enabled` flag as `maybe_post_pr_opened`.
async fn maybe_post_branch_pushed_no_pr(
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    branch_url: &str,
    suggested_command: &str,
    change_count: usize,
) {
    let Some(ctx) = chatops_ctx else { return };
    if !ctx.pr_opened_enabled {
        return;
    }
    let text = format!(
        "📦 `{url}`: branch pushed with {change_count} change(s): {branch_url}\nRun: {suggested_command}",
        url = repo.url,
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo.url,
            branch_url = %branch_url,
            "branch-pushed-no-pr notification failed; continuing: {e:#}"
        );
    }
}

/// OSS-fork support (a26): compose a GitHub branch tree URL of the
/// shape `https://github.com/<owner>/<repo>/tree/<branch>`. Used by
/// the `auto_submit_pr: false` path so the chatops notification
/// links to the pushed branch the operator can review locally.
fn compose_branch_url(owner: &str, repo: &str, branch: &str) -> String {
    format!("https://github.com/{owner}/{repo}/tree/{branch}")
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
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
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
        Ok(ExecutorOutcome::Aborted { reason }) => {
            // a39: the executor's subprocess was killed by the
            // daemon's own SIGTERM cascade. The classifier set this
            // outcome because `SHUTDOWN_REQUESTED == true` AND the
            // exit status was 143. Drop the `.in-progress` lock per
            // the canonical unlock-on-any-outcome rule; do NOT
            // increment the failure counter, do NOT write
            // `.perma-stuck.json`, do NOT post a chatops failure
            // alert (operator initiated the shutdown), AND leave any
            // `.iteration-pending.json` marker in place (the next
            // iteration after restart resumes context).
            tracing::info!(
                url = %repo.url,
                change = %change,
                "executor aborted: {reason}"
            );
            // Don't propagate the unlock error to the walker — the
            // walker would otherwise treat a stale-lock cleanup
            // hiccup as a post-executor Err AND bump the counter for
            // an outcome we explicitly chose to exempt. Best-effort
            // is consistent with how the `IterationRequested` arm
            // unlocks below.
            if let Err(e) = queue::unlock(workspace, change) {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "Aborted arm: dropping .in-progress failed (continuing): {e:#}"
                );
            }
            Ok(QueueStep::Aborted)
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
                unarchivable_deltas: Vec::new(),
                revision_suggestion: revision_suggestion.clone(),
            };
            if let Err(e) = spec_revision::write_marker(workspace, change, &detail) {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "failed to write spec-needs-revision marker: {e:#}"
                );
            }
            // a27a1: SpecNeedsRevision terminates the iteration sequence
            // (operator action is required from here on); drop the
            // iteration-pending marker so the change reverts to normal
            // queue ordering on the next iteration. Idempotent — absent
            // marker is OK.
            let basename_for_marker = workspace
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            if let Err(e) = crate::iteration_pending::remove_marker(
                paths,
                basename_for_marker,
                change,
            ) {
                tracing::warn!(
                    url = %repo.url,
                    change = %change,
                    "failed to remove iteration-pending marker on SpecNeedsRevision: {e:#}"
                );
            }
            // (c) Post the chatops alert. Best-effort: any failure is
            // logged at WARN and does not propagate.
            maybe_post_spec_revision_alert(
                paths,
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
                escalate_to_chatops(paths, workspace, repo, ctx, change, &question, resume_handle.0)
                    .await?;
                Ok(QueueStep::Escalated)
            }
            None => {
                tracing::warn!("executor asked a question on `{change}`: {question}");
                Ok(QueueStep::AskUserExitEarly)
            }
        },
        Ok(ExecutorOutcome::IterationRequested {
            completed_tasks,
            remaining_tasks,
            reason,
            iteration_number,
        }) => {
            handle_iteration_requested(
                paths,
                workspace,
                repo,
                github_cfg,
                change,
                completed_tasks,
                remaining_tasks,
                reason,
                iteration_number,
            )
            .await
        }
        Ok(ExecutorOutcome::Completed { .. }) => {
            // Remove the `.in-progress` lock BEFORE inspecting the working
            // tree: the lock file is untracked and would otherwise show up
            // in `git status --porcelain`, contaminating the dirty check
            // and getting swept into the commit by `git add -A`.
            queue::unlock(workspace, change)?;
            // a27a1: lifecycle — if a stale `.iteration-pending.json`
            // marker is present (the prior iteration emitted
            // IterationRequested AND this iteration emitted Completed),
            // delete it after the commit + archive step completes
            // successfully. This is done AFTER the archive section
            // below; we just stash the workspace + change here so the
            // delete-after-success site is easy to spot.
            let dirty = git::status_porcelain(workspace)?;
            if dirty.is_empty() {
                // Self-heal probe: if every task is `[x]` AND
                // `openspec validate --strict` exits 0, the change's
                // implementation is already on the base branch and the
                // only thing missing is the archive move. Run the archive
                // ourselves rather than burn another iteration on a no-op
                // Completed.
                let spec_root = crate::spec_root::SpecRoot::for_repo(repo, workspace);
                let tasks_complete = tasks_md_all_complete(&spec_root, change).unwrap_or(false);
                if tasks_complete && openspec_validate_strict_passes(&spec_root, change) {
                    tracing::info!(
                        url = %repo.url,
                        change = %change,
                        "self-heal: implementation already in HEAD, archiving"
                    );
                    let subject =
                        format!("archive: {change}: implementation already in base");
                    if let Err(e) = queue::archive_at(&spec_root, change) {
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
                // a27a1: lifecycle — if this Completed terminates a
                // multi-iteration sequence, delete the iteration-pending
                // marker (now in state_dir; no longer in the archived
                // directory regardless). Idempotent — absent marker is
                // fine.
                let basename_for_marker = workspace
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                if let Err(e) = crate::iteration_pending::remove_marker(
                    paths,
                    basename_for_marker,
                    change,
                ) {
                    tracing::warn!(
                        url = %repo.url,
                        change = %change,
                        "failed to remove iteration-pending marker on Completed: {e:#}"
                    );
                }
                // Archive BEFORE the commit so the single commit captures
                // both the executor's implementation diff AND the archive
                // rename. After this sequence the working tree is clean,
                // even for the trailing change of a pass — no dangling
                // rename for the next iteration's dirty-check to trip on.
                let spec_root = crate::spec_root::SpecRoot::for_repo(repo, workspace);
                queue::archive_at(&spec_root, change)?;
                git::add_all(workspace)?;
                git::commit(workspace, &subject)?;
            }
            Ok(QueueStep::Archived)
        }
    }
}

/// Polling-loop arm for `ExecutorOutcome::IterationRequested` (a27a1).
/// Performs, in order:
///
/// 1. Commit the workspace's diff to the agent branch with the message
///    `iteration <N> of <change>: <reason-truncated-to-80-chars>`. If
///    the working tree is clean (the agent emitted iteration_request
///    without modifying anything), the commit step is skipped with a
///    `tracing::warn!` AND the function proceeds to step 3.
/// 2. Force-push the agent branch to the remote. Push failure aborts:
///    `tracing::error!` AND skip steps 3 (no marker written, so the next
///    polling iteration treats the change as normally-pending).
/// 3. Write `.iteration-pending.json` atomically with the new state.
/// 4. Drop `.in-progress`.
///
/// Step 4 ALWAYS runs (even on push failure, AND even if the marker
/// write also fails), so the change is never left locked.
///
/// This arm SHALL NOT call any PR-open OR PR-comment routine. PRs are
/// reserved for the FINAL iteration's `Completed` outcome.
#[allow(clippy::too_many_arguments)]
async fn handle_iteration_requested(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    change: &str,
    completed_tasks: Vec<String>,
    remaining_tasks: Vec<String>,
    reason: String,
    iteration_number: u32,
) -> Result<QueueStep> {
    // Always unlock at the end of the arm — collect any deferred
    // errors first AND treat unlock as a best-effort cleanup.
    let result = run_iteration_requested_steps(
        paths,
        workspace,
        repo,
        github_cfg,
        change,
        completed_tasks,
        remaining_tasks,
        reason,
        iteration_number,
    )
    .await;
    if let Err(e) = queue::unlock(workspace, change) {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            "failed to drop .in-progress on IterationRequested arm: {e:#}"
        );
    }
    result
}

/// Inner workflow of [`handle_iteration_requested`]. Pulled out so the
/// outer wrapper can guarantee `.in-progress` is dropped on every exit
/// path (success, push failure, marker-write failure).
#[allow(clippy::too_many_arguments)]
async fn run_iteration_requested_steps(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    change: &str,
    completed_tasks: Vec<String>,
    remaining_tasks: Vec<String>,
    reason: String,
    iteration_number: u32,
) -> Result<QueueStep> {
    // Step 1: commit the diff (or skip if clean).
    // The .in-progress file is untracked, but `git add -A` would sweep
    // it into the commit. Drop the lock first (matches the other
    // outcome arms' discipline). The outer wrapper's unlock-on-exit is
    // idempotent against this drop.
    queue::unlock(workspace, change)?;
    let dirty = git::status_porcelain(workspace)?;
    if dirty.is_empty() {
        tracing::warn!(
            url = %repo.url,
            change = %change,
            iteration_number,
            "IterationRequested with clean working tree: agent emitted iteration_request without modifying any files; writing marker anyway (lack-of-progress will count against the cap on the next iteration)"
        );
    } else {
        let subject = build_iteration_commit_subject(change, iteration_number, &reason);
        git::add_all(workspace)?;
        if let Err(e) = git::commit(workspace, &subject) {
            // Mirror the clean-tree case: log AND proceed to write the
            // marker. A non-clean tree that nonetheless fails to commit
            // is an anomaly (probably a config issue like missing
            // user.email); the marker still belongs because the agent
            // INTENDED to advance, AND the cap will catch a loop.
            tracing::warn!(
                url = %repo.url,
                change = %change,
                iteration_number,
                "iteration-request commit failed (proceeding to marker): {e:#}"
            );
        }
    }

    // Step 2: force-push the agent branch to the remote.
    let push_remote = if github_cfg.fork_owner.is_some() {
        "fork"
    } else {
        "origin"
    };
    if let Err(e) =
        git::push_force_with_lease(workspace, &repo.agent_branch, push_remote)
    {
        tracing::error!(
            url = %repo.url,
            change = %change,
            iteration_number,
            "iteration-request force-push failed; NOT writing marker: {e:#}"
        );
        // Per D5: push failure leaves no marker. The change reverts to
        // normal pending behaviour on the next polling cycle.
        return Ok(QueueStep::IterationPending);
    }

    // Step 3: write the iteration-pending marker atomically. The marker
    // lives under `<state>/iteration-pending/<basename>/<change>.json`
    // (NOT in the workspace) per a16's "daemon bookkeeping never appears
    // in the managed repo's working tree" rule; this avoids the
    // `git clean -fd` wipe that broke earlier in-workspace implementations.
    let marker = crate::iteration_pending::IterationPendingMarker {
        completed_tasks,
        remaining_tasks,
        reason,
        iteration_number,
    };
    let basename_for_marker = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    if let Err(e) = crate::iteration_pending::write_marker(
        paths,
        basename_for_marker,
        change,
        &marker,
    ) {
        tracing::error!(
            url = %repo.url,
            change = %change,
            iteration_number,
            "iteration-pending marker write failed; next iteration will see no continuation context: {e:#}"
        );
    }
    Ok(QueueStep::IterationPending)
}

/// Build the commit subject for an `IterationRequested` arm's WIP
/// commit. Format: `iteration <N> of <change>: <reason>` truncated to
/// keep the subject under 80 chars (the same discipline as
/// `build_commit_subject`).
fn build_iteration_commit_subject(
    change: &str,
    iteration_number: u32,
    reason: &str,
) -> String {
    const MAX_SUBJECT_LEN: usize = 80;
    let prefix = format!("iteration {iteration_number} of {change}: ");
    let room = MAX_SUBJECT_LEN.saturating_sub(prefix.len());
    let trimmed_reason: String =
        reason.lines().next().unwrap_or("").trim().chars().take(room).collect();
    format!("{prefix}{trimmed_reason}")
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

/// Test-only routing hook for the PR-creation HTTP call. When set, the
/// helper below targets the override URL (a mockito server) instead of
/// `github::DEFAULT_API_BASE`. Tests acquire `test_hooks::lock()` before
/// installing the override so two tests cannot race on the process-wide
/// static. Never linked outside `cfg(test)`.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static GITHUB_API_BASE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn cell() -> &'static Mutex<Option<String>> {
        GITHUB_API_BASE.get_or_init(|| Mutex::new(None))
    }

    /// Snapshot the currently-installed override URL (or `None`).
    pub fn github_api_base() -> Option<String> {
        cell().lock().unwrap().clone()
    }

    /// Install a PR-creation API-base override for the duration of a
    /// test. The test holds the returned guard until it has finished
    /// reading mockito's recorded calls; on drop the override is cleared.
    pub fn set_github_api_base(value: Option<String>) {
        *cell().lock().unwrap() = value;
    }

    /// Process-wide mutex held by any test that installs the PR-creation
    /// override. Serializes tests that share the static so two concurrent
    /// tests do not clobber each other's override URL.
    pub fn lock<'a>() -> MutexGuard<'a, ()> {
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }
}

/// PR-creation routing wrapper. In production this is a thin shim around
/// `github::create_pull_request` (targets the live GitHub API). Under
/// `cfg(test)`, when an override is installed via `test_hooks`, the call
/// is rerouted to `github::create_pull_request_at_for_test` against a
/// mockito server URL so the test can assert head/base/title/body.
#[allow(clippy::too_many_arguments)]
async fn create_pull_request_via_hook(
    owner: &str,
    repo: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
    token: &str,
    review_report: Option<&ReviewReport>,
    draft: bool,
) -> Result<github::CreatedPr> {
    #[cfg(test)]
    {
        if let Some(api_base) = test_hooks::github_api_base() {
            return github::create_pull_request_at_for_test(
                &api_base,
                owner,
                repo,
                head,
                base,
                title,
                body,
                token,
                review_report,
                draft,
            )
            .await;
        }
    }
    github::create_pull_request(
        owner,
        repo,
        head,
        base,
        title,
        body,
        token,
        review_report,
        draft,
    )
    .await
}

/// Build the initial per-PR `RevisionState` written at PR-open time when the
/// original automatic review ran (a33 §7.2 baseline + the per-PR caps).
///
/// The caps are SOURCED — never hardcoded — so this init agrees with the
/// revision dispatcher's own state init in `revisions::process_one_pr`:
/// - `revision_cap` is the resolved `executor.max_auto_revisions_per_pr`
///   (already clamped at config load) — bounds AUTOMATIC revisions only.
/// - `code_review_cap` is `reviewer.max_code_reviews_per_pr()`, where `None`
///   means UNLIMITED (the a47 default). Hardcoding `Some(5)` here would
///   silently re-cap re-reviews on every daemon-opened PR even when the
///   operator set no cap, defeating a47's default-unlimited re-reviews.
fn initial_revision_state_at_pr_open(
    pr_number: u64,
    agent_branch: String,
    now: chrono::DateTime<chrono::Utc>,
    revision_cap: u32,
    reviewer: Option<&CodeReviewer>,
    head_sha: String,
) -> crate::revisions::RevisionState {
    crate::revisions::RevisionState {
        pr_number,
        agent_branch,
        last_seen_comment_at: now,
        auto_revisions_applied: 0,
        revision_cap,
        cap_decline_posted: false,
        code_reviews_applied: 0,
        code_review_cap: reviewer.and_then(|r| r.max_code_reviews_per_pr()),
        cap_decline_posted_for_code_review: false,
        last_suggested_rereview_at_revisions_count: None,
        original_review_head_sha: Some(head_sha),
    }
}

#[allow(clippy::too_many_arguments)]
async fn open_pull_request(
    paths: &DaemonPaths,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    changes: &[String],
    includes_self_heal: bool,
    review_report: Option<&ReviewReport>,
    reviewer: Option<&CodeReviewer>,
    revision_cap: u32,
    draft: bool,
    reviewer_revision_concerns: &[ReviewConcern],
    chatops_ctx: Option<&ChatOpsContext>,
    workspace: &Path,
) -> Result<()> {
    let (owner, repo_name) = github::parse_repo_url(&repo.url)?;
    // PAT routing uses the UPSTREAM owner, not the fork owner — the PR is
    // posted to upstream's /pulls endpoint regardless of fork-PR mode, so
    // the credential authorizing that call must have access to upstream.
    let token = crate::github_credentials::resolve_token(github_cfg, &owner)?;
    // Audit-only iterations have no implementer-processed changes; the
    // agent branch carries only the audit's `audit: <type> proposals
    // (N change(s))` commits. Build the PR title + body from those
    // commit subjects so reviewers see which audits fired.
    let (title, body) = if changes.is_empty() {
        let range = format!("{}..{}", repo.base_branch, repo.agent_branch);
        let subjects = git::log_subjects(workspace, &range).unwrap_or_default();
        (
            build_audit_only_pr_title(&subjects),
            build_audit_only_pr_body(&subjects),
        )
    } else {
        (
            build_pr_title(changes),
            build_pr_body(workspace, changes, includes_self_heal),
        )
    };

    // In fork-PR mode the `head` is namespaced `<fork-owner>:<branch>` for
    // GitHub to recognize the cross-repo PR. Direct-push mode uses the bare
    // branch name (same-repo PR).
    let head = match github_cfg.fork_owner.as_deref() {
        Some(fork_owner) => format!("{fork_owner}:{}", repo.agent_branch),
        None => repo.agent_branch.clone(),
    };

    // OSS-fork support (a26): when `auto_submit_pr: false`, skip the
    // PR-creation API call. The branch has already been pushed to its
    // remote by the caller; we surface the branch URL AND a
    // templated `gh pr create` command to chatops so the operator can
    // open the PR manually after local review.
    if !repo.auto_submit_pr {
        let branch_url = compose_branch_url(&owner, &repo_name, &repo.agent_branch);
        let pr_base = repo
            .upstream
            .as_ref()
            .map(|u| u.branch.as_str())
            .unwrap_or(&repo.base_branch);
        let suggested = format!(
            "gh pr create --base {pr_base} --head {}",
            repo.agent_branch
        );
        maybe_post_branch_pushed_no_pr(
            repo,
            chatops_ctx,
            &branch_url,
            &suggested,
            changes.len(),
        )
        .await;
        tracing::info!(
            url = %repo.url,
            branch_url = %branch_url,
            "auto_submit_pr: false — skipped PR creation; surfaced branch URL to chatops"
        );
        // Best-effort: post implementer-summary comments only when a PR
        // exists. Without a PR we have no number to attach them to —
        // skip and rely on chatops surfacing.
        return Ok(());
    }

    let pr = match create_pull_request_via_hook(
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
                paths,
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

    // a33 task 7.2: record the agent-branch head SHA at the time the
    // original automatic review completed, so the diff-overlap suggestion
    // path has a baseline. Best-effort — failures here do NOT abort PR
    // opening. Only fires when a review_report is present (i.e. a
    // reviewer ran on this iteration).
    if review_report.is_some()
        && let Ok(head_sha) = git::rev_parse(workspace, &repo.agent_branch)
    {
        {
            let now = chrono::Utc::now();
            let existing = crate::revisions::read_state(paths, workspace, pr.number)
                .ok()
                .flatten();
            let state = match existing {
                Some(mut s) => {
                    s.original_review_head_sha = Some(head_sha);
                    s
                }
                None => initial_revision_state_at_pr_open(
                    pr.number,
                    repo.agent_branch.clone(),
                    now,
                    revision_cap,
                    reviewer,
                    head_sha,
                ),
            };
            if let Err(e) = crate::revisions::write_state(paths, workspace, &state) {
                tracing::warn!(
                    url = %repo.url,
                    pr_number = pr.number,
                    "failed to persist original_review_head_sha: {e:#}"
                );
            }
        }
    }

    // Best-effort: post a one-line ChatOps notification with a link to
    // the new PR. PR creation already succeeded; never propagate a
    // failure from this step.
    maybe_post_pr_opened(repo, chatops_ctx, &pr.html_url, changes.len()).await;

    // Best-effort: post a follow-up comment with each change's implementer
    // stdout. PR creation already succeeded; never propagate a failure
    // from this step.
    post_implementer_summary_comment(
        paths,
        github::DEFAULT_API_BASE,
        workspace,
        &owner,
        &repo_name,
        pr.number,
        changes,
        &token,
    )
    .await;

    // Best-effort: post one `<!-- reviewer-revision -->` comment per
    // taken reviewer concern, so the revision dispatcher (running on the
    // next polling iteration) picks them up and forwards them to the
    // implementer agent. PR creation already succeeded; per-concern post
    // failures are logged at WARN but never propagated.
    if !reviewer_revision_concerns.is_empty() {
        post_reviewer_revision_comments(
            github::DEFAULT_API_BASE,
            &owner,
            &repo_name,
            pr.number,
            reviewer_revision_concerns,
            &token,
        )
        .await;
    }

    Ok(())
}

/// Post one `<!-- reviewer-revision -->` PR issue comment per concern.
/// The body shape matches the spec: marker line, then
/// `@<bot> revise <actionable_request>`. Per-concern failures log at WARN
/// and never abort; the iteration's PR creation has already succeeded.
async fn post_reviewer_revision_comments(
    api_base: &str,
    upstream_owner: &str,
    upstream_repo: &str,
    pr_number: u64,
    concerns: &[ReviewConcern],
    token: &str,
) {
    // Resolve the bot's GitHub login once — the trigger pattern is
    // `@<bot> revise ...`. Without the username we cannot construct a
    // valid trigger, so we abort the posting step (logging a WARN); the
    // iteration's PR creation still succeeded.
    let bot_username = match github::self_bot_username(api_base, token).await {
        Ok(name) => name,
        Err(e) => {
            tracing::warn!(
                pr_number,
                "reviewer-revision posting skipped: bot-username lookup failed: {e:#}"
            );
            return;
        }
    };
    for (idx, concern) in concerns.iter().enumerate() {
        let request = match concern.actionable_request.as_deref() {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => continue, // shouldn't happen; partition filters these out
        };
        let body = format!(
            "{}\n@{} revise {}",
            crate::revisions::REVIEWER_REVISION_MARKER,
            bot_username,
            request,
        );
        let post_result = if api_base == github::DEFAULT_API_BASE {
            github::create_issue_comment(upstream_owner, upstream_repo, pr_number, &body, token)
                .await
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
        if let Err(e) = post_result {
            tracing::warn!(
                pr_number,
                concern_index = idx,
                "reviewer-revision comment post failed: {e:#}"
            );
        }
    }
}

/// Decide which concerns from `report.concerns` get posted as
/// `<!-- reviewer-revision -->` PR comments and which are dropped due to
/// the per-PR revision-cap budget. Pre-conditions assume the caller has
/// already gated on `reviewer.auto_revise == true`.
///
/// Selection rules:
/// - The verdict is NOT consulted. The actionability signal lives at the
///   per-concern granularity: a concern is "revisable" when
///   `should_request_revision == true` AND `actionable_request` is
///   non-empty (whitespace-trimmed). This fires under any verdict
///   (`Pass`, `Concerns`, OR `Block`). Concerns failing either condition
///   stay as commentary in the `## Code Review` section and do not post.
///   (`Block` retains its separate effect of marking the PR draft; that
///   is handled by the caller and no longer gates this function.)
/// - When the revisable set exceeds `budget`, the first `budget`
///   concerns (in reviewer output order — the template instructs
///   most-critical-first) are taken; the remainder are annotated into
///   `report.markdown` with `(not auto-revised; cap budget exhausted)`
///   so the human reader of the PR body sees what was skipped.
/// - When the revisable set is empty, a WARN is logged surfacing the
///   "you flipped the flag but your template produced no actionable
///   concerns" misconfiguration.
fn partition_and_annotate_reviewer_revisions(
    report: &mut ReviewReport,
    budget: u32,
) -> Vec<ReviewConcern> {
    let revisable: Vec<ReviewConcern> = report
        .concerns
        .iter()
        .filter(|c| {
            c.should_request_revision
                && c.actionable_request
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
        })
        .cloned()
        .collect();
    if revisable.is_empty() {
        tracing::warn!(
            "reviewer auto-revise is enabled but no concerns had `actionable_request` + `should_request_revision: true` populated; verify the reviewer prompt template has been updated to emit these fields."
        );
        return Vec::new();
    }
    let budget_us = budget as usize;
    let (taken, dropped): (Vec<ReviewConcern>, Vec<ReviewConcern>) = if revisable.len() > budget_us
    {
        (
            revisable[..budget_us].to_vec(),
            revisable[budget_us..].to_vec(),
        )
    } else {
        (revisable, Vec::new())
    };
    if !dropped.is_empty() {
        if report.per_change_sections.is_empty() {
            annotate_dropped_in_markdown(&mut report.markdown, &dropped);
        } else {
            annotate_dropped_in_per_change_sections(report, &dropped);
        }
    }
    taken
}

/// Append the "dropped (cap budget exhausted)" footer to a bundled-mode
/// report's single `## Code Review` markdown.
fn annotate_dropped_in_markdown(markdown: &mut String, dropped: &[ReviewConcern]) {
    if !markdown.ends_with("\n\n") {
        if markdown.ends_with('\n') {
            markdown.push('\n');
        } else {
            markdown.push_str("\n\n");
        }
    }
    markdown
        .push_str("### Reviewer-initiated revisions: dropped (cap budget exhausted)\n");
    for c in dropped {
        markdown.push_str(&format!(
            "- (not auto-revised; cap budget exhausted) {}\n",
            c.summary
        ));
    }
}

/// In per-change mode, group dropped concerns by their originating
/// change slug and append the footer to each matching `PerChangeSection`'s
/// markdown so the annotation lands in the correct `## Code Review:
/// <slug>` PR-body section. Dropped concerns lacking a slug attribution
/// (shouldn't happen — `synthesize_per_change_report` always stamps it)
/// are appended to the LAST section as a safety net.
fn annotate_dropped_in_per_change_sections(
    report: &mut ReviewReport,
    dropped: &[ReviewConcern],
) {
    use std::collections::HashMap;
    let mut by_slug: HashMap<String, Vec<&ReviewConcern>> = HashMap::new();
    let mut unattributed: Vec<&ReviewConcern> = Vec::new();
    for c in dropped {
        match c.change_slug.as_deref() {
            Some(slug) => by_slug.entry(slug.to_string()).or_default().push(c),
            None => unattributed.push(c),
        }
    }
    for section in report.per_change_sections.iter_mut() {
        if let Some(concerns) = by_slug.get(&section.change_slug) {
            annotate_dropped_in_markdown(&mut section.markdown, &concerns_to_owned(concerns));
        }
    }
    if !unattributed.is_empty() {
        if let Some(last) = report.per_change_sections.last_mut() {
            annotate_dropped_in_markdown(&mut last.markdown, &concerns_to_owned(&unattributed));
        }
    }
}

fn concerns_to_owned(refs: &[&ReviewConcern]) -> Vec<ReviewConcern> {
    refs.iter().map(|c| (*c).clone()).collect()
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
    paths: &DaemonPaths,
    api_base: &str,
    workspace: &Path,
    upstream_owner: &str,
    upstream_repo: &str,
    pr_number: u64,
    processed: &[String],
    token: &str,
) {
    let body = build_implementer_summary(paths, workspace, processed);
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
fn build_implementer_summary(paths: &DaemonPaths, workspace: &Path, processed: &[String]) -> String {
    let timeout_fallback =
        "(executor timed out before final summary; see daemon log for action stream)";
    let mut sections = Vec::new();
    for change in processed {
        let path = crate::executor::claude_cli::run_log_path(paths, workspace, change);
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
        // Prefer the FINAL ANSWER section (JSON streaming mode). Fall
        // back to the legacy STDOUT section (text-mode opt-out OR a
        // legacy log carried over from before this change shipped).
        let body = if let Some(final_answer) =
            crate::executor::event_log::read_final_answer(&path)
        {
            final_answer
        } else if raw.contains("=== FINAL ANSWER (") {
            // FINAL ANSWER section present but empty → timeout-kill case.
            timeout_fallback.to_string()
        } else {
            let stdout = extract_stdout_section(&raw);
            let trimmed = stdout.trim_end();
            if trimmed.is_empty() {
                "_(no implementer output captured)_".to_string()
            } else {
                trimmed.to_string()
            }
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
///
/// `pub(crate)` so the revision success-comment composer
/// (`revisions::compose_revision_success_comment`) can reuse the same
/// limit-and-marker behavior. The marker text is generalized (it does
/// not say "implementer") because it now serves both the implementer
/// summary AND the revision summary; both recover the full output from
/// the same per-change run-log path.
pub(crate) fn truncate_to_fit(body: String, max: usize) -> String {
    if body.len() <= max {
        return body;
    }
    let mut cut = max;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = body[..cut].to_string();
    truncated.push_str(
        "\n\n_[summary truncated to fit GitHub comment limit; full output at <logs_dir>/runs/<workspace-basename>/<change>.log]_",
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
        let why = read_change_why(workspace, change);
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

/// Parse audit-produced commit subjects of shape
/// `audit: <type> proposals (<N> change(s))` and return `(total, types)`:
/// the sum of `N` across all matching subjects AND the deduplicated list
/// of audit types preserving first-seen order. Subjects not matching the
/// canonical shape contribute nothing to the totals (they are listed
/// verbatim in the PR body, but the title summary skips them).
fn summarize_audit_commit_subjects(subjects: &[String]) -> (u32, Vec<String>) {
    let re = match regex::Regex::new(
        r"^audit: (?P<type>\S+) proposals \((?P<n>\d+) change\(s\)\)$",
    ) {
        Ok(r) => r,
        Err(_) => return (0, Vec::new()),
    };
    let mut total: u32 = 0;
    let mut types: Vec<String> = Vec::new();
    for s in subjects {
        if let Some(caps) = re.captures(s) {
            if let Ok(n) = caps["n"].parse::<u32>() {
                total = total.saturating_add(n);
            }
            let t = caps["type"].to_string();
            if !types.contains(&t) {
                types.push(t);
            }
        }
    }
    (total, types)
}

/// Buckets agent-branch commits go into for content-aware PR-body
/// rendering (a38). Each bucket holds the verbatim commit subjects that
/// matched its category prefix; the renderer enumerates them under the
/// category's section AND only emits a section for non-empty buckets.
#[derive(Debug, Default, PartialEq, Eq)]
struct CommitCategories {
    /// `audit: <type> proposals (<N> change(s))` subjects (a20a3).
    audit: Vec<String>,
    /// `iteration <N> of <change>: <reason>` subjects (a27a1).
    iteration_wip: Vec<String>,
    /// Implementer-archived commits — `archive: <change>: ...` (self-heal)
    /// OR the canonical `<aNN-slug>: <why>` shape produced by
    /// [`build_commit_subject`].
    implementer: Vec<String>,
    /// Anything else (manual edits, merges, unrecognized prefixes).
    other: Vec<String>,
}

impl CommitCategories {
    fn total(&self) -> usize {
        self.audit.len()
            + self.iteration_wip.len()
            + self.implementer.len()
            + self.other.len()
    }

    /// Stable, human-friendly category labels for the PR title's
    /// "across <categories>" suffix (mixed-content title shape).
    fn nonempty_labels(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        if !self.audit.is_empty() {
            out.push("audit");
        }
        if !self.iteration_wip.is_empty() {
            out.push("iteration WIP");
        }
        if !self.implementer.is_empty() {
            out.push("implementer");
        }
        if !self.other.is_empty() {
            out.push("other");
        }
        out
    }
}

/// Categorize agent-branch commit subjects by message-prefix into the
/// buckets the audit-only PR body renderer uses. The match is
/// prefix-anchored; the categorizer is deliberately permissive — the
/// renderer treats unknown shapes as `other` rather than failing.
fn categorize_commit_subjects(subjects: &[String]) -> CommitCategories {
    // Lazily compile each pattern once per call. The renderer is a
    // hot-ish path (every PR opens), but not so hot that a static
    // OnceLock-cached regex pays its weight; keep it simple.
    let audit_re = regex::Regex::new(
        r"^audit: \S+ proposals \(\d+ change\(s\)\)$",
    )
    .expect("static audit regex compiles");
    let iteration_re = regex::Regex::new(r"^iteration \d+ of \S+:")
        .expect("static iteration regex compiles");
    let archive_re = regex::Regex::new(r"^archive: ").expect("static archive regex compiles");
    // `<aNN-slug>: <why>` from `build_commit_subject` — the aNN prefix
    // convention is enforced by the openspec change-name discipline.
    let implementer_re = regex::Regex::new(r"^[a-z]+\d+[a-z0-9-]*: ")
        .expect("static implementer regex compiles");

    let mut cats = CommitCategories::default();
    for s in subjects {
        if audit_re.is_match(s) {
            cats.audit.push(s.clone());
        } else if iteration_re.is_match(s) {
            cats.iteration_wip.push(s.clone());
        } else if archive_re.is_match(s) || implementer_re.is_match(s) {
            cats.implementer.push(s.clone());
        } else {
            cats.other.push(s.clone());
        }
    }
    cats
}

/// PR title for the audit-only-PR path. When EVERY commit on the agent
/// branch matches the canonical `audit: <type> proposals (<N> change(s))`
/// shape, the title takes today's `audit-only: <N> proposal(s) from
/// <comma-separated-types>` form. When commits are mixed (audit + iteration
/// WIP + ...) OR when no audit commits exist (the defensive case — by
/// a38's suppression rule, an iteration with iteration-pending markers
/// shouldn't reach this renderer in production), the title falls back to
/// a generic `agent-q changes: <N> commits across <categories>` shape so
/// the title accurately describes what the PR contains.
fn build_audit_only_pr_title(commit_subjects: &[String]) -> String {
    const MAX_LEN: usize = 80;
    const ELLIPSIS: char = '…';
    let cats = categorize_commit_subjects(commit_subjects);
    let title = build_audit_only_pr_title_from_categories(commit_subjects, &cats);
    if title.chars().count() <= MAX_LEN {
        return title;
    }
    let take = MAX_LEN.saturating_sub(1);
    let truncated: String = title.chars().take(take).collect();
    let mut out = truncated;
    out.push(ELLIPSIS);
    out
}

/// Inner of [`build_audit_only_pr_title`] — works on already-categorized
/// commits. Pulled out so tests can drive the title shape from a
/// fixture `CommitCategories` directly without re-running the regex
/// match.
fn build_audit_only_pr_title_from_categories(
    commit_subjects: &[String],
    cats: &CommitCategories,
) -> String {
    let total = cats.total();
    if total == 0 {
        return "audit-only: agent-branch commits without implementer changes".to_string();
    }
    // Pure-audit case: today's canonical title.
    if !cats.audit.is_empty()
        && cats.iteration_wip.is_empty()
        && cats.implementer.is_empty()
        && cats.other.is_empty()
    {
        let (audit_total, types) = summarize_audit_commit_subjects(commit_subjects);
        if !types.is_empty() {
            return format!(
                "audit-only: {} proposal(s) from {}",
                audit_total,
                types.join(", "),
            );
        }
        // Audit commits present but none matched the proposals-counting
        // shape (unusual — would be an audit type that produced an audit
        // commit without proposal counts). Fall through to generic.
    }
    // Generic mixed-or-non-audit shape.
    let labels = cats.nonempty_labels();
    if labels.is_empty() {
        return "audit-only: agent-branch commits without implementer changes".to_string();
    }
    format!(
        "agent-q changes: {} commit(s) across {}",
        total,
        labels.join(", "),
    )
}

/// PR body for the audit-only-PR path. Partitions the agent-branch commit
/// subjects into categories (audit-produced, iteration WIP, implementer-
/// archived, other) AND emits one section per non-empty category. The
/// "audit-produced proposals only" framing is included ONLY when audit
/// commits actually exist — fixing PR #77's misleading body that named
/// "audit-produced proposals" with zero audit commits in the diff.
fn build_audit_only_pr_body(commit_subjects: &[String]) -> String {
    let cats = categorize_commit_subjects(commit_subjects);
    build_audit_only_pr_body_from_categories(&cats)
}

/// Inner of [`build_audit_only_pr_body`] — works on already-categorized
/// commits. Pulled out so tests can drive the body content from a
/// fixture `CommitCategories` directly.
fn build_audit_only_pr_body_from_categories(cats: &CommitCategories) -> String {
    let mut s = String::new();

    // Lead sentence: framing depends on what's actually present.
    if cats.total() == 0 {
        s.push_str(
            "This PR was opened from the audit-only path but no commit subjects were readable from the agent branch.\n",
        );
        return s;
    }
    if !cats.audit.is_empty()
        && cats.iteration_wip.is_empty()
        && cats.implementer.is_empty()
        && cats.other.is_empty()
    {
        s.push_str(
            "This PR ships audit-produced proposals only — no implementer changes this iteration.\n\n",
        );
    } else {
        s.push_str(
            "This PR ships agent-branch commits from multiple sources. Sections below enumerate each category present in the diff.\n\n",
        );
    }

    if !cats.audit.is_empty() {
        s.push_str("## Audit-produced proposals\n\n");
        for subject in &cats.audit {
            s.push_str(&format!("- {subject}\n"));
        }
        s.push('\n');
    }
    if !cats.iteration_wip.is_empty() {
        s.push_str("## Iteration WIP\n\n");
        s.push_str(
            "_The following commits are in-progress iteration work (a27a1). They are NOT ready to merge as-is; the iteration sequence has not concluded._\n\n",
        );
        for subject in &cats.iteration_wip {
            s.push_str(&format!("- {subject}\n"));
        }
        s.push('\n');
    }
    if !cats.implementer.is_empty() {
        s.push_str("## Implementer-archived changes\n\n");
        for subject in &cats.implementer {
            s.push_str(&format!("- {subject}\n"));
        }
        s.push('\n');
    }
    if !cats.other.is_empty() {
        s.push_str("## Other commits\n\n");
        for subject in &cats.other {
            s.push_str(&format!("- {subject}\n"));
        }
        s.push('\n');
    }

    if !cats.audit.is_empty() {
        s.push_str(
            "Each `audit: <type>` commit creates new `openspec/changes/<prefix>-*` directories that the next polling iteration will pick up via `list_pending` and route to the implementer.\n",
        );
    }
    s
}

/// Read a change's proposal `## Why` section, preferring the archive
/// location and falling back to the active path. Step 1 looks up
/// `<workspace>/openspec/changes/archive/*-<change>/proposal.md` (picking
/// the lexicographically last match if multiple exist). Step 2, on
/// archive miss, tries `<workspace>/openspec/changes/<change>/proposal.md`.
/// When the active-path fallback yields a parseable `## Why`, emit a
/// per-change WARN so operators can correlate the PR with the likely
/// upstream archive failure. Returns `None` if both paths miss or
/// neither yields a `## Why` heading; no WARN fires in those cases.
fn read_change_why(workspace: &Path, change: &str) -> Option<String> {
    if let Some(why) = read_proposal_why_from_archive(workspace, change) {
        return Some(why);
    }
    let active = workspace
        .join("openspec/changes")
        .join(change)
        .join("proposal.md");
    if active.is_file() {
        let raw = std::fs::read_to_string(&active).ok()?;
        if let Some(why) = extract_why_section(&raw) {
            tracing::warn!(
                change = %change,
                "proposal read from active path, not archive — likely indicates an upstream archive failure for this iteration"
            );
            return Some(why);
        }
    }
    None
}

/// Step 1 of [`read_change_why`]: locate
/// `<workspace>/openspec/changes/archive/*-<change>/proposal.md` (picking
/// the lexicographically last match if multiple exist), read the file,
/// and return its `## Why` section. Returns `None` if the directory or
/// file is missing, the read fails, or no `## Why` heading is present.
fn read_proposal_why_from_archive(workspace: &Path, change: &str) -> Option<String> {
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
pub fn tasks_md_all_complete(spec_root: &crate::spec_root::SpecRoot, change: &str) -> Result<bool> {
    let tasks_path = spec_root.changes_dir().join(change).join("tasks.md");
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
pub fn openspec_validate_strict_passes(spec_root: &crate::spec_root::SpecRoot, change: &str) -> bool {
    match std::process::Command::new("openspec")
        .args(["validate", change, "--strict"])
        .current_dir(spec_root.openspec_cwd())
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
    _paths: &DaemonPaths,
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
    paths: &DaemonPaths,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
) -> bool {
    #[cfg(test)]
    {
        if let Some(api_base) = test_hooks::github_api_base() {
            return open_pr_exists_for_agent_branch_at(paths, &api_base, repo, github_cfg).await;
        }
    }
    open_pr_exists_for_agent_branch_at(paths, github::DEFAULT_API_BASE, repo, github_cfg).await
}

// ====================================================================
// Audit-triage processing (audit-reply-acts `send it` flow)
// ====================================================================

/// Process every queued audit-triage `thread_ts` for this repo. The
/// caller passes the per-repo queue snapshot already drained; this
/// function loads each `AuditThreadState`, runs the executor in triage
/// mode, discards non-spec writes, and opens at most one spec PR (a43).
///
/// Failures inside one triage do NOT abort the others — each entry is
/// processed independently, errors are logged and the audit-thread
/// state's `status` is updated to `TriageFailed` so the operator can
/// retry via `@<bot> send it` again.
pub async fn process_audit_triages(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    thread_tses: &[String],
) -> Result<()> {
    use crate::audits::threads;
    // Workspace must be clean and on a fresh agent_branch off base
    // before we let the executor loose on it. The downstream
    // `run_pass_through_commits` does the same setup; we duplicate it
    // here because triage runs OUTSIDE the normal pass and leaves the
    // workspace in whatever state the executor produces.
    let fork_url = match github_cfg.fork_owner.as_deref() {
        Some(owner) => Some(crate::github::derive_fork_url(&repo.url, owner)?),
        None => None,
    };
    let fork_arg = fork_url.as_deref().map(|u| (u, repo.agent_branch.as_str()));
    crate::workspace::ensure_initialized(paths, workspace, &repo.url, fork_arg)
        .with_context(|| "audit-triage: workspace ensure_initialized".to_string())?;
    let _ = crate::queue::clear_stale_locks(workspace);
    let _ = git::reset_hard_head(workspace);
    let _ = git::clean_force(workspace);
    git::fetch(workspace)
        .with_context(|| "audit-triage: git fetch".to_string())?;
    git::checkout(workspace, &repo.base_branch)
        .with_context(|| format!("audit-triage: checkout `{}`", repo.base_branch))?;
    git::pull_ff_only(workspace, &repo.base_branch)
        .with_context(|| format!("audit-triage: pull --ff-only `{}`", repo.base_branch))?;
    git::recreate_branch(workspace, &repo.agent_branch)
        .with_context(|| format!("audit-triage: recreate `{}`", repo.agent_branch))?;

    for thread_ts in thread_tses {
        let state_root = threads::default_state_root(paths);
        let mut state = match threads::read_state(&state_root, thread_ts) {
            Ok(Some(s)) => s,
            Ok(None) => {
                tracing::warn!(
                    thread_ts = %thread_ts,
                    "audit-triage: no state file (entry pruned between trigger and processing); skipping"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    thread_ts = %thread_ts,
                    "audit-triage: state read failed: {e:#}"
                );
                continue;
            }
        };

        // Build the canonical-specs index from openspec/specs/<name>/.
        let canonical_specs_index = build_canonical_specs_index(workspace);
        let ctx = crate::executor::TriageContext {
            findings: state.findings_excerpt.clone(),
            audit_type: state.audit_type.clone(),
            repo_url: state.repo_url.clone(),
            canonical_specs_index,
        };

        tracing::info!(
            url = %repo.url,
            thread_ts = %thread_ts,
            audit_type = %state.audit_type,
            "audit-triage: invoking executor in triage mode"
        );

        let outcome = executor.run_triage(workspace, &ctx).await;
        match outcome {
            Ok(crate::executor::ExecutorOutcome::Completed { final_answer }) => {
                if let Err(e) = process_completed_triage(
                    paths,
                    workspace,
                    repo,
                    github_cfg,
                    chatops_ctx,
                    &mut state,
                    final_answer.as_deref(),
                )
                .await
                {
                    tracing::error!(
                        url = %repo.url,
                        thread_ts = %thread_ts,
                        "audit-triage: post-Completed processing failed: {e:#}"
                    );
                    mark_triage_failed(
                        paths,
                        &state_root,
                        &mut state,
                        format!("post-Completed processing: {e:#}"),
                        chatops_ctx,
                    )
                    .await;
                }
            }
            Ok(crate::executor::ExecutorOutcome::Failed { reason }) => {
                tracing::error!(
                    url = %repo.url,
                    thread_ts = %thread_ts,
                    "audit-triage: executor returned Failed: {reason}"
                );
                mark_triage_failed(paths, &state_root, &mut state, reason, chatops_ctx).await;
            }
            Ok(crate::executor::ExecutorOutcome::AskUser { .. }) => {
                // Triage's escalation: the agent asked a question. The
                // existing chatops escalation machinery is per-change;
                // for triage we treat AskUser as a no-op (status stays
                // TriagePending so a future iteration could retry).
                tracing::info!(
                    url = %repo.url,
                    thread_ts = %thread_ts,
                    "audit-triage: executor returned AskUser; leaving status TriagePending"
                );
            }
            Ok(crate::executor::ExecutorOutcome::SpecNeedsRevision { .. }) => {
                tracing::warn!(
                    url = %repo.url,
                    thread_ts = %thread_ts,
                    "audit-triage: executor returned SpecNeedsRevision; treating as failure"
                );
                mark_triage_failed(
                    paths,
                    &state_root,
                    &mut state,
                    "executor flagged SpecNeedsRevision during triage".to_string(),
                    chatops_ctx,
                )
                .await;
            }
            Ok(crate::executor::ExecutorOutcome::IterationRequested { .. }) => {
                tracing::warn!(
                    url = %repo.url,
                    thread_ts = %thread_ts,
                    "audit-triage: executor returned IterationRequested; treating as failure (iteration sequences not applicable to triage mode)"
                );
                mark_triage_failed(
                    paths,
                    &state_root,
                    &mut state,
                    "executor returned IterationRequested during triage".to_string(),
                    chatops_ctx,
                )
                .await;
            }
            Ok(crate::executor::ExecutorOutcome::Aborted { reason }) => {
                // a39: subprocess killed by the daemon's own SIGTERM
                // cascade. Leave state at TriagePending so the next
                // iteration after restart retries the triage; do NOT
                // mark_triage_failed (operator initiated the shutdown).
                tracing::info!(
                    url = %repo.url,
                    thread_ts = %thread_ts,
                    "audit-triage: executor aborted by daemon shutdown: {reason}"
                );
            }
            Err(e) => {
                tracing::error!(
                    url = %repo.url,
                    thread_ts = %thread_ts,
                    "audit-triage: executor task errored: {e:#}"
                );
                mark_triage_failed(
                    paths,
                    &state_root,
                    &mut state,
                    format!("executor task error: {e:#}"),
                    chatops_ctx,
                )
                .await;
            }
        }
        // After triage (success or failure), reset to clean working tree
        // so the next operation isn't contaminated by triage leftovers.
        // best-effort — failures are logged but never propagated.
        if let Err(e) = git::reset_hard_head(workspace) {
            tracing::warn!(
                url = %repo.url,
                "audit-triage: post-triage reset_hard_head failed: {e:#}"
            );
        }
        let _ = git::clean_force(workspace);
        // Move back to base branch so subsequent steps in the iteration
        // start from a known state.
        let _ = git::checkout(workspace, &repo.base_branch);
    }
    Ok(())
}

/// Inspect the changed paths in `workspace` after a Completed triage and
/// open AT MOST ONE PR — the spec PR (a43). Code-path writes outside
/// `openspec/changes/<derived-slug>/` are discarded before the commit so
/// the spec PR's diff is genuinely spec-only; the dropped paths are
/// logged AND surfaced to chatops. On the empty-diff path, post the
/// agent's final-summary text into the audit thread reply chain and flip
/// the state to `Acted`. `final_summary` carries the executor's
/// final-answer text (used for the empty-diff reply).
async fn process_completed_triage(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    state: &mut crate::audits::threads::AuditThreadState,
    final_summary: Option<&str>,
) -> Result<()> {
    use crate::audits::threads::{self, AuditThreadStatus};
    let state_root = threads::default_state_root(paths);

    let changed: Vec<String> = triage_status_entries(workspace)
        .with_context(|| "audit-triage: reading post-Completed git status".to_string())?
        .into_iter()
        .map(|(_, p)| p)
        .collect();

    // A stable slug derived from `<audit_type>-<short_hash>`, retained
    // purely as a diagnostic label for logs (the executor picks its own
    // change-directory name; the spec/code boundary is the universal
    // `openspec/changes/` root, NOT this slug).
    let new_slug = derive_unique_triage_slug(workspace, &state.audit_type, &state.findings_excerpt);

    // Brightline-specific diff-scope validation: the `Mark as
    // intentional` triage output writes ONLY `.brightline-ignore`. The
    // overall brightline-triage diff must therefore be limited to
    // `.brightline-ignore` plus `openspec/changes/`. A diff touching
    // arbitrary code AND `.brightline-ignore` indicates a confused LLM
    // run; we refuse rather than ship a half-valid PR.
    if let Err(violations) =
        validate_brightline_triage_scope(&state.audit_type, &changed, "openspec/changes/")
    {
        tracing::warn!(
            thread_ts = %state.thread_ts,
            "audit-triage: brightline diff scope violation; rejecting. Out-of-scope paths: {violations:?}"
        );
        if let Some(ctx) = chatops_ctx {
            let body = format!(
                "✗ Triage for `{audit_type}` on `{repo_url}` rejected: out-of-scope diff. \
                Brightline triages may only write `.brightline-ignore` or `openspec/changes/<slug>/`. \
                Offending paths:\n{violations}",
                audit_type = state.audit_type,
                repo_url = state.repo_url,
                violations = violations.join("\n"),
            );
            let _ = ctx
                .chatops
                .post_threaded_reply(&state.channel, &state.thread_ts, &body)
                .await;
        }
        state.status = AuditThreadStatus::TriageFailed;
        let _ = threads::write_state(&state_root, state);
        return Ok(());
    }

    // a43: triage produces a SPEC-ONLY PR. Code-path writes outside
    // `openspec/changes/<slug>/` are discarded before commit;
    // implementation flows through the standard implementer pipeline on a
    // later iteration after the operator merges the spec PR.
    let push_remote = if github_cfg.fork_owner.is_some() {
        "fork"
    } else {
        "origin"
    };
    let agent_branch = &repo.agent_branch;
    let base_branch = &repo.base_branch;

    // Brightline `Mark as intentional` is the one exception to the
    // spec-only rule: its sole deliverable is the `.brightline-ignore`
    // suppression file, which has no implementer-pipeline equivalent, so
    // ship it directly the way the pre-a43 single-PR path did.
    // `validate_brightline_triage_scope` (run above) already guarantees a
    // brightline diff carrying `.brightline-ignore` contains nothing but
    // that file plus `openspec/changes/<slug>/`, so a straight commit is
    // safe.
    let brightline_intentional = state.audit_type == "architecture_brightline"
        && changed.iter().any(|p| p == ".brightline-ignore");
    if brightline_intentional {
        git::checkout(workspace, base_branch)
            .with_context(|| format!("audit-triage: checkout base branch `{base_branch}`"))?;
        let branch = format!("{agent_branch}-triage-spec");
        git::recreate_branch(workspace, &branch)
            .with_context(|| format!("audit-triage: recreate `{branch}`"))?;
        git::add_all(workspace)
            .with_context(|| "audit-triage: staging brightline-intentional diff".to_string())?;
        let subject = format!("audit-triage intentional-marks from {}", state.audit_type);
        git::commit(workspace, &subject)
            .with_context(|| "audit-triage: commit brightline-intentional branch".to_string())?;
        if let Err(e) = git::push_force_with_lease(workspace, &branch, push_remote) {
            return Err(anyhow!(
                "audit-triage: pushing brightline-intentional branch failed: {e:#}"
            ));
        }
        let body = format!(
            "This PR marks brightline duplicate-signature findings from the `{audit_type}` audit on `{repo_url}` as intentional by adding `.brightline-ignore` entries. No code changes are included.",
            audit_type = state.audit_type,
            repo_url = state.repo_url,
        );
        let pr_url = match open_triage_pull_request(
            paths,
            repo,
            github_cfg,
            &branch,
            base_branch,
            &format!("audit-triage intentional-marks ({})", state.audit_type),
            &body,
        )
        .await
        {
            Ok(url) => Some(url),
            Err(e) => {
                tracing::error!(
                    url = %repo.url,
                    "audit-triage: brightline-intentional PR creation failed: {e:#}"
                );
                None
            }
        };
        if let Some(ctx) = chatops_ctx {
            let mut reply = format!("✓ Triage for `{}` complete.", state.audit_type);
            if let Some(u) = &pr_url {
                reply.push_str(&format!("\nPR: {u}"));
            }
            let _ = ctx
                .chatops
                .post_threaded_reply(&state.channel, &state.thread_ts, &reply)
                .await;
        }
        state.status = AuditThreadStatus::Acted;
        let _ = threads::write_state(&state_root, state);
        return Ok(());
    }

    // --- Generic a43 spec-only path ---
    let was_empty = changed.is_empty();
    let has_spec = changed.iter().any(|p| p.starts_with("openspec/changes/"));

    // Discard every non-spec write so the spec PR's diff is spec-only.
    let discarded = discard_non_spec_writes(workspace, &new_slug)
        .with_context(|| "audit-triage: discarding non-spec writes".to_string())?;
    if !discarded.is_empty() {
        tracing::warn!(
            url = %repo.url,
            audit_type = %state.audit_type,
            slug = %new_slug,
            dropped = ?discarded,
            "audit-triage: discarded non-spec writes (a43 spec-only enforcement)"
        );
    }

    if !has_spec {
        // No spec content survived the discard. Distinguish "nothing was
        // produced" (empty diff → Acted) from "only code, now dropped"
        // (code-only → TriageFailed, retryable).
        if let Some(ctx) = chatops_ctx {
            let body = if was_empty {
                match final_summary.map(str::trim).filter(|s| !s.is_empty()) {
                    Some(summary) => format!(
                        "ℹ️ Triage for `{at}` on `{ru}` completed with no actionable changes.\n\n{summary}",
                        at = state.audit_type,
                        ru = state.repo_url,
                    ),
                    None => format!(
                        "ℹ️ Triage for `{at}` on `{ru}` completed with no actionable changes.",
                        at = state.audit_type,
                        ru = state.repo_url,
                    ),
                }
            } else {
                format!(
                    "ℹ️ Triage for `{at}` on `{ru}` produced no spec content; retry with a clearer directive.",
                    at = state.audit_type,
                    ru = state.repo_url,
                )
            };
            if let Err(e) = ctx
                .chatops
                .post_threaded_reply(&state.channel, &state.thread_ts, &body)
                .await
            {
                tracing::warn!(
                    thread_ts = %state.thread_ts,
                    "audit-triage: no-PR thread reply failed: {e:#}"
                );
            }
        }
        state.status = if was_empty {
            AuditThreadStatus::Acted
        } else {
            AuditThreadStatus::TriageFailed
        };
        let _ = threads::write_state(&state_root, state);
        return Ok(());
    }

    // Spec content exists → open exactly one PR (the spec PR). If the
    // agent also wrote code (now discarded), warn the operator so the
    // dropped fixes can be captured as tasks.md items if load-bearing.
    if !discarded.is_empty()
        && let Some(ctx) = chatops_ctx
    {
        let body = format!(
            "⚠️ The triage agent attempted to write {n} path(s) outside `openspec/changes/`: {list}. \
            Per a43, code fixes go through the standard implementer pipeline. The spec PR has been opened; \
            if the dropped fixes were load-bearing, revise the spec to capture them as tasks.md items.",
            n = discarded.len(),
            list = discarded.join(", "),
        );
        if let Err(e) = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.thread_ts, &body)
            .await
        {
            tracing::warn!(
                thread_ts = %state.thread_ts,
                "audit-triage: dropped-paths thread reply failed: {e:#}"
            );
        }
    }

    git::checkout(workspace, base_branch)
        .with_context(|| format!("audit-triage: checkout base branch `{base_branch}`"))?;
    let spec_branch = format!("{agent_branch}-triage-spec");
    git::recreate_branch(workspace, &spec_branch)
        .with_context(|| format!("audit-triage: recreate `{spec_branch}`"))?;
    git::add_all(workspace)
        .with_context(|| "audit-triage: staging spec paths".to_string())?;
    let subject = format!("audit-triage spec proposal from {}", state.audit_type);
    git::commit(workspace, &subject)
        .with_context(|| "audit-triage: commit spec branch".to_string())?;
    if let Err(e) = git::push_force_with_lease(workspace, &spec_branch, push_remote) {
        return Err(anyhow!("audit-triage: pushing spec branch failed: {e:#}"));
    }
    let body = format!(
        "This PR carries the new spec change(s) from the `{at}` audit on `{ru}`. \
        After merge, the next polling iteration's implementer will produce the code fixes through the standard pipeline.",
        at = state.audit_type,
        ru = state.repo_url,
    );
    let spec_pr_url = match open_triage_pull_request(
        paths,
        repo,
        github_cfg,
        &spec_branch,
        base_branch,
        &format!("audit-triage spec ({})", state.audit_type),
        &body,
    )
    .await
    {
        Ok(url) => Some(url),
        Err(e) => {
            tracing::error!(url = %repo.url, "audit-triage: spec PR creation failed: {e:#}");
            None
        }
    };

    if let Some(ctx) = chatops_ctx
        && let Some(u) = &spec_pr_url
    {
        let reply = format!("✓ Triage for `{}` complete.\nSpec PR: {u}", state.audit_type);
        let _ = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.thread_ts, &reply)
            .await;
    }

    state.status = AuditThreadStatus::Acted;
    let _ = threads::write_state(&state_root, state);
    Ok(())
}

/// Derive a unique `openspec/changes/<slug>/` path for a triage run.
/// The slug is `<audit_type-sanitized>-<short-hash>`; if it already
/// exists on disk, we append `-2`, `-3`, ... until we find a free path.
fn derive_unique_triage_slug(workspace: &Path, audit_type: &str, findings: &str) -> String {
    let mut sanitized: String = audit_type
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    sanitized = sanitized.trim_matches('-').to_string();
    if sanitized.is_empty() {
        sanitized = "triage".to_string();
    }
    // Short hash: first 8 hex chars of a non-crypto fold over the
    // findings string. Deterministic per identical findings, so re-running
    // the same `send it` reuses the same slug instead of forking a new one.
    let hash = short_findings_hash(findings);
    let base_slug = format!("{sanitized}-{hash}");
    let mut slug = base_slug.clone();
    let mut suffix = 2u32;
    while workspace.join("openspec/changes").join(&slug).exists() {
        slug = format!("{base_slug}-{suffix}");
        suffix += 1;
        if suffix > 100 {
            // Pathological case: bail out with whatever we have.
            break;
        }
    }
    slug
}

/// 8-hex-char fold over `findings`. Not cryptographic — only used as a
/// slug uniqueness suffix.
fn short_findings_hash(findings: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for b in findings.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3); // FNV prime
    }
    format!("{:08x}", h as u32)
}

/// Diff-scope check applied to `architecture_brightline` triage diffs.
/// The brightline `send it` LLM emits one of three output shapes per
/// finding:
///
/// 1. **Fix** — touches arbitrary source files.
/// 2. **Spec-worthy** — touches files under `openspec/changes/<slug>/`.
/// 3. **Mark as intentional** — touches ONLY `.brightline-ignore`.
///
/// Per the spec, a brightline triage diff is permitted to touch
/// `.brightline-ignore` and/or `openspec/changes/<slug>/` — but if
/// `.brightline-ignore` writes mix with arbitrary code edits, the run
/// is confused and we refuse to ship it (the caller posts a chatops
/// rejection and flips state to `TriageFailed`).
///
/// For non-brightline audits this function is a no-op: every other
/// audit's triage diff is unconstrained beyond the spec/fixes
/// partition that happens downstream.
///
/// Returns `Ok(())` when the diff passes. Returns `Err(violations)`
/// listing the offending paths when it fails.
fn validate_brightline_triage_scope(
    audit_type: &str,
    changed: &[String],
    slug_prefix: &str,
) -> Result<(), Vec<String>> {
    if audit_type != "architecture_brightline" {
        return Ok(());
    }
    if !changed.iter().any(|p| p == ".brightline-ignore") {
        // No `.brightline-ignore` write in this diff → the brightline
        // triage took the fix/spec path, which is unconstrained.
        return Ok(());
    }
    let violations: Vec<String> = changed
        .iter()
        .filter(|p| p.as_str() != ".brightline-ignore" && !p.starts_with(slug_prefix))
        .cloned()
        .collect();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Pull the path out of a `git status --porcelain` line. Lines look like
/// `XY <path>`; for renames the format is `R  <from> -> <to>` and we
/// keep the trailing target.
fn extract_porcelain_path(line: &str) -> Option<&str> {
    let trimmed = line.get(3..)?.trim_start();
    let path = if let Some(idx) = trimmed.rfind(" -> ") {
        trimmed[idx + 4..].trim()
    } else {
        trimmed.trim()
    };
    if path.is_empty() { None } else { Some(path) }
}

/// a43: discard every working-tree change OUTSIDE the OpenSpec change
/// root (`openspec/changes/`), reverting each non-spec path to its
/// committed (HEAD) state so the spec-PR commit is genuinely spec-only.
/// Returns the sorted, de-duplicated list of discarded paths so the caller
/// can log them AND surface them to chatops.
///
/// Revert strategy per non-spec path, chosen by where the path lives:
///   - **Untracked addition** (`??`): removed from disk — it has no HEAD
///     blob to restore.
///   - **Tracked path present in HEAD** (a modification, deletion,
///     type-change, OR the SOURCE side of a rename): reverted with `git
///     checkout HEAD -- <path>`, which rewrites BOTH the index AND the
///     worktree to the committed content — so a code edit the executor
///     staged with `git add` cannot survive into the spec commit.
///   - **Tracked path absent from HEAD** (a brand-new file the executor
///     created AND `git add`ed — porcelain `A ` — OR the DESTINATION of a
///     rename): unstaged with `git reset HEAD -- <path>` (which demotes it
///     to untracked) AND then removed from disk. This case is handled
///     WITHOUT `git checkout HEAD -- <path>` / `git restore --source=HEAD`
///     on purpose: those reject a pathspec absent from HEAD with a
///     "pathspec did not match any file(s) known to git" error on some git
///     versions, which would abort the whole triage flow exactly when the
///     executor `git add`ed a new code file — the common case.
///
/// Triage executor runs are spec-only under a43: any code-path write the
/// agent made despite the prompt restriction is dropped here BEFORE the
/// spec-PR commit so the PR diff is genuinely spec-only. Spec content is
/// kept regardless of the executor's chosen slug — the keep boundary is
/// the change root rather than a single `openspec/changes/<slug>/` path,
/// because the executor picks its own (LLM-chosen) slug AND a single
/// triage may produce several change directories. `spec_slug` is the
/// handler's derived slug, threaded for diagnostic logging. A clean
/// working tree (and a spec-only diff) both return an empty list with no
/// side effects.
pub fn discard_non_spec_writes(workspace: &Path, spec_slug: &str) -> Result<Vec<String>> {
    const KEEP_PREFIX: &str = "openspec/changes/";
    tracing::debug!(spec_slug = %spec_slug, "discard_non_spec_writes: keeping openspec/changes/ content");
    let mut discarded: Vec<String> = Vec::new();
    for (is_untracked, path) in triage_status_entries(workspace)
        .with_context(|| "discard_non_spec_writes: reading git status".to_string())?
    {
        if path.starts_with(KEEP_PREFIX) {
            continue;
        }
        if is_untracked {
            // Untracked addition: no HEAD blob to restore, so remove it
            // from disk.
            remove_non_spec_path_from_disk(workspace, &path, spec_slug)?;
        } else if path_exists_in_head(workspace, &path)? {
            // Tracked change to a path that EXISTS in HEAD (modification,
            // deletion, type-change, OR a rename source). `git checkout
            // HEAD -- <path>` rewrites BOTH the index AND the worktree to
            // the committed content — so a code edit the executor staged
            // with `git add` cannot survive into the spec commit.
            run_git_revert(
                workspace,
                &["checkout", "-q", "HEAD", "--", path.as_str()],
                &path,
                spec_slug,
            )?;
        } else {
            // Tracked change to a path ABSENT from HEAD: a brand-new file
            // the executor `git add`ed (porcelain `A `) OR a rename
            // destination. `git checkout HEAD -- <path>` / `git restore
            // --source=HEAD` reject a not-in-HEAD pathspec on some git
            // versions, so unstage it (`git reset HEAD -- <path>` demotes
            // it to untracked) AND remove it from disk.
            run_git_revert(
                workspace,
                &["reset", "-q", "HEAD", "--", path.as_str()],
                &path,
                spec_slug,
            )?;
            remove_non_spec_path_from_disk(workspace, &path, spec_slug)?;
        }
        discarded.push(path);
    }
    discarded.sort();
    discarded.dedup();
    Ok(discarded)
}

/// Remove a non-spec working-tree path from disk (symlink-safe), treating
/// an already-absent path as success. Shared by the untracked-addition
/// branch AND the not-in-HEAD tracked branch (a staged add OR rename
/// destination that `git reset` has just demoted to untracked) of
/// `discard_non_spec_writes`. Any failure other than "already gone" is
/// fatal: a surviving file would be swept into the spec-only commit by the
/// caller's subsequent `git add -A`, silently violating the spec-only
/// invariant, so we fail loudly rather than let the write leak.
fn remove_non_spec_path_from_disk(workspace: &Path, path: &str, spec_slug: &str) -> Result<()> {
    let abs = workspace.join(path);
    // Decide dir-vs-file from the path's OWN metadata (lstat), not
    // `is_dir()` which follows symlinks: git reports an untracked symlink
    // as a single entry, AND following it into `remove_dir_all` could
    // delete the link TARGET's contents. `symlink_metadata` reports a
    // symlink as a non-dir, so it is unlinked via `remove_file` (dropping
    // just the link). A real untracked directory still routes to
    // `remove_dir_all`.
    let is_real_dir = abs
        .symlink_metadata()
        .map(|m| m.is_dir())
        .unwrap_or(false);
    let removal = if is_real_dir {
        std::fs::remove_dir_all(&abs)
    } else {
        std::fs::remove_file(&abs)
    };
    if let Err(e) = removal
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            spec_slug = %spec_slug,
            path = %path,
            "discard_non_spec_writes: failed to remove non-spec write: {e:#}"
        );
        return Err(anyhow!(
            "discard_non_spec_writes: failed to remove non-spec write `{path}`: {e}; \
             refusing to proceed so it does not leak into the spec-only PR"
        ));
    }
    Ok(())
}

/// Whether `path` exists as a blob in HEAD — i.e. `git cat-file -e
/// HEAD:<path>` succeeds. Picks the revert strategy for a tracked non-spec
/// change in `discard_non_spec_writes`: a path IN HEAD is reverted with
/// `git checkout HEAD -- <path>`; a path NOT in HEAD (a staged add OR a
/// rename destination) is unstaged AND deleted instead. A failure to spawn
/// git propagates; a clean non-zero exit (the blob is absent, with git's
/// diagnostic captured rather than spilled to stderr) is reported as
/// `false`.
fn path_exists_in_head(workspace: &Path, path: &str) -> Result<bool> {
    let out = std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("HEAD:{path}")])
        .current_dir(workspace)
        .output()
        .with_context(|| {
            format!("discard_non_spec_writes: spawning git cat-file for `{path}`")
        })?;
    Ok(out.status.success())
}

/// Run a git working-tree revert subprocess (`checkout`/`reset`) for
/// `discard_non_spec_writes`, capturing diagnostics via `output()` (rather
/// than letting `status()` spill git's stderr to the daemon's inherited
/// stderr) AND surfacing them in the error so a failed revert is
/// debuggable (mirrors the `git::run_git` contract). A non-zero exit is
/// fatal: we refuse to proceed so the non-spec write cannot leak into the
/// spec-only PR.
fn run_git_revert(workspace: &Path, args: &[&str], path: &str, spec_slug: &str) -> Result<()> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .with_context(|| {
            format!(
                "discard_non_spec_writes: spawning `git {}` for `{path}`",
                args.join(" ")
            )
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let diag = match (stderr.is_empty(), stdout.is_empty()) {
            (false, _) => stderr,
            (true, false) => stdout,
            (true, true) => format!("(no output; exit {:?})", out.status.code()),
        };
        tracing::warn!(
            spec_slug = %spec_slug,
            path = %path,
            "discard_non_spec_writes: `git {}` exited non-zero reverting non-spec write: {diag}",
            args.join(" ")
        );
        return Err(anyhow!(
            "discard_non_spec_writes: `git {}` exited non-zero ({diag}); \
             refusing to proceed so the non-spec write does not leak into the spec-only PR",
            args.join(" ")
        ));
    }
    Ok(())
}

/// Robustly enumerate the working tree's changed paths via `git status
/// --porcelain=v1 -z --untracked-files=all`, returning `(is_untracked,
/// path)` per entry.
///
/// `-z` is load-bearing. The default porcelain format wraps any path
/// containing "unusual" bytes — non-ASCII, a space, control chars — in
/// double quotes AND C-escapes it (`core.quotePath`, on by default), so a
/// file like `föö.rs` renders as `"f\303\266\303\266.rs"`. Parsing that
/// quoted literal would (a) miss the `openspec/changes/` keep-prefix on a
/// quoted spec path — silently DISCARDING real spec content — AND (b)
/// hand a path matching nothing on disk to `git restore` / `remove_file`,
/// leaving the actual file in place to be swept into the supposedly
/// spec-only commit. `-z` emits NUL-terminated records with NO quoting or
/// escaping, sidestepping both failures. (It also avoids the
/// `git::status_porcelain*` whole-string `.trim()` that would corrupt a
/// leading ` M <path>`.)
///
/// Each record is `XY <path>`: two status chars, a separator space, then
/// the raw path bytes. A rename/copy record carries a SECOND
/// NUL-terminated field — under `-z` the destination is emitted first AND
/// the source second — so the source field is consumed (and yielded) to
/// keep the record stream aligned; both sides of a staged rename are
/// changes relative to HEAD that must be reverted.
fn triage_status_entries(workspace: &Path) -> Result<Vec<(bool, String)>> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(workspace)
        .output()
        .with_context(|| "git status --porcelain=v1 -z failed to spawn".to_string())?;
    if !output.status.success() {
        return Err(anyhow!(
            "git status --porcelain=v1 -z exited non-zero: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut records = output.stdout.split(|&b| b == 0u8);
    let mut entries = Vec::new();
    while let Some(record) = records.next() {
        // A valid record is `XY <path>` (>= 4 bytes: 2 status + space +
        // >= 1 path byte). The split's trailing element after the final
        // NUL is empty; skip it AND any stray short record. Do NOT trim
        // the path — under `-z` the bytes are exact, so a filename with a
        // leading/trailing space must survive verbatim.
        if record.len() < 4 {
            continue;
        }
        let is_untracked = record.starts_with(b"??");
        // `R` (rename) or `C` (copy) in either status column means a
        // second field (the source path) trails this record.
        let is_rename_or_copy =
            matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C');
        let path = String::from_utf8_lossy(&record[3..]).into_owned();
        if !path.is_empty() {
            entries.push((is_untracked, path));
        }
        if is_rename_or_copy
            && let Some(source) = records.next()
        {
            let source = String::from_utf8_lossy(source).into_owned();
            if !source.is_empty() {
                // A staged rename's source is a tracked deletion at HEAD;
                // revert it alongside the destination.
                entries.push((false, source));
            }
        }
    }
    Ok(entries)
}

/// Read the workspace's `openspec/specs/` directory and produce a brief
/// listing of the canonical spec names available. Used by the triage
/// prompt's `{{canonical_specs_index}}` substitution.
fn build_canonical_specs_index(workspace: &Path) -> String {
    let specs_dir = workspace.join("openspec/specs");
    if !specs_dir.is_dir() {
        return "(no openspec/specs/ directory found)".to_string();
    }
    let mut names: Vec<String> = match std::fs::read_dir(&specs_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => return "(error reading openspec/specs/)".to_string(),
    };
    names.sort();
    if names.is_empty() {
        return "(no specs in openspec/specs/)".to_string();
    }
    names
        .iter()
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Flip the audit-thread state to `TriageFailed` and post the failure
/// to the audit thread. Best-effort — every failure path here logs and
/// continues so the surrounding iteration is unaffected.
async fn mark_triage_failed(
    _paths: &DaemonPaths,
    state_root: &Path,
    state: &mut crate::audits::threads::AuditThreadState,
    reason: String,
    chatops_ctx: Option<&ChatOpsContext>,
) {
    use crate::audits::threads::{self, AuditThreadStatus};
    state.status = AuditThreadStatus::TriageFailed;
    state.reason = Some(reason.clone());
    if let Err(e) = threads::write_state(state_root, state) {
        tracing::warn!(
            thread_ts = %state.thread_ts,
            "audit-triage: failed to record TriageFailed state: {e:#}"
        );
    }
    if let Some(ctx) = chatops_ctx {
        let body = format!(
            "✗ Triage for `{audit_type}` on `{repo_url}` failed: {reason}\n\nReply `@<bot> send it` to retry, or revise the audit and re-run.",
            audit_type = state.audit_type,
            repo_url = state.repo_url,
        );
        if let Err(e) = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.thread_ts, &body)
            .await
        {
            tracing::warn!(
                thread_ts = %state.thread_ts,
                "audit-triage: TriageFailed thread reply failed: {e:#}"
            );
        }
    }
}

/// Open the audit-triage / chat-triage spec PR. Mirrors the shape of
/// `polling_loop::open_pull_request` but is purpose-built for the
/// spec-only triage flow (no reviewer step, no change-list body). Routes
/// through `create_pull_request_via_hook` so tests can assert against a
/// mockito server.
async fn open_triage_pull_request(
    _paths: &DaemonPaths,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    head_branch: &str,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let (owner, name) = github::parse_repo_url(&repo.url)
        .with_context(|| "audit-triage: parsing repo URL".to_string())?;
    let token = crate::github_credentials::resolve_token(github_cfg, &owner)?;
    let head = if let Some(fork_owner) = github_cfg.fork_owner.as_deref() {
        format!("{fork_owner}:{head_branch}")
    } else {
        head_branch.to_string()
    };
    let pr = create_pull_request_via_hook(
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

/// Drain handler for chat-driven proposal requests. The polling loop's
/// `run` calls this once per iteration with the per-iteration drained
/// queue snapshot. Each entry loads its `ProposalRequestState`, runs
/// the chat-triage executor, and routes the outcome through:
///   - QUESTION → post `.chat-reply.md` contents to the lifecycle
///     thread, set status to `Discussed`.
///   - DIRECTIVE → discard non-spec writes and open at most one spec PR
///     (a43; reusing the same helper that powers `audit-reply-acts`),
///     set status to `Acted`.
///   - AskUser → leave status at `TriagePending` (existing chatops
///     escalation posts the question into the lifecycle thread).
///   - Failed → post a failure reply, set status to `TriageFailed`.
///
/// Failures inside one entry do NOT abort the others — each is processed
/// independently.
pub async fn process_proposal_requests(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    requests: &[crate::control_socket::ProposalRequest],
) -> Result<()> {
    // Workspace preparation mirrors the audit-triage path: ensure clean
    // base branch checkout, recreate the agent branch, so the chat-triage
    // executor sees a known state. The downstream pass-through uses the
    // same convention; we duplicate it here because chat-triage runs
    // OUTSIDE the normal pass.
    let fork_url = match github_cfg.fork_owner.as_deref() {
        Some(owner) => Some(crate::github::derive_fork_url(&repo.url, owner)?),
        None => None,
    };
    let fork_arg = fork_url.as_deref().map(|u| (u, repo.agent_branch.as_str()));
    crate::workspace::ensure_initialized(paths, workspace, &repo.url, fork_arg)
        .with_context(|| "chat-triage: workspace ensure_initialized".to_string())?;
    let _ = crate::queue::clear_stale_locks(workspace);
    let _ = git::reset_hard_head(workspace);
    let _ = git::clean_force(workspace);
    git::fetch(workspace).with_context(|| "chat-triage: git fetch".to_string())?;
    git::checkout(workspace, &repo.base_branch)
        .with_context(|| format!("chat-triage: checkout `{}`", repo.base_branch))?;
    git::pull_ff_only(workspace, &repo.base_branch)
        .with_context(|| format!("chat-triage: pull --ff-only `{}`", repo.base_branch))?;
    git::recreate_branch(workspace, &repo.agent_branch)
        .with_context(|| format!("chat-triage: recreate `{}`", repo.agent_branch))?;

    let state_root = crate::proposal_requests::default_state_root(paths);
    for request in requests {
        let mut state = match crate::proposal_requests::read_state(
            &state_root,
            &repo.url,
            &request.request_id,
        ) {
            Ok(Some(s)) => s,
            Ok(None) => {
                tracing::warn!(
                    request_id = %request.request_id,
                    "chat-triage: no state file (entry pruned between enqueue and processing); skipping"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request.request_id,
                    "chat-triage: state read failed: {e:#}"
                );
                continue;
            }
        };

        // Flip Pending → TriagePending up front so a daemon crash mid-
        // run is observable on disk.
        state.status = crate::proposal_requests::ProposalRequestStatus::TriagePending;
        let _ = crate::proposal_requests::write_state(&state_root, &state);

        let canonical_specs_index = build_canonical_specs_index(workspace);
        let ctx = crate::executor::ChatTriageContext {
            request_text: state.request_text.clone(),
            repo_url: state.repo_url.clone(),
            canonical_specs_index,
        };

        tracing::info!(
            url = %repo.url,
            request_id = %state.request_id,
            "chat-triage: invoking executor"
        );
        let outcome = executor.run_chat_triage(workspace, &ctx).await;
        match outcome {
            Ok(crate::executor::ExecutorOutcome::Completed { final_answer }) => {
                if let Err(e) = process_completed_proposal(
                    paths,
                    workspace,
                    repo,
                    github_cfg,
                    chatops_ctx,
                    &mut state,
                    final_answer.as_deref(),
                )
                .await
                {
                    tracing::error!(
                        url = %repo.url,
                        request_id = %state.request_id,
                        "chat-triage: post-Completed processing failed: {e:#}"
                    );
                    mark_proposal_failed(
                        paths,
                        &state_root,
                        &mut state,
                        format!("post-Completed processing: {e:#}"),
                        chatops_ctx,
                    )
                    .await;
                }
            }
            Ok(crate::executor::ExecutorOutcome::Failed { reason }) => {
                tracing::error!(
                    url = %repo.url,
                    request_id = %state.request_id,
                    "chat-triage: executor returned Failed: {reason}"
                );
                mark_proposal_failed(paths, &state_root, &mut state, reason, chatops_ctx).await;
            }
            Ok(crate::executor::ExecutorOutcome::AskUser { .. }) => {
                tracing::info!(
                    url = %repo.url,
                    request_id = %state.request_id,
                    "chat-triage: executor returned AskUser; leaving status TriagePending"
                );
            }
            Ok(crate::executor::ExecutorOutcome::SpecNeedsRevision { .. }) => {
                tracing::warn!(
                    url = %repo.url,
                    request_id = %state.request_id,
                    "chat-triage: executor returned SpecNeedsRevision; treating as failure"
                );
                mark_proposal_failed(
                    paths,
                    &state_root,
                    &mut state,
                    "executor flagged SpecNeedsRevision during chat-triage".to_string(),
                    chatops_ctx,
                )
                .await;
            }
            Ok(crate::executor::ExecutorOutcome::IterationRequested { .. }) => {
                tracing::warn!(
                    url = %repo.url,
                    request_id = %state.request_id,
                    "chat-triage: executor returned IterationRequested; treating as failure (iteration sequences not applicable to chat-triage mode)"
                );
                mark_proposal_failed(
                    paths,
                    &state_root,
                    &mut state,
                    "executor returned IterationRequested during chat-triage".to_string(),
                    chatops_ctx,
                )
                .await;
            }
            Ok(crate::executor::ExecutorOutcome::Aborted { reason }) => {
                // a39: subprocess killed by the daemon's own SIGTERM
                // cascade. Leave state at TriagePending so the next
                // iteration after restart retries; do NOT
                // mark_proposal_failed (operator initiated the
                // shutdown).
                tracing::info!(
                    url = %repo.url,
                    request_id = %state.request_id,
                    "chat-triage: executor aborted by daemon shutdown: {reason}"
                );
            }
            Err(e) => {
                tracing::error!(
                    url = %repo.url,
                    request_id = %state.request_id,
                    "chat-triage: executor task errored: {e:#}"
                );
                mark_proposal_failed(
                    paths,
                    &state_root,
                    &mut state,
                    format!("executor task error: {e:#}"),
                    chatops_ctx,
                )
                .await;
            }
        }
        // Always reset to clean working tree so the next operation isn't
        // contaminated by leftovers. Best-effort — failures are logged.
        if let Err(e) = git::reset_hard_head(workspace) {
            tracing::warn!(
                url = %repo.url,
                "chat-triage: post-run reset_hard_head failed: {e:#}"
            );
        }
        let _ = git::clean_force(workspace);
        let _ = git::checkout(workspace, &repo.base_branch);
    }
    Ok(())
}

/// Handle a `Completed` chat-triage outcome. Checks for the
/// `.chat-reply.md` marker FIRST; if present, posts the contents to the
/// lifecycle thread and flips to `Discussed`. Otherwise discards non-spec
/// writes and opens AT MOST ONE PR — the spec PR (a43) — identical in
/// shape to the audit-triage handler. `final_summary` carries the
/// executor's final-answer text (used for the empty-diff reply).
async fn process_completed_proposal(
    paths: &DaemonPaths,
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    state: &mut crate::proposal_requests::ProposalRequestState,
    final_summary: Option<&str>,
) -> Result<()> {
    use crate::proposal_requests::{self, ProposalRequestStatus};
    let state_root = proposal_requests::default_state_root(paths);

    // 1. Marker-file check first. The `.chat-reply.md` file at the
    //    workspace root indicates the LLM classified as QUESTION.
    let chat_reply_path = workspace.join(".chat-reply.md");
    if chat_reply_path.exists() {
        let contents = match std::fs::read_to_string(&chat_reply_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %chat_reply_path.display(),
                    "chat-triage: reading .chat-reply.md failed: {e}; treating as empty"
                );
                String::new()
            }
        };
        // Non-empty? Treat as a QUESTION outcome.
        if !contents.trim().is_empty() {
            let truncated = proposal_requests::truncate_chat_reply_with_pointer(
                &contents,
                &state.request_id,
                proposal_requests::CHAT_REPLY_BODY_CAP,
            );
            if let Some(ctx) = chatops_ctx
                && let Err(e) = ctx
                    .chatops
                    .post_threaded_reply(&state.channel, &state.thread_ts, &truncated)
                    .await
            {
                tracing::warn!(
                    request_id = %state.request_id,
                    "chat-triage: posting Discussed reply failed: {e:#}"
                );
            }
            // Best-effort: delete the marker.
            if let Err(e) = std::fs::remove_file(&chat_reply_path) {
                tracing::warn!(
                    path = %chat_reply_path.display(),
                    "chat-triage: removing .chat-reply.md failed: {e}"
                );
            }
            // Detect any OTHER modifications and WARN + revert.
            let porcelain = git::status_porcelain(workspace)
                .unwrap_or_default();
            let unexpected: Vec<String> = porcelain
                .lines()
                .filter_map(|l| extract_porcelain_path(l).map(|p| p.to_string()))
                .filter(|p| !p.is_empty() && p != ".chat-reply.md")
                .collect();
            if !unexpected.is_empty() {
                tracing::warn!(
                    request_id = %state.request_id,
                    "chat-triage: Discussed-mode run produced unexpected modifications: {unexpected:?} — reverting"
                );
                let _ = git::reset_hard_head(workspace);
                let _ = git::clean_force(workspace);
            }
            state.status = ProposalRequestStatus::Discussed;
            let _ = proposal_requests::write_state(&state_root, state);
            return Ok(());
        }
        // Empty file: treat as "no reply"; fall through to the
        // diff-split path (likely an empty diff too, which posts the
        // no-action reply).
        let _ = std::fs::remove_file(&chat_reply_path);
    }

    // 2. No `.chat-reply.md`. a43: produce a SPEC-ONLY PR. Code-path
    //    writes are discarded before commit; implementation flows through
    //    the standard implementer pipeline on a later iteration after the
    //    operator merges the spec PR. Mirrors `process_completed_triage`.
    let changed: Vec<String> = triage_status_entries(workspace)
        .with_context(|| "chat-triage: reading post-Completed git status".to_string())?
        .into_iter()
        .map(|(_, p)| p)
        .collect();

    // Stable diagnostic label only; the spec/code boundary is the
    // universal `openspec/changes/` root, NOT this slug (the executor
    // picks its own change-directory name).
    let new_slug = derive_unique_chat_request_slug(workspace, &state.request_text);

    let was_empty = changed.is_empty();
    let has_spec = changed.iter().any(|p| p.starts_with("openspec/changes/"));

    let push_remote = if github_cfg.fork_owner.is_some() {
        "fork"
    } else {
        "origin"
    };
    let agent_branch = &repo.agent_branch;
    let base_branch = &repo.base_branch;

    // Discard every non-spec write so the spec PR's diff is spec-only.
    let discarded = discard_non_spec_writes(workspace, &new_slug)
        .with_context(|| "chat-triage: discarding non-spec writes".to_string())?;
    if !discarded.is_empty() {
        tracing::warn!(
            url = %repo.url,
            request_id = %state.request_id,
            slug = %new_slug,
            dropped = ?discarded,
            "chat-triage: discarded non-spec writes (a43 spec-only enforcement)"
        );
    }

    if !has_spec {
        // No spec content survived the discard. Distinguish "nothing was
        // produced" (empty diff → Acted) from "only code, now dropped"
        // (code-only → TriageFailed, retryable).
        if let Some(ctx) = chatops_ctx {
            let body = if was_empty {
                match final_summary.map(str::trim).filter(|s| !s.is_empty()) {
                    Some(summary) => format!(
                        "ℹ️ Chat-triage for `{ru}` completed with no actionable changes.\n\n{summary}",
                        ru = state.repo_url,
                    ),
                    None => format!(
                        "ℹ️ Chat-triage for `{ru}` completed with no actionable changes.",
                        ru = state.repo_url,
                    ),
                }
            } else {
                format!(
                    "ℹ️ Chat-triage for `{ru}` produced no spec content; retry with a clearer directive.",
                    ru = state.repo_url,
                )
            };
            if let Err(e) = ctx
                .chatops
                .post_threaded_reply(&state.channel, &state.thread_ts, &body)
                .await
            {
                tracing::warn!(
                    request_id = %state.request_id,
                    "chat-triage: no-PR thread reply failed: {e:#}"
                );
            }
        }
        state.status = if was_empty {
            ProposalRequestStatus::Acted
        } else {
            ProposalRequestStatus::TriageFailed
        };
        let _ = proposal_requests::write_state(&state_root, state);
        return Ok(());
    }

    // Spec content exists → open exactly one PR (the spec PR). If the
    // agent also wrote code (now discarded), warn the operator so the
    // dropped fixes can be captured as tasks.md items if load-bearing.
    if !discarded.is_empty()
        && let Some(ctx) = chatops_ctx
    {
        let body = format!(
            "⚠️ The triage agent attempted to write {n} path(s) outside `openspec/changes/`: {list}. \
            Per a43, code fixes go through the standard implementer pipeline. The spec PR has been opened; \
            if the dropped fixes were load-bearing, revise the spec to capture them as tasks.md items.",
            n = discarded.len(),
            list = discarded.join(", "),
        );
        if let Err(e) = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.thread_ts, &body)
            .await
        {
            tracing::warn!(
                request_id = %state.request_id,
                "chat-triage: dropped-paths thread reply failed: {e:#}"
            );
        }
    }

    git::checkout(workspace, base_branch)
        .with_context(|| format!("chat-triage: checkout base branch `{base_branch}`"))?;
    let spec_branch = format!("{agent_branch}-chat-spec");
    git::recreate_branch(workspace, &spec_branch)
        .with_context(|| format!("chat-triage: recreate `{spec_branch}`"))?;
    git::add_all(workspace)
        .with_context(|| "chat-triage: staging spec paths".to_string())?;
    let subject = format!("chat-triage spec proposal (request {})", state.request_id);
    git::commit(workspace, &subject)
        .with_context(|| "chat-triage: commit spec branch".to_string())?;
    if let Err(e) = git::push_force_with_lease(workspace, &spec_branch, push_remote) {
        return Err(anyhow!("chat-triage: pushing spec branch failed: {e:#}"));
    }
    let body = format!(
        "This PR carries the new spec change(s) from the `propose` request on `{repo_url}`. \
        After merge, the next polling iteration's implementer will produce the code fixes through the standard pipeline.\n\nOperator's request:\n\n> {request_excerpt}",
        repo_url = state.repo_url,
        request_excerpt = short_request_excerpt(&state.request_text),
    );
    let spec_pr_url = match open_triage_pull_request(
        paths,
        repo,
        github_cfg,
        &spec_branch,
        base_branch,
        &format!("chat-triage spec ({})", short_request_excerpt(&state.request_text)),
        &body,
    )
    .await
    {
        Ok(url) => Some(url),
        Err(e) => {
            tracing::error!(url = %repo.url, "chat-triage: spec PR creation failed: {e:#}");
            None
        }
    };

    if let Some(ctx) = chatops_ctx
        && let Some(u) = &spec_pr_url
    {
        let reply = format!("✓ Chat-triage complete.\nSpec PR: {u}");
        let _ = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.thread_ts, &reply)
            .await;
    }

    state.status = ProposalRequestStatus::Acted;
    let _ = proposal_requests::write_state(&state_root, state);
    Ok(())
}

/// Flip the proposal-request state to `TriageFailed` and post the
/// failure to the request's lifecycle thread. Best-effort — every
/// failure path here logs and continues.
async fn mark_proposal_failed(
    _paths: &DaemonPaths,
    state_root: &Path,
    state: &mut crate::proposal_requests::ProposalRequestState,
    reason: String,
    chatops_ctx: Option<&ChatOpsContext>,
) {
    use crate::proposal_requests::{self, ProposalRequestStatus};
    state.status = ProposalRequestStatus::TriageFailed;
    state.reason = Some(reason.clone());
    if let Err(e) = proposal_requests::write_state(state_root, state) {
        tracing::warn!(
            request_id = %state.request_id,
            "chat-triage: recording TriageFailed state failed: {e:#}"
        );
    }
    if let Some(ctx) = chatops_ctx {
        let body = format!(
            "✗ Chat-triage for `{repo_url}` failed: {reason}",
            repo_url = state.repo_url,
        );
        if let Err(e) = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.thread_ts, &body)
            .await
        {
            tracing::warn!(
                request_id = %state.request_id,
                "chat-triage: TriageFailed thread reply failed: {e:#}"
            );
        }
    }
}

/// Derive a unique `openspec/changes/<slug>/` path for a chat-triage
/// run. The slug is `chat-request-<short-hash-of-request-text>`; if it
/// already exists on disk, we append `-2`, `-3`, ... until we find a
/// free path.
fn derive_unique_chat_request_slug(workspace: &Path, request_text: &str) -> String {
    let hash = short_findings_hash(request_text);
    let base_slug = format!("chat-request-{hash}");
    let mut slug = base_slug.clone();
    let mut suffix = 2u32;
    while workspace.join("openspec/changes").join(&slug).exists() {
        slug = format!("{base_slug}-{suffix}");
        suffix += 1;
        if suffix > 100 {
            break;
        }
    }
    slug
}

/// Render a short single-line excerpt of the operator's request for PR
/// titles. Replaces internal newlines with spaces and truncates at 60
/// chars with a trailing `…`.
fn short_request_excerpt(request_text: &str) -> String {
    let one_line = request_text.replace('\n', " ");
    let cleaned: String = one_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= 60 {
        cleaned
    } else {
        let mut out: String = cleaned.chars().take(60).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightline_triage_scope_accepts_only_ignore_file() {
        let changed = vec![".brightline-ignore".to_string()];
        assert!(validate_brightline_triage_scope(
            "architecture_brightline",
            &changed,
            "openspec/changes/architecture-brightline-abcd1234/",
        )
        .is_ok());
    }

    #[test]
    fn brightline_triage_scope_accepts_ignore_plus_spec_dir() {
        let changed = vec![
            ".brightline-ignore".to_string(),
            "openspec/changes/architecture-brightline-abcd1234/proposal.md".to_string(),
        ];
        assert!(validate_brightline_triage_scope(
            "architecture_brightline",
            &changed,
            "openspec/changes/architecture-brightline-abcd1234/",
        )
        .is_ok());
    }

    #[test]
    fn brightline_triage_scope_rejects_ignore_mixed_with_code() {
        let changed = vec![
            ".brightline-ignore".to_string(),
            "src/foo.rs".to_string(),
        ];
        let err = validate_brightline_triage_scope(
            "architecture_brightline",
            &changed,
            "openspec/changes/architecture-brightline-abcd1234/",
        )
        .expect_err("mixed-scope diff must be rejected");
        assert_eq!(err, vec!["src/foo.rs".to_string()]);
    }

    #[test]
    fn brightline_triage_scope_accepts_pure_fixes_diff_without_ignore_file() {
        // No `.brightline-ignore` write → the LLM took the fix path.
        // That path is unconstrained.
        let changed = vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()];
        assert!(validate_brightline_triage_scope(
            "architecture_brightline",
            &changed,
            "openspec/changes/architecture-brightline-abcd1234/",
        )
        .is_ok());
    }

    #[test]
    fn brightline_triage_scope_noop_for_other_audits() {
        // Non-brightline audits are unaffected: a mixed diff is fine.
        let changed = vec![
            ".brightline-ignore".to_string(),
            "src/foo.rs".to_string(),
        ];
        assert!(validate_brightline_triage_scope(
            "drift_audit",
            &changed,
            "openspec/changes/drift-audit-abcd1234/",
        )
        .is_ok());
    }

    // ================================================================
    // a43: `discard_non_spec_writes` helper unit tests
    // ================================================================

    /// Init a throwaway git repo with a committed `src/bar.rs`, `README.md`
    /// at base. Returns the temp-dir guard (drop = cleanup) and workspace.
    fn dnsw_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/bar.rs"), "orig\n").unwrap();
        std::fs::write(ws.join("README.md"), "hi\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "base"]);
        (dir, ws)
    }

    #[test]
    fn discard_non_spec_writes_spec_only_returns_empty() {
        let (_d, ws) = dnsw_repo();
        std::fs::create_dir_all(ws.join("openspec/changes/foo")).unwrap();
        std::fs::write(ws.join("openspec/changes/foo/proposal.md"), "## Why\nx\n").unwrap();
        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();
        assert!(dropped.is_empty(), "spec-only diff drops nothing: {dropped:?}");
        assert!(
            ws.join("openspec/changes/foo/proposal.md").exists(),
            "spec file must be left untouched"
        );
    }

    #[test]
    fn discard_non_spec_writes_code_only_restores_and_removes() {
        let (_d, ws) = dnsw_repo();
        // Modify a tracked file AND add an untracked code file.
        std::fs::write(ws.join("src/bar.rs"), "MUTATED\n").unwrap();
        std::fs::write(ws.join("newcode.rs"), "junk\n").unwrap();
        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();
        assert_eq!(
            dropped,
            vec!["newcode.rs".to_string(), "src/bar.rs".to_string()]
        );
        // Tracked modification reverted; untracked addition removed.
        assert_eq!(std::fs::read_to_string(ws.join("src/bar.rs")).unwrap(), "orig\n");
        assert!(!ws.join("newcode.rs").exists());
        assert_eq!(
            crate::git::status_porcelain(&ws).unwrap(),
            "",
            "working tree must be clean after discarding all code writes"
        );
    }

    #[test]
    fn discard_non_spec_writes_mixed_keeps_spec_drops_code() {
        let (_d, ws) = dnsw_repo();
        std::fs::create_dir_all(ws.join("openspec/changes/foo")).unwrap();
        std::fs::write(ws.join("openspec/changes/foo/proposal.md"), "## Why\nx\n").unwrap();
        std::fs::write(ws.join("src/bar.rs"), "MUTATED\n").unwrap();
        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();
        assert_eq!(dropped, vec!["src/bar.rs".to_string()]);
        assert!(
            ws.join("openspec/changes/foo/proposal.md").exists(),
            "spec file must survive a mixed diff"
        );
        assert_eq!(std::fs::read_to_string(ws.join("src/bar.rs")).unwrap(), "orig\n");
    }

    #[test]
    fn discard_non_spec_writes_untracked_and_modified_mix() {
        let (_d, ws) = dnsw_repo();
        // Untracked spec file (kept) + modified tracked code (restored) +
        // untracked nested code file (removed).
        std::fs::create_dir_all(ws.join("openspec/changes/foo")).unwrap();
        std::fs::write(ws.join("openspec/changes/foo/tasks.md"), "- [ ] x\n").unwrap();
        std::fs::write(ws.join("src/bar.rs"), "MUTATED\n").unwrap();
        std::fs::create_dir_all(ws.join("src/sub")).unwrap();
        std::fs::write(ws.join("src/sub/new.rs"), "n\n").unwrap();
        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();
        assert_eq!(
            dropped,
            vec!["src/bar.rs".to_string(), "src/sub/new.rs".to_string()]
        );
        assert!(ws.join("openspec/changes/foo/tasks.md").exists());
        assert_eq!(std::fs::read_to_string(ws.join("src/bar.rs")).unwrap(), "orig\n");
        assert!(!ws.join("src/sub/new.rs").exists());
    }

    #[test]
    fn discard_non_spec_writes_clean_tree_noop() {
        let (_d, ws) = dnsw_repo();
        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();
        assert!(dropped.is_empty());
        assert_eq!(crate::git::status_porcelain(&ws).unwrap(), "");
    }

    /// a43 revision: a code edit the executor *staged* with `git add`
    /// must be fully reverted — index AND worktree. A plain
    /// `git restore -- <path>` only rewrites the worktree from the index,
    /// so the staged modification would survive in the index and leak
    /// into the supposedly spec-only commit. Because the path exists in
    /// HEAD, the handler reverts it with `git checkout HEAD -- <path>`,
    /// which unstages AND reverts regardless of the staged state.
    #[test]
    fn discard_non_spec_writes_reverts_staged_code_modification() {
        let (_d, ws) = dnsw_repo();
        std::fs::write(ws.join("src/bar.rs"), "STAGED MUTATION\n").unwrap();
        // Stage the code edit the way an LLM bash tool might.
        let st = std::process::Command::new("git")
            .args(["add", "src/bar.rs"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success(), "staging src/bar.rs failed");
        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();
        assert_eq!(dropped, vec!["src/bar.rs".to_string()]);
        // Worktree reverted to the committed base content...
        assert_eq!(std::fs::read_to_string(ws.join("src/bar.rs")).unwrap(), "orig\n");
        // ...AND nothing staged survives: the index is clean, so the
        // caller's `git add -A` + commit cannot sweep the code edit into
        // the spec-only PR.
        assert_eq!(
            crate::git::status_porcelain(&ws).unwrap(),
            "",
            "a staged code modification must be fully unstaged and reverted"
        );
    }

    /// a43 revision: a brand-new code file the executor created AND staged
    /// with `git add` (porcelain `A `, NOT present in HEAD) must be cleanly
    /// discarded — unstaged AND removed from disk — NOT aborted with a
    /// pathspec error. `git checkout HEAD -- <path>` / `git restore
    /// --source=HEAD` reject a path absent from HEAD on some git versions;
    /// the handler routes not-in-HEAD tracked paths through `git reset` +
    /// disk removal so the common "LLM `git add`ed a new file" case does
    /// not crash the triage flow.
    #[test]
    fn discard_non_spec_writes_discards_staged_new_file() {
        let (_d, ws) = dnsw_repo();
        std::fs::write(ws.join("newcode.rs"), "junk\n").unwrap();
        // Stage the brand-new file the way an LLM bash tool might.
        let st = std::process::Command::new("git")
            .args(["add", "newcode.rs"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success(), "staging newcode.rs failed");
        // Sanity: it is a STAGED ADD (`A `), not untracked (`??`) — so it
        // takes the tracked, not-in-HEAD branch, the one the old `git
        // restore --source=HEAD` would have choked on.
        let porc = crate::git::status_porcelain(&ws).unwrap();
        assert!(
            porc.starts_with("A "),
            "expected a staged addition (A ), got {porc:?}"
        );

        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();

        assert_eq!(dropped, vec!["newcode.rs".to_string()]);
        assert!(
            !ws.join("newcode.rs").exists(),
            "the staged new file must be removed from disk"
        );
        assert_eq!(
            crate::git::status_porcelain(&ws).unwrap(),
            "",
            "a staged new file must be fully unstaged AND removed — nothing \
             left for the caller's `git add -A` to sweep into the spec PR"
        );
    }

    /// a43 revision: when an untracked non-spec write cannot be removed
    /// (here: a write-protected parent directory blocks the unlink), the
    /// helper must fail loudly rather than silently leave the file for
    /// the caller's `git add -A` to sweep into the spec-only PR.
    #[test]
    fn discard_non_spec_writes_errors_when_removal_fails() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, ws) = dnsw_repo();
        // An untracked code file inside a directory we then strip write
        // permission from (`r-xr-xr-x`) so `remove_file` fails with
        // PermissionDenied — the unlink needs write permission on the
        // parent directory, not on the file itself.
        let locked = ws.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("leak.rs"), "junk\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = discard_non_spec_writes(&ws, "foo");

        // Restore write permission so the TempDir guard can clean up,
        // regardless of the assertion outcome below.
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));

        assert!(
            result.is_err(),
            "a removal failure must surface as an error, not a silent leak"
        );
        assert!(
            ws.join("locked/leak.rs").exists(),
            "the un-removable file must still be on disk — the error has to \
             prevent the spec commit rather than the file being quietly gone"
        );
    }

    /// a43 revision: paths git would quote under the default
    /// `core.quotePath` (a space AND non-ASCII bytes both trigger it) must
    /// still be parsed AND acted on correctly. `triage_status_entries`
    /// uses `-z`, which disables quoting; the pre-fix default-format parse
    /// would yield the literal `"f\303\266\303\266.rs"`, so `remove_file`
    /// would NotFound-no-op AND the real file would survive into the
    /// spec-only commit. A quoted SPEC path must likewise keep its
    /// `openspec/changes/` prefix so it is NOT misclassified as non-spec.
    #[test]
    fn discard_non_spec_writes_handles_quoted_special_char_paths() {
        let (_d, ws) = dnsw_repo();
        // Untracked code files whose names force quoting: a space AND
        // non-ASCII. Both must be dropped.
        std::fs::write(ws.join("a b.rs"), "junk\n").unwrap();
        std::fs::write(ws.join("föö.rs"), "junk\n").unwrap();
        // A spec file with a quote-forcing name must be KEPT.
        std::fs::create_dir_all(ws.join("openspec/changes/foo")).unwrap();
        std::fs::write(ws.join("openspec/changes/foo/néw.md"), "## Why\nx\n").unwrap();

        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();

        // Sorted: "a b.rs" (0x61) precedes "föö.rs" (0x66).
        assert_eq!(
            dropped,
            vec!["a b.rs".to_string(), "föö.rs".to_string()],
            "both quote-forcing untracked code paths must be parsed AND dropped"
        );
        assert!(!ws.join("a b.rs").exists(), "the spaced path must be removed");
        assert!(!ws.join("föö.rs").exists(), "the non-ASCII path must be removed");
        assert!(
            ws.join("openspec/changes/foo/néw.md").exists(),
            "a quote-forcing spec path must be kept, not discarded"
        );
    }

    /// a43 revision: a STAGED rename of a tracked code file must be fully
    /// undone. Under `-z` the rename record is `dest\0source\0`, so the
    /// parser MUST consume both fields (else the source path leaks back as
    /// a bogus untracked entry AND the rename half-survives). Both sides
    /// revert to the committed state.
    #[test]
    fn discard_non_spec_writes_reverts_staged_rename() {
        let (_d, ws) = dnsw_repo();
        let run = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        // Stage a rename the way an LLM bash tool might.
        run(&["mv", "src/bar.rs", "src/renamed.rs"]);
        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();
        assert_eq!(
            dropped,
            vec!["src/bar.rs".to_string(), "src/renamed.rs".to_string()],
            "both the rename destination AND source must be reported AND reverted"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("src/bar.rs")).unwrap(),
            "orig\n",
            "the rename source must be restored to its committed content"
        );
        assert!(
            !ws.join("src/renamed.rs").exists(),
            "the rename destination must be removed"
        );
        assert_eq!(
            crate::git::status_porcelain(&ws).unwrap(),
            "",
            "a staged rename must be fully undone — index AND worktree"
        );
    }

    /// a43 revision: an untracked SYMLINK must be unlinked (dropping just
    /// the link), NOT followed. `is_dir()` follows the link, so a
    /// symlink-to-directory would route into `remove_dir_all` and could
    /// wipe the TARGET's contents; `symlink_metadata` routes it to
    /// `remove_file` instead. The link target must be left intact.
    #[test]
    fn discard_non_spec_writes_unlinks_symlink_without_following() {
        use std::os::unix::fs::symlink;
        let (_d, ws) = dnsw_repo();
        // A directory OUTSIDE the repo (its own temp dir, so git status
        // never sees it) holding a file we must not touch.
        let target_guard = tempfile::TempDir::new().unwrap();
        let target = target_guard.path().join("outside-target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("precious.txt"), "do not delete\n").unwrap();
        // An untracked symlink inside the repo pointing at that directory.
        symlink(&target, ws.join("linkdir")).unwrap();

        let dropped = discard_non_spec_writes(&ws, "foo").unwrap();

        assert_eq!(dropped, vec!["linkdir".to_string()]);
        assert!(
            !ws.join("linkdir").exists(),
            "the untracked symlink must be removed"
        );
        assert!(
            target.join("precious.txt").exists(),
            "the symlink target's contents must NOT be followed and deleted"
        );
    }

    // ================================================================
    // a43: triage completion-handler tests (spec-only PR shape)
    // ================================================================

    /// ChatOps backend that records every threaded reply for assertion.
    struct RecordingChatOps {
        replies: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl ChatOpsBackend for RecordingChatOps {
        fn provider_name(&self) -> &'static str {
            "recording"
        }
        fn is_experimental(&self) -> bool {
            true
        }
        async fn post_question(&self, _: &str, _: &str, _: &str) -> Result<String> {
            unreachable!("triage handlers never post_question")
        }
        async fn poll_thread_for_human_reply(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<crate::chatops::HumanReply>> {
            Ok(None)
        }
        async fn post_notification(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn post_threaded_reply(&self, _: &str, _: &str, text: &str) -> Result<()> {
            self.replies.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn recording_ctx(chatops: &Arc<RecordingChatOps>) -> ChatOpsContext {
        ChatOpsContext {
            chatops: chatops.clone(),
            channel: "C_TEST".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        }
    }

    fn triage_github_cfg() -> GithubConfig {
        GithubConfig {
            token_env: "X".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "inline-test-token".into(),
            }),
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        }
    }

    fn audit_state() -> crate::audits::threads::AuditThreadState {
        crate::audits::threads::AuditThreadState {
            thread_ts: "T-audit".into(),
            channel: "C_TEST".into(),
            repo_url: "git@github.com:owner/fixture.git".into(),
            audit_type: "security_bug".into(),
            findings_excerpt: "FINDINGS".into(),
            posted_at: chrono::Utc::now(),
            status: crate::audits::threads::AuditThreadStatus::TriagePending,
            reason: None,
        }
    }

    fn proposal_state() -> crate::proposal_requests::ProposalRequestState {
        crate::proposal_requests::ProposalRequestState {
            request_id: "req-1".into(),
            repo_url: "git@github.com:owner/fixture.git".into(),
            channel: "C_TEST".into(),
            thread_ts: "T-chat".into(),
            ack_message_ts: "T-chat".into(),
            operator_user: "U_OP".into(),
            request_text: "add a /healthz endpoint".into(),
            submitted_at: chrono::Utc::now(),
            status: crate::proposal_requests::ProposalRequestStatus::TriagePending,
            reason: None,
        }
    }

    /// Write a fake spec change dir (mimics the executor's openspec write).
    fn write_fake_spec(ws: &Path, slug: &str) {
        let dir = ws.join("openspec/changes").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proposal.md"), "## Why\nfixture\n## What Changes\n- x\n## Impact\n- y\n").unwrap();
        std::fs::write(dir.join("tasks.md"), "- [ ] do the thing\n").unwrap();
    }

    /// 7.1: audit-triage mixed diff → one spec PR, code discarded, chatops
    /// warning posted, spec branch diff is spec-only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a43_audit_mixed_diff_opens_one_spec_pr_and_warns() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td, paths) = crate::testing::test_daemon_paths();
        // Executor's writes: a spec dir + an out-of-scope code file.
        write_fake_spec(&ws, "audit-fix-x");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/foo.rs"), "agent code\n").unwrap();

        let _hook = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"html_url":"https://github.com/owner/fixture/pull/7","number":7}"#)
            .expect(1)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let chatops = Arc::new(RecordingChatOps { replies: std::sync::Mutex::new(Vec::new()) });
        let ctx = recording_ctx(&chatops);
        let mut state = audit_state();
        let res = process_completed_triage(
            &paths, &ws, &fixture_repo(&ws), &triage_github_cfg(), Some(&ctx), &mut state, None,
        )
        .await;
        test_hooks::set_github_api_base(None);
        res.expect("handler must succeed");

        pr_mock.assert_async().await;
        assert_eq!(state.status, crate::audits::threads::AuditThreadStatus::Acted);
        // Code path discarded from the working tree.
        assert!(!ws.join("src/foo.rs").exists(), "code write must be discarded");
        // Spec branch carries ONLY openspec/changes/ paths.
        let files = crate::git::diff_files_changed(&ws, "main", "agent-q-triage-spec").unwrap();
        assert!(!files.is_empty(), "spec branch must carry a diff");
        assert!(
            files.iter().all(|f| f.starts_with("openspec/changes/")),
            "spec PR diff must be spec-only, got {files:?}"
        );
        let replies = chatops.replies.lock().unwrap().clone();
        assert!(
            replies.iter().any(|r| r.contains("src/foo.rs") && r.contains("outside")),
            "a dropped-paths warning naming src/foo.rs must be posted, got {replies:?}"
        );
        assert!(
            replies.iter().any(|r| r.contains("Spec PR:")),
            "the spec PR URL must be surfaced, got {replies:?}"
        );
    }

    /// 7.2: chat-triage mixed diff → same shape.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a43_chat_mixed_diff_opens_one_spec_pr_and_warns() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td, paths) = crate::testing::test_daemon_paths();
        write_fake_spec(&ws, "chat-request-y");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/foo.rs"), "agent code\n").unwrap();

        let _hook = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"html_url":"https://github.com/owner/fixture/pull/8","number":8}"#)
            .expect(1)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let chatops = Arc::new(RecordingChatOps { replies: std::sync::Mutex::new(Vec::new()) });
        let ctx = recording_ctx(&chatops);
        let mut state = proposal_state();
        let res = process_completed_proposal(
            &paths, &ws, &fixture_repo(&ws), &triage_github_cfg(), Some(&ctx), &mut state, None,
        )
        .await;
        test_hooks::set_github_api_base(None);
        res.expect("handler must succeed");

        pr_mock.assert_async().await;
        assert_eq!(state.status, crate::proposal_requests::ProposalRequestStatus::Acted);
        assert!(!ws.join("src/foo.rs").exists(), "code write must be discarded");
        let files = crate::git::diff_files_changed(&ws, "main", "agent-q-chat-spec").unwrap();
        assert!(
            !files.is_empty() && files.iter().all(|f| f.starts_with("openspec/changes/")),
            "spec PR diff must be spec-only, got {files:?}"
        );
        let replies = chatops.replies.lock().unwrap().clone();
        assert!(
            replies.iter().any(|r| r.contains("src/foo.rs") && r.contains("outside")),
            "dropped-paths warning expected, got {replies:?}"
        );
    }

    /// 7.3: spec-only outcome → one PR, NO dropped-paths warning.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a43_audit_spec_only_opens_pr_without_warning() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td, paths) = crate::testing::test_daemon_paths();
        write_fake_spec(&ws, "audit-fix-z");

        let _hook = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"html_url":"https://github.com/owner/fixture/pull/9","number":9}"#)
            .expect(1)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let chatops = Arc::new(RecordingChatOps { replies: std::sync::Mutex::new(Vec::new()) });
        let ctx = recording_ctx(&chatops);
        let mut state = audit_state();
        let res = process_completed_triage(
            &paths, &ws, &fixture_repo(&ws), &triage_github_cfg(), Some(&ctx), &mut state, None,
        )
        .await;
        test_hooks::set_github_api_base(None);
        res.expect("handler must succeed");

        pr_mock.assert_async().await;
        assert_eq!(state.status, crate::audits::threads::AuditThreadStatus::Acted);
        let replies = chatops.replies.lock().unwrap().clone();
        assert!(
            !replies.iter().any(|r| r.contains("outside")),
            "no dropped-paths warning when the agent followed the restriction, got {replies:?}"
        );
        assert!(
            replies.iter().any(|r| r.contains("Spec PR:")),
            "spec PR URL must be surfaced, got {replies:?}"
        );
    }

    /// 7.3 (chat): spec-only outcome → one PR, no warning.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a43_chat_spec_only_opens_pr_without_warning() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td, paths) = crate::testing::test_daemon_paths();
        write_fake_spec(&ws, "chat-request-z");

        let _hook = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"html_url":"https://github.com/owner/fixture/pull/10","number":10}"#)
            .expect(1)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let chatops = Arc::new(RecordingChatOps { replies: std::sync::Mutex::new(Vec::new()) });
        let ctx = recording_ctx(&chatops);
        let mut state = proposal_state();
        let res = process_completed_proposal(
            &paths, &ws, &fixture_repo(&ws), &triage_github_cfg(), Some(&ctx), &mut state, None,
        )
        .await;
        test_hooks::set_github_api_base(None);
        res.expect("handler must succeed");

        pr_mock.assert_async().await;
        assert_eq!(state.status, crate::proposal_requests::ProposalRequestStatus::Acted);
        let replies = chatops.replies.lock().unwrap().clone();
        assert!(!replies.iter().any(|r| r.contains("outside")), "no warning expected");
    }

    /// 7.4: code-only outcome → NO PR, "no spec content" reply, tree clean,
    /// status TriageFailed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a43_audit_code_only_opens_no_pr() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td, paths) = crate::testing::test_daemon_paths();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/foo.rs"), "agent code\n").unwrap();

        let _hook = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .expect(0)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let chatops = Arc::new(RecordingChatOps { replies: std::sync::Mutex::new(Vec::new()) });
        let ctx = recording_ctx(&chatops);
        let mut state = audit_state();
        let res = process_completed_triage(
            &paths, &ws, &fixture_repo(&ws), &triage_github_cfg(), Some(&ctx), &mut state, None,
        )
        .await;
        test_hooks::set_github_api_base(None);
        res.expect("handler must succeed");

        pr_mock.assert_async().await; // expect(0): no PR opened
        assert_eq!(
            state.status,
            crate::audits::threads::AuditThreadStatus::TriageFailed
        );
        assert!(!ws.join("src/foo.rs").exists(), "code write must be restored away");
        assert_eq!(
            crate::git::status_porcelain(&ws).unwrap(),
            "",
            "working tree must be clean after the handler returns"
        );
        let replies = chatops.replies.lock().unwrap().clone();
        assert!(
            replies.iter().any(|r| r.contains("no spec content")),
            "the no-spec-content reply must be posted, got {replies:?}"
        );
    }

    /// 7.4 (chat): code-only → NO PR, TriageFailed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a43_chat_code_only_opens_no_pr() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td, paths) = crate::testing::test_daemon_paths();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/foo.rs"), "agent code\n").unwrap();

        let _hook = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .expect(0)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let chatops = Arc::new(RecordingChatOps { replies: std::sync::Mutex::new(Vec::new()) });
        let ctx = recording_ctx(&chatops);
        let mut state = proposal_state();
        let res = process_completed_proposal(
            &paths, &ws, &fixture_repo(&ws), &triage_github_cfg(), Some(&ctx), &mut state, None,
        )
        .await;
        test_hooks::set_github_api_base(None);
        res.expect("handler must succeed");

        pr_mock.assert_async().await;
        assert_eq!(
            state.status,
            crate::proposal_requests::ProposalRequestStatus::TriageFailed
        );
        assert_eq!(crate::git::status_porcelain(&ws).unwrap(), "", "tree must be clean");
        let replies = chatops.replies.lock().unwrap().clone();
        assert!(replies.iter().any(|r| r.contains("no spec content")), "no-spec reply expected");
    }

    /// Empty-diff audit outcome → no PR, no-action reply carries the
    /// executor's final summary, status Acted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a43_audit_empty_diff_posts_no_action_reply() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td, paths) = crate::testing::test_daemon_paths();

        let _hook = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .expect(0)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let chatops = Arc::new(RecordingChatOps { replies: std::sync::Mutex::new(Vec::new()) });
        let ctx = recording_ctx(&chatops);
        let mut state = audit_state();
        let res = process_completed_triage(
            &paths,
            &ws,
            &fixture_repo(&ws),
            &triage_github_cfg(),
            Some(&ctx),
            &mut state,
            Some("Nothing actionable in these findings."),
        )
        .await;
        test_hooks::set_github_api_base(None);
        res.expect("handler must succeed");

        pr_mock.assert_async().await;
        assert_eq!(state.status, crate::audits::threads::AuditThreadStatus::Acted);
        let replies = chatops.replies.lock().unwrap().clone();
        assert!(
            replies.iter().any(|r| r.contains("no actionable changes")
                && r.contains("Nothing actionable in these findings.")),
            "no-action reply must carry the executor's summary, got {replies:?}"
        );
    }

    /// Brightline "Mark as intentional" carve-out: a brightline triage whose
    /// only write is `.brightline-ignore` ships it directly in one PR (NOT
    /// discarded as a non-spec write), since it has no implementer-pipeline
    /// equivalent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a43_brightline_intentional_ships_ignore_file_in_one_pr() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td, paths) = crate::testing::test_daemon_paths();
        std::fs::write(
            ws.join(".brightline-ignore"),
            "ignore:\n  - file: a.ts\n    function: f\n    signature_match: \"f(\"\n    reason: intentional\n",
        )
        .unwrap();

        let _hook = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"html_url":"https://github.com/owner/fixture/pull/11","number":11}"#)
            .expect(1)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let chatops = Arc::new(RecordingChatOps { replies: std::sync::Mutex::new(Vec::new()) });
        let ctx = recording_ctx(&chatops);
        let mut state = audit_state();
        state.audit_type = "architecture_brightline".into();
        let res = process_completed_triage(
            &paths, &ws, &fixture_repo(&ws), &triage_github_cfg(), Some(&ctx), &mut state, None,
        )
        .await;
        test_hooks::set_github_api_base(None);
        res.expect("handler must succeed");

        pr_mock.assert_async().await;
        assert_eq!(state.status, crate::audits::threads::AuditThreadStatus::Acted);
        let files = crate::git::diff_files_changed(&ws, "main", "agent-q-triage-spec").unwrap();
        assert!(
            files.iter().any(|f| f == ".brightline-ignore"),
            "the .brightline-ignore write must ship (not be discarded), got {files:?}"
        );
    }

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

    // -----------------------------------------------------------------
    // a26 OSS-fork support: opportunistic upstream fetch tests.
    // -----------------------------------------------------------------

    fn init_bare(dir: &Path) {
        let st = std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(dir)
            .status()
            .unwrap();
        assert!(st.success(), "bare init failed");
    }

    fn init_clone(remote: &Path, target: &Path) {
        let st = std::process::Command::new("git")
            .args([
                "clone",
                "-q",
                remote.to_string_lossy().as_ref(),
                target.to_string_lossy().as_ref(),
            ])
            .status()
            .unwrap();
        assert!(st.success(), "clone failed");
    }

    fn remote_url(workspace: &Path, name: &str) -> Option<String> {
        let out = std::process::Command::new("git")
            .args(["remote", "get-url", name])
            .current_dir(workspace)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    #[test]
    fn opportunistic_upstream_fetch_no_block_no_action() {
        // Upstream unconfigured: function is a no-op.
        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().join("bare.git");
        init_bare(&bare);
        let workspace = dir.path().join("workspace");
        init_clone(&bare, &workspace);
        let repo = fixture_repo(&workspace);
        // Capture pre-state: no `upstream` remote.
        assert!(remote_url(&workspace, "upstream").is_none());
        opportunistic_upstream_fetch(&workspace, &repo);
        // Still no `upstream` remote — function did nothing.
        assert!(remote_url(&workspace, "upstream").is_none());
    }

    #[test]
    fn opportunistic_upstream_fetch_adds_remote_and_fetches() {
        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().join("bare.git");
        init_bare(&bare);
        let upstream_bare = dir.path().join("upstream.git");
        init_bare(&upstream_bare);
        let workspace = dir.path().join("workspace");
        init_clone(&bare, &workspace);
        let mut repo = fixture_repo(&workspace);
        repo.upstream = Some(crate::config::UpstreamConfig {
            remote: "upstream".to_string(),
            branch: "main".to_string(),
            url: upstream_bare.to_string_lossy().to_string(),
        });
        opportunistic_upstream_fetch(&workspace, &repo);
        let url = remote_url(&workspace, "upstream")
            .expect("upstream remote should be added");
        assert_eq!(url, upstream_bare.to_string_lossy().to_string());
    }

    #[test]
    fn opportunistic_upstream_fetch_corrects_drifted_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().join("bare.git");
        init_bare(&bare);
        let upstream_a = dir.path().join("upstream-a.git");
        init_bare(&upstream_a);
        let upstream_b = dir.path().join("upstream-b.git");
        init_bare(&upstream_b);
        let workspace = dir.path().join("workspace");
        init_clone(&bare, &workspace);
        // Pre-seed an `upstream` remote pointing at A.
        let st = std::process::Command::new("git")
            .args(["remote", "add", "upstream"])
            .arg(upstream_a.to_string_lossy().as_ref())
            .current_dir(&workspace)
            .status()
            .unwrap();
        assert!(st.success());
        // Configure upstream B in the repo.
        let mut repo = fixture_repo(&workspace);
        repo.upstream = Some(crate::config::UpstreamConfig {
            remote: "upstream".to_string(),
            branch: "main".to_string(),
            url: upstream_b.to_string_lossy().to_string(),
        });
        opportunistic_upstream_fetch(&workspace, &repo);
        let url = remote_url(&workspace, "upstream").unwrap();
        assert_eq!(url, upstream_b.to_string_lossy().to_string());
    }

    #[test]
    fn opportunistic_upstream_fetch_failure_does_not_propagate() {
        // Point upstream.url at a path that isn't a git repo — fetch
        // will fail, function must log a WARN and return cleanly.
        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().join("bare.git");
        init_bare(&bare);
        let workspace = dir.path().join("workspace");
        init_clone(&bare, &workspace);
        let mut repo = fixture_repo(&workspace);
        repo.upstream = Some(crate::config::UpstreamConfig {
            remote: "upstream".to_string(),
            branch: "main".to_string(),
            url: "/dev/null/definitely-not-a-repo".to_string(),
        });
        // Should not panic AND should return normally.
        opportunistic_upstream_fetch(&workspace, &repo);
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
            Ok(ExecutorOutcome::Completed { final_answer: None })
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
            Ok(ExecutorOutcome::Completed { final_answer: None })
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
        let (_td, paths) = crate::testing::test_daemon_paths();
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
                &paths, workspace, &repo, &github_cfg, executor, None, u32::MAX, u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            )
            .await?;
        Ok(processed)
    }

    /// 13.3.2 / executor baseline: when the executor returns `Failed`,
    /// autocoder unlocks the change AND does NOT archive it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_change_unlocks_and_does_not_archive() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "feature-a", "fixture reason");

        let executor = AlwaysFailingExecutor;
        let _ = run_one_pass_no_push(&ws, &executor).await; // Failed is a normal outcome

        // The change is still in the active queue (not archived).
        let pending = queue::list_pending(&paths, &ws).unwrap();
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
        let (_td_paths, _paths) = crate::testing::test_daemon_paths();
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
        let (_td_paths, _paths) = crate::testing::test_daemon_paths();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            queue::list_pending(&paths, &ws).unwrap(),
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
        let (_td_paths, _paths) = crate::testing::test_daemon_paths();

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
            Ok(ExecutorOutcome::Completed { final_answer: None })
        }
    }

    /// 5.2: AskUser on a pending change → posts to Slack, writes
    /// `.question.json`, unlocks the change, change is excluded from
    /// pending and shows up in `list_waiting`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn askuser_on_pending_escalates_to_chatops() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        assert_eq!(queue::list_pending(&paths, &ws).unwrap(), Vec::<String>::new());
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
            &paths,
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
            &std::collections::HashSet::new(),
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
            queue::list_pending(&paths, &ws).unwrap().contains(&"ambig-change".to_string()),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                Ok(ExecutorOutcome::Completed { final_answer: None })
            }
            async fn resume(
                &self,
                _h: ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                self.invocations.lock().unwrap().push("resume".to_string());
                std::fs::write(self.ws.join("RESUMED.txt"), "from resume")?;
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                Ok(ExecutorOutcome::Completed { final_answer: None })
            }
            async fn resume(
                &self,
                _h: ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                self.invocations.lock().unwrap().push("resume".to_string());
                std::fs::write(self.ws.join("RESUMED.txt"), "from resume")?;
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let still_pending = queue::list_pending(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                &paths,
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
            &std::collections::HashSet::new(),
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
                        concerns: Vec::new(),
                        per_change_sections: Vec::new(),
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
        let (_td_paths, _paths) = crate::testing::test_daemon_paths();
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
        let (_td_paths, _paths) = crate::testing::test_daemon_paths();
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
        let paths_for_run = std::sync::Arc::new(crate::testing::test_daemon_paths().1);
        let handle = tokio::spawn(async move {
            run(
                paths_for_run,
                repo_holder,
                executor_dyn,
                github_holder,
                reviewer_holder,
                chatops_holder,
                2400,
                u32::MAX,
                Some(u32::MAX),
                0, // revision_cap: disabled in tests
                0, // startup_jitter_max_secs: deterministic for tests
                0, // inter_iteration_jitter_pct: deterministic for tests
                std::sync::Arc::new(crate::audits::AuditRegistry::default()),
                None,
                std::sync::Arc::new(std::collections::HashMap::new()),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(None)),
                std::sync::Arc::new(tokio::sync::Notify::new()),
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

    // ============================================================
    // Per-iteration cancel + drained Notify (IterationGuard)
    // ============================================================

    /// IterationGuard's Drop impl clears the per-iteration cancel handle
    /// AND fires the drained Notify — exercised in isolation so we know
    /// the cleanup runs on every exit path, including panic unwind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iteration_guard_drop_clears_handle_and_notifies() {
        let iter_cancel: Arc<std::sync::Mutex<Option<CancellationToken>>> =
            Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let drained: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());

        // Subscribe to the Notify BEFORE the guard drops so we don't miss
        // the wake. `notify_waiters()` only wakes futures that are already
        // registered as waiters; the `.enable()` call registers the
        // `Notified` future synchronously without polling it.
        let notified = drained.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // Run the guard in a scope so it drops at the end.
        {
            let _guard = IterationGuard {
                iteration_cancel: iter_cancel.as_ref(),
                iteration_drained: drained.as_ref(),
            };
            assert!(
                iter_cancel.lock().unwrap().is_some(),
                "handle is populated before drop"
            );
        }
        // After the drop, the handle is cleared.
        assert!(
            iter_cancel.lock().unwrap().is_none(),
            "IterationGuard Drop must clear the cancel handle"
        );
        // And the pre-registered notified future is ready.
        tokio::time::timeout(Duration::from_secs(1), notified.as_mut())
            .await
            .expect("IterationGuard Drop must fire the drained Notify");
    }

    /// Panic inside the iteration scope still triggers the guard's Drop —
    /// the Notify fires AND the handle is cleared. Verifies the
    /// "every exit path" contract for tasks.md 1.3.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iteration_guard_clears_state_on_panic_unwind() {
        let iter_cancel: Arc<std::sync::Mutex<Option<CancellationToken>>> =
            Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let drained: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());

        // Pre-register on the Notify so the panic-driven Drop's
        // notify_waiters() has a waiter to wake.
        let notified = drained.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let iter_cancel_for_panic = iter_cancel.clone();
        let drained_for_panic = drained.clone();
        let join = std::thread::spawn(move || {
            let _guard = IterationGuard {
                iteration_cancel: iter_cancel_for_panic.as_ref(),
                iteration_drained: drained_for_panic.as_ref(),
            };
            // Force a panic inside the iteration body's scope. The Drop
            // impl runs on unwind — that's the contract we're verifying.
            panic!("simulated iteration-body panic");
        });
        // The thread panics; join returns Err(_). Drop ran nonetheless.
        let res = join.join();
        assert!(res.is_err(), "thread must have panicked");

        assert!(
            iter_cancel.lock().unwrap().is_none(),
            "guard Drop must clear the handle even on panic"
        );
        tokio::time::timeout(Duration::from_secs(1), notified.as_mut())
            .await
            .expect("Notify must fire even on panic-unwind drop");
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
        let paths_for_run = std::sync::Arc::new(crate::testing::test_daemon_paths().1);
        let handle = tokio::spawn(async move {
            run_with_hooks(
                paths_for_run,
                repo_holder,
                executor,
                github_holder,
                reviewer_holder,
                chatops_holder,
                2400,
                u32::MAX,
                Some(u32::MAX),
                0, // revision_cap: disabled in tests
                0, // startup_jitter_max_secs: deterministic for tests
                0, // inter_iteration_jitter_pct: deterministic for tests
                std::sync::Arc::new(crate::audits::AuditRegistry::default()),
                None,
                std::sync::Arc::new(std::collections::HashMap::new()),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(None)),
                std::sync::Arc::new(tokio::sync::Notify::new()),
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
    // a26 OSS-fork support: auto_submit_pr gate helpers.
    // ============================================================

    #[test]
    fn compose_branch_url_formats_github_tree_url() {
        assert_eq!(
            compose_branch_url("upstream-owner", "upstream-repo", "agent-q"),
            "https://github.com/upstream-owner/upstream-repo/tree/agent-q"
        );
    }

    #[test]
    fn auto_submit_pr_defaults_to_true_on_fixture() {
        let repo = open_pr_test_repo();
        assert!(repo.auto_submit_pr);
    }

    #[test]
    fn suggested_pr_command_picks_upstream_branch_when_configured() {
        // When upstream is set, the suggested gh pr create base is
        // upstream.branch.
        let mut repo = open_pr_test_repo();
        repo.upstream = Some(crate::config::UpstreamConfig {
            remote: "upstream".to_string(),
            branch: "trunk".to_string(),
            url: "https://github.com/up/repo.git".to_string(),
        });
        let pr_base = repo
            .upstream
            .as_ref()
            .map(|u| u.branch.as_str())
            .unwrap_or(&repo.base_branch);
        assert_eq!(pr_base, "trunk");
    }

    #[test]
    fn suggested_pr_command_falls_back_to_base_branch_when_no_upstream() {
        let repo = open_pr_test_repo();
        let pr_base = repo
            .upstream
            .as_ref()
            .map(|u| u.branch.as_str())
            .unwrap_or(&repo.base_branch);
        assert_eq!(pr_base, "main");
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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

        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let result = open_pr_exists_for_agent_branch_at(
            &paths,
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

        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let result = open_pr_exists_for_agent_branch_at(
            &paths,
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let result = open_pr_exists_for_agent_branch_at(
            &paths,
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let result = open_pr_exists_for_agent_branch_at(
            &paths,
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
        let _ = execute_one_pass(&paths, &ws,
            &fixture_repo(&ws),
            &executor,
            &github,
            None,
            Some(&chatops_ctx),
            stuck_secs,
            u32::MAX,
            u32::MAX,
            0, // revision_cap: disabled in tests
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
        )
        .await;
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            paths.alert_state_path(&basename).exists(),
            "iter 1's push failure must persist alert state"
        );

        // Iteration 2: invoke `handle_predictable_failure` directly with a
        // synthesized push error. State is loaded from disk; the entry is
        // recent (< 24h), so should_alert is false → no post, mock counter
        // stays at 1. This is the throttle assertion: a repeat failure
        // within the window is silent.
        crate::alerts::handle_predictable_failure(&paths, &ws,
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
        let _ = execute_one_pass(&paths, &ws,
            &fixture_repo(&ws),
            &executor,
            &github,
            None,
            Some(&chatops_ctx),
            stuck_secs,
            u32::MAX,
            u32::MAX,
            0, // revision_cap: disabled in tests
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
        )
        .await;
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            paths.alert_state_path(&basename).exists(),
            "alert state should be written after first failure"
        );

        // Iteration 2: simulate a successful pass-end by directly clearing
        // the alert state, mimicking what `execute_one_pass` does on each
        // of its Ok-return paths (after push+PR succeed, when processed is
        // empty, or when commit_count is zero). The clear paths are
        // covered by `AlertState::clear`'s own unit tests; here we just
        // need the on-disk state to be gone so iter 3 can re-alert.
        crate::alert_state::AlertState::clear(&paths, &ws).unwrap();
        assert!(
            !paths.alert_state_path(&basename).exists(),
            "alert state must be gone after clear"
        );

        // Iteration 3: simulate another push failure via the helper. State
        // file is gone (cleared in iter 2), so this re-alerts even though
        // less than 24h has elapsed since iter 1's alert.
        crate::alerts::handle_predictable_failure(&paths, &ws,
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
    fn write_fixture_run_log(paths: &crate::paths::DaemonPaths, workspace: &Path, change: &str, prompt: &str, stdout: &str, stderr: &str) {
        let path = crate::executor::claude_cli::run_log_path(paths, workspace, change);
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        write_fixture_run_log(
            &paths,
            ws,
            "alpha",
            "PROMPT_BODY_SECRET",
            "STDOUT_NARRATIVE_VISIBLE",
            "STDERR_LOG_NOISE",
        );
        let out = build_implementer_summary(&paths, ws, &["alpha".to_string()]);
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        write_fixture_run_log(&paths, ws, "present", "p", "PRESENT_STDOUT", "");
        // "absent" has no log file written.
        let out = build_implementer_summary(
            &paths,
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let out = build_implementer_summary(
            &paths,
            ws,
            &["nope-1".to_string(), "nope-2".to_string()],
        );
        assert!(out.is_empty(), "expected empty string, got: {out:?}");
    }

    #[test]
    fn build_implementer_summary_uses_placeholder_for_empty_stdout() {
        let dir = unique_workspace("empty-stdout");
        let ws = dir.path();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        write_fixture_run_log(&paths, ws, "silent", "p", "", "");
        let out = build_implementer_summary(&paths, ws, &["silent".to_string()]);
        assert!(out.contains("### silent"));
        assert!(out.contains("_(no implementer output captured)_"));
    }

    /// Write a fixture run-log in the new JSON-streaming shape
    /// (PROMPT, ACTIONS, FINAL ANSWER, STDERR sections). Used by
    /// tests that verify the PR-comment construction path reads from
    /// FINAL ANSWER, not the action stream.
    fn write_fixture_json_run_log(
        paths: &crate::paths::DaemonPaths,
        workspace: &Path,
        change: &str,
        prompt: &str,
        actions_lines: &[&str],
        final_answer: &str,
        stderr: &str,
    ) {
        let path = crate::executor::claude_cli::run_log_path(paths, workspace, change);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut body = format!(
            "=== PROMPT ({p} bytes) ===\n{prompt}\n\n=== ACTIONS ===\n",
            p = prompt.len()
        );
        for line in actions_lines {
            body.push_str(line);
            body.push('\n');
        }
        body.push_str(&format!(
            "\n=== FINAL ANSWER ({n} bytes) ===\n{final_answer}\n\n=== STDERR ({m} bytes) ===\n{stderr}\n",
            n = final_answer.len(),
            m = stderr.len(),
        ));
        std::fs::write(&path, body).unwrap();
    }

    #[test]
    fn build_implementer_summary_reads_final_answer_from_json_log() {
        let dir = unique_workspace("final-answer");
        let ws = dir.path();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        write_fixture_json_run_log(
            &paths,
            ws,
            "alpha",
            "PROMPT_BODY",
            &[
                "[tool_use] Read foo.rs",
                "[tool_result] (123 bytes returned)",
                "[assistant] looking at the code",
            ],
            "FINAL_SUMMARY_TEXT",
            "",
        );
        let out = build_implementer_summary(&paths, ws, &["alpha".to_string()]);
        assert!(out.contains("FINAL_SUMMARY_TEXT"));
        // Action stream MUST NOT leak into the PR comment.
        assert!(!out.contains("[tool_use]"));
        assert!(!out.contains("[tool_result]"));
        assert!(!out.contains("[assistant]"));
        assert!(!out.contains("Read foo.rs"));
    }

    #[test]
    fn build_implementer_summary_falls_back_to_timeout_placeholder() {
        let dir = unique_workspace("timeout-fallback");
        let ws = dir.path();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        write_fixture_json_run_log(
            &paths,
            ws,
            "alpha",
            "p",
            &["[tool_use] Read a"],
            "", // empty FINAL ANSWER → timeout case
            "",
        );
        let out = build_implementer_summary(&paths, ws, &["alpha".to_string()]);
        assert!(
            out.contains("(executor timed out before final summary; see daemon log for action stream)"),
            "expected timeout fallback in: {out}"
        );
    }

    #[test]
    fn build_implementer_summary_legacy_text_mode_log_still_works() {
        // Operators with `output_format: text` produce the legacy
        // STDOUT/STDERR log shape; the PR comment must still surface
        // the raw stdout (today's behavior preserved).
        let dir = unique_workspace("legacy-shape");
        let ws = dir.path();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        write_fixture_run_log(&paths, ws, "alpha", "p", "LEGACY_STDOUT_CONTENT", "");
        let out = build_implementer_summary(&paths, ws, &["alpha".to_string()]);
        assert!(out.contains("LEGACY_STDOUT_CONTENT"));
    }

    #[test]
    fn truncate_to_fit_appends_marker_when_exceeded() {
        let body = "x".repeat(100_000);
        let out = truncate_to_fit(body, 60_000);
        let marker = "_[summary truncated to fit GitHub comment limit;";
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        write_fixture_run_log(
            &paths,
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

        post_implementer_summary_comment(&paths, &server.url(), ws,
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", mockito::Matcher::Any)
            .expect(0)
            .create_async()
            .await;

        post_implementer_summary_comment(&paths, &server.url(), ws,
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
        paths: &DaemonPaths,
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
            paths,
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
            &std::collections::HashSet::new(),
        )
        .await?;
        Ok(processed)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_increments_failure_counter() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "stuck-change", "fixture reason");
        let executor = AlwaysFailingExecutor;
        // Use a high threshold so a single failure does NOT yet mark
        // perma-stuck; we are asserting only the counter side-effect here.
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, 10).await;
        let state = failure_state::load(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "recovered", "fixture");
        // Pre-populate the failure-state file with a count for this change.
        let _ = failure_state::record_failure(&paths, &ws, "recovered", "earlier fail").unwrap();
        assert!(
            failure_state::load(&paths, &ws).unwrap().entries.contains_key("recovered"),
            "fixture must have a counter entry before the pass"
        );
        let executor = CompletingExecutorWithDiff {
            artifact_name: "RECOVERED.txt".into(),
            artifact_text: "x".into(),
        };
        let processed = run_one_pass_with_threshold(&paths, &ws, &executor, 10)
            .await
            .expect("pass succeeds");
        assert_eq!(processed, vec!["recovered".to_string()]);
        let state = failure_state::load(&paths, &ws).unwrap();
        assert!(
            !state.entries.contains_key("recovered"),
            "archive must clear the failure-state entry"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn threshold_reached_writes_marker_and_excludes_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "doomed", "fixture");
        let executor = AlwaysFailingExecutor;

        // Pass 1: count 1, no marker.
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, 2).await;
        assert!(
            !ws.join("openspec/changes/doomed/.perma-stuck.json").exists(),
            "no marker after first failure"
        );
        assert_eq!(
            queue::list_pending(&paths, &ws).unwrap(),
            vec!["doomed".to_string()],
            "change still pending after one failure"
        );

        // Pass 2: count 2 = threshold → marker written, change excluded.
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, 2).await;
        assert!(
            ws.join("openspec/changes/doomed/.perma-stuck.json").exists(),
            "marker must be written when threshold is reached"
        );
        assert!(
            queue::list_pending(&paths, &ws).unwrap().is_empty(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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

        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, 2).await;
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, 2).await;
        // 2 invocations so far; marker should now exist.
        assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 2);
        let marker = ws.join("openspec/changes/recoverable/.perma-stuck.json");
        assert!(marker.exists(), "marker must be written by pass 2");

        // Pass 3: marker present → excluded → executor NOT invoked.
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, 2).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "executor must not run while marker is present"
        );

        // Operator removes the marker.
        std::fs::remove_file(&marker).unwrap();

        // Pass 4: change is back in pending, executor runs again.
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, 2).await;
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
            &paths,
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
            &std::collections::HashSet::new(),
        )
        .await;
        assert!(result.is_err(), "pre-executor failure must propagate");
        // The per-repo failure-state must remain empty — a transient
        // pre-executor error must not bump the counter.
        let state = failure_state::load(&paths, &ws).unwrap();
        assert!(
            state.entries.is_empty(),
            "transient pre-executor errors must not bump the counter; got: {:?}",
            state.entries
        );
    }

    /// Iteration-level workspace-validity gate (see
    /// `audits-require-valid-workspace`): when `ensure_initialized`
    /// returns Err for the iteration, the audit scheduler must NOT be
    /// invoked. The registry can carry an audit fixture that records
    /// its invocations; after the iteration, the counter must be zero.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_scheduler_not_invoked_when_ensure_initialized_fails() {
        let (_td_paths, _paths) = crate::testing::test_daemon_paths();
        use crate::audits::{
            Audit, AuditContext, AuditOutcome, AuditRegistry, WritePolicy,
        };
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingAudit {
            invocations: Arc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl Audit for CountingAudit {
            fn audit_type(&self) -> &'static str {
                "iter_gate_probe"
            }
            fn description(&self) -> &'static str {
                "test probe for the iteration-level workspace-validity gate"
            }
            fn requires_head_change(&self) -> bool {
                false
            }
            fn write_policy(&self) -> WritePolicy {
                WritePolicy::None
            }
            async fn run(
                &self,
                _ctx: &mut AuditContext<'_>,
            ) -> Result<AuditOutcome> {
                self.invocations.fetch_add(1, Ordering::SeqCst);
                Ok(AuditOutcome::NoFindings)
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
        };
        let github_cfg = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let executor = AlwaysFailingExecutor;

        let invocations = Arc::new(AtomicU32::new(0));
        let probe = CountingAudit {
            invocations: invocations.clone(),
        };
        let registry =
            AuditRegistry::with_audits(vec![Arc::new(probe) as Arc<dyn Audit>]);

        let result = run_pass_through_commits(
            &paths,
            &ws,
            &repo,
            &github_cfg,
            &executor,
            None,
            1,
            u32::MAX,
            &registry,
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
        )
        .await;
        assert!(
            result.is_err(),
            "ensure_initialized failure must propagate; the iteration's audit-scheduler call is unreachable"
        );
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            0,
            "iteration-level gate: audit scheduler must NOT be invoked when ensure_initialized fails"
        );
    }

    /// End-to-end: when the workspace dir exists with partial-clone-shape
    /// content but no `.git/`, the iteration's auto-cleanup + re-clone
    /// runs internally and the iteration's outcome is a normal success
    /// (not Failed). The recovery is invisible to the iteration's
    /// reporting layer — only the WARN log signals it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iteration_auto_recovers_partial_clone_without_failure() {
        use std::process::Command;
        // Set up a real local fixture remote so the re-clone after
        // auto-cleanup actually succeeds (no network access required).
        let dir = tempfile::TempDir::new().unwrap();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        fn run(path: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {}", path.display());
        }
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "user.email", "test@example.com"]);
        run(&remote, &["config", "user.name", "test"]);
        std::fs::write(remote.join("README.md"), "fixture\n").unwrap();
        run(&remote, &["add", "README.md"]);
        run(&remote, &["commit", "-q", "-m", "initial"]);

        // Workspace dir exists with openspec partial-clone artifacts and
        // NO `.git/`. The safety check must pass (nothing operator-
        // meaningful here) and the auto-cleanup must run, then the
        // re-clone from the local fixture remote succeeds.
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(ws.join("openspec/changes/foo")).unwrap();
        std::fs::write(
            ws.join("openspec/changes/foo/proposal.md"),
            "## proposal\n",
        )
        .unwrap();

        let remote_url = remote.to_string_lossy().to_string();
        let repo = RepositoryConfig {
            url: remote_url,
            local_path: Some(ws.clone()),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
        };
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let executor = AlwaysFailingExecutor; // unused: no pending changes after re-clone

        let result = run_pass_through_commits(
            &paths,
            &ws,
            &repo,
            &github_cfg,
            &executor,
            None,
            u32::MAX,
            u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
        )
        .await;
        let (processed, _self_heal) = result.expect(
            "iteration must report normal success after internal auto-cleanup + re-clone; \
             the recovery is invisible to the outcome layer",
        );
        assert!(
            processed.is_empty(),
            "the fixture remote has no pending changes, so nothing should be archived"
        );
        // The workspace is now a fresh clone of the remote — `.git/`
        // present, partial-clone artifact gone, remote's README in place.
        assert!(ws.join(".git").is_dir(), "auto-cleanup + re-clone must produce a valid .git/");
        assert!(
            ws.join("README.md").is_file(),
            "remote's README.md must exist after re-clone"
        );
        assert!(
            !ws.join("openspec/changes/foo/proposal.md").exists(),
            "partial-clone artifact must not survive auto-cleanup"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn perma_stuck_alert_posts_to_chatops() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "no-counter-bump", "fixture");
        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor = SpecRevisionExecutor {
            tasks: fixture_unimpl_tasks(),
            suggestion: "x".into(),
            invocations: invocations.clone(),
        };
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, 1).await;
        // Marker is present.
        assert!(
            ws.join("openspec/changes/no-counter-bump/.needs-spec-revision.json").exists()
        );
        // failure-state must NOT have an entry for this change. The
        // marker handles exclusion; the counter is operator-action
        // territory, not repeat-failure territory.
        let state = failure_state::load(&paths, &ws).unwrap();
        assert!(
            !state.entries.contains_key("no-counter-bump"),
            "SpecNeedsRevision must not write a failure-state entry"
        );
    }

    /// Pre-place a marker → change is excluded from list_pending → the
    /// executor is never invoked for that change.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn change_with_revision_marker_excluded_from_list_pending() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "executor must not be invoked while marker is present"
        );

        // Operator removes the marker.
        std::fs::remove_file(&marker).unwrap();

        // Second pass: change is back in pending, executor runs.
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "executor must run after the operator clears the marker"
        );
    }

    // ============================================================
    // Queue-blocking markers (a18)
    // ============================================================

    /// a18: A perma-stuck change on the queue blocks subsequent pending
    /// changes in the same repo. The pending sibling's executor is never
    /// invoked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn perma_stuck_marker_blocks_subsequent_pending_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "01-broken", "fixture");
        add_committed_change(&ws, "02-sibling", "fixture");
        // Pre-place the perma-stuck marker on the first change.
        std::fs::write(
            ws.join("openspec/changes/01-broken/.perma-stuck.json"),
            r#"{"change":"01-broken","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"Delete this file to retry the change."}"#,
        )
        .unwrap();

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "queue must halt on perma-stuck; pending sibling must not be processed"
        );
        // Sibling is still on disk waiting to run next time.
        assert!(
            ws.join("openspec/changes/02-sibling/proposal.md").exists(),
            "sibling change must remain in the queue"
        );
    }

    /// a18: When `.ignore-for-queue.json` accompanies the blocking
    /// marker, the queue walk RESUMES — the pending sibling IS processed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ignore_for_queue_marker_unblocks_subsequent_pending_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "01-broken", "fixture");
        add_committed_change(&ws, "02-sibling", "fixture");
        // Perma-stuck marker on the first change.
        std::fs::write(
            ws.join("openspec/changes/01-broken/.perma-stuck.json"),
            r#"{"change":"01-broken","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();
        // AND the ignore-for-queue downgrade marker.
        std::fs::write(
            ws.join("openspec/changes/01-broken/.ignore-for-queue.json"),
            r#"{"change":"01-broken","marked_at":"2026-01-01T00:00:00Z","marked_by":"U_OP","reason":"x","operator_action":"x"}"#,
        )
        .unwrap();

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let invoked_with = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct Counter {
            count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, change: &str) -> Result<ExecutorOutcome> {
                self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.seen.lock().unwrap().push(change.to_string());
                Ok(ExecutorOutcome::Completed { final_answer: None })
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }
        let executor = Counter {
            count: invocations.clone(),
            seen: invoked_with.clone(),
        };
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;
        let seen = invoked_with.lock().unwrap().clone();
        assert!(
            !seen.contains(&"01-broken".to_string()),
            "perma-stuck change must still be excluded; got {seen:?}"
        );
        assert!(
            seen.contains(&"02-sibling".to_string()),
            "ignore-for-queue must let the sibling proceed; got {seen:?}"
        );
    }

    /// a18: `.needs-spec-revision.json` continues to block the queue
    /// (unchanged behavior — confirms the new pre-walk gate matches the
    /// existing per-iteration behavior).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_spec_revision_marker_blocks_subsequent_pending_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "01-revision", "fixture");
        add_committed_change(&ws, "02-sibling", "fixture");
        std::fs::write(
            ws.join("openspec/changes/01-revision/.needs-spec-revision.json"),
            r#"{"change":"01-revision","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"x"}"#,
        )
        .unwrap();

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "queue must halt on needs-spec-revision; pending sibling must not be processed"
        );
    }

    /// a18: A workspace with no operator-action markers proceeds normally
    /// — the new pre-walk gate is a no-op.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clean_workspace_processes_pending_changes_normally() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "only-pending", "fixture");

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "clean queue must process the pending change"
        );
    }

    // ============================================================
    // Spec-delta archivability pre-flight (a17)
    // ============================================================

    /// Write a canonical capability spec under
    /// `openspec/specs/<cap>/spec.md` and commit it.
    fn add_committed_canonical_spec(workspace: &Path, capability: &str, body: &str) {
        let dir = workspace.join("openspec/specs").join(capability);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spec.md"), body).unwrap();
        let st = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(workspace)
            .status()
            .unwrap();
        assert!(st.success());
        let st = std::process::Command::new("git")
            .args(["commit", "-q", "-m", &format!("scaffold canonical {capability}")])
            .current_dir(workspace)
            .status()
            .unwrap();
        assert!(st.success());
    }

    /// Write a change with a spec delta block and commit it.
    fn add_committed_change_with_spec(
        workspace: &Path,
        name: &str,
        capability: &str,
        delta_body: &str,
    ) {
        let dir = workspace.join("openspec/changes").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proposal.md"), format!("## Why\nfixture {name}\n")).unwrap();
        std::fs::write(dir.join("tasks.md"), "- [ ] do thing\n").unwrap();
        let spec_dir = dir.join("specs").join(capability);
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::write(spec_dir.join("spec.md"), delta_body).unwrap();
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

    /// A change whose MODIFIED header doesn't match canonical (the a07
    /// failure mode) is caught by the pre-flight: the executor is NOT
    /// invoked, a `.needs-spec-revision.json` marker is written with
    /// `unarchivable_deltas` populated, and the queue walk halts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preflight_catches_a07_style_modified_mismatch() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_canonical_spec(
            &ws,
            "code-reviewer",
            "## Requirements\n\n### Requirement: AI-driven code-quality review\nThe reviewer SHALL accept.\n",
        );
        add_committed_change_with_spec(
            &ws,
            "a07-style-broken",
            "code-reviewer",
            "## MODIFIED Requirements\n\n### Requirement: Reviewer prompt budget is operator-configurable\nThe reviewer SHALL read.\n",
        );
        // A clean change that would run if the pre-flight didn't halt
        // the queue. Its presence verifies the same-iteration halt
        // semantics.
        add_committed_change(&ws, "b-runs-if-not-halted", "fixture");

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Completed { final_answer: None })
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

        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;

        // Executor must NOT have been invoked for the broken change.
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "pre-flight must halt before any executor invocation"
        );

        // Marker is at the expected path with `unarchivable_deltas`
        // populated and an auto-generated suggestion.
        let marker_path =
            ws.join("openspec/changes/a07-style-broken/.needs-spec-revision.json");
        assert!(marker_path.exists(), "marker must be written");
        let raw = std::fs::read_to_string(&marker_path).unwrap();
        assert!(raw.contains("\"unarchivable_deltas\""));
        assert!(raw.contains("\"code-reviewer\""));
        assert!(raw.contains("\"Modified\""));
        assert!(raw.contains("Reviewer prompt budget is operator-configurable"));
        assert!(
            raw.contains("a07-style"),
            "auto-generated reason should mention a07-style class"
        );
        assert!(raw.contains("\"revision_suggestion\""));
        assert!(
            raw.contains("Pre-flight check found"),
            "revision_suggestion should lead with pre-flight prefix"
        );

        // Marker excludes the change from list_pending. The clean
        // second change is in the same iteration; the queue walk halts
        // on the first marker write, so it must NOT have been processed.
        assert!(
            ws.join("openspec/changes/b-runs-if-not-halted").exists(),
            "the clean trailing change must remain in pending"
        );
    }

    /// A change whose spec deltas are clean against canonical passes
    /// pre-flight and reaches the executor — the existing behavior is
    /// preserved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preflight_passes_clean_change_through_to_executor() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_canonical_spec(
            &ws,
            "code-reviewer",
            "## Requirements\n\n### Requirement: AI-driven code-quality review\nThe reviewer SHALL accept.\n",
        );
        add_committed_change_with_spec(
            &ws,
            "clean-modify",
            "code-reviewer",
            "## MODIFIED Requirements\n\n### Requirement: AI-driven code-quality review\nReplacement body SHALL.\n",
        );

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Failed { reason: "fixture".into() })
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

        let _ = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX).await;

        // Executor IS invoked: pre-flight was a no-op for the clean change.
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "clean change must reach the executor"
        );
        // No marker written.
        assert!(
            !ws.join("openspec/changes/clean-modify/.needs-spec-revision.json").exists(),
            "no marker for clean change"
        );
    }

    /// The pre-flight chatops alert fires with body framing the failure
    /// as "unarchivable spec deltas" and enumerating each violation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preflight_failure_posts_chatops_alert_with_deltas_body() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_canonical_spec(
            &ws,
            "code-reviewer",
            "## Requirements\n\n### Requirement: AI-driven code-quality review\nThe reviewer SHALL accept.\n",
        );
        add_committed_change_with_spec(
            &ws,
            "alerted-broken",
            "code-reviewer",
            "## MODIFIED Requirements\n\n### Requirement: Invented Title\nBody SHALL.\n",
        );

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let alert_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex("unarchivable spec deltas".into()))
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

        struct Noop;
        #[async_trait::async_trait]
        impl Executor for Noop {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                unreachable!("pre-flight must short-circuit before executor.run")
            }
            async fn resume(
                &self,
                _h: crate::executor::ResumeHandle,
                _a: &str,
            ) -> Result<ExecutorOutcome> {
                unreachable!()
            }
        }
        let executor = Noop;
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
            &paths,
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
            &std::collections::HashSet::new(),
        )
        .await;

        alert_mock.assert_async().await;
    }

    // ============================================================
    // Change-internal contradiction pre-flight (a19)
    // ============================================================

    /// A test LlmClient that returns a fixed body OR a fixed error.
    struct CcFixedLlm {
        body: std::sync::Mutex<Option<String>>,
        error: std::sync::Mutex<Option<String>>,
    }
    impl CcFixedLlm {
        fn ok(body: &str) -> std::sync::Arc<dyn crate::llm::LlmClient> {
            std::sync::Arc::new(Self {
                body: std::sync::Mutex::new(Some(body.into())),
                error: std::sync::Mutex::new(None),
            })
        }
        fn err(msg: &str) -> std::sync::Arc<dyn crate::llm::LlmClient> {
            std::sync::Arc::new(Self {
                body: std::sync::Mutex::new(None),
                error: std::sync::Mutex::new(Some(msg.into())),
            })
        }
    }
    #[async_trait::async_trait]
    impl crate::llm::LlmClient for CcFixedLlm {
        async fn complete(&self, _prompt: &str) -> Result<String> {
            if let Some(msg) = self.error.lock().unwrap().clone() {
                return Err(anyhow!(msg));
            }
            Ok(self.body.lock().unwrap().clone().unwrap_or_default())
        }
    }

    /// Disabled mode: no scoped context (or explicit `None`) → no LLM
    /// call, executor reached normally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn contradiction_preflight_disabled_proceeds_to_executor() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "plain", "fixture");

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Failed { reason: "fixture".into() })
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
        let fut = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX);
        let _ = crate::preflight::change_contradiction::scope(None, fut).await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "executor must be invoked when contradiction check is disabled"
        );
    }

    /// Enabled mode + LLM returns empty contradictions → executor still
    /// reached (the check is a no-op outcome-wise).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn contradiction_preflight_empty_findings_proceeds_to_executor() {
        let ctx = crate::preflight::change_contradiction::ContradictionCheckCtx {
            llm: CcFixedLlm::ok(r#"{"contradictions": []}"#),
            prompt_template: "TEST_PROMPT".into(),
        };
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        // Change has a spec delta so build_spec_input has something to send,
        // but archivability check passes (no canonical to fight with).
        add_committed_change_with_spec(
            &ws,
            "clean",
            "newcap",
            "## ADDED Requirements\n\n### Requirement: A\nThe system SHALL a.\n",
        );

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Failed { reason: "fixture".into() })
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
        let fut = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX);
        let _ = crate::preflight::change_contradiction::scope(
            Some(std::sync::Arc::new(ctx)),
            fut,
        )
        .await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "executor must be invoked when contradiction check returns empty findings"
        );
    }

    /// Enabled mode + LLM returns contradictions → marker is written,
    /// `unimplementable_tasks` AND `unarchivable_deltas` are empty, AND
    /// the executor is NOT invoked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn contradiction_preflight_findings_write_marker_and_skip_executor() {
        let body = r#"{
          "contradictions": [
            { "requirement_a": "All secrets in env vars",
              "requirement_b": "API key in config.yaml",
              "summary": "A forbids what B requires" },
            { "requirement_a": "Cap operations at 60s",
              "requirement_b": "Run the 5-minute workflow",
              "summary": "B exceeds A's cap" }
          ]
        }"#;
        let ctx = crate::preflight::change_contradiction::ContradictionCheckCtx {
            llm: CcFixedLlm::ok(body),
            prompt_template: "TEST_PROMPT".into(),
        };
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change_with_spec(
            &ws,
            "conflicting",
            "newcap",
            "## ADDED Requirements\n\n### Requirement: All secrets in env vars\nThe system SHALL store secrets only in env vars.\n\n### Requirement: API key in config.yaml\nThe API key SHALL live in config.yaml.\n",
        );

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
        let fut = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX);
        let _ = crate::preflight::change_contradiction::scope(
            Some(std::sync::Arc::new(ctx)),
            fut,
        )
        .await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "executor must NOT be invoked when contradictions are found"
        );

        let marker_path = ws.join("openspec/changes/conflicting/.needs-spec-revision.json");
        assert!(marker_path.exists(), "marker must be written");
        let raw = std::fs::read_to_string(&marker_path).unwrap();
        assert!(
            raw.contains("Pre-flight contradiction check found 2 issue(s)"),
            "revision_suggestion should announce 2 findings; got: {raw}"
        );
        assert!(raw.contains("Requirement A: All secrets in env vars"));
        assert!(raw.contains("Requirement B: API key in config.yaml"));
        assert!(raw.contains("A forbids what B requires"));
        assert!(raw.contains("Requirement A: Cap operations at 60s"));
        assert!(raw.contains("Requirement B: Run the 5-minute workflow"));
        assert!(raw.contains("B exceeds A's cap"));
        assert!(
            raw.contains("clear-revision"),
            "revision_suggestion should name the clear-revision verb; got: {raw}"
        );

        let parsed: crate::spec_revision::SpecNeedsRevisionMarker =
            serde_json::from_str(&raw).unwrap();
        assert!(
            parsed.unimplementable_tasks.is_empty(),
            "unimplementable_tasks must be empty (semantic-not-mechanical case)"
        );
        assert!(
            parsed.unarchivable_deltas.is_empty(),
            "unarchivable_deltas must be empty (semantic-not-mechanical case)"
        );
        assert!(
            !parsed.revision_suggestion.is_empty(),
            "revision_suggestion must carry the narrative"
        );
    }

    /// Enabled mode + LLM transport error → fail open, executor IS
    /// invoked, no marker written.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn contradiction_preflight_llm_error_fails_open() {
        let ctx = crate::preflight::change_contradiction::ContradictionCheckCtx {
            llm: CcFixedLlm::err("simulated transport error"),
            prompt_template: "TEST_PROMPT".into(),
        };
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change_with_spec(
            &ws,
            "transport-err",
            "newcap",
            "## ADDED Requirements\n\n### Requirement: A\nThe system SHALL a.\n",
        );

        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Executor for Counter {
            async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExecutorOutcome::Failed { reason: "fixture".into() })
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
        let fut = run_one_pass_with_threshold(&paths, &ws, &executor, u32::MAX);
        let _ = crate::preflight::change_contradiction::scope(
            Some(std::sync::Arc::new(ctx)),
            fut,
        )
        .await;
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fail-open: executor must be invoked despite the transport error"
        );
        assert!(
            !ws.join("openspec/changes/transport-err/.needs-spec-revision.json")
                .exists(),
            "no marker on fail-open"
        );
    }

    /// Sanity test for the marker's `revision_suggestion` text shape —
    /// uses the public `build_contradiction_revision_suggestion` helper
    /// directly.
    #[test]
    fn revision_suggestion_text_enumerates_findings() {
        let findings = vec![
            crate::preflight::change_contradiction::ContradictionFinding {
                requirement_a: "A1".into(),
                requirement_b: "B1".into(),
                summary: "S1".into(),
            },
            crate::preflight::change_contradiction::ContradictionFinding {
                requirement_a: "A2".into(),
                requirement_b: "B2".into(),
                summary: "S2".into(),
            },
        ];
        let text = build_contradiction_revision_suggestion(&findings);
        assert!(text.contains("Pre-flight contradiction check found 2 issue(s)"));
        assert!(text.contains("1. Requirement A: A1"));
        assert!(text.contains("   Requirement B: B1"));
        assert!(text.contains("   S1"));
        assert!(text.contains("2. Requirement A: A2"));
        assert!(text.contains("   Requirement B: B2"));
        assert!(text.contains("   S2"));
        assert!(text.contains("clear-revision"));
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
        let sr = crate::spec_root::SpecRoot::from_parts(
            ws.to_path_buf(),
            ws.join("openspec"),
            false,
        );
        assert!(tasks_md_all_complete(&sr, "c").unwrap());
    }

    /// `tasks_md_all_complete`: mixed `[x]` and `[ ]` → false.
    #[test]
    fn tasks_md_all_complete_mixed_returns_false() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let tasks = ws.join("openspec/changes/c/tasks.md");
        std::fs::create_dir_all(tasks.parent().unwrap()).unwrap();
        std::fs::write(&tasks, "- [x] done\n- [ ] still open\n").unwrap();
        let sr = crate::spec_root::SpecRoot::from_parts(
            ws.to_path_buf(),
            ws.join("openspec"),
            false,
        );
        assert!(!tasks_md_all_complete(&sr, "c").unwrap());
    }

    /// `tasks_md_all_complete`: every checkbox is `[ ]` → false.
    #[test]
    fn tasks_md_all_complete_all_open_returns_false() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let tasks = ws.join("openspec/changes/c/tasks.md");
        std::fs::create_dir_all(tasks.parent().unwrap()).unwrap();
        std::fs::write(&tasks, "- [ ] a\n- [ ] b\n").unwrap();
        let sr = crate::spec_root::SpecRoot::from_parts(
            ws.to_path_buf(),
            ws.join("openspec"),
            false,
        );
        assert!(!tasks_md_all_complete(&sr, "c").unwrap());
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
        let sr = crate::spec_root::SpecRoot::from_parts(
            ws.to_path_buf(),
            ws.join("openspec"),
            false,
        );
        assert!(!tasks_md_all_complete(&sr, "c").unwrap());
    }

    /// `tasks_md_all_complete`: missing file → Err.
    #[test]
    fn tasks_md_all_complete_missing_file_returns_err() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = dir.path();
        let sr = crate::spec_root::SpecRoot::from_parts(
            ws.to_path_buf(),
            ws.join("openspec"),
            false,
        );
        assert!(tasks_md_all_complete(&sr, "does-not-exist").is_err());
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            run_pass_through_commits(&paths, &ws, &repo, &github_cfg, &executor, None, u32::MAX, u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            run_pass_through_commits(&paths, &ws, &repo, &github_cfg, &executor, None, u32::MAX, u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
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
            queue::list_pending(&paths, &ws).unwrap(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            run_pass_through_commits(&paths, &ws, &repo, &github_cfg, &executor, None, u32::MAX, u32::MAX,
            &crate::audits::AuditRegistry::default(),
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
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
            Ok(ExecutorOutcome::Completed { final_answer: None })
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let still_pending = queue::list_pending(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
        )
        .await
        .expect("pass succeeds");
        assert_eq!(processed, vec!["ch01".to_string()], "cap=1 → one archive");
        let still_pending = queue::list_pending(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let still_pending = queue::list_pending(&paths, &ws).unwrap();
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
        let state = failure_state::load(&paths, &ws).unwrap();
        assert!(
            !state.entries.contains_key("ch03"),
            "ch03 must not have a failure-state entry — walker never reached it; got: {:?}",
            state.entries
        );
    }

    /// halt-queue-walk-on-non-archive: an `Escalated` outcome (AskUser
    /// posted to chatops) halts the walk regardless of cap. Later
    /// pending changes wait for the next iteration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_queue_halts_on_escalated_change() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let still_pending = queue::list_pending(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
        )
        .await;
        assert!(result.is_err(), "recovery failure must surface as Err");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("recovery failed") || err.contains("dirty"),
            "error should name the recovery failure; got: {err}"
        );
        mock.assert_async().await;
        let state = crate::alert_state::AlertState::load_or_default(&paths, &ws);
        assert!(
            state
                .alerts
                .contains_key(&crate::alert_state::AlertCategory::WorkspaceDirtyMidIteration),
            "alert state must record the WorkspaceDirtyMidIteration timestamp"
        );
    }

    /// classify-recovery-failure-mid-iteration: when a recovery failure
    /// classifies as `Permanent` (e.g. "remains dirty after recovery"),
    /// the chatops alert text carries the operator-inspection suffix.
    /// The 24h throttle is unchanged; only the message body differs from
    /// the legacy (no-class) form. Exercises the composition path
    /// directly so the test does not depend on reproducing the rarer
    /// `recheck_filtered` non-empty branch of `run_pass_through_commits`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dirty_workspace_remains_dirty_after_recovery_alerts_with_permanent_suffix() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex(
                    "workspace dirty mid-iteration \\(permanent; skipped until daemon restart\\) \
                     — operator inspection required\\. Latest:"
                        .to_string(),
                ),
            ]))
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
        let err = anyhow!(
            "workspace {} still dirty after recovery; refusing to proceed:\n D foo.rs",
            ws.display()
        );
        crate::alerts::handle_classified_recovery_failure(&paths, &ws,
            "git@github.com:owner/repo.git",
            Some(&chatops_ctx),
            true,
            crate::alert_state::AlertCategory::WorkspaceDirtyMidIteration,
            &err,
            crate::recovery_classification::RecoveryFailureClass::Permanent,
        )
        .await;
        mock.assert_async().await;
    }

    /// classify-recovery-failure-mid-iteration: a transient classification
    /// (network blip, e.g. "Could not resolve host") produces an alert
    /// with the `(transient; retrying)` suffix and otherwise behaves
    /// identically to the pre-classification path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn workspace_init_transient_alert_carries_retrying_suffix() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex(
                "workspace init keeps failing \\(transient; retrying\\)\\. Latest:".to_string(),
            ))
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
        let err = anyhow!("clone failed: fatal: Could not resolve host: github.com");
        let class = crate::recovery_classification::classify_recovery_failure(&err);
        assert_eq!(
            class,
            crate::recovery_classification::RecoveryFailureClass::Transient,
            "fixture should classify as transient"
        );
        crate::alerts::handle_classified_recovery_failure(&paths, &ws,
            "git@github.com:owner/repo.git",
            Some(&chatops_ctx),
            true,
            crate::alert_state::AlertCategory::WorkspaceInitFailure,
            &err,
            class,
        )
        .await;
        mock.assert_async().await;
    }

    /// recover-dirty-workspace-mid-iteration: without chatops the
    /// auto-recovery still runs. Workspace dirty → recovery cleans
    /// → iteration succeeds. No state file is written (no alert posted).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dirty_workspace_recovers_without_chatops() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, _paths) = crate::testing::test_daemon_paths();
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        execute_rebuild_iteration(
            &paths,
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

    // ----- rebuild rename + abort notification tests -----

    fn make_rename_record(
        from: &str,
        to: &str,
        day: &str,
        summary: &str,
    ) -> crate::cli::sync_specs_deps::RenameRecord {
        crate::cli::sync_specs_deps::RenameRecord {
            from: from.into(),
            to: to.into(),
            day: day.into(),
            dependency_summary: summary.into(),
        }
    }

    #[test]
    fn format_renames_notification_single_rename_one_day() {
        let renames = vec![make_rename_record(
            "2026-05-14-self-healing-deployment",
            "2026-05-14-a01-self-healing-deployment",
            "2026-05-14",
            "dependency of `2026-05-14-no-op-completion-is-failure`, which MODIFIES requirement \"Reject archive-only iterations as Failed\" added here",
        )];
        let text = format_rebuild_renames_notification("owner/repo", &renames);
        assert!(text.starts_with("🔀 `owner/repo`: rebuild applied dependency-prefix renames in 1 day-group(s)"));
        assert!(text.contains("2026-05-14:"));
        assert!(text.contains("2026-05-14-self-healing-deployment → 2026-05-14-a01-self-healing-deployment"));
        assert!(text.contains("(dependency of"));
        assert!(text.contains("MODIFIES requirement"));
    }

    #[test]
    fn format_renames_notification_multiple_days_grouped() {
        let renames = vec![
            make_rename_record("2026-05-14-x", "2026-05-14-a01-x", "2026-05-14", "reason A"),
            make_rename_record("2026-05-15-y", "2026-05-15-a01-y", "2026-05-15", "reason B"),
        ];
        let text = format_rebuild_renames_notification("owner/repo", &renames);
        assert!(text.contains("2 day-group(s)"));
        // Both day-group headers appear.
        assert!(text.contains("2026-05-14:"));
        assert!(text.contains("2026-05-15:"));
        // Each rename listed under its day.
        let idx_14 = text.find("2026-05-14:").unwrap();
        let idx_15 = text.find("2026-05-15:").unwrap();
        assert!(idx_14 < idx_15, "days should appear in chronological order");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn renames_notification_fires_when_prefix_renames_present() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("🔀".to_string()),
                mockito::Matcher::Regex("self-healing-deployment".to_string()),
                mockito::Matcher::Regex("a01-self-healing-deployment".to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".into(),
            start_work_enabled: false,
            failure_alerts_enabled: false,
            pr_opened_enabled: false,
        };
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 2,
            successful: 2,
            failed: 0,
            prefix_renames: vec![make_rename_record(
                "2026-05-14-self-healing-deployment",
                "2026-05-14-a01-self-healing-deployment",
                "2026-05-14",
                "dependency of `2026-05-14-no-op-completion-is-failure`",
            )],
            ..Default::default()
        };
        maybe_post_rebuild_renames_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            Some(&ctx),
        )
        .await;
        mock.assert_async().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn renames_notification_noop_when_empty() {
        // No mockito server: any POST would fail to match. The helper
        // must short-circuit when `prefix_renames` is empty.
        let ctx_dummy = None; // also no-op without chatops; double-safety
        let report = crate::cli::sync_specs::RebuildReport::default();
        maybe_post_rebuild_renames_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            ctx_dummy,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn renames_notification_post_failure_does_not_panic() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        // Server returns 500 → post_notification errors. The helper
        // must log+continue (no panic).
        let _mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(500)
            .with_body("nope")
            .expect_at_least(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".into(),
            start_work_enabled: false,
            failure_alerts_enabled: false,
            pr_opened_enabled: false,
        };
        let report = crate::cli::sync_specs::RebuildReport {
            prefix_renames: vec![make_rename_record(
                "2026-05-14-x",
                "2026-05-14-a01-x",
                "2026-05-14",
                "r",
            )],
            ..Default::default()
        };
        maybe_post_rebuild_renames_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            Some(&ctx),
        )
        .await;
        // Survival is the test.
    }

    #[test]
    fn format_abort_notification_cycle_names_both_changes() {
        let reason = crate::cli::sync_specs_deps::RebuildAbortReason::Cycle {
            changes: vec!["2026-05-14-a".into(), "2026-05-14-b".into()],
            requirements: vec![
                ("cap".into(), "Foo".into()),
                ("cap".into(), "Bar".into()),
            ],
        };
        let text = format_rebuild_abort_notification("owner/repo", &reason);
        assert!(text.starts_with("❌ `owner/repo`: rebuild aborted —"));
        assert!(text.contains("2026-05-14-a"));
        assert!(text.contains("2026-05-14-b"));
        assert!(text.contains("No archives were renamed"));
        assert!(text.contains("Operator action required"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_notification_fires_with_cycle() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops_for(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("❌".to_string()),
                mockito::Matcher::Regex("2026-05-14-a".to_string()),
                mockito::Matcher::Regex("2026-05-14-b".to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = ChatOpsContext {
            chatops,
            channel: "C_TEST".into(),
            start_work_enabled: false,
            failure_alerts_enabled: false,
            pr_opened_enabled: false,
        };
        let report = crate::cli::sync_specs::RebuildReport {
            abort_reason: Some(crate::cli::sync_specs_deps::RebuildAbortReason::Cycle {
                changes: vec!["2026-05-14-a".into(), "2026-05-14-b".into()],
                requirements: vec![("cap".into(), "Foo".into())],
            }),
            ..Default::default()
        };
        maybe_post_rebuild_abort_notification(
            &fixture_repo_for_rebuild_test(),
            &report,
            Some(&ctx),
        )
        .await;
        mock.assert_async().await;
    }

    #[test]
    fn pr_body_includes_renames_section_before_canonical_specs() {
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 2,
            successful: 2,
            failed: 0,
            spec_files: vec![crate::cli::sync_specs::SpecFileOutcome {
                path: "openspec/specs/orchestrator/spec.md".into(),
                modified: true,
            }],
            prefix_renames: vec![make_rename_record(
                "2026-05-14-self-healing-deployment",
                "2026-05-14-a01-self-healing-deployment",
                "2026-05-14",
                "dependency of `2026-05-14-no-op-completion-is-failure`",
            )],
            ..Default::default()
        };
        let body = build_rebuild_pr_body(&report);
        let renames_idx = body
            .find("Applied dependency-prefix renames")
            .expect("renames section present");
        let canonical_idx = body
            .find("Canonical spec files")
            .expect("canonical section present");
        assert!(
            renames_idx < canonical_idx,
            "renames section must precede canonical-spec-files section"
        );
        assert!(body.contains("2026-05-14-self-healing-deployment → 2026-05-14-a01-self-healing-deployment"));
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let (_td_paths, paths_inner) = crate::testing::test_daemon_paths();
        let paths_for_run = std::sync::Arc::new(paths_inner);
        let handle = tokio::spawn(async move {
            run(
                paths_for_run,
                repo_holder,
                executor,
                github_holder,
                reviewer_holder,
                chatops_holder,
                1_000_000,
                u32::MAX,
                None,
                0,  // revision_cap: disabled in tests
                60, // startup_jitter_max_secs: large window
                0,  // inter_iteration_jitter_pct: irrelevant
                std::sync::Arc::new(crate::audits::AuditRegistry::default()),
                None,
                std::sync::Arc::new(std::collections::HashMap::new()),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                std::sync::Arc::new(std::sync::Mutex::new(None)),
                std::sync::Arc::new(tokio::sync::Notify::new()),
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
    // read_change_why — archive + active-path fallback
    // ============================================================

    /// Write a fixture active-path proposal.md at
    /// `<workspace>/openspec/changes/<change>/proposal.md`.
    fn write_fixture_active_proposal(workspace: &Path, change: &str, proposal: &str) {
        let dir = workspace.join("openspec/changes").join(change);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proposal.md"), proposal).unwrap();
    }

    #[test]
    #[tracing_test::traced_test]
    fn read_change_why_archive_path_wins_without_warn() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fixture_archive(
            tmp.path(),
            "2026-05-18-fix-thing",
            "## Why\n\nArchive rationale.\n\n## What Changes\n\nx\n",
        );
        let why = read_change_why(tmp.path(), "fix-thing");
        assert!(
            why.as_deref()
                .map(|s| s.contains("Archive rationale."))
                .unwrap_or(false),
            "expected archive why; got: {why:?}"
        );
        assert!(
            !logs_contain("proposal read from active path"),
            "no fallback WARN expected on archive hit"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn read_change_why_falls_back_to_active_with_warn() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No archive fixture.
        write_fixture_active_proposal(
            tmp.path(),
            "fix-thing",
            "## Why\n\nActive-path rationale.\n\n## What Changes\n\nx\n",
        );
        let why = read_change_why(tmp.path(), "fix-thing");
        assert!(
            why.as_deref()
                .map(|s| s.contains("Active-path rationale."))
                .unwrap_or(false),
            "expected active-path why; got: {why:?}"
        );
        assert!(
            logs_contain("proposal read from active path"),
            "expected fallback WARN naming the change"
        );
        assert!(
            logs_contain("fix-thing"),
            "WARN must name the change slug"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn read_change_why_active_without_why_section_returns_none_no_warn() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No archive fixture; active proposal lacks a `## Why` heading.
        write_fixture_active_proposal(
            tmp.path(),
            "fix-thing",
            "## What Changes\n\nstuff but no why\n",
        );
        let why = read_change_why(tmp.path(), "fix-thing");
        assert!(why.is_none(), "expected None; got: {why:?}");
        assert!(
            !logs_contain("proposal read from active path"),
            "WARN should not fire when fallback extracts no content"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn read_change_why_both_paths_missing_returns_none_no_warn() {
        let tmp = tempfile::TempDir::new().unwrap();
        let why = read_change_why(tmp.path(), "fix-thing");
        assert!(why.is_none(), "expected None; got: {why:?}");
        assert!(
            !logs_contain("proposal read from active path"),
            "WARN should not fire when both paths miss"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn read_change_why_archive_present_overrides_active_no_warn() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fixture_archive(
            tmp.path(),
            "2026-05-18-fix-thing",
            "## Why\n\nArchive rationale.\n\n## What Changes\n\nx\n",
        );
        write_fixture_active_proposal(
            tmp.path(),
            "fix-thing",
            "## Why\n\nActive rationale.\n\n## What Changes\n\nx\n",
        );
        let why = read_change_why(tmp.path(), "fix-thing");
        let text = why.expect("expected archive why");
        assert!(
            text.contains("Archive rationale."),
            "archive path must win; got: {text}"
        );
        assert!(
            !text.contains("Active rationale."),
            "active text must not leak through; got: {text}"
        );
        assert!(
            !logs_contain("proposal read from active path"),
            "no WARN expected when archive path wins"
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let state = crate::alert_state::AlertState::load_or_default(&paths, &ws);
        assert!(
            state
                .alerts
                .contains_key(&crate::alert_state::AlertCategory::ArchiveCollision),
            "ArchiveCollision entry must be persisted after the alert post"
        );
        // (d) failure-state counter for `foo` is NOT incremented
        let fs = crate::failure_state::load(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
            &paths,
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
            &std::collections::HashSet::new(),
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
        let fs = crate::failure_state::load(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
                &paths,
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
                &std::collections::HashSet::new(),
            )
            .await
            .expect("iteration succeeds");
            assert!(processed.is_empty(), "no commits in a pure-collision pass");
        }

        mock.assert_async().await;
        let fs = crate::failure_state::load(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "racy", "fixture");

        // Sanity: no failure entries yet.
        let fs0 = crate::failure_state::load(&paths, &ws).unwrap();
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
                Ok(ExecutorOutcome::Completed { final_answer: None })
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
            &paths,
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
            &std::collections::HashSet::new(),
        )
        .await;

        let fs = crate::failure_state::load(&paths, &ws).unwrap();
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
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
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
            &paths,
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
            &std::collections::HashSet::new(),
        )
        .await;
        assert!(result.is_err(), "iteration must surface the recovery failure");

        // The iteration-level alert fired (WorkspaceDirtyMidIteration)…
        let state = crate::alert_state::AlertState::load_or_default(&paths, &ws);
        assert!(
            state
                .alerts
                .contains_key(&crate::alert_state::AlertCategory::WorkspaceDirtyMidIteration),
            "iteration-level failure must route through WorkspaceDirtyMidIteration"
        );
        // …but no per-change counter moved.
        let fs = crate::failure_state::load(&paths, &ws).unwrap();
        assert!(
            fs.entries.is_empty(),
            "iteration-level failure must not increment any per-change counter; got: {:?}",
            fs.entries
        );
    }

    // ----- build_rebuild_pr_body snapshot tests -----

    #[test]
    fn rebuild_pr_body_all_success_omits_failures_and_parenthetical() {
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 3,
            successful: 3,
            failed: 0,
            rolled_back: 0,
            spec_files: vec![crate::cli::sync_specs::SpecFileOutcome {
                path: "openspec/specs/example/spec.md".into(),
                modified: true,
            }],
            ..Default::default()
        };
        let body = build_rebuild_pr_body(&report);
        assert!(
            body.contains("Replayed 3 archived change(s) chronologically; 3 succeeded, 0 failed.\n"),
            "summary line wrong, got:\n{body}"
        );
        assert!(
            !body.contains("rolled back to archive"),
            "no rolled-back parenthetical when zero, got:\n{body}"
        );
        assert!(
            !body.contains("**Failed changes**"),
            "no failures section when zero failures, got:\n{body}"
        );
    }

    #[test]
    fn rebuild_pr_body_partial_failure_with_rollback_includes_count_and_header() {
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 5,
            successful: 3,
            failed: 2,
            rolled_back: 2,
            failures: vec![
                crate::cli::sync_specs::ChangeOutcome {
                    slug: "broken-modified-ref".into(),
                    original_name: "2026-05-15-broken-modified-ref".into(),
                    success: false,
                    failure_reason:
                        "openspec refused to apply: broken-modified-ref MODIFIED failed for header \"### Requirement: X\" - not found; full output: ..."
                            .into(),
                },
                crate::cli::sync_specs::ChangeOutcome {
                    slug: "another-bad".into(),
                    original_name: "2026-05-16-another-bad".into(),
                    success: false,
                    failure_reason: "openspec refused to apply: another-bad MODIFIED failed; full output: ...".into(),
                },
            ],
            spec_files: vec![],
            ..Default::default()
        };
        let body = build_rebuild_pr_body(&report);
        assert!(
            body.contains(
                "Replayed 5 archived change(s) chronologically; 3 succeeded, 2 failed (2 rolled back to archive).\n"
            ),
            "summary line wrong, got:\n{body}"
        );
        assert!(
            body.contains(
                "**Failed changes** (rolled back to archive — see failure reasons below for the openspec output explaining each):\n"
            ),
            "failures-section header wrong, got:\n{body}"
        );
        assert!(
            !body.contains("left at active path"),
            "stale 'left at active path' wording must be gone, got:\n{body}"
        );
        assert!(
            body.contains("- `broken-modified-ref`: openspec refused to apply:"),
            "per-change line missing the headline, got:\n{body}"
        );
    }

    #[test]
    fn rebuild_pr_body_rollback_gap_shows_smaller_rolled_back_count() {
        // 2 failed, only 1 rolled back (the other had a rollback-of-rollback
        // collision and ended up with "rollback ALSO failed" baked into its
        // failure_reason per the atomicity contract).
        let report = crate::cli::sync_specs::RebuildReport {
            processed: 5,
            successful: 3,
            failed: 2,
            rolled_back: 1,
            failures: vec![
                crate::cli::sync_specs::ChangeOutcome {
                    slug: "rolled-back-ok".into(),
                    original_name: "2026-05-15-rolled-back-ok".into(),
                    success: false,
                    failure_reason: "openspec refused to apply: foo; full output: ...".into(),
                },
                crate::cli::sync_specs::ChangeOutcome {
                    slug: "rollback-also-failed".into(),
                    original_name: "2026-05-16-rollback-also-failed".into(),
                    success: false,
                    failure_reason: "openspec refused to apply: bar; rollback ALSO failed: AlreadyExists".into(),
                },
            ],
            spec_files: vec![],
            ..Default::default()
        };
        let body = build_rebuild_pr_body(&report);
        assert!(
            body.contains(
                "Replayed 5 archived change(s) chronologically; 3 succeeded, 2 failed (1 rolled back to archive).\n"
            ),
            "summary line wrong, got:\n{body}"
        );
        let unrolled_line = body
            .lines()
            .find(|l| l.contains("rollback-also-failed"))
            .expect("entry for unrolled-back slug must appear in failures list");
        assert!(
            unrolled_line.contains("rollback ALSO failed"),
            "unrolled-back entry should still surface 'rollback ALSO failed', got: {unrolled_line}"
        );
    }

    // ============================================================
    // partition_and_annotate_reviewer_revisions (cap-budget; verdict-agnostic)
    // ============================================================

    fn make_report(verdict: ReviewVerdict, concerns: Vec<ReviewConcern>) -> ReviewReport {
        ReviewReport {
            verdict,
            markdown: "## Summary\nbase markdown.\n".to_string(),
            concerns,
            per_change_sections: Vec::new(),
        }
    }

    fn revisable_concern(summary: &str, request: &str) -> ReviewConcern {
        ReviewConcern {
            summary: summary.to_string(),
            actionable_request: Some(request.to_string()),
            should_request_revision: true,
            change_slug: None,
        }
    }

    fn commentary_concern(summary: &str) -> ReviewConcern {
        ReviewConcern {
            summary: summary.to_string(),
            actionable_request: None,
            should_request_revision: false,
            change_slug: None,
        }
    }

    #[test]
    fn synthesize_per_change_aggregates_verdict_worst() {
        use crate::code_reviewer::PerChangeReview;
        let per_change = vec![
            PerChangeReview {
                change_slug: "a".into(),
                report: ReviewReport {
                    verdict: ReviewVerdict::Pass,
                    markdown: "ok".into(),
                    concerns: Vec::new(),
                    per_change_sections: Vec::new(),
                },
            },
            PerChangeReview {
                change_slug: "b".into(),
                report: ReviewReport {
                    verdict: ReviewVerdict::Concerns,
                    markdown: "minor".into(),
                    concerns: Vec::new(),
                    per_change_sections: Vec::new(),
                },
            },
            PerChangeReview {
                change_slug: "c".into(),
                report: ReviewReport {
                    verdict: ReviewVerdict::Block,
                    markdown: "bad".into(),
                    concerns: Vec::new(),
                    per_change_sections: Vec::new(),
                },
            },
        ];
        let synth = synthesize_per_change_report(per_change);
        // Worst verdict wins (Block > Concerns > Pass).
        assert_eq!(synth.verdict, ReviewVerdict::Block);
        // Each section preserves the per-change verdict in its body.
        assert_eq!(synth.per_change_sections.len(), 3);
        assert!(synth.per_change_sections[0].markdown.starts_with("VERDICT: Pass"));
        assert!(synth.per_change_sections[1].markdown.starts_with("VERDICT: Concerns"));
        assert!(synth.per_change_sections[2].markdown.starts_with("VERDICT: Block"));
    }

    #[test]
    fn synthesize_per_change_stamps_change_slug_on_concerns() {
        use crate::code_reviewer::PerChangeReview;
        let mut c1 = revisable_concern("c1", "fix");
        c1.change_slug = None; // simulate freshly-parsed (untagged)
        let mut c2 = revisable_concern("c2", "fix");
        c2.change_slug = None;
        let per_change = vec![
            PerChangeReview {
                change_slug: "alpha".into(),
                report: ReviewReport {
                    verdict: ReviewVerdict::Block,
                    markdown: String::new(),
                    concerns: vec![c1.clone()],
                    per_change_sections: Vec::new(),
                },
            },
            PerChangeReview {
                change_slug: "beta".into(),
                report: ReviewReport {
                    verdict: ReviewVerdict::Block,
                    markdown: String::new(),
                    concerns: vec![c2.clone()],
                    per_change_sections: Vec::new(),
                },
            },
        ];
        let synth = synthesize_per_change_report(per_change);
        assert_eq!(synth.concerns.len(), 2);
        assert_eq!(synth.concerns[0].change_slug.as_deref(), Some("alpha"));
        assert_eq!(synth.concerns[1].change_slug.as_deref(), Some("beta"));
    }

    // a46 task 3.3: the verdict is fully decoupled from auto-revise. A
    // `Pass` verdict carrying one actionable concern returns that concern.
    #[test]
    fn partition_pass_verdict_with_actionable_concern_returns_it() {
        let mut r = make_report(
            ReviewVerdict::Pass,
            vec![revisable_concern("a", "fix a")],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 5);
        assert_eq!(taken.len(), 1, "Pass + actionable concern must post");
        assert_eq!(taken[0].summary, "a");
        assert!(
            !r.markdown.contains("cap budget exhausted"),
            "nothing dropped: no annotation"
        );
    }

    // a46 task 3.1: inverted from the old "Concerns posts nothing" test. A
    // `Concerns` verdict with one actionable concern now returns it.
    #[test]
    fn partition_concerns_verdict_with_actionable_concern_returns_it() {
        let mut r = make_report(
            ReviewVerdict::Concerns,
            vec![revisable_concern("a", "fix a")],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 5);
        assert_eq!(taken.len(), 1, "Concerns + actionable concern must post");
        assert_eq!(taken[0].summary, "a");
        assert!(!r.markdown.contains("cap budget exhausted"));
    }

    #[test]
    fn partition_block_under_budget_takes_all_no_dropped() {
        let mut r = make_report(
            ReviewVerdict::Block,
            vec![
                revisable_concern("a", "fix a"),
                revisable_concern("b", "fix b"),
            ],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 5);
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].summary, "a");
        assert_eq!(taken[1].summary, "b");
        assert!(
            !r.markdown.contains("cap budget exhausted"),
            "no annotation when nothing is dropped"
        );
    }

    #[test]
    fn partition_block_over_budget_drops_tail_and_annotates() {
        let mut r = make_report(
            ReviewVerdict::Block,
            vec![
                revisable_concern("a", "fix a"),
                revisable_concern("b", "fix b"),
                revisable_concern("c", "fix c"),
            ],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 2);
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].summary, "a");
        assert_eq!(taken[1].summary, "b");
        assert!(
            r.markdown.contains("(not auto-revised; cap budget exhausted) c"),
            "third concern must be annotated; got:\n{}",
            r.markdown
        );
        assert!(
            !r.markdown.contains("(not auto-revised; cap budget exhausted) a"),
            "kept concerns must NOT appear in the dropped section"
        );
    }

    #[test]
    fn partition_block_zero_budget_drops_everything() {
        let mut r = make_report(
            ReviewVerdict::Block,
            vec![
                revisable_concern("a", "fix a"),
                revisable_concern("b", "fix b"),
            ],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 0);
        assert!(taken.is_empty());
        assert!(r.markdown.contains("(not auto-revised; cap budget exhausted) a"));
        assert!(r.markdown.contains("(not auto-revised; cap budget exhausted) b"));
    }

    /// 3-change per-change pass with 2 revision requests per change AND
    /// `max_auto_revisions_per_pr: 5` → 5 comments posted, 1 annotated as
    /// "(not auto-revised; cap budget exhausted)" inside its OWN per-
    /// change section (not the bundled markdown).
    #[test]
    fn partition_per_change_drops_extra_into_change_section() {
        use crate::code_reviewer::PerChangeSection;
        let mut concern_a1 = revisable_concern("a-c1", "fix a-c1");
        concern_a1.change_slug = Some("change-a".into());
        let mut concern_a2 = revisable_concern("a-c2", "fix a-c2");
        concern_a2.change_slug = Some("change-a".into());
        let mut concern_b1 = revisable_concern("b-c1", "fix b-c1");
        concern_b1.change_slug = Some("change-b".into());
        let mut concern_b2 = revisable_concern("b-c2", "fix b-c2");
        concern_b2.change_slug = Some("change-b".into());
        let mut concern_c1 = revisable_concern("c-c1", "fix c-c1");
        concern_c1.change_slug = Some("change-c".into());
        let mut concern_c2 = revisable_concern("c-c2", "fix c-c2");
        concern_c2.change_slug = Some("change-c".into());

        let mut report = ReviewReport {
            verdict: ReviewVerdict::Block,
            markdown: String::new(),
            concerns: vec![
                concern_a1, concern_a2, concern_b1, concern_b2, concern_c1, concern_c2,
            ],
            per_change_sections: vec![
                PerChangeSection {
                    change_slug: "change-a".into(),
                    markdown: "VERDICT: Block\n\n## Summary\nchange a notes.\n".into(),
                },
                PerChangeSection {
                    change_slug: "change-b".into(),
                    markdown: "VERDICT: Block\n\n## Summary\nchange b notes.\n".into(),
                },
                PerChangeSection {
                    change_slug: "change-c".into(),
                    markdown: "VERDICT: Block\n\n## Summary\nchange c notes.\n".into(),
                },
            ],
        };
        let taken = partition_and_annotate_reviewer_revisions(&mut report, 5);
        // 5 of 6 revisable concerns posted.
        assert_eq!(taken.len(), 5);
        let taken_summaries: Vec<String> = taken.iter().map(|c| c.summary.clone()).collect();
        assert_eq!(
            taken_summaries,
            vec!["a-c1", "a-c2", "b-c1", "b-c2", "c-c1"]
        );
        // The 6th concern (c-c2) is annotated inside change-c's section,
        // NOT in the bundled markdown field.
        let change_c_section = report
            .per_change_sections
            .iter()
            .find(|s| s.change_slug == "change-c")
            .expect("change-c section retained");
        assert!(
            change_c_section.markdown.contains("(not auto-revised; cap budget exhausted) c-c2"),
            "dropped concern must be annotated in its own section; got:\n{}",
            change_c_section.markdown
        );
        // Other sections must NOT carry the dropped annotation.
        for slug in ["change-a", "change-b"] {
            let s = report
                .per_change_sections
                .iter()
                .find(|s| s.change_slug == slug)
                .unwrap();
            assert!(
                !s.markdown.contains("cap budget exhausted"),
                "section {slug} should not be annotated; got:\n{}",
                s.markdown
            );
        }
        // The bundled `markdown` field stays empty in per-change mode.
        assert!(!report.markdown.contains("cap budget exhausted"));
    }

    #[test]
    fn partition_block_with_no_revisable_concerns_returns_empty() {
        let mut r = make_report(
            ReviewVerdict::Block,
            vec![commentary_concern("style nit"), commentary_concern("preference")],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 5);
        assert!(taken.is_empty(), "no should_request_revision => no posts");
        // No "cap budget exhausted" annotation when nothing was revisable
        // to begin with (this is the WARN case, not a budget case).
        assert!(!r.markdown.contains("cap budget exhausted"));
    }

    #[test]
    fn partition_filters_revisable_with_empty_actionable_request() {
        // A concern with should_request_revision: true but no
        // actionable_request body is not a valid revision request — the
        // posting step would have nothing to put after `@<bot> revise`.
        let mut r = make_report(
            ReviewVerdict::Block,
            vec![
                ReviewConcern {
                    summary: "missing-body".into(),
                    actionable_request: Some("   ".into()),
                    should_request_revision: true,
                    change_slug: None,
                },
                revisable_concern("ok", "fix this"),
            ],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 5);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].summary, "ok");
    }

    // a46 task 3.2: the Block path is preserved, not regressed — a `Block`
    // verdict with actionable concerns still returns them.
    #[test]
    fn partition_block_verdict_with_actionable_concerns_still_returns_them() {
        let mut r = make_report(
            ReviewVerdict::Block,
            vec![
                revisable_concern("a", "fix a"),
                revisable_concern("b", "fix b"),
            ],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 5);
        assert_eq!(taken.len(), 2, "Block + actionable concerns must post");
        assert_eq!(taken[0].summary, "a");
        assert_eq!(taken[1].summary, "b");
    }

    // a46 task 3.4: any verdict + zero actionable concerns returns empty.
    // Complements `partition_block_with_no_revisable_concerns_returns_empty`
    // (the Block case) by exercising a non-Block verdict — proving the
    // "no actionable concerns" gate is itself verdict-agnostic. The WARN is
    // logged inside the function on this path (not asserted here, matching
    // the existing no-revisable test's pattern).
    #[test]
    fn partition_concerns_verdict_with_no_actionable_concerns_returns_empty() {
        let mut r = make_report(
            ReviewVerdict::Concerns,
            vec![commentary_concern("style nit"), commentary_concern("preference")],
        );
        let taken = partition_and_annotate_reviewer_revisions(&mut r, 5);
        assert!(taken.is_empty(), "no actionable concerns => no posts under Concerns");
        assert!(!r.markdown.contains("cap budget exhausted"));
    }

    // ============================================================
    // initial_revision_state_at_pr_open (caps are SOURCED, not hardcoded)
    // ============================================================

    /// Build a minimal `CodeReviewer` whose `max_code_reviews_per_pr` is the
    /// given value, for the PR-open state-init tests below.
    fn reviewer_with_review_cap(cap: Option<u32>) -> CodeReviewer {
        use crate::llm::LlmClient;
        use async_trait::async_trait;
        struct NoopClient;
        #[async_trait]
        impl LlmClient for NoopClient {
            async fn complete(&self, _: &str) -> Result<String> {
                Ok(String::new())
            }
        }
        CodeReviewer::new(Box::new(NoopClient), "t".to_string())
            .with_max_code_reviews_per_pr(cap)
    }

    /// Regression guard: the PR-open state init must SOURCE the re-review cap
    /// from the reviewer (NOT hardcode `Some(5)`). With the a47 default
    /// (`reviewer.max_code_reviews_per_pr` unset → `None`), a freshly-opened
    /// PR's state must carry `code_review_cap: None` (unlimited) — otherwise
    /// every daemon-opened PR is silently re-capped at 5 reruns.
    #[test]
    fn pr_open_state_init_sources_unlimited_review_cap_from_reviewer() {
        let reviewer = reviewer_with_review_cap(None);
        let now = chrono::Utc::now();
        let state = initial_revision_state_at_pr_open(
            42,
            "agent-q".to_string(),
            now,
            5,
            Some(&reviewer),
            "deadbeef".to_string(),
        );
        assert_eq!(
            state.code_review_cap, None,
            "unset reviewer cap must yield None (unlimited), not the old hardcoded Some(5)"
        );
        assert_eq!(state.revision_cap, 5, "auto-revision cap is sourced from the passed value");
        assert_eq!(state.auto_revisions_applied, 0);
        assert_eq!(state.code_reviews_applied, 0);
        assert_eq!(state.original_review_head_sha.as_deref(), Some("deadbeef"));
        assert_eq!(state.last_seen_comment_at, now);
    }

    /// When the operator set an opt-in re-review ceiling, the PR-open init
    /// carries it through as `Some(n)`.
    #[test]
    fn pr_open_state_init_sources_set_review_cap_from_reviewer() {
        let reviewer = reviewer_with_review_cap(Some(3));
        let state = initial_revision_state_at_pr_open(
            7,
            "agent-q".to_string(),
            chrono::Utc::now(),
            12,
            Some(&reviewer),
            "cafe".to_string(),
        );
        assert_eq!(state.code_review_cap, Some(3));
        // The auto-revision cap reflects the configured value, not a hardcoded 5.
        assert_eq!(state.revision_cap, 12);
    }

    /// No reviewer configured → the re-review cap is `None` (unlimited).
    #[test]
    fn pr_open_state_init_no_reviewer_yields_unlimited_review_cap() {
        let state = initial_revision_state_at_pr_open(
            9,
            "agent-q".to_string(),
            chrono::Utc::now(),
            0,
            None,
            "f00d".to_string(),
        );
        assert_eq!(state.code_review_cap, None);
        assert_eq!(state.revision_cap, 0);
    }

    // ============================================================
    // post_reviewer_revision_comments (HTTP-shape assertion)
    // ============================================================

    #[tokio::test]
    async fn post_reviewer_revision_comments_posts_marker_and_trigger() {
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        // Expect TWO comment POSTs, each matching the canonical body shape.
        let first = server
            .mock("POST", "/repos/owner/repo/issues/77/comments")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"body":"<!-- reviewer-revision -->\n@my-bot revise fix find_user"}"#
                    .to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":1}"#)
            .expect(1)
            .create_async()
            .await;
        let second = server
            .mock("POST", "/repos/owner/repo/issues/77/comments")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"body":"<!-- reviewer-revision -->\n@my-bot revise restore the audit hook"}"#
                    .to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":2}"#)
            .expect(1)
            .create_async()
            .await;

        let concerns = vec![
            revisable_concern("find_user error context", "fix find_user"),
            revisable_concern("audit hook removed", "restore the audit hook"),
        ];
        post_reviewer_revision_comments(
            &server.url(),
            "owner",
            "repo",
            77,
            &concerns,
            "test-token",
        )
        .await;
        first.assert_async().await;
        second.assert_async().await;
    }

    /// Per-concern POST failures do not abort the loop — every concern
    /// is attempted, and the helper returns normally even when one fails.
    #[tokio::test]
    async fn post_reviewer_revision_comments_continues_on_partial_failure() {
        let mut server = mockito::Server::new_async().await;
        let _user = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"my-bot"}"#)
            .create_async()
            .await;
        // First POST fails 500; second succeeds. The loop must attempt
        // both — verified by `.expect(1)` on each.
        let _fail = server
            .mock("POST", "/repos/owner/repo/issues/88/comments")
            .match_body(mockito::Matcher::Regex("fail this one".to_string()))
            .with_status(500)
            .with_body(r#"{"error":"transient"}"#)
            .expect(1)
            .create_async()
            .await;
        let _ok = server
            .mock("POST", "/repos/owner/repo/issues/88/comments")
            .match_body(mockito::Matcher::Regex("succeed this one".to_string()))
            .with_status(201)
            .with_body(r#"{"id":2}"#)
            .expect(1)
            .create_async()
            .await;

        let concerns = vec![
            revisable_concern("a", "fail this one"),
            revisable_concern("b", "succeed this one"),
        ];
        post_reviewer_revision_comments(
            &server.url(),
            "owner",
            "repo",
            88,
            &concerns,
            "test-token",
        )
        .await;
    }

    // ================================================================
    // a12-changes-have-precedence-over-audits: iteration sequence puts
    // pending queue walk BEFORE the audit phase. Tests in this section
    // exercise the new ordering and the one-iteration delay for
    // audit-generated changes' implementation.
    // ================================================================

    /// Executor that records the order of `run` invocations into a shared
    /// log and writes a unique artifact per change so each invocation
    /// produces a real commit. Lets ordering tests assert the order of
    /// `executor:<change>` and `audit:<type>` entries.
    struct OrderRecordingExecutor {
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl Executor for OrderRecordingExecutor {
        async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
            self.log
                .lock()
                .unwrap()
                .push(format!("executor:{change}"));
            // Produce a deterministic, change-scoped artifact so the
            // commit step has a non-empty diff and the change archives.
            let artifact_dir = workspace.join("openspec/changes").join(change);
            std::fs::create_dir_all(&artifact_dir)?;
            std::fs::write(
                artifact_dir.join("IMPL_NOTES.md"),
                format!("implementation for {change}\n"),
            )?;
            Ok(ExecutorOutcome::Completed { final_answer: None })
        }
        async fn resume(
            &self,
            _h: crate::executor::ResumeHandle,
            _a: &str,
        ) -> Result<ExecutorOutcome> {
            unreachable!()
        }
    }

    /// Test audit fixture used to assert iteration-ordering. Records each
    /// invocation in a shared log under the slug `audit:<audit_type>` and
    /// returns the configured outcome. When the outcome includes new
    /// `openspec/changes/<name>/` directories, the audit commits them on
    /// the agent branch so the post-hoc `OpenSpecOnly` enforcement passes.
    struct OrderRecordingAudit {
        audit_type: &'static str,
        log: Arc<std::sync::Mutex<Vec<String>>>,
        creates_changes: Vec<String>,
        write_policy: crate::audits::WritePolicy,
    }
    #[async_trait::async_trait]
    impl crate::audits::Audit for OrderRecordingAudit {
        fn audit_type(&self) -> &'static str {
            self.audit_type
        }
        fn description(&self) -> &'static str {
            "ordering-test audit fixture"
        }
        fn requires_head_change(&self) -> bool {
            false
        }
        fn write_policy(&self) -> crate::audits::WritePolicy {
            self.write_policy
        }
        async fn run(
            &self,
            ctx: &mut crate::audits::AuditContext<'_>,
        ) -> Result<crate::audits::AuditOutcome> {
            self.log
                .lock()
                .unwrap()
                .push(format!("audit:{}", self.audit_type));
            if self.creates_changes.is_empty() {
                return Ok(crate::audits::AuditOutcome::NoFindings);
            }
            // Create + commit each new openspec/changes/<name>/ directory
            // so the post-hoc `OpenSpecOnly` enforcement sees a clean
            // tree. This mirrors what real spec-writing audits do via
            // the `specs_writing` helper.
            for name in &self.creates_changes {
                let dir = ctx.workspace.join("openspec/changes").join(name);
                std::fs::create_dir_all(&dir)?;
                std::fs::write(
                    dir.join("proposal.md"),
                    format!("## Why\nfixture proposal {name}\n"),
                )?;
                std::fs::write(dir.join("tasks.md"), "- [ ] do thing\n")?;
            }
            let st = std::process::Command::new("git")
                .args(["add", "-A"])
                .current_dir(ctx.workspace)
                .status()?;
            anyhow::ensure!(st.success(), "git add failed in fixture audit");
            let subject = format!(
                "audit: {} proposals ({} change(s))",
                self.audit_type,
                self.creates_changes.len()
            );
            let st = std::process::Command::new("git")
                .args(["commit", "-q", "-m", &subject])
                .current_dir(ctx.workspace)
                .status()?;
            anyhow::ensure!(st.success(), "git commit failed in fixture audit");
            Ok(crate::audits::AuditOutcome::specs_written(
                self.creates_changes.clone(),
            ))
        }
    }

    /// 2.4 (a12): with 2 pending changes AND 1 eligible audit, pending
    /// changes are processed FIRST, then the audit runs. Both phases
    /// commit on agent-q so a single iteration's PR carries both.
    ///
    /// The audit is made eligible via the `queued_audit_types` set
    /// (bypasses cadence) — equivalent for ordering purposes to a
    /// cadence-driven eligible audit, and avoids constructing a full
    /// `AuditsConfig` just to set a cadence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_changes_process_before_audits() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "change-one", "first pending");
        add_committed_change(&ws, "change-two", "second pending");

        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let executor = OrderRecordingExecutor { log: log.clone() };
        let probe = OrderRecordingAudit {
            audit_type: "ordering_probe_a",
            log: log.clone(),
            creates_changes: Vec::new(),
            write_policy: crate::audits::WritePolicy::None,
        };
        let registry = crate::audits::AuditRegistry::with_audits(vec![
            Arc::new(probe) as Arc<dyn crate::audits::Audit>,
        ]);
        let mut queued = std::collections::HashSet::new();
        queued.insert("ordering_probe_a".to_string());

        let test_github = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let (processed, _) = run_pass_through_commits(
            &paths,
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            None,
            u32::MAX,
            u32::MAX,
            &registry,
            None,
            &std::collections::HashMap::new(),
            &queued,
        )
        .await
        .expect("pass succeeds");

        assert_eq!(
            processed.len(),
            2,
            "both pending changes must be processed"
        );

        let entries = log.lock().unwrap().clone();
        // Both executor entries must precede the audit entry.
        let audit_idx = entries
            .iter()
            .position(|e| e == "audit:ordering_probe_a")
            .expect("audit must have run");
        let exec_indices: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.starts_with("executor:").then_some(i))
            .collect();
        assert_eq!(exec_indices.len(), 2, "executor ran for both changes");
        for i in &exec_indices {
            assert!(
                *i < audit_idx,
                "executor invocations must precede the audit invocation; log was: {entries:?}"
            );
        }

        // Both kinds of commits must be present on agent-q (change-one,
        // change-two artifacts + their archive moves; archive landing
        // means the queue is empty).
        assert_eq!(
            queue::list_pending(&paths, &ws).unwrap(),
            Vec::<String>::new(),
            "both pending changes must be archived this iteration"
        );
    }

    /// 2.4 (a12): with 0 pending changes AND 1 eligible audit that
    /// creates 2 new proposals, the audit's creation commit ships in
    /// THIS iteration's PR but the 2 generated changes wait for the
    /// NEXT iteration's `list_pending` (they appear as pending on disk
    /// after the iteration but the executor was never invoked on them).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_generated_changes_wait_one_iteration_for_implementer() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        // No pending changes at iteration start. The audit will create
        // two openspec/changes/<name>/ directories below.

        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let executor = OrderRecordingExecutor { log: log.clone() };
        let probe = OrderRecordingAudit {
            audit_type: "ordering_probe_b",
            log: log.clone(),
            creates_changes: vec![
                "tests-generated-one".to_string(),
                "tests-generated-two".to_string(),
            ],
            write_policy: crate::audits::WritePolicy::OpenSpecOnly,
        };
        let registry = crate::audits::AuditRegistry::with_audits(vec![
            Arc::new(probe) as Arc<dyn crate::audits::Audit>,
        ]);
        let mut queued = std::collections::HashSet::new();
        queued.insert("ordering_probe_b".to_string());

        let pre_main = crate::git::rev_parse(&ws, "main").unwrap();
        let test_github = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let (processed, _) = run_pass_through_commits(
            &paths,
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            None,
            u32::MAX,
            u32::MAX,
            &registry,
            None,
            &std::collections::HashMap::new(),
            &queued,
        )
        .await
        .expect("pass succeeds");

        assert!(
            processed.is_empty(),
            "no pending changes existed at iteration start; the implementer must not have run"
        );

        let entries = log.lock().unwrap().clone();
        assert_eq!(
            entries,
            vec!["audit:ordering_probe_b".to_string()],
            "executor must not have been invoked on the audit's generated changes this iteration"
        );

        // Audit's creation commit must be on agent-q (the head moved
        // past pre_main on the agent branch).
        let agent_sha = crate::git::rev_parse(&ws, "agent-q").unwrap();
        assert_ne!(
            agent_sha, pre_main,
            "audit's commit must land on agent-q so it ships in this iteration's PR"
        );

        // The two new proposals are on disk and now show up in
        // list_pending — so the NEXT iteration's queue walk picks them
        // up.
        let mut pending = queue::list_pending(&paths, &ws).unwrap();
        pending.sort();
        assert_eq!(
            pending,
            vec![
                "tests-generated-one".to_string(),
                "tests-generated-two".to_string()
            ],
            "audit-generated proposals must be pending for the next iteration"
        );
    }

    /// 2.4 (a12): with 1 pending change AND 0 eligible audits, only the
    /// change processes; no audit work happens. (Sanity check that the
    /// reorder did not accidentally couple the two phases.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_only_iteration_runs_no_audit_work() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "only-change", "solo pending");

        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let executor = OrderRecordingExecutor { log: log.clone() };
        // Empty registry: no audits to run, so the scheduler is a no-op.
        let registry = crate::audits::AuditRegistry::default();

        let test_github = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let (processed, _) = run_pass_through_commits(
            &paths,
            &ws,
            &fixture_repo(&ws),
            &test_github,
            &executor,
            None,
            u32::MAX,
            u32::MAX,
            &registry,
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
        )
        .await
        .expect("pass succeeds");

        assert_eq!(processed, vec!["only-change".to_string()]);
        let entries = log.lock().unwrap().clone();
        assert_eq!(entries, vec!["executor:only-change".to_string()]);
    }

    // ================================================================
    // a20a3-audit-only-iterations-push-and-pr: when the queue walk
    // produces zero implementer commits BUT an audit produces proposal
    // commits on the agent branch, the iteration MUST push and open
    // a PR. The pre-fix code's `if processed.is_empty() { return Ok(()) }`
    // guard caused the audit's commits to be silently destroyed by the
    // next iteration's `recreate_branch` step. This test asserts the
    // commit-count gate fires instead so the audit's commits ship.
    // ================================================================

    /// Pure-function test: title shape for an audit-only iteration.
    #[test]
    fn build_audit_only_pr_title_single_audit() {
        let subjects = vec!["audit: security_bug proposals (1 change(s))".to_string()];
        let title = build_audit_only_pr_title(&subjects);
        assert_eq!(title, "audit-only: 1 proposal(s) from security_bug");
    }

    /// Multiple audit commits aggregate counts AND list types in
    /// first-seen order.
    #[test]
    fn build_audit_only_pr_title_aggregates_multiple_audits() {
        let subjects = vec![
            "audit: security_bug proposals (2 change(s))".to_string(),
            "audit: missing_tests proposals (3 change(s))".to_string(),
        ];
        let title = build_audit_only_pr_title(&subjects);
        assert_eq!(
            title,
            "audit-only: 5 proposal(s) from security_bug, missing_tests"
        );
    }

    /// Body explicitly states this is an audit-only PR, lists every
    /// agent-branch commit subject, AND notes that the produced
    /// directories will be picked up by the next iteration.
    #[test]
    fn build_audit_only_pr_body_lists_subjects_and_next_iter_note() {
        let subjects = vec![
            "audit: security_bug proposals (1 change(s))".to_string(),
            "audit: missing_tests proposals (2 change(s))".to_string(),
        ];
        let body = build_audit_only_pr_body(&subjects);
        assert!(
            body.contains("audit-produced proposals only"),
            "body must mark itself as audit-only: {body}"
        );
        assert!(
            body.contains("- audit: security_bug proposals (1 change(s))"),
            "body must list first subject: {body}"
        );
        assert!(
            body.contains("- audit: missing_tests proposals (2 change(s))"),
            "body must list second subject: {body}"
        );
        assert!(
            body.contains("next polling iteration will pick"),
            "body must explain next-iteration pickup: {body}"
        );
    }

    // ================================================================
    // a38: content-aware PR title + body rendering. The renderer
    // partitions commit subjects by message-prefix category AND only
    // emits a section for each non-empty category. The
    // "audit-produced proposals" framing is included ONLY when audit
    // commits exist (regression guard against PR #77's misleading
    // body that claimed audit-produced proposals with zero audit
    // commits in the diff).
    // ================================================================

    #[test]
    fn categorize_commit_subjects_buckets_canonical_shapes() {
        let subjects = vec![
            "audit: security_bug proposals (2 change(s))".to_string(),
            "iteration 2 of a35-foo: refactor scope-overflow".to_string(),
            "archive: a30-bar: implementation already in base".to_string(),
            "a31-baz: do the thing".to_string(),
            "Merge pull request #99 from a-branch".to_string(),
        ];
        let cats = categorize_commit_subjects(&subjects);
        assert_eq!(cats.audit, vec!["audit: security_bug proposals (2 change(s))".to_string()]);
        assert_eq!(
            cats.iteration_wip,
            vec!["iteration 2 of a35-foo: refactor scope-overflow".to_string()]
        );
        assert_eq!(
            cats.implementer,
            vec![
                "archive: a30-bar: implementation already in base".to_string(),
                "a31-baz: do the thing".to_string(),
            ]
        );
        assert_eq!(
            cats.other,
            vec!["Merge pull request #99 from a-branch".to_string()]
        );
    }

    /// All commits are audit → title is the canonical `audit-only:`
    /// shape AND body has the "Audit-produced proposals" section only
    /// (no other sections present, AND the "audit-produced proposals
    /// only" framing IS included).
    #[test]
    fn audit_only_renderer_three_audit_zero_others() {
        let subjects = vec![
            "audit: security_bug proposals (1 change(s))".to_string(),
            "audit: missing_tests proposals (2 change(s))".to_string(),
        ];
        let title = build_audit_only_pr_title(&subjects);
        assert_eq!(
            title,
            "audit-only: 3 proposal(s) from security_bug, missing_tests"
        );
        let body = build_audit_only_pr_body(&subjects);
        assert!(
            body.contains("audit-produced proposals only"),
            "pure-audit body must keep canonical framing: {body}"
        );
        assert!(body.contains("## Audit-produced proposals"));
        assert!(!body.contains("## Iteration WIP"));
        assert!(!body.contains("## Implementer-archived changes"));
        assert!(!body.contains("## Other commits"));
    }

    /// Mixed: audit + iteration WIP. Title uses generic mixed shape
    /// (NOT `audit-only:`), body has both sections.
    #[test]
    fn audit_only_renderer_mixed_audit_and_iteration_wip() {
        let subjects = vec![
            "audit: security_bug proposals (1 change(s))".to_string(),
            "audit: missing_tests proposals (1 change(s))".to_string(),
            "iteration 2 of a35-foo: bar".to_string(),
        ];
        let title = build_audit_only_pr_title(&subjects);
        assert!(
            title.starts_with("agent-q changes: 3 commit(s) across "),
            "mixed-content title must use generic shape: {title}"
        );
        assert!(title.contains("audit"));
        assert!(title.contains("iteration WIP"));
        let body = build_audit_only_pr_body(&subjects);
        assert!(
            !body.contains("audit-produced proposals only"),
            "mixed body must NOT carry the pure-audit framing: {body}"
        );
        assert!(body.contains("## Audit-produced proposals"));
        assert!(body.contains("## Iteration WIP"));
        assert!(body.contains("- audit: security_bug proposals (1 change(s))"));
        assert!(body.contains("- iteration 2 of a35-foo: bar"));
    }

    /// Defensive: zero audit commits AND one iteration WIP (this
    /// combination shouldn't reach the renderer in production —
    /// a38's suppression rule blocks the PR. But the renderer must
    /// still produce a sensible body if invoked directly via test).
    #[test]
    fn audit_only_renderer_zero_audit_iteration_wip_only() {
        let subjects = vec!["iteration 2 of a35-foo: scope-overflow".to_string()];
        let title = build_audit_only_pr_title(&subjects);
        assert!(
            !title.starts_with("audit-only: "),
            "no-audit title must NOT use audit-only shape: {title}"
        );
        let body = build_audit_only_pr_body(&subjects);
        assert!(
            !body.contains("audit-produced proposals only"),
            "no-audit body must NOT claim audit-produced proposals: {body}"
        );
        assert!(
            !body.contains("## Audit-produced proposals"),
            "audit-produced section must be absent when no audit commits: {body}"
        );
        assert!(
            body.contains("## Iteration WIP"),
            "iteration WIP section must be present: {body}"
        );
        assert!(body.contains("- iteration 2 of a35-foo: scope-overflow"));
    }

    /// No subjects readable → fallback title + body that don't claim
    /// audit-produced framing.
    #[test]
    fn audit_only_renderer_empty_subjects() {
        let subjects: Vec<String> = Vec::new();
        let title = build_audit_only_pr_title(&subjects);
        assert_eq!(
            title,
            "audit-only: agent-branch commits without implementer changes"
        );
        let body = build_audit_only_pr_body(&subjects);
        assert!(!body.contains("audit-produced proposals only"));
    }

    /// Regression-prevention end-to-end test for the audit-only PR
    /// flow. Fixture: workspace with no pending changes + a mock audit
    /// that writes a proposal directory AND commits it on the agent
    /// branch. Expected behaviour: the iteration's push reaches the
    /// fixture remote AND the PR-creation HTTP call is invoked with the
    /// audit-only title + body. Against the pre-fix code (early-return
    /// on `processed.is_empty()`), the push step is unreachable and
    /// the mockito mock's `.expect(1)` assertion fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_only_iteration_pushes_and_opens_pr() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        // Rename workspace so its basename is unique vs other tests that
        // share `fixture_workspace_with_remote`'s default name. The
        // busy-marker keys off workspace basename only.
        let ws = {
            let renamed = ws.parent().unwrap().join("workspace-audit-only-pr-test");
            std::fs::rename(&ws, &renamed).unwrap();
            renamed
        };
        // No pending changes at iteration start. The fixture audit
        // creates one openspec/changes/secure-test-1 directory and
        // commits it on the agent branch.

        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let probe = OrderRecordingAudit {
            audit_type: "security_bug",
            log: log.clone(),
            creates_changes: vec!["secure-test-1".to_string()],
            write_policy: crate::audits::WritePolicy::OpenSpecOnly,
        };
        let registry = crate::audits::AuditRegistry::with_audits(vec![
            Arc::new(probe) as Arc<dyn crate::audits::Audit>,
        ]);
        let mut queued = std::collections::HashSet::new();
        queued.insert("security_bug".to_string());

        // Serialize: tests sharing the github-api-base test hook must not
        // race on the process-wide static.
        let _hook_guard = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        // The PR-existence pre-check queries `/pulls` and must return
        // an empty list so the iteration proceeds past the open-PR
        // short-circuit.
        let _list_mock = server
            .mock("GET", mockito::Matcher::Regex("^/repos/owner/fixture/pulls".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;
        // PR-creation: assert head + base + title + body shape.
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::PartialJsonString(
                    r#"{"head":"agent-q","base":"main"}"#.to_string(),
                ),
                mockito::Matcher::Regex("audit-only:".to_string()),
                mockito::Matcher::Regex(
                    "audit-only: 1 proposal\\(s\\) from security_bug".to_string(),
                ),
                mockito::Matcher::Regex(
                    "audit: security_bug proposals \\(1 change\\(s\\)\\)".to_string(),
                ),
            ]))
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"html_url":"https://github.com/owner/fixture/pull/42","number":42}"#,
            )
            .expect(1)
            .create_async()
            .await;

        test_hooks::set_github_api_base(Some(server.url()));

        // Inline token so credential resolution succeeds.
        let github_cfg = GithubConfig {
            token_env: "X".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "inline-test-token".into(),
            }),
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let executor = AlwaysFailingExecutor; // unused: no pending changes

        let stuck_secs = 2400u64;
        let result = execute_one_pass(&paths, &ws,
            &fixture_repo(&ws),
            &executor,
            &github_cfg,
            None,
            None,
            stuck_secs,
            u32::MAX,
            u32::MAX,
            0, // revision_cap: disabled in tests
            &registry,
            None,
            &std::collections::HashMap::new(),
            &queued,
        )
        .await;

        // Clear the test hook BEFORE asserting so a panic in an assertion
        // does not leave the override installed for the next test that
        // happens to acquire the lock.
        test_hooks::set_github_api_base(None);

        result.expect("audit-only iteration must succeed end-to-end");

        // The audit must have run.
        let entries = log.lock().unwrap().clone();
        assert!(
            entries.iter().any(|e| e == "audit:security_bug"),
            "audit must have run; log was: {entries:?}"
        );

        // The PR-creation HTTP call MUST have been invoked: this is the
        // regression assertion. Against the pre-fix code (early-return
        // on `processed.is_empty()`), the iteration returns before the
        // push step AND before this PR call. The mockito `.expect(1)`
        // assertion then fails.
        pr_mock.assert_async().await;

        // Push reached the fixture remote: the audit's commit must be on
        // `origin/agent-q` AND the agent-branch ref on the remote must
        // contain the new proposal directory.
        let remote = _dir.path().join("remote");
        let remote_log = std::process::Command::new("git")
            .args(["log", "agent-q", "--format=%s"])
            .current_dir(&remote)
            .output()
            .expect("git log on remote agent-q");
        assert!(
            remote_log.status.success(),
            "agent-q must exist on the fixture remote after push"
        );
        let subjects = String::from_utf8_lossy(&remote_log.stdout).to_string();
        assert!(
            subjects.contains("audit: security_bug proposals (1 change(s))"),
            "audit's commit subject must be present on remote agent-q; got: {subjects}"
        );
    }

    // ================================================================
    // a38: audit-only-PR suppression on iteration-pending state. When
    // ANY `.iteration-pending.json` marker is present in the workspace,
    // the audit-only-PR path SHALL be suppressed for this iteration —
    // the agent-branch's commits-ahead-of-master include iteration_request
    // WIP that is explicitly not ready to ship. Audit-produced commits
    // (if any) remain on agent-q AND ship in the NEXT iteration's PR
    // after the iteration-pending change concludes.
    // ================================================================

    /// Task 2.3: with one `.iteration-pending.json` marker present AND
    /// an audit that produces a commit on agent-q, the audit-only-PR
    /// path is suppressed: `git::push_force_with_lease` is NOT invoked,
    /// `github::create_pull_request` is NOT invoked, AND the iteration
    /// returns Ok(()) cleanly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_only_pr_suppressed_when_iteration_pending_marker_present() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        // Unique basename so list_pending_changes' state-dir lookup
        // does not collide with other tests' markers.
        let ws = {
            let renamed = ws
                .parent()
                .unwrap()
                .join("workspace-a38-suppression-test");
            std::fs::rename(&ws, &renamed).unwrap();
            renamed
        };
        let basename = ws.file_name().and_then(|s| s.to_str()).unwrap().to_string();

        // Plant the iteration-pending marker BEFORE the iteration runs
        // — this is the regression-shape: a prior iteration left the
        // marker on disk, the current iteration sees iteration-pending
        // state at the post-commit-count gate AND must suppress.
        crate::iteration_pending::write_marker(
            &paths,
            &basename,
            "a35-thread-daemon-paths-globals-removal",
            &crate::iteration_pending::IterationPendingMarker {
                completed_tasks: vec!["1".into()],
                remaining_tasks: vec!["2".into()],
                reason: "prior".into(),
                iteration_number: 2,
            },
        )
        .unwrap();

        // Audit produces one commit on agent-q so commit_count > 0 at
        // the gate (otherwise the iteration short-circuits before the
        // suppression check ever runs).
        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let probe = OrderRecordingAudit {
            audit_type: "security_bug",
            log: log.clone(),
            creates_changes: vec!["secure-test-2".to_string()],
            write_policy: crate::audits::WritePolicy::OpenSpecOnly,
        };
        let registry = crate::audits::AuditRegistry::with_audits(vec![
            Arc::new(probe) as Arc<dyn crate::audits::Audit>,
        ]);
        let mut queued = std::collections::HashSet::new();
        queued.insert("security_bug".to_string());

        // Mockito: GET /pulls is the iteration's open-PR pre-check
        // (runs BEFORE the audit + commit-count gate, must return []
        // so the iteration proceeds far enough to reach the
        // suppression rule). POST /pulls is the PR-creation call —
        // assert .expect(0) since suppression must block it.
        let _hook_guard = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let _list_mock = server
            .mock("GET", mockito::Matcher::Regex("^/repos/owner/fixture/pulls".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .with_status(201)
            .with_body(r#"{"html_url":"x","number":1}"#)
            .expect(0)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let github_cfg = GithubConfig {
            token_env: "X".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "inline-test-token".into(),
            }),
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let executor = AlwaysFailingExecutor; // unused: no pending changes
        let stuck_secs = 2400u64;
        let result = execute_one_pass(&paths, &ws,
            &fixture_repo(&ws),
            &executor,
            &github_cfg,
            None,
            None,
            stuck_secs,
            u32::MAX,
            u32::MAX,
            0,
            &registry,
            None,
            &std::collections::HashMap::new(),
            &queued,
        )
        .await;
        test_hooks::set_github_api_base(None);
        result.expect("suppressed iteration must return Ok(())");

        // Audit ran (so commit_count > 0 at the gate).
        let entries = log.lock().unwrap().clone();
        assert!(
            entries.iter().any(|e| e == "audit:security_bug"),
            "audit must have run; log was: {entries:?}"
        );

        // Audit's commit IS present locally on agent-q (the audit
        // committed during its run; suppression only skips push + PR).
        let local_log = std::process::Command::new("git")
            .args(["log", "agent-q", "--format=%s"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let local_subjects = String::from_utf8_lossy(&local_log.stdout).to_string();
        assert!(
            local_subjects.contains("audit: security_bug proposals (1 change(s))"),
            "audit's commit must be present on LOCAL agent-q; got: {local_subjects}"
        );

        // PR-creation HTTP call was NOT invoked — the POST mock's
        // .expect(0) fires if a regression of the suppression rule
        // tries to open a PR despite the marker.
        pr_mock.assert_async().await;

        // The marker is still present (we never wrote a Completed or
        // SpecNeedsRevision outcome for it).
        assert!(
            crate::iteration_pending::marker_exists(
                &paths,
                &basename,
                "a35-thread-daemon-paths-globals-removal"
            ),
            "iteration-pending marker must persist across the suppressed iteration"
        );
    }

    /// Task 5.3: mixed case — workspace with one iteration-pending
    /// marker present AND the iteration produces audit-shaped commits
    /// AND iteration_request-WIP-shaped commits on agent-q. The
    /// suppression rule fires on ANY marker presence regardless of
    /// commit-message content; no PR opens AND the agent-q commits
    /// remain on disk for the next iteration to ship.
    ///
    /// Note on fixture mechanics: the iteration's `recreate_branch`
    /// step (`git checkout -B agent-q` from base) wipes any
    /// pre-iteration agent-q commits, so the audit fixture below
    /// creates BOTH an audit-shaped AND an iteration-WIP-shaped
    /// commit during its `run()` to put the mixed-content state on
    /// agent-q AFTER recreate. The suppression rule fires after the
    /// audit phase, so this is the same shape the production flow
    /// presents.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_only_pr_suppressed_mixed_audit_and_iteration_wip() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let ws = {
            let renamed = ws
                .parent()
                .unwrap()
                .join("workspace-a38-mixed-test");
            std::fs::rename(&ws, &renamed).unwrap();
            renamed
        };
        let basename = ws.file_name().and_then(|s| s.to_str()).unwrap().to_string();

        // Plant the iteration-pending marker (the prior iteration's
        // `IterationRequested` arm would have written it).
        crate::iteration_pending::write_marker(
            &paths,
            &basename,
            "a35-foo",
            &crate::iteration_pending::IterationPendingMarker {
                completed_tasks: vec!["1".into()],
                remaining_tasks: vec!["2".into()],
                reason: "scope-overflow".into(),
                iteration_number: 2,
            },
        )
        .unwrap();

        // Fixture audit: produces an audit-shaped commit AND an
        // extra iteration-WIP-shaped commit on agent-q so commit_count
        // > 0 AND the agent branch carries mixed content at the time
        // the suppression rule runs.
        struct MixedContentAudit {
            log: Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl crate::audits::Audit for MixedContentAudit {
            fn audit_type(&self) -> &'static str {
                "security_bug"
            }
            fn description(&self) -> &'static str {
                "mixed-content fixture"
            }
            fn requires_head_change(&self) -> bool {
                false
            }
            fn write_policy(&self) -> crate::audits::WritePolicy {
                crate::audits::WritePolicy::OpenSpecOnly
            }
            async fn run(
                &self,
                ctx: &mut crate::audits::AuditContext<'_>,
            ) -> Result<crate::audits::AuditOutcome> {
                self.log.lock().unwrap().push("audit:security_bug".into());
                // Audit-shaped commit: new proposal directory.
                let dir = ctx.workspace.join("openspec/changes/secure-test-3");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(
                    dir.join("proposal.md"),
                    "## Why\nfixture proposal secure-test-3\n",
                )?;
                std::fs::write(dir.join("tasks.md"), "- [ ] do thing\n")?;
                let st = std::process::Command::new("git")
                    .args(["add", "-A"])
                    .current_dir(ctx.workspace)
                    .status()?;
                anyhow::ensure!(st.success(), "git add failed");
                let st = std::process::Command::new("git")
                    .args([
                        "commit",
                        "-q",
                        "-m",
                        "audit: security_bug proposals (1 change(s))",
                    ])
                    .current_dir(ctx.workspace)
                    .status()?;
                anyhow::ensure!(st.success(), "audit commit failed");
                // Iteration-WIP-shaped commit on top so the suppression
                // rule sees mixed commit content at gate time.
                std::fs::write(ctx.workspace.join("wip.txt"), "iteration 2 work\n")?;
                let st = std::process::Command::new("git")
                    .args(["add", "-A"])
                    .current_dir(ctx.workspace)
                    .status()?;
                anyhow::ensure!(st.success(), "git add wip failed");
                let st = std::process::Command::new("git")
                    .args([
                        "commit",
                        "-q",
                        "-m",
                        "iteration 2 of a35-foo: refactor scope-overflow",
                    ])
                    .current_dir(ctx.workspace)
                    .status()?;
                anyhow::ensure!(st.success(), "wip commit failed");
                Ok(crate::audits::AuditOutcome::specs_written(vec![
                    "secure-test-3".to_string(),
                ]))
            }
        }

        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let probe = MixedContentAudit { log: log.clone() };
        let registry = crate::audits::AuditRegistry::with_audits(vec![
            Arc::new(probe) as Arc<dyn crate::audits::Audit>,
        ]);
        let mut queued = std::collections::HashSet::new();
        queued.insert("security_bug".to_string());

        let _hook_guard = test_hooks::lock();
        let mut server = mockito::Server::new_async().await;
        let _list_mock = server
            .mock("GET", mockito::Matcher::Regex("^/repos/owner/fixture/pulls".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;
        let pr_mock = server
            .mock("POST", "/repos/owner/fixture/pulls")
            .with_status(201)
            .with_body(r#"{"html_url":"x","number":1}"#)
            .expect(0)
            .create_async()
            .await;
        test_hooks::set_github_api_base(Some(server.url()));

        let github_cfg = GithubConfig {
            token_env: "X".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "inline-test-token".into(),
            }),
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let executor = AlwaysFailingExecutor;
        let result = execute_one_pass(&paths, &ws,
            &fixture_repo(&ws),
            &executor,
            &github_cfg,
            None,
            None,
            2400u64,
            u32::MAX,
            u32::MAX,
            0,
            &registry,
            None,
            &std::collections::HashMap::new(),
            &queued,
        )
        .await;
        test_hooks::set_github_api_base(None);
        result.expect("mixed-content suppressed iteration must return Ok(())");

        // PR-creation HTTP call NOT invoked (suppression rule fires
        // on ANY marker, mixed commit content doesn't change it).
        pr_mock.assert_async().await;

        // Both commits remain on local agent-q awaiting the next
        // iteration's PR after the iteration-pending change concludes.
        let local_log = std::process::Command::new("git")
            .args(["log", "agent-q", "--format=%s"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let local_subjects = String::from_utf8_lossy(&local_log.stdout).to_string();
        assert!(
            local_subjects.contains("audit: security_bug proposals (1 change(s))"),
            "audit's commit must remain on LOCAL agent-q; got: {local_subjects}"
        );
        assert!(
            local_subjects.contains("iteration 2 of a35-foo: refactor scope-overflow"),
            "iteration-WIP commit must remain on LOCAL agent-q; got: {local_subjects}"
        );
    }

    // -----------------------------------------------------------------
    // a27a1: IterationRequested polling-loop arm tests
    // -----------------------------------------------------------------

    /// build_iteration_commit_subject keeps the subject under 80 chars
    /// AND uses the canonical `iteration N of <change>: <reason>` shape.
    #[test]
    fn build_iteration_commit_subject_truncates_long_reason() {
        let long_reason = "a".repeat(200);
        let s = build_iteration_commit_subject("a30-foo", 2, &long_reason);
        assert!(s.len() <= 80, "subject too long: {} chars: {s}", s.len());
        assert!(s.starts_with("iteration 2 of a30-foo: "), "subject: {s}");
    }

    #[test]
    fn build_iteration_commit_subject_uses_first_line_of_reason() {
        let multi_line = "first line\nsecond line";
        let s = build_iteration_commit_subject("a30-foo", 3, multi_line);
        assert_eq!(s, "iteration 3 of a30-foo: first line");
    }

    /// Task 4.7: integration test. An `IterationRequested` outcome
    /// dispatched through `handle_outcome` (a) commits the workspace
    /// diff to the agent branch with the iteration-numbered subject,
    /// (b) force-pushes the agent branch to the remote, AND (c) writes
    /// `.iteration-pending.json` with the documented payload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iteration_requested_commits_pushes_and_writes_marker() {
        let (dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "a31-bar", "fixture reason");
        // Switch to agent-q so the iteration arm's commit + push hits the
        // expected branch. `recreate_branch` is idempotent here.
        git::recreate_branch(&ws, "agent-q").unwrap();
        // Establish the change's .in-progress lock — the arm must drop
        // it as part of its cleanup.
        queue::lock(&ws, "a31-bar").unwrap();
        // Modify a workspace file so there's a real diff to commit.
        std::fs::write(ws.join("artifact.txt"), "iteration 2 progress\n").unwrap();

        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let outcome = Ok(ExecutorOutcome::IterationRequested {
            completed_tasks: vec!["1".into(), "2".into()],
            remaining_tasks: vec!["3".into()],
            reason: "task 3 needs a refactor I want to plan more carefully".into(),
            iteration_number: 2,
        });
        let step =
            handle_outcome(&paths, &ws, &repo, &github_cfg, None, "a31-bar", outcome).await.unwrap();
        assert!(
            matches!(step, QueueStep::IterationPending),
            "expected IterationPending QueueStep; got {step:?}"
        );

        // (a) The marker was written with the documented payload.
        // It now lives under `<state>/iteration-pending/<basename>/<change>.json`
        // (state_dir, NOT the workspace), so read via DaemonPaths +
        // the workspace's basename — same resolution `handle_outcome`
        // used internally for the write.
        let test_basename = ws.file_name().and_then(|s| s.to_str()).unwrap();
        let marker = crate::iteration_pending::read_marker(
            &paths,
            test_basename,
            "a31-bar",
        )
        .unwrap()
        .unwrap();
        assert_eq!(marker.iteration_number, 2);
        assert_eq!(marker.completed_tasks, vec!["1".to_string(), "2".to_string()]);
        assert_eq!(marker.remaining_tasks, vec!["3".to_string()]);
        assert_eq!(
            marker.reason,
            "task 3 needs a refactor I want to plan more carefully"
        );

        // (b) The agent-branch's HEAD subject is the iteration commit.
        let head_subject = std::process::Command::new("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let subject = String::from_utf8_lossy(&head_subject.stdout).to_string();
        assert!(
            subject.starts_with("iteration 2 of a31-bar:"),
            "agent-branch HEAD subject must be the iteration commit; got: {subject}"
        );

        // (c) The remote's agent-q ref also has the new commit (the
        // arm force-pushed). Look up the remote agent-q's log subjects.
        let remote = dir.path().join("remote");
        let remote_log = std::process::Command::new("git")
            .args(["log", "agent-q", "--format=%s"])
            .current_dir(&remote)
            .output()
            .unwrap();
        let remote_subjects = String::from_utf8_lossy(&remote_log.stdout).to_string();
        assert!(
            remote_log.status.success(),
            "agent-q must exist on the remote after force-push: {remote_subjects}"
        );
        assert!(
            remote_subjects.contains("iteration 2 of a31-bar:"),
            "remote agent-q must contain the iteration commit; got: {remote_subjects}"
        );

        // (d) The .in-progress lock was dropped.
        assert!(
            !ws.join("openspec/changes/a31-bar/.in-progress").exists(),
            ".in-progress must be dropped by the IterationRequested arm"
        );

        // (e) No PR-related routine was called — verify by absence of
        // any HTTP mock setup. This test does NOT set up mockito for
        // GitHub; if the arm tried to open a PR it would fail with a
        // connection error AND fall over before this assertion.
    }

    /// Task 6.5: Completed deletes `.iteration-pending.json`. Self-heal
    /// AND main Completed paths both archive (which itself moves the
    /// directory); the explicit deletion happens BEFORE the archive
    /// rename, so the archived directory does not carry the marker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_arm_deletes_iteration_pending_marker() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "a31-bar", "fixture reason");
        // Establish a stale marker (prior iteration's IterationRequested).
        crate::iteration_pending::write_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "a31-bar",
            &crate::iteration_pending::IterationPendingMarker {
                completed_tasks: vec!["1".into(), "2".into()],
                remaining_tasks: vec!["3".into()],
                reason: "prior reason".into(),
                iteration_number: 2,
            },
        )
        .unwrap();
        queue::lock(&ws, "a31-bar").unwrap();
        // Make a real diff so the Completed arm reaches its archive +
        // commit branch.
        std::fs::write(ws.join("artifact.txt"), "final work\n").unwrap();

        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let outcome = Ok(ExecutorOutcome::Completed { final_answer: None });
        let step =
            handle_outcome(&paths, &ws, &repo, &github_cfg, None, "a31-bar", outcome).await.unwrap();
        assert!(matches!(step, QueueStep::Archived), "expected Archived; got {step:?}");
        // Marker was deleted BEFORE the archive rename, so the
        // archived directory should NOT carry it either.
        assert!(
            !crate::iteration_pending::marker_exists(
                &paths,
                ws.file_name().and_then(|s| s.to_str()).unwrap(),
                "a31-bar",
            ),
            "iteration-pending marker must be removed on Completed"
        );
        // (sanity) the active dir is gone (it was archived).
        assert!(
            !ws.join("openspec/changes/a31-bar").exists(),
            "active change dir must have been archived"
        );
    }

    /// Task 6.5: SpecNeedsRevision deletes `.iteration-pending.json`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spec_needs_revision_arm_deletes_iteration_pending_marker() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "a31-bar", "fixture reason");
        crate::iteration_pending::write_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "a31-bar",
            &crate::iteration_pending::IterationPendingMarker {
                completed_tasks: vec!["1".into()],
                remaining_tasks: vec!["2".into()],
                reason: "prior".into(),
                iteration_number: 2,
            },
        )
        .unwrap();
        queue::lock(&ws, "a31-bar").unwrap();

        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let outcome = Ok(ExecutorOutcome::SpecNeedsRevision {
            unimplementable_tasks: vec![crate::executor::UnimplementableTask {
                task_id: "6.4".into(),
                task_text: "manual".into(),
                reason: "sandbox".into(),
            }],
            revision_suggestion: "do a thing".into(),
        });
        let step =
            handle_outcome(&paths, &ws, &repo, &github_cfg, None, "a31-bar", outcome).await.unwrap();
        assert!(
            matches!(step, QueueStep::SpecRevisionMarked),
            "expected SpecRevisionMarked; got {step:?}"
        );
        assert!(
            !crate::iteration_pending::marker_exists(
                &paths,
                ws.file_name().and_then(|s| s.to_str()).unwrap(),
                "a31-bar",
            ),
            "iteration-pending marker must be removed on SpecNeedsRevision"
        );
    }

    /// Task 6.5: Failed arm leaves `.iteration-pending.json` untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_arm_preserves_iteration_pending_marker() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "a31-bar", "fixture reason");
        let marker = crate::iteration_pending::IterationPendingMarker {
            completed_tasks: vec!["1".into()],
            remaining_tasks: vec!["2".into()],
            reason: "prior".into(),
            iteration_number: 2,
        };
        crate::iteration_pending::write_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "a31-bar",
            &marker,
        )
        .unwrap();
        queue::lock(&ws, "a31-bar").unwrap();

        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let outcome = Ok(ExecutorOutcome::Failed {
            reason: "timeout".into(),
        });
        let step =
            handle_outcome(&paths, &ws, &repo, &github_cfg, None, "a31-bar", outcome).await.unwrap();
        assert!(
            matches!(step, QueueStep::Failed { .. }),
            "expected Failed; got {step:?}"
        );
        let still = crate::iteration_pending::read_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "a31-bar",
        )
        .unwrap()
        .unwrap();
        assert_eq!(still, marker, "Failed must NOT touch the marker");
    }

    /// Task 6.5: AskUser arm leaves `.iteration-pending.json` untouched.
    /// AskUser without chatops_ctx returns `AskUserExitEarly` AND does
    /// NOT touch the marker (the agent's question may resolve into a
    /// continuation; the iteration context stays available).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_user_arm_preserves_iteration_pending_marker() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "a31-bar", "fixture reason");
        let marker = crate::iteration_pending::IterationPendingMarker {
            completed_tasks: vec!["1".into()],
            remaining_tasks: vec!["2".into()],
            reason: "prior".into(),
            iteration_number: 2,
        };
        crate::iteration_pending::write_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "a31-bar",
            &marker,
        )
        .unwrap();
        queue::lock(&ws, "a31-bar").unwrap();

        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let outcome = Ok(ExecutorOutcome::AskUser {
            question: "what next?".into(),
            resume_handle: crate::executor::ResumeHandle(serde_json::json!({})),
        });
        let step =
            handle_outcome(&paths, &ws, &repo, &github_cfg, None, "a31-bar", outcome).await.unwrap();
        assert!(
            matches!(step, QueueStep::AskUserExitEarly),
            "expected AskUserExitEarly (no chatops_ctx); got {step:?}"
        );
        let still = crate::iteration_pending::read_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "a31-bar",
        )
        .unwrap()
        .unwrap();
        assert_eq!(still, marker, "AskUser must NOT touch the marker");
    }

    // ================================================================
    // a39: polling-loop Aborted arm tests
    // ================================================================

    /// Task 4.3: `handle_outcome` receiving `Aborted` from the stub
    /// executor returns `QueueStep::Aborted` AND:
    /// - drops `.in-progress`
    /// - does NOT increment the failure counter
    /// - does NOT write `.perma-stuck.json`
    /// - leaves `.iteration-pending.json` (if any) untouched
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_arm_drops_lock_and_skips_counter() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "a31-bar", "fixture reason");
        // Establish .in-progress so the arm has something to unlock.
        queue::lock(&ws, "a31-bar").unwrap();
        // Plant an iteration-pending marker — the Aborted arm must
        // leave it in place (mirrors the Failed-arm preservation
        // requirement so the next iteration's continuation context
        // survives a daemon restart mid-iteration).
        let basename = ws.file_name().and_then(|s| s.to_str()).unwrap().to_string();
        let marker = crate::iteration_pending::IterationPendingMarker {
            completed_tasks: vec!["1".into()],
            remaining_tasks: vec!["2".into()],
            reason: "prior".into(),
            iteration_number: 2,
        };
        crate::iteration_pending::write_marker(&paths, &basename, "a31-bar", &marker)
            .unwrap();

        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let outcome = Ok(ExecutorOutcome::Aborted {
            reason: "daemon shutdown (SIGTERM cascade)".into(),
        });
        let step = handle_outcome(
            &paths,
            &ws,
            &repo,
            &github_cfg,
            None,
            "a31-bar",
            outcome,
        )
        .await
        .unwrap();
        assert!(
            matches!(step, QueueStep::Aborted),
            "expected QueueStep::Aborted; got {step:?}"
        );

        // (a) .in-progress dropped.
        assert!(
            !ws.join("openspec/changes/a31-bar/.in-progress").exists(),
            ".in-progress must be dropped by the Aborted arm"
        );

        // (b) The failure counter for the change is NOT recorded.
        let state = crate::failure_state::load(&paths, &ws).unwrap();
        assert!(
            !state.entries.contains_key("a31-bar"),
            "Aborted must NOT increment the failure counter; got {state:?}"
        );

        // (c) .perma-stuck.json is NOT written.
        assert!(
            !crate::perma_stuck::marker_exists(&ws, "a31-bar"),
            ".perma-stuck.json must NOT be written for Aborted"
        );

        // (d) The iteration-pending marker is preserved.
        let still = crate::iteration_pending::read_marker(&paths, &basename, "a31-bar")
            .unwrap()
            .unwrap();
        assert_eq!(still, marker, "Aborted must NOT touch the iteration-pending marker");
    }

    /// Task 4.4 (integration): two consecutive `Aborted` outcomes for
    /// the same change do NOT trigger perma-stuck (counter stays at 0;
    /// marker absent). This is the regression assertion for the
    /// production scenario: operator restarts the daemon twice in a
    /// row mid-iteration, each restart triggers the SIGTERM cascade,
    /// AND the change must not perma-stuck on either occurrence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_consecutive_aborted_outcomes_do_not_perma_stuck() {
        let (_dir, ws) = fixture_workspace_with_remote();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        add_committed_change(&ws, "a31-bar", "fixture reason");

        let repo = fixture_repo(&ws);
        let github_cfg = GithubConfig {
            token_env: "DOES_NOT_EXIST".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        for iteration in 0..2u32 {
            // Re-establish the .in-progress lock each pass; production
            // re-locks per pending iteration.
            queue::lock(&ws, "a31-bar").unwrap();
            let outcome = Ok(ExecutorOutcome::Aborted {
                reason: "daemon shutdown (SIGTERM cascade)".into(),
            });
            let step = handle_outcome(
                &paths,
                &ws,
                &repo,
                &github_cfg,
                None,
                "a31-bar",
                outcome,
            )
            .await
            .unwrap_or_else(|e| {
                panic!("Aborted arm errored on pass {iteration}: {e:#}")
            });
            assert!(
                matches!(step, QueueStep::Aborted),
                "pass {iteration}: expected QueueStep::Aborted; got {step:?}"
            );

            let state = crate::failure_state::load(&paths, &ws).unwrap();
            assert!(
                !state.entries.contains_key("a31-bar"),
                "pass {iteration}: counter must remain absent after Aborted; got {state:?}"
            );
            assert!(
                !crate::perma_stuck::marker_exists(&ws, "a31-bar"),
                "pass {iteration}: .perma-stuck.json must NOT be written"
            );
        }
    }
}
