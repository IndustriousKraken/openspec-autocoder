## 1. Iteration sequence refactor

- [ ] 1.1 Locate the polling iteration's macro sequence (likely in `autocoder/src/polling_loop.rs`). The current order is approximately:
  ```
  ensure_initialized → recreate_branch → run_audits → list_waiting → list_pending → push+PR
  ```
- [ ] 1.2 Reorder to:
  ```
  ensure_initialized → recreate_branch → list_waiting → list_pending → run_audits → push+PR
  ```
- [ ] 1.3 The push+PR step continues to run if ANY commits exist on the agent branch (audit creation commits OR change implementation commits OR both). No change to the push+PR semantics.

## 2. Audit-state and generated-change handling

- [ ] 2.1 An audit's `SpecsWritten(names)` outcome continues to commit the new change directories as-the-audit-completes. The CHANGE is just position-in-sequence — the audit's internal behavior is unchanged.
- [ ] 2.2 The new pending changes do NOT get picked up by THIS iteration's queue walk (it already completed). They sit on disk as pending; the NEXT iteration's `list_pending` picks them up.
- [ ] 2.3 The existing chatops `🔍 created proposal` notification fires when the audit produces a valid proposal — unchanged. Operators see the audit's outputs in chat even though implementation waits one iteration.
- [ ] 2.4 Tests:
  - Iteration with 2 pending changes + 1 eligible audit → pending changes process first; audit fires after; both phases' commits ship in the same iteration's PR.
  - Iteration with 0 pending + 1 eligible audit that creates 2 new proposals → audit creation commits push in this iteration's PR; the 2 new pending changes wait for next iteration's queue walk.
  - Iteration with 1 pending change + 0 eligible audits → only the change processes; no audit work.

## 3. Verify the existing PR-skip-on-open-PR gate still fires first

- [ ] 3.1 The "skip iteration if open PR exists for agent branch" gate (the existing requirement) runs BEFORE either the audit or the change phase. This is unchanged. An open PR continues to block the whole iteration regardless of pending changes.
- [ ] 3.2 Test: iteration with open PR for agent-q + 1 pending change → entire iteration is skipped (no audit, no change processing).

## 4. Docs

- [ ] 4.1 In `docs/OPERATIONS.md`'s `## Periodic audits` section, update the "When audits fire" paragraph to reflect the new ordering: audits run AFTER `list_pending` (was BEFORE).
- [ ] 4.2 Add a paragraph explaining the one-iteration delay for audit-generated changes' implementation: audits create proposals in iteration N; the implementer picks them up in iteration N+1. The two ship as separable PRs.
- [ ] 4.3 In `docs/OPERATIONS.md`'s `## Per-change run log shape` or related operator-visible section, note that PR contents now have change implementation commits FIRST and audit creation commits AFTER (the commit ordering swap follows the iteration sequence change).

## 5. Spec deltas

- [ ] 5.1 `openspec/changes/a12-changes-have-precedence-over-audits/specs/orchestrator-cli/spec.md` MODIFIES the existing `Periodic audit framework` requirement's first scenario to reflect the new ordering AND adds a new scenario clarifying the one-iteration delay for audit-generated changes.
- [ ] 5.2 `openspec/changes/a12-changes-have-precedence-over-audits/specs/project-documentation/spec.md` ADDs one requirement covering the OPERATIONS.md updates.

## 6. Verification

- [ ] 6.1 `cargo test` passes (new + existing).
- [ ] 6.2 `openspec validate a09-changes-have-precedence-over-audits --strict` passes.
- [ ] 6.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
