# orchestrator-cli — delta for a002-single-pass-prompt-substitution

## ADDED Requirements

### Requirement: Prompt-template substitution is single-pass (no placeholder re-expansion)
autocoder SHALL render `{{placeholder}}` prompt templates by substituting all placeholders in a SINGLE pass, such that a placeholder token appearing inside a substituted value is NEVER re-expanded. A shared substitution helper SHALL perform this; every prompt-assembly site that injects dynamic content into a `{{placeholder}}` template SHALL use it. The polling-iteration prompts — `scout`, `brownfield-draft`, AND `brownfield-survey` — SHALL use the helper (the reviewer adopts the same helper per the code-reviewer requirement).

Naive chained `String::replace` re-scans already-substituted content, so a value (a repo `README`, a docs listing, a symbols overview, operator `guidance`, a changed file's contents) that itself contains a `{{…}}` token has that token expanded by a later substitution pass — corrupting the prompt AND, when the injected token's value is large, multiplying the prompt's size. The single-pass helper makes rendering linear in input size: it cannot multiply, and unrecognized `{{tokens}}` (in the template OR inside a value) are emitted verbatim.

#### Scenario: A placeholder token inside a substituted value is not re-expanded
- **WHEN** a template containing `{{readme}}` AND `{{symbols_overview}}` is rendered
- **AND** the `readme` value's text itself contains the literal `{{symbols_overview}}`
- **THEN** the `{{symbols_overview}}` literal carried in the README appears verbatim in the output (it is NOT replaced by the symbols-overview value)
- **AND** the template's own `{{symbols_overview}}` placeholder is substituted exactly once
- **AND** this holds regardless of the order in which the placeholder/value pairs are supplied

#### Scenario: Rendering is linear, never multiplicative
- **WHEN** a substituted value contains K occurrences of another placeholder token `{{x}}`
- **THEN** the rendered output grows by `K × len("{{x}}")` (the literal tokens are preserved)
- **AND** NOT by `K × len(value_of_x)` (the pre-fix re-expansion blowup)

#### Scenario: Normal inputs render unchanged
- **WHEN** a template is rendered with values that contain no placeholder tokens
- **THEN** every placeholder is replaced by its value exactly once
- **AND** the output is byte-identical to the prior chained-`.replace` rendering

#### Scenario: The polling prompts use the helper
- **WHEN** the `scout`, `brownfield-draft`, OR `brownfield-survey` prompt is assembled
- **THEN** it renders via the single-pass helper
- **AND** an injected `README` / docs listing / symbols overview / operator `guidance` value that contains a `{{…}}` token does not corrupt the prompt
