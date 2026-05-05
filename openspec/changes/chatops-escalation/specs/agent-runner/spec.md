## MODIFIED Requirements

### Requirement: Handle execution failure
The system SHALL detect non-zero exit codes from the agent subprocess and categorize them into fatal errors or escalation requests.

#### Scenario: Agent requests human escalation
- **WHEN** the agent subprocess returns a specific exit code (e.g., 2) indicating it needs help
- **THEN** the agent runner reports this as an escalation request rather than a fatal failure.

#### Scenario: Agent fails or times out
- **WHEN** the agent subprocess returns any other non-zero exit code or times out
- **THEN** the agent runner halts the queue and reports the fatal failure.
