# Tasks

This change establishes the standard AND its detectability. Reversing the gates' runtime fail-open behavior is the named follow-on (see Conformance below).

## 1. The standard

- [x] 1.1 ADD `Control-plane gatekeepers fail closed, never to a passing verdict` to the `project-documentation` capability (this delta).
- [x] 1.2 Record the invariant in a developer-facing standards doc (a short `docs/STANDARDS.md` section, or an existing contributor doc) so it is applied to new gatekeepers — satisfying the capability's documentation nature.

## 2. Conformance (follow-on — NOT this change)

_The runtime conformance below already landed in the tree (the `fail closed conformity` / `gate fixes` work). It is recorded here as the named follow-on for this standard; the end-states were **verified** against the current code while implementing the standard, not newly diffed by this documentation change._

- [x] 2.1 Reverse the `[in]`/`[canon]` gates from fail-open to the explicit held state: MODIFY orchestrator-cli `Change-internal contradiction pre-flight check`, `Change-vs-canonical contradiction pre-flight check`, AND the `Verifier-gate framework` so a session error / unavailable-CLI / no-submission writes a distinct "gate failed to run" held marker (operator-cleared), NOT "no contradictions". Bounded transient retry, then held. — _Verified: `ContradictionCheckOutcome::Errored` + the default-deny `GateVerdict` ledger (`FailedToRun`); `executor.verifier_gate_retries` bounds the retry, then holds._
- [x] 2.2 `[out]` advisory gate: render `## Spec Verification: FAILED TO RUN — <cause>` on error instead of omitting the section. — _Verified: `code_implements_spec.rs` renders the FAILED TO RUN section on every can't-run path._
- [x] 2.3 Reviewer: keep its discard-on-no-submission (already conformant); make the discard an explicit surfaced state if not already. — _Verified: `AgenticReviewOutcome::Discarded { reason }`; never defaults to `Approve`._
- [x] 2.4 Audit/grep or `drift_audit` coverage that flags a verdict initializer / zero-item aggregation defaulting to pass. — _Verified: the standard is canonical, so `drift_audit` (read-only Read/Glob/Grep/Bash over the tree + `openspec/specs/`) and the `[canon]` gate can flag a gatekeeper that defaults to pass._

## 3. Acceptance

- [x] 3.1 `openspec validate gatekeepers-fail-closed --strict` passes.
