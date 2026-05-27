## MODIFIED Requirements

### Requirement: Status reply always shows live workspace snapshot
The `@<bot> status <repo>` reply SHALL contain five always-present sections — branches, last commit on each branch, latest PR from the agent branch, currently-busy state, AND the next-iteration estimate — followed by any active markers, currently-engaged 24h alert throttles, AND the queue snapshot. The `currently:` line SHALL surface the busy marker's actual contents (not just `idle` vs. `working on <change>`) so operators diagnosing a "stuck pending change" can distinguish between an audit in flight, a stale marker awaiting recovery, AND a truly-idle daemon.

The `currently:` line's value SHALL be computed by branching on marker contents in this order:

1. No marker present → `idle`.
2. Marker present AND classification per `a08`'s busy-marker semantics says the marker is stale (dead pid OR live pid past threshold) → `stale marker from pid <pid> (age <age>, recovery <eligible-or-remaining-time>)`.
3. Marker present AND `change` non-empty → `working on <change> (started <age> ago)`.
4. Marker present AND `stage=executor` AND `change` empty AND an audit-log file at `<logs_dir>/runs/<workspace>/audits/<audit_type>-<timestamp>.log` matches the marker's `started_at` → `running audit <audit_type> (started <age> ago)`.
5. Marker present AND `stage` ∈ `{commit, review, push, pr}` AND `change` empty → `<stage> in progress (started <age> ago)`.
6. Marker present AND `stage` matches a recovery operation (rebuild-specs, fork recreation) → `recovery in progress (started <age> ago, type=<recovery-type>)`.
7. Marker present but no classification matches → `busy (stage=<stage>, started <age> ago)` fallback.

The status code path SHALL read the busy marker from the daemon's resolved runtime-dir path (per `a09`'s state-path-resolution rule). The status reply MUST NOT report `idle` when the daemon's writer has stamped a marker at the runtime path.

The age formatting matches the existing convention: `Xm ago` for ages under 1 hour, `XhYm ago` for older.

#### Scenario: Daemon working on a named change
- **WHEN** the busy marker has `change: a36-expense-tracking`, `stage: executor`, `started_at: now - 180 seconds`
- **AND** an operator runs `@<bot> status coterie`
- **THEN** the reply's `currently:` line reads `working on a36-expense-tracking (started 3m ago)`

#### Scenario: Daemon running an audit (change field empty)
- **WHEN** the busy marker has `change: ""`, `stage: executor`, `started_at: 2026-05-27T19:11:45Z`
- **AND** an audit log exists at `<logs_dir>/runs/github_com_owner_coterie/audits/architecture_consultative-2026-05-27T19:11:45Z.log` (timestamp matching)
- **AND** an operator runs `@<bot> status coterie` now (say 19:25:00Z)
- **THEN** the reply's `currently:` line reads `running audit architecture_consultative (started 13m ago)`

#### Scenario: Daemon in a post-executor phase
- **WHEN** the busy marker has `stage: commit`, `change: ""`, `started_at: now - 12 seconds`
- **THEN** the reply's `currently:` line reads `commit in progress (started 12s ago)`
- **AND** similarly for `stage: review`, `stage: push`, `stage: pr`

#### Scenario: Stale marker with dead pid surfaces immediate recovery
- **WHEN** the busy marker has `pid: 490170`, `started_at: now - 53 minutes` AND `/proc/490170` does NOT exist
- **THEN** the reply's `currently:` line reads `stale marker from pid 490170 (age 53m, recovery eligible now)`
- **AND** the operator sees this as a directly-actionable diagnostic (per `a08`, the next iteration will clear it; OR the operator can `rm` the file directly)

#### Scenario: Stale marker with live pid past threshold surfaces upcoming recovery
- **WHEN** the busy marker has `pid: <some live pid>`, `started_at: now - 700 seconds` AND `executor.busy_marker_stale_threshold_secs: 600`
- **THEN** the reply's `currently:` line reads `stale marker from pid <pid> (age 11m40s, recovery eligible next iteration)`
- **AND** the operator sees that recovery will fire on the next polling iteration via SIGTERM (per `a08`)

#### Scenario: Stale marker approaching threshold surfaces remaining time
- **WHEN** the busy marker has `pid: <some live pid>`, `started_at: now - 8 minutes` AND threshold is 10 minutes
- **THEN** the reply's `currently:` line reads `stale marker from pid <pid> (age 8m, recovery in 2m)`
- **AND** the heuristic (surface upcoming-recovery when age > 80% of threshold) makes "stuck-feeling" markers visibly transitioning rather than permanent

#### Scenario: Truly idle daemon
- **WHEN** no busy marker exists at the resolved runtime-dir path
- **THEN** the reply's `currently:` line reads `idle`
- **AND** an operator seeing this combined with a non-empty `queue: <N> pending` line knows the daemon SHOULD be picking up the change on the next iteration

#### Scenario: Status read path matches daemon write path
- **WHEN** the daemon's busy-marker writer stamps a marker at `<runtime_dir>/busy/<workspace>.json`
- **AND** the status reply composer reads the marker for that workspace
- **THEN** both code paths use the same resolved `<runtime_dir>` (per `a09`'s state-path-resolution rule)
- **AND** the status reply never reports `idle` when a marker file exists at the daemon's write path
