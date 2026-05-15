## 1. AlertCategory addition

- [x] 1.1 In `autocoder/src/alert_state.rs`, add `WorkspaceDirtyMidIteration` to the `AlertCategory` enum. Order it after `WorkspaceInitFailure` for narrative grouping.
- [x] 1.2 Add the label in `AlertCategory::label()`: `"workspace dirty mid-iteration"`.
- [x] 1.3 Verify no `match AlertCategory` site outside the enum needs updating. Rust's exhaustiveness check on the `label()` impl confirms.

## 2. Wire the alert into the dirty-workspace branch

- [x] 2.1 In `autocoder/src/polling_loop.rs::run_pass_through_commits`, the dirty-workspace branch previously read:
  ```rust
  if !dirty_filtered.is_empty() {
      return Err(anyhow!(
          "workspace {} is dirty before pass; refusing to proceed:\n{dirty_filtered}",
          workspace.display()
      ));
  }
  ```
- [x] 2.2 Wrap the `Err` in a `handle_predictable_failure` call mirroring the `WorkspaceInitFailure` pattern. Build the error first, pass it to the handler, then return the same `Err`:
  ```rust
  if !dirty_filtered.is_empty() {
      let e = anyhow!(
          "workspace {} is dirty before pass; refusing to proceed:\n{dirty_filtered}",
          workspace.display()
      );
      handle_predictable_failure(
          workspace,
          &repo.url,
          chatops_ctx,
          chatops_ctx.map(|c| c.failure_alerts_enabled).unwrap_or(false),
          AlertCategory::WorkspaceDirtyMidIteration,
          &e,
      )
      .await;
      return Err(e);
  }
  ```

## 3. Tests

- [x] 3.1 `polling_loop::tests::dirty_workspace_emits_alert_when_chatops_configured` — fixture: workspace pre-seeded with an uncommitted file under `openspec/changes/`. Call `run_pass_through_commits` with chatops_ctx + failure_alerts_enabled = true. Asserts: (a) returned `Err` naming "dirty before pass", (b) `.alert-state.json` contains `WorkspaceDirtyMidIteration`, (c) mockito server saw exactly one `chat.postMessage`.
- [x] 3.2 `dirty_workspace_suppresses_within_throttle` — covered by existing tests in `alerts::tests` (e.g. `repeat_within_24h_is_silent`) which exhaustively exercise the 24h throttle for all `AlertCategory` variants. The throttle is shared logic in `handle_predictable_failure`; adding a category does not change its semantics.
- [x] 3.3 `polling_loop::tests::dirty_workspace_silent_without_chatops` — same fixture, `chatops_ctx = None`. Asserts: (a) returned `Err`, (b) no `.alert-state.json` file written (handle_predictable_failure short-circuits on missing ctx).
- [x] 3.4 `alerts::tests::format_alert_text_workspace_dirty_mid_iteration` — pure unit test, verifies `format_alert_text` produces a sensible string for the new category.

## 4. Verification

- [x] 4.1 `cargo test` passes. (370/371; 1 ignored is unrelated.)
- [x] 4.2 `openspec validate alert-on-dirty-workspace-mid-iteration --strict` passes.
