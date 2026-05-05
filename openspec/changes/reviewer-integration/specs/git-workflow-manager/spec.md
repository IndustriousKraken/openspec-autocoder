## MODIFIED Requirements

### Requirement: PR creation
The system SHALL create a pull request (or leave a placeholder for one) when the active queue is exhausted or paused. It MUST include the reviewer agent's report in the PR description if one was generated.

#### Scenario: Opening a PR
- **WHEN** there are no more ready changes in the queue
- **THEN** the manager opens a PR from the agent branch to the `dev` branch, appending any saved review reports to the PR body
