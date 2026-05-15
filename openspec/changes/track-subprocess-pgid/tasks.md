## 1. busy_marker: sidecar path + read/write/remove helpers

- [x] 1.1 Add `pub fn subprocess_marker_path(workspace: &Path) -> PathBuf` to `busy_marker.rs`. Returns `<system-temp>/autocoder/busy/<workspace-basename>.subprocess`. Mirror the existing `marker_path` derivation.
- [x] 1.2 Add `pub fn write_subprocess_marker(workspace: &Path, pid: u32) -> Result<()>`. Implementation: ensure parent dir exists; write `format!("{pid}\n")` atomically via temp-file-then-rename. The `pid` is the spawned child's PID; the corresponding PGID is the same value because the executor spawns with `process_group(0)` (Claude is its own group leader).
- [x] 1.3 Add `pub fn read_subprocess_marker(workspace: &Path) -> Option<i32>`. Reads the file, parses the first whitespace-stripped line as i32. Returns `None` on any error (file absent, parse failure) — best-effort, never propagates errors.
- [x] 1.4 Add `pub fn remove_subprocess_marker(workspace: &Path)`. Best-effort `remove_file`; silent on `NotFound`, WARN-logs other errors.

## 2. busy_marker: stuck-recovery uses sidecar PGID

- [x] 2.1 In `try_acquire_with`'s "stuck threshold exceeded, PID alive, comm matches" branch, BEFORE the `ops.killpg_terminate(existing.pgid)` call: read `read_subprocess_marker(workspace)`. Bind the result to `target_pgid`. If `Some(p)`, use `p`; if `None`, fall back to `existing.pgid` (current behavior).
- [x] 2.2 Pass `target_pgid` (not `existing.pgid`) to `ops.killpg_terminate`, the `ops.wait_for_exit` (use the subprocess PID, not the marker's), and `ops.killpg_kill`.
- [x] 2.3 After the kill sequence (success or otherwise), call `remove_subprocess_marker(workspace)` so the next iteration sees a clean slate.
- [x] 2.4 In the "stuck threshold exceeded, PID dead" branch: also call `remove_subprocess_marker(workspace)` along with the existing marker-file removal — keeps the sibling files consistent.
- [x] 2.5 In the "malformed JSON" branch: same — remove the subprocess sidecar too.

## 3. claude_cli: write sidecar after spawn, RAII-cleanup on exit

- [x] 3.1 In `claude_cli::run_subprocess`, immediately after `child.spawn()` succeeds AND immediately after `child.id()` returns the PID: call `busy_marker::write_subprocess_marker(workspace, pid)`. Log WARN on error but do NOT fail the run — the sidecar is diagnostic and recovery-related, not load-bearing for the executor's normal path.
- [x] 3.2 Wrap the cleanup in a new RAII guard pattern: define `struct SubprocessMarkerGuard { workspace: PathBuf }` whose `Drop` calls `busy_marker::remove_subprocess_marker(&self.workspace)`. Construct the guard immediately after the successful `write_subprocess_marker` call so any subsequent return path (timeout, error, success) cleans up.
- [x] 3.3 The existing `TempFileGuard` for the sandbox settings file already drops correctly; the new guard follows the same pattern. Both are scoped to `run_subprocess` so cleanup happens before the function returns.

## 4. Tests

- [x] 4.1 `busy_marker::tests::stuck_recovery_uses_sidecar_pgid_when_present` — pre-populate marker with `pgid: 1234` AND pre-write a sidecar file at `subprocess_marker_path(...)` containing `5678`. Use `MockOps` that records `killpg_terminate` calls. Acquire. Assert `killpg_terminate` was called with `5678` (the sidecar's pgid), NOT `1234` (the marker's). Assert the marker and sidecar are both removed after recovery.
- [x] 4.2 `busy_marker::tests::stuck_recovery_falls_back_to_marker_pgid_when_no_sidecar` — pre-populate marker with `pgid: 1234`. No sidecar. Acquire. Assert `killpg_terminate` was called with `1234`. Backward-compat path.
- [x] 4.3 `busy_marker::tests::write_and_read_subprocess_marker_roundtrip` — write_subprocess_marker(ws, 99). Assert `read_subprocess_marker(ws) == Some(99)`. Then `remove_subprocess_marker(ws)`. Assert `read_subprocess_marker(ws) == None`.
- [x] 4.4 `busy_marker::tests::read_subprocess_marker_returns_none_on_garbage` — write `"not a number"` to the sidecar path. Assert `read_subprocess_marker` returns None without panicking.
- [x] 4.5 `busy_marker::tests::stale_dead_pid_also_removes_sidecar` — pre-populate stale marker (dead pid) + sidecar. Acquire. Assert both the marker AND the sidecar are gone afterward.
- [x] 4.6 **Verify:** existing busy_marker tests continue to pass. The `acquire_when_stuck_kills_pgid_and_recovers` test asserts kill targets — with this change, if there is no sidecar pre-populated, behavior is unchanged (falls back to marker pgid). Update the assertion comment to reflect the new precedence rule but no behavioral change is required.

## 5. Verification

- [x] 5.1 `cargo test` passes; net new tests = at least 4.
- [x] 5.2 `openspec validate track-subprocess-pgid --strict` passes.
- [ ] 5.3 Operator verification on a running deployment: with the deploy live, inspect during a Claude run:
    ```
    cat /tmp/autocoder/busy/<basename>.subprocess
    ```
    The integer printed must match the actual Claude PID from `ps --ppid <autocoder-pid>`. After the run completes, the file must be gone.
