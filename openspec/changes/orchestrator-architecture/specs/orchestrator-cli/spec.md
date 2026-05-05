## ADDED Requirements

### Requirement: CLI entry point
The system SHALL provide a command-line interface as the primary entry point for orchestrating implementations.

#### Scenario: Running the orchestrator
- **WHEN** the user executes the main CLI command
- **THEN** the system parses arguments and initiates the queue processing loop

### Requirement: Rewind command
The system SHALL provide a sub-command to explicitly rewind the queue state to recover from a failed PR or bad implementation.

#### Scenario: Rewinding after a failure
- **WHEN** the user runs the rewind command for a specific set of changes
- **THEN** the system unarchives the specified changes and resets the git branch to `dev`
