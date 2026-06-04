# Implementation tasks

## 1. `submit_findings` MCP tool (executor)

- [x] 1.1 `mcp_askuser_server.rs` — register a `submit_findings` tool under a56's per-role advertisement framework, gated on `ORCH_MCP_ROLE ∈ {drift_audit, architecture_consultative, documentation_audit}`. Advertise it alongside the common tools; do NOT advertise it for the executor (`implementer`) or the specs-writing roles (`missing_tests`, `security_bug`).
- [x] 1.2 Register the audit-specific finding schema per role with the control socket's submission validator (a56 `record_submission`): drift `{capability, requirement, severity, code_anchors[], divergence}`; architecture `{subject, body, anchor, severity}` with `maxItems: 5` on the array; documentation `{category, severity, anchor, body}`. The tool relays via a56's `relay_submission` → `record_submission`.
- [x] 1.3 Define deserialization from each role's submitted payload to the existing `Finding` / audit finding types (no new finding shapes — reuse what `parse_findings` produced today).

## 2. Advisory audits run through `agentic_run` WITH MCP (executor/audits)

- [x] 2.1 `audits/drift.rs`, `audits/architecture_consultative.rs`, `audits/documentation_audit.rs` — invoke a56's `agentic_run` with MCP enabled (the `submit_findings` tool + `ORCH_MCP_ROLE = <audit id>`), capture mode, and the audit's existing read-only allowed-tools list. Remove the stdout-JSON `parse_findings` path for these three.
- [x] 2.2 After the subprocess exits, `consume_submission` (a56) → map the payload to findings → `AuditOutcome::Reported`. An empty array → `Reported(vec![])`. No stored submission → `Err` (audit failure: state not updated, chatops audit-failure alert, retry next iteration).
- [x] 2.3 `documentation_audit` — keep `query_canonical_specs` as a common tool when `canonical_rag` is enabled; `submit_findings` coexists with it. The `high → medium` severity demotion applies to the consumed submission (unchanged behavior, new transport).
- [x] 2.4 `audits/specs_writing.rs` (`missing_tests`, `security_bug`) — unchanged: still `agentic_run` with NO MCP, still produce on-disk proposals.

## 3. Tests

- [x] 3.1 `submit_findings` is advertised when `ORCH_MCP_ROLE` is each of the three advisory roles AND is absent for `implementer` / `missing_tests` / `security_bug`.
- [x] 3.2 A schema-valid `submit_findings` payload for each audit round-trips through `record_submission` → `consume_submission` to the expected `Finding` values and an `AuditOutcome::Reported`.
- [x] 3.3 An architecture payload with 6 findings is rejected by the schema as a correctable tool error; a subsequent valid (≤5) submission in the same execution succeeds.
- [x] 3.4 An advisory-audit execution that ends with no stored submission yields `Err` (audit failure); audit state is not updated.
- [x] 3.5 An empty `findings` array submission yields `AuditOutcome::Reported(vec![])` and posts no chatops unless `notify_on_clean: true`.

## 4. Acceptance gate

- [x] 4.1 `cargo test` passes for the autocoder crate.
- [x] 4.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 4.3 `openspec validate a57-advisory-audits-submit-findings --strict` passes.
