## 1. New busy_marker module

- [x] 1.1 Create `autocoder/src/busy_marker.rs`. Public types:
    ```rust
    pub enum Stage { Executor, Commit, Review, Push, Pr }
    pub struct BusyMarker { /* path, contents */ }
    pub struct BusyGuard { /* path; Drop deletes */ }
    pub enum AcquireOutcome {
        Acquired(BusyGuard),                  // proceed with iteration
        SkipFreshInProgress(BusyMarker),      // marker present, age < threshold
        SkipAmbiguous(BusyMarker),            // PID reuse detected
    }
    ```
- [x] 1.2 Implement `pub fn marker_path(workspace_basename: &str) -> PathBuf` returning `/tmp/autocoder/busy/<basename>.json` via `std::env::temp_dir().join("autocoder").join("busy").join(format!("{basename}.json"))`.
- [x] 1.3 Implement `pub fn try_acquire(workspace: &Path, repo_url: &str, stuck_threshold_secs: u64) -> Result<AcquireOutcome>`:
    1. Compute `marker_path` from workspace's basename.
    2. Ensure parent dirs exist (`create_dir_all`).
    3. Build `MarkerContents` with `repo_url`, `pid = std::process::id()`, `pgid` (use `nix::unistd::getpgrp()` or `libc::getpgrp()`), `comm` (read from `/proc/{pid}/comm` on Linux, `String::new()` elsewhere), `started_at = chrono::Utc::now()`, `stage = "executor"`.
    4. Attempt atomic create via `OpenOptions::new().write(true).create_new(true).open(path)`. On success, write the JSON and return `Acquired(BusyGuard)`.
    5. On `ErrorKind::AlreadyExists`: read the existing file. Parse JSON. If parse fails, log WARN, delete the file, recurse to step 4 once (so a malformed marker doesn't loop). If parse succeeds, classify:
        - Age via `(now - started_at).num_seconds()`. Negative ages (clock skew, started_at in the future) are treated as `0`.
        - If age < stuck_threshold_secs: return `SkipFreshInProgress(parsed)`.
        - Age >= threshold: check liveness via `kill(pid, 0)` returning `ESRCH` (use `nix::sys::signal::kill` or raw `libc::kill`). If dead: delete file, log WARN, recurse to step 4.
        - PID alive: if Linux AND `parsed.comm` is non-empty AND current `/proc/{pid}/comm` differs: return `SkipAmbiguous(parsed)`.
        - PID alive AND comm matches (or non-Linux): kill PGID with SIGTERM, wait up to 5s for it to exit (poll `kill(pid,0)` every 200ms), SIGKILL if still alive, delete file, log WARN naming the action, recurse to step 4.
- [x] 1.4 Implement `BusyGuard::set_stage(&self, stage: Stage)` that rewrites the JSON via atomic write-temp-then-rename (`{path}.tmp` → `rename(.tmp, path)`).
- [x] 1.5 Implement `Drop for BusyGuard` that calls `std::fs::remove_file(&self.path)` with a `tracing::warn!` if it errors (best-effort).
- [x] 1.6 **Verify:** unit tests `busy_marker::tests::*`:
    - `acquire_on_clean_returns_acquired` — empty dir, acquire succeeds, file exists, Drop releases.
    - `acquire_when_fresh_returns_skip_fresh` — pre-populate marker with `started_at = now`, acquire returns `SkipFreshInProgress`.
    - `acquire_when_stale_dead_pid_recovers` — pre-populate marker with `pid = 1` (init; or a synthetic dead pid like 999999), `started_at = 1 hour ago`. Acquire deletes and re-acquires successfully.
    - `acquire_when_malformed_recovers` — pre-populate with garbage text, acquire deletes and re-acquires.
    - `set_stage_persists_atomically` — acquire, set stage to Commit, read file, assert stage is "commit".
    - `guard_drop_removes_file` — acquire, drop guard explicitly, assert file gone.
    - Mock-out the comm-check and kill calls behind a small trait so tests can simulate "PID alive but comm differs" without spawning real processes. The trait has one default-prod impl using real syscalls and one test impl with injectable answers.

## 2. Executor: process-group launch

- [x] 2.1 In `claude_cli::run_subprocess`, after constructing the `Command` and before `.spawn()`, add `command.process_group(0)` (stable since Rust 1.64) so the child becomes a new process-group leader. This lets the busy-marker code `killpg` the whole subprocess tree.
- [x] 2.2 **Verify:** existing executor tests continue to pass — `process_group(0)` doesn't change exit behavior, only group membership. No new test needed for the group setup itself (POSIX semantics are stable); the stuck-state-kill test in §1.6 covers the consuming behavior.

## 3. Run-log path unification

- [x] 3.1 In `claude_cli.rs`, modify `run_log_path` to return `<temp>/autocoder/logs/<basename>/<change>.log` instead of `<temp>/autocoder-logs/<basename>/<change>.log`.
- [x] 3.2 Update the test `run_log_path_is_under_workspace_basename_and_change_name` to assert the new path layout.
- [x] 3.3 No backwards-compat shim. Old logs at `/tmp/autocoder-logs/` become unreferenced; they'll age out via `/tmp` cleanup. README documents the new path.

## 4. Polling-loop integration

- [x] 4.1 At the top of `polling_loop::execute_one_pass`, after workspace path resolution but before any other work, call `busy_marker::try_acquire(workspace, &repo.url, repo.executor_timeout_secs + 600)`. Return early on `SkipFreshInProgress` (INFO log) and `SkipAmbiguous` (ERROR log + chatops alert). The threshold uses `cfg.executor.timeout_secs` reached via the executor config — the polling-loop signature may need a tweak to thread this value through.
- [x] 4.2 On `Acquired(guard)`, bind the guard to a local variable for the duration of the function. Drop happens automatically at every return path.
- [x] 4.3 At each stage transition (before the executor, after walk_queue, before review, before push, before PR), call `guard.set_stage(...)`. This requires holding a `&BusyGuard` accessible to the right code paths — pass through `execute_one_pass`'s helpers as needed.
- [x] 4.4 For the chatops alerts on stuck/ambiguous states: reuse the `handle_predictable_failure` helper introduced by `chatops-progress-notifications` once that change re-lands, OR if it has not re-landed yet, write a small inline `post_notification` call gated on `chatops_ctx.is_some()`. Mark the chatops integration as a soft dependency in the proposal.
- [x] 4.5 **Verify:** integration tests in `polling_loop::tests`:
    - `busy_marker_acquired_for_clean_iteration` — fixture pass, assert marker file existed during the pass and is gone after.
    - `iteration_skipped_when_fresh_marker_exists` — pre-write a fresh marker, run a pass with a `MustNotRunExecutor`, assert no executor invocation.
    - `iteration_recovers_when_stale_marker_has_dead_pid` — pre-write a stale marker with PID 999999, run a pass, assert marker is gone after AND the executor ran.
    - `iteration_skipped_on_ambiguous_marker` — pre-write a stale marker whose comm doesn't match a live PID (use the test trait from §1.6 to inject), assert executor NOT invoked AND marker file is unchanged.

## 5. Documentation

- [x] 5.1 README "Operating Notes": new subsection "Busy marker" describing the file's purpose, location, contents, lifecycle, and what an operator should do if they see a stuck marker. No kitschy framing — declarative.
- [x] 5.2 README "Operating Notes": update the existing workspace-recovery subsection to mention that the run-log path is now `/tmp/autocoder/logs/...` not `/tmp/autocoder-logs/...`.
- [x] 5.3 README "Operating Notes": brief note that operators can `cat /tmp/autocoder/busy/<basename>.json` to see what stage the daemon is in for any active repo.

## 6. Verification

- [x] 6.1 `cargo test` passes.
- [x] 6.2 `openspec validate per-repo-busy-marker --strict` passes.
- [x] 6.3 `cargo build --release` produces a binary that, on a clean `/tmp`, runs a polling pass that creates `/tmp/autocoder/busy/<basename>.json`, holds it through the pass, and removes it on completion. Verifiable by `watch ls /tmp/autocoder/busy/` during a manual run.
