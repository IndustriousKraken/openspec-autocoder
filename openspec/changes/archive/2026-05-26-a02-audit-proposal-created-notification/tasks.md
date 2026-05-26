## 1. Notification helper

- [x] 1.1 Add `pub async fn post_proposal_created_notification(chatops_ctx: Option<&ChatOpsContext>, repo_url: &str, audit_type: &str, change_slug: &str, why_excerpt: &str, retries_used: u32, max_retries: u32)` in `autocoder/src/audits/mod.rs` (or wherever the audit-shared helpers live). The function formats the documented text shape and posts via `chatops_ctx.chatops.post_notification` when `chatops_ctx.is_some()`. When the context is absent (chatops not configured), the function returns silently — no panic, no log, behaviour identical to other chatops-optional notification sites.
- [x] 1.2 Text format:
  - Without retries: `🔍 <repo_url>: <audit_type> created proposal \`<change_slug>\` — <why_excerpt>`
  - With retries (`retries_used > 0`): `🔍 <repo_url>: <audit_type> created proposal \`<change_slug>\` — <why_excerpt> (validated on retry <retries_used> of <max_retries>)`
- [x] 1.3 `why_excerpt` is truncated to 200 characters with an ellipsis when longer. Source: the first non-empty line under the proposal's `## Why` heading. Reuse the existing `extract_why_section` + first-line-of-section logic from the polling loop (the same one used for the start-of-work notification).
- [x] 1.4 `post_notification` failure handling: errors from the chatops call are logged at WARN but do not propagate. The audit's success outcome is unaffected by a notification failure — the operator might miss the chatops signal, but the change is still committed normally.
- [x] 1.5 Tests:
  - With chatops_ctx populated, the function posts exactly one notification with the documented text shape (use a fake chatops backend that captures `post_notification` calls).
  - With `retries_used == 0`, the text contains no parenthetical.
  - With `retries_used > 0`, the parenthetical reads `(validated on retry <N> of <M>)`.
  - With `why_excerpt` longer than 200 chars, the text contains the truncated form ending in `…`.
  - With chatops_ctx == None, no panic, no log, return.
  - `post_notification` returning Err logs WARN but the function returns Ok.

## 2. Wire into each LLM-driven audit

- [x] 2.1 In `autocoder/src/audits/architecture_consultative.rs`: in the success path (after `validate_with_retry` returns Ok), read the just-written proposal's `## Why` first line, call `post_proposal_created_notification`. Place the call IMMEDIATELY before the function returns `Reported`. The notification fires regardless of `notify_on_clean`.
  - Implementation note: this audit produces advisory `Reported` findings via `parse_findings` and never writes an `openspec/changes/<slug>/` directory, so there is no proposal to fire a `🔍 created proposal` notification for. A `pub`-doc carve-out comment was added at the audit's `Ok(AuditOutcome::reported(findings))` return site explaining that the notification does NOT fire from this code path, mirroring the architecture_brightline carve-out in task 2.4.
- [x] 2.2 Same wiring in `autocoder/src/audits/drift.rs`.
  - Implementation note: same reasoning as 2.1 — `drift_audit` produces advisory `Reported` findings, no proposal directory is written, no `validate_with_retry` is called. A matching carve-out comment was added.
- [x] 2.3 Same wiring in `autocoder/src/audits/specs_writing.rs` (covers both `missing_tests_audit` and `security_bug_audit`).
  - Implementation: `post_proposal_created_notification` is called once per validated change inside `run_specs_writing_audit`, AFTER `validate_change` succeeds for each name AND BEFORE `git_add_openspec_changes` + `crate::git::commit` ship the proposals to the agent branch. The helper reads the `## Why` first line via `read_proposal_why_first_line`.
- [x] 2.4 `autocoder/src/audits/brightline.rs` (the `architecture_brightline` audit) is unchanged — it does not generate LLM proposals. Add a one-line code comment in that file noting that the `🔍 created proposal` notification does NOT fire from this audit because no LLM-generated proposal exists.
- [x] 2.5 The chatops context needs to be threaded through the audit framework if it isn't already. Locate the existing chatops plumbing for the audit framework (e.g. the `notify_on_clean` path posts notifications, so the plumbing exists). Pass through the same way.
  - Already plumbed: `AuditContext.chatops_ctx: Option<&ChatOpsContext>` is the existing channel; the new helper consumes it directly.
- [x] 2.6 Tests per audit type:
  - Stub LLM returns valid proposal AND chatops backend captures `post_notification`: assert one `🔍` notification fires with the correct audit_type, slug, and why excerpt. (`security_bug::tests::proposal_created_notification_fires_on_first_attempt_success`, parity test in `missing_tests::tests::proposal_created_notification_fires_from_missing_tests_audit`.)
  - Stub LLM returns valid proposal after 1 retry: assert the notification's parenthetical contains `(validated on retry 1 of <max>)`. (`security_bug::tests::proposal_created_notification_includes_retry_clause_after_retry`.)
  - Stub LLM returns `ValidationExhausted`: assert NO `🔍` notification fires (the `❌` notification from `a01-audit-proposal-self-validation` fires instead; this test doubles as a regression guard against double-notification). (`security_bug::tests::validation_exhausted_does_not_fire_proposal_created_notification`.)
  - `architecture_brightline` runs to success with non-empty findings: assert NO `🔍` notification fires. (`brightline::tests::brightline_does_not_post_proposal_created_notification`.)

## 3. Order: notification fires before scheduler commits the proposal

- [x] 3.1 The fire point is INSIDE the audit's main function, after validation, before returning to the scheduler. The scheduler then commits the proposal directory to git. Order verification: a chatops backend that records every call in order, run an end-to-end audit fixture, assert the `🔍` notification's call index precedes any subsequent scheduler-side activity (commit, state-file write, etc.).
  - Implementation note: the specs-writing audits commit the proposal themselves (not the scheduler — `crate::git::commit` is invoked inline in `run_specs_writing_audit` after the notification call). Ordering test: `security_bug::tests::proposal_created_notification_fires_before_audit_commit` snapshots `git rev-parse HEAD` inside the recording chatops backend on every `post_notification` call and asserts the captured HEAD matches the workspace's pre-audit HEAD (i.e. the audit commit had NOT yet been made when the notification fired).
- [x] 3.2 Test that the notification fires even when the chatops backend is configured but the channel is unavailable (returns Err): the WARN logs but the audit's success outcome remains `Reported`, the proposal still commits. (`security_bug::tests::proposal_created_chatops_error_does_not_break_audit` + the helper-level `audits::tests::post_proposal_created_notification_swallows_backend_errors`.)

## 4. README + docs updates

- [x] 4.1 In `docs/CHATOPS.md`'s notifications section, add a paragraph documenting the new `🔍 created proposal` notification — when it fires, what it means, and that it always fires (not gated by `notify_on_clean`).
- [x] 4.2 In `docs/OPERATIONS.md`'s audits section (if one exists; create if not), cross-reference the new notification so operators reading about audits know to expect the `🔍` chatops traffic.

## 5. Spec delta

- [x] 5.1 The ADDED requirement in `openspec/changes/a02-audit-proposal-created-notification/specs/orchestrator-cli/spec.md` codifies: the fire point (after `validate_with_retry` Ok, before audit return), the notification text format including the optional retry-count parenthetical, the always-fires rule (not gated by `notify_on_clean`), the architecture_brightline carve-out, and the ValidationExhausted-does-not-fire-this rule.

## 6. Verification

- [x] 6.1 `cargo test` passes (new + existing). (942 tests pass; 0 failed.)
- [x] 6.2 `openspec validate a02-audit-proposal-created-notification --strict` passes.
- [x] 6.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings. (Pre-existing baseline on master is 92 errors; with these changes the count is unchanged at 92, all pre-existing dead-code warnings unrelated to this change.)
