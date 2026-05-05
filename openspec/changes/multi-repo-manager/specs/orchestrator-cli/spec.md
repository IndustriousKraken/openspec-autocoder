## MODIFIED Requirements

### Requirement: CLI entry point
The system SHALL provide a command-line interface as the primary entry point for orchestrating implementations.

#### Scenario: Running the orchestrator
- **WHEN** the user executes the main CLI command (e.g. `orchestrator start --config config.yaml`)
- **THEN** the system parses arguments, loads the configuration, and initiates the async polling daemon instead of running a single local pass.
