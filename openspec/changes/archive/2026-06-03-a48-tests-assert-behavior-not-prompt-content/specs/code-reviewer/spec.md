# code-reviewer — delta for a48-tests-assert-behavior-not-prompt-content

## MODIFIED Requirements

### Requirement: Default prompt template enforces code-quality scope
The code-reviewer SHALL ship a default prompt template that explicitly limits the review to code-quality concerns and instructs the LLM not to assess spec compliance. The template SHALL use the `{{change_context}}`, `{{changed_files}}`, and `{{diff}}` placeholders.

The scope-limiting intent — that the default template confines the review to code quality and instructs the model not to assess spec compliance — is design intent captured by this requirement and verified by the drift audit's semantic judgment. It SHALL NOT be verified by a unit test asserting a verbatim substring of the template's instruction prose (per the project-documentation requirement `Tests assert behavior or derivation, never message wording`). The placeholder references, being behavior-relevant (the substitution code fills them), SHALL be verified by rendering the real default with sentinel inputs and asserting the substituted values appear — never by asserting the surrounding wording.

#### Scenario: Default template is shipped and substitutes every placeholder
- **WHEN** the autocoder binary is built AND the default template is rendered with a distinct sentinel value supplied for each of `{{change_context}}`, `{{changed_files}}`, AND `{{diff}}`
- **THEN** a file named `prompts/code-review-default.md` is included in the project repository at the relative path `prompts/code-review-default.md`
- **AND** the rendered output contains each placeholder's sentinel value, proving the shipped default references all three placeholders at least once
- **AND** the test asserts only the substituted sentinel values, NOT any hand-authored instruction wording of the template (the scope-limiting intent is verified by the drift audit, not a substring check)

#### Scenario: User-provided template overrides default
- **WHEN** `reviewer.prompt_template_path` is set in config
- **THEN** the code-reviewer reads the template from that path at
  startup and uses it instead of the default
- **AND** if the path does not exist or fails to read, startup
  returns a `Err(_)` naming the path
- **AND** no scope enforcement is performed on user-supplied
  templates (custom templates are user-owned)
- **AND** custom templates that still reference the retired
  `{{change_summary}}` placeholder are left with the literal text
  unsubstituted — the operator is responsible for migrating
