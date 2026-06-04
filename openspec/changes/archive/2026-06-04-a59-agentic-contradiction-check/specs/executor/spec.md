# executor — delta for a59-agentic-contradiction-check

## ADDED Requirements

### Requirement: submit_contradictions MCP tool returns change-internal contradictions
The per-execution MCP child SHALL advertise a `submit_contradictions` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE = contradiction_check`, AND SHALL NOT advertise it for any other role. The tool's payload schema SHALL be `{ contradictions: [{ requirement_a: string, requirement_b: string, summary: string }] }`. The tool relays through a56's `relay_submission` → `record_submission`; a schema-invalid payload is rejected AND surfaced to the agent as a correctable tool error it can retry in the same session.

Because the contradiction check is fail-open (per the orchestrator-cli requirement), a session that ends with no stored submission SHALL be consumed as an empty result rather than an error — the fail-open WARN-and-proceed decision lives in the orchestrator-cli caller, not in this tool. A non-empty consumed submission carries the contradictions the caller turns into the `.needs-spec-revision.json` marker.

#### Scenario: Advertised only for the contradiction-check role
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = contradiction_check`
- **THEN** the `tools/list` response advertises `submit_contradictions` with the contradictions schema alongside the common tools
- **WHEN** the MCP child starts with any other role (`implementer`, `reviewer`, an advisory audit)
- **THEN** `submit_contradictions` is NOT advertised

#### Scenario: Valid submission is consumed by the caller
- **WHEN** the agent calls `submit_contradictions` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the session exits the daemon `consume_submission`s the stored payload for the orchestrator-cli caller to act on

#### Scenario: Schema-invalid submission is correctable
- **WHEN** a `submit_contradictions` payload fails the schema (missing field, non-array `contradictions`)
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session

#### Scenario: Missing submission consumed as empty, not an error
- **WHEN** a contradiction-check session exits with no stored submission for the execution
- **THEN** `consume_submission` returns an empty result (no contradictions)
- **AND** the tool layer does NOT raise an error — the orchestrator-cli caller's fail-open policy decides the WARN-and-proceed outcome
