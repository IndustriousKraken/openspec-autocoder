## 1. SystemActions trait extension

- [ ] 1.1 In `autocoder/src/cli/install.rs`, add to the `SystemActions` trait:
  ```rust
  async fn probe_systemd_unit(&self, unit_name: &str) -> Result<SystemdUnitProbe>;
  ```
- [ ] 1.2 Define the return type at module scope:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct SystemdUnitProbe {
      pub load_state: LoadState,
      pub fragment_path: Option<PathBuf>,
      pub exec_start_config_path: Option<PathBuf>,
  }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum LoadState {
      Loaded,
      NotFound,
      Other(String),
  }
  ```

## 2. Production impl: `RealSystemActions::probe_systemd_unit`

- [ ] 2.1 Shell out to `systemctl show <unit> -p LoadState -p FragmentPath -p ExecStart` and capture stdout.
- [ ] 2.2 Parse the `KEY=VALUE` lines (one per requested property). `LoadState=loaded` → `LoadState::Loaded`; `LoadState=not-found` → `LoadState::NotFound`; anything else → `LoadState::Other(value)`. `FragmentPath=<empty>` → `None`; otherwise `Some(PathBuf::from(value))`.
- [ ] 2.3 Parse `ExecStart` to extract the first `--config <path>` argument. systemd renders ExecStart as a structured `{ path=... ; argv[]=... ; ... }` block; the simplest reliable parse is to find the substring after `argv[]=` (or just scan the whole `ExecStart=` line) and tokenize on whitespace, looking for `--config` followed by a non-flag token. Return `None` if `--config` is absent OR followed by another `--<flag>` (operator passed `--config` with no value).
- [ ] 2.4 If `systemctl` itself fails (binary missing, non-zero exit), return `Ok(SystemdUnitProbe { load_state: LoadState::NotFound, ... })` — same as "no unit found." A failure to probe should not be a failure to install.
- [ ] 2.5 Unit tests against captured `systemctl show` output fixtures: a loaded unit with `--config /home/autocoder/autocoder/config.yaml`; a loaded unit with no `--config` flag; a not-found unit; a unit with `LoadState=masked`. Each fixture parses to the expected `SystemdUnitProbe`.

## 3. Recording impl for tests

- [ ] 3.1 `RecordingActions` gains a `probe_systemd_unit_responses: Mutex<HashMap<String, SystemdUnitProbe>>` field (keyed by unit name). Tests set up expected fixtures via a `with_probe_response(unit_name, probe)` builder method.
- [ ] 3.2 `RecordingActions::probe_systemd_unit` returns the configured fixture or — if none — a default `SystemdUnitProbe { load_state: LoadState::NotFound, fragment_path: None, exec_start_config_path: None }`.
- [ ] 3.3 The call is recorded in the `RecordedCall` log so tests can assert the probe was invoked.

## 4. `execute_inner` integration

- [ ] 4.1 Add a `detect_existing_install` step at the top of `execute_inner` (before the existing default-path idempotency check). Skip the step when `mode == InstallMode::Dev` — dev mode has no systemd unit by definition.
- [ ] 4.2 Call `actions.probe_systemd_unit("autocoder.service").await?`. Branch on the result:
  - `LoadState::Loaded` with `exec_start_config_path: Some(path)` AND `path.exists()` → call `print_existing_install_verbs(&path)` and return `Ok(())`.
  - `LoadState::Loaded` with `exec_start_config_path: Some(path)` AND `!path.exists()` → bail with a diagnostic naming `fragment_path`, the missing config path, and the two remediation hints.
  - `LoadState::Loaded` with `exec_start_config_path: None` (operator's unit has no `--config` flag, or our parser couldn't extract it) → log a WARN line naming this, then fall through to the default-path check (better to proceed than to refuse on a parse ambiguity).
  - `LoadState::NotFound` OR `LoadState::Other(_)` → fall through to the default-path check (existing behavior).
- [ ] 4.3 New function `print_existing_install_verbs(config_path: &Path)`:
  ```text
  autocoder is already installed (config: <path>).
  
  To update the binary:        ./update.sh        (or wire into cron)
  To reconfigure a section:    autocoder install --reconfigure <audits|reviewer|chatops>
  To wipe and reinstall:       sudo rm -rf <config-dir> && ./install.sh
  
  No changes made.
  ```
  The `--reconfigure` and `update.sh` hints land in `a02` and `a04`; the text is correct text whether those changes have merged yet (mentioning the verbs an operator will eventually have is harmless and helps a reader discover them).
- [ ] 4.4 Tests:
  - Loaded unit + config exists → `execute_inner` prints the verbs block AND returns without invoking `create_user`, `apt_install`, or the wizard (assert via the `RecordedCall` log).
  - Loaded unit + config missing → `execute_inner` returns `Err` with a message containing the fragment path AND the missing config path.
  - Not-found unit + no default-location config → `execute_inner` proceeds through the wizard (existing behavior).
  - Not-found unit + default-location config exists → `execute_inner` hits the existing idempotency exit (existing behavior).
  - Dev mode → `probe_systemd_unit` is NOT called (assert via the `RecordedCall` log).
  - Loaded unit + no `--config` flag in `ExecStart` → falls through with a WARN, behaves like the default-path check.

## 5. DEPLOYMENT.md "Switching from source-build to binary updates"

- [ ] 5.1 Add a new section in `docs/DEPLOYMENT.md` titled `Switching from source-build to binary updates`, positioned between `Recommended: install from a binary release` and `## 1. Install the binary`.
- [ ] 5.2 Document the safe invocation: `curl ... install.sh | bash -s -- --config-dir <existing-config-dir>`. Explain that the wizard's new systemd probe detects the existing daemon, leaves config + unit untouched, and just swaps the binary at `/usr/local/bin/autocoder`. Follow with `sudo systemctl restart autocoder`.
- [ ] 5.3 Document the manual-download alternative — the same `curl + sha256sum + install -m 755` sequence `install.sh` runs internally — for operators who prefer to skip the bash wrapper. Reference the contractual asset names from the `project-documentation` spec (`autocoder-<tag>-<triple>`).
- [ ] 5.4 Cross-link forward to the unattended-update story landing in `a04` (write the cross-link as `[Unattended updates via cron](DEPLOYMENT.md#unattended-updates-via-cron)` — the anchor will resolve once `a04` ships; until then it's a dead anchor in the same file, which is acceptable for a stacked-dependency change).

## 6. Spec deltas

- [ ] 6.1 `openspec/changes/a01-installer-detects-existing-setup/specs/orchestrator-cli/spec.md` ADDs one requirement covering the systemd probe, the three-way branching, dev-mode skip, and the verb-print exit shape.
- [ ] 6.2 `openspec/changes/a01-installer-detects-existing-setup/specs/project-documentation/spec.md` ADDs one requirement requiring `DEPLOYMENT.md` to carry the source-build-to-binary switch documentation.

## 7. Verification

- [ ] 7.1 `cargo test` passes (new + existing).
- [ ] 7.2 `openspec validate a01-installer-detects-existing-setup --strict` passes.
- [ ] 7.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
