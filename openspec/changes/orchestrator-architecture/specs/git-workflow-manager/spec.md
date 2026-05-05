## ADDED Requirements

### Requirement: Branch creation
The system SHALL branch from the configured `dev` branch to a dedicated agent branch (e.g. `agent-q`) before starting implementations.

#### Scenario: Branching for a new queue run
- **WHEN** the queue is processed and the agent branch does not exist
- **THEN** the manager checks out the `dev` branch, pulls latest, and creates the agent branch

### Requirement: Serial commits
The system SHALL commit changes to the same agent branch after each successful proposal implementation.

#### Scenario: Committing a change
- **WHEN** an agent successfully completes a change
- **THEN** the manager adds all files, creates a commit with the change name, and pushes to the remote agent branch

### Requirement: PR creation
The system SHALL create a pull request (or leave a placeholder for one) when the active queue is exhausted or paused.

#### Scenario: Opening a PR
- **WHEN** there are no more ready changes in the queue
- **THEN** the manager opens a PR from the agent branch to the `dev` branch
