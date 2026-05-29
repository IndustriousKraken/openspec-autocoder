## Why

Operator-initiated revisions (`@<bot> revise <text>` PR comments on autocoder-opened PRs) are picked up by the revisions dispatcher AND processed by `Executor::run_revision`. The dispatcher's status query (`@<bot> status <repo>`) accurately reports `currently: busy (stage=executor, started Nm ago)`, AND the PR receives an `## Agent implementation notes` comment when the revision iteration concludes. But the chat channel — where most operators are watching — gets nothing AT ALL during the revise lifecycle:

- **No "received" acknowledgment.** An operator who posts a revise comment AND walks away has no chat-side signal that the comment was seen, parsed, dispatched, OR is running. The next `status` query confirms it but requires manual prompting.
- **No "done" signal.** The PR-comment side does eventually update (force-pushed agent branch, optional reviewer-block follow-ons), but operators monitoring chat AND only chat have no closure signal — they have to context-switch to GitHub to see the result.
- **No "failed" signal.** Revision failures (timeout, unparseable subprocess outcome, executor error) are surfaced in `journalctl` AND on the per-change run log, but only become operator-visible via a delayed status query OR a deep log dive. The chat-side coverage that exists for PR-side perma-stuck AND spec-needs-revision events does NOT cover revise-lifecycle failures.

The gap is structural, not behavioral: the revise pipeline works correctly; it just doesn't TELL the operator it's working. Sibling features in the chatops layer already cover comparable operator-action events — `maybe_post_spec_revision_alert` announces `outcome_spec_needs_revision` results, `post_perma_stuck_alert` announces perma-stuck markers, audit findings post to chat when long enough to thread. Revise lifecycle is the missing piece.

This change adds three notification points along the existing revise dispatcher's code path, routed through the existing chatops channel resolution (per-repo `chatops_channel_id` override AND fall back to `chatops.default_channel_id`). Each notification fires AT MOST ONCE per operator-comment-trigger via a new deduplication map in the alert-state file (consistent with the existing throttle-keyed-by-change pattern).

## What Changes

**Three new chatops notifications, each fired at a specific point in the revise lifecycle:**

1. **Revise picked up** — fires when the revisions dispatcher decides to enqueue a revision for an operator comment. Posted BEFORE the executor subprocess launches so the operator gets near-immediate acknowledgment.
2. **Revise succeeded** — fires after the executor returns `Completed`, autocoder commits the revision diff, AND the agent branch force-push succeeds. Posted as the last step of a successful revise iteration.
3. **Revise failed** — fires when the executor returns `Failed`, `SpecNeedsRevision` (existing alert path remains AS-IS for the per-marker case; this notification covers the revise-iteration framing specifically), OR the commit/push step fails. Posted with the canonical reason text from the outcome OR step that failed.

**Notification content (canonical text shape):**

- **Picked up** (single-line):
  ```
  🔧 `<repo_url>`: revising PR #<num> (`<first_change>` +N more): "<truncated 80-char operator-comment quote>"
  <pr_url>
  ```
  The change-name list mirrors today's PR-title shape (`first_change +N more` when the bundled iteration spans multiple changes). The comment quote is the operator's `revise <text>` content after the verb, truncated at 80 characters with a trailing `…` if longer.

- **Succeeded** (single-line):
  ```
  ✓ `<repo_url>`: revision applied to PR #<num> (`<first_change>` +N more) — force-pushed `<agent_branch>` (took Nm Ns)
  <pr_url>
  ```
  Duration is human-formatted using the existing duration-rendering helper used by the `status` reply.

- **Failed** (single-line OR threaded depending on length, mirroring audit-finding behavior):
  ```
  ✗ `<repo_url>`: revision failed on PR #<num>: <reason>
  <pr_url>
  ```
  When `<reason>` is long enough to push the message past the `audit findings threading threshold` (per the existing canonical "Thread body truncates at 35,000 characters" requirement), the failed notification SHALL use the threaded-notification path AND truncate the reason body at 35,000 characters with the existing pointer-to-daemon-log tail.

**Routing:** all three notifications use the existing chatops channel resolution (per-repo `chatops_channel_id` override; fallback to `chatops.default_channel_id`). When no chatops backend is configured OR `ChatOpsContext` is `None`, the notifications are silently skipped (consistent with every other chatops notification in the daemon).

**Toggle:** all three notifications respect `ctx.failure_alerts_enabled` (the existing toggle that gates `maybe_post_spec_revision_alert` AND `post_perma_stuck_alert`). When the toggle is `false`, NONE of the three fire. The "picked up" AND "succeeded" notifications are arguably non-failure signals, but bundling them under the same toggle is consistent with the existing pattern (operators who want zero chatops noise turn this off; operators who want full visibility turn it on).

**Deduplication:** the alert-state file gains a new `revise_notifications` map keyed by `<comment_id>` → `{ posted_picked_up_at, posted_succeeded_at, posted_failed_at }`. Each notification SHALL check the map BEFORE posting AND SHALL update the map AFTER successful posting. A second iteration on the same comment (e.g. autocoder restarts mid-revision AND re-processes the comment) does NOT re-post the "picked up" notification. The "succeeded" / "failed" notifications fire on the iteration's terminal outcome, so they post at most once per comment per outcome class.

**No new config knobs.** The behavior is gated by existing toggles (`failure_alerts_enabled`, `chatops_channel_id`/`default_channel_id`). Operators who already configured chatops get the new notifications by default after deploy.

## Impact

- **Affected specs:**
  - `chatops-manager` — ADDED requirement for the three revise-lifecycle notifications, naming the canonical text shapes, the channel-resolution rule, the toggle behavior, AND the deduplication-via-alert-state-map mechanism.
- **Affected code:**
  - `autocoder/src/revisions.rs` — `process_one_pr` gains a "picked up" notification call BEFORE `executor.run_revision(...).await`. The terminal-outcome dispatch gains "succeeded" / "failed" notification calls based on the returned `ExecutorOutcome` AND the subsequent commit + push step's result.
  - `autocoder/src/polling_loop.rs` — three new helper functions (`maybe_post_revise_picked_up_alert`, `maybe_post_revise_succeeded_alert`, `maybe_post_revise_failed_alert`) following the established `maybe_post_*_alert` pattern AND the `ChatOpsContext` accessor convention.
  - `autocoder/src/alert_state.rs` — new `revise_notifications: HashMap<String, ReviseNotificationEntry>` field on `AlertState` with serde-default for backward compatibility (existing alert-state files without this map load cleanly as empty).
  - `autocoder/src/alert_state_migration.rs` — no migration required (the new field has a serde default).
- **Operator-visible behavior:**
  - Chat channels receive three new notifications per operator-comment-triggered revise iteration: one at start, one at end. Total: up to 2 chat lines per revise comment (the "picked up" AND one of "succeeded"/"failed"). Operators who post many revise comments see proportional chat volume.
  - Operators with `failure_alerts_enabled: false` see no new chat activity (the toggle gates all three).
  - PR-comment behavior is UNCHANGED. The `## Agent implementation notes` PR comment continues to post on the same terminal-outcome path; the new notifications are additive AND chat-side.
- **Backward compatibility:** alert-state files written by older daemons (no `revise_notifications` field) load via serde-default as empty maps. Older daemons reading alert-state files written by this version ignore the unknown field (existing `#[serde(default)]` behavior across the AlertState struct; no migration required).
- **Dependencies:** none. This change is independent of the `a27a*` outcome-tools stack AND can land before, after, OR concurrently with that work.
- **Acceptance:** `cargo test` passes; `openspec validate a31-chatops-revise-lifecycle-notifications --strict` passes. Tests:
  - "Picked up" notification fires when the dispatcher enqueues a revision AND the `revise_notifications` map has no prior entry for the comment_id.
  - "Picked up" notification is skipped (no double-post) when the map already records `posted_picked_up_at` for the same comment_id.
  - "Succeeded" notification fires after `Completed` outcome AND a successful commit + push step.
  - "Failed" notification fires for `Failed`, `SpecNeedsRevision`, AND commit/push-failure paths, with the appropriate reason text.
  - All three notifications respect `failure_alerts_enabled: false` (no post; map is not updated).
  - All three notifications respect the per-repo `chatops_channel_id` override (channel resolution uses the existing helper).
  - Threaded path for long failure reasons: a reason >35,000 characters posts via the threaded-notification API AND truncates per the existing canonical requirement.
  - Alert-state forward/backward compatibility: an alert-state file without `revise_notifications` loads cleanly; an alert-state file with `revise_notifications` is written cleanly across daemon restarts.
