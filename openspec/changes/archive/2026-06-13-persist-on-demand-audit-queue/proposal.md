## Why

The on-demand audit-run queue (`pending_audit_runs`) is durable *within* a daemon lifetime — a busy-skipped, early-returning, or bounded-out pass no longer discards a queued request — but it lives only in memory. A daemon restart between an operator's `✓ Queued` acknowledgement and the audit's run silently loses the request, which is the one remaining hole in the "an acknowledged enqueue is never silently lost" guarantee. Persisting the queue closes it.

This is the deferred follow-on to `audits-fail-closed-and-report` (which introduced the `QueuedAudit` element, the chat-origin plumbing, and the in-memory durability). It depends on that change being applied first.

## What Changes

- Persist each repo's `pending_audit_runs` queue to a state-directory JSON file, written on every mutation (enqueue AND post-run prune) using an atomic tempfile + rename.
- Load the persisted queue into memory when a repo's polling task is (re)spawned, so a restart restores queued-but-not-yet-run audits.
- Reconcile away a persisted entry whose repo is no longer configured at load time (a startup orphan sweep, matching the other startup marker sweeps).
- Persistence is best-effort: a read/write failure is logged and never aborts a run; the in-memory queue stays authoritative for the live process.

## Capabilities

### New Capabilities
<!-- none -->

### Modified Capabilities
- `orchestrator-cli`: ADD `On-demand audit-run queue persists across daemon restart` — the in-memory queue is mirrored to durable storage, loaded at task spawn, and orphan-reconciled, so a restart between enqueue and run preserves the request. (Re-establishes the restart guarantee that `audits-fail-closed-and-report` deliberately scoped out.)

## Impact

- **Code:** a `DaemonPaths::pending_audit_runs_path(workspace_basename)` helper (`paths.rs`); `save`/`load` helpers (`polling_loop`); save-on-enqueue in the `queue_audit` control-socket handler (which requires `paths` to be reachable from `ControlState`); save-on-prune in the scheduler's queued-drain; load-on-spawn in the daemon's per-repo task setup (`cli/run.rs`).
- **Depends on:** `audits-fail-closed-and-report` (the `QueuedAudit` type already derives `Serialize`/`Deserialize`; the in-memory queue and origin plumbing exist).
- **Non-goals:** no change to the in-memory durability, the completion notification, or the queue's de-duplication/ordering semantics — this is purely the disk-backed survival layer.
