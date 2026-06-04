# executor — delta for a58-agentic-reviewer

## ADDED Requirements

### Requirement: submit_review MCP tool returns the reviewer verdict
The per-execution MCP child SHALL advertise a `submit_review` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE = reviewer`, AND SHALL NOT advertise it for any other role. The tool's payload schema SHALL be `{ verdict: "Approve" | "Block", summary: string, concerns: [{ title: string, detail: string, anchor: string, should_request_revision: bool, actionable_request: string|null }] }`. The schema SHALL enforce the `verdict` enum AND SHALL require a non-empty `actionable_request` whenever `should_request_revision` is `true`. The tool relays through a56's `relay_submission` → `record_submission`.

A schema-invalid `submit_review` payload (a verdict outside the enum, a `should_request_revision` concern with no `actionable_request`, a malformed shape) SHALL be rejected by `record_submission` AND surfaced to the agent as a correctable tool error it can retry in the same session. After the reviewer session exits the daemon `consume_submission`s the stored payload into a `ReviewResult` (`verdict`, `per_concern`, `raw_output`). A reviewer session that ends with NO stored submission SHALL cause the caller to discard the review AND alert the operator (it SHALL NOT be treated as an implicit `Approve`). This is the structural retirement of the malformed-verdict-defaults-to-approve behavior: the verdict can only enter the daemon through the schema-validated tool.

#### Scenario: Advertised only for the reviewer role
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = reviewer`
- **THEN** the `tools/list` response advertises `submit_review` with the review schema alongside the common tools
- **WHEN** the MCP child starts with any other role (`implementer`, an advisory audit, a specs-writing audit)
- **THEN** `submit_review` is NOT advertised

#### Scenario: Valid submission becomes the ReviewResult
- **WHEN** the reviewer agent calls `submit_review` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the session exits the daemon `consume_submission`s the payload into a `ReviewResult` whose `verdict` AND `per_concern` come from the submission

#### Scenario: Schema-invalid submission is correctable, not fatal
- **WHEN** a `submit_review` payload has a `verdict` outside `{Approve, Block}`, OR a concern with `should_request_revision: true` AND an empty `actionable_request`
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session
- **AND** a single rejection does NOT discard the review on its own — a subsequent valid submission in the same execution is accepted

#### Scenario: No submission discards the review, never auto-approves
- **WHEN** a reviewer session exits with no stored submission for the execution
- **THEN** the caller discards the review (writes no verdict) AND posts the reviewer-failure operator alert
- **AND** the outcome is NOT an implicit `Approve`
