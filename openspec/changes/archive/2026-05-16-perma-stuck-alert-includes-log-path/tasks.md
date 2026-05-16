## 1. Code

- [x] 1.1 In `autocoder/src/polling_loop.rs::post_perma_stuck_alert`, after the existing `marker_path` computation, compute `let log_path = crate::executor::claude_cli::run_log_path(&workspace, change);`.
- [x] 1.2 Extended the `text` format!() to include a `run_log: <path>` line BEFORE the trailing operator-action sentence — operator sees the diagnostic pointer before the action they would take to re-engage.
- [x] 1.3 Added comment at the call site noting the tight coupling to Claude CLI's log convention, with a refactor hint if a second executor is added.

## 2. Tests

- [x] 2.1 Existing `perma_stuck_alert_posts_to_chatops` test continues to pass unchanged (matcher is broad enough to accept the extra line).
- [x] 2.2 New test `polling_loop::tests::perma_stuck_alert_body_contains_log_path` — fixture asserts the body contains `run_log:` AND the expected `<change_name>.log` segment using mockito's `AllOf` matcher.

## 3. Verification

- [x] 3.1 `cargo test` passes (376/377; 1 ignored, unrelated).
- [x] 3.2 `openspec validate perma-stuck-alert-includes-log-path --strict` passes.
