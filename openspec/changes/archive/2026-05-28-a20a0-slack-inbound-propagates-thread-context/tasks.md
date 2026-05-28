## 1. Capture thread_ts from the Slack inbound envelope

- [x] 1.1 In `autocoder/src/chatops/slack.rs`, extend `AppMentionEvent` (around line 631) with:
  ```rust
  #[serde(default)]
  pub thread_ts: Option<String>,
  ```
  The `#[serde(default)]` annotation handles top-level mentions, where Slack does NOT include `thread_ts` in the payload — they deserialize with `None`.
- [x] 1.2 Verify (or add) a unit test that deserializes a fixture Slack `app_mention` payload with `thread_ts: "9999.1234"` AND asserts `event.thread_ts == Some("9999.1234".to_string())`. Also test the top-level-mention case (no `thread_ts` field present) deserializes to `event.thread_ts.is_none()`.

## 2. Swap the production dispatch call to forward context

- [x] 2.1 At `autocoder/src/chatops/slack.rs:1184` (the `ctx.dispatcher.handle_message(...)` call), replace the call with `handle_message_with_context(...)`. The new arguments:
  ```rust
  let reply = ctx.dispatcher.handle_message_with_context(
      &normalized_text,
      &event.channel,
      event.thread_ts.as_deref(),
      event.user.as_deref(),
      &bot_mention,
      &repos,
      &submitter,
  ).await;
  ```
- [x] 2.2 Verify there are no other production call sites of `handle_message` in `autocoder/src/`. Search: `grep -rn "\.handle_message(" autocoder/src/`. Any non-test hit needs the same swap.

## 3. Prevent regression: mark `handle_message` test-only OR delete it

Pick ONE of the two options based on test-suite impact:

- [ ] 3.1 ~~**Option A — Delete `handle_message`.**~~ Not chosen — 60+ test callers; Option B is dramatically less churn.
- [x] 3.2 **Option B — Annotate `handle_message` `#[cfg(test)]`.** Both `handle_message` AND `handle_message_in_thread` are now `#[cfg(test)]`-gated; the production build cannot link against them. The `#[allow(dead_code)]` annotation is removed (no longer needed under `cfg(test)`).
- [x] 3.3 EITHER option closes the regression vector: any future production code that tries to call the no-context entry point fails to compile, surfacing the wiring contract at compile time.
- [x] 3.4 Verify: `cargo build --release` succeeds; `cargo test --bin autocoder chatops::slack::` passes (61/61).

## 4. Regression-prevention integration test

- [x] 4.1 Added `slack_inbound_propagates_thread_ts_to_dispatcher_for_send_it` in `autocoder/src/chatops/slack.rs`'s test module. Constructs the AppMentionEvent with thread_ts: Some("9999.1234"), pre-stamps an AuditThreadState, replicates the production dispatch call shape with a RecordingSubmitter, AND asserts `trigger_audit_action` is submitted with thread_ts "9999.1234".
- [x] 4.2 The test passes against the post-fix code (verified via `cargo test`). Pre-fix it would have failed because handle_message dropped thread_ts → ParseOutcome::None → no action submitted.
- [x] 4.3 Added `slack_inbound_send_it_outside_thread_still_refused` — same dispatcher setup but thread_ts: None. Asserts the dispatcher returns None AND no action is submitted, confirming top-level `send it` is correctly refused.

## 5. Spec deltas

- [x] 5.1 `openspec/changes/a20a0-slack-inbound-propagates-thread-context/specs/chatops-manager/spec.md` ADDs the listener-propagation requirement covering AppMentionEvent shape, the production call-site contract, AND the regression-test invariant. Validated via `openspec validate ... --strict`.

## 6. Verification

- [x] 6.1 `cargo test --bin autocoder`: 1600 passed, 0 failed, 2 ignored. New regression tests in slack.rs pass; existing parser-level + dispatcher-level tests in operator_commands.rs unchanged.
- [x] 6.2 `openspec validate a20a0-slack-inbound-propagates-thread-context --strict` passes.
- [x] 6.3 Clippy on touched files (`src/chatops/slack.rs`, `src/chatops/operator_commands.rs`) is clean — no new warnings in the lines I added. (Pre-existing 51 strict-mode warnings in other files are unchanged from baseline; matches the a10-era implementation note pattern.)
- [ ] 6.4 Manual verification on the live daemon — deferred to operator after the daemon picks up this change.
