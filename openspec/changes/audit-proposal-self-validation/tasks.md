## 1. Config field for retry budget

- [ ] 1.1 In `autocoder/src/config.rs`, extend `AuditsConfig` with `pub max_validation_retries: u32` (defaults to `1` via `#[serde(default = "default_max_validation_retries")]`). Add the default function returning `1`.
- [ ] 1.2 Validation at config-load: clamp values above `5` to `5` with a WARN log at startup (runaway retries from operator misconfiguration is more harmful than a tight cap; the `5` ceiling is arbitrary but reasonable — operators who think they need 6+ retries probably have a different problem).
- [ ] 1.3 Tests:
  - Default config (no `audits.max_validation_retries` field) parses with `max_validation_retries == 1`.
  - Explicit `audits.max_validation_retries: 0` parses with `0`.
  - Explicit `audits.max_validation_retries: 5` parses with `5`, no WARN.
  - Explicit `audits.max_validation_retries: 10` parses with `5` AND emits the WARN at load.

## 2. Shared validation helper

- [ ] 2.1 In `autocoder/src/audits/mod.rs`, add `pub fn validate_proposal(workspace: &Path, slug: &str) -> Result<(), String>` that shells out to `openspec validate <slug> --strict` in the workspace, captures stderr, and returns:
  - Exit 0 → `Ok(())`
  - Exit non-zero → `Err(stderr.trim().to_string())`
  - Spawn failure → `Err(format!("openspec validate spawn failed: {e}"))`
- [ ] 2.2 Tests using mockable subprocess invocation (or against real fixture workspaces with openspec installed; tests can skip when openspec is missing, the same pattern as `cli::sync_specs::tests::rebuild_canonical_e2e_via_openspec`):
  - Validation against a fixture with a well-formed proposal returns `Ok(())`.
  - Validation against a fixture with a `MODIFIED` block whose target header doesn't exist returns `Err(stderr)` containing `not found` or similar openspec error text.
  - Validation against a fixture with a requirement body missing the SHALL keyword returns `Err(stderr)` containing the missing-keyword diagnostic.

## 3. Retry-loop helper

- [ ] 3.1 Add `pub async fn validate_with_retry<F, Fut>(workspace: &Path, slug: &str, max_retries: u32, mut llm_call: F) -> Result<RetryOutcome, ValidationExhausted>` where:
  - `F: FnMut(Option<&str>) -> Fut` — the closure takes `None` on the first call and `Some(validation_stderr)` on retries, allowing the audit to amend its LLM prompt with the validation error
  - `Fut: Future<Output = Result<(), String>>` — the closure writes the proposal to `openspec/changes/<slug>/` and returns Ok on successful write or Err with the LLM-call error message
  - Returns `Ok(RetryOutcome { retries_used: u32 })` on success
  - Returns `Err(ValidationExhausted { retries_attempted: u32, final_error: String })` when retries are exhausted
- [ ] 3.2 Loop flow:
  ```rust
  for attempt in 0..=max_retries {
      let validation_addendum = if attempt == 0 { None } else { Some(last_error.as_str()) };
      llm_call(validation_addendum).await?;          // writes change dir; propagate LLM-call errors as ValidationExhausted with final_error = "llm-call failed: ..."
      match validate_proposal(workspace, slug) {
          Ok(()) => return Ok(RetryOutcome { retries_used: attempt }),
          Err(e) => last_error = e,
      }
  }
  Err(ValidationExhausted { retries_attempted: max_retries, final_error: last_error })
  ```
- [ ] 3.3 Tests:
  - `max_retries=0`, stubbed `llm_call` writes valid proposal → returns `Ok(RetryOutcome { retries_used: 0 })`.
  - `max_retries=0`, stubbed `llm_call` writes invalid proposal → returns `Err(ValidationExhausted { retries_attempted: 0, .. })`.
  - `max_retries=1`, stubbed `llm_call` writes invalid on attempt 0 and valid on attempt 1 → returns `Ok(RetryOutcome { retries_used: 1 })`. Assert the addendum on attempt 1 contained the validation error from attempt 0.
  - `max_retries=1`, stubbed `llm_call` writes invalid on both attempts → returns `Err(ValidationExhausted { retries_attempted: 1, .. })`.
  - `max_retries=2`, valid on attempt 2 → returns `Ok(RetryOutcome { retries_used: 2 })`.

## 4. Discard helper

- [ ] 4.1 Add `pub async fn discard_proposal_and_notify(workspace: &Path, slug: &str, audit_type: &str, retries_attempted: u32, final_error: &str, chatops_ctx: Option<&ChatOpsContext>) -> Result<()>`. Actions in order:
  1. Remove `openspec/changes/<slug>/` recursively if it exists; ignore NotFound.
  2. If `chatops_ctx.is_some()`, post the documented `❌` notification (text formatted as below). Wrap in best-effort error handling — `post_notification` errors are logged but do not propagate.
  3. Return `Ok(())` (the discard always succeeds from the orchestration layer's perspective).
- [ ] 4.2 Notification text format:
  ```
  ❌ <repo-url>: <audit-type> produced an invalid proposal that failed openspec validation after <N> retries.
  Final validation error:
  <truncated stderr, capped at 800 chars with ellipsis>
  No commit was made. The audit will retry on its next scheduled cadence.
  ```
- [ ] 4.3 Tests:
  - With chatops_ctx populated, asserts one `post_notification` call with the documented text shape.
  - Without chatops_ctx, the directory is removed and no panics occur.
  - `post_notification` failure does not propagate; the function still returns `Ok(())`.

## 5. Wire every LLM-driven audit through the retry loop

- [ ] 5.1 `autocoder/src/audits/architecture_consultative.rs`: refactor the existing LLM-call site so its proposal-writing step is a closure passed to `validate_with_retry`. On `Ok(RetryOutcome)`, return the existing `AuditOutcome::Reported` (or whatever variant fits) with the `retries_used` field populated. On `Err(ValidationExhausted)`, call `discard_proposal_and_notify` then return `AuditOutcome::ValidationExhausted`.
- [ ] 5.2 Same shape for `autocoder/src/audits/drift.rs`.
- [ ] 5.3 Same shape for `autocoder/src/audits/specs_writing.rs` (which hosts both `missing_tests_audit` and `security_bug_audit`).
- [ ] 5.4 The `architecture_brightline` audit does NOT generate proposals via LLM (it's pure-data file-line-counting), so it is unaffected by this change. Document this in a code comment.
- [ ] 5.5 Tests per audit type:
  - Stub LLM returns valid proposal on first attempt → audit completes normally with `retries_used=0`.
  - Stub LLM returns invalid then valid (max_retries=1) → audit completes with `retries_used=1`.
  - Stub LLM returns invalid both times (max_retries=1) → audit returns `ValidationExhausted`, change directory does NOT exist after the call, chatops notification fires.

## 6. AuditOutcome enum extension

- [ ] 6.1 Extend `pub enum AuditOutcome` in `autocoder/src/audits/mod.rs` with:
  ```rust
  ValidationExhausted {
      audit_type: String,
      retries_attempted: u32,
      final_error: String,
  }
  ```
- [ ] 6.2 Update the existing `Reported` variant (or whatever the LLM-driven audits use for success) to carry `retries_used: u32` so success-with-retry can be distinguished from success-on-first-attempt in logs and the chatops notification.
- [ ] 6.3 Update the scheduler's outcome handling at `autocoder/src/audits/scheduler.rs` to:
  - Log `ValidationExhausted` at WARN with all three fields.
  - When `retries_used > 0` in a `Reported` outcome, append `" (validated on retry <N> of <M>)"` to the existing success log line (and to the chatops notification when `notify_on_clean=true`).
- [ ] 6.4 Tests:
  - Scheduler handling of `ValidationExhausted` logs the WARN with all fields present.
  - Scheduler handling of `Reported { retries_used: 0 }` is unchanged (no retry-clause in the log).
  - Scheduler handling of `Reported { retries_used: 2 }` includes "validated on retry 2 of N" in the log line.

## 7. Audit-state JSON history

- [ ] 7.1 Each audit type's state file (under `<workspace>/.autocoder/audit-state/<audit-type>.json` or wherever the existing state lives) gains an `attempt_history: Vec<AttemptEntry>` field where each entry records:
  ```rust
  pub struct AttemptEntry {
      pub when: chrono::DateTime<Utc>,
      pub outcome_kind: String,  // "Reported" | "NoFindings" | "ValidationExhausted" | etc.
      pub retries_used: u32,
      pub error_excerpt: Option<String>,  // first 200 chars of final_error for ValidationExhausted
  }
  ```
  Existing fields are preserved. History entries SHALL be capped at the most recent 20 entries (FIFO) so the file stays bounded.
- [ ] 7.2 Tests:
  - Fixture state file with no `attempt_history` field parses cleanly (backwards-compatible).
  - After a successful audit run, the state file gains one entry with `outcome_kind: "Reported"` and the correct `retries_used`.
  - After a `ValidationExhausted` audit run, the state file gains one entry with `outcome_kind: "ValidationExhausted"` and the truncated `error_excerpt`.
  - After 25 audit runs, the state file's history contains exactly 20 entries (the most recent).

## 8. README + docs/CONFIG.md updates

- [ ] 8.1 Add a paragraph in `docs/CONFIG.md`'s `audits:` reference section documenting the new `max_validation_retries` field (default `1`, max `5`, semantics).
- [ ] 8.2 Add a paragraph in `docs/CHATOPS.md` documenting the new `❌ <audit-type> produced an invalid proposal` notification — when it fires, what it means, what the operator should do (typically: review the audit's prompt template if the audit-type fails validation repeatedly).
- [ ] 8.3 Add an entry in `docs/TROUBLESHOOTING.md` for the new failure mode: "Audit produces invalid proposal — what to do." Cross-reference the related cascade-prevention specs (`queue-archive-aborted-detection`, `pr-body-proposal-active-path-fallback`).

## 9. Spec delta

- [ ] 9.1 The ADDED requirement in `openspec/changes/audit-proposal-self-validation/specs/orchestrator-cli/spec.md` codifies: the post-write validation obligation, the retry loop semantics (including the validation-error addendum on retry attempts), the discard-and-notify path on exhaustion, the chatops notification rules for both the success-with-retry and the validation-exhausted cases, the config field, and the audit-state history extension.

## 10. Verification

- [ ] 10.1 `cargo test` passes (new + existing).
- [ ] 10.2 `openspec validate audit-proposal-self-validation --strict` passes.
- [ ] 10.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
