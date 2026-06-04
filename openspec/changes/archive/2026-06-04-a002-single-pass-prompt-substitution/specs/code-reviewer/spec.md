# code-reviewer — delta for a002-single-pass-prompt-substitution

## ADDED Requirements

### Requirement: Reviewer renders its prompt with single-pass substitution
The reviewer SHALL assemble its prompt using the single-pass substitution helper (per the orchestrator-cli `Prompt-template substitution is single-pass` requirement), so a `{{cross_change_preamble}}` / `{{change_context}}` / `{{changed_files}}` / `{{diff}}` token appearing inside a substituted value is NOT re-expanded. This matters most for the `{{changed_files}}` value: a changed file's contents are arbitrary, and when the change under review is a template, documentation, OR the reviewer's own code/specs, those contents contain the very placeholder tokens the reviewer substitutes. Re-expanding them corrupts the review AND can multiply the prompt past the model's context limit.

#### Scenario: A `{{diff}}` literal in a changed file is not expanded
- **WHEN** a `ReviewContext` whose changed files include a file whose contents contain the literal `{{diff}}` AND `{{changed_files}}` is rendered (e.g. the change under review edits the reviewer's own spec, which documents those tokens)
- **THEN** those literals appear verbatim in the rendered changed-files section
- **AND** the diff AND the changed-files block are each inserted exactly once, at the template's own placeholders
- **AND** the rendered prompt's size is bounded by `change_context + changed_files + diff + template` — it does NOT grow by the number of placeholder literals present in the changed files

#### Scenario: Ordinary reviews are unchanged
- **WHEN** a `ReviewContext` whose values contain no placeholder tokens is rendered
- **THEN** each of the four placeholders is substituted exactly once
- **AND** the rendered prompt is byte-identical to the prior chained-`.replace` rendering
