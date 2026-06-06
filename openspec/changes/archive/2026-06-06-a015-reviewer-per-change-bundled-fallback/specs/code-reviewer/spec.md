# code-reviewer — delta for a015-reviewer-per-change-bundled-fallback

## ADDED Requirements

### Requirement: Per-change review falls back to bundled when the change set is empty
In `reviewer.mode: per_change`, the reviewer SHALL fall back to a single bundled review of the whole context whenever splitting the context into per-change sub-contexts yields ZERO sub-contexts, rather than synthesizing a verdict from zero reviews. Every verdict the reviewer emits SHALL be derived from at least one completed reviewer invocation; an empty per-change synthesis SHALL NOT be treated as a `Pass`/`Approve`.

This closes a path where a `per_change` PR whose change set fails to resolve — for example, a PR created under one daemon build and re-reviewed under another, so no archived-change briefs are found and the split is empty — was silently approved with no reviewer invocation (zero LLM calls, an empty synthesized report defaulting to `Pass`). With the fallback, the PR's diff and changed files still reach the reviewer in bundled form and the verdict reflects an actual review.

The populated per-change path is unchanged: when the split yields one or more sub-contexts, each is reviewed and the results are synthesized as before, with no fallback.

#### Scenario: Empty split falls back to a bundled review
- **WHEN** `reviewer.mode` is `per_change` AND splitting the review context yields zero per-change sub-contexts
- **THEN** the reviewer performs exactly one bundled review of the whole context (one reviewer invocation occurs)
- **AND** the emitted verdict is the one that bundled review returns, NOT a verdict defaulted from an empty synthesis

#### Scenario: The fallback review still receives the diff
- **WHEN** the context that triggers the fallback carries a non-empty diff or changed-file set
- **THEN** that diff and those changed files are passed to the bundled review (the reviewer builds its prompt rather than skipping the call)

#### Scenario: An empty per-change synthesis is never an approval
- **WHEN** a per-change report would be synthesized from zero per-change reviews
- **THEN** the result is not a `Pass`/`Approve` verdict produced without any review

#### Scenario: A resolvable change set still dispatches per change
- **WHEN** `reviewer.mode` is `per_change` AND the split yields one or more per-change sub-contexts
- **THEN** each sub-context is reviewed and the results are synthesized as before
- **AND** no bundled fallback occurs
