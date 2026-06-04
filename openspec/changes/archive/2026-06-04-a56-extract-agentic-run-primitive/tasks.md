# Implementation tasks

## 1. Extract the shared primitive

- [x] 1.1 New module `autocoder/src/agentic_run.rs` defining `AgenticRunOutcome { timed_out: bool, exit_status: Option<ExitStatus>, stdout: String, stderr: String, final_answer: Option<String>, session_id: Option<String>, streamed_log: bool }`.
- [x] 1.2 `pub async fn agentic_run(opts: AgenticRunOpts) -> Result<AgenticRunOutcome>` where `AgenticRunOpts` carries: workspace, `&dyn CliStrategy`, prompt, sandbox (allowed_tools + disallowed_bash_patterns + disallowed_read_paths), optional MCP config (tools-to-expose + relay env), output mode (`Streaming` | `Capture`), timeout. It spawns in a new process group, pipes the prompt on stdin, captures stdout/stderr, enforces the timeout (the existing `tokio::select!` pattern), and parses the streaming-JSON events (`final_answer`, `session_id`, incremental log) ONLY in `Streaming` mode.
- [x] 1.3 Reuse `audits/mod.rs::write_sandbox_settings` (already shared) for the `--settings` file; do not re-implement it.

## 2. CliStrategy trait + claude implementation

- [x] 2.1 Define `trait CliStrategy`: `fn build_command(&self, opts) -> Command` (binary, flags, allowed-tools arg, settings/MCP-config file format) AND `fn apply_model_selection(&self, cmd: &mut Command, model: Option<&ResolvedModel>)` (env/flags for model selection).
- [x] 2.2 `struct ClaudeStrategy` reproducing today's invocation EXACTLY: `--settings <sandbox>`, `--allowedTools <build_allowed_tools_arg>`, `--permission-mode acceptEdits`, and (streaming mode) `--verbose --output-format stream-json`; MCP via `.mcp.json` (the existing `write_mcp_config` format). Model selection sets `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` when a model is given, AND sets nothing when `model` is `None` (preserving the executor's current CLI-default behavior).
- [x] 2.3 Resolve a role's strategy from the model's provider via a55's `provider → default CLI` rule (the only strategy registered in this change is `claude`; a non-`claude` resolution returns a clear "strategy not yet implemented" error until the opencode change lands).

## 3. Refactor the five call sites onto the primitive (no behavior change)

- [x] 3.1 `executor/claude_cli.rs::run_subprocess` → thin caller of `agentic_run` with `ClaudeStrategy`, `Streaming` mode, MCP enabled. Replace `SubprocessOutcome` with `AgenticRunOutcome`. The recovery loop / session-id reuse is unchanged.
- [x] 3.2 `audits/drift.rs`, `audits/architecture_consultative.rs`, `audits/documentation_audit.rs`, `audits/specs_writing.rs` — delete each local `run_subprocess` + `SubprocessOutcome`; call `agentic_run` with `ClaudeStrategy`, `Capture` mode, NO MCP, and the audit's existing allowed-tools list. Preserve each audit's ETXTBSY retry.
- [x] 3.3 Confirm no `run_subprocess` / `SubprocessOutcome` definitions remain outside `agentic_run.rs` (grep).

## 4. Per-role submission MCP infrastructure (framework only)

- [x] 4.1 `mcp_askuser_server.rs` — add `relay_submission(role, payload)` paralleling `relay_record_outcome` (control-socket request `{ action: "record_submission", workspace_basename, change, role, payload }`). Add the advertisement framework: read `ORCH_MCP_ROLE` from env; advertise the role's `submit_*` tool (definitions added per-role by later changes) alongside the common tools. No concrete `submit_*` tool is wired in this change.
- [x] 4.2 `write_mcp_config` — write `ORCH_MCP_ROLE` into the `.mcp.json` env when a role is supplied.
- [x] 4.3 `control_socket.rs` — add `record_submission` (store execution-scoped submission) AND `consume_submission` (retrieve + clear) actions, paralleling `record_outcome` / `consume_outcome`.

## 5. Tests

- [x] 5.1 The executor path through `agentic_run` yields the same streaming log + `final_answer` + outcome classification as the pre-refactor path for a canned run.
- [x] 5.2 An audit path through `agentic_run` (`Capture`, no MCP) yields the same `stdout`/`exit_status` outcome as the pre-refactor audit `run_subprocess`.
- [x] 5.3 `ClaudeStrategy::apply_model_selection` sets none of the `ANTHROPIC_*` vars when `model` is `None`, AND sets all three when a `ResolvedModel` is given.
- [x] 5.4 A provider resolution to a CLI with no registered strategy returns a clear error naming the CLI (only `claude` is registered in this change).
- [x] 5.5 `relay_submission` → `record_submission`; `consume_submission` returns the stored payload then clears it.

## 6. Acceptance gate

- [x] 6.1 `cargo test` passes for the autocoder crate.
- [x] 6.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 6.3 `openspec validate a56-extract-agentic-run-primitive --strict` passes.
