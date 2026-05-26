## Why

When an LLM-driven audit creates a change proposal, that proposal enters the polling iteration's queue and gets implemented by the executor on a subsequent pass. From an operator's perspective, this manifests as the daemon "starting work on a change I didn't recognize" — the existing start-of-work chatops notification fires with the change name and the first line of its `## Why`, but with no context that this change was AUDIT-generated rather than human-authored. An operator who didn't initiate the change has to guess at its provenance from naming conventions (`secure-` prefix from `security_bug_audit`, etc.) or by reading the proposal file.

A real-world example: an operator observed the daemon begin implementing `secure-bound-arp-step-count` and only inferred from the `secure-` prefix that the security audit had generated the proposal. With no notification at proposal-creation time, there was no signal in the channel pointing at the audit. The operator had to reverse-engineer the chain.

A dedicated "audit produced proposal" notification closes that gap. Fired right after the proposal validates successfully (the existing `Reported` outcome of the just-specced `a01-audit-proposal-self-validation`), it tells operators in one line: which audit ran, which change was created, and a one-line summary of the finding. The next chatops message they see about that change — the existing `🚀 starting work on ...` — then has obvious context.

## What Changes

**Fire a new chatops notification when any LLM-driven audit's proposal passes validation.** The notification is a separate signal from the audit-success log line (which is internal to the daemon) and from the start-of-work notification (which fires later when the executor picks up the change). It bridges the two: between "the audit decided this is worth doing" and "we're now doing it."

Notification text:

```
🔍 <repo-url>: <audit-type> created proposal `<change-slug>` — <first line of ## Why, truncated to 200 chars>
```

Fires for every LLM-driven audit type (`architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`). Does NOT fire for `architecture_brightline` (pure-data audit; does not generate LLM proposals; out of scope).

**Always fires.** Not gated by the audit's `notify_on_clean` setting. `notify_on_clean` controls the empty-findings success message; this notification is the opposite signal class — something WAS found, an actionable change was created. Suppressing it would defeat the spec's purpose.

**Fires AFTER validation success, BEFORE the change enters the queue.** The fire point is the `Reported { retries_used }` return path from the audit's main function (after `validate_with_retry` succeeds, before the function returns to the audit scheduler). Sequence:

1. Audit's LLM generates proposal → written to `openspec/changes/<slug>/`.
2. `openspec validate <slug> --strict` runs → passes (possibly after retries).
3. **New: chatops notification fires.**
4. Audit returns `Reported { retries_used }` to the scheduler.
5. Scheduler commits the proposal directory to git.
6. Next polling iteration enumerates the new change as pending.
7. Iteration picks it up and starts work → existing `🚀 starting work on ...` fires.

Step 3 is the new one; everything else is unchanged.

**Retry-count clause.** When `retries_used > 0`, the notification text includes a parenthetical: `— <summary> (validated on retry <N> of <max>)`. Same wording the `a01-audit-proposal-self-validation` spec uses for the success-with-retry log line, but in the channel-visible notification rather than only the daemon's internal log. Operators see "the audit's first attempt was invalid; the retry succeeded" alongside the success itself.

**No notification when the audit returns `ValidationExhausted`.** That case is covered by the existing `❌ <audit-type> produced an invalid proposal` notification from `a01-audit-proposal-self-validation`. The `🔍 created proposal` notification is the success path counterpart.

## Impact

- **Affected specs:** `orchestrator-cli` — one ADDED requirement covering the notification fire point, format, and the always-fires rule.
- **Affected code:**
  - `autocoder/src/audits/mod.rs` (or the scheduler) — add a `post_proposal_created_notification` helper that posts the `🔍` message. Wire it into the success path of every LLM-driven audit, right before the audit function returns `Reported`.
  - `autocoder/src/audits/{architecture_consultative,drift,specs_writing}.rs` — call the helper in the success path. The `architecture_brightline` audit is unchanged.
  - Tests:
    - Helper unit test: given an `audit_type`, `change_slug`, `retries_used`, `why_excerpt`, the helper formats the documented `🔍` text. With `retries_used == 0` no parenthetical; with `retries_used > 0` the parenthetical is included.
    - Integration tests per audit type: stub the LLM to return a valid proposal; assert one `post_notification` call with the documented shape; assert the helper fires AFTER `validate_with_retry` returns Ok AND BEFORE the audit returns.
    - `architecture_brightline` integration test: assert NO `post_notification` call fires from that audit's success path (the brightline audit does not generate LLM proposals).
    - `ValidationExhausted` integration test: assert the `🔍` notification does NOT fire when the audit returns `ValidationExhausted` (the `❌` notification fires instead, per `a01-audit-proposal-self-validation`).

- **Operator-visible behavior:** every time an LLM-driven audit produces a queue-bound change proposal, operators see one `🔍` chatops message naming the audit and the change. Subsequent `🚀 starting work on ...` messages for that change have a clear provenance line in the recent channel history.
- **Breaking:** no. Pure addition. Operators on the chatops channel see slightly more traffic (one extra message per audit-generated change). Sites that find this too noisy can disable LLM-driven audits or set their cadences to less frequent values; there is no per-notification suppression flag for the `🔍` since it is the inverse of `notify_on_clean` (the latter suppresses no-findings messages; suppressing findings messages would defeat the purpose).
- **Acceptance:** `cargo test` passes (new + existing). An audit run with a stubbed LLM that produces a valid proposal posts exactly one `🔍 <audit_type> created proposal \`<slug>\` — <excerpt>` notification to the resolved channel BEFORE the audit's scheduler-callable function returns.
