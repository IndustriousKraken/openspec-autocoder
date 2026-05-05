## ADDED Requirements

### Requirement: Execute reviewer CLI
The system SHALL execute a reviewer AI agent CLI as a subprocess to analyze recent commits.

#### Scenario: Running the reviewer
- **WHEN** the `reviewer` module is invoked
- **THEN** it executes the configured reviewer CLI command (e.g. passing the git diff as input)

### Requirement: Capture review output
The system SHALL capture the standard output of the reviewer agent to be used as a report.

#### Scenario: Saving the report
- **WHEN** the reviewer agent completes its execution
- **THEN** the system saves the output to a text file or returns it as a string for use in the PR description
