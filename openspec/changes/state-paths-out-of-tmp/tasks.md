## 1. Path-resolution module

- [ ] 1.1 Create `autocoder/src/paths.rs`. Public surface:
  ```rust
  pub struct DaemonPaths {
      pub state: PathBuf,    // persistent state (audit cadence, failure counters, revisions, alert throttles)
      pub cache: PathBuf,    // re-creatable but kept (workspaces)
      pub logs: PathBuf,     // per-change run logs
      pub runtime: PathBuf,  // socket, transient pid/lock files
  }
  pub fn resolve_daemon_paths(config: &Config) -> Result<DaemonPaths>;
  ```
- [ ] 1.2 Resolution priority per field:
  1. `config.paths.<field>` if set AND non-empty.
  2. `AUTOCODER_STATE_DIR` / `AUTOCODER_CACHE_DIR` / `AUTOCODER_LOGS_DIR` / `AUTOCODER_RUNTIME_DIR` env var if set AND non-empty.
  3. `$STATE_DIRECTORY` / `$CACHE_DIRECTORY` / `$LOGS_DIRECTORY` / `$RUNTIME_DIRECTORY` (systemd-set) if present.
  4. XDG defaults: `${XDG_STATE_HOME:-$HOME/.local/state}/autocoder`, `${XDG_CACHE_HOME:-$HOME/.cache}/autocoder`, `${XDG_STATE_HOME:-$HOME/.local/state}/autocoder/logs`, `${XDG_RUNTIME_DIR:-/tmp/${UID}-runtime}/autocoder`. Used when no systemd vars are present (dev mode).
  5. Hard fallback (no env, no systemd, no $HOME for some reason): `/var/lib/autocoder` etc. Should rarely fire; log WARN if it does.
- [ ] 1.3 Validation at resolution time:
  - Every path must be absolute (`PathBuf::is_absolute()`).
  - No two of `state`, `cache`, `logs`, `runtime` may resolve to the same directory (rejected: ambiguous co-location of role).
  - The parent directory of each path must exist OR `resolve_daemon_paths` must be able to create it; permission failure on creation is a startup error.
- [ ] 1.4 Tests:
  - Config sets `paths.state_dir: /custom/state` → resolved `DaemonPaths.state == /custom/state`.
  - Env var `AUTOCODER_STATE_DIR=/env/state` (no config) → resolved state is `/env/state`.
  - systemd-style `STATE_DIRECTORY=/var/lib/autocoder` (no config, no env var) → resolved state matches.
  - Dev mode (no systemd vars, no env vars, no config) → XDG defaults used.
  - Relative-path config rejected with clear error.
  - Two fields resolving to same path rejected.
  - Non-existent unwritable parent rejected.

## 2. Config schema addition

- [ ] 2.1 In `autocoder/src/config.rs`, add a top-level optional `paths:` block:
  ```rust
  pub struct DaemonPathsConfig {
      pub state_dir: Option<PathBuf>,
      pub cache_dir: Option<PathBuf>,
      pub logs_dir: Option<PathBuf>,
      pub runtime_dir: Option<PathBuf>,
  }
  pub struct Config {
      // existing fields...
      #[serde(default)]
      pub paths: DaemonPathsConfig,
  }
  ```
  All four sub-fields default to `None`. Empty `paths:` block parses cleanly.
- [ ] 2.2 Tests:
  - Absent `paths:` block → all four fields are `None`.
  - Explicit `paths: { state_dir: /custom }` → `state_dir == Some(PathBuf::from("/custom"))`, others `None`.
  - All four explicit → all four `Some(_)`.

## 3. Wire `DaemonPaths` through callsites

- [ ] 3.1 In `autocoder/src/workspace.rs`, replace the hard-coded `/tmp/workspaces/` derivation with `daemon_paths.cache.join("workspaces")`. The deterministic-sanitization rule (URL → directory name) is unchanged. Callers thread `DaemonPaths` through (or grab from a process-global lazy `OnceCell` populated at startup; pick whichever fits the existing dependency injection pattern best).
- [ ] 3.2 In `autocoder/src/control_socket.rs`, replace `/tmp/autocoder/control/control.sock` with `daemon_paths.runtime.join("control.sock")`. The CLI's reload-via-socket lookup uses the same resolution.
- [ ] 3.3 In `autocoder/src/audits/scheduler.rs` (and any other audit modules that write state), the audit-state file path becomes `daemon_paths.state.join("audit-state").join(format!("{audit_type}.json"))`.
- [ ] 3.4 In `autocoder/src/failure_state.rs`, the failure-state file path becomes `daemon_paths.state.join("failure-state").join(repo_sanitized).join(format!("{change}.json"))`. The repo_sanitized form uses the same URL-sanitization rule as workspaces (consistency).
- [ ] 3.5 In `autocoder/src/revisions.rs` (from `a01-pr-comment-revision-loop`), the per-PR state file path becomes `daemon_paths.state.join("revisions").join(repo_sanitized).join(format!("{pr_number}.json"))`.
- [ ] 3.6 Per-change run logs at `daemon_paths.logs.join("runs").join(repo_sanitized).join(format!("{change}.log"))`. Update the executor's log-writer construction.
- [ ] 3.7 Any in-progress lock files / per-process pid files move to `daemon_paths.runtime`. These are ephemeral by design; living in /run (or `$XDG_RUNTIME_DIR`) means reboot cleans them up automatically.
- [ ] 3.8 Tests for each callsite: with a fixture `DaemonPaths` pointing at a tempdir, the write produces a file at the expected sub-path under that tempdir.

## 4. Audit-state reload on startup

- [ ] 4.1 In `autocoder/src/audits/scheduler.rs` (or wherever the AuditState in-memory map is owned), add `pub fn reload_from_disk(state_dir: &Path) -> Result<HashMap<String, AuditState>>` that:
  - Walks `state_dir.join("audit-state")`, finds every `<audit-type>.json`.
  - Parses each via the existing AuditState serde derive.
  - Returns the populated map. Parse failures per-file are logged at WARN and skipped (that audit treats as first-run); other audits continue to load.
- [ ] 4.2 Call `reload_from_disk` at daemon start, BEFORE any audit cadence check fires. The result populates the in-memory map; subsequent cadence checks read from that map.
- [ ] 4.3 Tests:
  - Empty state dir → empty map returned, no errors.
  - State dir with three valid audit-state files → map has three entries with the expected last-run timestamps.
  - State dir with one corrupt JSON file + two valid → map has the two valid entries; WARN was logged for the corrupt one.
  - Subsequent in-memory writes after reload persist correctly (the existing write path is unchanged; this task only adds the read path).

## 5. Migration from legacy /tmp paths

- [ ] 5.1 Create `autocoder/src/migration.rs`. Public surface:
  ```rust
  pub struct MigrationReport {
      pub workspaces_moved: u32,
      pub state_files_moved: u32,
      pub log_files_moved: u32,
      pub errors: Vec<String>,
  }
  pub fn migrate_legacy_tmp_paths(daemon_paths: &DaemonPaths) -> Result<MigrationReport>;
  ```
- [ ] 5.2 Migration flow on each call:
  1. If `daemon_paths.state.join(".migration-from-tmp-done")` exists, return immediately with an empty report. (Subsequent startups skip the scan.)
  2. Scan well-known legacy paths:
     - `/tmp/autocoder/audit-state/*.json` → `daemon_paths.state.join("audit-state")/`
     - `/tmp/autocoder/failure-state/**/*.json` → `daemon_paths.state.join("failure-state")/<same-relative-path>`
     - `/tmp/autocoder/revisions/**/*.json` → `daemon_paths.state.join("revisions")/<same-relative-path>`
     - `/tmp/autocoder/logs/**/*.log` → `daemon_paths.logs.join("runs")/<same-relative-path>`
     - `/tmp/workspaces/<entry>/` → `daemon_paths.cache.join("workspaces")/<entry>/`
  3. For each source entry:
     - If the corresponding target already exists, skip (do not overwrite; assume the target is canonical).
     - Otherwise attempt `fs::rename`. On EXDEV (cross-partition; tmpfs → disk is the common case), fall back to recursive copy + delete-on-success.
     - On any other error, record in `MigrationReport.errors` and continue.
  4. If `errors.is_empty()`, write the migration marker file at `daemon_paths.state.join(".migration-from-tmp-done")`.
  5. Return the report.
- [ ] 5.3 Logging: per source-entry, log at INFO the source + target paths. At end, log a summary line with the report's counts and any error count. ERRORs log per-entry at ERROR with the source path and the OS error.
- [ ] 5.4 Tests:
  - Fixture: legacy /tmp dirs populated, no marker → migration moves everything, marker is written.
  - Fixture: marker already present → no scan, no moves.
  - Fixture: target file already exists at the destination → skipped, source is left in place, no error.
  - Fixture: source on tmpfs (simulated by creating in a tempdir on a different mount), target on disk → cross-partition copy + delete path exercised (use mockable filesystem layer OR test with a tempfile crate that exercises rename failure paths).
  - Fixture: one entry has a permission error during copy → recorded in `errors`, marker NOT written, other entries proceed.

## 6. Wire migration into daemon startup

- [ ] 6.1 In `autocoder/src/cli/run.rs`, at daemon start:
  1. Load config (existing).
  2. Call `resolve_daemon_paths(&config)` to get `DaemonPaths`.
  3. Create the four directories if missing (mkdir-p with mode 0750).
  4. Call `migrate_legacy_tmp_paths(&daemon_paths)`. Log the report.
  5. Call `reload_audit_state(&daemon_paths.state)` to populate the in-memory map.
  6. Pass `DaemonPaths` (or stash in a `OnceCell`) to the rest of the daemon initialization.
- [ ] 6.2 Migration failures (the report containing errors) are LOGGED but do not abort startup. The daemon proceeds with whatever state is in place. Operators see the ERROR lines in journalctl and can investigate / clean up orphan /tmp entries manually.
- [ ] 6.3 Tests:
  - End-to-end fixture: tempdir as daemon-paths root, populate /tmp with legacy fixture data (or mock the legacy-path constants), run daemon-start path, assert the migration ran, the marker was written, the daemon continues to its normal init.

## 7. systemd unit template updates

- [ ] 7.1 In `autocoder/src/cli/install_systemd.service`, add under `[Service]`:
  ```
  StateDirectory=autocoder
  CacheDirectory=autocoder
  LogsDirectory=autocoder
  RuntimeDirectory=autocoder
  ```
- [ ] 7.2 Adjust the existing `ReadWritePaths=` directive to reflect the new directories (the directives systemd auto-creates are already RW for the service user; ReadWritePaths can be tightened or removed accordingly).
- [ ] 7.3 The install wizard re-renders the unit when an upgrade install is run; operators on the OLD unit can `autocoder install --upgrade` to refresh, OR manually edit their unit to add the four directives.
- [ ] 7.4 Tests:
  - Render the unit template, assert it contains the four `*Directory=autocoder` lines.

## 8. Install wizard updates

- [ ] 8.1 Server mode: rendered unit gains the four directives (covered by task 7).
- [ ] 8.2 Dev mode: the install wizard computes XDG paths and writes them into the generated `config.yaml`'s `paths:` block so operators see the resolved values explicitly. Helps debuggability ("where is my state actually written?").
- [ ] 8.3 Tests:
  - Dev-mode install against a tempdir as $HOME produces a `config.yaml` whose `paths:` block contains the XDG-derived paths under that $HOME.

## 9. Docs + README updates

- [ ] 9.1 Add `docs/STATE-LAYOUT.md` (or section in `docs/OPERATIONS.md`) describing:
  - The four data categories and their default paths per mode.
  - The resolution priority (config > env > systemd > XDG > hard fallback).
  - The migration behaviour on first startup.
  - How to manually clean up legacy /tmp artefacts after the migration completes (and how to verify the migration ran via the marker file).
- [ ] 9.2 Update `docs/CONFIG.md` with the new `paths:` block reference.
- [ ] 9.3 Update `docs/DEPLOYMENT.md` if it currently mentions `/tmp/workspaces/...` paths anywhere (it almost certainly does).
- [ ] 9.4 Update `docs/TROUBLESHOOTING.md` with an entry for "audit storm after reboot" pointing operators at the audit-state reload mechanism — if a storm still happens after this change ships, the operator should check that their daemon actually picked up the new paths (look for the migration log line at startup; if absent, the daemon didn't migrate, check `paths:` config or env vars).

## 10. Spec delta

- [ ] 10.1 The ADDED requirement in `openspec/changes/state-paths-out-of-tmp/specs/orchestrator-cli/spec.md` codifies: the directory layout per mode, the resolution-priority order, the migration contract (idempotency, per-entry error tolerance, marker-write rule), the audit-state reload-on-startup obligation, and the systemd unit directives.

## 11. Verification

- [ ] 11.1 `cargo test` passes (new + existing).
- [ ] 11.2 `openspec validate state-paths-out-of-tmp --strict` passes.
- [ ] 11.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
