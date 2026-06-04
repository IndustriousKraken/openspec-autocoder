## Why

`reviewer.mode: per_change` is honored on the initial-review path but silently ignored on the operator-triggered re-review path (`@<bot> code-review`). Reruns always emit a single bundled `## Code Review (rerun N of M)` block even when the operator configured per-change review and the PR carries multiple changes.

This is a spec-compliance bug, not a missing feature. Two canonical requirements already mandate the correct behavior:

- `Reviewer entry point is reusable across polling-loop AND operator-trigger callers` states the reusable function "SHALL use the configured `reviewer.mode` … (one call per change in per_change mode; one call per PR in bundled mode)."
- `Operator-initiated re-review via @<bot> code-review verb` includes the scenario "Re-review under per_change mode emits per-change content."

The implementation diverges in two places:

1. `code_reviewer::review_pr_at_state_with` (`autocoder/src/code_reviewer.rs`) calls the bundled-mode entry point `reviewer.review(ctx)` unconditionally, regardless of `ctx.mode`. In per_change mode it should dispatch per change (as the initial-review path does at `polling_loop.rs` ~1114-1125) and populate the per-change sections.
2. The rerun comment composer (`autocoder/src/revisions.rs` ~1281) formats only `result.markdown` and never reads `result.per_change_sections`, so even a correctly-populated per-change result would render as one bundled block.

The root cause that let this slip is a gap in the canonical contract: the `Reviewer entry point is reusable …` requirement defines `ReviewResult` as carrying `verdict`, `per_concern`, and `raw_output` — but NOT the per-change sections. With no per-change field in the documented contract, the operator-trigger caller has nothing to render per-change from. This change closes that gap and pins the regression.

## What Changes

**The reusable reviewer-function contract gains the per-change output (code-reviewer).** The `Reviewer entry point is reusable across polling-loop AND operator-trigger callers` requirement is MODIFIED so `ReviewResult` carries `per_change_sections: Vec<PerChangeSection>` (populated in per_change mode, empty in bundled mode), and so `review_pr_at_state` itself performs the per-mode dispatch (it MUST NOT route through a bundled-only entry point). A new scenario pins that the operator-trigger caller renders one per-change section per change under the rerun heading when `reviewer.mode: per_change`.

**No new behavior is invented.** The per-change dispatch logic, prompt construction, budget handling, and verdict semantics are the existing per_change machinery from the canonical `reviewer.mode: per_change dispatches one reviewer call per change in the PR` requirement; this change only makes the operator-trigger path use them, matching the initial-review path.

## Impact

- **Affected specs:**
  - `code-reviewer` — MODIFIED `Reviewer entry point is reusable across polling-loop AND operator-trigger callers` (ReviewResult carries `per_change_sections`; `review_pr_at_state` dispatches per mode; new scenario for the operator-trigger per_change render). Both existing scenarios preserved.
- **Affected code:**
  - `autocoder/src/code_reviewer.rs::review_pr_at_state_with` — dispatch on `ctx.mode`: bundled → `reviewer.review(ctx)` (unchanged); per_change → build per-change contexts and run the per-change pass, populating `ReviewResult.per_change_sections`. Mirror the initial-review path's mode switch.
  - `autocoder/src/revisions.rs` (rerun composer, ~1281) — when `result.per_change_sections` is non-empty, render one `## Code Review: <change-slug>` subsection per change beneath the `## Code Review (rerun N of M)` heading; when empty (bundled), render `result.markdown` as today.
- **Operator-visible behavior:** with `reviewer.mode: per_change`, an `@<bot> code-review` rerun on a multi-change PR now posts per-change sections, matching the initial review. With `reviewer.mode: bundled` (default) the rerun output is unchanged.
- **Acceptance:** `cargo test` passes; `openspec validate a53-reviewer-mode-honored-on-rerun --strict` passes. Tests: `review_pr_at_state` in per_change mode over a 3-change context invokes the reviewer 3 times and returns 3 `per_change_sections`; in bundled mode returns empty `per_change_sections` and is byte-identical to today; the rerun composer renders 3 per-change subsections from a populated result and a single block from an empty one.
- **Dependencies:** none. Independent of a46/a47 (which MODIFY different code-reviewer requirements); the code touch in `revisions.rs` is a distinct function from theirs.
