## 1. Config

- [x] 1.1 Add `pub perma_stuck_after_failures: Option<u32>` to `ExecutorConfig` in `config.rs` with `#[serde(default)]`. Default behavior: when `None`, use `2`. When `Some(0)`, treat as `1` (zero would mean "mark stuck immediately before any attempt"; that's a misconfiguration — clamp to 1 with a WARN log at startup).
- [x] 1.2 Add a helper `pub fn perma_stuck_threshold(&self) -> u32` on `ExecutorConfig` that returns `self.perma_stuck_after_failures.unwrap_or(2).max(1)` and (in the startup path) WARN-logs once if the configured value is 0.
- [x] 1.3 **Verify:** config tests `executor_perma_stuck_default_is_two`, `executor_perma_stuck_clamps_zero_to_one`, `executor_perma_stuck_accepts_custom_value`.

## 2. Failure-state file module

- [x] 2.1 Create `autocoder/src/failure_state.rs`. Public types:
    ```rust
    pub struct FailureState { /* HashMap<String, FailureEntry> */ }
    pub struct FailureEntry {
        pub count: u32,
        pub last_reason: String,
        pub last_failed_at: chrono::DateTime<chrono::Utc>,
    }
    ```
- [x] 2.2 Implement `pub fn load(workspace: &Path) -> Result<FailureState>` reading `<workspace>/.failure-state.json`. Missing file → default empty state. Parse failure → log WARN and return empty (treat as corrupt = fresh state; conservative).
- [x] 2.3 Implement `pub fn record_failure(workspace, change, reason) -> Result<u32>` — increments the change's counter (creates if absent), updates last_reason and last_failed_at, persists atomically via temp+rename. Returns the new count.
- [x] 2.4 Implement `pub fn clear(workspace, change) -> Result<()>` — removes the change's entry and persists atomically. Silent on "entry not present."
- [x] 2.5 At workspace init, ensure `.failure-state.json` is in `.git/info/exclude` (same pattern as `.alert-state.json`). Reuse the existing exclude-management helper if one exists; otherwise add a small one.
- [x] 2.6 **Verify:** `failure_state::tests::*`:
    - `load_missing_returns_empty`
    - `record_failure_creates_entry`
    - `record_failure_increments_existing`
    - `clear_removes_entry`
    - `clear_is_idempotent_when_entry_absent`
    - `corrupt_file_treated_as_empty`

## 3. Perma-stuck marker module

- [x] 3.1 Add `autocoder/src/perma_stuck.rs`. Public:
    ```rust
    pub struct PermaStuckMarker {
        pub change: String,
        pub consecutive_failures: u32,
        pub last_reason: String,
        pub marked_stuck_at: chrono::DateTime<chrono::Utc>,
        pub operator_action: String,
    }
    pub fn write_marker(workspace, change, entry: &failure_state::FailureEntry) -> Result<()>;
    pub fn marker_exists(workspace, change) -> bool;
    pub fn remove_marker(workspace, change) -> Result<()>;
    ```
- [x] 3.2 `write_marker` writes `<workspace>/openspec/changes/<change>/.perma-stuck.json` with the schema described in the proposal. `operator_action` is the literal string "Delete this file to retry the change."
- [x] 3.3 **Verify:** `perma_stuck::tests::*`:
    - `write_then_exists_returns_true`
    - `remove_makes_exists_false`
    - `marker_exists_false_for_clean_change_dir`

## 4. queue::list_pending exclusion

- [x] 4.1 In `queue.rs::list_pending`, add a check for `.perma-stuck.json` alongside the existing `.in-progress` and `.question.json` exclusions. The check pattern: `dir.join(".perma-stuck.json").exists()`.
- [x] 4.2 **Verify:** `queue::tests::list_pending_excludes_perma_stuck` — fixture workspace with three change dirs, one bearing a `.perma-stuck.json` marker. Assert `list_pending` returns the other two.

## 5. Wire into handle_outcome

- [x] 5.1 In `polling_loop::handle_outcome` (or its caller in `walk_queue`), after a `Failed` outcome and AFTER the existing `.in-progress` unlock, call `failure_state::record_failure(workspace, change, reason)`. The `reason` is the executor's Failed reason string (or a synthetic one like `"agent reported Completed without modifying the workspace"` for the no-op-completion case).
- [x] 5.2 If the new count `>=` the configured threshold, call `perma_stuck::write_marker(...)`, then call `post_perma_stuck_alert(chatops_ctx, repo, change, reason, count).await` (best-effort, swallow errors with WARN log).
- [x] 5.3 After an `Archived` outcome, call `failure_state::clear(workspace, change)`. Do this regardless of whether the archive was a normal Completed-with-diff or a self-heal — both reset the throttle.
- [x] 5.4 The threshold needs to be threaded through to `handle_outcome`. Easiest: extend `polling_loop::run`'s signature to accept the threshold (alongside `stuck_threshold_secs`); `cli::run::execute` reads `cfg.executor.perma_stuck_threshold()` and passes it down. Add a corresponding parameter to test helpers (`run_pass_through_commits`, `walk_queue`).
- [x] 5.5 The `post_perma_stuck_alert` helper mirrors the existing `post_stuck_alert` in `polling_loop.rs` (introduced by the busy-marker change). Subject the body to the existing 24h throttle by storing the alert state in `.alert-state.json`'s existing categories (introduce a new category `PermaStuck { change_name }` if needed, or piggyback on the workspace-level alert state with a per-change key).

## 6. Tests

- [x] 6.1 `polling_loop::tests::failed_increments_failure_counter` — fixture with `AlwaysFailingExecutor`. Run one pass. Read `.failure-state.json`. Assert the change's count is 1.
- [x] 6.2 `polling_loop::tests::archived_clears_failure_counter` — fixture pre-populates `.failure-state.json` with a count of 1 for a change. Run pass with `CompletingExecutorWithDiff`. After the pass, read `.failure-state.json`. Assert the change's entry is gone.
- [x] 6.3 `polling_loop::tests::threshold_reached_writes_marker_and_excludes_change` — fixture with threshold = 2. Run two Failed passes against the same change. After the second, assert `.perma-stuck.json` exists in the change dir, and `queue::list_pending` returns an empty list (change is excluded).
- [x] 6.4 `polling_loop::tests::removing_marker_re_enables_change` — same fixture but the test then deletes `.perma-stuck.json` and runs a third pass. Assert the executor was invoked again on the now-recovered change.
- [x] 6.5 `polling_loop::tests::transient_error_does_not_increment_counter` — fixture where `workspace::ensure_initialized` errors (or pre-pass step fails before executor invocation). Run pass. Assert `.failure-state.json` is unchanged (or empty).
- [x] 6.6 `polling_loop::tests::perma_stuck_alert_posts_to_chatops` — fixture with mockito chatops backend + threshold = 1 + AlwaysFailingExecutor. Run pass. Assert the mockito mock for `post_notification` was hit with a body containing "perma-stuck".

## 7. Documentation

- [x] 7.1 README "Operating Notes": new subsection "Perma-stuck change detection" describing the threshold, the marker file location and contents, the chatops alert, and how to clear the marker (delete the file). No kitschy framing.
- [x] 7.2 README "Configuration Reference → `executor:`": add `perma_stuck_after_failures` row to the table with default 2 and a one-line description.

## 8. Verification

- [x] 8.1 `cargo test` passes; net new tests ≥ 14.
- [x] 8.2 `openspec validate perma-stuck-change-detection --strict` passes.
