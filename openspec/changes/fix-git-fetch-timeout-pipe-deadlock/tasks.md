## 1. Extract a concurrent-draining wait helper

- [ ] 1.1 In `autocoder/src/git.rs`, add a private function
  `wait_capture_with_timeout(mut child: std::process::Child, op_label: &str, timeout_secs: u64) -> Result<std::process::Output>`.
  Immediately after the call site spawns the child, the helper takes
  `child.stdout.take()` and `child.stderr.take()` and moves each into
  its own `std::thread::spawn` closure that calls
  `std::io::Read::read_to_end` into a `Vec<u8>`; keep both
  `JoinHandle`s.
- [ ] 1.2 In the helper, loop on `child.try_wait()` with the existing
  100 ms `std::thread::sleep` cadence and the existing deadline
  (`Instant::now() + Duration::from_secs(timeout_secs)`). Do NOT read
  the pipes inside the loop.
- [ ] 1.3 On `Ok(Some(status))`, `join()` both reader threads to
  collect the captured stdout/stderr `Vec<u8>`s (treat a thread join
  error or inner read error as empty bytes), then return
  `Ok(std::process::Output { status, stdout, stderr })`.
- [ ] 1.4 On deadline exceeded, call `child.kill()` then
  `child.wait()`, `join()` both reader threads (the killed child closes
  its pipe write ends, so the readers reach EOF and finish), and return
  `Err(anyhow!("{op_label} timed out after {timeout_secs}s"))`.
- [ ] 1.5 On `Err(e)` from `try_wait()`, return
  `Err(anyhow!("waiting on `{op_label}`: {e}"))` (preserve the current
  message shape), joining the reader threads first so neither is
  leaked.

## 2. Route `fetch_remote_with_timeout` through the helper

- [ ] 2.1 In `autocoder/src/git.rs::fetch_remote_with_timeout`, keep
  the `Command::new("git").args(["fetch", remote])` spawn with
  `stdout(Stdio::piped())` and `stderr(Stdio::piped())`, then replace
  the inline `try_wait` loop with a call to
  `wait_capture_with_timeout(child, &format!("git fetch {remote}"), timeout_secs)`.
- [ ] 2.2 Map the returned `Output` exactly as today: on
  `status.success()` return `Ok(())`; otherwise build
  `stderr_s = String::from_utf8_lossy(&output.stderr).trim().to_string()`
  and return `Err(anyhow!("git fetch {remote} failed: {stderr_s}"))`.
  The timeout `Err` produced by the helper already matches the current
  `"git fetch {remote} timed out after {timeout_secs}s"` message.

## 3. Regression test for large-output capture

- [ ] 3.1 Add a unit test in the `#[cfg(test)]` module of
  `autocoder/src/git.rs` named
  `wait_capture_drains_more_than_pipe_buffer_without_timeout`. Spawn a
  child via
  `std::process::Command::new("sh").args(["-c", "i=0; while [ $i -lt 5000 ]; do echo 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' 1>&2; i=$((i+1)); done; exit 1"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()`.
  This writes well over 64 KiB to stderr (5000 lines x ~50 bytes) and
  then exits non-zero.
- [ ] 3.2 Call `wait_capture_with_timeout(child, "test fetch", 30)` and
  assert the result is `Ok(output)` with `!output.status.success()`
  AND `output.stderr.len() > 64 * 1024`. If the deadlock regressed, the
  child would block on the full stderr pipe and the helper would return
  the timeout `Err` instead — so this assertion fails closed on the bug.
- [ ] 3.3 Add a second test
  `wait_capture_reports_timeout_when_child_never_exits` that spawns
  `sh -c 'sleep 60'` with piped stdio, calls
  `wait_capture_with_timeout(child, "test sleep", 1)`, and asserts the
  result is `Err` whose message contains `timed out after 1s`,
  confirming the genuine-timeout path still kills and reports.
