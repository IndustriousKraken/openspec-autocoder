# Tasks

The implementation is complete; this change reconciles the canonical spec with it.

## 1. Spec reconciliation (this delta)

- [x] 1.1 MODIFY `Verifier-gate framework`, `Change-internal contradiction pre-flight check`, `Change-vs-canonical contradiction pre-flight check`, AND `Code-implements-spec verification` in `orchestrator-cli`: pre-executor gates fail CLOSED (held), the `[out]` gate renders FAILED TO RUN.

## 2. Implementation (done)

- [x] 2.1 `[in]`/`[canon]` gate run fns return `Clean | Found | Errored` (`preflight/change_contradiction.rs`, `preflight/canon_contradiction.rs`) — error/no-submission/unregistered-strategy → `Errored`, never empty-`Clean`.
- [x] 2.2 `spec_revision.rs`: `.needs-spec-revision.json` gains a structured `gate_error` population (gate + cause) + a gate-error `operator_action`.
- [x] 2.3 `polling_loop/preflight_checks.rs`: `Errored` → `handle_gate_error` writes the `gate_error` hold marker + halts; `Clean` → proceed; `Found` → existing findings marker.
- [x] 2.4 `polling_loop/alerts_throttle.rs`: `maybe_post_gate_error_alert` posts the distinct "gate FAILED TO RUN — change held" alert (shares the `SpecNeedsRevision` throttle).
- [x] 2.5 `[out]` gate: `code_implements_spec.rs` returns `Verified | FailedToRun { cause }`; `polling_loop/pass.rs` renders `render_spec_verification_failed_section` (a `## Spec Verification: FAILED TO RUN` section) instead of omitting.

## 3. Tests (done)

- [x] 3.1 Gate-level: error / no-submission / unregistered-strategy → `Errored` (not `Clean`); empty submission → `Clean`; findings → `Found` (`change_contradiction.rs`, `canon_contradiction.rs`).
- [x] 3.2 Marker-level: a `gate_error` marker serializes the structured field AND sets the gate-error `operator_action` (`spec_revision.rs`).
- [x] 3.3 Caller-level: a no-submission `[in]`/`[canon]` gate holds the change (executor NOT invoked, `gate_error` marker written) (`polling_loop/tests/t09.rs`).
- [x] 3.4 `[out]`: no submission renders a FAILED TO RUN section (not omitted) (`polling_loop/tests/t01.rs`).

## 4. Documentation

- [x] 4.1 `docs/OPERATIONS.md` (Pre-flight checks + SpecNeedsRevision section): a gate that cannot run HOLDS the change (failed-to-run alert + `gate_error` marker) rather than failing open; `[out]` renders FAILED TO RUN. (`docs/CHATOPS.md` clear-revision wording already covers the operator path.)

## 5. Structural fail-closed: default-deny verdict ledger (TODO)

The sections above achieve fail-closed by *inspection* (interim). This section makes it fail-closed by *construction* — the default-deny ledger that supersedes the inspect-and-branch dispatch.

- [x] 5.1 Per-change gate-verdict ledger type: a verdict per gate slot (`[in]`/`[canon]`/`[out]`), each defaulting to `PENDING`; verdict set `PENDING | PASS | FAIL | FAILED_TO_RUN | DISABLED`. Persisted per change under `.git/autocoder-gate-ledger/<change>.json` (a16: out of the managed tree). (`src/gate_ledger.rs`)
- [x] 5.2 No-skip dispatch: every gate slot runs a runner that affirmatively writes a verdict — a disabled gate runs a STUB writing `DISABLED` (the `if let Some(ctx)…` skip path in `queue_walk.rs` is replaced by `run_in_gate`/`run_canon_gate`). A runner that returns without writing leaves `PENDING`.
- [x] 5.3 Gating by construction: the executor runs ONLY when every blocking gate (`[in]`/`[canon]`) is `PASS`/`DISABLED` (`ledger.blocking_ok()` defensive proceed-gate); `PENDING`/`FAIL`/`FAILED_TO_RUN` holds. Replaces the per-arm `Ok(Some)/Ok(None)/Err` branching.
- [x] 5.4 Tests: a runner that never records → `PENDING` → held; a disabled gate → `DISABLED` → proceeds; executor invoked only when all blocking gates `PASS`/`DISABLED` (`gate_ledger` unit tests + `polling_loop/tests/t20.rs`).

## 6. Gate verdicts rendered in the PR (TODO)

- [x] 6.1 Render the gate-verdict ledger into the PR body (`## Gate verdicts` section, `render_gate_verdicts_with_reviewer` in `pass.rs`): per gate — identifier, model, verdict (+ one-line summary for `FAIL`/`FAILED_TO_RUN`); the agentic reviewer's verdict is folded in. A `PASS` is visible, not inferred from silence.
- [x] 6.2 Thread the pre-executor `[in]`/`[canon]` verdicts (recorded at pre-flight, persisted under `.git/`) through to PR-body construction via `seed_ledger_from_processed`; the `[out]` gate records into the same ledger AND its `## Spec Verification` section renders alongside it.
- [x] 6.3 Tests: the PR body contains a gate-verdict section naming each gate, its model, and its verdict (`polling_loop/tests/t20.rs`).

## 7. Acceptance

- [x] 7.1 `cargo test` passes (inspect-and-branch interim; only the pre-existing parallel-load flakes intermittently fail).
- [x] 7.2 `cargo test` passes after the ledger lands (ledger + PR-render tests green).
- [x] 7.3 `openspec validate verifier-gates-fail-closed --strict` passes.
