## ADDED Requirements

### Requirement: All daemon state-file reads and writes route through the `DaemonPaths` resolver
Every daemon-side code path that reads OR writes a state-file shall construct its path through the `DaemonPaths` resolver's helper methods (`state_dir()`, `cache_dir()`, `logs_dir()`, `runtime_dir()`, AND per-state-shape helpers like `audit_threads_dir()`, `busy_markers_dir()`, `proposal_requests_dir()`, etc.). Hard-coded `/tmp/autocoder/` literals SHALL NOT appear in any source file outside an explicit allowlist (today: only the migration scan logic that references the legacy path on purpose). A CI test SHALL grep the source tree for the literal substring AND fail on any unauthorized hit.

This rule eliminates a defect class where readers and writers drift to different paths after the legacy-to-standard migration. Operator-visible symptoms of the defect class included: `send it` returning `?` for real audit threads (read at `/tmp` while writes go to `<state_dir>`); `@<bot> status` reporting `idle` while the busy marker was held (status read at one path, daemon wrote to another).

#### Scenario: Every state-file consumer uses a `DaemonPaths` helper
- **WHEN** a developer searches the codebase for state-file path construction
- **THEN** every read AND every write goes through a `DaemonPaths` method
- **AND** no source file outside the allowlist contains the literal substring `/tmp/autocoder`
- **AND** the allowlist comment names why each allowed file is exempt

#### Scenario: CI test catches new literal hits
- **WHEN** a future contributor adds a hard-coded `/tmp/autocoder/...` path to a source file not in the allowlist
- **AND** `cargo test` runs
- **THEN** the `path_literals_audit` test fails with the offending file:line:line-contents listed
- **AND** the failure message points at the `DaemonPaths` resolver as the correct fix

#### Scenario: Path resolution is consistent across writer and reader for the same state shape
- **WHEN** the daemon's busy-marker writer stamps a marker at `<runtime_dir>/busy/<workspace>.json`
- **AND** the `@<bot> status` reply composer attempts to read the marker for that workspace
- **THEN** both code paths resolve `<runtime_dir>` through the same `DaemonPaths.runtime_dir()` call
- **AND** the reader finds the writer's marker

#### Scenario: Audit-thread state reader and stamper agree
- **WHEN** an audit's threaded-finding post stamps an audit-thread state file via `paths.audit_threads_dir().join(format!("{thread_ts}.json"))`
- **AND** the `send it` dispatcher looks up the same `thread_ts` via the same `paths.audit_threads_dir().join(...)` call
- **THEN** the dispatcher finds the stamped state
- **AND** the `send it` verb produces a triage run instead of a `?` reaction

#### Scenario: Migration code is explicitly allowed to reference the legacy path
- **WHEN** the migration scan (`autocoder/src/state/migration.rs` or equivalent) references `/tmp/autocoder/` to identify legacy state to move
- **THEN** the path-literals CI test treats that file as part of the allowlist
- **AND** the migration code can continue to reference the legacy path without triggering a CI failure
