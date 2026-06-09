# Tasks

This change establishes the standard AND its detectability. Reversing the gates' runtime fail-open behavior is the named follow-on (see Conformance below).

## 1. The standard

- [ ] 1.1 ADD `Control-plane gatekeepers fail closed, never to a passing verdict` to the `project-documentation` capability (this delta).
- [ ] 1.2 Record the invariant in a developer-facing standards doc (a short `docs/STANDARDS.md` section, or an existing contributor doc) so it is applied to new gatekeepers — satisfying the capability's documentation nature.

## 2. Conformance (follow-on — NOT this change)

- [ ] 2.1 Reverse the `[in]`/`[canon]` gates from fail-open to the explicit held state: MODIFY orchestrator-cli `Change-internal contradiction pre-flight check`, `Change-vs-canonical contradiction pre-flight check`, AND the `Verifier-gate framework` so a session error / unavailable-CLI / no-submission writes a distinct "gate failed to run" held marker (operator-cleared), NOT "no contradictions". Bounded transient retry, then held.
- [ ] 2.2 `[out]` advisory gate: render `## Spec Verification: FAILED TO RUN — <cause>` on error instead of omitting the section.
- [ ] 2.3 Reviewer: keep its discard-on-no-submission (already conformant); make the discard an explicit surfaced state if not already.
- [ ] 2.4 Audit/grep or `drift_audit` coverage that flags a verdict initializer / zero-item aggregation defaulting to pass.

## 3. Acceptance

- [ ] 3.1 `openspec validate gatekeepers-fail-closed --strict` passes.
