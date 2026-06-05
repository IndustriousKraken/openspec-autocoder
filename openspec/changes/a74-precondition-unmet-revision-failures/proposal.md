## Why

When a revision attempt fails because the agent subprocess **never started** — an unmet precondition like the `a006` OS-sandbox-mechanism gate refusing to spawn — the revise path treats it exactly like a substantive task failure: it posts a failure comment, **consumes the trigger** (so it does not auto-retry), AND **increments the revision count** against the per-PR cap (`revisions.rs:1503`, and the canonical "a failed attempt counts toward the cap"). Consuming the trigger is correct — an unmet precondition (e.g. a missing dependency) will not heal between polls, so auto-retrying would just spin against a broken host, and a deliberate operator re-trigger is the right recovery. But burning a revision slot for work that **never ran** is wrong, and the failure message does not make clear that the fix is "resolve the precondition, then post a new revision request."

This change carves out the precondition-unmet case: keep the manual re-trigger, but don't charge a revision slot, and point the operator at the right next step.

## What Changes

**The executor surfaces a classifiable precondition-unmet failure** — distinct from a substantive `Failed` outcome where the subprocess ran and the task failed. The sandbox-mechanism gate (and similar pre-spawn refusals) produce it. The distinction is carried by the outcome/error **kind**, not by matching a substring of the message, so callers can branch reliably.

**The revise path handles precondition-unmet specially.** It still posts a failure reply comment and still consumes the trigger (manual re-trigger; no auto-retry), but it does **NOT** increment the revision count (no revision work was attempted), and the message directs the operator to resolve the precondition and post a new revision request. A substantive `Failed` is unchanged — it still counts toward the cap.

This complements `a011` (whose startup doctor catches a missing/unusable sandbox mechanism at boot, so the revise path rarely reaches this gate at all) by making the residual case cost-free and self-explanatory.

## Impact

- **Affected specs:** `executor` — ADD `Agentic run surfaces a precondition-unmet failure distinct from a run failure`. `orchestrator-cli` — MODIFY `Revision execution updates the agent branch and posts a reply comment` (distinguish substantive `Failed` from precondition-unmet on the revise path).
- **Affected code:** the agentic-run/executor surfaces a precondition-unmet error kind for pre-spawn refusals (the `a006` sandbox-mechanism gate); the revise dispatcher (`revisions.rs`) branches on it — posts the guiding failure comment, advances the seen-marker (consume trigger), but skips the `auto_revisions_applied` / `human_revise_count` increment.
- **Operator-visible behavior:** a revision that fails because the agent never started (e.g. no usable sandbox mechanism) no longer burns a revision slot, and its comment tells the operator to fix the precondition and post a new revision request. Manual re-trigger is still required (no auto-retry). Substantive revision failures are unchanged.
- **Dependencies:** complements `a011` (the dependency doctor) and `a006`/`a73` (the sandbox-mechanism gate that produces the precondition-unmet failure). The analogous pending-change perma-stuck case is out of scope here (and largely mooted by `a011`'s startup preflight).
- **Acceptance:** `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean; `openspec validate a74-precondition-unmet-revision-failures --strict` passes. Tests: a precondition-unmet revise failure does NOT increment the revision count AND advances the seen-marker (manual re-trigger); a substantive `Failed` still increments the count; the classification is driven by the outcome kind, not a message substring.
