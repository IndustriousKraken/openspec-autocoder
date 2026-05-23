## ADDED Requirements

### Requirement: Archive-collision pre-flight exclusion
autocoder SHALL detect, at the top of every polling iteration's queue walk, the structural condition where a pending change would fail at archive time because its dated archive entry already exists. For each change name `<slug>` in the iteration's pending set, the polling loop SHALL check whether `openspec/changes/archive/<UTC-YYYY-MM-DD>-<slug>/` exists; if so, the change SHALL be excluded from this iteration without invoking the executor, AND a chatops alert under a new `AlertCategory::ArchiveCollision` SHALL be posted (subject to the existing per-category 24h throttle). The exclusion does NOT count as a perma-stuck failure — the situation is a structural one the operator must resolve, not a repeatable executor failure.

The motivation is cost: invoking the executor for a change that will demonstrably fail at archive time burns real agent-API tokens on work that cannot land. Pre-flight detection costs microseconds and prevents the full executor invocation.

#### Scenario: Both paths present blocks the executor
- **WHEN** an iteration enters `walk_queue` AND a pending change
  `foo` has BOTH `openspec/changes/foo/` AND
  `openspec/changes/archive/<today>-foo/` present on disk
- **THEN** autocoder excludes `foo` from this iteration's
  working set BEFORE the executor is invoked
- **AND** the executor is NEVER called for `foo` in this
  iteration
- **AND** autocoder posts exactly one chatops alert under
  `AlertCategory::ArchiveCollision` (subject to the 24h
  throttle) naming both paths AND describing the operator
  workflow to resolve the collision
- **AND** the per-change failure-state counter for `foo` is
  NOT incremented (collision is a structural condition, not
  an executor failure)

#### Scenario: Only the archive entry exists is the normal post-archive state
- **WHEN** an iteration runs AND a change `foo` has ONLY
  `openspec/changes/archive/<today>-foo/` present (no active
  dir at `openspec/changes/foo/`)
- **THEN** `list_pending` does not return `foo` at all (the
  active dir is absent, so the change is not pending)
- **AND** no collision check applies; no alert fires; the
  iteration proceeds normally with whatever other changes
  are in pending

#### Scenario: Mixed collision and clean changes in the same iteration
- **WHEN** an iteration's pending set contains `foo` (with
  the collision condition) AND `bar` (clean, archive entry
  absent)
- **THEN** `foo` is excluded with the collision alert
- **AND** `bar` is processed normally: executor invoked,
  outcome handled, archive moved, etc.
- **AND** the iteration's `processed` list contains `bar` (if
  it produced a diff) and does NOT contain `foo`

#### Scenario: Repeated collision within 24h is throttled
- **WHEN** a previous iteration in the last 24 hours has
  already posted an `ArchiveCollision` alert for repository
  `<repo>` AND a fresh iteration detects the same condition
- **THEN** no chatops post is made (24h per-category
  throttle applies, same as every other predictable failure
  category)
- **AND** the WARN-level log line still emits per-iteration
  so journalctl tailing shows the diagnosis even with
  chatops disabled

### Requirement: Perma-stuck counter covers all per-change errors
The perma-stuck failure-state counter SHALL increment for every per-change error returned from the polling loop's per-change processing function, not only for executor-reported Failed outcomes. Specifically: any `Err` returned by `queue::archive`, by the post-executor commit step, by `queue::unlock`, or by any other operation scoped to the per-change loop counts as one failure for the affected change. When the counter reaches `executor.perma_stuck_after_failures`, the existing perma-stuck marker is written AND the existing chatops alert fires.

Iteration-level errors that happen OUTSIDE the per-change loop (workspace init, dirty-workspace pre-pass check, branch push, PR creation) MUST NOT increment any change's counter — those have their own throttled chatops categories and are not attributable to a specific pending change.

#### Scenario: Executor Failed increments the counter (existing behavior pinned)
- **WHEN** the executor returns `Failed { reason }` for a
  change `foo`
- **THEN** `failure_state::record_failure(ws, "foo", reason)`
  is called exactly once for this iteration
- **AND** the counter for `foo` increments by 1

#### Scenario: Post-executor archive failure increments the counter (new behavior)
- **WHEN** the executor returns `Completed` for a change
  `foo` AND `queue::archive` (or any subsequent per-change
  step) returns `Err`
- **THEN** `failure_state::record_failure(ws, "foo", reason)`
  is called exactly once for this iteration, with `reason`
  naming the error origin (e.g. "archive failed: <message>")
- **AND** the counter for `foo` increments by 1

#### Scenario: Counter increment threshold writes the marker
- **WHEN** the counter for change `foo` reaches
  `executor.perma_stuck_after_failures` (default 2) via any
  combination of executor failures and post-executor
  failures
- **THEN** autocoder writes
  `openspec/changes/foo/.perma-stuck.json` AND the existing
  perma-stuck chatops alert fires (per the existing
  "Perma-stuck chatops alert content" requirement)
- **AND** subsequent iterations exclude `foo` from
  `list_pending` until the marker is removed by the operator

#### Scenario: Iteration-level error does not increment per-change counter
- **WHEN** an iteration fails at workspace init, OR fails the
  pre-pass dirty check (even after the auto-recovery
  attempt), OR fails at branch push, OR fails at PR creation
- **THEN** no per-change counter increments
- **AND** the iteration's failure routes through the
  appropriate iteration-level `AlertCategory`
  (`WorkspaceInitFailure`, `WorkspaceDirtyMidIteration`,
  `BranchPushFailure`, `PrCreationFailure`)
- **AND** the per-change processing function was either
  never entered (init/dirty failures) or did not return Err
  itself (push/PR failures happen after the per-change loop
  completes)

#### Scenario: No double-counting on executor-Failed
- **WHEN** the executor returns `Failed` AND the existing
  outcome handler calls `record_failure`
- **THEN** the broader wrapper does NOT also call
  `record_failure` for the same change in the same iteration
- **AND** the counter increments by exactly 1, not 2
