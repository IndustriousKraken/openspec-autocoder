## 1. Script skeleton

- [ ] 1.1 Create `install.sh` at the repo root. First lines: `#!/usr/bin/env bash`, then `set -euo pipefail`, then a `trap` on `ERR` that prints `install failed at step: $LAST_STEP — see <log-file>` and exits non-zero. Maintain a `LAST_STEP` variable updated before each major section so the trap message is actionable.
- [ ] 1.2 Define named constants near the top: `REPO_OWNER`, `REPO_NAME`, `CLAUDE_INSTALL_URL` (single-source for the canonical claude installer command — update one place if Anthropic changes the URL), `RECOMMENDED_TAG_COUNT=5` (how many production tags to show in the menu), `DEFAULT_INSTALL_PREFIX_SERVER="/usr/local/bin"`, `DEFAULT_CONFIG_DIR_SERVER="/etc/autocoder"`, `DEFAULT_STATE_DIR_SERVER="/var/lib/autocoder"`, `DEFAULT_INSTALL_PREFIX_DEV_USER="$HOME/.local/bin"`, `DEFAULT_CONFIG_DIR_DEV="$HOME/.config/autocoder"`.
- [ ] 1.3 Banner function: prints script name + version + repo URL on startup so an operator who pipes the script knows what they're running. Include a "this script will install autocoder + optionally claude CLI + optionally system packages" summary line before any prompts so the consent is informed.
- [ ] 1.4 Logging: tee everything to `/tmp/autocoder-install-<timestamp>.log` so a post-mortem on failure is possible. Print the log path on the trap message.

## 2. Platform + architecture detection

- [ ] 2.1 `detect_os()`: returns `linux` or `darwin` based on `uname -s`. Other values exit with a clear "unsupported OS" error message.
- [ ] 2.2 `detect_arch()`: returns `x86_64` or `aarch64` based on `uname -m`. Maps `arm64` (Apple's reporting) to `aarch64`. Other values exit with a clear "unsupported architecture" error.
- [ ] 2.3 `detect_target_triple()`: combines the above into one of `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`. Any other combination exits with "no pre-built binary for <triple>; build from source per README".
- [ ] 2.4 `detect_debian()`: returns true iff `/etc/debian_version` exists. Used to decide whether to offer the apt-install step.
- [ ] 2.5 `detect_systemd()`: returns true iff `command -v systemctl >/dev/null` AND `/run/systemd/system` exists (a running systemd PID 1, not just systemctl available in a container). Used to decide whether server mode is available.

## 3. Install-mode selection

- [ ] 3.1 On darwin: force dev mode silently (server mode not supported yet). Print a one-line note.
- [ ] 3.2 On Linux: prompt `Install mode: [1] Server (systemd, dedicated user, system paths) [2] Local dev (no systemd, user paths)` with default `[1]` IFF systemd is detected, else default `[2]`.
- [ ] 3.3 If server mode selected: require root or `sudo` availability. The script re-execs itself under `sudo bash install.sh` if not already root, prompting the operator first. If sudo is not available (e.g. rootless container), fall back to dev mode with a printed explanation.
- [ ] 3.4 If dev mode selected with no sudo: install to `~/.local/bin/autocoder` and config to `~/.config/autocoder/config.yaml`. If dev mode with sudo: ask separately whether to install the binary to `/usr/local/bin/` (default yes — picked up by all users) or `~/.local/bin/` (default no — picked up by current user only).

## 4. Optional: apt-based system dependencies

- [ ] 4.1 If Debian-detected: prompt `Install system dependencies (git, curl, ca-certificates) via apt? [Y/n]`. On yes: `sudo apt-get update && sudo apt-get install -y git curl ca-certificates`. On no: skip with a "remember to ensure these are installed" line.
- [ ] 4.2 If non-Debian Linux: print a one-line "you'll need: git, curl, ca-certificates installed via your distribution's package manager" and skip.
- [ ] 4.3 On macOS: check for `git`, `curl`. If both present: skip. If missing: print "install via Homebrew or Xcode Command Line Tools" and continue (the operator can re-run after installing).

## 5. Optional: claude CLI install

- [ ] 5.1 If `command -v claude` returns a path: skip with "claude CLI already present at $(which claude)".
- [ ] 5.2 If not present: prompt `Install Claude CLI now? [Y/n]`. Print the upstream URL ($CLAUDE_INSTALL_URL) before the prompt so the operator sees what's about to be curl-bashed.
- [ ] 5.3 On yes: run the claude installer. On completion print "now run `claude auth login` to authenticate before starting autocoder".
- [ ] 5.4 On no: print "remember to install + auth claude before autocoder can run; see <claude-install-url>".

## 6. Version selection wizard

- [ ] 6.1 `fetch_releases()`: `curl -fsSL https://api.github.com/repos/<owner>/<repo>/releases` parses to a list of objects with `tag_name`, `prerelease`, `published_at`. Use minimal jq-free parsing if possible (the GitHub JSON is regular enough that `grep -oE '"tag_name":[^,]*'` + sed extraction works) OR require `jq` and check for it (offer to apt-install jq earlier if missing). Lean toward requiring `jq` — it's a 5-line apt-install and the parsing is fragile without it.
- [ ] 6.2 Filter: tags matching the production regex `^v[0-9]+\.[0-9]+\.[0-9]+$` are production; anything with a dash suffix is pre-release. Take the top `$RECOMMENDED_TAG_COUNT` production tags by published-at.
- [ ] 6.3 Present a numbered menu: `[1] v1.2.3 (released YYYY-MM-DD) — recommended` ... `[5] v1.0.0 (...)`, `[m] Enter a specific tag manually (use this for pre-release versions like v1.3.0-rc1)`, `[s] Skip — I'll download the binary myself`.
- [ ] 6.4 Default selection on bare-Enter: `[1]`. On `[m]`: prompt for the tag string, validate it exists on the releases page before proceeding (curl HEAD against the release URL).
- [ ] 6.5 On `[s]`: skip the download + verification step, jump to the config wizard. Operator's responsibility to place the binary somewhere on PATH.

## 7. Binary download + verification

- [ ] 7.1 Construct asset URL: `https://github.com/<owner>/<repo>/releases/download/<tag>/autocoder-<tag>-<target-triple>`. Same for `.sha256`.
- [ ] 7.2 Download both into a `mktemp -d` workspace. Refuse to proceed if either curl returns non-2xx (with the asset URL printed so the operator can sanity-check the tag/triple exists).
- [ ] 7.3 Verify: `cd <tmpdir> && sha256sum -c autocoder-<tag>-<triple>.sha256` (or `shasum -a 256 -c` on macOS). Print the computed digest + expected digest on failure. Refuse to install on mismatch — leave the temp dir in place for forensics.
- [ ] 7.4 On success: `install -m 755 -o <owner> -g <group> autocoder-<tag>-<triple> <install-path>/autocoder`. In server mode the binary is installed root-owned but executable by `autocoder`. In dev mode the operator's own UID/GID owns it.

## 8. Server-mode user creation + state dirs

- [ ] 8.1 If install mode is server AND the `autocoder` user does NOT exist: `useradd --system --shell /usr/sbin/nologin --home-dir /var/lib/autocoder --create-home autocoder`. Idempotent — if the user already exists from a prior install, skip with a log line.
- [ ] 8.2 Create state directories with correct ownership: `/var/lib/autocoder/` (chowned autocoder:autocoder, mode 0750), `/etc/autocoder/` (mode 0750, owned root:autocoder so root can edit, autocoder can read). `/tmp/workspaces/` and `/tmp/autocoder/` are NOT pre-created — the daemon creates them on first iteration; we just chown them to autocoder:autocoder if they exist already (idempotent re-install case).

## 9. Config wizard

- [ ] 9.1 If `<config-dir>/config.yaml` already exists: print "existing config detected at <path>; skipping config wizard" and jump to the systemd-unit step. Upgrade path = binary swap only.
- [ ] 9.2 Download `config.example.yaml` from the chosen release tag (`https://raw.githubusercontent.com/<owner>/<repo>/<tag>/config.example.yaml`) so the operator gets the version-matched defaults. Save it to `<config-dir>/config.example.yaml` for future reference.
- [ ] 9.3 Prompt for each essential field with sensible defaults shown in brackets:
  - Repository URL (no default; required; example: `git@github.com:owner/repo.git`)
  - Base branch (default `main`)
  - Agent branch (default `agent-q`)
  - Poll interval seconds (default `300`)
- [ ] 9.4 GitHub PAT prompt: `read -s` (silent). Validate that the value isn't empty and looks plausibly like a PAT (starts with `ghp_` or `github_pat_`). Write to `<config-dir>/secrets.env` as `GITHUB_TOKEN=<value>` with chmod 600 + appropriate ownership.
- [ ] 9.5 ChatOps backend menu: `[1] none (default)` / `[2] slack` / `[3] discord` / `[4] teams` / `[5] mattermost` / `[6] matrix`. On non-`none`: prompt for the backend's token (silent input) AND the default channel ID. Append to `secrets.env` under the appropriate env var name (`SLACK_BOT_TOKEN`, etc., matching the names in `config.example.yaml`).
- [ ] 9.6 Reviewer menu: `[1] none (default)` / `[2] Anthropic — claude-sonnet-4-6` / `[3] OpenAI-compatible`. On non-`none`: prompt for the API key (silent). Append to `secrets.env`.
- [ ] 9.7 Assemble `config.yaml` by starting from the downloaded `config.example.yaml` and: editing the first `repositories[].url` to the operator's input, replacing default branches with operator values, uncommenting the matching `reviewer:` and `chatops:` blocks with operator's choices, leaving `audits:` commented (operator can opt in later). Write to `<config-dir>/config.yaml` with chmod 640 + appropriate ownership.

## 10. systemd unit (server mode only)

- [ ] 10.1 Render `/etc/systemd/system/autocoder.service` with content:
  ```
  [Unit]
  Description=autocoder — autonomous OpenSpec-driven coding agent
  After=network-online.target
  Wants=network-online.target

  [Service]
  Type=simple
  User=autocoder
  Group=autocoder
  EnvironmentFile=/etc/autocoder/secrets.env
  ExecStart=/usr/local/bin/autocoder run --config /etc/autocoder/config.yaml
  Restart=on-failure
  RestartSec=10
  WorkingDirectory=/var/lib/autocoder
  StandardOutput=journal
  StandardError=journal
  # Hardening — restrictive but compatible with the daemon's normal operation
  NoNewPrivileges=true
  PrivateTmp=false   # daemon uses /tmp/workspaces; can't isolate
  ProtectSystem=strict
  ReadWritePaths=/tmp /var/lib/autocoder /etc/autocoder
  ProtectHome=true

  [Install]
  WantedBy=multi-user.target
  ```
- [ ] 10.2 `systemctl daemon-reload`. Prompt `Enable and start now? [Y/n]` — default yes. On yes: `systemctl enable --now autocoder`. Print `journalctl -u autocoder -f` as the live-logs command.

## 11. Post-install summary

- [ ] 11.1 Print a clear "✓ install complete" line followed by:
  - Binary path
  - Config path
  - Secrets path (with reminder it has chmod 600)
  - Service status (server) or run command (dev)
  - Reminder: `claude auth login` if not yet done
  - Reminder: to add more repos, edit config.yaml and `autocoder reload`
  - One-line link to README

## 12. README integration

- [ ] 12.1 Add a new "## Quick install" section at the very top of README (immediately under the title line). Content: one-line summary of what autocoder is (existing first paragraph), then the curl one-liner prominently, then a sentence describing what the script does, then the server-vs-dev distinction.
- [ ] 12.2 Demote the existing "## Quick Start" section to "## Manual install from source" and move it below "## Quick install". Add a sentence at the top of "Manual install from source" saying "recommended for contributors and operators who need fine-grained control; new users should prefer the install script above."
- [ ] 12.3 In the existing "## Deployment" section (further down): add a one-line cross-reference to the install script ("the install script handles the steps below for you; this section documents the manual equivalents for operators who want to audit or customize").

## 13. shellcheck

- [ ] 13.1 `shellcheck install.sh` — zero errors, zero warnings. Suppress style warnings selectively with `# shellcheck disable=SCxxxx` only when there's a documented reason; don't blanket-disable.

## 14. Spec delta

- [ ] 14.1 Author the ADDED requirement under `project-documentation` per the proposal: "Install script is the recommended deployment path".

## 15. Verification

- [ ] 15.1 `openspec validate install-script-and-wizard --strict` passes.
- [ ] 15.2 Run `bash install.sh` end-to-end on a fresh Debian 12 VM (Vagrant box, Docker container with systemd, etc.). Expect: prompts work, systemd unit installed and active, daemon's first iteration succeeds (it will fail to do anything productive without a real openspec change in the configured repo, but it should not panic). Document the result in the change's PR description.
- [ ] 15.3 Run `bash install.sh` on macOS — expect: dev-mode-only flow, binary in `~/.local/bin/`, config in `~/.config/autocoder/`. Document result.
- [ ] 15.4 Re-run `bash install.sh` against an existing install — expect: wizard skipped, binary swapped, config preserved.
