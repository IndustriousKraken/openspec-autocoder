## 1. Notification helper

- [ ] 1.1 Add `pub async fn post_proposal_created_notification(chatops_ctx: Option<&ChatOpsContext>, repo_url: &str, audit_type: &str, change_slug: &str, why_excerpt: &str, retries_used: u32, max_retries: u32)` in `autocoder/src/audits/mod.rs` (or wherever the audit-shared helpers live). The function formats the documented text shape and posts via `chatops_ctx.chatops.post_notification` when `chatops_ctx.is_some()`. When the context is absent (chatops not configured), the function returns silently — no panic, no log, behaviour identical to other chatops-optional notification sites.
- [ ] 1.2 Text format:
  - Without retries: `🔍 <repo_url>: <audit_type> created proposal \`<change_slug>\` — <why_excerpt>`
  - With retries (`retries_used > 0`): `🔍 <repo_url>: <audit_type> created proposal \`<change_slug>\` — <why_excerpt> (validated on retry <retries_used> of <max_retries>)`
- [ ] 1.3 `why_excerpt` is truncated to 200 characters with an ellipsis when longer. Source: the first non-empty line under the proposal's `## Why` heading. Reuse the existing `extract_why_section` + first-line-of-section logic from the polling loop (the same one used for the start-of-work notification).
- [ ] 1.4 `post_notification` failure handling: errors from the chatops call are logged at WARN but do not propagate. The audit's success outcome is unaffected by a notification failure — the operator might miss the chatops signal, but the change is still committed normally.
- [ ] 1.5 Tests:
  - With chatops_ctx populated, the function posts exactly one notification with the documented text shape (use a fake chatops backend that captures `post_notification` calls).
  - With `retries_used == 0`, the text contains no parenthetical.
  - With `retries_used > 0`, the parenthetical reads `(validated on retry <N> of <M>)`.
  - With `why_excerpt` longer than 200 chars, the text contains the truncated form ending in `…`.
  - With chatops_ctx == None, no panic, no log, return.
  - `post_notification` returning Err logs WARN but the function returns Ok.

## 2. Wire into each LLM-driven audit

- [ ] 2.1 In `autocoder/src/audits/architecture_consultative.rs`: in the success path (after `validate_with_retry` returns Ok), read the just-written proposal's `## Why` first line, call `post_proposal_created_notification`. Place the call IMMEDIATELY before the function returns `Reported`. The notification fires regardless of `notify_on_clean`.
- [ ] 2.2 Same wiring in `autocoder/src/audits/drift.rs`.
- [ ] 2.3 Same wiring in `autocoder/src/audits/specs_writing.rs` (covers both `missing_tests_audit` and `security_bug_audit`).
- [ ] 2.4 `autocoder/src/audits/brightline.rs` (the `architecture_brightline` audit) is unchanged — it does not generate LLM proposals. Add a one-line code comment in that file noting that the `🔍 created proposal` notification does NOT fire from this audit because no LLM-generated proposal exists.
- [ ] 2.5 The chatops context needs to be threaded through the audit framework if it isn't already. Locate the existing chatops plumbing for the audit framework (e.g. the `notify_on_clean` path posts notifications, so the plumbing exists). Pass through the same way.
- [ ] 2.6 Tests per audit type:
  - Stub LLM returns valid proposal AND chatops backend captures `post_notification`: assert one `🔍` notification fires with the correct audit_type, slug, and why excerpt.
  - Stub LLM returns valid proposal after 1 retry: assert the notification's parenthetical contains `(validated on retry 1 of <max>)`.
  - Stub LLM returns `ValidationExhausted`: assert NO `🔍` notification fires (the `❌` notification from `audit-proposal-self-validation` fires instead; this test doubles as a regression guard against double-notification).
  - `architecture_brightline` runs to success with non-empty findings: assert NO `🔍` notification fires.

## 3. Order: notification fires before scheduler commits the proposal

- [ ] 3.1 The fire point is INSIDE the audit's main function, after validation, before returning to the scheduler. The scheduler then commits the proposal directory to git. Order verification: a chatops backend that records every call in order, run an end-to-end audit fixture, assert the `🔍` notification's call index precedes any subsequent scheduler-side activity (commit, state-file write, etc.).
- [ ] 3.2 Test that the notification fires even when the chatops backend is configured but the channel is unavailable (returns Err): the WARN logs but the audit's success outcome remains `Reported`, the proposal still commits.

## 4. README + docs updates

- [ ] 4.1 In `docs/CHATOPS.md`'s notifications section, add a paragraph documenting the new `🔍 created proposal` notification — when it fires, what it means, and that it always fires (not gated by `notify_on_clean`).
- [ ] 4.2 In `docs/OPERATIONS.md`'s audits section (if one exists; create if not), cross-reference the new notification so operators reading about audits know to expect the `🔍` chatops traffic.

## 5. Spec delta

- [ ] 5.1 The ADDED requirement in `openspec/changes/audit-proposal-created-notification/specs/orchestrator-cli/spec.md` codifies: the fire point (after `validate_with_retry` Ok, before audit return), the notification text format including the optional retry-count parenthetical, the always-fires rule (not gated by `notify_on_clean`), the architecture_brightline carve-out, and the ValidationExhausted-does-not-fire-this rule.

## 6. Verification

- [ ] 6.1 `cargo test` passes (new + existing).
- [ ] 6.2 `openspec validate audit-proposal-created-notification --strict` passes.
- [ ] 6.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
