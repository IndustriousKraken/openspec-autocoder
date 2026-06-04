# Implementation tasks

## 1. Carry per-change output through the reusable reviewer result

- [x] 1.1 In `autocoder/src/code_reviewer.rs`, ensure `ReviewResult` carries `per_change_sections: Vec<PerChangeSection>` (the field already exists in the struct per the trace; confirm it is part of the value returned by `review_pr_at_state` / `review_pr_at_state_with`, not dropped).
- [x] 1.2 In `review_pr_at_state_with` (~line 414), dispatch on `ctx.mode` instead of unconditionally calling `reviewer.review(ctx)`:
  - `ReviewerMode::Bundled` → `reviewer.review(ctx)` as today; `per_change_sections` is empty.
  - `ReviewerMode::PerChange` → build per-change contexts and run the per-change pass (the same machinery the initial-review path uses at `polling_loop.rs` ~1114-1125: `build_per_change_contexts` + `review_per_change` + `synthesize_per_change_report`), populating `per_change_sections`.
- [x] 1.3 Keep `review_pr_at_state` / `review_pr_at_state_with` free of output-disposition logic (the caller decides rendering); the function only returns the populated `ReviewResult`.

## 2. Render per-change sections on the rerun comment

- [x] 2.1 In `autocoder/src/revisions.rs` (rerun comment composer, ~line 1281), when `result.per_change_sections` is non-empty, render one `## Code Review: <change-slug>` subsection per change beneath the `## Code Review (rerun N of M)` heading (verdict + concerns + body per change, mirroring the initial-review per-change layout).
- [x] 2.2 When `result.per_change_sections` is empty (bundled mode), render `result.markdown` exactly as today — no behavior change for the default path.

## 3. Tests

- [x] 3.1 `review_pr_at_state` (or `_with`) over a synthetic 3-change `ReviewContext` with `mode: PerChange` invokes the reviewer 3 times AND returns a `ReviewResult` with 3 `per_change_sections`.
- [x] 3.2 Same function with `mode: Bundled` invokes the reviewer once, returns empty `per_change_sections`, AND is byte-identical to the pre-change bundled result for the same input.
- [x] 3.3 The rerun composer renders 3 `## Code Review: <slug>` subsections from a result with 3 populated `per_change_sections`, AND renders a single bundled block from a result with empty `per_change_sections`. Assert on the composed output structure (section count / slugs), not on reviewer prose.

## 4. Spec delta

- [x] 4.1 `specs/code-reviewer/spec.md` — MODIFY `Reviewer entry point is reusable across polling-loop AND operator-trigger callers` per this change's delta (ReviewResult carries `per_change_sections`; per-mode dispatch in the function; new operator-trigger per_change render scenario; both existing scenarios preserved).

## 5. Acceptance gate

- [x] 5.1 `cargo test` passes for the autocoder crate.
- [x] 5.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 5.3 `openspec validate a53-reviewer-mode-honored-on-rerun --strict` passes.
