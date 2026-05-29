# Tasks

## 1. Alert-state extension

- [ ] 1.1 Add `revise_notifications: HashMap<String, ReviseNotificationEntry>` field to `AlertState` (`autocoder/src/alert_state.rs`) with `#[serde(default)]` for backward compatibility.
- [ ] 1.2 Define `ReviseNotificationEntry { posted_picked_up_at: Option<DateTime<Utc>>, posted_succeeded_at: Option<DateTime<Utc>>, posted_failed_at: Option<DateTime<Utc>> }` with serde round-trip.
- [ ] 1.3 Add `record_revise_notification(&mut self, comment_id: &str, kind: ReviseNotificationKind, when: DateTime<Utc>)` accessor that inserts-or-updates the appropriate field. `ReviseNotificationKind` enum: `PickedUp`, `Succeeded`, `Failed`.
- [ ] 1.4 Add `revise_notification_already_posted(&self, comment_id: &str, kind: ReviseNotificationKind) -> bool` accessor that returns `true` when the corresponding field is `Some(_)`.
- [ ] 1.5 Unit-test round-trip: an `AlertState` with `revise_notifications` populated serializes AND deserializes byte-for-byte.
- [ ] 1.6 Unit-test backward-compat: an `AlertState` JSON without the `revise_notifications` field loads cleanly with the field defaulting to an empty map.

## 2. Notification helpers in polling_loop

- [ ] 2.1 Add `maybe_post_revise_picked_up_alert(chatops_ctx, repo, pr_number, pr_url, change_list_summary, operator_comment_quote, comment_id)` following the existing `maybe_post_*_alert` shape. The helper:
  - Returns early when `chatops_ctx` is `None` OR `failure_alerts_enabled` is `false`.
  - Loads alert-state, checks `revise_notification_already_posted(comment_id, PickedUp)`, returns early if `true`.
  - Composes the canonical "picked up" text per the chatops-manager capability deltas.
  - Calls `ctx.chatops.post_notification(&ctx.channel, &text).await`; on failure logs `tracing::warn!` AND returns without updating state.
  - On success, records `posted_picked_up_at` AND saves alert-state.
- [ ] 2.2 Add `maybe_post_revise_succeeded_alert(chatops_ctx, repo, pr_number, pr_url, change_list_summary, agent_branch, duration, comment_id)` mirroring the picked-up shape with `Succeeded` kind. Duration is formatted using the existing duration-rendering helper used by status replies.
- [ ] 2.3 Add `maybe_post_revise_failed_alert(chatops_ctx, repo, pr_number, pr_url, reason, comment_id)` mirroring the picked-up shape with `Failed` kind. When `reason.len() > 35_000`, the helper switches to the threaded-notification API AND truncates per the existing canonical "Thread body truncates at 35,000 characters" requirement.
- [ ] 2.4 Unit-test each helper:
  - Posts when state is clean AND toggle is on.
  - Skips when toggle is off (no post, no state update).
  - Skips when state shows already-posted (no post, no state update).
  - Updates state after successful post.
  - Does NOT update state when post fails (so a future retry can succeed).
- [ ] 2.5 Unit-test the threaded path for the failed helper: a 40,000-character reason produces a threaded post AND a 35,000-character truncated body.

## 3. Dispatch sites in revisions

- [ ] 3.1 In `revisions::process_one_pr`, BEFORE the call to `executor.run_revision(...).await`, invoke `maybe_post_revise_picked_up_alert(...)`. Pass the operator's `revision_text` (the post-verb body) as the `operator_comment_quote` argument; truncate to 80 chars with a trailing `…` if longer.
- [ ] 3.2 AFTER `executor.run_revision(...).await` returns, branch on the outcome:
  - `Ok(ExecutorOutcome::Completed { .. })`: after the commit + push step succeeds, invoke `maybe_post_revise_succeeded_alert(...)` with the iteration's duration.
  - `Ok(ExecutorOutcome::Completed { .. })` but the commit OR push step fails: invoke `maybe_post_revise_failed_alert(...)` with the step-failure reason (e.g. "push to agent-q failed: <error>").
  - `Ok(ExecutorOutcome::Failed { reason })`: invoke `maybe_post_revise_failed_alert(...)` with the reason verbatim.
  - `Ok(ExecutorOutcome::SpecNeedsRevision { .. })`: invoke `maybe_post_revise_failed_alert(...)` with reason `"spec needs revision (see PR comment for details)"`. The existing `maybe_post_spec_revision_alert` continues to fire from its own canonical path; the revise-lifecycle "failed" notification provides the iteration-framing context the existing alert lacks.
  - `Ok(ExecutorOutcome::IterationRequested { .. })` (after `a27a1`): invoke `maybe_post_revise_succeeded_alert(...)` with the duration of THIS iteration AND the agent_branch — the iteration sequence continues; the operator sees "applied" framing because the revision DID make progress AND was pushed. (When the final iteration concludes via a subsequent Completed, that iteration's notification posts independently.)
  - `Ok(ExecutorOutcome::AskUser { .. })`: NO notification. The existing AskUser notification path (separate from this change's scope) covers operator engagement here.
  - `Err(e)`: invoke `maybe_post_revise_failed_alert(...)` with reason `format!("executor error: {e:#}")`.
- [ ] 3.3 The `process_one_pr` function needs access to `chatops_ctx`. Thread it as a new parameter from the polling-loop caller. Existing call sites either have a `ChatOpsContext` available OR can pass `None` (e.g. test fixtures).
- [ ] 3.4 Unit-test: `process_one_pr` with a stub executor returning `Completed` AND a captured chatops backend asserts that BOTH `PickedUp` AND `Succeeded` notifications were posted (in that order).
- [ ] 3.5 Unit-test: `process_one_pr` with a stub executor returning `Failed { reason: "timeout" }` asserts BOTH `PickedUp` AND `Failed` notifications were posted with the reason text.
- [ ] 3.6 Unit-test: `process_one_pr` with `chatops_ctx: None` runs to completion without panic AND skips all notifications.

## 4. Validation

- [ ] 4.1 `cargo test` passes.
- [ ] 4.2 `cargo clippy` produces no NEW warnings against the existing baseline.
- [ ] 4.3 `openspec validate a31-chatops-revise-lifecycle-notifications --strict` passes.
