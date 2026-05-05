## ADDED Requirements

### Requirement: Watch the changes directory
The system SHALL monitor the `openspec/changes/` directory to identify pending proposals that are ready for implementation.

#### Scenario: Finding the next task
- **WHEN** the queue engine is queried for the next job
- **THEN** it returns the oldest unarchived change that is ready for implementation

### Requirement: Archive state management
The system SHALL move completed changes to the `archive/` folder to remove them from the active queue.

#### Scenario: Archiving a completed job
- **WHEN** a change implementation is successfully committed
- **THEN** the engine moves the change folder to `openspec/changes/archive/`

### Requirement: Unarchive state management
The system SHALL move changes from the `archive/` folder back to the active queue when a rewind is requested.

#### Scenario: Unarchiving a failed job
- **WHEN** the rewind command is executed for a change
- **THEN** the engine moves the change folder from `archive/` back to `openspec/changes/`
