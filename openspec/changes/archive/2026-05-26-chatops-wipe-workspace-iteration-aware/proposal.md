## Why

`@<bot> wipe-workspace <repo>` today performs a direct `std::fs::remove_dir_all(/tmp/workspaces/<sanitized>/)` without any coordination with the per-repo polling task. When the wipe fires mid-iteration — the most operationally relevant time, since wipes are usually a response to a stuck or misbehaving change — the executor subprocess loses its CWD, hits ENOENT on its next file op, and dies. The iteration ends in `Failed: executor exited with exit status: 143` (SIGTERM). The next polling tick detects the missing workspace, re-clones from scratch, and resumes processing. The system recovers, but the failure path is messy: the agent's in-flight work is killed in a way that produces a real-looking failure log entry, the busy marker may race with the directory deletion, and the operator gets no signal in the chatops channel that their wipe interrupted anything.

Two improvements close the gap:

**Show context in the confirmation.** The current first-step warning is a generic one-liner naming only the workspace path. An operator deciding whether to type `confirm` is missing the most relevant information: what's the daemon currently doing for this repo, and what will resume after the wipe? Showing the currently-busy state, the queue depth, and active markers in the confirmation lets the operator make an informed go/no-go call before committing to the wipe. (Most operators will still proceed — wiping usually means resetting to clean state and the killed work is by design — but the visibility matters for the cases where they realize "oh wait, I should let this finish first" or "oh, there's an open question waiting in chat that I should answer before wiping.")

**Drain the iteration before wiping.** Once the operator confirms, the daemon signals the per-repo polling task to cancel its current iteration cleanly, waits briefly for the iteration to exit, THEN performs the directory deletion. The next polling tick fires normally and re-clones. The result: no SIGTERM-shaped failure log entry, no race on the busy marker, the operator's chatops reply reads as a clean "wipe complete" rather than an opaque deletion that happens to leave a failed-iteration tombstone behind.

## What Changes

**Enriched confirmation message.** The first-step warning gains a status excerpt drawn from the same data the `@<bot> status <repo>` reply produces — but compacted to the wipe's decision-making needs. Shape:

```
⚠️ Wipe-workspace requested for git@github.com:acme/myrepo.git
This will delete /tmp/workspaces/github_com_acme_myrepo (forces a re-clone on the next iteration).

Currently: working on `audit-proposal-self-validation` (started 5m ago) — will be cancelled
Queue (continues after wipe): 2 pending (pr-body-..., queue-archive-...), 0 waiting
Active markers (git-tracked; preserved across the wipe):
  • audit-proposal-created-notification (.needs-spec-revision.json)

Reply 'confirm' within 60 seconds to proceed.
```

Sections collapse when empty:

- **`Currently:`** — `idle` when no busy marker exists; `working on <change> (started <age> ago) — will be cancelled` when busy. Always shown so the operator sees what state the wipe is acting on.
- **`Queue (continues after wipe):`** — one-line summary in the same compact form as `status`'s queue clause. When the queue is fully empty (`pending == 0 && waiting == 0 && excluded == 0`), the line collapses to `Queue: empty`.
- **`Active markers (git-tracked; preserved across the wipe):`** — only shown when at least one `.perma-stuck.json` or `.needs-spec-revision.json` marker exists. The "git-tracked; preserved" note reassures operators that the wipe does not lose marker state (it returns from origin on the next re-clone).

The `Reply 'confirm' ...` line is unchanged.

**Drain coordination on confirm.** After the operator types `confirm`, the daemon performs the following sequence:

1. **Set the per-iteration cancel signal.** Each polling iteration runs under a per-iteration cancellation token (a child of the global cancel token). The control-socket handler reaches into the per-repo task state, retrieves the current iteration's cancel handle (if any), and fires it.
2. **Wait for the iteration to exit.** The polling task observes the cancellation at its next safety point, returns early from the iteration body, and releases the busy marker. A per-repo `iteration_drained` `Notify` (added in this change) fires when the iteration's per-iteration cleanup completes. The wipe handler awaits the Notify with a configurable timeout (default 30 seconds).
3. **Perform the wipe.** Whether the drain completed within the timeout OR the timeout fired (the iteration is genuinely stuck and won't drain), the directory is deleted. The "wipe regardless of drain" semantics matches operator intent: the directory is going away one way or another; the drain was a politeness, not a hard precondition.
4. **Post the success reply.** The reply includes a clause naming the drain outcome: `✓ Wiped /tmp/workspaces/... (drained cleanly in 1.2s)` OR `✓ Wiped /tmp/workspaces/... (drain timeout — iteration may have been stuck)`. The latter is a yellow flag for the operator to investigate why the iteration didn't respond.

**Drain timeout config.** A new `executor.wipe_drain_timeout_secs: u64` (default `30`, max `300` with WARN-and-clamp at startup). Operators who deal with long-running executor invocations can raise it; sites where wipe-on-stuck is common can keep the default.

**Per-iteration cancel infrastructure.** A new `iteration_cancel: Arc<Mutex<Option<CancellationToken>>>` field on `RepoTaskHandle` (or equivalent per-repo state struct). The polling task at iteration start creates `let iter_cancel = global_cancel.child_token();` and stores `Some(iter_cancel.clone())` in the field. At iteration end (normal or cancelled), the polling task clears the field to `None` AND fires the per-repo `iteration_drained` Notify. The wipe handler reads the field; if `Some`, fires the token and awaits the Notify; if `None` (no iteration in flight), proceeds directly to the deletion.

**No global-cancel side effect.** The per-iteration cancel is fired in isolation. The global cancel token is not touched, so SIGINT / SIGTERM handling remains unchanged, the polling task continues to live (only its current iteration ends), and the next polling tick fires normally with the workspace gone (workspace::ensure_initialized handles the missing-directory case via its existing re-clone path).

**Behaviour when no iteration is in flight.** If the per-iteration cancel handle is `None` at the time of confirm (daemon is between iterations, in the inter-iteration sleep), the wipe proceeds immediately. The reply reads `✓ Wiped /tmp/workspaces/... (no iteration in flight)`. No drain is attempted; no Notify is awaited.

## Impact

- **Affected specs:** `chatops-manager` — one ADDED requirement covering the enriched confirmation message and the drain-then-wipe coordination contract.
- **Affected code:**
  - `autocoder/src/chatops/operator_commands.rs` — extend the wipe-workspace first-step handler to build the enriched confirmation message. The data sources are the same as `status`: live `RepoStatusResponse` (or its underlying data) for the repo. Refactor to share the queue-clause and currently-busy-clause formatters between `status` and `wipe-workspace` so both stay in sync visually.
  - `autocoder/src/control_socket.rs` — extend the `wipe_workspace` action handler with the drain-then-wipe sequence: read the per-iteration cancel handle, fire it, await the `iteration_drained` Notify with the configured timeout, perform the rm, report the drain outcome in the response payload.
  - `autocoder/src/polling_loop.rs` — at iteration start, create the per-iteration cancel token as `global_cancel.child_token()`, store in the per-repo state. At iteration end (every exit path, including failures), clear the stored token AND fire the `iteration_drained` Notify. The iteration body's existing `cancel.cancelled()` checks become `iter_cancel.cancelled()` checks (the child token fires when either the parent OR the child is cancelled, preserving the global-cancel propagation).
  - `autocoder/src/control_socket.rs` — extend `RepoTaskHandle` (or the per-repo state struct) with `iteration_cancel: Arc<Mutex<Option<CancellationToken>>>` and `iteration_drained: Arc<Notify>`.
  - `autocoder/src/config.rs` — add `executor.wipe_drain_timeout_secs: u64` (default `30`, max `300` with clamp).
  - Tests:
    - Parser tests for the enriched confirmation message (snapshot test against fixture RepoStatusResponses for idle / busy / empty-queue / markers-present cases).
    - Section-collapse tests: idle + empty queue + no markers produces the compact form (no marker section); busy + non-empty queue + markers produces the full form.
    - Drain-coordination unit test using a stubbed polling task: fire the per-iteration cancel, assert the Notify fires within a few hundred ms, assert the wipe runs after the Notify, assert the response payload reads "drained cleanly".
    - Drain-timeout test: stub a polling task that ignores the cancel for longer than the configured timeout; assert the wipe still runs, assert the response payload reads "drain timeout — iteration may have been stuck".
    - No-iteration-in-flight test: stub a polling task that is currently in its inter-iteration sleep (iteration_cancel field is None); assert the wipe proceeds immediately, assert the response payload reads "no iteration in flight".
    - End-to-end integration test: spawn a real polling task against a fixture workspace with a stub executor that does a long sleep (simulating mid-iteration work); fire the wipe via the control socket; assert the workspace is gone AND the next iteration re-clones AND the executor's failure log entry is absent (no 143 SIGTERM tombstone because the iteration drained cleanly).

- **Operator-visible behavior:** the wipe-workspace confirmation message gains a context preview. Successful wipes that interrupted an iteration produce no 143 SIGTERM tombstone in the logs; the iteration ends cleanly via the per-iteration cancel path. The chatops reply names the drain outcome so operators see "drained cleanly" vs "drain timeout" at a glance.
- **Breaking:** no. The confirmation message gains lines but the `confirm` mechanic is unchanged. The drain coordination is internal — operators observing the wipe see a cleaner log path but no new operator-actionable behaviour. The new config field defaults to a sensible value.
- **Acceptance:** `cargo test` passes (new + existing). A wipe-workspace fired against a repo with an in-flight iteration produces a chatops reply naming the drained iteration, the workspace is deleted, the next polling tick re-clones and resumes processing, and `journalctl` for the iteration that was cancelled shows a clean "iteration cancelled by wipe-workspace" log line instead of the historical `executor exited with exit status: 143` Failed entry.
