## MODIFIED Requirements

### Requirement: Status reply always shows live workspace snapshot
The `status` verb's reply SHALL always include five sections regardless of whether the repo has any markers, throttled alerts, or queued changes: (1) `branches: base=<base>, agent=<agent>`; (2) one `last commit on <branch>` line per branch (base and agent), each rendering as `<short_sha> "<subject>" (<age> ago)` when a commit exists or `(none)` when the branch does not exist or has no commits; (3) `latest PR: ...` with a URL on the following line when a PR exists from the agent branch, or `latest PR: (none)` otherwise; (4) the `currently:` line surfacing the live busy marker's actual contents (per the branching rules below); (5) the existing `next iteration: in <age> ...` line. These sections SHALL precede the existing marker / throttled-alert / queue sections.

The `currently:` line's value SHALL be computed by branching on the busy marker's contents in this order:

1. No marker present → `idle`.
2. Marker present AND classification per `a08`'s busy-marker semantics says the marker is stale (dead pid OR live pid past threshold) → `stale marker from pid <pid> (age <age>, recovery <eligible-or-remaining-time>)`.
3. Marker present AND `change` non-empty → `working on <change> (started <age> ago)`.
4. Marker present AND `stage=executor` AND `change` empty AND an audit-log file at `<logs_dir>/runs/<workspace>/audits/<audit_type>-<timestamp>.log` matches the marker's `started_at` → `running audit <audit_type> (started <age> ago)`.
5. Marker present AND `stage` ∈ `{commit, review, push, pr}` AND `change` empty → `<stage> in progress (started <age> ago)`.
6. Marker present AND `stage` matches a recovery operation (rebuild-specs, fork recreation) → `recovery in progress (started <age> ago, type=<recovery-type>)`.
7. Marker present but no classification matches → `busy (stage=<stage>, started <age> ago)` fallback.

The status code path SHALL read the busy marker from the daemon's resolved runtime-dir path (per `a09`'s state-path-resolution rule). The status reply MUST NOT report `idle` when the daemon's writer has stamped a marker at the runtime path.

The age formatting matches the existing convention: `Xm ago` for ages under 1 hour, `XhYm ago` for older.

#### Scenario: All sections present for a healthy repo
- **WHEN** an operator issues `status <repo>` against a repo with commits on both branches, an open PR from the agent branch, an idle daemon, and an empty queue
- **THEN** the reply contains all five always-present sections in the documented order
- **AND** the `currently:` line reads `idle`
- **AND** the queue section either reads `queue: 0 pending, 0 waiting, 0 excluded` (one-liner form) or is omitted entirely per the queue-one-liner requirement

#### Scenario: Absent data renders `(none)`, not blank or missing
- **WHEN** the agent branch does not exist yet (fresh clone)
- **THEN** `last commit on <agent_branch>:` reads `(none)`
- **AND** the line is still present (the section is always shown)

#### Scenario: GitHub failure does not break the reply
- **WHEN** the GitHub API call for `latest PR` returns an error (network failure, 4xx, 5xx, rate-limit)
- **THEN** the daemon logs a WARN with the underlying error
- **AND** the reply's `latest PR:` line reads `(none)`
- **AND** every other section is rendered normally
- **AND** the status reply succeeds — the operator gets the local-state half even when GitHub is unreachable

#### Scenario: Local git failure does not break the reply
- **WHEN** `git log -1` returns an error (workspace not yet cloned, .git directory corrupt)
- **THEN** the daemon logs a WARN with the underlying error
- **AND** the affected `last commit on <branch>:` line reads `(none)`
- **AND** every other section is rendered normally

#### Scenario: Currently-busy line reflects the live busy marker
- **WHEN** the daemon is mid-iteration on change `a05-foo` started 2 minutes ago
- **THEN** the `currently:` line reads `working on a05-foo (started 2m ago)`
- **AND** the busy-marker file is read but NOT taken, held, or released by the status path

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

#### Scenario: Status read path matches daemon write path
- **WHEN** the daemon's busy-marker writer stamps a marker at `<runtime_dir>/busy/<workspace>.json`
- **AND** the status reply composer reads the marker for that workspace
- **THEN** both code paths use the same resolved `<runtime_dir>` (per `a09`'s state-path-resolution rule)
- **AND** the status reply never reports `idle` when a marker file exists at the daemon's write path
