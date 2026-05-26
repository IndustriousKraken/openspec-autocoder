## Why

The LLM-driven audits (`security_bug_audit`, `missing_tests_audit`, `architecture_consultative`, `drift_audit`) generate OpenSpec change proposals as their output. The proposal is written to `openspec/changes/<slug>/` with `proposal.md`, `tasks.md`, and `specs/<capability>/spec.md` delta files. The change then enters the polling iteration's normal queue and gets implemented by the executor in a subsequent pass.

The audits do not currently validate their generated proposals before committing them. When the LLM hallucinates a `## MODIFIED Requirements` block targeting a requirement header that does not actually exist in canonical state — a common failure mode for content-generating LLMs working against a large, evolving spec corpus — the audit-generated change ships into the queue with a delta that openspec will refuse to apply. The downstream symptoms cascade through the iteration:

1. The polling iteration enumerates the change and runs the executor against it.
2. The executor implements the source-code changes the audit described (since those are derivable from natural-language guidance independent of the spec delta correctness).
3. The iteration's archive step calls `queue::archive` → openspec sees the broken delta → silently aborts (the bug separately fixed by `queue-archive-aborted-detection`).
4. The PR opens with the implementation commits but the change directory remains at `openspec/changes/<slug>/`.
5. After merge, the next iteration enumerates the same change again, self-heal probes, archive silently fails again, two iterations in a row → perma-stuck.

The downstream fixes (the rebuild/self-heal `Aborted.` detection, the PR-body active-path fallback) make this cascade *visible*. They do not prevent it. The right place to catch the broken proposal is at the audit boundary — before the proposal directory is committed to the workspace, before any iteration tries to act on it.

`openspec validate <slug> --strict` already exists and is precisely the right check: it runs the same validation openspec performs internally at archive time, but as an explicit pre-flight. If the audit's just-written proposal fails validation, the audit knows immediately that the LLM produced invalid content. With the validation error in hand, the audit can either discard the proposal cleanly (operator sees "audit failed; here's why") or feed the error back to the LLM for one retry attempt (handles the common "transient hallucination" case without operator intervention).

The retry is bounded: one attempt by default, operator-configurable. The retry handles the typical case where the LLM made a single fixable error (wrong header name, missing `SHALL`, missing `#### Scenario:` block) and can self-correct when shown the error. After the retry budget is exhausted, the proposal is discarded — the autocoder does not commit invalid content.

## What Changes

**Every LLM-driven audit validates its generated proposal post-write.** After the audit writes `openspec/changes/<slug>/` to the workspace, but BEFORE the audit returns success, autocoder runs `openspec validate <slug> --strict` against the just-written directory. The exit code and stderr drive the next step:

- Exit 0 → proposal is valid; audit returns success normally.
- Exit non-zero → proposal is invalid; enter the retry-or-discard path.

**One retry by default, configurable.** A new config field `audits.max_validation_retries` (default `1`, `u32`) controls retry budget. When validation fails and retries remain:

1. Capture openspec's stderr (the validation error message).
2. Re-invoke the audit's underlying LLM call with an appended addendum: the original prompt, the LLM's previous response, then `"Your previous response produced this proposal which failed openspec validation:\n\n<validation-stderr>\n\nPlease correct the proposal and reply with the full revised content."`
3. Overwrite the change directory with the new response (delete-and-rewrite is simpler than diffing).
4. Re-run `openspec validate <slug> --strict`.
5. On exit 0 → success.
6. On exit non-zero AND retries-remaining > 0 → repeat step 1.
7. On exit non-zero AND retries-remaining == 0 → discard path.

**Discard path.** When retries are exhausted, the audit:

1. Removes the entire `openspec/changes/<slug>/` directory (the proposal is discarded; nothing about it lands in git).
2. Records the failure in the daemon's audit-state JSON so the next audit run honours the audit-type's cadence (a validation-failure does not count as a successful run; the cadence should retry naturally on the next due-date).
3. Posts a chatops failure notification to the repo's resolved chatops channel:
   ```
   ❌ <repo>: <audit-type> produced an invalid proposal that failed openspec validation after <N> retries.
   Final validation error:
   <truncated stderr>
   No commit was made. The audit will retry on its next scheduled cadence.
   ```
4. Returns a structured `AuditOutcome::ValidationExhausted { audit_type, retries_attempted, final_error }` so the orchestration layer can record it in metrics / logs distinctly from genuine "no findings" outcomes.

**Notification suppression for genuine no-finding cases.** Existing `notify_on_clean` semantics are unchanged. The new `❌` validation-exhausted notification fires regardless of that flag (an audit that produces an invalid proposal IS a notable event; suppressing it would hide a real LLM/audit-prompt issue from the operator).

**Retry counts in `notify_on_clean = true` mode.** When an audit succeeds AFTER retries (e.g. retried once, second attempt validated), the chatops success notification gains a clause: `"<audit-type> succeeded (validated on retry N of M)"`. This is informational — operators tracking audit reliability over time can see "this audit type retries a lot" as a signal that the prompt template might benefit from tightening.

**Retry telemetry.** Each audit run's outcome is annotated with `validation_retries_used: u32` so internal logging and the chatops notification can surface the count. No external telemetry endpoint is added in this change; the field exists so future observability work has a hook.

## Impact

- **Affected specs:** `orchestrator-cli` — one ADDED requirement codifying the post-write validation contract, the retry loop, the discard path, and the chatops notification rules.
- **Affected code:**
  - `autocoder/src/config.rs` — add `audits.max_validation_retries: u32` (default `1`) to `AuditsConfig`, validated at load time (cap at some sensible upper bound like 5 to prevent runaway retries from an over-eager operator config).
  - `autocoder/src/audits/mod.rs` — add a shared post-write validation helper `validate_proposal(workspace, slug) -> Result<(), String>` that shells out to `openspec validate <slug> --strict` and returns the stderr on non-zero exit. Add a shared retry-loop helper that takes a closure (the audit's LLM-call generator), runs the closure, validates, and on failure invokes the closure again with an addendum until success or exhaustion.
  - `autocoder/src/audits/{architecture_consultative,drift,specs_writing}.rs` — wire each audit's existing LLM-call site through the new retry helper. The retry budget comes from the resolved `audits.max_validation_retries`. The discard-on-exhaustion path is the same across all audit types — extract into a shared `discard_proposal_and_notify(workspace, slug, audit_type, final_error, chatops_ctx)` helper.
  - `autocoder/src/audits/mod.rs` — extend the `AuditOutcome` enum with `ValidationExhausted { audit_type, retries_attempted, final_error }`. Update the audit-scheduler's outcome-handling to log this distinctly.
  - `autocoder/src/audits/scheduler.rs` — the audit-state JSON gains an `attempt_history: Vec<{when: timestamp, outcome: AuditOutcome}>` field (or extends the existing history if one exists) so per-run validation-failure data is available for future inspection.
  - Tests:
    - Unit test: `validate_proposal` against a fixture proposal that validates → returns `Ok(())`.
    - Unit test: against a fixture with a broken `MODIFIED` reference → returns `Err(stderr)` containing the openspec error message.
    - Unit test: retry-loop helper with a stubbed LLM that returns a broken proposal on attempt 1 and a valid one on attempt 2, with `max_validation_retries=1` → returns `Ok` with `retries_used=1`.
    - Unit test: same shape but the stubbed LLM returns broken on both attempts, with `max_validation_retries=1` → returns `Err(ValidationExhausted { retries_attempted: 1, .. })`.
    - Unit test: `max_validation_retries=0` → no retry on first failure, immediate `Err(ValidationExhausted { retries_attempted: 0, .. })`.
    - Integration test on one audit type (e.g. `security_bug_audit`): wire a stub LLM, exercise the full flow including chatops notification on exhaustion; assert the change directory does NOT exist after the discard, assert the chatops notification text matches the documented `❌` format.
    - Integration test: stub LLM that succeeds on retry 1; assert the change directory DOES exist after the call, the chatops notification (if `notify_on_clean` is on) mentions "validated on retry 1 of 1".

- **Operator-visible behavior:** audits that previously committed invalid proposals (causing the downstream perma-stuck cascade) now either self-correct via one retry OR fail loudly with a chatops `❌` and discard the proposal. Operators see fewer perma-stuck loops AND see when an audit type is producing low-quality output (the retry-on-success message in the success path AND the validation-exhausted message in the failure path).
- **Breaking:** no. Audits that produce valid proposals on first attempt are unchanged. Audits that previously produced invalid proposals (and caused downstream cascades) now visibly fail at the audit boundary. The new config field `audits.max_validation_retries` is optional with a sensible default.
- **Acceptance:** `cargo test` passes (new + existing). An audit run with a stubbed LLM that always returns an invalid proposal AND `max_validation_retries=1` produces no commit, posts the documented `❌` chatops notification, and records a `ValidationExhausted` outcome in the audit-state JSON. An audit run with a stubbed LLM that returns invalid then valid produces a valid commit, posts the success-with-retry chatops notification (when `notify_on_clean=true`), and records a `Reported { retries_used: 1 }` outcome.
