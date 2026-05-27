## ADDED Requirements

### Requirement: CLI.md documents the `check-config` subcommand
`docs/CLI.md` SHALL include a `## \`check-config\`` section documenting the new subcommand's invocation, exit-code matrix, output formats, and intended use cases.

#### Scenario: CLI.md section exists with full coverage
- **WHEN** an operator reads `docs/CLI.md`
- **THEN** a section titled `## \`check-config\`` appears between the existing subcommand entries
- **AND** the section documents the required `--config <path>` argument
- **AND** the section documents the optional `--json` flag with the structured per-line JSON output
- **AND** the section enumerates the exit codes: `0` (valid), `1` (warnings only), `2` (hard errors)
- **AND** the section names the two intended audiences: operators editing YAML by hand AND scripted preflight (specifically `update.sh`, landing in a later stacked change)

#### Scenario: Section provides a copy-paste example for each exit code
- **WHEN** the operator reads the section
- **THEN** the page contains at least one example invocation each for an exit-0, exit-1, and exit-2 scenario
- **AND** each example shows both the stdout and stderr the operator would observe
