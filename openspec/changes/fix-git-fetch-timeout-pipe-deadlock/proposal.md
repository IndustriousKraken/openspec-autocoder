## Why

`fetch_remote_with_timeout` in `autocoder/src/git.rs:84` spawns
`git fetch <remote>` with both stdout and stderr set to
`Stdio::piped()`, then polls `child.try_wait()` in a loop and only
reads the pipes **after** the child has exited:

```rust
// autocoder/src/git.rs:97-115
loop {
    match child.try_wait() {
        Ok(Some(status)) => {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = s.read_to_end(&mut stdout);   // only read after exit
            }
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_end(&mut stderr);    // only read after exit
            }
            ...
```

Nothing drains the pipes while the child is still running. OS pipes
have a fixed buffer (typically 64 KiB on Linux). When `git fetch`
writes more than that to its pipes before exiting, it blocks on the
`write()` syscall waiting for the reader to consume bytes — but the
reader (`read_to_end`) does not run until `try_wait()` reports the
child has exited, which can never happen because the child is blocked.
This is a classic reader/writer pipe **deadlock**: the child cannot
finish, `try_wait()` returns `Ok(None)` forever, and the function
escapes only when the deadline elapses, kills the child, and returns a
spurious `"git fetch {remote} timed out"` error.

`git fetch <remote>` writes its diagnostic output (`remote:` server
lines and a one-line ref-update summary per created/updated ref) to
**stderr**, and that volume scales with the number of refs. Fetching
an upstream with thousands of tags/branches — common on the first
fetch of a heavily-tagged OSS project — easily exceeds 64 KiB and
triggers the deadlock.

This is reachable in production from two call sites, both fetching the
configured **upstream** remote (an arbitrary external repository chosen
by the operator):

- `autocoder/src/polling_loop.rs:781` — the opportunistic per-iteration
  upstream fetch (30s timeout). Every iteration wastes the full timeout
  and the upstream is never updated.
- `autocoder/src/polling/sync_upstream.rs:114` — the operator-triggered
  `sync-upstream` chatops command (60s timeout). For a large upstream
  the command **always** reports `FetchFailed: git fetch <remote> timed
  out after 60s`, so the operator can never sync the fork.

Harm: a liveness/correctness bug — the timeout-bounded fetch hangs
until killed and silently misreports a healthy-but-large fetch as a
network timeout, so OSS-fork upstream sync silently and permanently
fails for any upstream whose fetch output exceeds the pipe buffer. No
attacker is required; a benign large upstream is sufficient.

`run_git` (the helper used everywhere else, `autocoder/src/git.rs:19`)
is **not** affected because `Command::output()` drains both pipes
concurrently internally. Only the hand-rolled spawn+poll path in
`fetch_remote_with_timeout` has this bug.

## What Changes

Drain stdout and stderr **concurrently** with waiting on the child so
the captured output is bounded only by memory, not by the pipe buffer:

- Extract the spawn-wait-capture logic into a private helper that
  starts one reader thread per pipe (each `read_to_end` into a `Vec`)
  immediately after spawn, polls `try_wait()` with the existing 100 ms
  cadence and deadline, and joins the reader threads once the child
  exits (or after it is killed on timeout).
- Rewrite `fetch_remote_with_timeout` to delegate to that helper,
  preserving the existing error messages, poll cadence, and timeout
  semantics (genuine timeouts still kill + reap the child and return
  the timeout error).
- Add a regression test that drives the drain path with a command
  emitting more than the pipe buffer of output and exiting non-zero,
  asserting it returns the captured output rather than a timeout.

## Impact

- `autocoder/src/git.rs` — refactor `fetch_remote_with_timeout`; add a
  concurrent-draining wait helper and a regression test.
- Behavior change: large-output upstream fetches now complete instead
  of timing out; callers in `polling_loop.rs` and
  `polling/sync_upstream.rs` are unchanged but stop seeing spurious
  timeouts.
- No operator follow-up required. No external interface changes.
