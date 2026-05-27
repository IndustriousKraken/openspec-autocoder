## 1. Validation data types

- [ ] 1.1 In `autocoder/src/config.rs` (or a new sibling module `autocoder/src/config/validate.rs`), define:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct ValidationReport {
      pub errors: Vec<Finding>,
      pub warnings: Vec<Finding>,
  }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Finding {
      pub category: FindingCategory,
      pub message: String,
      pub config_pointer: Option<String>,
  }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum FindingCategory {
      Parse,
      Schema,
      TokenRoute,
      WorkspaceCollision,
      AuditSlug,
      PathCollision,
      SecretSource,
  }
  impl ValidationReport {
      pub fn is_ok(&self) -> bool { self.errors.is_empty() && self.warnings.is_empty() }
      pub fn has_errors(&self) -> bool { !self.errors.is_empty() }
  }
  ```

## 2. Shared `validate_config` function

- [ ] 2.1 Extract the existing startup-time validation checks into a shared `pub fn validate_config(config: &Config) -> ValidationReport` function. The function takes an already-parsed `Config` AND returns the structured report; it does NOT parse YAML itself (the caller does that and reports parse errors via the `FindingCategory::Parse` variant).
- [ ] 2.2 Checks to extract:
  - **Schema**: required fields, value-range invariants (positive `poll_interval_sec`, `max_revisions_per_pr` in range, etc.). Each violation pushes a `FindingCategory::Schema` Finding with a `config_pointer` naming the offending field.
  - **TokenRoute**: for each `repositories[].url`, derive the owner (case-insensitive). If `github.owner_tokens` contains a matching key, that's the route. Else fall back to `github.token` (inline) or `github.token_env`. If no route resolves, push a `FindingCategory::TokenRoute` ERROR.
  - **WorkspaceCollision**: derive each repo's `local_path` (existing logic). If two repos resolve to the same path, push two FindingCategory::WorkspaceCollision ERRORs naming both indices.
  - **AuditSlug**: every key under `audits.defaults` AND every key under `repositories[].audits` must match a registered audit type. Unknown slugs push `FindingCategory::AuditSlug` ERRORs.
  - **PathCollision**: the four `paths.*` directories (after env-var / XDG resolution) must be distinct absolute paths. Two same → two `FindingCategory::PathCollision` ERRORs.
  - **SecretSource** (WARN only): for each `*_env`-referenced field, `std::env::var(name)` returns `Err` AND no inline alternative is set. Push a WARN; don't block.
- [ ] 2.3 Refactor `autocoder/src/main.rs` (or wherever `run` startup lives) to call `validate_config` AND react to its report: any ERRORs → exit non-zero with the existing startup-error UX; WARNs → log at WARN. Existing tests covering startup behavior MUST continue to pass.
- [ ] 2.4 Unit tests in `config/validate.rs`:
  - Valid config → empty report.
  - Schema violation (e.g. negative `poll_interval_sec`) → one ERROR finding with correct `config_pointer`.
  - Token-route gap → one ERROR finding naming the missing owner.
  - Workspace collision → two ERROR findings, one per colliding repo index.
  - Audit slug typo → one ERROR finding naming the slug.
  - Path collision (state == cache, etc.) → two ERROR findings.
  - Missing env var on `*_env` field → one WARN finding.

## 3. `check-config` subcommand

- [ ] 3.1 New module `autocoder/src/cli/check_config.rs`. Surface:
  ```rust
  #[derive(Args, Debug, Clone)]
  pub struct CheckConfigArgs {
      #[arg(long)]
      pub config: PathBuf,
      #[arg(long, default_value_t = false)]
      pub json: bool,
  }
  pub async fn execute(args: CheckConfigArgs) -> Result<()> { ... }
  ```
- [ ] 3.2 `execute` body:
  - Read `args.config` to a String (`tokio::fs::read_to_string`). On read error, emit a single `FindingCategory::Parse` ERROR (could-not-read) and exit 2.
  - Parse as YAML into `Config`. On parse error, emit one `FindingCategory::Parse` ERROR with the serde_yaml error message AND any line/column info AND exit 2.
  - Call `validate_config(&config)` → `ValidationReport`.
  - Render the report per the output format (below).
  - Exit code: 2 if `report.errors.is_empty()` is false; 1 if `report.warnings.is_empty()` is false (and no errors); 0 otherwise.
- [ ] 3.3 Default (non-JSON) output format:
  - For each passing check category (no findings of that category), print `OK: <category> — <one-line summary>` (e.g. `OK: schema — all required fields present and value ranges respected`).
  - For each finding, print `ERROR: <category>: <message>` or `WARN: <category>: <message>` with the optional `config_pointer` appended in parentheses.
  - On any failure, also print to stderr: `check-config: <N> error(s), <M> warning(s) in <path>`.
- [ ] 3.4 `--json` output format:
  - One JSON object per line on stdout, each: `{"level": "error"|"warn"|"ok", "category": "<slug>", "message": "<text>", "config_pointer": "..."}`.
  - At end: `{"level": "summary", "errors": N, "warnings": M, "config": "<path>"}`.

## 4. Subcommand registration

- [ ] 4.1 In the clap subcommand dispatch (`autocoder/src/cli/mod.rs` or `main.rs`), register `check-config` alongside `run`, `reload`, `rewind`, `audit run`, `install`.
- [ ] 4.2 The `check-config` verb has no other clap interaction (no global config-default file resolution; `--config <path>` is required).

## 5. Tests

- [ ] 5.1 Unit tests in `cli/check_config.rs`:
  - Reading a missing file → exits 2 with the read-error message.
  - Reading a malformed YAML → exits 2 with a Parse ERROR mentioning the parse failure.
  - Reading a valid config → exits 0; stdout contains `OK:` lines for each category.
  - Reading a config with a single schema violation → exits 2; stdout contains the ERROR line; stderr contains the summary.
  - Reading a config that's structurally valid BUT has an unset `*_env` → exits 1; stdout contains the WARN line; stderr summary names 0 errors and 1 warning.
- [ ] 5.2 `--json` output tests:
  - Each fixture above also produces parseable JSON when `--json` is set.
  - The summary object is always the last line of stdout.

## 6. CLI.md update

- [ ] 6.1 In `docs/CLI.md`, add a new section `## \`check-config\`` documenting:
  - The verb and `--config <path>` requirement.
  - The exit-code matrix (0 / 1 / 2).
  - The default output format AND the `--json` flag.
  - The two intended audiences: operators editing YAML by hand AND scripts (the cron-updater landing in `a04` calls this as a preflight).

## 7. Spec deltas

- [ ] 7.1 `openspec/changes/a03-config-validation-subcommand/specs/orchestrator-cli/spec.md` ADDs one requirement: `check-config subcommand validates a config file without side effects`. Names the validation surface, the exit-code matrix, the JSON output shape, and the shared `validate_config` function.
- [ ] 7.2 `openspec/changes/a03-config-validation-subcommand/specs/project-documentation/spec.md` ADDs one requirement: `CLI.md documents the check-config subcommand`.

## 8. Verification

- [ ] 8.1 `cargo test` passes (new + existing). The refactor of `autocoder run` startup into `validate_config` MUST NOT break any existing startup test.
- [ ] 8.2 `openspec validate a03-config-validation-subcommand --strict` passes.
- [ ] 8.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
