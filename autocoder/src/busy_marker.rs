//! Per-repo busy marker. A small JSON file outside the workspace whose
//! presence signals "an autocoder pass is currently working on this repo;
//! no other pass should start." Survives across daemon crashes (the file
//! is not deleted unless its RAII guard drops, so SIGKILL or segfault
//! leaves it for the next daemon start to discover and recover from).
//!
//! Path layout: `<system-temp>/autocoder/busy/<workspace-basename>.json`.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Coarse lifecycle stages of a polling pass, recorded in the marker so an
/// operator inspecting a stuck file knows what step the daemon was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Executor,
    Commit,
    Review,
    Push,
    Pr,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Executor => "executor",
            Stage::Commit => "commit",
            Stage::Review => "review",
            Stage::Push => "push",
            Stage::Pr => "pr",
        }
    }
}

/// Persisted marker contents. Read back when the next daemon pass finds an
/// existing file and needs to classify its state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusyMarker {
    pub repo_url: String,
    pub pid: u32,
    pub pgid: i32,
    pub comm: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub stage: Stage,
}

/// Outcome of `try_acquire`. Acquired means the caller owns the marker and
/// should proceed with the iteration. The other variants name the failure
/// mode: a fresh marker (someone else is working), or an ambiguous one
/// (PID is alive but appears to be unrelated to autocoder — operator
/// should investigate).
pub enum AcquireOutcome {
    Acquired(BusyGuard),
    SkipFreshInProgress(BusyMarker),
    SkipAmbiguous(BusyMarker),
}

/// RAII handle. On Drop, the marker file is deleted. The guard is the only
/// way to release a marker on a normal return path; crashes that bypass
/// Drop intentionally leave the file behind for stale-state detection.
pub struct BusyGuard {
    path: PathBuf,
    contents: BusyMarker,
}

impl BusyGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Update the `stage` field via atomic write-temp-then-rename. The
    /// rename is POSIX-atomic against concurrent readers — they see either
    /// the prior stage or the new one, never a partial write.
    pub fn set_stage(&mut self, stage: Stage) -> Result<()> {
        self.contents.stage = stage;
        write_atomic(&self.path, &self.contents)
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.path.display(),
                    "busy_marker: failed to remove file on Drop: {e}"
                );
            }
        }
    }
}

/// Compute the busy-marker path for the given workspace.
/// `<system-temp>/autocoder/busy/<workspace-basename>.json`.
pub fn marker_path(workspace: &Path) -> PathBuf {
    let basename = workspace
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    std::env::temp_dir()
        .join("autocoder")
        .join("busy")
        .join(format!("{basename}.json"))
}

/// Compute the subprocess-sidecar path for the given workspace.
/// `<system-temp>/autocoder/busy/<workspace-basename>.subprocess`. The file
/// holds the spawned subprocess's PID (= PGID, since the executor spawns
/// with `process_group(0)`) so stuck-state recovery can target the right
/// process group when the daemon's own pgid does not cover orphaned
/// children.
pub fn subprocess_marker_path(workspace: &Path) -> PathBuf {
    let basename = workspace
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    std::env::temp_dir()
        .join("autocoder")
        .join("busy")
        .join(format!("{basename}.subprocess"))
}

/// Atomically record `pid` to the subprocess-sidecar file for `workspace`.
/// Writes via temp-file-then-rename so concurrent readers never see a
/// partial value.
pub fn write_subprocess_marker(workspace: &Path, pid: u32) -> Result<()> {
    let path = subprocess_marker_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating subprocess-marker dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("subprocess.tmp");
    std::fs::write(&tmp, format!("{pid}\n"))
        .with_context(|| format!("writing subprocess-marker temp {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| {
        format!(
            "renaming subprocess-marker {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Best-effort read of the subprocess-sidecar file. Returns `None` if the
/// file is absent, unreadable, or fails to parse — recovery never
/// propagates errors out of this read because the sidecar is diagnostic.
pub fn read_subprocess_marker(workspace: &Path) -> Option<i32> {
    let path = subprocess_marker_path(workspace);
    let raw = std::fs::read_to_string(&path).ok()?;
    let first = raw.split_whitespace().next()?;
    first.parse::<i32>().ok()
}

/// Best-effort removal of the subprocess-sidecar file. Silent on
/// `NotFound`; WARN-logs other errors so recovery can continue.
pub fn remove_subprocess_marker(workspace: &Path) {
    let path = subprocess_marker_path(workspace);
    if let Err(e) = std::fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                "busy_marker: failed to remove subprocess marker: {e}"
            );
        }
    }
}

/// Try to acquire the busy marker for the given workspace.
///
/// `stuck_threshold_secs` is the age above which an existing marker is
/// treated as potentially stuck (process died mid-pass or is hanging).
/// In production this is `executor.timeout_secs + 600` (10-minute buffer
/// for review/push/PR steps).
pub fn try_acquire(
    workspace: &Path,
    repo_url: &str,
    stuck_threshold_secs: u64,
) -> Result<AcquireOutcome> {
    try_acquire_with(
        workspace,
        repo_url,
        stuck_threshold_secs,
        &RealProcessOps,
    )
}

/// Test-injectable acquire. `ops` lets unit tests simulate "PID alive vs
/// dead" and "comm matches vs differs" without spawning real processes.
pub fn try_acquire_with(
    workspace: &Path,
    repo_url: &str,
    stuck_threshold_secs: u64,
    ops: &dyn ProcessOps,
) -> Result<AcquireOutcome> {
    let path = marker_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating busy-marker dir {}", parent.display()))?;
    }

    // Single retry: if we detect a stale/malformed marker and clear it,
    // attempt to create the file once more. Without the bound, a pathological
    // race could in theory loop, but the `clear-then-recurse` model is
    // bounded by "we just deleted the file, only we should now create it".
    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                let pid = std::process::id();
                let pgid = unsafe { libc::getpgrp() };
                let comm = read_comm(pid);
                let marker = BusyMarker {
                    repo_url: repo_url.to_string(),
                    pid,
                    pgid,
                    comm,
                    started_at: chrono::Utc::now(),
                    stage: Stage::Executor,
                };
                drop(file); // we'll use write_atomic for the actual content
                write_atomic(&path, &marker)?;
                return Ok(AcquireOutcome::Acquired(BusyGuard {
                    path,
                    contents: marker,
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = match read_marker(&path) {
                    Ok(m) => m,
                    Err(parse_err) => {
                        tracing::warn!(
                            path = %path.display(),
                            "busy_marker: existing file is malformed ({parse_err:#}); deleting and retrying"
                        );
                        let _ = std::fs::remove_file(&path);
                        remove_subprocess_marker(workspace);
                        if attempt == 0 {
                            continue;
                        }
                        return Err(anyhow!(
                            "busy_marker: could not acquire after clearing malformed file at {}",
                            path.display()
                        ));
                    }
                };

                let age_secs = age_secs(&existing.started_at);
                if age_secs < stuck_threshold_secs {
                    return Ok(AcquireOutcome::SkipFreshInProgress(existing));
                }

                // Stale: classify.
                if !ops.pid_alive(existing.pid) {
                    tracing::warn!(
                        path = %path.display(),
                        pid = existing.pid,
                        age_secs,
                        stage = %existing.stage.as_str(),
                        "busy_marker: stale (PID dead); clearing and acquiring"
                    );
                    let _ = std::fs::remove_file(&path);
                    remove_subprocess_marker(workspace);
                    if attempt == 0 {
                        continue;
                    }
                    return Err(anyhow!(
                        "busy_marker: could not acquire after clearing stale-dead file at {}",
                        path.display()
                    ));
                }

                // PID alive. Check comm to defeat PID reuse.
                let live_comm = ops.read_comm(existing.pid);
                let comm_check_skipped = existing.comm.is_empty() || live_comm.is_none();
                let comm_matches = if let Some(live) = live_comm.as_deref() {
                    !existing.comm.is_empty() && live == existing.comm
                } else {
                    false
                };

                if !comm_check_skipped && !comm_matches {
                    tracing::error!(
                        path = %path.display(),
                        pid = existing.pid,
                        recorded_comm = %existing.comm,
                        live_comm = ?live_comm,
                        age_secs,
                        "busy_marker: stuck-ambiguous (PID reuse suspected); requires investigation"
                    );
                    return Ok(AcquireOutcome::SkipAmbiguous(existing));
                }

                // PID alive and (comm matches OR comm-check unavailable):
                // assume it's our own stuck process. Kill the group and
                // clear the file.
                //
                // Precedence: prefer the subprocess sidecar's PGID over
                // the marker's `pgid` field. The marker records
                // autocoder's *own* process group (via `getpgrp()` at
                // acquire time), but the kill target an orphan-cleanup
                // needs is the spawned subprocess (Claude), which lives
                // in its own group (= its PID, since the executor
                // spawns with `process_group(0)`). The sidecar carries
                // that PID. Fall back to the marker's `pgid` only when
                // no sidecar exists (older daemon, or the subprocess
                // never started).
                let sidecar_pid = read_subprocess_marker(workspace);
                let target_pgid = sidecar_pid.unwrap_or(existing.pgid);
                let wait_pid: u32 = match sidecar_pid {
                    Some(p) if p > 0 => p as u32,
                    _ => existing.pid,
                };
                tracing::warn!(
                    path = %path.display(),
                    pid = existing.pid,
                    marker_pgid = existing.pgid,
                    sidecar_pgid = ?sidecar_pid,
                    target_pgid,
                    age_secs,
                    stage = %existing.stage.as_str(),
                    "busy_marker: stuck (PID alive past threshold); killing process group and clearing"
                );
                ops.killpg_terminate(target_pgid);
                // SIGKILL fallback after a short grace window. Wait on
                // the actual subprocess PID (sidecar) when present so
                // we observe the orphaned tree's exit, not autocoder's.
                ops.wait_for_exit(wait_pid, Duration::from_secs(5));
                if ops.pid_alive(wait_pid) {
                    ops.killpg_kill(target_pgid);
                }
                let _ = std::fs::remove_file(&path);
                remove_subprocess_marker(workspace);
                if attempt == 0 {
                    continue;
                }
                return Err(anyhow!(
                    "busy_marker: could not acquire after killing stuck process at {}",
                    path.display()
                ));
            }
            Err(e) => {
                return Err(anyhow!(
                    "busy_marker: opening {} failed: {e}",
                    path.display()
                ));
            }
        }
    }
    unreachable!("loop exits via return in all branches");
}

fn read_marker(path: &Path) -> Result<BusyMarker> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading busy marker {}", path.display()))?;
    let m: BusyMarker = serde_json::from_str(&raw)
        .with_context(|| format!("parsing busy marker {}", path.display()))?;
    Ok(m)
}

fn write_atomic(path: &Path, marker: &BusyMarker) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(marker)
        .context("serializing busy marker")?;
    std::fs::write(&tmp, body)
        .with_context(|| format!("writing busy marker temp {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming busy marker {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn age_secs(started_at: &chrono::DateTime<chrono::Utc>) -> u64 {
    let delta = chrono::Utc::now().signed_duration_since(*started_at);
    if delta.num_seconds() < 0 {
        // Clock skew (started_at in the future) — treat as "just now".
        0
    } else {
        delta.num_seconds() as u64
    }
}

fn read_comm(pid: u32) -> String {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    }
}

/// Abstraction over the platform syscalls used by `try_acquire`. Production
/// uses `RealProcessOps`; tests inject `MockProcessOps` so they can
/// simulate "PID alive / comm matches / etc." without spawning processes.
pub trait ProcessOps {
    fn pid_alive(&self, pid: u32) -> bool;
    fn read_comm(&self, pid: u32) -> Option<String>;
    fn killpg_terminate(&self, pgid: i32);
    fn killpg_kill(&self, pgid: i32);
    fn wait_for_exit(&self, pid: u32, max: Duration);
}

pub struct RealProcessOps;

impl ProcessOps for RealProcessOps {
    fn pid_alive(&self, pid: u32) -> bool {
        // `kill(pid, 0)` returns 0 if the process exists OR if we have
        // permission to signal it. ESRCH means the PID does not exist.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() != Some(libc::ESRCH)
    }

    fn read_comm(&self, pid: u32) -> Option<String> {
        if cfg!(target_os = "linux") {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|s| s.trim().to_string())
                .ok()
        } else {
            None
        }
    }

    fn killpg_terminate(&self, pgid: i32) {
        let rc = unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGTERM) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            tracing::warn!(pgid, "busy_marker: SIGTERM to process group failed: {err}");
        }
    }

    fn killpg_kill(&self, pgid: i32) {
        let rc = unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            tracing::warn!(pgid, "busy_marker: SIGKILL to process group failed: {err}");
        }
    }

    fn wait_for_exit(&self, pid: u32, max: Duration) {
        let start = std::time::Instant::now();
        while start.elapsed() < max {
            if !self.pid_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Per-test mock that records calls and answers configurable predicates.
    struct MockOps {
        pid_alive_for: Vec<u32>,
        comms: std::collections::HashMap<u32, String>,
        killpg_terminate_called: Mutex<Vec<i32>>,
        killpg_kill_called: Mutex<Vec<i32>>,
    }

    impl MockOps {
        fn new() -> Self {
            Self {
                pid_alive_for: Vec::new(),
                comms: std::collections::HashMap::new(),
                killpg_terminate_called: Mutex::new(Vec::new()),
                killpg_kill_called: Mutex::new(Vec::new()),
            }
        }
        fn with_alive(mut self, pid: u32) -> Self {
            self.pid_alive_for.push(pid);
            self
        }
        fn with_comm(mut self, pid: u32, comm: &str) -> Self {
            self.comms.insert(pid, comm.to_string());
            self
        }
    }

    impl ProcessOps for MockOps {
        fn pid_alive(&self, pid: u32) -> bool {
            self.pid_alive_for.contains(&pid)
        }
        fn read_comm(&self, pid: u32) -> Option<String> {
            self.comms.get(&pid).cloned()
        }
        fn killpg_terminate(&self, pgid: i32) {
            self.killpg_terminate_called.lock().unwrap().push(pgid);
        }
        fn killpg_kill(&self, pgid: i32) {
            self.killpg_kill_called.lock().unwrap().push(pgid);
        }
        fn wait_for_exit(&self, _pid: u32, _max: Duration) {
            // Pretend the process exited immediately.
        }
    }

    /// Build a fixture workspace and a busy-marker path that's unique per
    /// test (uses the TempDir's basename, which is random per-test).
    fn fixture_workspace() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().to_path_buf();
        (dir, ws)
    }

    fn pre_populate_marker(
        workspace: &Path,
        pid: u32,
        comm: &str,
        age_secs: i64,
    ) {
        let path = marker_path(workspace);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let started = chrono::Utc::now() - chrono::Duration::seconds(age_secs);
        let marker = BusyMarker {
            repo_url: "git@github.com:test/repo.git".into(),
            pid,
            pgid: 1234,
            comm: comm.into(),
            started_at: started,
            stage: Stage::Executor,
        };
        write_atomic(&path, &marker).unwrap();
    }

    #[test]
    fn acquire_on_clean_returns_acquired() {
        let (_dir, ws) = fixture_workspace();
        match try_acquire(&ws, "git@github.com:test/repo.git", 1800).unwrap() {
            AcquireOutcome::Acquired(guard) => {
                assert!(guard.path().exists(), "marker file must exist");
                assert_eq!(guard.contents.repo_url, "git@github.com:test/repo.git");
                assert_eq!(guard.contents.stage, Stage::Executor);
                drop(guard);
                assert!(!marker_path(&ws).exists(), "Drop must remove file");
            }
            _ => panic!("expected Acquired"),
        }
    }

    #[test]
    fn acquire_when_fresh_returns_skip_fresh() {
        let (_dir, ws) = fixture_workspace();
        pre_populate_marker(&ws, 99999, "claude", 10);
        match try_acquire_with(&ws, "git@github.com:test/repo.git", 1800, &MockOps::new()) {
            Ok(AcquireOutcome::SkipFreshInProgress(m)) => {
                assert_eq!(m.pid, 99999);
                // Marker MUST remain untouched.
                assert!(marker_path(&ws).exists());
            }
            other => panic!("expected SkipFreshInProgress, got something else; result was: {:?}",
                other.map(|o| match o {
                    AcquireOutcome::Acquired(_) => "Acquired",
                    AcquireOutcome::SkipFreshInProgress(_) => "Fresh",
                    AcquireOutcome::SkipAmbiguous(_) => "Ambiguous",
                })),
        }
        // Cleanup so subsequent tests don't see stale file.
        let _ = std::fs::remove_file(marker_path(&ws));
    }

    #[test]
    fn acquire_when_stale_dead_pid_recovers() {
        let (_dir, ws) = fixture_workspace();
        pre_populate_marker(&ws, 99999, "claude", 3600);
        // MockOps with no alive PIDs → pid_alive(99999) returns false.
        let ops = MockOps::new();
        match try_acquire_with(&ws, "git@github.com:test/repo.git", 1800, &ops) {
            Ok(AcquireOutcome::Acquired(guard)) => {
                assert!(guard.path().exists());
                // The marker should be FRESH (recently written) — old PID
                // was 99999, new should be this process's PID.
                assert_eq!(guard.contents.pid, std::process::id());
                drop(guard);
            }
            _ => panic!("expected Acquired after clearing stale-dead marker"),
        }
    }

    #[test]
    fn acquire_when_malformed_recovers() {
        let (_dir, ws) = fixture_workspace();
        let path = marker_path(&ws);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not valid JSON {{{").unwrap();
        match try_acquire(&ws, "git@github.com:test/repo.git", 1800) {
            Ok(AcquireOutcome::Acquired(guard)) => {
                drop(guard);
            }
            _ => panic!("malformed file should be cleared and re-acquired"),
        }
    }

    #[test]
    fn acquire_when_stuck_kills_pgid_and_recovers() {
        let (_dir, ws) = fixture_workspace();
        pre_populate_marker(&ws, 99999, "claude", 3600);
        let ops = MockOps::new().with_alive(99999).with_comm(99999, "claude");
        match try_acquire_with(&ws, "git@github.com:test/repo.git", 1800, &ops) {
            Ok(AcquireOutcome::Acquired(guard)) => {
                // Precedence rule: stuck-recovery prefers the subprocess
                // sidecar's PGID over the marker's `pgid`. This test
                // pre-populates only the marker (no sidecar), so the
                // fallback path is exercised — killpg_terminate must
                // target the marker's pgid (1234 from
                // pre_populate_marker).
                let term = ops.killpg_terminate_called.lock().unwrap().clone();
                assert_eq!(term, vec![1234], "SIGTERM to pgid 1234 expected");
                drop(guard);
            }
            _ => panic!("expected Acquired after killing stuck process"),
        }
    }

    #[test]
    fn acquire_when_ambiguous_skips_and_leaves_file() {
        let (_dir, ws) = fixture_workspace();
        // Recorded comm was "claude", but the live PID's comm is "vim" —
        // either the PID was reused or this isn't an autocoder-spawned
        // process. Conservative path: leave file, skip iteration.
        pre_populate_marker(&ws, 99999, "claude", 3600);
        let ops = MockOps::new().with_alive(99999).with_comm(99999, "vim");
        match try_acquire_with(&ws, "git@github.com:test/repo.git", 1800, &ops) {
            Ok(AcquireOutcome::SkipAmbiguous(m)) => {
                assert_eq!(m.comm, "claude");
                assert!(marker_path(&ws).exists(),
                    "ambiguous case MUST leave the file for human inspection");
            }
            _ => panic!("expected SkipAmbiguous"),
        }
        // Cleanup so test doesn't leak the marker.
        let _ = std::fs::remove_file(marker_path(&ws));
    }

    #[test]
    fn set_stage_persists_atomically() {
        let (_dir, ws) = fixture_workspace();
        let mut guard = match try_acquire(&ws, "git@github.com:test/repo.git", 1800).unwrap() {
            AcquireOutcome::Acquired(g) => g,
            _ => panic!("acquire failed"),
        };
        guard.set_stage(Stage::Push).unwrap();
        let on_disk = read_marker(guard.path()).unwrap();
        assert_eq!(on_disk.stage, Stage::Push);
    }

    #[test]
    fn guard_drop_removes_file() {
        let (_dir, ws) = fixture_workspace();
        let path = match try_acquire(&ws, "git@github.com:test/repo.git", 1800).unwrap() {
            AcquireOutcome::Acquired(g) => {
                let p = g.path().to_path_buf();
                assert!(p.exists());
                drop(g);
                p
            }
            _ => panic!("acquire failed"),
        };
        assert!(!path.exists(), "file must be removed on Drop");
    }

    #[test]
    fn marker_path_layout_under_autocoder_busy() {
        let path = marker_path(Path::new("/tmp/workspaces/github_com_owner_repo"));
        let s = path.to_string_lossy();
        assert!(s.contains("autocoder"));
        assert!(s.contains("busy"));
        assert!(s.ends_with("github_com_owner_repo.json"));
    }

    #[test]
    fn age_secs_treats_future_started_at_as_zero() {
        let future = chrono::Utc::now() + chrono::Duration::seconds(60);
        assert_eq!(age_secs(&future), 0);
    }

    /// Pre-populate a sidecar file at `subprocess_marker_path(workspace)`
    /// containing `pid` so stuck-recovery can read it as the kill target.
    fn pre_populate_subprocess_marker(workspace: &Path, pid: i32) {
        let path = subprocess_marker_path(workspace);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{pid}\n")).unwrap();
    }

    /// When a sidecar is present alongside a stuck marker, the sidecar's
    /// PGID is the kill target — the marker's `pgid` is ignored. Both
    /// files must be removed before the new marker is acquired.
    #[test]
    fn stuck_recovery_uses_sidecar_pgid_when_present() {
        let (_dir, ws) = fixture_workspace();
        pre_populate_marker(&ws, 99999, "claude", 3600); // marker.pgid = 1234
        pre_populate_subprocess_marker(&ws, 5678);
        let ops = MockOps::new().with_alive(99999).with_comm(99999, "claude");
        match try_acquire_with(&ws, "git@github.com:test/repo.git", 1800, &ops) {
            Ok(AcquireOutcome::Acquired(guard)) => {
                let term = ops.killpg_terminate_called.lock().unwrap().clone();
                assert_eq!(
                    term,
                    vec![5678],
                    "SIGTERM must go to sidecar's PGID (5678), not marker's pgid (1234)"
                );
                // Sidecar was cleared as part of the kill sequence; new
                // marker (held by `guard`) should not be accompanied by
                // a stale sidecar.
                assert!(
                    !subprocess_marker_path(&ws).exists(),
                    "sidecar must be removed after stuck-recovery"
                );
                drop(guard);
                assert!(
                    !marker_path(&ws).exists(),
                    "marker must be removed when guard is dropped"
                );
            }
            _ => panic!("expected Acquired after killing stuck process"),
        }
    }

    /// Backward-compat path: when no sidecar exists, the marker's `pgid`
    /// is the fallback kill target. Behavior matches the pre-sidecar
    /// implementation.
    #[test]
    fn stuck_recovery_falls_back_to_marker_pgid_when_no_sidecar() {
        let (_dir, ws) = fixture_workspace();
        pre_populate_marker(&ws, 99999, "claude", 3600); // marker.pgid = 1234
        // No sidecar pre-written.
        let ops = MockOps::new().with_alive(99999).with_comm(99999, "claude");
        match try_acquire_with(&ws, "git@github.com:test/repo.git", 1800, &ops) {
            Ok(AcquireOutcome::Acquired(guard)) => {
                let term = ops.killpg_terminate_called.lock().unwrap().clone();
                assert_eq!(
                    term,
                    vec![1234],
                    "without sidecar, SIGTERM must fall back to marker's pgid"
                );
                drop(guard);
            }
            _ => panic!("expected Acquired after killing stuck process"),
        }
    }

    /// Round-trip: write_subprocess_marker → read_subprocess_marker
    /// returns the same PID; remove_subprocess_marker → read returns
    /// None.
    #[test]
    fn write_and_read_subprocess_marker_roundtrip() {
        let (_dir, ws) = fixture_workspace();
        write_subprocess_marker(&ws, 99).unwrap();
        assert_eq!(read_subprocess_marker(&ws), Some(99));
        remove_subprocess_marker(&ws);
        assert_eq!(read_subprocess_marker(&ws), None);
    }

    /// A sidecar containing non-numeric content yields None (no panic).
    /// Recovery is best-effort: garbage is treated the same as absent.
    #[test]
    fn read_subprocess_marker_returns_none_on_garbage() {
        let (_dir, ws) = fixture_workspace();
        let path = subprocess_marker_path(&ws);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not a number\n").unwrap();
        assert_eq!(read_subprocess_marker(&ws), None);
        // Cleanup so subsequent tests don't see the leftover file.
        let _ = std::fs::remove_file(&path);
    }

    /// Stale-dead-PID branch must clear the sidecar in addition to the
    /// marker so the two files stay consistent across iterations.
    #[test]
    fn stale_dead_pid_also_removes_sidecar() {
        let (_dir, ws) = fixture_workspace();
        pre_populate_marker(&ws, 99999, "claude", 3600);
        pre_populate_subprocess_marker(&ws, 5678);
        // MockOps with no alive PIDs → pid_alive(99999) returns false →
        // stale-dead branch fires.
        let ops = MockOps::new();
        match try_acquire_with(&ws, "git@github.com:test/repo.git", 1800, &ops) {
            Ok(AcquireOutcome::Acquired(guard)) => {
                assert!(
                    !subprocess_marker_path(&ws).exists(),
                    "sidecar must be removed in the stale-dead branch"
                );
                drop(guard);
                assert!(
                    !marker_path(&ws).exists(),
                    "marker must be removed when guard is dropped"
                );
            }
            _ => panic!("expected Acquired after clearing stale-dead marker"),
        }
    }

    #[test]
    fn subprocess_marker_path_layout_under_autocoder_busy() {
        let path = subprocess_marker_path(Path::new(
            "/tmp/workspaces/github_com_owner_repo",
        ));
        let s = path.to_string_lossy();
        assert!(s.contains("autocoder"));
        assert!(s.contains("busy"));
        assert!(s.ends_with("github_com_owner_repo.subprocess"));
    }
}
