## MODIFIED Requirements

### Requirement: Watch the changes directory
The system SHALL monitor the `openspec/changes/` directory to identify pending proposals that are ready for implementation.

#### Scenario: Finding the next task
- **WHEN** the queue engine is queried for the next job
- **THEN** it returns the oldest unarchived change that is ready for implementation AND does NOT contain a `.in-progress` lock file.

## ADDED Requirements

### Requirement: Lock state management
The system SHALL create and remove lock files to prevent duplicate execution.

#### Scenario: Locking a change
- **WHEN** the agent runner begins executing a change
- **THEN** the queue engine creates an empty `.in-progress` file inside the change's directory.
