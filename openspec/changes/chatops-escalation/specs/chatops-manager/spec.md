## ADDED Requirements

### Requirement: Asynchronous Escalation Communication
The system SHALL intercept human escalation requests from agents and facilitate asynchronous communication to resolve them without blocking parallel queue processing.

#### Scenario: Agent asks a question
- **WHEN** an implementation agent creates a `.question.json` file and halts
- **THEN** the `chatops-manager` sends the question to a configured chat channel (e.g., Slack) and begins tracking the resulting message thread.

#### Scenario: Human answers a question
- **WHEN** a human replies to the tracked message thread
- **THEN** the `chatops-manager` captures the reply, saves it to `.answer.json`, and allows the queue engine to resume the agent's execution.
