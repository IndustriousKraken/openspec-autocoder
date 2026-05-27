## MODIFIED Requirements

### Requirement: Per-repo busy marker prevents concurrent work
At the start of each polling iteration, autocoder SHALL write a per-repo JSON marker at `<runtime_dir>/busy/<workspace-basename>.json` AND hold it through every stage of the pass (executor → review → push → PR). The marker SHALL be removed when the pass returns normally. A daemon crash that bypasses normal cleanup (SIGKILL, segfault, host power loss, daemon restart mid-iteration) intentionally leaves the marker for the next pass to discover.

On the next iteration's startup, autocoder SHALL classify any pre-existing marker via the following branches, in order:

1. **File absent** → acquire, run iteration.
2. **Malformed JSON** → WARN log, clear marker, proceed.
3. **PID not in `/proc`** → clear marker AND log a WARN naming the dead pid, then proceed. **No age check applies to this branch.** A pid that no longer exists cannot be doing legitimate work; the marker is unambiguously stale.
4. **PID alive AND age < `executor.busy_marker_stale_threshold_secs`** → skip iteration with the enhanced "busy marker present" INFO log.
5. **PID alive AND age ≥ threshold AND `comm` matches** → SIGTERM the process group, wait 5 seconds, SIGKILL if still alive, clear marker, post chatops alert, proceed.
6. **PID alive AND age ≥ threshold AND `comm` differs** → ambiguous (PID reuse suspected) — ERROR log, post chatops alert, SKIP iteration, leave marker for human inspection.

The stale-threshold field SHALL be a dedicated `executor.busy_marker_stale_threshold_secs` (default `600` seconds, max `7200` with WARN-and-clamp), NOT a derived value from `executor.timeout_secs`. Raising the executor timeout for legitimately long work SHALL NOT proportionally delay stale-marker recovery on unrelated iterations.

The "busy marker present; skipping iteration" INFO log line SHALL include the marker's age, the resolved threshold, the PID-alive state, AND a `recovery_eligible` boolean computed as `!pid_alive || age >= threshold`. Operators reading `journalctl` can see the marker's recovery state inline without reading the marker file separately.

At daemon startup, after resolving both fields, the daemon SHALL log one INFO line naming the resolved `executor.timeout_secs` AND `executor.busy_marker_stale_threshold_secs`. If the new threshold field was NOT explicitly set in config AND the pre-spec implicit formula (`timeout_secs + 600`) would have produced a longer threshold, an additional INFO line SHALL name the gap so operators migrating from the pre-spec behavior see the change.

Marker contents (unchanged from pre-spec): `repo_url`, `pid`, `pgid`, `comm`, `started_at`, AND `stage` (one of `executor`, `commit`, `review`, `push`, `pr`).

#### Scenario: Dead-pid marker is recovered immediately regardless of age
- **WHEN** a marker file exists with `pid = <some pid not in /proc>` AND `started_at = now - 1 second`
- **AND** the daemon's classification logic runs against this marker
- **THEN** the marker is cleared
- **AND** a WARN log fires naming the dead pid
- **AND** the iteration proceeds
- **AND** the recovery does NOT wait for the age threshold

#### Scenario: Live-pid marker under threshold skips iteration
- **WHEN** a marker file exists with a live PID AND `started_at = now - 30 seconds`
- **AND** `executor.busy_marker_stale_threshold_secs` resolves to `600`
- **THEN** the iteration is skipped with the enhanced INFO log line
- **AND** the log contains `age=30s threshold=10m pid_alive=true recovery_eligible=false`

#### Scenario: Live-pid marker over threshold triggers SIGTERM recovery
- **WHEN** a marker file exists with a live PID whose `comm` matches the daemon AND `started_at = now - 700 seconds`
- **AND** `executor.busy_marker_stale_threshold_secs` resolves to `600`
- **THEN** the daemon sends SIGTERM to the process group, waits 5 seconds, sends SIGKILL if still alive
- **AND** clears the marker
- **AND** posts a chatops alert (subject to the existing throttle)
- **AND** proceeds with the iteration

#### Scenario: Threshold change is independent of `executor.timeout_secs`
- **WHEN** an operator sets `executor.timeout_secs: 5400` AND does NOT explicitly set `executor.busy_marker_stale_threshold_secs`
- **THEN** the resolved threshold is `600` (the default), NOT `6000` (the pre-spec coupled formula)
- **AND** a startup INFO log notes the gap so operators migrating from pre-spec behavior see the change
- **AND** dead-pid markers continue to recover immediately regardless of either value

#### Scenario: Out-of-bounds threshold values are clamped
- **WHEN** an operator sets `executor.busy_marker_stale_threshold_secs: 10000`
- **THEN** the resolved value is `7200` (the max)
- **AND** a WARN log at startup names both the requested and clamped values

#### Scenario: PID-alive check uses `/proc/<pid>` stat
- **WHEN** the classification logic checks whether a pid is alive
- **THEN** the implementation stats `/proc/<pid>` (not signal-0 or other approaches)
- **AND** returns `false` on `ENOENT` (pid does not exist)
- **AND** returns `true` on successful stat
- **AND** on any other error (permission, transient) the implementation treats the pid as "unknown alive" — falling through to the age-based branches rather than incorrectly clearing a possibly-live marker
