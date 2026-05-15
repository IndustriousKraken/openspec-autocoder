## ADDED Requirements

### Requirement: Perma-stuck change detection
autocoder SHALL track consecutive failures per change in a per-repo `.failure-state.json` file at the workspace root. After the executor returns `Failed` for a change (or the daemon transforms a Completed-with-empty-workspace outcome to Failed), the counter for that change SHALL be incremented. After the executor returns `Archived` (including via self-heal), the counter for that change SHALL be cleared. When a change's counter reaches the configured `executor.perma_stuck_after_failures` threshold (default 2), autocoder SHALL write a `.perma-stuck.json` marker into the change directory, post a chatops alert, and exclude the change from subsequent polling iterations until the marker is removed manually.

#### Scenario: Failure increments the counter
- **WHEN** `handle_outcome` produces a `Failed` result for a
  change (whether the executor returned Failed or the daemon
  transformed a Completed-with-empty-workspace via the
  no-op-completion or self-heal logic into Failed)
- **THEN** autocoder reads `.failure-state.json` from the
  workspace root, increments the entry for that change (or
  creates it with `count: 1` if absent), sets `last_reason` and
  `last_failed_at`, and writes the file back atomically
  (write-temp-then-rename)
- **AND** transient daemon-side errors that prevent the
  executor from running (workspace init failure, openspec
  preflight failure, GitHub API transport error) do NOT
  increment the counter — only outcomes where the executor
  itself ran and Failed (or was forced to Failed by
  post-execution classification) count

#### Scenario: Archive clears the counter
- **WHEN** `handle_outcome` produces an `Archived` result for a
  change (including via the self-heal path from
  `self-heal-already-implemented`)
- **THEN** autocoder removes that change's entry from
  `.failure-state.json` and writes the file back atomically
- **AND** the next failure of any change starts fresh from
  `count: 1`

#### Scenario: Threshold reached → mark perma-stuck
- **WHEN** incrementing the counter results in `count >=
  executor.perma_stuck_after_failures` (default 2)
- **THEN** autocoder writes a `.perma-stuck.json` marker file
  inside the change directory containing the change name,
  consecutive_failures count, last_reason, marked_stuck_at
  timestamp, and the operator_action message
- **AND** autocoder posts a chatops alert via the configured
  backend with subject "change perma-stuck" and a body naming
  the repo, change, count, and last reason. The alert is
  subject to the existing 24h throttle so repeat-mark events
  do not spam
- **AND** autocoder logs an ERROR line naming the change and
  the marker file path
- **AND** when no chatops backend is configured, the ERROR log
  is the operator's only signal — the marker is still written
  and the change is still excluded from `list_pending` going
  forward

#### Scenario: Operator clears the marker
- **WHEN** the operator deletes `.perma-stuck.json` from a
  change directory
- **THEN** the next polling iteration sees the change in
  `list_pending` again and runs the executor against it
- **AND** the counter starts fresh at 0 (or whatever
  `.failure-state.json` records for that change after the
  removal — implementations MAY also clear the change's entry
  in `.failure-state.json` at marker-removal time; either is
  acceptable as long as the operator's "retry" signal does
  reset behavior)

#### Scenario: Threshold is one
- **WHEN** `executor.perma_stuck_after_failures` is set to `1`
- **THEN** the very first Failed outcome for a change marks
  perma-stuck (no retry at all)

#### Scenario: Default threshold
- **WHEN** `executor.perma_stuck_after_failures` is unset
- **THEN** autocoder uses `2` as the threshold value
