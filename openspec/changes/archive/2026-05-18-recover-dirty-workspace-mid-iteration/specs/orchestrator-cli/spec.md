## ADDED Requirements

### Requirement: Dirty workspace auto-recovers mid-iteration
autocoder SHALL attempt automatic recovery before falling back to the existing alert-and-return-Err behavior when a polling iteration's pre-pass dirty check finds a non-empty `git status --porcelain` output (after filtering autocoder bookkeeping files like `.alert-state.json`). Recovery consists of (best-effort) `git checkout <base_branch>`, `git reset --hard origin/<base_branch>`, and `git clean -fd` — identical to the startup recovery. After recovery, autocoder SHALL re-run the dirty check; if clean, the iteration proceeds past the dirty check as if the workspace had been clean initially.

Recovery is safe in this position because (a) the agent branch is rebuilt from base each iteration via `recreate_branch`, so wholesale wiping does not lose recoverable work, and (b) any uncommitted modifications at this point are by definition residue from a previously-failed executor invocation whose outcome was already `Failed`/`Escalated` and whose work the operator does not want to ship.

#### Scenario: Workspace dirty due to prior failed executor invocation
- **WHEN** a polling iteration's pre-pass `git status --porcelain` is
  non-empty after filtering autocoder bookkeeping files (typically
  because the previous iteration's executor modified tracked files but
  returned `Failed` or timed out without committing)
- **THEN** autocoder logs a `warn`-level line naming the dirty entry
  count and indicating recovery is being attempted
- **AND** autocoder runs (best-effort) `git checkout <base_branch>`,
  then `git reset --hard origin/<base_branch>`, then `git clean -fd`
  in the workspace
- **AND** autocoder re-runs `git status --porcelain`; if empty,
  logs `info` "workspace recovered mid-iteration; proceeding" and
  the iteration continues into its normal flow (fetch, checkout
  base, recreate agent branch, queue walk)
- **AND** NO `WorkspaceDirtyMidIteration` chatops alert is posted
  for this iteration — recovery succeeded, so the operator does
  not need to be notified

#### Scenario: Workspace remains dirty after recovery attempt
- **WHEN** the recovery commands all complete but a subsequent
  `git status --porcelain` is still non-empty (gitignored state,
  read-only mount, file-locking, etc.)
- **THEN** autocoder posts a `WorkspaceDirtyMidIteration` chatops
  alert (subject to the existing 24h throttle) naming the
  repository URL and a short excerpt of the porcelain output
- **AND** the iteration returns `Err` with the existing message
  shape, preserving prior conservative behavior for genuinely
  unrecoverable cases

#### Scenario: Workspace already clean
- **WHEN** the pre-pass `git status --porcelain` is empty
  (after filtering autocoder bookkeeping files)
- **THEN** no recovery commands are executed
- **AND** the iteration proceeds normally, identical to prior
  behavior — recovery is invoked ONLY when the dirty check would
  otherwise trip

#### Scenario: Recovery command itself fails
- **WHEN** any of the recovery commands (`git reset --hard`,
  `git clean -fd`) returns a non-zero exit
- **THEN** autocoder posts a `WorkspaceDirtyMidIteration` alert
  whose error excerpt names the recovery failure (not the
  original dirty state) so the operator sees the actionable
  problem
- **AND** the iteration returns `Err`; the polling loop proceeds
  to the next sleep as with any iteration-level failure

## MODIFIED Requirements

### Requirement: Iteration-level error tolerance
The polling loop SHALL continue running after a failed iteration; a single iteration's error MUST NOT terminate the task or affect other repositories. Predictable failure categories (workspace init, mid-iteration dirty workspace, branch push, PR creation) SHALL emit a throttled chatops alert via the existing `AlertCategory` + `handle_predictable_failure` mechanism before the iteration returns `Err`. For the mid-iteration dirty-workspace category, the alert SHALL fire only AFTER an auto-recovery attempt has been made and failed to clean the workspace (see "Dirty workspace auto-recovers mid-iteration").

#### Scenario: Iteration fails
- **WHEN** any error occurs during a polling iteration (workspace init, git operation, executor failure, PR creation)
- **THEN** the task emits a log line of the form `"polling iteration failed for <url>: <error chain>"` naming the failed step
- **AND** the task sleeps for `poll_interval_sec` and proceeds to the next iteration
- **AND** other repositories' polling tasks are unaffected (their iterations continue on schedule)

#### Scenario: Mid-iteration dirty workspace alerts via chatops
- **WHEN** `run_pass_through_commits` finds `git status --porcelain`
  non-empty at the start of a pass (after filtering autocoder
  bookkeeping files like `.alert-state.json`) AND auto-recovery
  (see "Dirty workspace auto-recovers mid-iteration") has been
  attempted AND a subsequent dirty check is STILL non-empty
  AND chatops is configured AND `failure_alerts_enabled` is true
- **THEN** autocoder posts a throttled chatops notification under
  `AlertCategory::WorkspaceDirtyMidIteration` naming the repository
  URL and a short excerpt of the porcelain output
- **AND** the iteration returns the existing `Err` ("workspace ... is
  dirty before pass; refusing to proceed: ...")
- **AND** subsequent iterations that produce the same dirty state
  within 24 hours do NOT re-post (the per-category 24h throttle
  suppresses duplicates, matching the existing
  `WorkspaceInitFailure`/`BranchPushFailure`/`PrCreationFailure`
  behavior)

#### Scenario: Mid-iteration dirty workspace without chatops still logs
- **WHEN** the dirty-workspace condition above occurs AND chatops is
  not configured (or `failure_alerts_enabled` is false)
- **THEN** no chatops post is attempted
- **AND** the existing ERROR log line is the operator's sole signal
- **AND** the iteration still returns `Err` and the polling loop
  proceeds to the next sleep

#### Scenario: Dirty-workspace alert clears after recovery
- **WHEN** a subsequent iteration succeeds (workspace no longer
  dirty AND the pass produces commits AND push+PR steps both
  succeed)
- **THEN** the existing on-success `AlertState::clear` call clears
  the `WorkspaceDirtyMidIteration` throttle alongside every other
  category
- **AND** if the workspace becomes dirty again later, the next
  occurrence re-alerts immediately (no leftover suppression)
