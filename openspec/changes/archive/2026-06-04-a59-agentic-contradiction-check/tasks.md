# Implementation tasks

## 1. Contradiction check runs through `agentic_run` (orchestrator-cli)

- [x] 1.1 Replace the `LlmClient::complete` + JSON-parse path with a56's `agentic_run`: read-only sandbox (`allowed_tools = ["Read","Glob","Grep"]`, `Bash`/`Write`/`Edit` denied), `ORCH_MCP_ROLE = contradiction_check`, the `submit_contradictions` MCP tool, capture mode. The prompt is the embedded `prompts/change-contradiction-check.md` (OR the `executor.change_internal_contradiction_check_prompt_path` override).
- [x] 1.2 The model from `executor.change_internal_contradiction_check_llm` is translated to the CLI's model-selection mechanism via a56's `ClaudeStrategy` (Anthropic-shaped until a60); keep the startup fail-fast when the check is enabled but the `_llm` block is unset.
- [x] 1.3 After the session, `consume_submission` (a56) → contradictions. Non-empty → write `.needs-spec-revision.json` (`revision_suggestion` from the contradictions narrative; empty `unarchivable_deltas`/`unimplementable_tasks`), fire `AlertCategory::SpecNeedsRevision`, halt the queue walk. Empty → proceed.
- [x] 1.4 Fail-open: a session error (spawn/timeout/strategy-not-implemented), a never-corrected schema rejection, OR no submission at session end → WARN + treat as "no contradictions found" + proceed. The daemon never gates iteration progress on the check.

## 2. `submit_contradictions` MCP tool (executor)

- [x] 2.1 `mcp_askuser_server.rs` — register `submit_contradictions` under a56's per-role framework, gated on `ORCH_MCP_ROLE = contradiction_check`; not advertised for any other role. Relay via a56's `relay_submission` → `record_submission`.
- [x] 2.2 Register the schema `{ contradictions: [{ requirement_a: string, requirement_b: string, summary: string }] }` with the control-socket validator. A schema-invalid payload is a correctable tool error (a56).
- [x] 2.3 Because the check is fail-open, a missing submission is consumed as an empty result (NOT an error) — the fail-open decision lives in the orchestrator-cli caller (task 1.4), not in the tool.

## 3. Tests

- [x] 3.1 Default-disabled spawns no contradiction-check session and proceeds to the executor.
- [x] 3.2 An enabled run invokes `agentic_run` with the read-only sandbox + `submit_contradictions`; the agent's submission of `{contradictions:[...]}` is consumed.
- [x] 3.3 An empty `submit_contradictions` array proceeds to the executor with no marker and no alert.
- [x] 3.4 A non-empty submission writes `.needs-spec-revision.json`, fires the `SpecNeedsRevision` alert, and halts the queue walk (executor not invoked this iteration).
- [x] 3.5 A session error (including a not-yet-implemented strategy) AND a session with no submission both WARN and fail open (proceed to executor).
- [x] 3.6 `submit_contradictions` is advertised only when `ORCH_MCP_ROLE = contradiction_check`.

## 4. Acceptance gate

- [x] 4.1 `cargo test` passes for the autocoder crate.
- [x] 4.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 4.3 `openspec validate a59-agentic-contradiction-check --strict` passes.
