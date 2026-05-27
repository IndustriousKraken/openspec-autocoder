## ADDED Requirements

### Requirement: Install wizard probes systemd for an existing installation before falling through to default-path checks
`autocoder install` SHALL probe `systemctl show autocoder.service` before its default-path idempotency check to detect existing installations whose config is at a non-default location. The probe SHALL extract three properties: `LoadState`, `FragmentPath`, and the `--config <path>` argument from `ExecStart`. The result SHALL drive a three-way branch:

- `LoadState=loaded` AND `--config <path>` extracted AND `<path>` exists → existing-install detected. The subcommand SHALL print a status block naming the existing config path and the three remediation verbs (`./update.sh` for binary update, `autocoder install --reconfigure <section>` for section-level re-prompt, `sudo rm -rf <config-dir> && ./install.sh` for full reset) AND exit 0 without invoking the wizard, creating users, installing packages, or rewriting any file.
- `LoadState=loaded` AND `--config <path>` extracted AND `<path>` does NOT exist → broken install. The subcommand SHALL exit non-zero with a diagnostic naming the unit's `FragmentPath`, the missing config path, and the suggested remediations.
- `LoadState=not-found` OR the probe itself fails (no systemd, command errors) OR `--config <path>` cannot be extracted from `ExecStart` → fall through to the existing `<config-dir>/config.yaml` idempotency check. Pre-spec behavior preserved.

Dev mode (`autocoder install --mode dev`, or auto-detected dev mode on macOS / non-systemd Linux) SHALL skip the probe entirely — dev mode has no systemd unit, and running `systemctl show` would either error or report `not-found`.

#### Scenario: Existing install at a non-default config location is detected and respected
- **WHEN** an operator runs `autocoder install` on a server-mode host AND `systemctl show autocoder.service` reports `LoadState=loaded` AND `ExecStart` contains `--config /home/autocoder/autocoder/config.yaml` AND that file exists
- **THEN** the subcommand prints a status block naming `/home/autocoder/autocoder/config.yaml` AND the three remediation verbs
- **AND** the subcommand exits 0
- **AND** the operator's existing config, secrets, and systemd unit are NOT modified
- **AND** `useradd`, `apt-get install`, `daemon-reload`, `enable_systemd_unit`, and `start_systemd_unit` are NOT called (verifiable via the `RecordedCall` log in `cargo test`)

#### Scenario: Broken install (unit loaded, config missing) is refused with a diagnostic
- **WHEN** the operator runs `autocoder install` AND `systemctl show autocoder.service` reports `LoadState=loaded` AND `ExecStart` contains `--config <path>` AND `<path>` does NOT exist on disk
- **THEN** the subcommand exits non-zero
- **AND** the error message names the unit's `FragmentPath`
- **AND** the error message names the missing config path
- **AND** the error message lists at least two remediation hints (restore the config from backup OR remove the unit file and re-run `install.sh`)
- **AND** no file is created or modified by the install subcommand

#### Scenario: No existing unit falls through to default-path check
- **WHEN** the operator runs `autocoder install` AND `systemctl show autocoder.service` reports `LoadState=not-found`
- **THEN** the subcommand proceeds to the existing default-path check at `<config-dir>/config.yaml`
- **AND** if that file exists, behavior matches the pre-spec "Existing config detected" scenario
- **AND** if that file does not exist, the wizard runs as it did pre-spec

#### Scenario: `systemctl` itself fails (host has no systemd binary)
- **WHEN** the operator runs `autocoder install` AND the `systemctl` command exits non-zero OR the binary is not on PATH
- **THEN** `probe_systemd_unit` returns `LoadState::NotFound` (treating the failure as "no unit found")
- **AND** the subcommand falls through to the default-path check
- **AND** the operator is not blocked from completing a fresh install on a non-systemd host

#### Scenario: Loaded unit with no `--config` flag falls through with a WARN
- **WHEN** the unit's `ExecStart` does NOT include `--config <path>` (operator launches autocoder against a config implied via env var, for example)
- **THEN** the subcommand logs a WARN naming the unit's `FragmentPath` and noting the missing `--config` flag
- **AND** the subcommand falls through to the default-path check (the parser cannot determine which config to respect; refusing to proceed on this ambiguity is worse than the default-path fallback)

#### Scenario: Dev mode skips the systemd probe
- **WHEN** the operator runs `autocoder install --mode dev` on any platform OR `autocoder install` on macOS / non-systemd Linux
- **THEN** `probe_systemd_unit` is NOT invoked (verifiable via the `RecordedCall` log)
- **AND** the existing dev-mode flow (write to `~/.config/autocoder/`, no systemd work) proceeds unchanged

#### Scenario: Probe surface is testable via the `SystemActions` trait
- **WHEN** the install-subcommand tests run under `cargo test`
- **THEN** every test uses a `RecordingActions` impl whose `probe_systemd_unit` returns a configured `SystemdUnitProbe` fixture
- **AND** tests cover at minimum: a loaded unit with a valid `--config` path; a loaded unit with a missing `--config` path; a not-found unit; a loaded unit with no `--config` flag; a probe-fails-entirely case
- **AND** no test invokes the production `RealSystemActions::probe_systemd_unit`
