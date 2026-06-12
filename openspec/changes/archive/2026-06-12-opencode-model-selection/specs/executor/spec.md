# executor — delta for opencode-model-selection

## MODIFIED Requirements

### Requirement: OpencodeStrategy implements the opencode CLI for agentic roles
The daemon SHALL provide a second `CliStrategy` (a56), `OpencodeStrategy`, for the `opencode` CLI, so a role whose model provider resolves to `opencode` (a55's `provider → CLI` rule for `openai_compatible`/`ollama`, OR an explicit registry `cli: opencode`) runs agentically instead of erroring with "no registered strategy."

`OpencodeStrategy` SHALL build an `opencode run` invocation whose model selection follows opencode's own contract: opencode's `--model` is `<opencode-provider-id>/<model>`, where the provider is one opencode actually knows. Autocoder's `LlmProvider` value (e.g. `openai_compatible`) is an API *type*, NOT an opencode provider id, AND SHALL NOT be used as the `--model` provider segment (`opencode models openai_compatible` returns "Provider not found"). Two cases follow:

- **autocoder DEFINES the provider** — `ollama` (always: opencode is not `auth login`-ed to a local daemon, so its base URL must be supplied) AND `openai_compatible` WHEN an `api_key` is supplied (autocoder injects it). The strategy SHALL write an `opencode.json` `provider` block carrying the base URL (and, for a supplied key, `apiKey` as an `{env:…}` REFERENCE — never the raw secret, which rides the subprocess env) AND select `--model <provider-id>/<model>`, where `<provider-id>` matches the block it wrote.
- **autocoder DEFERS to opencode's own auth** — an authenticating provider (`openai_compatible`) with NO `api_key` (the operator authenticated it out-of-band via `opencode auth login`). The strategy SHALL write NO `provider` block — a key-less block would shadow opencode's own stored credentials for that provider and break authentication ("No cookie auth credentials found") — AND SHALL pass the operator-configured model to `--model` VERBATIM. The operator's `model` value MUST therefore be the real opencode id (e.g. `openrouter/qwen/qwen3-max`); autocoder neither assumes nor infers the provider.

In all cases the strategy SHALL write the `opencode.json` `mcp` block (`type: local`, the MCP-child command, AND env including `ORCH_MCP_ROLE`) AND map a56's sandbox (allowed-tools list + deny patterns) onto opencode's permission configuration so a read-only role keeps its read-only profile. It SHALL set NO `ANTHROPIC_*` env (that is the `claude` strategy's mechanism), AND SHALL NOT write `.mcp.json` (the `claude` MCP format). The role's prompt SHALL be delivered by whichever mechanism headless `opencode run` accepts.

Every agentic role that drives opencode — the verifier gates AND the agentic reviewer — SHALL pass its resolved model to the strategy (NOT `None`), so opencode runs the operator-configured model rather than opencode's own default. (A role that passes `None` would silently run opencode's default while any verdict attribution named the configured model.)

`OpencodeStrategy` SHALL run in capture mode; the streaming-JSON event path (`final_answer` / `session_id` / incremental log) is `claude`-specific. opencode therefore serves the capture-mode structured-submission roles (the advisory audits, the reviewer, the contradiction check); the executor's streaming implementer path remains on the `claude` strategy. The opencode integration SHALL surface MCP tool calls AND surface a daemon-rejected submission to the model as a correctable tool error it can retry in the same session — the same submission contract a56 requires of the `claude` path.

Registering `opencode` unblocks the non-Anthropic agentic paths of the reviewer (a58) AND the contradiction check (a59); it does NOT change any role's default transport.

#### Scenario: Opencode provider resolves to a working strategy
- **WHEN** a role's model resolves (via a55's `provider → CLI` rule, OR an explicit `cli: opencode`) to the `opencode` CLI
- **THEN** strategy resolution returns `OpencodeStrategy` (NOT a "no registered strategy" error)
- **AND** it builds an `opencode run` invocation

#### Scenario: MCP and role env are delivered via opencode.json
- **WHEN** an `opencode` role runs with a structured-submission tool (e.g. `submit_review`)
- **THEN** the strategy writes `opencode.json` with an `mcp` block (`type: local`, the MCP-child command, env including `ORCH_MCP_ROLE`) so the role's `submit_*` tool is reachable
- **AND** NO `.mcp.json` is written for that run

#### Scenario: A login-authed provider defers to opencode (no block, verbatim model)
- **WHEN** the resolved model is an `openai_compatible` provider with NO `api_key` AND `model` `openrouter/qwen/qwen3-max`
- **THEN** `opencode.json` carries NO `provider` block (opencode resolves the provider + credentials from its own `auth login` + config)
- **AND** the invocation selects `--model openrouter/qwen/qwen3-max` VERBATIM — autocoder's `openai_compatible` type is NOT prefixed
- **AND** none of `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` is set

#### Scenario: A keyed provider is defined by autocoder
- **WHEN** the resolved model is `(openai_compatible, <model>, <base_url>, <key>)` (a key IS supplied)
- **THEN** `opencode.json` carries a `provider` block with the base URL AND `apiKey` as an `{env:…}` reference (the secret on the subprocess env, never raw in the file)
- **AND** the invocation selects `--model openai_compatible/<model>` (matching the block autocoder wrote)
- **AND** none of `ANTHROPIC_*` is set

#### Scenario: Ollama is always defined by autocoder
- **WHEN** the resolved model is `(ollama, <model>, <base_url>, "")`
- **THEN** `opencode.json` carries an `ollama` `provider` block with the base URL (no `apiKey`)
- **AND** the invocation selects `--model ollama/<model>`

#### Scenario: Agentic roles run their configured model, not opencode's default
- **WHEN** the agentic reviewer OR a verifier gate runs through `OpencodeStrategy`
- **THEN** it passes its resolved model to the strategy (not `None`)
- **AND** opencode runs that model, not opencode's own default

#### Scenario: Read-only sandbox is enforced via opencode permissions
- **WHEN** a read-only role (a56 sandbox: allow Read/Glob/Grep; deny Write/Edit/Bash) runs under opencode
- **THEN** the generated opencode permission configuration denies Write, Edit, AND Bash
- **AND** exposes only the read tools plus the role's MCP submission tool

#### Scenario: Capture mode only; streaming stays on claude
- **WHEN** an `opencode` role runs through `agentic_run`
- **THEN** it uses capture mode (stdout/stderr read at exit), NOT the streaming-JSON parse path
- **AND** the executor's streaming implementer path continues to use the `claude` strategy

#### Scenario: Submission contract holds under opencode
- **WHEN** an `opencode` role's agent calls its `submit_*` tool AND the daemon rejects the payload (schema-invalid)
- **THEN** the rejection reaches the model as a tool error it can correct AND retry within the same `opencode run` session
- **AND** this matches the correctable-tool-error contract a56 requires of the `claude` path

#### Scenario: Non-Anthropic agentic roles now function
- **WHEN** the reviewer (`reviewer.kind: agentic`) OR the contradiction check is configured with a model whose provider resolves to `opencode`
- **THEN** the role runs agentically via `OpencodeStrategy`
- **AND** it no longer errors / fails open on "no registered strategy"
