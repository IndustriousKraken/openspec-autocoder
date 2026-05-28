## ADDED Requirements

### Requirement: Slack inbound listener captures `thread_ts` AND `user` from the `AppMentionEvent` AND forwards them to `handle_message_with_context`
The Slack inbound listener's `AppMentionEvent` deserializer struct SHALL include `thread_ts: Option<String>` (annotated `#[serde(default)]` so top-level mentions, which Slack delivers WITHOUT a `thread_ts` field, deserialize cleanly with `None`). The existing `user` field on the struct is unchanged.

The production inbound-dispatch call site SHALL invoke `OperatorCommandDispatcher::handle_message_with_context(text, channel, thread_ts, operator_user, bot_mention, repositories, submitter)`, passing `event.thread_ts.as_deref()` AND `event.user.as_deref()`. The production listener SHALL NOT call the no-context `handle_message(text, channel, bot_mention, repositories, submitter)` entry point — that helper SHALL be either deleted OR annotated `#[cfg(test)]` so the production build cannot link against it. This compile-time guard prevents future regressions of the same wiring bug.

The contract this requirement enforces: any verb whose recognition depends on thread context (`send it` per the canonical "send it verb in an audit thread schedules a triage executor run" requirement; future verbs added by stacked specs) SHALL see the inbound envelope's `thread_ts` at parse time. Any verb whose dispatch records the issuing operator (verbs that populate `operator_user`, `marked_by`, OR equivalent attribution fields in state files) SHALL see the actual Slack user id, not the empty-string fallback.

#### Scenario: Threaded reply propagates thread_ts to the dispatcher
- **WHEN** Slack delivers an `app_mention` event with `text: "<@BOT> send it"`, `channel: "C0"`, `ts: "1.0"`, `user: Some("U_RAB")`, AND `thread_ts: Some("9999.1234")`
- **AND** an `AuditThreadState` exists for `thread_ts: "9999.1234"` with `status: Open`
- **THEN** the listener deserializes `event.thread_ts == Some("9999.1234")`
- **AND** the listener invokes `handle_message_with_context` with `thread_ts: Some("9999.1234")` AND `operator_user: Some("U_RAB")`
- **AND** the dispatcher's send-it handler is reached (the parser produces `ParseOutcome::Ok(SendItOnAudit { thread_ts: "9999.1234" })`)
- **AND** the `trigger_audit_action` control-socket submission fires with the correct `thread_ts`
- **AND** the listener does NOT apply the `?` (question) reaction

#### Scenario: Top-level mention deserializes with thread_ts None
- **WHEN** Slack delivers an `app_mention` event WITHOUT a `thread_ts` field (top-level channel mention)
- **THEN** the `AppMentionEvent` deserializes with `event.thread_ts == None`
- **AND** the listener invokes `handle_message_with_context` with `thread_ts: None`
- **AND** top-level verbs (`propose`, `audit`, `changelog`, `status`, etc.) parse normally
- **AND** `send it` (which requires `thread_ts: Some(non-empty)`) correctly returns `ParseOutcome::None` at the parser
- **AND** the listener applies the `?` reaction for the rejected `send it` (the canonical "Unrecognised verbs get a `?` reaction" requirement)

#### Scenario: Production build cannot link against the no-context entry point
- **WHEN** a maintainer inspects `autocoder/src/chatops/operator_commands.rs`
- **THEN** `pub async fn handle_message(...)` either does NOT exist OR is annotated `#[cfg(test)]`
- **AND** `cargo build --release` succeeds (no production code path calls the deleted/test-only function)
- **AND** any future code that re-introduces the call to the no-context entry point in production fails to compile

#### Scenario: Operator-user attribution lands in state files
- **WHEN** an operator runs `@<bot> propose <repo> <text>` (Slack delivers `user: Some("U_RAB")`)
- **THEN** the resulting `ProposalRequestState` file's `operator_user` field contains `"U_RAB"` (the actual Slack user id)
- **AND** the field is NOT the empty-string default that the pre-spec wiring produced

#### Scenario: Regression test exercises the propagation contract
- **WHEN** the test suite runs
- **THEN** at least one test constructs a synthetic `app_mention` event with `thread_ts: Some(_)` AND drives the inbound handler against a pre-stamped `AuditThreadState`
- **AND** the test asserts the `trigger_audit_action` is submitted with the correct `thread_ts` (the listener's dispatch call propagated context correctly)
- **AND** the test asserts the listener does NOT apply the `?` reaction
- **AND** a parallel test with `thread_ts: None` asserts `send it` is correctly refused with the `?` reaction (preserving the documented behaviour of `send it` outside an audit-thread context)
