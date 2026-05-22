## 1. Revert the prior bash-heavy implementation

- [x] 1.1 Delete the existing `install.sh` at the repo root (the file produced by the prior `install-script-and-wizard` change). The new bootstrap is a from-scratch rewrite; preserving the old file would just confuse reviewers.
- [x] 1.2 Revert README changes from the prior change: remove the "Quick install" section it added and restore "Quick Start" to its original heading. (If the prior change was never merged to `main`, this revert may already be implicit — confirm by `git diff main -- README.md` before editing.) The new README section is added in §6.

## 2. Bootstrap `install.sh`

- [x] 2.1 Write a new `install.sh` at the repo root, target length ≤ 80 lines including comments. Strict mode (`set -euo pipefail`), `trap` on `ERR` naming the last labeled step.
- [x] 2.2 Detection helpers: `detect_target_triple()` returning one of `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu` / `aarch64-apple-darwin` based on `uname -s` + `uname -m` (with arm64 → aarch64 mapping). Unsupported combos exit non-zero with a clear "no pre-built binary for <triple>; build from source per README" message.
- [x] 2.3 Version resolution: accept `--version vX.Y.Z` CLI flag OR `AUTOCODER_VERSION` env var. If neither, query `GET https://api.github.com/repos/<owner>/<repo>/releases/latest` (NOT the full releases list — `latest` already excludes pre-releases) and use that tag. The bootstrap NEVER shows a TUI menu; version choice is non-interactive by design (the operator chooses via flag/env-var or accepts the latest production tag).
- [x] 2.4 Download binary + `.sha256` from `https://github.com/<owner>/<repo>/releases/download/<tag>/autocoder-<tag>-<triple>` (and `.sha256` suffix) into a `mktemp -d` workspace. Refuse to proceed on non-2xx from either URL.
- [x] 2.5 Verify: `cd <tmpdir> && sha256sum -c <basename>.sha256` (or `shasum -a 256 -c` on macOS). On mismatch: print computed + expected digests, leave the temp dir intact, exit non-zero. On success: continue.
- [x] 2.6 Determine install path: `--user` flag OR no sudo available → `~/.local/bin/autocoder`. Else (Linux with sudo or root): `/usr/local/bin/autocoder`. Use `install -m 755 <src> <dst>` (sudo-wrapped when target is in `/usr/local/bin`).
- [x] 2.7 Final line: `exec autocoder install "$@"`. Any args after the bootstrap's own flags (e.g. `--mode dev --non-interactive --repo-url ...`) pass through to the Rust subcommand. If `--` is used as a separator on the install.sh command line, everything after it is passed verbatim.

## 3. Rust subcommand scaffolding

- [x] 3.1 Add an `Install` variant to whatever clap subcommand enum lives in `autocoder/src/main.rs` (mirroring `Run`, `Rewind`, `Reload`). Wire it to call a new `crate::cli::install::execute(args)` function.
- [x] 3.2 Create `autocoder/src/cli/install.rs`. Public surface: `pub async fn execute(args: InstallArgs) -> Result<()>`. Internal types: `InstallArgs` (clap-derived), `InstallMode { Server, Dev }`, `WizardAnswers` (the collected operator inputs).
- [x] 3.3 CLI flags on `InstallArgs`: `--mode <server|dev>`, `--config-dir <path>`, `--non-interactive`, `--upgrade` (explicit signal to skip the existing-config-check), plus `--repo-url`, `--base-branch`, `--agent-branch`, `--poll-interval-sec`, `--token-env-var`, `--chatops-backend`, `--chatops-channel-id`, `--reviewer-provider`, `--reviewer-model`. The latter group is consulted only when `--non-interactive` is also set; in interactive mode they pre-fill the wizard prompt defaults.

## 4. `SystemActions` trait + production / test implementations

- [x] 4.1 Define `pub trait SystemActions: Send + Sync` in `autocoder/src/cli/install.rs` (or a sibling module if the file gets long) with async methods covering every OS-mutating call the subcommand makes:
  - `async fn which(&self, command: &str) -> Option<PathBuf>`
  - `async fn run_subprocess(&self, cmd: &str, args: &[&str]) -> Result<SubprocessOutcome>` (used by claude installer + similar)
  - `async fn create_user(&self, name: &str, home_dir: &Path, shell: &str) -> Result<()>` (idempotent; Ok when the user already exists)
  - `async fn chown(&self, path: &Path, owner: &str, group: &str) -> Result<()>`
  - `async fn chmod(&self, path: &Path, mode: u32) -> Result<()>`
  - `async fn apt_install(&self, packages: &[&str]) -> Result<()>` (Ok-skipped on non-Debian hosts; production impl checks `/etc/debian_version` first)
  - `async fn daemon_reload(&self) -> Result<()>`
  - `async fn enable_systemd_unit(&self, name: &str) -> Result<()>`
  - `async fn start_systemd_unit(&self, name: &str) -> Result<()>`
- [x] 4.2 Production implementation `RealSystemActions` that shells out via `tokio::process::Command`. Each method has a 5–10 line impl; the trait's job is making the orchestration testable, not abstracting the underlying tools.
- [x] 4.3 Test implementation `RecordingActions` that captures method calls into `Mutex<Vec<RecordedCall>>` for assertions. Each test can pre-program return values for specific calls (e.g. `which("claude")` returns `Some(/usr/local/bin/claude)` for the "claude already installed" test).

## 5. Wizard implementation

- [x] 5.1 Define `WizardIo` trait with `read_line() -> Result<String>`, `read_password() -> Result<String>`, `print(s: &str)`, `confirm(prompt: &str, default: bool) -> Result<bool>`, `choose(prompt: &str, options: &[&str], default_idx: usize) -> Result<usize>`. Production impl uses `rpassword` (or `std::io::stdin().read_line` with terminal echo disabled via the existing `termion`/`crossterm` dep, or by adding `rpassword` as a small focused dep) for `read_password`.
- [x] 5.2 Test impl `ScriptedIo` reads from a pre-loaded `VecDeque<String>` of answers and writes prompts to a `Vec<u8>` buffer for inspection. Tests script answers in order and assert the prompts that were emitted.
- [x] 5.3 Wizard flow function `async fn run_wizard(io: &mut dyn WizardIo, mode: InstallMode, prefilled: &WizardPrefill) -> Result<WizardAnswers>`. Steps:
  - Repo URL (required; no default)
  - Base branch (default `main`)
  - Agent branch (default `agent-q`)
  - Poll interval seconds (default `300`)
  - GitHub PAT (silent input via `read_password`)
  - Chatops backend menu (`[1] none [2] slack ...`); on non-none, prompt for backend token + channel id
  - Reviewer menu (`[1] none [2] anthropic [3] openai_compatible`); on non-none, prompt for API key
- [x] 5.4 `WizardPrefill` struct collects the `--repo-url` / `--token-env-var` / `--chatops-*` flag values. In interactive mode, prefill values become the prompt defaults. In `--non-interactive` mode, missing prefill values cause an immediate error naming the missing flag.

## 6. Config + secrets file generation

- [x] 6.1 Bundle `config.example.yaml` into the binary via `include_str!("../../../config.example.yaml")` (path relative to `autocoder/src/cli/install.rs`). The bundled example is the source of truth for what fields exist; the wizard's job is to edit a deserialized copy of it.
- [x] 6.2 `assemble_config(answers: &WizardAnswers) -> Config` — deserializes the bundled example into a `Config` struct (the existing one from `autocoder/src/config.rs`), then mutates it: `cfg.repositories[0].url = answers.repo_url`, etc. Unused commented blocks in the example (e.g. `reviewer:` when the operator chose none) deserialize as `None` already; no action needed. Used blocks get populated from the answers.
- [x] 6.3 `serialize_config(cfg: &Config) -> String` — `serde_yaml::to_string`. Write to `<config-dir>/config.yaml` with chmod 640 (or 600 for dev mode).
- [x] 6.4 `assemble_secrets_env(answers: &WizardAnswers) -> String` — assembles lines like `GITHUB_TOKEN=<value>\nSLACK_BOT_TOKEN=<value>\n` etc. Write to `<config-dir>/secrets.env` with chmod 600 always (it carries the actual tokens, regardless of mode).
- [x] 6.5 (Server mode only) `SystemActions::chown` on both files to `autocoder:autocoder`.

## 7. systemd unit generation (server mode only)

- [x] 7.1 Bundle a systemd unit template via `include_str!`. Template content same as the prior install-script-and-wizard spec proposed (User=autocoder, EnvironmentFile=..., ExecStart=..., NoNewPrivileges, ProtectSystem=strict, ReadWritePaths=..., etc.). No template substitutions needed — the paths are fixed by the spec (binary at `/usr/local/bin/autocoder`, config at `/etc/autocoder/config.yaml`).
- [x] 7.2 Write the rendered unit to `/etc/systemd/system/autocoder.service` (via direct `tokio::fs::write` since the daemon will be running as the operator with sudo at this point — `SystemActions::chmod` covers permissions).
- [x] 7.3 Call `SystemActions::daemon_reload`, `SystemActions::enable_systemd_unit("autocoder")`. Prompt to start now (default yes); on yes, `SystemActions::start_systemd_unit("autocoder")`.

## 8. Idempotency and existing-config handling

- [x] 8.1 At the start of `execute`, check whether `<config-dir>/config.yaml` exists. If yes AND `--upgrade` is not set, print a status block ("autocoder is already configured at <path>; the new binary is installed; no wizard work needed") and return 0. The binary swap has already happened (it's install.sh's job, completed before this subcommand was invoked).
- [x] 8.2 If yes AND `--upgrade` is set: a no-op (binary already swapped). Print a confirmation of the install version and return 0.
- [x] 8.3 If no: run the full wizard.

## 9. Tests for `autocoder install`

- [x] 9.1 `wizard_collects_minimum_essential_fields`: scripted `ScriptedIo` with a fixed set of answers; assert `assemble_config` produces the expected `Config` struct (compare field-by-field).
- [x] 9.2 `existing_config_skips_wizard`: pre-place a `config.yaml` in a tempdir, run `execute` with `--config-dir <tempdir>` and `ScriptedIo` containing NO answers; assert the subcommand returns Ok without consuming any answers.
- [x] 9.3 `server_mode_calls_expected_system_actions_in_order`: scripted IO + `RecordingActions`; assert the recorded calls match an expected sequence including `create_user("autocoder", ...)`, `daemon_reload()`, `enable_systemd_unit("autocoder")`, `start_systemd_unit("autocoder")`.
- [x] 9.4 `dev_mode_does_not_call_useradd_or_systemctl`: `RecordingActions` asserts zero calls to `create_user`, `daemon_reload`, `enable_systemd_unit`, `start_systemd_unit`.
- [x] 9.5 `non_interactive_succeeds_with_all_required_flags`: `--non-interactive --repo-url ... --token-env-var GITHUB_TOKEN --chatops-backend none --reviewer-provider none`; assert no IO reads happen (the `ScriptedIo` is empty and not consumed).
- [x] 9.6 `non_interactive_errors_on_missing_required_flag`: `--non-interactive` without `--repo-url`; assert Err with a message naming `--repo-url`.
- [x] 9.7 `chatops_choice_writes_secrets_and_config_references_env_var`: scripted answers picking `slack`; assert `secrets.env` content contains `SLACK_BOT_TOKEN=<value>` AND `config.yaml` contains `bot_token_env: SLACK_BOT_TOKEN`.
- [x] 9.8 `reviewer_choice_writes_api_key_and_config_picks_provider`: similar to 9.7 for the reviewer.
- [x] 9.9 `assemble_config_round_trips_through_serde`: assert that `serde_yaml::from_str(&serialize_config(&assemble_config(&answers)))` produces a Config equal to the input (no field loss during the YAML round-trip).
- [x] 9.10 `apt_install_skipped_on_non_debian`: `RecordingActions` impl returns `false` from `which("apt-get")`-equivalent; assert zero `apt_install` calls.
- [x] 9.11 `claude_install_skipped_when_already_present`: `RecordingActions.which("claude")` returns `Some(path)`; assert no subprocess call to the claude installer.

## 10. README integration

- [x] 10.1 Add a "## Quick install" section at the top of README (immediately after the project description). Content: the curl one-liner, a paragraph explaining the bootstrap → `autocoder install` handoff, the server-vs-dev distinction, a pointer to "Manual install from source" further down.
- [x] 10.2 Add a one-paragraph "Reinstalling / upgrading" subsection: explains that re-running `install.sh` swaps the binary and `autocoder install` detects existing config and skips the wizard. Operators wanting a different version pass `--version vX.Y.Z` to `install.sh` or set `AUTOCODER_VERSION`.
- [x] 10.3 Demote the existing source-build instructions to "## Manual install from source" with a sentence framing it as the path for contributors / advanced operators.

## 11. Spec deltas

- [x] 11.1 Author the ADDED requirement under `orchestrator-cli`: "Install subcommand" — covers the wizard flow, the SystemActions abstraction, the `--non-interactive` flag contract, and the idempotency semantic. Scenarios for: first-time server install, first-time dev install, existing-config detection, non-interactive mode with all flags, non-interactive mode missing a required flag.
- [x] 11.2 Author the ADDED requirement under `project-documentation`: "Install script bootstraps `autocoder install`" — covers the contract that `install.sh` exists at the repo root, is a thin bootstrap, and hands off to the subcommand. Notes the deletion of the prior `install-script-and-wizard` change's approach (if relevant).

## 12. Verification

- [x] 12.1 `cargo test` passes.
- [x] 12.2 `openspec validate install-via-autocoder-subcommand --strict` passes.
- [x] 12.3 `bash -n install.sh` passes (syntax check; the file is small enough that visual review covers the rest in autocoder's sandbox; CI runs shellcheck strictly).
- [x] 12.4 `cargo run -- install --help` prints a sensible usage block.
