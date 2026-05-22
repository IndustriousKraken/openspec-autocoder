## ADDED Requirements

### Requirement: Install subcommand
autocoder SHALL ship an `install` subcommand alongside `run`, `rewind`, and `reload`. The subcommand SHALL collect the minimum configuration an operator needs for a working first-run (one repository URL, a GitHub PAT, optional chatops backend, optional reviewer backend), generate a `config.yaml` + `secrets.env` pair at the appropriate location for the chosen install mode (server vs dev), and on server mode generate + enable a systemd unit that runs the daemon as a dedicated `autocoder` system user. All OS-mutating actions (`useradd`, `chown`, `chmod`, `apt-get install`, `systemctl daemon-reload`, `systemctl enable`, `systemctl start`, claude installer subprocess) SHALL go through a `SystemActions` trait whose production implementation shells out and whose test implementation records calls — so `cargo test` covers the orchestration without needing a real host.

#### Scenario: First-time install (server mode)
- **WHEN** an operator runs `autocoder install` (typically via
  `install.sh`'s `exec autocoder install "$@"` handoff) on a
  Linux host with systemd available AND no existing
  `<config-dir>/config.yaml`
- **THEN** the subcommand creates the `autocoder` system user
  (idempotent: skipped if already present), prompts for the
  essential config fields, writes `/etc/autocoder/config.yaml`
  (chmod 640, owner root:autocoder) and
  `/etc/autocoder/secrets.env` (chmod 600, owner root:autocoder),
  renders and enables `/etc/systemd/system/autocoder.service`
  running as `User=autocoder` with
  `EnvironmentFile=/etc/autocoder/secrets.env`, starts the
  service (prompted, default yes), and prints a post-install
  summary

#### Scenario: First-time install (dev mode)
- **WHEN** an operator runs `autocoder install` on macOS OR on
  Linux without systemd available OR with the `--mode dev` flag
  AND no existing config
- **THEN** the subcommand prompts for the same essential
  fields, writes config to `~/.config/autocoder/config.yaml`
  (chmod 600, owned by the operator's UID), writes
  `~/.config/autocoder/secrets.env` (chmod 600), does NOT
  create a system user, does NOT install a systemd unit, AND
  prints `autocoder run --config ~/.config/autocoder/config.yaml`
  as the start command

#### Scenario: Existing config detected
- **WHEN** an operator runs `autocoder install` AND
  `<config-dir>/config.yaml` already exists
- **THEN** the subcommand prints a status block naming the
  existing config path, notes that any binary swap has already
  happened (in install.sh), AND exits 0 without prompting for
  anything
- **AND** the operator's existing config and secrets files are
  not touched

#### Scenario: Non-interactive mode with all required flags
- **WHEN** an operator runs
  `autocoder install --non-interactive --repo-url <url>
  --token-env-var GITHUB_TOKEN --chatops-backend none
  --reviewer-provider none`
- **THEN** the subcommand runs end-to-end without reading from
  stdin
- **AND** the generated config.yaml + secrets.env reflect the
  flag values verbatim
- **AND** the operator can drive `autocoder install` from
  Ansible, Terraform, cloud-init, etc. without a TTY

#### Scenario: Non-interactive mode missing a required flag
- **WHEN** an operator runs `autocoder install --non-interactive`
  WITHOUT supplying `--repo-url`
- **THEN** the subcommand exits non-zero with an error message
  naming the missing flag explicitly AND listing the full set of
  flags required for non-interactive mode
- **AND** no partial config is written to disk

#### Scenario: SystemActions abstraction tested via mock
- **WHEN** the install-subcommand tests run under `cargo test`
- **THEN** every test uses a `RecordingActions` impl of
  `SystemActions` that captures method calls into an in-memory
  vector
- **AND** tests assert the exact sequence of calls (e.g.
  `create_user("autocoder", ...)`, `daemon_reload()`,
  `enable_systemd_unit("autocoder")`,
  `start_systemd_unit("autocoder")`) for the server-mode flow
- **AND** no test ever calls the production
  `RealSystemActions::create_user` or runs `useradd` for real
  — the tests verify orchestration, not the underlying OS calls
- **AND** the production `RealSystemActions` impl is small
  enough (target ≤ 5 lines per method) to inspect by reading

#### Scenario: Wizard prompts are testable via scripted IO
- **WHEN** the wizard tests run
- **THEN** they use a `ScriptedIo` impl of the `WizardIo` trait
  that reads from a pre-loaded `VecDeque<String>` of answers
- **AND** assert the generated config.yaml + secrets.env match
  expected values for those answers
- **AND** no test depends on a TTY being available
