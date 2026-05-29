## ADDED Requirements

### Requirement: Operator-initiated revise iterations post lifecycle notifications to chatops

When the revisions dispatcher (`autocoder/src/revisions.rs::process_one_pr`) processes an operator-posted `@<bot> revise <text>` PR comment, the daemon SHALL post chatops notifications at three points in the iteration lifecycle:

1. **Revise picked up.** Posted BEFORE the executor subprocess launches (`executor.run_revision(...).await`). Signals to the operator that the comment was parsed AND dispatched.
2. **Revise succeeded.** Posted AFTER the executor returns `Completed` AND the commit + force-push to the agent branch both succeed. Signals successful completion.
3. **Revise failed.** Posted AFTER the executor returns `Failed`, `SpecNeedsRevision`, OR an error AND/OR the commit + push step fails. Signals the iteration did NOT complete cleanly.

Each notification SHALL be routed through the existing chatops channel resolution: the per-repo `chatops_channel_id` override when set, falling back to `chatops.default_channel_id`. When no chatops backend is configured (`ChatOpsContext` is `None`), all three notifications SHALL be silently skipped.

Each notification SHALL respect the `failure_alerts_enabled` toggle (the same toggle that gates `maybe_post_spec_revision_alert` AND `post_perma_stuck_alert`). When the toggle is `false`, NONE of the three notifications fire. The toggle gates the entire revise-lifecycle notification set as a unit — operators who want zero chatops noise turn it off; operators who want full visibility turn it on.

Each notification SHALL be deduplicated via the alert-state file's `revise_notifications` map keyed by the operator comment's GitHub `comment_id`. The map's per-comment entry tracks `posted_picked_up_at`, `posted_succeeded_at`, AND `posted_failed_at` timestamps. Each notification SHALL check the corresponding timestamp BEFORE posting; when non-`None`, the notification SHALL be skipped. After a successful post, the helper SHALL update the timestamp AND save the alert-state file. A failed post (chatops backend error) SHALL NOT update the timestamp so a subsequent iteration can retry.

**Canonical notification text shapes:**

- **Revise picked up:**

  ```
  🔧 `<repo_url>`: revising PR #<num> (`<first_change>` +<N> more): "<operator_comment_quote>"
  <pr_url>
  ```

  where `<first_change>` is the first change name in the PR's bundled iteration AND `<N>` is one less than the total number of changes (`+0 more` is omitted; `+1 more` AND higher are included). `<operator_comment_quote>` is the operator's post-verb revise text, truncated at 80 characters with a trailing `…` if longer. The PR URL appears on its own line so the chatops backend's URL-preview behavior unfurls it (where supported).

- **Revise succeeded:**

  ```
  ✓ `<repo_url>`: revision applied to PR #<num> (`<first_change>` +<N> more) — force-pushed `<agent_branch>` (took <human_duration>)
  <pr_url>
  ```

  where `<human_duration>` uses the existing duration-rendering helper (e.g. `38m 12s`, `1h 4m`).

- **Revise failed:**

  ```
  ✗ `<repo_url>`: revision failed on PR #<num>: <reason>
  <pr_url>
  ```

  where `<reason>` is the canonical reason text from the failed outcome OR step. When the reason is long (>35,000 characters), the failed notification SHALL use the threaded-notification path (per the existing canonical "ChatOpsBackend exposes a threaded-notification method with graceful degradation" requirement) AND truncate the body at 35,000 characters with the existing pointer-to-daemon-log tail (per the existing canonical "Thread body truncates at 35,000 characters with a pointer to the daemon log" requirement).

**Outcome-to-notification mapping in `process_one_pr`:**

- `Ok(ExecutorOutcome::Completed { .. })` followed by successful commit + push: posts **succeeded**.
- `Ok(ExecutorOutcome::Completed { .. })` followed by a commit OR push step failure: posts **failed** with the step-failure reason.
- `Ok(ExecutorOutcome::Failed { reason })`: posts **failed** with `reason` verbatim.
- `Ok(ExecutorOutcome::SpecNeedsRevision { .. })`: posts **failed** with reason `"spec needs revision (see PR comment for details)"`. The existing `maybe_post_spec_revision_alert` continues to fire independently for the spec-revision-marker case; the revise-lifecycle "failed" notification provides the iteration-framing context the existing alert lacks.
- `Ok(ExecutorOutcome::IterationRequested { .. })` (after `a27a1`): posts **succeeded** with this iteration's duration. The iteration sequence continues on the next polling cycle; the operator sees "applied" framing because the revision DID make progress AND was pushed. When the final iteration of the sequence concludes via a subsequent `Completed`, that iteration's notification posts independently.
- `Ok(ExecutorOutcome::AskUser { .. })`: NO revise-lifecycle notification. The existing AskUser notification path (separate from this requirement) covers operator engagement.
- `Err(e)`: posts **failed** with reason `format!("executor error: {e:#}")`.

#### Scenario: Revise picked up fires before executor launches
- **WHEN** the revisions dispatcher decides to enqueue a revision for an operator-posted `@<bot> revise implement task 2.3` comment on PR #71
- **AND** the alert-state file's `revise_notifications` map has no entry for this comment_id
- **AND** `chatops_ctx` is configured AND `failure_alerts_enabled` is `true`
- **THEN** before `executor.run_revision(...).await` is invoked, the daemon posts the canonical "Revise picked up" text to the per-repo chatops channel
- **AND** the alert-state file's `revise_notifications` map gains an entry for this comment_id with `posted_picked_up_at: <now>`

#### Scenario: Revise succeeded fires after commit + push completes
- **WHEN** the executor returns `ExecutorOutcome::Completed { final_answer }` for a revise iteration
- **AND** the subsequent commit + force-push to the agent branch both succeed
- **AND** the alert-state file's `revise_notifications` map shows `posted_succeeded_at: None` for this comment_id
- **THEN** the daemon posts the canonical "Revise succeeded" text to the per-repo chatops channel
- **AND** the duration string matches the human-readable format (e.g. `38m 12s`)
- **AND** the alert-state file's `posted_succeeded_at` is updated to `<now>`

#### Scenario: Revise failed fires on executor Failed outcome
- **WHEN** the executor returns `ExecutorOutcome::Failed { reason: "timeout" }` for a revise iteration
- **AND** the alert-state file's `revise_notifications` map shows `posted_failed_at: None` for this comment_id
- **THEN** the daemon posts the canonical "Revise failed" text with `<reason>` = `timeout` to the per-repo chatops channel
- **AND** the alert-state file's `posted_failed_at` is updated to `<now>`

#### Scenario: Revise failed uses threaded path for long reasons
- **WHEN** the failed notification would carry a reason longer than 35,000 characters
- **THEN** the notification posts via the threaded-notification API (per the existing canonical "ChatOpsBackend exposes a threaded-notification method with graceful degradation" requirement)
- **AND** the thread body is truncated at 35,000 characters with the existing pointer-to-daemon-log tail
- **AND** the top-line stays the canonical single-line "Revise failed" shape

#### Scenario: Deduplication prevents double-posting on dispatcher re-run
- **WHEN** autocoder restarts mid-revision AND the next polling iteration re-processes the same operator comment (whose comment_id matches an existing `revise_notifications` map entry)
- **AND** the entry shows `posted_picked_up_at: <earlier-timestamp>`
- **THEN** the "Revise picked up" notification is NOT posted again
- **AND** the executor still runs the revision (the deduplication gates the notification only, NOT the work)

#### Scenario: failure_alerts_enabled gates all three notifications
- **WHEN** `chatops_ctx.failure_alerts_enabled` is `false`
- **AND** an operator-posted revise comment triggers an iteration that completes successfully
- **THEN** NONE of "Revise picked up", "Revise succeeded", OR "Revise failed" notifications post
- **AND** the alert-state file's `revise_notifications` map is NOT updated for this comment_id

#### Scenario: Per-repo channel override routes the notification
- **WHEN** the repository has `chatops_channel_id: "C-REPO-SPECIFIC"` set in config
- **AND** `chatops.default_channel_id: "C-DEFAULT"` is also set
- **AND** a revise iteration triggers any of the three notifications
- **THEN** the post target is `C-REPO-SPECIFIC` (per-repo override wins)

#### Scenario: SpecNeedsRevision outcome posts both the lifecycle alert AND the existing spec-revision alert
- **WHEN** the executor returns `ExecutorOutcome::SpecNeedsRevision { unimplementable_tasks, revision_suggestion }` for a revise iteration
- **THEN** the daemon posts the "Revise failed" lifecycle notification with reason `"spec needs revision (see PR comment for details)"`
- **AND** the existing `maybe_post_spec_revision_alert` posts independently with its canonical text shape
- **AND** the two notifications coexist (the operator sees one iteration-framing AND one spec-marker-framing message)

#### Scenario: IterationRequested posts the succeeded notification
- **WHEN** the executor returns `ExecutorOutcome::IterationRequested { ..., iteration_number: 2 }` for a revise iteration
- **AND** the commit + push step completes successfully (the iteration's WIP is pushed)
- **THEN** the daemon posts the "Revise succeeded" lifecycle notification (the revision DID make progress AND was pushed; the next iteration runs on the same comment OR continues without further operator input per `a27a1` semantics)
- **AND** the `posted_succeeded_at` timestamp is recorded
