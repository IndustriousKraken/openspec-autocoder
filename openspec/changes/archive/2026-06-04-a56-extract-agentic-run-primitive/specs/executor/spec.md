# executor — delta for a56-extract-agentic-run-primitive

## ADDED Requirements

### Requirement: Shared agentic-run primitive
The daemon SHALL provide a single agentic-run primitive that wraps a CLI as a subprocess, hands it a prompt, and runs an agentic session to completion. Every CLI-wrapping role — the executor AND every audit, AND the agentic roles added by later changes — SHALL use this primitive; the per-module `run_subprocess` functions AND their duplicated `SubprocessOutcome` structs SHALL be removed.

The primitive SHALL accept the workspace, a `CliStrategy`, the prompt (delivered on stdin), the sandbox configuration (allowed-tools list AND disallowed bash/read patterns), an optional MCP configuration (which tools to expose AND the control-socket relay environment), an output mode (streaming-JSON OR simple-capture), AND a timeout. It SHALL spawn the child in its own process group, enforce the timeout via the existing select-and-kill pattern, AND return a unified `AgenticRunOutcome` carrying `timed_out`, `exit_status`, `stdout`, `stderr`, an optional `final_answer`, an optional `session_id`, AND whether a streamed log was written. The streaming-JSON event parsing (`final_answer`, `session_id`, incremental log) SHALL run ONLY in streaming mode; simple-capture mode reads stdout/stderr at exit.

The refactor SHALL be behavior-neutral: the executor retains streaming-JSON + MCP + the recovery/session-reuse path; each audit retains simple-capture + no-MCP + its existing read-only tool list AND its ETXTBSY retry.

#### Scenario: Executor path is behavior-identical through the primitive
- **WHEN** the executor runs a canned change through the primitive in streaming mode with MCP enabled
- **THEN** the streamed per-change log, the parsed `final_answer`, AND the outcome classification are identical to the pre-refactor `run_subprocess` for the same inputs

#### Scenario: Audit path is behavior-identical through the primitive
- **WHEN** an audit runs through the primitive in simple-capture mode with no MCP AND its existing allowed-tools list
- **THEN** the returned `stdout` AND `exit_status` are identical to the pre-refactor audit `run_subprocess`
- **AND** no `.mcp.json` is written for that run

#### Scenario: Single source of truth
- **WHEN** the codebase is searched after this change
- **THEN** no `run_subprocess` or `SubprocessOutcome` definition exists outside the agentic-run module

### Requirement: CliStrategy trait with the claude implementation
The agentic-run primitive SHALL select its CLI invocation through a `CliStrategy` trait so a model's provider can determine the CLI without role code changing. The trait SHALL do two jobs: build the invocation (binary, flags, the allowed-tools/sandbox-settings format, AND the MCP-config-file format) AND translate a resolved `(provider, model, api_base_url, api_key)` into that CLI's model-selection mechanism. A role's strategy SHALL be resolved from the model's provider via the model registry's `provider → default CLI` rule.

This change SHALL implement the `claude` strategy AND reproduce today's invocation exactly: `--settings <sandbox-file>`, `--allowedTools <combined>`, `--permission-mode acceptEdits`, AND — in streaming mode — `--verbose --output-format stream-json`, with MCP delivered via `.mcp.json`. The `claude` strategy SHALL select the model via `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` ONLY when a model is configured; when no model is configured it SHALL set none of them, preserving the executor's current CLI-default behavior. A role whose provider resolves to a CLI with no registered strategy SHALL return a clear error naming that CLI; this change registers only the `claude` strategy, so any non-`claude` resolution errors until that CLI's strategy is added (the `opencode` strategy is added by a later change).

#### Scenario: Claude strategy with no model preserves CLI-default behavior
- **WHEN** the `claude` strategy builds an invocation with `model: None` (the executor's current state)
- **THEN** none of `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` is set
- **AND** the invocation is byte-identical to the pre-refactor executor command

#### Scenario: Claude strategy with a model sets the selection env
- **WHEN** the `claude` strategy builds an invocation with a resolved model `(anthropic, claude-opus-4-8, base, key)`
- **THEN** `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, AND `ANTHROPIC_MODEL` are set from the resolved tuple

#### Scenario: A CLI with no registered strategy returns a clear error
- **WHEN** a role's model resolves (via the registry rule) to a CLI that has no registered strategy (e.g. `opencode`, before its strategy is added)
- **THEN** strategy resolution returns an error naming the CLI
- **AND** no subprocess is spawned

### Requirement: Per-execution MCP child exposes a per-role submission tool via control-socket relay
The per-execution MCP child SHALL support a per-role structured-submission tool family that relays a schema-validated payload to the daemon over the control socket, paralleling the existing `outcome_*` / `record_outcome` relay. The MCP child SHALL read an `ORCH_MCP_ROLE` value from its environment (written into `.mcp.json` by the config writer) AND advertise only that role's `submit_*` tool alongside the common tools; a child with no role advertises no submission tool.

This change establishes the framework AND the relay helper only. The concrete per-role tools (`submit_findings`, `submit_review`, `submit_contradictions`, `submit_verdict`) AND their schemas SHALL be added by the changes that consume them, each following this pattern. The relay SHALL send a control-socket request naming the role AND the payload, AND SHALL surface a tool error to the agent when the daemon rejects the submission (e.g. schema-invalid), so the agent can correct AND retry in the same session.

#### Scenario: Role-scoped advertisement
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE` set to a role that has a registered submission tool
- **THEN** the `tools/list` response advertises that role's `submit_*` tool AND the common tools (e.g. `query_canonical_specs`)
- **AND** it does NOT advertise submission tools for other roles

#### Scenario: Submission relays to the daemon
- **WHEN** an agent calls its role's `submit_*` tool with a valid payload
- **THEN** the MCP child relays a `record_submission` request over the control socket naming the role AND the payload
- **AND** a daemon rejection (e.g. schema-invalid) is surfaced to the agent as a correctable tool error
