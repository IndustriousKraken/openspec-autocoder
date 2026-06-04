# executor — delta for a002-single-pass-prompt-substitution

## ADDED Requirements

### Requirement: Executor prompt builders use single-pass substitution
The executor's multi-placeholder prompt builders — `build_revision_prompt`, `build_triage_prompt`, `build_chat_triage_prompt`, AND `build_changelog_prompt` — SHALL render their templates with the single-pass substitution helper (per the orchestrator-cli `Prompt-template substitution is single-pass` requirement), so a `{{…}}` token appearing inside an injected value (a PR body, a PR diff, an operator's revision/request text, audit findings, a canonical-specs index, OR changelog JSON) is NOT re-expanded by a later substitution. Single-replace builders (`build_prompt`, which substitutes only `{{change_body}}`) AND append-based builders (`build_recovery_prompt`) are unaffected — a single replace cannot re-expand.

This closes a self-hosting hazard: `prompts/implementer-revision.md` itself contains `{{pr_diff}}`, `{{revision_request}}`, AND `{{pr_body}}`, so revising a PR whose diff touches that template would, under chained `.replace`, re-expand those tokens inside the injected diff.

#### Scenario: A placeholder token in the PR diff is not re-expanded
- **WHEN** `build_revision_prompt` renders with a `pr_diff` whose text contains the literal `{{revision_request}}` AND `{{pr_body}}` (e.g. the PR under revision edits `prompts/implementer-revision.md`)
- **THEN** those literals appear verbatim in the rendered diff section
- **AND** the operator's revision request AND the PR body are each inserted exactly once, at the template's own placeholders
- **AND** the rendered prompt size does not grow by the number of placeholder literals carried in the diff

#### Scenario: Operator request text is not re-expanded
- **WHEN** `build_chat_triage_prompt` renders with a `request_text` that contains the literal `{{repo_url}}` OR `{{canonical_specs_index}}`
- **THEN** those literals appear verbatim
- **AND** the real `{{repo_url}}` / `{{canonical_specs_index}}` placeholders are each substituted exactly once

#### Scenario: Ordinary executor prompts are unchanged
- **WHEN** any of the four builders renders with injected values that contain no placeholder tokens
- **THEN** each placeholder is substituted exactly once
- **AND** the rendered prompt is byte-identical to the prior chained-`.replace` output
