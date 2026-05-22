## Why

The first attempt (`install-script-and-wizard`, unmerged) put every install-time concern — OS detection, package install, version selection, config wizard, useradd/systemd, claude CLI bootstrap, secrets capture — into a single ~500-line bash script. That choice surfaced a fundamental tension: bash is hard to unit-test, autocoder's sandbox can't exec `sudo`/`useradd`/`systemctl`, and "verify on a real host" doesn't compose with the project's automation model. Tasks 13.1 and 15.2–15.4 of the original spec asked the implementing agent to do things its sandbox couldn't do; autocoder correctly refused those tasks but the result is a 500-line piece of code with only `bash -n` syntax-checking behind it.

The correct shape pushes complexity OUT of bash and INTO the Rust binary, where the existing test infrastructure already works. The install script becomes a thin bootstrap whose entire job is "download the binary, verify the checksum, hand off to a Rust subcommand." The wizard, the OS interactions (`useradd`, `apt`, `systemctl`), the config-file generation, and the optional claude installer all move into a new `autocoder install` subcommand that ships standard Rust unit tests, mockito-based HTTP tests, and `SystemActions`-trait-based subprocess mocking.

This architecture has three direct benefits for this project:

1. **autocoder can maintain its own install path.** Adding a new chatops backend, a new audit, or a new config field stays a regular Rust change with `cargo test` coverage — no manual smoke-test on a fresh VM required to validate the wizard still works.
2. **The trust boundary shrinks.** Today the operator has to trust ~500 lines of bash that nobody can verify mechanically. After: the operator trusts ~50 lines of bash bootstrap (small enough to read in full) plus a Rust subcommand whose tests run in CI. The shell-outs to `useradd` / `systemctl` are isolated behind a single trait whose production impl is small and obvious.
3. **The version question gets a sensible home.** Today's bash wizard asks the operator to pick a version at install time, then re-runs the wizard on upgrade. After: `install.sh` picks the version (default latest production tag; override via flag/env var) at bootstrap, and `autocoder install` runs the wizard against the binary that was just installed. Upgrading means re-running `install.sh` to swap the binary; the wizard doesn't need to re-prompt for choices the operator already made.

## What Changes

**Delete the prior bash-heavy work** (assumed un-merged at the time this change implements):

- Delete the prior `install.sh` from the repo root (the implementation produced by `install-script-and-wizard`).
- Revert any README changes that the prior change made to the "Quick install" and "Manual install from source" sections — this change re-adds them with the new architecture.

**New: minimal bootstrap script** (`install.sh` at repo root):

- Pure bash, target ≤ 80 lines including comments and the help banner. Strict mode (`set -euo pipefail`), trap on ERR.
- Steps in order:
  1. Detect OS + architecture; map to Rust target triple. Unsupported combos exit with a clear error pointing at source-build instructions.
  2. Resolve the version to install. Default: latest production tag (`^v[0-9]+\.[0-9]+\.[0-9]+$`) from the GitHub Releases API. Override via `--version vX.Y.Z` CLI flag OR `AUTOCODER_VERSION=vX.Y.Z` environment variable so an operator pinning a pre-release version doesn't need to interact with a TUI.
  3. Download binary and `.sha256` to a `mktemp -d` workspace. Refuse to proceed on a non-2xx response from either URL.
  4. Verify with `sha256sum -c` (or `shasum -a 256 -c` on macOS). On mismatch: print computed + expected digests, preserve the temp dir for forensics, exit non-zero.
  5. Place the binary at `/usr/local/bin/autocoder` (with sudo) OR `~/.local/bin/autocoder` (no sudo path; default if `sudo` is unavailable or the script is run with `--user`).
  6. `exec autocoder install "$@"` — every argument after a `--` separator passes through. The bash script is done at this point.
- Tests: a small bats-style or `bash -n` + `shellcheck` gate is enough; the file is small enough to read in full. shellcheck is added as a cargo-build prerequisite check (a `cargo xtask check-install-script` thin wrapper that runs `shellcheck install.sh` if shellcheck is on PATH and skips with a WARN if not — the autocoder sandbox doesn't have shellcheck, so the check is best-effort; CI installs shellcheck and enforces it strictly).

**New: `autocoder install` Rust subcommand:**

- Added under `autocoder/src/cli/install.rs`. Wired into the existing clap subcommand enum in `main.rs` (`run`, `rewind`, `reload`, `install`).
- CLI flags:
  - `--mode <server|dev>` — install mode. Default: `server` on Linux with systemd available, `dev` otherwise.
  - `--config-dir <path>` — override default (`/etc/autocoder/` for server, `~/.config/autocoder/` for dev).
  - `--non-interactive` — accept all defaults and read remaining values from `--repo-url`, `--token-env-var`, etc.; intended for IaC / Ansible callers.
  - `--upgrade` — explicit signal to skip the wizard even when config exists (default behavior already does this).
- Architecture inside the subcommand:
  - **`SystemActions` trait** with methods covering every OS-mutating call: `create_user(name, home_dir, shell)`, `chown(path, owner, group)`, `chmod(path, mode)`, `enable_systemd_unit(name)`, `start_systemd_unit(name)`, `daemon_reload()`, `apt_install(packages: &[&str])`, `which(command)`, `run_subprocess(cmd, args)` (for the claude installer and similar). Production impl shells out via `std::process::Command`; test impl is a `RecordingActions` that captures method calls + arguments into a `Vec<RecordedCall>` for assertions.
  - **HTTP**: existing reqwest path; mockito for tests of the GitHub Releases API call (already used by the `dependency_update` audit's tests before its removal — same pattern).
  - **Stdin/stdout for prompts**: read from `Box<dyn BufRead>` and write to `Box<dyn Write>` (boxed for trait-object testability). Production impl uses `io::stdin().lock()` + `io::stdout()`. Test impl uses `Cursor<Vec<u8>>` so a test scripts the wizard answers and asserts the generated config.
  - **Config assembly**: deserialize `config.example.yaml` (bundled into the binary via `include_str!`), edit the resulting `Config` struct using the wizard's answers, serialize back to YAML. No sed; serde does the round-trip. The bundled example is the SOURCE OF TRUTH for what fields exist — the existing `example-config-covers-every-field` requirement guarantees the example carries every documented field.
- What the subcommand does, in order:
  1. Inspect existing state. If `<config-dir>/config.yaml` already exists and `--upgrade` isn't set explicitly, print a status block ("autocoder is already configured at <path>; the upgrade path is to re-run install.sh which has now swapped the binary") and exit 0. The binary swap done by install.sh already happened before this subcommand ran — there's nothing else to do.
  2. Determine mode (CLI flag → systemd-detection → default).
  3. (Server mode only) Verify root or sudo; bail with a clear error if neither.
  4. (Server mode only) Create the `autocoder` system user via `SystemActions::create_user`. Idempotent — exit 0 with a log line if the user already exists.
  5. Optional: install system packages (`apt_install` on Debian-detected hosts; print + skip elsewhere). Prompted unless `--non-interactive`, default yes.
  6. Optional: install Claude CLI via `run_subprocess(curl, [...])`. Prompted unless `--non-interactive`; the canonical Claude install URL lives in a single const for ease of update. Default yes if `claude` not on PATH; default skip if it's already there.
  7. Wizard prompts (skipped if `--non-interactive` with all required flags provided):
     - Repository URL
     - Base branch (default `main`)
     - Agent branch (default `agent-q`)
     - Poll interval seconds (default `300`)
     - GitHub PAT (silent read; written to secrets.env not config.yaml)
     - Chatops backend (default none) + token + channel id
     - Reviewer (default none) + API key
  8. Assemble `config.yaml` from the bundled example, edited per wizard answers, written to `<config-dir>/config.yaml` with chmod 640 + appropriate ownership.
  9. Write `<config-dir>/secrets.env` with chmod 600 + appropriate ownership, containing the env-var lines that the env-var routes in `config.yaml` reference.
  10. (Server mode only) Render the systemd unit (bundled into the binary via `include_str!` against a template like `prompts/` does for the implementer prompt), write to `/etc/systemd/system/autocoder.service`, `SystemActions::daemon_reload()`, `SystemActions::enable_systemd_unit("autocoder")`, prompt to start now (default yes), `SystemActions::start_systemd_unit("autocoder")`.
  11. Print a post-install summary: paths, the live-logs command (server) or run command (dev), the "claude auth login" reminder if Claude was installed but not authenticated, a reminder that more repos go in config.yaml + `autocoder reload`.
- Tests in `autocoder/src/cli/install.rs::tests`:
  - `wizard_collects_minimum_essential_fields` — scripted stdin, asserts the generated config.yaml has the expected repo URL / branches / token_env / chatops choice.
  - `existing_config_skips_wizard` — pre-place a config.yaml; assert the subcommand prints "already configured" and exits 0 without prompting.
  - `server_mode_calls_useradd_and_systemctl` — `RecordingActions` asserts the exact `create_user` + `enable_systemd_unit` + `start_systemd_unit` sequence with the expected arguments.
  - `dev_mode_does_not_call_useradd` — `RecordingActions` asserts zero `create_user` calls.
  - `non_interactive_with_required_flags_succeeds` — runs the subcommand with `--non-interactive --repo-url ... --token-env-var ...` and asserts the wizard prompts are skipped.
  - `non_interactive_missing_required_flag_errors` — runs without `--repo-url` and asserts a clear error message.
  - `chatops_choice_writes_secrets_env_entry` — picks `slack`, scripts the token + channel id, asserts `secrets.env` contains `SLACK_BOT_TOKEN=<value>` and config.yaml references `bot_token_env: SLACK_BOT_TOKEN`.
  - `checksum_mismatch_path_is_in_install_sh_not_here` — N/A, but document in the test module that binary download + verification is install.sh's job, not the subcommand's.

**README:**

- Re-add the "Quick install" section at the top with the curl one-liner. Note that the heavy lifting is `autocoder install`, a tested Rust subcommand, and the bash script is intentionally minimal.
- The "Manual install from source" section preserves the source-build flow.

**ADDED requirements:**

- Under `orchestrator-cli`: a new "Install subcommand" requirement covering the wizard, the system-actions abstraction, and the `--non-interactive` flag.
- Under `project-documentation`: the previous "Install script is the recommended deployment path" requirement is RESHAPED — the script exists at the repo root, but the wizard work it triggers is part of the binary itself.

## Impact

- Affected specs:
  - `orchestrator-cli` — one ADDED requirement establishing the `autocoder install` subcommand contract.
  - `project-documentation` — one MODIFIED scenario in the README-recommendation requirement (if the prior change was already applied; if not, this change establishes the requirement fresh).
- Affected code:
  - `install.sh` — rewritten as a small bootstrap (~50–80 lines).
  - `autocoder/src/cli/install.rs` — NEW module, the subcommand body + SystemActions trait + tests.
  - `autocoder/src/cli/mod.rs` (if exists) or `autocoder/src/main.rs` — wire the new `install` variant into the clap subcommand enum.
  - `autocoder/Cargo.toml` — no new deps expected (reqwest, serde, clap, mockito already in tree).
  - README — Quick install section.
- Operator-visible behavior: identical to what `install-script-and-wizard` proposed, but now backed by tested Rust code. The one-liner stays the same: `curl -fsSL <url>/install.sh | bash`.
- Breaking: no.
- Acceptance: `cargo test` passes (including the new install-subcommand tests). `shellcheck install.sh` clean in CI (autocoder's sandbox is exempt). `openspec validate install-via-autocoder-subcommand --strict` passes.
