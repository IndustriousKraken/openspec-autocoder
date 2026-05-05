## ADDED Requirements

### Requirement: Execute Internal LLM Loop
The system SHALL manage the LLM conversation loop internally, passing OpenSpec context to the API provider and resolving tool calls via MCP.

#### Scenario: Running an implementation
- **WHEN** the orchestrator selects a change from the queue
- **THEN** it shells out to `openspec instructions apply` to get the context, constructs an API payload to the configured LLM provider, and enters an execution loop until the LLM returns a completion signal.
