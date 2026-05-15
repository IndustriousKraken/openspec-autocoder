## Why

The busy marker's `pgid` field is autocoder's own process group (recorded via `getpgrp()` at acquire time), not the spawned Claude's. Because the executor sets `process_group(0)` on the child, Claude runs in its own pgid (= Claude's pid). For stuck-state recovery, `killpg(autocoder_pgid)` does not affect orphaned Claude trees — but Claude is exactly what an orphan-cleanup needs to terminate.

Observed in production: `ps -s 79884 -o pid,pgid,etime,cmd` showed autocoder at PGID 79884 and Claude at PGID 93720 (with its MCP server also in 93720). The marker recorded 79884, but the correct kill target is 93720.

## What Changes

- **MODIFIED capability:** `executor` — `ClaudeCliExecutor::run_subprocess` SHALL record the spawned child's PID to a sidecar file alongside the busy marker so stuck-state recovery can target the right process group. The file is removed on normal subprocess exit (RAII guard); a daemon crash leaves it for the next pass to find.
- **MODIFIED capability:** `orchestrator-cli` — the busy-marker stuck-state recovery path SHALL prefer the sidecar PGID (= subprocess PID, since the subprocess is its own group leader) over the marker's `pgid` field when killing a stuck process tree. If the sidecar is absent, the recorded `pgid` is the fallback.
- **Path:** `<system-temp>/autocoder/busy/<workspace-basename>.subprocess` (plain text, contains the subprocess PID as decimal digits + newline).
- **Code:**
  - `busy_marker::subprocess_marker_path(workspace) -> PathBuf` — new helper.
  - `busy_marker::write_subprocess_marker(workspace, pid) -> Result<()>` — atomic write (temp + rename).
  - `busy_marker::read_subprocess_marker(workspace) -> Option<i32>` — read on stuck recovery.
  - `busy_marker::remove_subprocess_marker(workspace)` — best-effort cleanup.
  - `SubprocessMarkerGuard` (RAII) inside `claude_cli.rs` — created after `child.spawn()` succeeds, removed on Drop.
  - Stuck-recovery `killpg` logic reads the sidecar first; falls back to the marker's recorded pgid if absent.
- **Tests:**
  - `busy_marker::tests::stuck_recovery_uses_sidecar_pgid_when_present` — pre-populate marker + sidecar with distinct pgids; assert killpg targets the sidecar's pgid.
  - `busy_marker::tests::stuck_recovery_falls_back_to_marker_pgid_when_no_sidecar` — pre-populate marker only; assert killpg targets marker's pgid.

## Impact

- Affected specs: `executor`, `orchestrator-cli`.
- Affected code: `autocoder/src/busy_marker.rs`, `autocoder/src/executor/claude_cli.rs`.
- No new dependencies. No new config knobs. Backward-compatible with existing markers (sidecar is optional; absence means "no subprocess to kill" or "old version of autocoder wrote the marker").
- One new file on disk per active subprocess. Cleaned automatically on normal exit; survives crashes for next-pass recovery.
