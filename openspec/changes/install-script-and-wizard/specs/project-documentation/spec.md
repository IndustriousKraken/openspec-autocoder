## ADDED Requirements

### Requirement: Install script is the recommended deployment path
The repository SHALL include an `install.sh` script at the repo root that an operator can run via `curl -fsSL <raw-github-url-to-install.sh> | bash` (or download-then-run) to obtain a working autocoder installation. The script SHALL be self-contained in pure bash with no `whiptail` / `dialog` dependencies, handle OS + architecture detection, optionally install system dependencies on Debian-based hosts, optionally install the `claude` CLI, present a version-selection menu derived from the GitHub Releases API, download and SHA-256-verify the chosen binary, run a config wizard that asks the minimum essential questions, and (server mode only) generate a systemd unit running as a dedicated `autocoder` system user. README SHALL position this script as the recommended onboarding path and demote the source-build instructions to a "Manual install from source" section for advanced users.

#### Scenario: First-time install on a Linux server
- **WHEN** an operator runs `curl -fsSL <install-url> | bash` on a
  fresh Debian/Ubuntu host with systemd and sudo available
- **THEN** the script offers server mode by default, installs
  system dependencies (after explicit prompt), optionally installs
  the Claude CLI, downloads the latest production-tagged binary,
  verifies its SHA-256 against the published checksum, creates an
  `autocoder` system user via `useradd --system`, writes
  `/etc/autocoder/config.yaml` and `/etc/autocoder/secrets.env`
  (chmod 640 / 600, owned root:autocoder), generates and enables
  `/etc/systemd/system/autocoder.service` running as the
  `autocoder` user with `EnvironmentFile=/etc/autocoder/secrets.env`,
  starts the service, and prints `journalctl -u autocoder -f` as
  the live-logs command

#### Scenario: First-time install on a local dev machine
- **WHEN** an operator runs the install script on macOS OR on
  Linux without systemd available
- **THEN** the script selects dev mode automatically, downloads
  the binary to `~/.local/bin/autocoder` (or `/usr/local/bin/`
  with sudo, prompted), writes config to
  `~/.config/autocoder/config.yaml` (chmod 600), does NOT create
  a system user, does NOT install a systemd unit, and prints
  `autocoder run --config ~/.config/autocoder/config.yaml` as
  the start command

#### Scenario: Production-version recommendation in the version menu
- **WHEN** the version-selection wizard fetches the releases list
  from the GitHub API
- **THEN** the script filters to tags matching the strict
  production regex `^v[0-9]+\.[0-9]+\.[0-9]+$` (excluding any tag
  with a dash suffix like `-rc1`, `-dev`, `-beta.2`), sorts by
  published-at descending, and presents the top 5 as a numbered
  menu with the latest production tag flagged as the recommended
  default
- **AND** a `[m] Enter a tag manually` option is available for
  operators who specifically want to install a pre-release
  version, with no restriction on the tag format the operator
  enters (the script just validates the asset URL resolves)

#### Scenario: Checksum verification failure refuses install
- **WHEN** the downloaded binary's SHA-256 digest does NOT match
  the content of the corresponding `.sha256` file from the
  release
- **THEN** the script aborts the install with a clear error
  message naming both the computed and expected digests AND the
  download URL
- **AND** the binary is NOT moved into the install path
- **AND** the temporary download directory is preserved (logged
  in the error message) so an operator can investigate manually

#### Scenario: Re-running the script on an existing install (upgrade)
- **WHEN** the install script runs and `<config-dir>/config.yaml`
  already exists from a prior install
- **THEN** the script skips the config wizard entirely
- **AND** offers to upgrade the binary to the chosen version,
  preserving the existing config and secrets files unchanged
- **AND** the existing systemd unit (server mode) is left in
  place unless its content has materially changed, in which case
  the script asks before overwriting

#### Scenario: Server mode requires sudo or root
- **WHEN** the operator selects server mode AND the script is not
  running as root AND `sudo` is not available on the host
- **THEN** the script prints a clear "server mode requires root
  privileges; falling back to dev mode" message AND continues
  with the dev-mode flow
- **AND** the rest of the install completes successfully under
  dev mode without leaving any partial server-mode state on disk
  (no `autocoder` user, no `/etc/autocoder/`, no systemd unit)

#### Scenario: Optional Claude CLI install
- **WHEN** the script detects `claude` is not on the operator's
  PATH AND prompts whether to install it
- **THEN** the script prints the canonical Claude install URL
  before any curl-bash so the operator can see what they're about
  to execute and decline if they want to install Claude manually
- **AND** on yes: runs the official installer
- **AND** on no: prints a one-line reminder that `claude` must
  be installed and authenticated (`claude auth login`) before
  autocoder's executor can run, with the canonical Claude docs
  URL

#### Scenario: README recommends the script as the onboarding path
- **WHEN** a new visitor reads README from the top
- **THEN** the first major section after the project description
  is "Quick install" featuring the curl one-liner prominently,
  with a brief explanation of what the script does AND the
  server-vs-dev distinction
- **AND** any source-build instructions appear LATER in the
  document under a clearly-labeled "Manual install from source"
  heading, framed as the path for contributors and advanced
  operators rather than the default
