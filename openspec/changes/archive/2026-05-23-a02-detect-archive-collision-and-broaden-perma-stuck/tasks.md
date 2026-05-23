## 1. New AlertCategory variant

- [x] 1.1 In `autocoder/src/alert_state.rs`, add `ArchiveCollision` to the `AlertCategory` enum. Add its `label()` arm returning `"archive collision"`. Make sure serde round-trip is exercised by the existing serialization test (or add one if the existing tests enumerate variants).
- [x] 1.2 Add a unit test confirming the new variant serializes/deserializes correctly and round-trips through `AlertState::save` + `AlertState::load_or_default`.

## 2. Collision-detection helper

- [x] 2.1 In `autocoder/src/queue.rs`, add `pub fn archive_collision_path(workspace: &Path, change: &str) -> PathBuf` that returns `<workspace>/openspec/changes/archive/<UTC-YYYY-MM-DD>-<change>/`. The date format matches the existing `queue::archive` implementation so the path returned is exactly what `queue::archive` would attempt to create.
- [x] 2.2 Add `pub fn would_collide_on_archive(workspace: &Path, change: &str) -> bool` that returns `archive_collision_path(workspace, change).exists()`. Thin wrapper, but having it as a named function makes the call site at the polling loop self-documenting.
- [x] 2.3 Unit test: build a fixture workspace with `openspec/changes/foo/` AND `openspec/changes/archive/<today>-foo/`. Assert `would_collide_on_archive(ws, "foo")` is true. Also assert it's false when only the active dir exists, and false when only the archive entry exists (the latter is a legitimate post-archive state).

## 3. Pre-flight exclusion in the polling loop

- [x] 3.1 In `autocoder/src/polling_loop.rs::run_pass_through_commits`, AFTER `queue::list_pending` returns its list AND BEFORE `walk_queue` (or `process_waiting_changes`, whichever runs first) is entered: for each candidate change, call `queue::would_collide_on_archive`. If true, drop the change from the processed list AND call `handle_predictable_failure` with `AlertCategory::ArchiveCollision`. The error passed to `handle_predictable_failure` has the body described in the proposal (concrete paths + the fix workflow).
- [x] 3.2 The same pre-flight applies to `process_waiting_changes` — if a change emerged from waiting (escalation resumed) AND collision conditions are met, exclude with the same alert. The implementation centralizes the check in one helper called from both call sites.
- [x] 3.3 Log line: emit a WARN-level structured log including `url`, `change`, `archive_path`, and `iteration_skipped: true` so journalctl tailing surfaces the diagnosis even with chatops disabled.
- [x] 3.4 The iteration proceeds with the remaining (non-colliding) changes. If ALL changes in the pending set collide, the iteration completes with zero processed changes — same shape as an iteration where every pending change was perma-stuck-marked.

## 4. Broader perma-stuck counter

- [x] 4.1 Identify the per-change processing function inside `walk_queue` (the function that handles ONE change end-to-end: lock → executor invocation → outcome handling → commit + archive). Wrap the call site so any `Err` it returns triggers `failure_state::record_failure(workspace, change, reason)` BEFORE the Err propagates up. Where the existing code already calls `record_failure` for executor-Failed outcomes, that path stays unchanged — the wrapper covers the gap where the function returns Err from any OTHER source (queue::archive failures, post-executor commit errors, etc.).
- [x] 4.2 Make sure NOT to double-count: if executor returns `Failed` and the existing handler already calls `record_failure`, the wrapper must not call it again. Achieve this by having the inner code consume the Failed outcome (record + return Err) and the outer wrapper only fire `record_failure` when the Err originated from a non-executor source. The simplest implementation: pass a flag through the error type (e.g., a tuple `(anyhow::Error, FailureKind)` where `FailureKind::ExecutorReported` is already-recorded and `FailureKind::PostExecutor` needs to be recorded by the wrapper).
- [x] 4.3 Iteration-level errors (workspace init, dirty workspace, branch push, PR creation) MUST NOT increment the per-change counter — they're outside `walk_queue` entirely and have their own alert categories. Verify by tracing the existing error paths in `run_pass_through_commits` AND `execute_one_pass` — only the per-change-scoped errors inside `walk_queue` go through the new wrapper.

## 5. Tests

- [x] 5.1 Polling-loop test: `archive_collision_excludes_change_and_alerts`. Seed a workspace fixture with `openspec/changes/foo/` AND `openspec/changes/archive/<today>-foo/`. Run `run_pass_through_commits` with a chatops fixture and an `UnreachableExecutor` (panics if invoked). Assert (a) the executor is NEVER called, (b) exactly one chatops post under `ArchiveCollision`, (c) the iteration returns Ok with empty `processed` list (no commits produced), (d) the failure-state counter for `foo` is NOT incremented (collision is not a perma-stuck-counting event).
- [x] 5.2 Polling-loop test: `archive_collision_does_not_block_other_changes`. Seed a workspace with two pending changes — `foo` (colliding) and `bar` (clean). Assert `bar` is processed normally while `foo` is excluded with one chatops alert. Use a `RecordingExecutor` that succeeds on `bar` and panics on any other change name to verify only `bar` was invoked.
- [x] 5.3 Polling-loop test: `post_executor_archive_failure_increments_counter`. Seed a workspace where the executor returns `Completed` with diff (use a fixture executor that creates a small file edit), but make `queue::archive` fail by some means (the cleanest: pre-create the archive destination dir AFTER `list_pending` runs but BEFORE archive is attempted — tricky to time; the simpler test is a unit test on the wrapper directly with a stub closure that returns Err). Assert the failure counter increments by 1.
- [x] 5.4 Iteration-level test: `iteration_level_failure_does_not_increment_per_change_counter`. Trigger a dirty-workspace recovery failure (existing test fixture pattern). Assert no per-change counter movement (the dirty-workspace alert fires through its own category, not the per-change one).
- [x] 5.5 End-to-end-shape regression test: reproducing the myrepo incident's preconditions. Seed two paths (active and dated-archive) for the same change. Run two consecutive `run_pass_through_commits` invocations. Assert the executor is invoked ZERO times across both iterations (collision detected, change excluded), the chatops post fires ONCE (24h throttle catches the second), and the failure-state counter stays at 0.

## 6. Spec deltas

- [x] 6.1 Add ADDED requirement "Archive-collision pre-flight exclusion" to `orchestrator-cli`. Scenarios: collision detected (executor not invoked, alert fires), collision absent (normal flow), multiple changes with mixed collision states (non-colliding ones proceed), alert throttling (second collision within 24h does not re-post).
- [x] 6.2 Add ADDED requirement "Perma-stuck counter covers all per-change errors" to `orchestrator-cli`. Scenarios: executor Failed (counter increments — existing behavior pinned), post-executor archive failure (counter increments — new behavior), iteration-level error like dirty workspace (counter does NOT increment — boundary case), counter reaching threshold writes the marker AND emits the alert (cross-reference existing perma-stuck alert content requirement).

## 7. Verification

- [x] 7.1 `cargo test` passes (new tests + existing).
- [x] 7.2 `openspec validate detect-archive-collision-and-broaden-perma-stuck --strict` passes.
