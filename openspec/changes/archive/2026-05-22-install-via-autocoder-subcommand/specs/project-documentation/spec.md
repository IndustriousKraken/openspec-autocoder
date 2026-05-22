## ADDED Requirements

### Requirement: Install script is a thin bootstrap for `autocoder install`
The repository SHALL ship `install.sh` at the repo root as a minimal bootstrap (target ≤ 80 lines including comments) whose sole responsibilities are: detect OS + architecture, resolve a binary version (default latest production tag from the GitHub Releases API; overridable via `--version` flag or `AUTOCODER_VERSION` env var), download the binary and its SHA-256 checksum, verify the checksum, place the binary on PATH, and `exec autocoder install "$@"`. All wizard logic, system-user creation, config generation, systemd unit rendering, and optional Claude-CLI bootstrap SHALL live in the `autocoder install` subcommand (a tested Rust subcommand), NOT in bash.

This split exists because the project's automation model relies on autocoder being able to verify its own behavior via `cargo test`. Bash code cannot meaningfully be exercised inside autocoder's sandbox (no sudo, no useradd, no systemctl). Keeping `install.sh` small enough to read in one sitting AND moving the real logic into Rust where it can be unit-tested is the only way to maintain the install path without depending on manual smoke-testing.

README SHALL recommend the install script as the default onboarding path. The existing source-build instructions SHALL be preserved under a "Manual install from source" heading for contributors and operators who specifically want to avoid downloaded binaries.

#### Scenario: First-time install via the curl one-liner
- **WHEN** a new operator runs
  `curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/main/install.sh | bash`
- **THEN** `install.sh` detects OS + architecture, queries the
  GitHub Releases API for the latest production tag, downloads
  the matching binary asset + its `.sha256` file, verifies the
  checksum, places the binary at `/usr/local/bin/autocoder` (with
  sudo if needed) OR `~/.local/bin/autocoder` (no sudo path),
  AND execs `autocoder install`
- **AND** `autocoder install` handles every subsequent prompt
  via its own Rust-tested wizard flow

#### Scenario: install.sh is bounded in size and complexity
- **WHEN** a reviewer inspects `install.sh`
- **THEN** the entire file is ≤ 80 lines including comments
  AND contains no operator prompts, no useradd, no systemctl,
  no apt-get, no claude-installer invocation — those concerns
  live in `autocoder install`
- **AND** every step in install.sh is verifiable by visual
  inspection (the file is small enough to read in one minute)

#### Scenario: Reinstall / upgrade
- **WHEN** an operator re-runs `install.sh` against an existing
  install
- **THEN** the script downloads the latest binary (or the
  version named via `--version` / `AUTOCODER_VERSION`), verifies
  its checksum, and replaces the existing binary at the install
  path
- **AND** the subsequent `exec autocoder install` detects the
  existing config, prints a status block, and exits 0 without
  re-prompting

#### Scenario: README positions the install script as the default
- **WHEN** a new visitor reads README from the top
- **THEN** the first major section after the project description
  is "Quick install" featuring the curl one-liner prominently
- **AND** a one-paragraph explanation of the bootstrap →
  `autocoder install` handoff makes clear that the heavy lifting
  is tested Rust code, not unverified bash
- **AND** the source-build content appears LATER under a
  clearly-labeled "Manual install from source" heading
