## ADDED Requirements

### Requirement: Async polling loops
The system SHALL spawn independent asynchronous tasks to poll each configured repository.

#### Scenario: Concurrent repository watching
- **WHEN** multiple repositories are configured
- **THEN** the orchestrator spawns a `tokio` task for each repo that sleeps for the configured interval between queue checks
