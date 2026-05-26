## Why

`/tmp` on most Linux server distributions is mounted as `tmpfs` — a RAM-backed filesystem that is wiped on every reboot. autocoder today writes essentially everything to `/tmp`:

- Per-repo cloned workspaces at `/tmp/workspaces/<sanitized-url>/` — tens of MB each, growing with repo size, accumulating across configured repos. Real-world observation: 180+ directories in `/tmp` from this project alone, sitting in RAM.
- Per-change run logs at `/tmp/autocoder/logs/<workspace>/<change>.log`.
- The control socket at `/tmp/autocoder/control/control.sock`.
- (Effectively) audit-cadence state, which is either in-memory only or stored somewhere that doesn't survive daemon restart cleanly — every audit-state surface that isn't reloaded on startup behaves as if "never run before."

The consequences cascade:

1. **RAM pressure.** Repo workspaces are not transient scratch space; they are working trees with full git history. Putting them in tmpfs trades disk usage for RAM usage on a long-running service. A modest 8 GB host with several repos sees most of the RAM-equivalent of its `/tmp` budget spent on git working trees that should be on real disk.
2. **Markers lost on reboot.** Operator-set markers like `.perma-stuck.json` and `.needs-spec-revision.json` live inside the workspace directory. Reboot wipes the workspace; the markers go with it. A change that was correctly perma-stuck before the reboot gets retried after, hitting the same failure all over again.
3. **Failure-state lost on reboot.** Consecutive failure counters (the data behind perma-stuck thresholding) live alongside the workspace. Reboot resets them to zero. A change that was one iteration away from triggering its perma-stuck protection now has its budget reset.
4. **Audit storm after reboot.** The audit framework checks `now - last_run >= cadence_interval` to decide whether to fire an audit. When the audit-state can't be read (file gone from /tmp; in-memory state empty on fresh process), `last_run` defaults to "never," every audit's cadence check passes, every audit fires on the first iteration tick after startup. The chatops channel sees hundreds of `🔍 created proposal` messages within minutes — useful information, drowned in noise.
5. **Partial audit re-fire on daemon restart (no reboot).** Even when /tmp is intact, plain daemon restart re-fires some audits. This is a separate bug: the daemon does not consistently reload existing audit-state from disk on startup. Some audits' state survives the restart (those don't re-fire); others rebuild in-memory from scratch (those do). Different code paths, same outcome class.

The fix is not optional. A daemon that loses operator-meaningful state on reboot is not a daemon, it's a script that pretends. The standard Linux file-system conventions exist precisely for the categories of data autocoder produces, and `systemd` provides built-in machinery (`StateDirectory=`, `CacheDirectory=`, `LogsDirectory=`, `RuntimeDirectory=`) to wire it together.

## What Changes

**Adopt the standard layout, by mode.**

| Data category | Server mode (systemd) | Dev mode (XDG) | Survives reboot? |
|---|---|---|---|
| Persistent state (audit cadence, failure counters, alert throttles, revision state) | `/var/lib/autocoder/` | `${XDG_STATE_HOME:-$HOME/.local/state}/autocoder/` | Yes |
| Repo workspaces (cloned git trees + their in-tree markers) | `/var/cache/autocoder/workspaces/` | `${XDG_CACHE_HOME:-$HOME/.cache}/autocoder/workspaces/` | Yes (re-clonable but kept) |
| Per-change run logs | `/var/log/autocoder/runs/` | `${XDG_STATE_HOME:-$HOME/.local/state}/autocoder/logs/runs/` | Yes (or pruned by operator policy) |
| Runtime (control socket, in-progress pid locks) | `/run/autocoder/` | `${XDG_RUNTIME_DIR:-/tmp/${UID}-runtime}/autocoder/` | No (correct) |

**Workspaces include their markers.** `.perma-stuck.json`, `.needs-spec-revision.json`, `.question.json`, `.answer.json`, `.alert-state.json`, and `.in-progress*` markers continue to live inside the workspace directory tree. Moving workspaces to `/var/cache/autocoder/workspaces/` is sufficient to make the markers survive reboot — no changes to marker storage are needed.

**Audit cadence state, failure counters, and other persistent state move out of the workspace.** These data classes are not per-repo working-tree state; they are daemon-global accounting. They move to dedicated subdirectories under the state dir:

- `/var/lib/autocoder/audit-state/<audit-type>.json` — per-audit-type last-run timestamp, retry history (from `a01-audit-proposal-self-validation`'s attempt_history extension), cadence-resolution cache.
- `/var/lib/autocoder/failure-state/<repo-sanitized>/<change-slug>.json` — consecutive-failure counters per change per repo.
- `/var/lib/autocoder/revisions/<repo-sanitized>/<pr-number>.json` — per-PR revision state (from `a01-pr-comment-revision-loop`).

The split between state-dir state (daemon-global, indexed by repo+change) and workspace-local markers (per-checkout, indexed by change directory) is deliberate: workspace-local markers are operator-visible filesystem artefacts inside the change's directory and survive as-is via the workspace move; state-dir state is daemon-internal accounting that has no business sitting inside a git working tree.

**systemd unit gains the standard directives.** The install scaffold's unit template adds:

```
[Service]
StateDirectory=autocoder
CacheDirectory=autocoder
LogsDirectory=autocoder
RuntimeDirectory=autocoder
```

systemd auto-creates `/var/lib/autocoder/`, `/var/cache/autocoder/`, `/var/log/autocoder/`, and `/run/autocoder/` owned by the service user with mode 0750 (operator-overridable via `*DirectoryMode=` per systemd convention). The daemon reads the `$STATE_DIRECTORY`, `$CACHE_DIRECTORY`, `$LOGS_DIRECTORY`, `$RUNTIME_DIRECTORY` environment variables systemd sets to discover the paths at runtime.

**Path-resolution priority order.** The daemon resolves each path with this precedence:

1. Explicit override in `config.yaml`'s new optional `paths:` block.
2. Environment variable (`AUTOCODER_STATE_DIR`, `AUTOCODER_CACHE_DIR`, `AUTOCODER_LOGS_DIR`, `AUTOCODER_RUNTIME_DIR`) — same names regardless of systemd presence.
3. systemd-provided variable (`$STATE_DIRECTORY`, etc.) when running under systemd.
4. XDG-default (when not under systemd) or `/var/lib/autocoder/` etc. as a hard fallback.

Operators with unusual setups (NFS-mounted `/var/lib`, dedicated partitions for cache, etc.) override via the config or env-var path. The default path produces no surprises.

**Auto-migration from `/tmp` on first startup after upgrade.** On daemon start, BEFORE any normal initialization runs, a migration pass scans for legacy paths and moves data to the new locations. The migration is idempotent and incremental — a partially-completed migration (daemon killed mid-move) resumes safely on the next start.

Migration steps, in order:

1. **Audit state**: if `/tmp/autocoder/audit-state/` (or wherever today's audit state lives) contains any `*.json` AND the new path doesn't yet have those entries, move them.
2. **Failure state**: same pattern for failure-state files.
3. **Other state files** (alert-state, revisions, anything else found at well-known legacy paths): same.
4. **Per-change logs**: if `/tmp/autocoder/logs/` exists, move it to the new logs directory.
5. **Workspaces**: for each entry under `/tmp/workspaces/`, if the new `cache/workspaces/<same-name>/` does NOT exist, move it. `fs::rename` works only within a partition (tmpfs ↔ disk crosses partitions); fall back to recursive copy + delete-on-success when EXDEV is returned.
6. **Migration marker**: write `<state-dir>/.migration-from-tmp-done` after a clean migration pass. Subsequent startups see the marker and skip the migration scan entirely.

If migration ITSELF fails on any individual entry (permissions, disk space), log ERROR per failure but continue with the rest. The daemon does NOT refuse to start — operators can resolve any orphan in `/tmp` manually. The marker is only written if every step completed without error; otherwise the migration retries on the next start.

**Audit-state reload-on-startup.** Independent of the path move: the daemon SHALL read every `*.json` under `<state-dir>/audit-state/` on startup and populate the in-memory audit cadence state from it. Today's behaviour (audits whose state isn't reloaded re-fire on every restart) is a bug. Fix: scan, parse, populate. Audits whose state-file is missing OR corrupt fall back to "never run" (the existing behaviour for first-run); audits whose state-file parses successfully respect their last-run timestamp.

**Config schema addition.** A new optional top-level `paths:` block:

```yaml
paths:
  state_dir: /var/lib/autocoder        # all optional; defaults via the priority order above
  cache_dir: /var/cache/autocoder
  logs_dir: /var/log/autocoder
  runtime_dir: /run/autocoder
```

Each field is optional. Absent or empty `paths:` block means "use defaults per the priority order." Validation at config load: paths must be absolute, parent directories must be writable, no two of them may resolve to the same directory.

**Install wizard updates.** Server-mode install adds the `StateDirectory=` family to the rendered unit. Dev-mode install computes the XDG paths and writes them into `config.yaml`'s `paths:` block so operators see the resolved values explicitly.

**Documentation updates.** A new `docs/STATE-LAYOUT.md` (or section in `docs/OPERATIONS.md`) covers the directory layout, the resolution priority, the migration behaviour, and how to manually clean up legacy `/tmp` artefacts after migration if any failed.

## Impact

- **Affected specs:** `orchestrator-cli` — one ADDED requirement covering the directory layout, resolution-priority, migration contract, audit-state reload, and the systemd-unit additions.
- **Affected code:**
  - New module `autocoder/src/paths.rs` housing the path-resolution logic. Public types: `pub struct DaemonPaths { state, cache, logs, runtime: PathBuf }` and `pub fn resolve_daemon_paths(config: &Config) -> Result<DaemonPaths>` implementing the priority order. Every callsite that today reads or writes under `/tmp/...` goes through `DaemonPaths` instead.
  - `autocoder/src/config.rs` — add the optional `paths:` block to the top-level `Config` struct. Validate at load time.
  - `autocoder/src/workspace.rs` — replace the hard-coded `/tmp/workspaces/` derivation with `daemon_paths.cache.join("workspaces")`. The deterministic-sanitization rule is unchanged.
  - `autocoder/src/control_socket.rs` — replace the hard-coded `/tmp/autocoder/control/control.sock` with `daemon_paths.runtime.join("control.sock")`.
  - `autocoder/src/audits/scheduler.rs` (and friends) — replace audit-state path with `daemon_paths.state.join("audit-state").join(audit_type).with_extension("json")`. Add a startup `reload_all_state` function that scans the audit-state dir on daemon start and populates the in-memory map.
  - `autocoder/src/failure_state.rs` — replace failure-state path with `daemon_paths.state.join("failure-state").join(repo_sanitized).join(change_slug).with_extension("json")`.
  - `autocoder/src/revisions.rs` (from `a01-pr-comment-revision-loop`) — replace the per-PR revision state path with `daemon_paths.state.join("revisions").join(repo_sanitized).join(pr_number).with_extension("json")`.
  - `autocoder/src/cli/run.rs` — at daemon start, BEFORE any normal initialization (including config validation that doesn't depend on paths), call `migrate_legacy_tmp_paths(daemon_paths)`. Log the outcome.
  - New module `autocoder/src/migration.rs` housing the legacy-path scan + move logic, with `fs::rename`-then-copy-fallback for cross-partition moves.
  - `autocoder/src/cli/install_systemd.service` (the unit template) — add `StateDirectory=autocoder`, `CacheDirectory=autocoder`, `LogsDirectory=autocoder`, `RuntimeDirectory=autocoder` under `[Service]`.
  - `autocoder/src/cli/install.rs` — server-mode rendering picks up the new unit directives automatically; dev-mode rendering computes XDG paths and writes them into the generated `config.yaml`.
  - Tests:
    - `resolve_daemon_paths` priority-order tests: config-explicit wins; env-var wins over systemd; systemd wins over XDG; XDG wins over hard fallback.
    - Validation tests: relative path rejected; same path for two roles rejected; non-writable parent rejected (mock the writability check or use tempfile-permissions).
    - Migration tests: legacy paths empty → no migration; legacy paths populated → moves succeed; partial migration (some files succeed, some fail) → marker not written, next run retries; cross-partition move falls back to copy-and-delete; idempotency (running migration twice is a no-op after first success).
    - Audit-state reload tests: empty state dir → in-memory map is empty (every audit treats as first-run); state dir with valid files → in-memory map matches contents; state dir with one corrupt file → that audit treats as first-run, every other audit reloads correctly.
    - End-to-end test with a temp-dir as state/cache/logs/runtime: full daemon start, do some work, restart daemon, assert no audit re-fires (last-run timestamps respected) AND assert markers / failure-state survive.

- **Operator-visible behavior:** post-upgrade, the daemon migrates state from /tmp to /var (or XDG paths) on first start. Subsequent reboots no longer cause audit storms, marker resets, or failure-counter resets. RAM usage drops by the size of all workspaces (potentially hundreds of MB per host). The control socket and a few transient files remain in /run (correct: those should not survive reboot).
- **Breaking:** no for operators on systemd. The install wizard's rendered unit gains the `*Directory=` directives; existing unit files (operator-customized or not) keep working until next install/upgrade. The daemon checks both new AND legacy paths during the migration window; once migrated, only the new paths are touched. Operators with custom configurations who set `local_path` per-repo continue to use those paths as today — `local_path` is an explicit operator choice and is preserved verbatim (no migration applied).
- **Acceptance:** `cargo test` passes (new + existing). A daemon upgraded from a /tmp-based deploy migrates every legacy path on first start; the migration is logged with per-entry outcomes; subsequent restarts skip the migration scan. After a host reboot, the daemon resumes operation without re-firing any audit whose cadence has not elapsed, without losing any operator marker, and without resetting any failure counter.
