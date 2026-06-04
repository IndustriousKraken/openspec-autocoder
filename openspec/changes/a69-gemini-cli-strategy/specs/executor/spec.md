# executor — delta for a69-gemini-cli-strategy

## ADDED Requirements

### Requirement: GeminiStrategy implements the gemini CLI for agentic roles
The daemon SHALL provide a third `CliStrategy` (a56), `GeminiStrategy`, for the `gemini` CLI, so a role whose model provider resolves to `gemini` (a55's `provider → CLI` rule mapping the Gemini provider to the `gemini` CLI, OR an explicit registry `cli: gemini`) runs agentically instead of erroring with "no registered strategy."

`GeminiStrategy` SHALL build a `gemini` invocation that: selects the model via `--model <model>`; writes a `.gemini/settings.json` into the workspace carrying the MCP `mcpServers` block (the MCP-child `command`/`args`, AND `env` including `ORCH_MCP_ROLE`); AND maps a56's sandbox (allowed-tools list + deny patterns) onto Gemini's `coreTools` **allowlist** so a read-only role exposes only the read tools plus the role's `submit_*` tool and excludes shell/write/edit. It SHALL set Gemini's auth env (e.g. `GEMINI_API_KEY` / Vertex), NOT any `ANTHROPIC_*` (the claude strategy's mechanism), AND SHALL write neither `.mcp.json` (claude) NOR `opencode.json` (opencode). The role's prompt SHALL be delivered by whichever mechanism non-interactive `gemini` accepts (stdin or `-p`/positional), as determined by the integration spike.

`GeminiStrategy` SHALL run in capture mode; the streaming-JSON event path (`final_answer` / `session_id` / incremental log) is claude-specific. gemini therefore serves the capture-mode structured-submission roles (the advisory audits, the reviewer, the contradiction check); the executor's streaming implementer path remains on the claude strategy until the capture-mode-implementer change generalizes it. The gemini integration SHALL surface MCP tool calls AND surface a daemon-rejected submission to the model as a correctable tool error it can retry in the same session — the same submission contract a56 requires of the claude path.

Because Gemini's tool-policy enforcement has reported gaps in non-interactive mode, a read-only gemini role SHALL NOT rely on the `coreTools` allowlist alone: the existing read-only post-hoc write enforcement (`WritePolicy::None` — a non-empty post-run `git status --porcelain` reverts via `git reset --hard HEAD` AND fails the run) applies, so any write that escapes the allowlist is caught and reverted rather than corrupting the workspace. The integration spike SHALL verify the allowlist holds in non-interactive mode.

Registering `gemini` unblocks the non-Anthropic agentic paths of the reviewer (a58) AND the contradiction check (a59) for Gemini models; it does NOT change any role's default transport.

#### Scenario: Gemini provider resolves to a working strategy
- **WHEN** a role's model resolves (via a55's `provider → CLI` rule, OR an explicit `cli: gemini`) to the `gemini` CLI
- **THEN** strategy resolution returns `GeminiStrategy` (NOT a "no registered strategy" error)
- **AND** it builds a `gemini` invocation selecting the model via `--model <model>`

#### Scenario: MCP and role env are delivered via .gemini/settings.json
- **WHEN** a `gemini` role runs with a structured-submission tool (e.g. `submit_review`)
- **THEN** the strategy writes `.gemini/settings.json` with an `mcpServers` entry (the MCP-child `command`/`args`, AND `env` including `ORCH_MCP_ROLE`) so the role's `submit_*` tool is reachable
- **AND** neither `.mcp.json` NOR `opencode.json` is written for that run

#### Scenario: Model selection targets Gemini auth, not Anthropic env
- **WHEN** the resolved model is a Gemini model
- **THEN** the invocation selects it via `--model <model>` AND the Gemini auth env (e.g. `GEMINI_API_KEY` / Vertex) is set
- **AND** none of `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` is set

#### Scenario: Read-only sandbox is enforced via the coreTools allowlist
- **WHEN** a read-only role (a56 sandbox: allow Read/Glob/Grep; deny Write/Edit/Bash) runs under gemini
- **THEN** the generated `coreTools` allowlist exposes only the read tools plus the role's `submit_*` tool
- **AND** it does NOT include shell, write, OR edit tools

#### Scenario: A write that escapes the allowlist is caught by the post-hoc revert
- **WHEN** a read-only gemini role nonetheless produces a non-empty post-run `git status --porcelain` (the non-interactive policy gap)
- **THEN** the `WritePolicy::None` enforcement reverts the workspace via `git reset --hard HEAD` AND fails the run
- **AND** the escaped write does NOT persist into the workspace

#### Scenario: Capture mode only; streaming stays on claude
- **WHEN** a `gemini` role runs through `agentic_run`
- **THEN** it uses capture mode (stdout/stderr read at exit), NOT the streaming-JSON parse path
- **AND** the executor's streaming implementer path continues to use the `claude` strategy

#### Scenario: Submission contract holds under gemini
- **WHEN** a `gemini` role's agent calls its `submit_*` tool AND the daemon rejects the payload (schema-invalid)
- **THEN** the rejection reaches the model as a tool error it can correct AND retry within the same `gemini` session
- **AND** this matches the correctable-tool-error contract a56 requires of the `claude` path

#### Scenario: Non-Anthropic agentic roles function under gemini
- **WHEN** the reviewer (`reviewer.kind: agentic`) OR the contradiction check is configured with a Gemini model
- **THEN** the role runs agentically via `GeminiStrategy`
- **AND** it no longer errors / fails open on "no registered strategy"
