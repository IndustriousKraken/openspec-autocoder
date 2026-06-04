## Why

The change-internal contradiction pre-flight check is the third LLM-over-HTTP caller in the daemon: it concatenates a change's spec-delta files into one prompt, calls `LlmClient::complete`, and parses a `{ contradictions: [...] }` JSON object out of the free-text response. Same fragility as the audits and the reviewer had before their migrations — a stray line around the JSON turns a real result into a parse failure. a56 landed `agentic_run` and the per-role `submit_*` framework; a57 (audits) and a58 (reviewer) already moved onto it. This change migrates the contradiction check the same way, completing the "kill stdout/HTTP-JSON for LLM steps" arc for the pre-executor pipeline.

The migration is unusually low-risk here because the check is **fail-open by contract**: any failure (transport, parse, malformed) already degrades to "no contradictions found" and proceeds. That property carries straight into the agentic world — if the resolved CLI strategy isn't implemented yet (a non-`claude` command before opencode/a60), the session errors and the check fails open with a WARN, exactly as a transport error would. So the check can migrate wholesale with no `kind` selector: non-Anthropic operators who enabled it simply see it degrade to a logged no-op until a60, never a break.

## What Changes

**Contradiction check runs agentically (orchestrator-cli).** When enabled, the check now runs through a56's `agentic_run` primitive in a read-only sandbox (`Read`, `Glob`, `Grep` — NO `Bash`/`Write`/`Edit`) with `ORCH_MCP_ROLE = contradiction_check` and the `submit_contradictions` MCP tool. The agent reads the change's spec-delta files on demand and returns its findings by calling `submit_contradictions` instead of emitting JSON on stdout. The embedded `prompts/change-contradiction-check.md` prompt, the prompt-override knob, the opt-in gating, the `.needs-spec-revision.json` marker, the `AlertCategory::SpecNeedsRevision` alert, the queue halt, and the startup fail-fast are all unchanged. The `executor.change_internal_contradiction_check_llm` block continues to configure the model — now translated into the CLI's model-selection mechanism by a56's strategy (Anthropic-shaped until a60).

**Fail-open posture extended to the new transport.** Session errors (spawn, timeout, strategy-not-implemented), a schema-rejected submission the agent never corrects, and a session that ends with no submission all log a WARN and treat the check as "no contradictions found." A schema-invalid `submit_contradictions` call mid-session is a correctable tool error the agent can retry, per a56.

**`submit_contradictions` MCP tool (executor).** Built on a56's per-role framework, advertised only when `ORCH_MCP_ROLE = contradiction_check`. Payload `{ contradictions: [{ requirement_a, requirement_b, summary }] }`. Relays through `record_submission`; consumed after the session. Because the check is fail-open, a missing submission is NOT an audit-style failure — it is consumed as an empty result.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — MODIFIED `Change-internal contradiction pre-flight check (opt-in)` (transport HTTP-JSON → agentic `submit_contradictions`; fail-open posture preserved and extended).
  - `executor` — ADDED `submit_contradictions MCP tool returns change-internal contradictions`.
- **Affected code:**
  - `autocoder/src/<contradiction-check module>.rs` — replace the `LlmClient::complete` + JSON-parse path with `agentic_run` (read-only sandbox + `submit_contradictions`); `consume_submission` → contradictions; no/failed submission → fail-open.
  - `autocoder/src/mcp_askuser_server.rs` — register `submit_contradictions` + schema under a56's framework, gated on `ORCH_MCP_ROLE = contradiction_check`.
- **Operator-visible behavior:** none for Anthropic-shaped configs (same opt-in, same marker, same alert, same fail-open). A non-Anthropic-configured check degrades to a logged no-op (fail-open) until a60, rather than calling its prior HTTP endpoint.
- **Acceptance:** `cargo test` passes; `openspec validate a59-agentic-contradiction-check --strict` passes. Tests: default-disabled spawns no session; an enabled run reads deltas and submits; an empty submission proceeds; a non-empty submission writes the marker + alert + halts; a session error and a no-submission session both fail open with a WARN; `submit_contradictions` advertised only for the `contradiction_check` role.
- **Dependencies:** stacks on **a56** (`agentic_run`, the `submit_*` framework, `record_submission`/`consume_submission`) and **a55** (`provider → CLI` strategy resolution). Independent of a57/a58 (different roles). a61 later reframes this check as the change-internal-consistency `[in]` gate of the verifier trio; a59 only migrates the transport.
