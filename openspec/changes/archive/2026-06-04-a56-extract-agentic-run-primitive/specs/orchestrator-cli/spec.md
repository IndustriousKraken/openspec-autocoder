# orchestrator-cli — delta for a56-extract-agentic-run-primitive

## ADDED Requirements

### Requirement: Control socket exposes record_submission AND consume_submission actions
The daemon's Unix-domain control socket SHALL expose `record_submission` AND `consume_submission` actions for execution-scoped structured submissions, paralleling the existing `record_outcome` / `consume_outcome` actions. `record_submission` SHALL accept a workspace-basename routing key, a change/execution key, a role name, AND a payload; it SHALL validate the payload against the role's registered schema AND store it keyed by execution, returning `{ ok: true }` on success OR `{ ok: false, error: <reason> }` on a schema/validation failure (which the MCP relay surfaces to the agent as a correctable tool error). `consume_submission` SHALL return the stored submission for an execution AND clear it, so the role's daemon-side caller owns the result.

This change establishes the actions AND the execution-scoped storage. The per-role payload schemas are registered by the changes that add each role's `submit_*` tool (4/5/6/8); this requirement defines the transport AND lifecycle the schemas plug into.

#### Scenario: Submission round-trips through the control socket
- **WHEN** an MCP child sends a valid `record_submission` for an execution
- **THEN** the daemon stores it AND returns `{ ok: true }`
- **AND** a subsequent `consume_submission` for that execution returns the stored payload AND clears it

#### Scenario: Schema-invalid submission is rejected
- **WHEN** a `record_submission` payload fails its role's registered schema
- **THEN** the daemon returns `{ ok: false, error: <reason> }` without storing it
- **AND** the reason is suitable for the MCP relay to surface to the agent for correction

#### Scenario: Consume with no stored submission is empty, not an error
- **WHEN** `consume_submission` is called for an execution with no stored submission
- **THEN** it returns an empty result (no submission) rather than failing
- **AND** the caller treats absence as "the role did not submit"
