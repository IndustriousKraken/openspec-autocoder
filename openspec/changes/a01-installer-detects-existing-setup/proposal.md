## Why

`autocoder install`'s existing-config detection looks only at `<default-config-dir>/config.yaml` (`/etc/autocoder/config.yaml` in server mode, `~/.config/autocoder/config.yaml` in dev mode). Operators who set autocoder up before the install wizard shipped — typically with config under `/home/autocoder/autocoder/config.yaml` and a hand-written systemd unit — are invisible to the check. Re-running `install.sh` against such a host triggers the full wizard, overwrites the systemd unit (losing custom `Environment="PATH=..."` lines, `WorkingDirectory`, the `--config` path), and produces a daemon that's pointed at a fresh wizard-generated config instead of the operator's existing one. In one observed case the new unit additionally sets `ProtectHome=true`, which then blocks the daemon from reading `/home/autocoder/.claude/` and `/home/autocoder/.ssh/` — bricking the install entirely on the next restart.

The installer needs to find non-standard existing setups before writing anything. The reliable signal is `systemctl show autocoder.service` — systemd knows where the unit lives and what `--config` path the daemon is launched with, regardless of where the operator put it.

## What Changes

**`SystemActions` gains a `probe_systemd_unit` method.** Returns the parsed result of `systemctl show autocoder.service -p LoadState -p FragmentPath -p ExecStart`. Production impl shells out and parses; test impl returns a recorded fixture.

**`execute_inner` runs the systemd probe before the default-path idempotency check.** Three cases:

- `LoadState=loaded` AND `ExecStart` contains `--config <path>` AND that path exists → existing-install detected at `<path>`. Print the available verbs (`./update.sh` for binary update — landing in `a04`; `autocoder install --reconfigure <section>` — landing in `a02`; `rm -rf <config-dir> && ./install.sh` for full reset) and exit 0 without prompting. The operator's existing config and unit are unchanged.
- `LoadState=loaded` AND `ExecStart` contains `--config <path>` AND that path does NOT exist → broken install. Exit non-zero with a diagnostic naming the unit's `FragmentPath`, the missing config path, and the suggested remediations (restore the config or `rm` the unit and re-run `install.sh`). Don't silently proceed as if fresh — that's the bug.
- `LoadState=not-found` OR the probe itself fails (no systemd, command errors) → fall through to the existing default-path check at `<config-dir>/config.yaml`. Pre-spec behavior preserved.

**Dev mode skips the systemd probe entirely.** Dev mode by definition has no systemd unit; running the probe would either error or report `not-found`. Skipping it avoids a useless subprocess on Mac and on Linux hosts without systemd.

**`docs/DEPLOYMENT.md` gains a "Switching from source-build to binary updates" section.** Targets operators who originally built from source (older deployments, contributors) and want to switch to the released-binary path. Documents the safe invocation: `install.sh --config-dir <existing-config-dir>` triggers the binary swap, the new probe detects the existing install at that path, the wizard exits clean. Also documents the manual-download alternative for operators who prefer to skip `install.sh` entirely.

**No interactive menu on existing-install detection.** The print-and-exit shape is intentional: `install.sh` is normally invoked via `curl | bash`, which has no TTY for interactive prompts. The printed verbs are equally discoverable without the interaction cost.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one ADDED requirement: `Install wizard probes systemd for an existing installation before falling through to default-path checks`. Extends today's "Existing config detected" scenario to recognize non-standard config locations.
  - `project-documentation` — one ADDED requirement: `DEPLOYMENT.md documents switching from source-build to binary upgrades`.
- **Affected code:**
  - `autocoder/src/cli/install.rs`:
    - `SystemActions::probe_systemd_unit(&self, unit_name: &str) -> Result<SystemdUnitProbe>` where `SystemdUnitProbe { load_state: LoadState, fragment_path: Option<PathBuf>, exec_start_config_path: Option<PathBuf> }` and `LoadState { Loaded, NotFound, Other(String) }`. Production impl runs `systemctl show <unit> -p LoadState -p FragmentPath -p ExecStart` and parses the `KEY=VALUE` lines. ExecStart parsing extracts the first `--config <path>` token; multiple `--config` flags or no `--config` flag resolve to `None`.
    - `RecordingActions` impl returns a configurable fixture so tests can simulate every state.
    - `execute_inner` gains a `detect_existing_install` step before the default-path check. Server mode invokes the probe; dev mode skips it.
    - New function `print_existing_install_verbs(config_path: &Path)` writes the three-verb status block to stdout.
  - `docs/DEPLOYMENT.md` — new "Switching from source-build to binary updates" section between "Recommended: install from a binary release" and "1. Install the binary."
- **Operator-visible behavior:** an operator running `install.sh` against a pre-wizard deployment sees a clear "existing install detected at `<path>`, no changes made; here's what you can do" message rather than the wizard prompts. Their config, secrets, and systemd unit are untouched. The detection also catches broken installs (unit present, config missing) and refuses to silently proceed.
- **Breaking:** no. The new probe runs before today's idempotency check; when the probe finds nothing, behavior is identical to pre-spec. Operators who relied on the default-path check continue to get it.
- **Acceptance:** `cargo test` passes; `openspec validate a01-installer-detects-existing-setup --strict` passes. A fixture simulating an existing systemd unit with `--config /home/autocoder/autocoder/config.yaml` (and that file existing in the test temp dir) causes `execute_inner` to print the verbs and return without invoking the wizard. A fixture with the unit loaded but the config path missing causes `execute_inner` to exit non-zero with a diagnostic.
