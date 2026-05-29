## ADDED Requirements

### Requirement: Control socket's `record_outcome` action accepts `iteration_request` variant

The `record_outcome` control-socket action (added in `a27a0`) SHALL accept the `iteration_request` variant tag in its `outcome` payload, alongside the existing `success` AND `spec_needs_revision` variants.

Variant payload shape:

```json
{
  "type": "iteration_request",
  "completed_tasks": ["1", "2"],
  "remaining_tasks": ["3"],
  "reason": "task 3 needs a refactor I want to plan more carefully"
}
```

All three fields are required. The handler trusts the payload (the MCP layer validated it before relaying) AND stores it as the corresponding `RecordedOutcome::IterationRequest` enum variant. The store's last-writer-wins semantics (per a27a0) apply: a second `record_outcome` for the same `(workspace_basename, change)` key replaces the prior entry.

The `consume_outcome` action's response shape SHALL include `iteration_request` payloads with the same field set (the handler returns whatever was stored).

#### Scenario: `record_outcome` accepts iteration_request variant
- **WHEN** a client sends `{"action":"record_outcome","workspace_basename":"my-repo","change":"a30-foo","outcome":{"type":"iteration_request","completed_tasks":["1","2"],"remaining_tasks":["3"],"reason":"..."}}`
- **THEN** the response is `{"ok":true}`
- **AND** a subsequent `consume_outcome` for the same key returns the recorded payload byte-for-byte

#### Scenario: `consume_outcome` returns iteration_request payload
- **WHEN** a client has recorded an `iteration_request` outcome AND subsequently sends `consume_outcome` for the same key
- **THEN** the response shape is `{"ok":true,"outcome":{"type":"iteration_request","completed_tasks":[...],"remaining_tasks":[...],"reason":"..."}}`
- **AND** the store entry is cleared

### Requirement: Polling loop handles `IterationRequested` by committing WIP, pushing, marking, AND dropping the lock — without touching any PR

When the polling loop receives `ExecutorOutcome::IterationRequested { completed_tasks, remaining_tasks, reason, iteration_number }` from the executor, it SHALL perform the following actions in order:

1. **Commit the workspace's diff to the agent branch.** Commit message: `iteration <iteration_number> of <change>: <reason-truncated-to-80-chars>`. If the working tree is clean (the agent emitted `outcome_request_iteration` without modifying any files), the polling loop SHALL skip the commit step, emit `tracing::warn!` naming the anomaly, AND proceed to step 3 (the marker is still useful for the next iteration; the lack-of-progress will count against the cap on the next iteration request).
2. **Force-push the agent branch to the remote.** Push failure aborts the sequence: the polling loop emits `tracing::error!` naming the failure, SKIPS steps 3, AND proceeds to step 4 (drop lock). The change reverts to normal pending behavior on the next polling cycle.
3. **Write `.iteration-pending.json`** using atomic tempfile + rename, with the payload `{ completed_tasks, remaining_tasks, reason, iteration_number }`.
4. **Drop `.in-progress`** per the existing canonical "Unlocking after any executor outcome" requirement.

The polling loop SHALL NOT call any PR-open, PR-comment, OR PR-close routine on the `IterationRequested` arm. PR lifecycle is reserved for the `Completed` outcome (today's behavior, unchanged). An iteration sequence can run entirely without ever opening a PR; the PR opens on the FINAL iteration's `Completed` outcome AND the PR body includes the accumulated implementation-notes content from all prior iterations (per the implementer's open question in design.md; this is implementer scope, not spec-binding).

After step 4 completes, the polling loop continues normally. The next polling iteration on this repo picks up the iteration-pending change ahead of any alphabetically-earlier pending sibling (per the queue-engine deltas in this change).

#### Scenario: IterationRequested commits, pushes, marks, drops lock
- **WHEN** the polling loop receives `ExecutorOutcome::IterationRequested { completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "...", iteration_number: 2 }` AND the workspace has a dirty diff
- **THEN** the loop commits the diff with message `iteration 2 of <change>: <truncated reason>`
- **AND** force-pushes the agent branch to the remote
- **AND** writes `.iteration-pending.json` atomically with the documented payload
- **AND** drops `.in-progress`
- **AND** does NOT call any PR-related routine

#### Scenario: Clean working tree on IterationRequested skips commit, still writes marker
- **WHEN** the polling loop receives `ExecutorOutcome::IterationRequested { ... }` AND the workspace has no diff
- **THEN** the loop skips the commit step
- **AND** emits `tracing::warn!` naming the clean-tree anomaly
- **AND** still writes `.iteration-pending.json` atomically
- **AND** drops `.in-progress`

#### Scenario: Push failure aborts marker write, drops lock
- **WHEN** the polling loop receives `ExecutorOutcome::IterationRequested { ... }` AND the commit succeeds BUT the force-push fails (network error, upstream rejection, etc.)
- **THEN** the loop emits `tracing::error!` naming the push failure
- **AND** does NOT write `.iteration-pending.json`
- **AND** drops `.in-progress`
- **AND** the next polling cycle sees the change as a normal pending entry (no front-insertion preference)

#### Scenario: No PR is opened or modified during iteration sequence
- **WHEN** the polling loop processes an iteration sequence (iteration 1 → IterationRequested → iteration 2 → IterationRequested → iteration 3 → Completed)
- **THEN** no PR is opened OR commented on during iterations 1 AND 2
- **AND** the PR is opened on iteration 3's `Completed` outcome (today's behavior)
- **AND** the iteration 3 PR body reflects the cumulative work from iterations 1, 2, AND 3
