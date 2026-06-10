# orchestrator-cli — delta for warn-pathless-openai-base

## MODIFIED Requirements

### Requirement: `check-config` subcommand validates a config file without side effects
autocoder SHALL ship a `check-config` subcommand alongside `run`, `reload`, `rewind`, `audit run`, and `install`. The subcommand SHALL accept `--config <path>` (required) AND `--json` (optional flag). It SHALL run the same validation pipeline `autocoder run` executes at startup (YAML parse, schema validation, token-route resolution, workspace-collision check, audit-slug validation, path-collision check, secret-source check) AND exit with one of three codes: `0` on a fully-valid config, `1` on a config that passes hard-error checks but has at least one WARN-level finding, `2` on at least one hard error. The subcommand SHALL NOT spawn any daemon work, SHALL NOT mutate any file, AND SHALL NOT contact any external service.

Schema validation SHALL additionally emit an ADVISORY WARN-level finding (category `schema`, never a hard error) when a `models:` registry entry whose provider is `openai_compatible` or `ollama` has a **path-less** `api_base_url` — a host[:port] with no path segment, or a bare trailing slash (e.g. `http://host:11434`). A registry model always drives an agentic CLI, which posts to `<base>/chat/completions`, so a path-less base typically returns 404; most OpenAI-compatible endpoints (Ollama included) serve under `/v1`. The WARN SHALL name the model AND carry the `config_pointer` `models/<name>/api_base_url`. It is scoped to the registry; the one-shot HTTP Ollama path (native `/api/chat`, bare base) SHALL NOT be flagged.

A shared free function `validate_config(config: &Config) -> ValidationReport` SHALL host every check. The `check-config` subcommand AND the `autocoder run` startup path SHALL both call this function so the surface stays in sync — there is no "check-config validates extra things" OR "autocoder run skips a check" drift.

#### Scenario: Valid config exits 0 with OK lines
- **WHEN** an operator runs `autocoder check-config --config <valid-config-path>`
- **THEN** the subcommand exits 0
- **AND** stdout contains one `OK:` line per validated category (schema, token-route, workspace-collision, audit-slug, path-collision, secret-source)
- **AND** stderr is empty

#### Scenario: Schema violation exits 2 with an ERROR line and stderr summary
- **WHEN** the config has `repositories[0].poll_interval_sec: 0` (a schema violation)
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 2
- **AND** stdout contains a line starting with `ERROR: schema:` naming the offending field AND its `config_pointer` (e.g. `repositories/0/poll_interval_sec`)
- **AND** stderr contains a summary line: `check-config: 1 error(s), 0 warning(s) in <path>`

#### Scenario: Missing env var produces a WARN and exits 1
- **WHEN** the config references `github.token_env: GITHUB_TOKEN` AND the `GITHUB_TOKEN` env var is unset in the calling environment AND no inline `github.token` is set
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 1
- **AND** stdout contains a line starting with `WARN: secret-source:` naming the env var
- **AND** stderr contains `check-config: 0 error(s), 1 warning(s) in <path>`
- **AND** the WARN does not block: a config that has only WARNs but no ERRORs still exits 1 (not 2)

#### Scenario: Path-less registry base produces an advisory WARN
- **WHEN** the config's `models:` registry has an entry with `provider: ollama` (or `openai_compatible`) AND `api_base_url: http://host:11434` (no path segment)
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 1 (advisory only — not a hard error)
- **AND** stdout contains a line starting with `WARN: schema:` naming the model AND carrying `config_pointer` `models/<name>/api_base_url`, suggesting the base include its path (e.g. `/v1`)
- **AND** a registry entry whose `api_base_url` already includes a path (e.g. `http://host:11434/v1`) produces no such WARN

#### Scenario: Parse failure exits 2 with the serde_yaml diagnostic
- **WHEN** the config file contains malformed YAML
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 2
- **AND** stdout contains a line starting with `ERROR: parse:` AND the serde_yaml error message (including line/column where available)
- **AND** no other validation categories are reported (validation cannot continue past a parse failure)

#### Scenario: Token-route gap exits 2 with a structured diagnostic
- **WHEN** the config has `repositories[1].url` with owner `my-org-b` AND `github.owner_tokens` has no `my-org-b` entry AND `github.token_env` references an unset env var AND no inline `github.token` is set
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 2
- **AND** stdout contains a line starting with `ERROR: token-route:` naming the unresolved owner AND the repo's `config_pointer`

#### Scenario: --json flag emits one JSON object per finding plus a summary
- **WHEN** the operator runs `autocoder check-config --config <path> --json`
- **THEN** stdout contains one JSON object per line, each shaped `{"level": "error"|"warn"|"ok", "category": "<slug>", "message": "<text>", "config_pointer": "..."}`
- **AND** the final line is `{"level": "summary", "errors": N, "warnings": M, "config": "<path>"}`
- **AND** every line is independently parseable as JSON
- **AND** exit code matches the non-JSON behavior (0 / 1 / 2)

#### Scenario: `autocoder run` startup uses the same validation pipeline
- **WHEN** `autocoder run` starts up against a config with a hard error
- **THEN** the startup path invokes `validate_config(&config)` AND reads `report.errors`
- **AND** if any errors are present, the daemon exits non-zero with the same error message `check-config` would produce
- **AND** the existing startup-error tests continue to pass without modification
