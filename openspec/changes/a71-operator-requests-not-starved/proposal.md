## Why

The per-repo polling loop drains operator chatops requests — the `send it` audit-triage, `propose`, AND `changelog` — at the top of each iteration, *before* the change-queue walk (`polling_loop.rs:411-485`). But the walk (`walk_queue`, `polling_loop.rs:2210`, `for change in pending`) grinds through a whole **batch** of pending changes in one iteration — each change a full executor run (15–26 min), bundled into one PR — before the iteration ends and the next drain runs.

So an operator request that arrives *while the walk is mid-batch* waits for the **entire current batch** to finish before the next iteration drains it. On an idle repo the batch is instant and the request runs promptly; on a busy self-hosting repo with a deep backlog it waits the full batch — minutes to hours. This was observed directly: a `@<bot> changelog` request on the busy `autocoder` repo never surfaced while the same request on an idle repo (`coterie`) shipped immediately. `propose` is the worse case — an operator waiting on a proposal response sits behind the whole queue, which is the opposite of what an interactive request should do.

## What Changes

**The queue walk yields to pending operator chatops requests.** After completing each change in the walk, the daemon checks whether any operator chatops request (`send it` / `propose` / `changelog`) is pending for the repo; if so, it ends the current batch — opening its PR with the changes accumulated so far — AND returns, so the **next iteration drains the pending operator request before starting a new batch.** This bounds operator-request latency to at most **one in-flight change-cycle** rather than a full backlog.

The walk does NOT interrupt a change that is already executing (the current change runs to its outcome); it only declines to *start the next* change when an operator request is waiting. So a workspace-resetting operator request (changelog/propose reset to the base branch) never interleaves with an in-flight change.

## Impact

- **Affected specs:** `orchestrator-cli` — ADD `The queue walk yields to pending operator chatops requests`.
- **Affected code:** `walk_queue` (`polling_loop.rs:2210`) checks the pending-request queues (`pending_triages`, `pending_proposal_requests`, `pending_changelog_requests`) after each change and returns early (ending the batch, the caller opens the accumulated PR) when any is pending.
- **Operator-visible behavior:** `changelog`, `propose`, AND `send it` get attention within one change-cycle even on a busy repo, instead of waiting for the whole backlog. PRs may bundle fewer changes when an operator request interleaves. Idle repos and empty-queue iterations are unaffected.
- **Dependencies:** none — independent. Touches only the queue-walk loop.
- **Acceptance:** `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean; `openspec validate a71-operator-requests-not-starved --strict` passes. Test: with a non-empty `pending` list AND a queued changelog (or propose) request, the walk processes at most one change before yielding, and the operator request is drained on the next iteration — not deferred to the end of the batch.
