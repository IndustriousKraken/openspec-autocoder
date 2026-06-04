# code-reviewer — delta for a53-reviewer-mode-honored-on-rerun

## MODIFIED Requirements

### Requirement: Reviewer entry point is reusable across polling-loop AND operator-trigger callers

The reviewer's LLM-invocation logic SHALL be exposed as a reusable function `code_reviewer::review_pr_at_state(cfg: &ReviewerConfig, ctx: &ReviewContext) -> Result<ReviewResult>`.

- `ReviewContext` SHALL carry `head_sha: String`, `diff: String`, `change_list: Vec<String>`, `files: Vec<FileEntry>`, AND `mode: ReviewerMode`.
- `ReviewResult` SHALL carry `verdict: Verdict (Approve | Block)`, `per_concern: Vec<ConcernEntry>`, `raw_output: String`, AND `per_change_sections: Vec<PerChangeSection>`. `per_change_sections` is populated with one entry per change when `ctx.mode` is `per_change`, AND is empty when `ctx.mode` is `bundled`.

The function SHALL NOT decide output disposition; the caller decides whether to write into the PR body's `## Code Review` block (polling-loop caller) OR post as a fresh PR comment with `## Code Review (rerun N of M)` heading (operator-trigger caller).

The function SHALL itself perform the per-mode dispatch per the existing canonical `reviewer.mode: per_change dispatches one reviewer call per change in the PR` requirement: one call per change in `per_change` mode (populating `per_change_sections`), one call per PR in `bundled` mode (leaving `per_change_sections` empty). It SHALL NOT route through a bundled-only entry point that ignores `ctx.mode`; both the polling-loop caller AND the operator-trigger caller observe the configured mode identically.

Because `ReviewResult` carries the per-change sections, the operator-trigger caller (the `@<bot> code-review` rerun composer) SHALL render them: when `per_change_sections` is non-empty it emits one per-change section per entry beneath the `## Code Review (rerun N of M)` heading; when empty it renders the bundled output as before.

The reviewer prompt template, LLM client, output validation, AND retry semantics are unchanged in this change.

#### Scenario: Polling-loop caller produces byte-identical PR body output to pre-spec behavior
- **WHEN** the polling-loop invokes `review_pr_at_state` with a canned PR state
- **AND** the function returns a `ReviewResult`
- **AND** the polling-loop's output-disposition code writes into the PR body's `## Code Review` block
- **THEN** the resulting PR body is byte-identical to pre-spec output for the same inputs (the extraction is refactor-only, no behavior change)

#### Scenario: Operator-trigger caller uses the same function with different disposition
- **WHEN** the operator-trigger dispatcher invokes `review_pr_at_state` with the SAME `ReviewContext` the polling-loop would have built
- **THEN** the function returns the SAME `ReviewResult` (the LLM call AND validation logic are identical)
- **AND** the operator-trigger's output-disposition code posts as a fresh PR comment instead of editing the PR body

#### Scenario: Operator-trigger rerun honors per_change mode end-to-end
- **GIVEN** `reviewer.mode: per_change` AND a PR carrying 3 changes
- **WHEN** an operator-initiated re-review invokes `review_pr_at_state` with a `ReviewContext` whose `mode` is `per_change`
- **THEN** the reviewer is invoked once per change (3 invocations), matching the initial-review path
- **AND** the returned `ReviewResult.per_change_sections` contains 3 entries, one per change
- **AND** the rerun comment composer renders 3 per-change sections (one `## Code Review: <change-slug>` per change) beneath the `## Code Review (rerun N of M)` heading, NOT a single bundled block

#### Scenario: Bundled mode rerun is unchanged
- **GIVEN** `reviewer.mode` is `bundled` (the default)
- **WHEN** an operator-initiated re-review invokes `review_pr_at_state`
- **THEN** the reviewer is invoked once for the PR
- **AND** `ReviewResult.per_change_sections` is empty
- **AND** the rerun comment composer renders the single bundled block exactly as before this change
