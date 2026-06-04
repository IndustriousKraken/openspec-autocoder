## Why

The three advisory audits — `drift_audit`, `architecture_consultative`, and `documentation_audit` — return their findings by emitting a single JSON object on the agent's stdout, which autocoder then parses. That transport is fragile in exactly the way a CLI-wrapped agent makes worse: any stray prose, a trailing log line, or a markdown fence around the JSON turns a real result into a parse failure (`Err` → audit failure → retry), and the agent gets no chance to correct because the parse happens after the process exits. a56 just landed the structured-submission MCP infrastructure (`record_submission` / `consume_submission`, the per-role `submit_*` advertisement framework) precisely so roles can return validated results in-session instead of through stdout scraping.

This change makes the advisory audits the first consumers of that infrastructure. Each audit's agent returns its findings by calling a `submit_findings` MCP tool whose payload is validated against the audit-specific finding schema; a schema violation is surfaced to the agent as a correctable tool error it can retry in the same session, and the daemon consumes the validated submission as the audit result. It is the change-4 step of the fleet migration: it retires stdout-JSON for the advisory audits and leaves the specs-writing audits (`missing_tests`, `security_bug`) untouched, since they produce on-disk proposals, not findings.

## What Changes

**`submit_findings` MCP tool (executor).** Built on a56's per-role submission framework, the per-execution MCP child advertises a `submit_findings` tool whenever `ORCH_MCP_ROLE` names an advisory audit (`drift_audit`, `architecture_consultative`, `documentation_audit`). The tool's payload schema is the audit-specific finding shape registered for that role — drift `{capability, requirement, severity, code_anchors, divergence}`; architecture `{subject, body, anchor, severity}` capped at 5; documentation `{category, severity, anchor, body}`. The tool relays through a56's `relay_submission` → `record_submission`; a schema-invalid payload is rejected and surfaced to the agent as a correctable tool error.

**The three advisory audits run with MCP enabled (executor).** Each advisory audit now invokes a56's `agentic_run` primitive WITH MCP (the `submit_findings` tool + `ORCH_MCP_ROLE`), in capture mode, with its existing read-only allowed-tools list — superseding a56's interim "audits run with no MCP" for these three roles only. The agent returns findings via `submit_findings`; after the subprocess exits the daemon `consume_submission`s the stored payload into `Finding` values for `AuditOutcome::Reported`. An audit run that ends with no stored submission is an audit failure (the old "malformed stdout" failure mode, restated for the new transport). The specs-writing audits keep their no-MCP path.

**Audit-requirement transport updated (orchestrator-cli).** The `Drift audit`, `Architecture consultative audit`, and `Documentation audit reports coverage, stale-reference, and organization findings` requirements are MODIFIED to change the findings transport from stdout-JSON to `submit_findings`. The sandbox, prompt, severity-filter, chatops-rendering, RAG, demotion, and audit-run-log scenarios are unchanged; only the transport and the "no valid output" failure mode change.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — MODIFIED `Drift audit`, `Architecture consultative audit`, AND `Documentation audit reports coverage, stale-reference, and organization findings` (findings transport stdout-JSON → `submit_findings`).
  - `executor` — ADDED `submit_findings MCP tool returns advisory-audit findings`.
- **Affected code:**
  - `autocoder/src/mcp_askuser_server.rs` — register the `submit_findings` tool + per-role finding schemas under a56's framework.
  - `autocoder/src/audits/{drift,architecture_consultative,documentation_audit}.rs` — call `agentic_run` with MCP enabled; replace `parse_findings`-from-stdout with `consume_submission`; no-submission → audit failure.
  - `autocoder/src/audits/{specs_writing,...}.rs` — unchanged (remain no-MCP).
- **Operator-visible behavior:** none intended. The same findings reach the same chatops surfaces; only the agent→daemon transport changes. A malformed result is now self-correctable in-session, so transient parse-failure retries should drop.
- **Acceptance:** `cargo test` passes; `openspec validate a57-advisory-audits-submit-findings --strict` passes. Tests: `submit_findings` advertised only for the three advisory roles; a valid payload round-trips to `AuditOutcome::Reported`; a >5-finding architecture payload is rejected as a correctable tool error then a valid retry succeeds; a run with no submission is an audit failure; an empty-array submission is a silent `Reported(vec![])`.
- **Dependencies:** stacks on **a56** (the `submit_*` framework, `record_submission` / `consume_submission`, and `agentic_run` with MCP). Independent of the later agentic roles (a58 reviewer, a59 contradiction-check), which add their own `submit_*` tools following the same pattern.
