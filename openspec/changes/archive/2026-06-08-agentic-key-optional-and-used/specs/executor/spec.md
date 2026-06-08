# executor — delta for agentic-key-optional-and-used

## MODIFIED Requirements

### Requirement: CLI strategies pass no LLM credential to the wrapped subprocess
An agentic CLI role's credential handling SHALL depend on whether the operator supplied an `api_key`, per the two cases below:

- **No key (the default).** No `CliStrategy` SHALL place any LLM credential in the wrapped subprocess — NOT in a workspace file (`opencode.json`, `mcp_config.json`, `.gemini/*`, etc.), AND NOT in the subprocess environment. The strategy SHALL select the model (e.g. `--model`) AND rely on the CLI's **own** authentication — its credential store or login (`claude login`, opencode / its provider config, `agy` login), or the operator's out-of-band CLI provider config (e.g. opencode → OpenRouter). This is the safe default: no credential ever reaches the model.
- **Key supplied (an explicit opt-in).** When a CLI role has a configured `api_key`, the strategy SHALL pass it to the CLI so the CLI uses that key — uniformly across the three CLIs: `claude` via `ANTHROPIC_API_KEY`, the `opencode` strategy via opencode's own provider config, AND `agy` via `AV_API_KEY`. A supplied key SHALL be placed where the existing config-store protection covers it — the CLI's own config store, reached by the `engine_deny` tool denylist — AND SHALL NEVER be written into a workspace file (a workspace file can be committed AND is freely readable by the model).

The supplied-key path cannot fully isolate the credential from the model: the model AND the wrapped CLI are the **same process AND uid**, so a key the CLI can use is one the model can ultimately reach. `engine_deny` is deterrence, not a bound (see the os-hide/engine-deny requirement), AND a CLI that accepts a key only via the subprocess environment (e.g. `claude` → `ANTHROPIC_API_KEY`) leaves the key readable from the model's own environment. Supplying a key is therefore an explicit operator opt-in to that exposure; the no-key default preserves the no-credential posture. The daemon SHALL document this residual rather than claim isolation it cannot provide.

A resolved `api_key` SHALL still flow to autocoder's **in-process** HTTP clients (the non-agentic `oneshot` reviewer AND any RAG/embedding HTTP call), which the daemon calls directly so the key stays in the daemon's process; those are not subprocesses AND are unaffected by the CLI-role rules above.

#### Scenario: No-key CLI role passes no credential to the subprocess
- **WHEN** a CLI role's resolved model has no `api_key`
- **THEN** the strategy writes no credential into any workspace file
- **AND** sets no credential in the subprocess environment
- **AND** the CLI authenticates from its own login / credential store

#### Scenario: A supplied key is passed to the CLI
- **WHEN** a CLI role's resolved model has a non-empty `api_key`
- **THEN** the strategy makes the CLI use that key — `claude` via `ANTHROPIC_API_KEY`, `opencode` via its provider config, `agy` via `AV_API_KEY`

#### Scenario: A supplied key is never written to a workspace file
- **WHEN** any `CliStrategy` writes its config for a role whose resolved model has an `api_key`
- **THEN** no credential appears in any file written into the workspace (e.g. the workspace `opencode.json` carries the MCP block, the permission/sandbox config, AND the provider's model + base URL, but NOT the `api_key`)
- **AND** a supplied key is placed only in a location covered by `engine_deny` (the CLI's own config store) OR, for a CLI that accepts a key only via the environment, in the subprocess environment with the residual documented

#### Scenario: The supplied-key location is engine-deny covered
- **WHEN** a key is supplied AND written to the CLI's config store
- **THEN** that location is included in the `engine_deny` tool-denylist applied for the run
- **AND** the protection is understood as deterrence, not a bound (same-process / same-uid residual)

#### Scenario: In-process HTTP roles still receive the key
- **WHEN** the non-agentic `oneshot` reviewer (or a RAG/embedding HTTP call) runs with a configured `api_key`
- **THEN** the key is used by the daemon's in-process HTTP client for that call
- **AND** the key is never passed to a subprocess (file or env)
