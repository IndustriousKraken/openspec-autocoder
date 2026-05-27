## ADDED Requirements

### Requirement: CODE-REVIEW.md and CONFIG.md document the prompt-budget and per-change-mode fields
`docs/CODE-REVIEW.md` SHALL include a `## Prompt budget` subsection AND a `## Per-change reviewer mode` subsection documenting the new `reviewer.prompt_budget_chars` AND `reviewer.mode` config fields respectively. `docs/CONFIG.md`'s existing `reviewer:` table SHALL gain rows for both fields.

#### Scenario: CODE-REVIEW.md documents the prompt budget field
- **WHEN** an operator reads `docs/CODE-REVIEW.md`
- **THEN** a section titled `## Prompt budget` appears between the existing `## Review context` section AND `## Reviewer-initiated revisions on \`Block\` verdicts`
- **AND** the section names `reviewer.prompt_budget_chars` AND its default value (2_000_000)
- **AND** the section explains the no-hard-ceiling property — operators match the value to their provider's actual context window
- **AND** the section gives at least one example: Grok-4 / Claude Sonnet 4.6 → 4M (or whatever the current window is)

#### Scenario: CODE-REVIEW.md documents per-change mode
- **WHEN** an operator reads `docs/CODE-REVIEW.md`
- **THEN** a section titled `## Per-change reviewer mode` documents `reviewer.mode` with values `bundled` (default) AND `per_change`
- **AND** the section explains the LLM-cost trade-off (per_change = N× cost on N-change PRs)
- **AND** the section describes the PR-body shape change (one `## Code Review: <slug>` section per change instead of one combined block)
- **AND** the section explains the cross-change preamble (each per-change prompt includes a fixed-size list of the other changes in the same PR for cross-reference context)

#### Scenario: CONFIG.md table includes both fields
- **WHEN** an operator reads `docs/CONFIG.md`'s `reviewer:` table
- **THEN** the table contains a row for `prompt_budget_chars` (type `usize`, default `2_000_000`, no max)
- **AND** the table contains a row for `mode` (type enum, default `bundled`, values `bundled` / `per_change`)
- **AND** both rows link to the relevant `docs/CODE-REVIEW.md` section for the full discussion
