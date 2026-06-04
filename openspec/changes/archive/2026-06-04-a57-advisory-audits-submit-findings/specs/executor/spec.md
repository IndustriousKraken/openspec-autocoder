# executor — delta for a57-advisory-audits-submit-findings

## ADDED Requirements

### Requirement: submit_findings MCP tool returns advisory-audit findings
The per-execution MCP child SHALL advertise a `submit_findings` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE` names an advisory audit (`drift_audit`, `architecture_consultative`, OR `documentation_audit`). The tool's payload schema is the audit-specific finding shape registered for that role: drift findings carry `{capability, requirement, severity, code_anchors, divergence}`; architecture findings carry `{subject, body, anchor, severity}` with the array capped at 5 entries; documentation findings carry `{category, severity, anchor, body}`. A non-advisory role (the executor `implementer`, the specs-writing audits `missing_tests` / `security_bug`) SHALL NOT advertise `submit_findings`.

The three advisory audits SHALL run through a56's `agentic_run` primitive WITH MCP enabled (capture mode retained, existing read-only allowed-tools list) so the tool is reachable; this supersedes a56's interim "audits run with no MCP" for these three roles ONLY. The agent returns findings by calling `submit_findings`; after the audit subprocess exits the daemon `consume_submission`s the stored payload (a56) to produce the `AuditOutcome::Reported` findings. A `submit_findings` call whose payload fails the role schema is rejected by `record_submission` AND surfaced to the agent as a correctable tool error it can retry in the same session; an audit run that ends with NO stored submission is an audit failure.

#### Scenario: Advertised only for advisory roles
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = architecture_consultative`
- **THEN** the `tools/list` response advertises `submit_findings` with the architecture finding schema alongside the common tools
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = implementer`, `missing_tests`, OR `security_bug`
- **THEN** `submit_findings` is NOT advertised

#### Scenario: Submission becomes the audit result
- **WHEN** an advisory audit's agent calls `submit_findings` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the subprocess exits the daemon `consume_submission`s the stored payload into `Finding` values for `AuditOutcome::Reported`

#### Scenario: Schema-invalid submission is correctable, not fatal
- **WHEN** a `submit_findings` payload violates the role schema (a missing required field, OR more than 5 architecture findings)
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session
- **AND** a single rejection does NOT fail the audit on its own — a subsequent valid submission in the same execution is accepted

#### Scenario: No submission fails the audit
- **WHEN** an advisory-audit subprocess exits with no stored submission for the execution
- **THEN** the audit returns `Err` (audit failure: state not updated, chatops audit-failure alert posts, the next iteration retries)

#### Scenario: Advisory audits gain MCP; specs-writing audits do not
- **WHEN** a `drift_audit`, `architecture_consultative`, OR `documentation_audit` run is built
- **THEN** it invokes `agentic_run` with MCP enabled (the `submit_findings` tool + `ORCH_MCP_ROLE`), in capture mode, with the audit's existing read-only allowed-tools list
- **WHEN** a `missing_tests` OR `security_bug` run is built
- **THEN** it invokes `agentic_run` with NO MCP (unchanged from a56), producing its on-disk proposal as before
