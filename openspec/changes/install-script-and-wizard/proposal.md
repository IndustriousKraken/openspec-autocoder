## Why

The current onboarding story is "clone the repo, install Rust, run `cargo build --release`, copy the binary somewhere, write a config from scratch using `config.example.yaml`, figure out systemd yourself". Most candidates for using this software don't want to spend an hour setting that up to evaluate it. With the companion `release-pipeline-github-actions` change supplying pre-built binaries, the path of least resistance is a single shell command that:

- detects the user's OS + architecture
- offers to install system dependencies (Debian-based apt; other distros print a "you'll need: X Y Z" list)
- offers to install the `claude` CLI (the default executor backend) via its official installer
- presents a numbered menu of production-version tags (filtered from the GitHub Releases list), with manual entry available for pre-release tags
- downloads the appropriate binary + checksum, verifies SHA-256
- runs a config wizard that asks for one repo URL + GitHub PAT + (optionally) chatops backend, and stitches the rest of `config.example.yaml`'s defaults around the operator's answers
- installs to the right paths depending on install mode (server vs local dev)
- generates and enables a systemd unit when the mode is server

The script is a strict superset of "manually download and copy" — operators who want full control can still do that, but the install script is the recommended path. README documents both.

## What Changes

- **NEW**: `install.sh` at the repo root. Pure bash; no `whiptail` / `dialog` dependencies; pure `read -p` and `select` for prompts. Self-contained so `curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/main/install.sh | bash` works end-to-end. Strict mode (`set -euo pipefail`) at the top; trap errors with a clear "install failed at <step>" message + a pointer to logs.
- **Install-mode choice up front**:
  - `[1] Server` (Linux only — checked via `uname -s`): creates a dedicated `autocoder` system user via `useradd --system --shell /usr/sbin/nologin --home-dir /var/lib/autocoder --create-home autocoder`, binary at `/usr/local/bin/autocoder`, config at `/etc/autocoder/config.yaml` (chmod 640, owned by `autocoder:autocoder`), secrets at `/etc/autocoder/secrets.env` (chmod 600, owned by `autocoder:autocoder`), workspace state directories chowned to the autocoder user. systemd unit installed and enabled.
  - `[2] Local dev`: no new user, binary at `/usr/local/bin/autocoder` (with sudo) OR `~/.local/bin/autocoder` (no sudo — asked separately), config at `~/.config/autocoder/config.yaml` (chmod 600), no systemd unit. Operator runs `autocoder run` manually in a terminal or tmux/screen.
  - macOS gets dev mode only (server-mode launchd support is out of scope for this change; revisit if anyone asks). The script detects darwin and skips the mode prompt.
- **System dependency install** (prompted, default `yes`):
  - On Debian/Ubuntu (`/etc/debian_version` exists): `apt-get update && apt-get install -y git curl ca-certificates` plus any other essentials. Requires sudo; the script prompts before sudo'ing.
  - On other Linux: print a list and tell the operator to install equivalents via their package manager; continue.
  - On macOS: check for Homebrew; if present offer `brew install git`; if not, print "install Homebrew first" and continue.
- **Claude CLI install** (prompted, default `yes` if not already on PATH): runs the official Claude installer (`curl -fsSL https://claude.ai/install | sh` or whatever the current canonical command is; the script reads the canonical command from a single named constant near the top so future Anthropic changes update one place). The script prints the upstream URL before piping it so the operator knows what they're running and can opt out. The `claude auth login` step is left to the operator (it requires browser interaction); the script prints a clear "now run `claude auth login` to authenticate" message after install.
- **Version selection wizard**: queries `GET https://api.github.com/repos/<owner>/<repo>/releases` (unauthenticated — public repos accept up to 60 requests/hour from a single IP, plenty for an install run). Filters to production tags (`^v\d+\.\d+\.\d+$` regex), sorts by published-at desc, presents the top 5 as a numbered menu. Default selection is `[1]` (the latest production tag). Additional options: `[m] Enter a tag manually` (lets operators install pre-release versions like `v1.2.3-rc1`), `[s] Skip — I'll install the binary myself`.
- **Binary download + verification**: constructs the asset URL from the chosen tag + detected target triple (`uname -m` + `uname -s` → `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu` / `aarch64-apple-darwin`). Downloads both the binary and its `.sha256` file. Verifies via `sha256sum -c` (or `shasum -a 256 -c` on macOS). Refuses to install on checksum mismatch. Moves the verified binary to the install path with `install -m 755`.
- **Config wizard**: asks the minimum essential questions, stitches the answers into a working `config.yaml` by starting from `config.example.yaml` (downloaded fresh from the release tag — operators always get the version-matched defaults) and uncommenting / editing the relevant lines. Questions:
  - **Repository URL** (required, single repo at first install; the wizard prints "to add more, edit `config.yaml` after install and `autocoder reload`")
  - **Base branch** (default `main`, with an explanatory line)
  - **Agent branch** (default `agent-q`)
  - **Poll interval seconds** (default `300`)
  - **GitHub PAT**: stored in `secrets.env` as `GITHUB_TOKEN=<value>` (chmod 600). The wizard pastes the value via `read -s` (silent input). The resulting `config.yaml` references `token_env: GITHUB_TOKEN` so the existing env-var path applies.
  - **ChatOps backend**: `[1] none (default)` / `[2] slack` / `[3] discord` / `[4] teams` / `[5] mattermost` / `[6] matrix`. If non-`none`, prompts for the backend's token (silent input) and the default channel ID, writes to `secrets.env` + `config.yaml`.
  - **Reviewer**: `[1] none (default)` / `[2] Anthropic (claude-sonnet-4-6)` / `[3] OpenAI-compatible`. If non-`none`, prompts for the API key.
- **systemd unit generation (server mode only)**: writes `/etc/systemd/system/autocoder.service` with `User=autocoder`, `Group=autocoder`, `EnvironmentFile=/etc/autocoder/secrets.env`, `ExecStart=/usr/local/bin/autocoder run --config /etc/autocoder/config.yaml`, `Restart=on-failure`, `RestartSec=10`, `WorkingDirectory=/var/lib/autocoder`. Runs `systemctl daemon-reload`, `systemctl enable --now autocoder`. Prints `journalctl -u autocoder -f` as the follow-the-logs command.
- **Post-install summary**: prints the install paths, the systemd-status command (server mode) or the run command (dev mode), a checklist of next steps (claude auth login, edit config.yaml to add repos, etc.), and a one-line link to README.
- **Idempotency / upgrade path**: if `/etc/autocoder/config.yaml` (server) or `~/.config/autocoder/config.yaml` (dev) already exists, the script skips the wizard, asks "Upgrade existing install? [Y/n]", and on yes just swaps the binary (verifying checksum), preserving config and secrets. The systemd unit is left alone unless its content has changed (diff check; if changed, asks before overwriting).
- **README documentation**: a new "Quick install" section at the top of README pointing at the curl one-liner, with the existing "Quick Start" demoted to "Manual install from source" further down. The new section explicitly addresses both server and dev modes.
- **ADDED requirement** under `project-documentation`: "Install script is the recommended deployment path" — pins the contract that `install.sh` exists at the repo root, the README recommends it as the default, and the script supports both server and dev modes with the server-mode-specific user-creation step.

## Impact

- Affected specs: `project-documentation` (one ADDED requirement establishing the install-script contract + README recommendation).
- New repo files: `install.sh` at the repo root (executable). Optionally `install/systemd.service.template` and `install/secrets.env.template` if the script gets long enough that templating helps; first cut is fine inlined into `install.sh`.
- Existing file changes: README gets a new "Quick install" section near the top; the existing source-build content is preserved and demoted under a "Manual install from source" heading.
- Operator-visible behavior: new operators get a one-command install path. Existing operators are unaffected — their source-built binaries keep working.
- Security: the script runs `curl | bash` only after the operator explicitly chooses to (the script itself can be curl|bash'd from a trusted source, GitHub-served HTTPS); the binaries it downloads are SHA-256 verified against the release's published checksum file. Secrets (PAT, chatops tokens, reviewer API key) are written to `secrets.env` with chmod 600 — never logged, never echoed back. `read -s` is used for every credential prompt.
- Breaking: no. New file additions only; no existing code paths change.
- Acceptance: running `bash install.sh` end-to-end on a fresh Debian 12 VM produces a working `autocoder` daemon under systemd. Running it on macOS produces a working binary in `~/.local/bin/`. Running it a second time triggers the upgrade path (binary swap, config preserved). `shellcheck install.sh` passes with no warnings. `openspec validate install-script-and-wizard --strict` passes.
