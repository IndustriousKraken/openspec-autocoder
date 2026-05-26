## ADDED Requirements

### Requirement: Daemon resolves four standard data-category paths with a defined precedence
The daemon SHALL resolve four data-category paths at startup: `state` (persistent state — audit cadence, failure counters, alert throttles, revisions), `cache` (re-creatable but kept — repo workspaces), `logs` (per-change run logs), and `runtime` (control socket, transient locks). Each path is resolved by this precedence: (1) an explicit `paths.<field>` value in `config.yaml`, (2) the per-field environment variable `AUTOCODER_STATE_DIR` / `AUTOCODER_CACHE_DIR` / `AUTOCODER_LOGS_DIR` / `AUTOCODER_RUNTIME_DIR`, (3) the systemd-set environment variable `$STATE_DIRECTORY` / `$CACHE_DIRECTORY` / `$LOGS_DIRECTORY` / `$RUNTIME_DIRECTORY`, (4) XDG-derived defaults (dev mode), (5) a hard fallback to `/var/lib/autocoder` and siblings. All four paths SHALL be absolute. No two paths may resolve to the same directory.

#### Scenario: Config explicit value wins over all env vars
- **WHEN** `config.yaml` sets `paths.state_dir: /custom/state` AND `AUTOCODER_STATE_DIR=/env/state` is set AND `$STATE_DIRECTORY=/var/lib/autocoder` is set
- **THEN** the resolved state path is `/custom/state`

#### Scenario: Env var wins over systemd-set var
- **WHEN** no config override AND `AUTOCODER_STATE_DIR=/env/state` AND `$STATE_DIRECTORY=/var/lib/autocoder`
- **THEN** the resolved state path is `/env/state`

#### Scenario: systemd-set var used when no config or env override
- **WHEN** no config override AND no env var AND `$STATE_DIRECTORY=/var/lib/autocoder`
- **THEN** the resolved state path is `/var/lib/autocoder`

#### Scenario: XDG defaults used in dev mode
- **WHEN** no config override AND no env var AND no systemd-set var AND `$HOME=/home/dev`
- **THEN** the resolved state path is `/home/dev/.local/state/autocoder` (or `$XDG_STATE_HOME/autocoder` when set)

#### Scenario: Relative-path config is rejected at startup
- **WHEN** `config.yaml` sets `paths.state_dir: relative/path`
- **THEN** the daemon fails to start with a clear error naming the field and requiring an absolute path

#### Scenario: Same path for two roles is rejected
- **WHEN** the resolution yields the same directory for two of the four roles
- **THEN** the daemon fails to start with an error naming both roles and the conflicting path

### Requirement: Workspaces, markers, and state move to standard locations; runtime remains ephemeral
Repo workspaces SHALL live under `<cache_dir>/workspaces/<sanitized-url>/` and SHALL include their in-tree marker files (`.perma-stuck.json`, `.needs-spec-revision.json`, `.question.json`, `.answer.json`, `.alert-state.json`, `.in-progress*`) as today. Per-audit-type cadence state SHALL live under `<state_dir>/audit-state/<audit-type>.json`. Per-change failure counters SHALL live under `<state_dir>/failure-state/<repo-sanitized>/<change-slug>.json`. Per-PR revision state SHALL live under `<state_dir>/revisions/<repo-sanitized>/<pr-number>.json`. Per-change run logs SHALL live under `<logs_dir>/runs/<repo-sanitized>/<change-slug>.log`. The control socket SHALL live at `<runtime_dir>/control.sock`. In-progress lock files SHALL live under `<runtime_dir>` so reboot clears them automatically.

#### Scenario: Workspace and its markers survive reboot under cache_dir
- **WHEN** the cache_dir resolves to `/var/cache/autocoder` (on real disk, not tmpfs) AND the workspace for repo X has `.perma-stuck.json` set for change Y AND the host reboots
- **THEN** after reboot the workspace at `/var/cache/autocoder/workspaces/<sanitized-X>/openspec/changes/Y/.perma-stuck.json` is still present
- **AND** the next polling iteration treats change Y as perma-stuck (no retry)

#### Scenario: Audit-state survives reboot under state_dir
- **WHEN** an audit ran 1 hour ago AND its state file at `<state_dir>/audit-state/<audit-type>.json` records that timestamp AND the host reboots
- **THEN** after reboot the daemon reads the state file at startup AND treats the audit's last-run as 1 hour ago
- **AND** the audit does NOT fire on the first polling iteration (its cadence has not elapsed)

#### Scenario: Control socket is recreated after reboot under runtime_dir
- **WHEN** the daemon starts AND the runtime_dir resolves to `/run/autocoder/` (tmpfs, cleared on reboot)
- **THEN** the daemon creates the control socket at `/run/autocoder/control.sock` regardless of whether it existed before
- **AND** the `autocoder reload` CLI's connection lookup uses the same resolved path

### Requirement: Audit-state is reloaded from disk on every daemon startup
The daemon SHALL scan `<state_dir>/audit-state/` on startup AND populate its in-memory audit cadence map from every parseable `<audit-type>.json` file found. Parse failures on individual files SHALL log a WARN naming the file and the parse error, and that audit treats as "never run" (the existing first-run fallback); other audits' state continues to load normally. Daemon restart without reboot SHALL NOT cause any audit to re-fire if its on-disk cadence timestamp shows the cadence has not elapsed.

#### Scenario: Audit-state reload populates the in-memory map
- **WHEN** the daemon starts AND `<state_dir>/audit-state/` contains valid state files for three audit types
- **THEN** the in-memory audit cadence map contains entries for all three audit types with their on-disk last-run timestamps

#### Scenario: One corrupt state file does not block other audits
- **WHEN** the audit-state dir has one parse-failing file AND two valid files
- **THEN** the in-memory map has the two valid entries
- **AND** a WARN is logged naming the corrupt file
- **AND** the corresponding audit treats as "never run"

#### Scenario: Daemon restart respects on-disk timestamps
- **WHEN** an audit's on-disk state shows `last_run: <30 minutes ago>` AND its cadence is `every-2-hours` AND the daemon restarts
- **THEN** the audit does NOT fire on the first polling iteration after restart
- **AND** the audit fires only after the cadence interval has elapsed from the on-disk timestamp

### Requirement: Legacy `/tmp` paths are auto-migrated on first startup
On daemon startup, if the file `<state_dir>/.migration-from-tmp-done` does NOT exist, the daemon SHALL scan well-known legacy `/tmp` paths and move their contents to the new locations. The migration is idempotent (a partially-completed migration resumes on the next startup), per-entry error-tolerant (one failing entry does not abort the rest), and writes the marker file only when every entry completed without error. Cross-partition moves (tmpfs → disk is the common case) fall back to recursive copy + delete-on-success when `fs::rename` fails with EXDEV. The daemon does NOT refuse to start if migration fails; partial migration is logged and operators can resolve orphan /tmp entries manually.

#### Scenario: First startup migrates legacy state
- **WHEN** the daemon starts AND no `.migration-from-tmp-done` marker exists AND legacy paths under /tmp contain state files / workspaces
- **THEN** each legacy entry is moved to its corresponding new location under state_dir / cache_dir / logs_dir
- **AND** the migration log line names the per-entry source and target paths

#### Scenario: Second startup skips migration
- **WHEN** the daemon starts AND `.migration-from-tmp-done` already exists
- **THEN** no legacy-path scan is performed
- **AND** no migration work is done

#### Scenario: Partial migration retries on next startup
- **WHEN** the daemon starts AND migration runs AND one entry fails (e.g. permission error) while others succeed
- **THEN** the marker file is NOT written
- **AND** the successful moves persist
- **AND** the next daemon startup re-scans, sees the migration is not complete, retries (entries already moved are skipped via the target-exists check; only the previously-failed entries are retried)

#### Scenario: Cross-partition move uses copy-and-delete fallback
- **WHEN** the source is on tmpfs AND the target is on a different partition AND `fs::rename` returns EXDEV
- **THEN** the migration falls back to recursive copy + delete-on-success
- **AND** the result is functionally identical to `fs::rename` (target populated, source removed)

#### Scenario: Target already exists is skipped
- **WHEN** a legacy source entry exists AND its corresponding target already exists
- **THEN** the entry is skipped (the target is treated as canonical)
- **AND** no overwrite is attempted
- **AND** the legacy source is left in place for operator inspection (the migration does not delete sources whose targets already exist)

### Requirement: systemd unit declares the four standard directories
The installed systemd unit template SHALL declare `StateDirectory=autocoder`, `CacheDirectory=autocoder`, `LogsDirectory=autocoder`, AND `RuntimeDirectory=autocoder` under `[Service]`. systemd auto-creates these directories with the service user's ownership at unit-start time and sets the `$STATE_DIRECTORY`, `$CACHE_DIRECTORY`, `$LOGS_DIRECTORY`, `$RUNTIME_DIRECTORY` environment variables, which the daemon's path-resolution reads (per the resolution-priority requirement).

#### Scenario: Rendered unit contains the four directives
- **WHEN** the install wizard renders the systemd unit template
- **THEN** the rendered unit text contains the lines `StateDirectory=autocoder`, `CacheDirectory=autocoder`, `LogsDirectory=autocoder`, AND `RuntimeDirectory=autocoder` under the `[Service]` section

#### Scenario: Daemon under systemd uses systemd-provided paths
- **WHEN** the daemon is started by systemd AND systemd has created the four directories AND set the corresponding env vars AND no config or `AUTOCODER_*_DIR` overrides exist
- **THEN** the resolved `DaemonPaths.state` matches `$STATE_DIRECTORY` (likely `/var/lib/autocoder`)
- **AND** the resolved `DaemonPaths.cache` matches `$CACHE_DIRECTORY` (likely `/var/cache/autocoder`)
- **AND** the resolved `DaemonPaths.logs` matches `$LOGS_DIRECTORY` (likely `/var/log/autocoder`)
- **AND** the resolved `DaemonPaths.runtime` matches `$RUNTIME_DIRECTORY` (likely `/run/autocoder`)
