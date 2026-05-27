## Why

Concurrent-execution exposure: two daemon instances accidentally running (manual `autocoder run` while systemd unit is active), daemon-crash-mid-iteration leaving orphaned Claude subprocesses still writing to the workspace, or sub-stage races between the executor and the code-reviewer (reviewer's HTTP call to Grok could fire while Claude is still scribbling on the workspace, if iteration timing aligned just so). The `.in-progress` lock at the per-change level doesn't prevent any of these — it lives inside the workspace, is cleared as "stale" on each daemon startup, and doesn't survive across process boundaries.

A per-repo busy marker outside the workspace addresses all three scenarios in one mechanism and supplies operators with a diagnostic artifact they can inspect when something is wrong.

## What Changes

- **ADDED capability:** `orchestrator-cli` SHALL acquire a per-repo busy marker at the start of each polling iteration and hold it through the entire pass (executor → commit → review → push → PR). The marker is a JSON file outside the workspace; its presence prevents any other autocoder pass from working on the same repo.
- **Path layout:** unify the existing `/tmp/autocoder-logs/` and the new busy marker under a single `/tmp/autocoder/` root. New paths:
  - Busy marker: `/tmp/autocoder/busy/<workspace-basename>.json`
  - Run logs (migrated): `/tmp/autocoder/logs/<workspace-basename>/<change>.log`
- **Marker contents:** JSON with `repo_url`, `pid`, `pgid`, `comm` (process name from `/proc/<pid>/comm` at acquire time), `started_at` (RFC 3339), `stage` (one of: `executor`, `commit`, `review`, `push`, `pr`). Atomic stage transitions via write-temp-then-rename.
- **Acquire/release:**
  - Acquire at top of `execute_one_pass` via `OpenOptions::create_new(true)` (POSIX O_EXCL) — atomic against concurrent daemons.
  - Held via RAII guard so any normal return path (success or error) releases.
  - Crashes that bypass Drop (SIGKILL, segfault, host power loss) leave the marker for the next pass to detect.
- **Stale-state detection at acquire time** (`O_EXCL` returned `EEXIST`):
  | Marker state | Action |
  |---|---|
  | Age < threshold | Skip iteration, log INFO. Healthy "in-progress" case. |
  | Age > threshold AND PID dead | Auto-recover: clear marker, log WARN, proceed. |
  | Age > threshold AND PID alive AND `/proc/<pid>/comm` matches recorded `comm` | Stuck: kill PGID (SIGTERM, wait 5s, SIGKILL if still alive), clear marker, log WARN, send chatops alert "repo recovered from stuck state", proceed. |
  | Age > threshold AND PID alive AND comm differs | PID reuse — ambiguous. Log ERROR, send chatops alert "repo stuck — please investigate", SKIP this iteration, leave marker. |
  | Malformed JSON | Treat as stale: log WARN, clear marker, proceed. |
- **Threshold:** `executor.timeout_secs + 600` (10-minute buffer for review/push/PR).
- **Chatops fallback:** if `slack:` block is absent or post fails, the WARN/ERROR log line is sufficient — the recovery decision does NOT depend on chatops availability. Only the notification does.
- **Run-log path migration:** the existing `/tmp/autocoder-logs/<basename>/<change>.log` becomes `/tmp/autocoder/logs/<basename>/<change>.log`. Single-line code change. README updated. No backwards-compat shim (logs are diagnostic; old logs become unreferenced files in `/tmp/autocoder-logs/` which `/tmp` cleanup eventually clears).

## Impact

- Affected specs: `orchestrator-cli` (ADDED requirement for busy marker), `executor` (MODIFIED scenario for run-log path).
- Affected code: `autocoder/src/polling_loop.rs` (acquire/release/stale-detection), new `autocoder/src/busy_marker.rs` module, `autocoder/src/executor/claude_cli.rs` (run-log path constant).
- New filesystem footprint: one JSON file per active repo at `/tmp/autocoder/busy/`. Cleaned on every successful pass.
- Process-group kill on Linux requires the spawned Claude to be its own process group. The current spawn doesn't explicitly `setsid()` — we'll add a `pre_exec` hook via `std::os::unix::process::CommandExt::pre_exec` so the child becomes a session leader and we can `killpg(pgid, SIGTERM)`.
- Breaking? No, but operators with custom monitoring that grepped the old `/tmp/autocoder-logs/` path need to update to `/tmp/autocoder/logs/`.
