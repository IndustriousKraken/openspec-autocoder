## Why

Production operator feedback: `@<bot> send it` has never worked in Slack on this daemon. Every invocation returns the `?` reaction (the "unrecognised verb" fallback) with no log line. Diagnosis traced the failure end-to-end:

**The send-it parser** at `autocoder/src/chatops/operator_commands.rs` requires `thread_ts: Some(non_empty)`:

```rust
let ts = match thread_ts {
    Some(s) if !s.is_empty() => s.to_string(),
    _ => return ParseOutcome::None,
};
```

Returning `ParseOutcome::None` propagates as `Option<Reply>::None` from the dispatcher, which the Slack inbound listener interprets as "this verb is unknown" AND surfaces via the canonical `?` reaction (per `chatops-manager`'s "Unrecognised verbs get a `?` reaction" requirement).

**The dispatcher** has two entry points. `handle_message_with_context(text, channel, thread_ts, operator_user, …)` accepts the thread context the parser needs. `handle_message(text, channel, bot_mention, repos, submitter)` forwards `thread_ts: None` AND `operator_user: None`. A doc comment on the thread-aware variant explicitly says: `// used by tests; production path goes via handle_message_with_context`.

**The production Slack inbound listener** at `autocoder/src/chatops/slack.rs:1184` calls the OLD `handle_message`, NOT `handle_message_with_context`. Two compounding defects:

1. `AppMentionEvent` (the deserializer struct for Slack's inbound payload) has no `thread_ts` field — even though Slack delivers it under that exact key on reply events. So the listener cannot capture the thread context even if it wanted to.
2. The call site passes only `(text, channel, bot_mention, repos, submitter)` to the dispatcher, with no thread or user context.

Net effect: every `send it` invocation has `thread_ts=None` at the parser, fails to parse, falls through to `?` reaction. The audit-thread state files ARE being written correctly by the scheduler; the read path is never reached because the parser bails before the dispatcher's send-it handler runs.

**Secondary impact:** the same wiring bug drops `operator_user`. Verbs that record the issuing operator in state files (e.g. `propose`, `clear-perma-stuck`-style operator-audit fields) store the empty-string default instead of the actual Slack user id. Operator attribution is silently lost on every chat-initiated state-file write.

The fix is two lines: capture `thread_ts` AND `user` from the Slack envelope; pass them through to `handle_message_with_context`. The complementary surgery is to remove the misleading `#[allow(dead_code)]` annotation from `handle_message_in_thread` AND mark the no-context `handle_message` as test-only (or delete it outright) to prevent the same regression.

## What Changes

**`AppMentionEvent` SHALL capture thread context from the Slack inbound envelope.** Two new fields, both `#[serde(default)]` so top-level mentions (which lack these fields) continue to deserialize:

```rust
pub struct AppMentionEvent {
    // existing fields unchanged
    #[serde(default)]
    pub thread_ts: Option<String>,
    // `user` already exists; no schema change for the user side.
}
```

Slack populates `thread_ts` only on reply events (messages posted inside an existing thread). Top-level mentions deserialize with `thread_ts: None`, which is the correct state for verbs that ARE valid at top level (e.g. `propose`, `audit`, `changelog`).

**The Slack inbound listener's production dispatch call SHALL pass thread AND user context.** The current call site:

```rust
let reply = ctx.dispatcher.handle_message(
    &normalized_text, &event.channel, &bot_mention, &repos, &submitter,
).await;
```

becomes:

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

**The dispatcher's no-context entry point (`handle_message`) SHALL be marked test-only OR removed.** Its current `#[allow(dead_code)]` annotation has been misleading — the production listener WAS using it (incorrectly) for months. Two acceptable resolutions:

- Delete `handle_message` entirely AND have any production-or-test caller use `handle_message_with_context` directly.
- Annotate `handle_message` with `#[cfg(test)]` so the production build cannot link against it. The compiler then prevents the same regression in any future call site.

The implementer picks one based on what touches less surface area in the test suite. The spec mandates the OUTCOME (no production call path SHALL reach the parser with `thread_ts: None` when the inbound envelope DID carry a thread_ts).

**Regression-prevention test.** A new integration test SHALL simulate a Slack `app_mention` event with `thread_ts: Some("9999.1234")` AND assert that the dispatcher's send-it handler is reached (or, equivalently, that the parser produces `ParseOutcome::Ok(SendItOnAudit { thread_ts: "9999.1234" })` rather than `ParseOutcome::None`). The test SHALL pass after the fix AND fail against the pre-fix code, so future refactors that drop the wiring re-trigger the alarm.

**Other inbound listeners (Discord, Teams, Mattermost, Matrix — experimental).** This spec only mandates the Slack listener fix, since Slack is the canonical-supported backend AND the operator-reported defect is on Slack. The experimental backends MAY have the same defect; their fixes (if needed) ship in separate changes tagged with their respective backend.

## Impact

- **Affected specs:**
  - `chatops-manager` — ADDED requirement: `Slack inbound listener captures thread_ts AND user from the AppMentionEvent AND forwards them to handle_message_with_context`. Covers the AppMentionEvent struct shape, the production call-site contract, AND the regression-prevention test invariant.
- **Affected code:**
  - `autocoder/src/chatops/slack.rs` — extend `AppMentionEvent` with `thread_ts: Option<String>` (line ~631); swap the dispatcher call from `handle_message` to `handle_message_with_context` (line ~1184) passing `event.thread_ts.as_deref()` AND `event.user.as_deref()`.
  - `autocoder/src/chatops/operator_commands.rs` — either delete `handle_message` (the no-context entry point at line 1845) OR annotate it with `#[cfg(test)]`. Remove `#[allow(dead_code)]` from `handle_message_in_thread` if the test annotation lands; that helper is now production-adjacent.
  - `autocoder/tests/` — new integration test (OR extension of an existing chatops integration test) covering the thread-context propagation.
- **Operator-visible behavior:**
  - `@<bot> send it` posted inside an audit thread reaches the dispatcher's send-it handler. Operators see `✓ acted on audit findings; triage queued (~Nm).` (or the appropriate stale/already-acted reply) instead of the `?` reaction.
  - `@<bot> propose <repo> <text>` (when posted at top level) AND other state-file-writing verbs record the actual Slack user id in `operator_user` / `marked_by` fields instead of the empty-string default. Operators inspecting state files see proper attribution.
  - No new config knobs. No behavioural change to verbs that don't depend on thread/user context.
- **Breaking:** no. The change extends `AppMentionEvent` with optional fields (forward-compatible with old Slack payloads — `#[serde(default)]` handles absence) AND swaps a function call to a strict-superset variant.
- **Acceptance:** `cargo test` passes (existing + new regression test); `openspec validate a31-slack-inbound-propagates-thread-context --strict` passes; `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings. Manual verification on the live daemon: an operator posts `@<bot> send it` in an audit's threaded notification AND the bot's response transitions from `?` reaction to one of the documented send-it replies (`✓ acted on …`, `✗ thread stale`, `✗ thread already acted on`, etc.).
