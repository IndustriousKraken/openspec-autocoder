# executor — delta for a69-antigravity-cli-strategy

## ADDED Requirements

### Requirement: AntigravityStrategy implements the `agy` CLI for agentic roles
The daemon SHALL provide a third `CliStrategy` (a56), `AntigravityStrategy`, for Google's Antigravity CLI (`agy`), so a role whose model provider resolves to `antigravity` (a55's `provider → CLI` rule for the Google/Antigravity provider, OR an explicit registry `cli: antigravity`) runs agentically instead of erroring with "no registered strategy." Antigravity CLI is the successor to the sunset Gemini CLI; the strategy targets `agy`, NOT `gemini`.

`AntigravityStrategy` SHALL build an `agy` invocation that: runs single-shot command mode (`agy -p "<prompt>"`, capture); selects the model via `--model <model>` (default `gemini-3-pro`); writes an `mcp_config.json` into the workspace carrying the MCP server entry (the MCP-child `command`/`args`, AND `env` including `ORCH_MCP_ROLE`, local stdio transport); AND maps a56's sandbox (allowed-tools list + deny patterns) onto Antigravity's tool restriction so a read-only role exposes only the read tools plus the role's `submit_*` tool and denies shell/write/edit. It SHALL set Antigravity's auth env (`AV_API_KEY`), NOT any `ANTHROPIC_*` (the claude strategy's mechanism), AND SHALL write neither `.mcp.json` (claude) NOR `opencode.json` (opencode).

`AntigravityStrategy` SHALL run in capture mode; the streaming-JSON event path (`final_answer` / `session_id` / incremental log) is claude-specific (Antigravity's `--stream` emits SSE, a different format). agy therefore serves the capture-mode structured-submission roles (the advisory audits, the reviewer, the contradiction check); the executor's streaming implementer path remains on the claude strategy until the strategy-agnostic-implementer change generalizes it. The agy integration SHALL surface MCP tool calls AND surface a daemon-rejected submission to the model as a correctable tool error it can retry in the same session — the same submission contract a56 requires of the claude path.

Because the exact non-interactive tool-restriction mechanism is confirmed by the integration spike, a read-only agy role SHALL NOT rely on the tool restriction alone: the existing read-only post-hoc write enforcement (`WritePolicy::None` — a non-empty post-run `git status --porcelain` reverts via `git reset --hard HEAD` AND fails the run) applies, so any write that escapes is caught and reverted rather than corrupting the workspace. The integration spike SHALL verify the restriction holds under `agy -p`.

Registering `agy` unblocks the non-Anthropic agentic paths of the reviewer (a58) AND the contradiction check (a59) for Google models; it does NOT change any role's default transport.

#### Scenario: Antigravity provider resolves to a working strategy
- **WHEN** a role's model resolves (via a55's `provider → CLI` rule, OR an explicit `cli: antigravity`) to the `agy` CLI
- **THEN** strategy resolution returns `AntigravityStrategy` (NOT a "no registered strategy" error)
- **AND** it builds an `agy -p` invocation selecting the model via `--model <model>`

#### Scenario: MCP and role env are delivered via mcp_config.json
- **WHEN** an `agy` role runs with a structured-submission tool (e.g. `submit_review`)
- **THEN** the strategy writes `mcp_config.json` with the MCP server entry (the MCP-child `command`/`args`, AND `env` including `ORCH_MCP_ROLE`, local stdio) so the role's `submit_*` tool is reachable
- **AND** neither `.mcp.json` NOR `opencode.json` is written for that run

#### Scenario: Model selection targets Antigravity auth, not Anthropic env
- **WHEN** the resolved model is a Google/Antigravity model (e.g. `gemini-3-pro`)
- **THEN** the invocation selects it via `--model <model>` AND the Antigravity auth env (`AV_API_KEY`) is set
- **AND** none of `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` is set

#### Scenario: Read-only sandbox denies write/edit/shell
- **WHEN** a read-only role (a56 sandbox: allow Read/Glob/Grep; deny Write/Edit/Bash) runs under agy
- **THEN** the generated Antigravity tool restriction exposes only the read tools plus the role's `submit_*` tool
- **AND** it denies shell, write, AND edit tools

#### Scenario: A write that escapes the restriction is caught by the post-hoc revert
- **WHEN** a read-only agy role nonetheless produces a non-empty post-run `git status --porcelain` (the non-interactive policy gap the spike probes)
- **THEN** the `WritePolicy::None` enforcement reverts the workspace via `git reset --hard HEAD` AND fails the run
- **AND** the escaped write does NOT persist into the workspace

#### Scenario: Capture mode only; streaming stays on claude
- **WHEN** an `agy` role runs through `agentic_run`
- **THEN** it uses capture mode (stdout/stderr read at exit), NOT the streaming-JSON parse path
- **AND** the executor's streaming implementer path continues to use the `claude` strategy

#### Scenario: Submission contract holds under agy
- **WHEN** an `agy` role's agent calls its `submit_*` tool AND the daemon rejects the payload (schema-invalid)
- **THEN** the rejection reaches the model as a tool error it can correct AND retry within the same `agy` session
- **AND** this matches the correctable-tool-error contract a56 requires of the `claude` path

#### Scenario: Non-Anthropic agentic roles function under agy
- **WHEN** the reviewer (`reviewer.kind: agentic`) OR the contradiction check is configured with a Google/Antigravity model
- **THEN** the role runs agentically via `AntigravityStrategy`
- **AND** it no longer errors / fails open on "no registered strategy"
