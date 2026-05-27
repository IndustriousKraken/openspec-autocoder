## MODIFIED Requirements

### Requirement: Reviewer prompt budget is operator-configurable
The reviewer's prompt-budget cap (the threshold past which touched-file context is truncated with a `## Skipped (budget exhausted): ...` footer) SHALL read from `reviewer.prompt_budget_chars` in `config.yaml`. The default value SHALL be `2_000_000` characters, preserving today's behavior verbatim for operators who do not set the field. There is no hard upper bound — the operator is responsible for matching the value to their LLM provider's actual context window (Grok-4, Claude Sonnet 4.6, etc. accept much more than 2M; smaller-window providers may want less).

#### Scenario: Default config preserves the 2,000,000-character cap
- **WHEN** the operator's `config.yaml` does NOT set `reviewer.prompt_budget_chars`
- **AND** the reviewer is invoked against a multi-file pass whose touched-file content exceeds the cap
- **THEN** the reviewer's prompt is truncated at 2,000,000 characters
- **AND** the `## Skipped (budget exhausted): ...` footer fires for the skipped files
- **AND** the verdict defaults to `Concerns` per the existing missing-context bias

#### Scenario: Higher cap permits more touched-file context
- **WHEN** the operator sets `reviewer.prompt_budget_chars: 4_000_000`
- **AND** the reviewer is invoked against a pass whose touched-file content is 3,000,000 characters total
- **THEN** the reviewer's prompt fits the full context (no truncation)
- **AND** no `## Skipped (budget exhausted): ...` footer fires
- **AND** the verdict is not biased toward `Concerns` for budget reasons

#### Scenario: Cap is hot-applicable via `autocoder reload`
- **WHEN** the operator changes `reviewer.prompt_budget_chars` in `config.yaml` AND runs `autocoder reload`
- **THEN** the daemon's reload handler applies the new value at the next iteration's reviewer invocation
- **AND** the existing `reviewer:` hot-reload path picks up the change without a daemon restart

## ADDED Requirements

### Requirement: `reviewer.mode: per_change` dispatches one reviewer call per change in the PR
The reviewer SHALL accept a `reviewer.mode` config field with values `bundled` (default) AND `per_change`. Under `bundled`, the existing single-reviewer-call-per-PR behavior SHALL be preserved verbatim. Under `per_change`, the reviewer SHALL dispatch one LLM call per change in the pass, each scoped to that change's diff + the files that specific change touched, AND emit one `## Code Review: <change-slug>` section per change in the PR body (instead of one combined `## Code Review` block).

Each per-change reviewer prompt SHALL include a fixed-size cross-change preamble naming the OTHER changes in the same PR (slug + first-paragraph-of-`## Why`, each truncated to 200 characters). The preamble exists for cross-reference context only; the reviewer's verdict for each change applies strictly to that change.

#### Scenario: Default `bundled` mode is unchanged
- **WHEN** the operator's `config.yaml` does NOT set `reviewer.mode`
- **AND** a 3-change PR pass is reviewed
- **THEN** the reviewer is invoked exactly once
- **AND** the PR body contains one `## Code Review` block (not three)
- **AND** the behavior is byte-identical to pre-spec output for the same inputs

#### Scenario: `per_change` mode invokes the reviewer N times for an N-change pass
- **WHEN** the operator sets `reviewer.mode: per_change`
- **AND** a 3-change PR pass is reviewed
- **THEN** the LLM client receives exactly 3 reviewer invocations
- **AND** each invocation's prompt contains ONLY that change's diff AND the files that change touched
- **AND** each invocation's prompt contains the cross-change preamble naming the OTHER 2 changes (slug + truncated-summary, one line each)
- **AND** the PR body contains 3 `## Code Review: <change-slug>` sections in change order
- **AND** each section follows the same verdict + concerns + format the bundled `## Code Review` block uses

#### Scenario: Per-change reviews independently respect the prompt budget
- **WHEN** `reviewer.mode: per_change` AND one change in a 3-change pass touches a huge file that exceeds the per-call budget
- **THEN** ONLY that change's reviewer section emits a `## Skipped (budget exhausted): ...` footer
- **AND** the other 2 changes' reviews are unaffected
- **AND** each change's verdict is computed independently

#### Scenario: Reviewer-initiated revisions aggregate across per-change reviews
- **WHEN** `reviewer.mode: per_change` AND `reviewer.auto_revise_on_block: true`
- **AND** a 3-change PR pass produces 2 revision-request concerns per change (6 total)
- **AND** `executor.max_revisions_per_pr: 5`
- **THEN** the dispatcher posts the 5 highest-priority revision requests as `<!-- reviewer-revision -->`-marked PR comments
- **AND** the 6th request is annotated in its `## Code Review: <slug>` section as `(not auto-revised; cap budget exhausted)`
- **AND** the cap-budget interaction applies across the union of all per-change reviews, not per-change

#### Scenario: Single-change pass omits the preamble's "other changes" list
- **WHEN** `reviewer.mode: per_change` AND a single-change pass is reviewed
- **THEN** the cross-change preamble is included with an empty "other changes" list (or the preamble is omitted entirely as a formatting choice)
- **AND** the LLM is not confused about the pass containing other changes when it doesn't
