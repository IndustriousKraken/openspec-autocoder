## Why

The daemon's only config validation today is its startup parse. Bad config means the daemon refuses to start AND systemd's `Restart=on-failure` puts it in a restart loop until an operator intervenes. There is no way to ask "is my config valid for this binary?" without actually trying to run the binary against it.

Two cases motivate a dedicated validator:

1. **Unattended upgrades** (`update.sh`, landing in `a04`). If the new release adds a required config field, swapping the binary then restarting hits the restart loop. The fix is to validate the new binary against the existing config BEFORE swapping — but that requires the new binary to expose a non-`run` validation entry point.
2. **CI pipelines and pre-flight scripts.** Operators editing `config.yaml` by hand want a `does this parse?` check without standing up a full daemon process against their actual repositories.

The `autocoder run` startup path already does this work — parse YAML, resolve paths, validate workspace collisions, validate token routes, validate audit slugs. The subcommand surfaces the same checks as a separate verb with no side effects.

## What Changes

**New subcommand `autocoder check-config --config <path>`.** Runs the full config-load + validation pipeline (the same code `autocoder run` executes at startup, minus the actual daemon spawn). Exits 0 if the config is valid for this binary; exits non-zero with a structured diagnostic if not.

**Validation surface.** The subcommand runs every check the daemon currently runs at startup:

- YAML parse (`serde_yaml`).
- Schema validation (every `Config::validate`-style check: required fields present, value ranges respected, mutually exclusive options not co-set).
- Token-route resolution: every repository's owner has either an explicit `owner_tokens` entry OR a fallback `token_env`/`token` resolves.
- Workspace-collision check: no two repos resolve to the same `local_path`.
- Audit-slug validation: every key under `audits.defaults` and `repositories[].audits` matches a registered audit type.
- Path-collision check: the four `paths.*` directories (state, cache, logs, runtime) resolve to distinct absolute paths.
- Secret-source check: for each secret-bearing field (`github.token`, `reviewer.api_key`, `chatops.slack.bot_token`, etc.), if `*_env` is referenced, the env var SHOULD be set — but this is a WARN-level finding, not a hard failure (env vars may be set at systemd-unit-start time via `EnvironmentFile=` and not present in the CLI invocation environment).

**Exit codes:**

- `0` — config parses AND every validation passes (zero hard errors, zero soft warnings).
- `1` — config parses AND every validation passes BUT at least one WARN-level finding (typically: a secret-bearing `*_env` field references an unset env var). Operator may want to inspect.
- `2` — config has at least one hard error (parse failure, schema violation, token route unresolvable, workspace collision, audit slug typo, path collision).

**Output format:**

- Stdout: one line per finding, prefixed `ERROR:` / `WARN:` / `OK:`. Findings are line-grouped by check (parse → schema → token routes → ...).
- Stderr: empty on success. On failure, a one-line summary: `check-config: <N> error(s), <M> warning(s) in <path>`.
- A `--json` flag swaps the output for machine-readable findings (one JSON object per finding), useful for CI pipelines AND for `update.sh`'s preflight in `a04`.

**The subcommand is testable.** Validation logic SHALL live in a `validate_config(config: &Config) -> ValidationReport` free function callable from both `check-config` AND `autocoder run`'s startup. The function takes a parsed `Config` and returns a structured `ValidationReport { errors: Vec<Finding>, warnings: Vec<Finding> }`. Tests exercise the report against fixtures (valid config, schema-violating config, collision config, etc.) without spawning processes.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one ADDED requirement: `check-config subcommand validates a config file without side effects`. Covers the validation surface, the exit-code matrix, the `--json` flag, and the shared `validate_config` function.
  - `project-documentation` — one ADDED requirement: `CLI.md documents the check-config subcommand`.
- **Affected code:**
  - `autocoder/src/cli/check_config.rs` (new) — module implementing the subcommand. Loads the config, calls `validate_config`, renders findings to stdout, sets exit code.
  - `autocoder/src/config.rs` (or a new `autocoder/src/config/validate.rs`) — extracts the existing startup-time validation checks into the shared `validate_config(config: &Config) -> ValidationReport` function. The `autocoder run` startup path is refactored to call this same function; today's behavior is preserved (any hard error fails startup; warnings fire but do not block).
  - `autocoder/src/cli/mod.rs` (or equivalent clap subcommand dispatch site) — register `check-config` alongside `run`, `reload`, `rewind`, `audit run`, `install`.
  - Output structures:
    ```rust
    pub struct ValidationReport {
        pub errors: Vec<Finding>,
        pub warnings: Vec<Finding>,
    }
    pub struct Finding {
        pub category: FindingCategory,  // Parse, Schema, TokenRoute, WorkspaceCollision, AuditSlug, PathCollision, SecretSource
        pub message: String,
        pub config_pointer: Option<String>,  // a JSON-Pointer-style locator into the YAML, e.g. "repositories/0/url"
    }
    ```
  - `docs/CLI.md` — new `## \`check-config\`` section documenting the verb, the flag, exit codes, and the `--json` output shape.
- **Operator-visible behavior:**
  - `autocoder check-config --config /etc/autocoder/config.yaml` exits 0 with `OK:` lines listing every passing check on a valid config.
  - On a schema violation (e.g. negative `poll_interval_sec`), exits 2 with `ERROR: schema: repositories[0].poll_interval_sec must be > 0`.
  - On a token-route gap, exits 2 with `ERROR: token-route: repositories[1].url (owner 'my-org-b') has no matching owner_tokens entry AND github.token_env (GITHUB_TOKEN) is unset`.
  - With `--json`, the same findings emit as one JSON object per line on stdout.
- **Breaking:** no. The new subcommand is additive. The internal refactor that pulls validation into `validate_config` preserves the existing `autocoder run` startup behavior — same checks, same hard-error vs. warning semantics.
- **Acceptance:** `cargo test` passes; `openspec validate a03-config-validation-subcommand --strict` passes. Unit tests cover each finding category (parse, schema, token, workspace, audit, path, secret) against in-memory fixtures; one integration test runs `autocoder check-config --config <fixture>` end-to-end against a tempdir.
