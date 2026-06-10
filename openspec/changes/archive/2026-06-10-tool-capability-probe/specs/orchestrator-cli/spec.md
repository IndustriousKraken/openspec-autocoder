# orchestrator-cli — delta for tool-capability-probe

## ADDED Requirements

### Requirement: Startup tool-capability probe for agentic model endpoints
The agentic roles (the verifier gates `[in]`/`[canon]`/`[out]` AND the agentic reviewer) drive their model through a tool-using CLI session — the model must emit tool calls (read the change, then call a `submit_*` MCP tool). A model whose endpoint cannot emit tool calls cannot serve these roles, and the fail-closed gate would hold every change with an opaque cause. To surface this BEFORE a change is held, the daemon SHALL run a tool-capability probe at startup.

After the dependency preflight AND before the first polling iteration, the daemon SHALL, for each `models:` registry entry whose provider is `openai_compatible` or `ollama`, send ONE tool-calling request to `<api_base_url>/chat/completions` (the path the agentic CLI uses) carrying a trivial tool definition, AND inspect the response:
- A response that carries a tool call → an INFO log line that the model is usable for agentic roles.
- A response that carries no tool call, OR a 4xx that rejects the tools request → a WARN-level log line naming the model AND the remedy (use a model whose template supports tools; `ollama show <model>` lists `tools`).
- A probe that cannot complete (connection error, timeout, 5xx, undecodable body) → a WARN-level "could not run" line; tool support is left unverified.

The probe SHALL be best-effort AND time-bounded: it SHALL NEVER block or fail startup, regardless of outcome. It SHALL skip `anthropic`/`google` registry entries (their `claude`/`agy` CLIs self-authenticate AND are known tool-capable, and no key is available to probe them) AND SHALL skip an `openai_compatible` entry with no resolvable config key (no way to authenticate the probe). Because the probe makes a network call, it is a startup-only behavior AND SHALL NOT be part of the side-effect-free `check-config` pipeline.

#### Scenario: A toolless model is flagged at startup
- **WHEN** the daemon starts with a `models:` registry entry for an `ollama` model whose endpoint answers the probe in prose without a tool call (OR rejects the tools request)
- **THEN** the daemon emits a WARN-level log line identifying that model AND stating the agentic gates require tool calling
- **AND** startup proceeds normally (the probe never blocks startup)

#### Scenario: A tool-capable model logs an info line
- **WHEN** the daemon starts with a registry `ollama`/`openai_compatible` model whose endpoint returns a tool call to the probe
- **THEN** the daemon emits an INFO-level log line that the model is usable for agentic roles
- **AND** no WARN is emitted for that model

#### Scenario: An unreachable endpoint does not block startup
- **WHEN** a probed model's endpoint cannot be reached (connection error or timeout) at startup
- **THEN** the daemon emits a WARN-level "could not run" line for that model
- **AND** the daemon continues startup and enters its normal polling state

#### Scenario: CLI-self-authenticating providers are not probed
- **WHEN** a `models:` registry entry's provider is `anthropic` or `google`
- **THEN** the daemon does NOT probe it (the `claude`/`agy` CLIs self-authenticate AND are known tool-capable)

#### Scenario: The probe is not part of check-config
- **WHEN** an operator runs `autocoder check-config`
- **THEN** no tool-capability probe is performed (check-config contacts no external service)
