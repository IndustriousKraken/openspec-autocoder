# project-documentation — delta for a45-revision-summary-surfaces-in-pr-comment

## ADDED Requirements

### Requirement: `prompts/implementer-revision.md` instructs the revision agent on `outcome_success` AND `final_answer` content
`prompts/implementer-revision.md` SHALL contain a section directing the revision agent to call `outcome_success` at end-of-run AND pass a brief content-shaped summary as the `final_answer` argument. The section ensures the revision agent produces summary content that the PR-comment composer (per the `orchestrator-cli` "Revision execution updates the agent branch and posts a reply comment" requirement) can surface to the operator.

The section SHALL contain the following markers, in any order AND with any surrounding prose:

- `outcome_success` — names the MCP tool the agent calls.
- `final_answer` — names the field carrying the summary text.
- `declined` — signals that declining the reviewer's request is a valid AND reportable outcome (the foundation the future critical-evaluation prompt will build on).
- `Test counts` — names one of the content categories the summary should cover.

The required markers are the load-bearing contract; the surrounding prose is flexible. Future contributors MAY reword the section for clarity OR style as long as the markers stay present.

A regression test SHALL read `prompts/implementer-revision.md` via `std::fs::read_to_string` AND verify each required marker is present. The test produces a combined failure listing (NOT first-failure-only) when markers are missing. The test pattern matches the existing `a41-link-openspec-conventions` AND `a44-mcp-outcome-tool-descriptions` regression-test conventions; consolidating into a shared helper OR keeping per-file tests is an implementation choice.

This requirement covers the prompt's outcome-tool guidance content ONLY. The prompt's other content (revision-mode framing, the rule about reading prior implementer notes as context rather than constraints, the input section markers) is unchanged AND remains governed by the prompt's existing role as the revision agent's primary instruction surface.

#### Scenario: All required markers are present
- **GIVEN** the repository is in its post-merge state for `a45-revision-summary-surfaces-in-pr-comment`
- **WHEN** the regression test reads `prompts/implementer-revision.md` via `std::fs::read_to_string`
- **THEN** the file contains the substring `outcome_success`
- **AND** the file contains the substring `final_answer`
- **AND** the file contains the substring `declined`
- **AND** the file contains the substring `Test counts`
- **AND** the test passes with no diagnostic output

#### Scenario: Removing a required marker fails the test
- **GIVEN** a hypothetical future change removes the `declined` substring from the outcome-signal section of `prompts/implementer-revision.md`
- **WHEN** the regression test runs in CI for that change
- **THEN** the test fails with a diagnostic naming `prompts/implementer-revision.md: missing required substring 'declined'`
- **AND** the failure surfaces before the change can merge

#### Scenario: Multiple missing markers are reported in one run
- **GIVEN** a hypothetical future change rewrites the section AND inadvertently drops two markers
- **WHEN** the regression test runs
- **THEN** the test fails with a single combined diagnostic naming both missing markers
- **AND** the contributor can fix both without re-running the test repeatedly

#### Scenario: Rewording within the marker contract is permitted
- **GIVEN** a future change rewrites the outcome-signal section for clarity, preserving all required substrings
- **WHEN** the regression test runs
- **THEN** the test passes
- **AND** no diagnostic is produced (the prose is flexible; only the substring contract is binding)
