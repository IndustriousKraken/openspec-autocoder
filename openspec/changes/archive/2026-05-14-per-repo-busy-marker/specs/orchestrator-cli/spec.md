## ADDED Requirements

### Requirement: Per-repo busy marker prevents concurrent work
autocoder SHALL acquire a per-repo busy marker file at the start of each polling iteration and hold it through every stage of the pass (executor invocation, commit, review, push, PR creation). The marker lives outside the workspace at `/tmp/autocoder/busy/<workspace-basename>.json` and is created atomically via POSIX `O_EXCL`. Its presence prevents any other autocoder pass — same daemon or different — from concurrently working on the same repo. Crashes that bypass normal release (SIGKILL, segfault, host power loss) leave the marker behind for the next pass to detect and recover from.

#### Scenario: Acquire on a clean repo
- **WHEN** a polling iteration begins AND no marker file exists at
  `/tmp/autocoder/busy/<workspace-basename>.json`
- **THEN** the daemon creates the marker via `OpenOptions::new()
  .write(true).create_new(true).open(path)` (atomic against
  concurrent daemons)
- **AND** the marker contains a JSON document with fields
  `repo_url`, `pid` (this process's PID), `pgid` (this process's
  process group ID), `comm` (the value of `/proc/<pid>/comm` at
  acquire time, on Linux; empty string on other platforms),
  `started_at` (RFC 3339 UTC timestamp), and `stage` (initially
  `"executor"`)
- **AND** the iteration proceeds normally

#### Scenario: Atomic stage transitions
- **WHEN** the iteration moves from one stage to the next
  (`executor → commit → review → push → pr`)
- **THEN** the daemon updates the marker's `stage` field via a
  write-to-temp-then-rename sequence so concurrent readers see
  either the prior stage or the new one, never a partial write
- **AND** stage names are exactly: `executor`, `commit`,
  `review`, `push`, `pr`

#### Scenario: Release on normal iteration end
- **WHEN** `execute_one_pass` returns (success or any error)
- **THEN** the RAII guard holding the marker drops, and the file
  is removed
- **AND** the next iteration finds no marker and proceeds normally

#### Scenario: Marker exists, age below stuck threshold
- **WHEN** acquire detects an existing marker AND its `started_at`
  is less than `executor.timeout_secs + 600 seconds` old
- **THEN** the daemon logs INFO with the marker contents and skips
  this iteration without modifying the marker
- **AND** the polling task continues with its normal sleep + next-iteration cycle

#### Scenario: Stuck threshold exceeded, PID dead
- **WHEN** acquire detects a marker older than the stuck threshold
  AND the recorded `pid` does not correspond to a running process
  (verified via `kill(pid, 0)` returning `ESRCH`)
- **THEN** the daemon deletes the marker, logs WARN naming the
  marker's prior contents (so operators see what crashed), and
  proceeds to acquire a fresh marker and run the iteration

#### Scenario: Stuck threshold exceeded, PID alive, comm matches
- **WHEN** acquire detects a marker older than the stuck threshold
  AND `kill(pid, 0)` returns Ok AND the value of
  `/proc/<pid>/comm` matches the recorded `comm` field (Linux;
  the comm-check is skipped on non-Linux platforms and the PID
  liveness check is trusted alone)
- **THEN** the daemon sends `SIGTERM` to the process group via
  `killpg(pgid, SIGTERM)`, waits up to 5 seconds for the group to
  exit, sends `SIGKILL` via `killpg(pgid, SIGKILL)` if still alive,
  deletes the marker, logs WARN with the action taken, attempts
  to post a chatops alert "repo recovered from stuck state"
  (best-effort; failure to post is logged but does not block the
  iteration), and proceeds to acquire a fresh marker and run
- **AND** the iteration proceeds even when no chatops backend is
  configured

#### Scenario: Stuck threshold exceeded, PID alive, comm differs
- **WHEN** acquire detects a marker older than the stuck threshold
  AND `kill(pid, 0)` returns Ok AND the recorded `comm` field is
  non-empty AND differs from the live `/proc/<pid>/comm` value
- **THEN** the daemon logs ERROR naming the discrepancy, attempts
  to post a chatops alert "repo stuck — please investigate"
  (best-effort), and SKIPS this iteration without modifying the
  marker
- **AND** the marker stays in place for human investigation; the
  next polling iteration will re-evaluate
- **AND** the iteration is skipped even when no chatops backend
  is configured (the ERROR log is the operator's only signal in
  that case)

#### Scenario: Malformed marker JSON
- **WHEN** acquire detects a marker file that cannot be parsed as
  the expected JSON shape
- **THEN** the daemon logs WARN naming the parse failure, deletes
  the marker, and proceeds to acquire a fresh one
