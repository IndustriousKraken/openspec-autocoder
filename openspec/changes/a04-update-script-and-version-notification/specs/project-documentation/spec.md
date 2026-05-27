## ADDED Requirements

### Requirement: `update.sh` is a thin bash bootstrap script for binary upgrades
The repository SHALL ship `update.sh` at the repo root as a bounded bash script (target ≤ 150 lines including comments) whose sole responsibilities are: resolve current and target versions, download the release binary and its SHA-256 companion, verify the checksum, invoke the binary's `check-config` subcommand as a preflight against the operator's existing config, atomically swap the binary with a `.previous` rollback artifact, restart the systemd unit, and verify the daemon comes back up — rolling back on failure. All business logic that benefits from unit-test coverage (config validation, version parsing) SHALL live in the autocoder Rust binary; `update.sh` orchestrates the steps but does not reimplement them.

`update.sh` SHALL NOT prompt the operator. The script is designed for cron invocation; every required input is either a flag, an environment variable, or derived from `systemctl show`. Operators wanting interaction stick to manual binary swaps.

`update.sh` SHALL default to the latest non-prerelease tag (via `GET /repos/<owner>/<repo>/releases/latest`). The `--version <tag>` flag opts in to a specific tag, including pre-releases.

#### Scenario: First-time run picks up latest tag and applies it
- **WHEN** an operator runs `./update.sh` AND the installed binary version is older than the latest non-prerelease tag
- **THEN** the script downloads the latest binary AND its `.sha256`
- **AND** verifies the checksum
- **AND** runs the new binary's `check-config --config <resolved-path> --json` preflight
- **AND** atomically swaps the old binary aside to `/usr/local/bin/autocoder.previous`
- **AND** installs the new binary at `/usr/local/bin/autocoder`
- **AND** restarts the `autocoder.service` systemd unit
- **AND** verifies the daemon is `active` within 30 seconds
- **AND** emits one INFO line to journalctl via `logger -t autocoder-update` naming the version transition
- **AND** exits 0

#### Scenario: Already-on-latest exit is a clean no-op
- **WHEN** the operator runs `./update.sh` AND the installed version matches the latest non-prerelease tag
- **THEN** the script prints `autocoder is already on <tag>; nothing to do`
- **AND** exits 0 without downloading anything
- **AND** the daemon is untouched

#### Scenario: Preflight failure leaves the daemon on the old binary
- **WHEN** the operator runs `./update.sh` AND the downloaded binary's `check-config` exits 2 (hard errors against the existing config)
- **THEN** the script dumps the JSON findings to stderr
- **AND** prints `update.sh: preflight failed; not swapping. Daemon continues on <current-version>.`
- **AND** exits non-zero
- **AND** the old binary at `/usr/local/bin/autocoder` is unchanged
- **AND** the daemon continues running on the old version

#### Scenario: Restart-verify failure triggers automatic rollback
- **WHEN** the swap completes AND `systemctl restart autocoder` runs AND the daemon does NOT reach `active` within 30 seconds
- **THEN** the script restores `/usr/local/bin/autocoder.previous` over `/usr/local/bin/autocoder`
- **AND** runs `systemctl restart autocoder` again
- **AND** prints `update.sh: new binary failed to start; rolled back to <previous-version>. Check journalctl -u autocoder.`
- **AND** exits non-zero
- **AND** the daemon resumes running on the previous version

#### Scenario: `--version <tag>` opts in to specific tags including pre-releases
- **WHEN** the operator runs `./update.sh --version v2.0.0-rc1`
- **THEN** the script uses `v2.0.0-rc1` as the target (bypassing the `/releases/latest` filter that excludes pre-releases)
- **AND** the rest of the flow (download, verify, preflight, swap, restart) is identical to a non-prerelease tag

#### Scenario: `--dry-run` reports without swapping
- **WHEN** the operator runs `./update.sh --dry-run`
- **THEN** the script resolves current and target versions, downloads, verifies, AND runs the preflight
- **AND** does NOT call `swap_binary` or `systemctl restart`
- **AND** prints `[dry-run] Would swap to <tag>` AND exits 0
- **AND** the daemon and binary on disk are unchanged

#### Scenario: Bounded size and complexity
- **WHEN** a reviewer inspects `update.sh`
- **THEN** the file is ≤ 150 lines including comments
- **AND** contains no business-logic implementations of config parsing, version comparison, or path resolution beyond what bash and `systemctl show` provide directly
- **AND** every preflight check delegates to the autocoder binary via `check-config`

### Requirement: DEPLOYMENT.md documents unattended updates via cron
`docs/DEPLOYMENT.md` SHALL include a section titled `Unattended updates via cron` documenting the `update.sh` workflow, the recommended crontab entry shape (with stagger guidance), the `--version` opt-out for operators who pin releases manually, and an explicit audience caveat naming who the workflow is for AND who it isn't.

#### Scenario: Section names the recommended crontab entry
- **WHEN** an operator reads `docs/DEPLOYMENT.md`'s `Unattended updates via cron` section
- **THEN** the section contains a sample crontab entry running `update.sh` at a low-traffic hour
- **AND** the entry redirects stdout + stderr to a log file under `/var/log/autocoder-update.log` (or operator-chosen location)
- **AND** the section suggests jittering the minute field when running across multiple hosts to avoid simultaneous fleet updates

#### Scenario: Section documents the `--version` opt-out for manual pinning
- **WHEN** the operator reads the section
- **THEN** the text describes `update.sh --version <tag>` for operators who freeze on a known-good release between manual upgrade reviews
- **AND** explains that pre-release tags require the explicit flag (default is non-prerelease only)

#### Scenario: Audience caveat is explicit
- **WHEN** the operator reads the section
- **THEN** the text names the intended audience (single-host SBC, indie VPS, homelab deployments where set-and-forget is the explicit goal)
- **AND** the text names the non-audience (enterprise change-control environments where Ansible / apt / k8s registries already own update orchestration) AND advises against using `update.sh` in those environments
- **AND** the text cross-links to the `Switching from source-build to binary updates` section (from `a01`) for operators upgrading their existing source-built deployment to the binary-update workflow
