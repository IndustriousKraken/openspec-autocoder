# Implementation tasks

## 1. Executor: classifiable precondition-unmet failure

- [ ] 1.1 Surface a precondition-unmet failure kind from the agentic-run/executor for pre-spawn refusals — the `a006` sandbox-mechanism gate (no usable mechanism, no unsandboxed opt-in) is the motivating case. Carry the distinction on the outcome/error kind, NOT a message substring.
- [ ] 1.2 Keep substantive failures (subprocess started, then failed) as the existing `Failed` outcome — not precondition-unmet.

## 2. Revise path: don't charge a slot for a never-started revision (`revisions.rs`)

- [ ] 2.1 Branch the revise dispatcher on the precondition-unmet kind: post a failure reply comment that directs the operator to resolve the precondition AND post a new revision request.
- [ ] 2.2 Advance the seen-marker (consume the trigger — manual re-trigger; no auto-retry).
- [ ] 2.3 Do NOT increment `auto_revisions_applied` / `human_revise_count` for the precondition-unmet case (no revision work was attempted). Substantive `Failed` still increments (unchanged).

## 3. Tests

- [ ] 3.1 A precondition-unmet revise failure does NOT increment the revision count AND advances the seen-marker (assert the count is unchanged and the trigger is consumed).
- [ ] 3.2 A substantive `Failed` revise outcome still increments the count (unchanged behavior).
- [ ] 3.3 The precondition-unmet classification is driven by the outcome kind, not a message substring (assert behavior under a synthetic precondition-unmet outcome).

## 4. Acceptance gate

- [ ] 4.1 `cargo test` passes for the autocoder crate.
- [ ] 4.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] 4.3 `openspec validate a74-precondition-unmet-revision-failures --strict` passes.
