# executor — delta for a75-canon-contradiction-audit

## ADDED Requirements

### Requirement: submit_canon_internal_contradictions MCP tool returns canon-internal contradictions
The per-execution MCP child SHALL advertise a `submit_canon_internal_contradictions` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE = canon_contradiction_audit`, AND SHALL NOT advertise it for any other role. The tool's payload schema SHALL be `{ contradictions: [{ capability_a: string, requirement_a: string, capability_b: string, requirement_b: string, summary: string }] }` — each finding names BOTH conflicting canonical requirements (by capability AND title). The schema is symmetric (both sides canonical), distinguishing it from a62's `submit_canon_contradictions`, which names a change requirement against a canonical one. The tool relays through a56's `relay_submission` → `record_submission`; a schema-invalid payload is rejected AND surfaced to the agent as a correctable tool error it can retry in the same session.

Because the audit reports advisorily (an empty result is a clean canon, not a failure), a session that ends with no stored submission SHALL be consumed as an empty result rather than an error.

#### Scenario: Advertised only for the canon-contradiction-audit role
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = canon_contradiction_audit`
- **THEN** the `tools/list` response advertises `submit_canon_internal_contradictions` with the canon-internal-contradictions schema alongside the common tools
- **WHEN** the MCP child starts with any other role (`implementer`, `reviewer`, `canon_contradiction_check`, an advisory audit)
- **THEN** `submit_canon_internal_contradictions` is NOT advertised

#### Scenario: Valid submission is consumed by the caller
- **WHEN** the agent calls `submit_canon_internal_contradictions` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the session exits the daemon `consume_submission`s the stored payload for the audit to turn into `AuditOutcome::Reported` findings

#### Scenario: Schema-invalid submission is correctable
- **WHEN** a `submit_canon_internal_contradictions` payload fails the schema (missing `requirement_b`, non-array `contradictions`)
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session

#### Scenario: Missing submission consumed as empty, not an error
- **WHEN** a `canon_contradiction_audit` session exits with no stored submission for the execution
- **THEN** `consume_submission` returns an empty result
- **AND** the tool layer does NOT raise an error — the audit reports a clean canon
