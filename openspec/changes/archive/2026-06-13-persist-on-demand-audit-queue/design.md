## Context

`pending_audit_runs` is an `Arc<Mutex<Vec<QueuedAudit>>>` on each repo's live polling-task handle. It is mutated in two places: the `queue_audit` control-socket handler (enqueue) and the scheduler's queued-drain (prune-on-run). To survive a restart, both mutations must be mirrored to disk, and the file must be loaded when the task is spawned.

## Goals / Non-Goals

**Goals:**
- A queued audit acknowledged to an operator survives a daemon restart and runs after restart.
- Best-effort: persistence failures degrade to the existing in-memory behavior, never aborting a run.

**Non-Goals:**
- No change to in-memory durability, ordering, de-duplication, or the completion notification.

## Decisions

### D1 — File layout mirrors the existing per-workspace state files
`<state>/pending-audit-runs/<workspace_basename>.json` via a new `DaemonPaths::pending_audit_runs_path(basename)`, exactly mirroring `alert_state_path`. Atomic write (tempfile + rename). The body is the serialized `Vec<QueuedAudit>` (serde already derived).

### D2 — Reaching `paths` from the enqueue site
The `queue_audit` control-socket handler runs against `ControlState`, which does **not** currently carry `DaemonPaths`. Two options:
- **(chosen) Add `paths: Arc<DaemonPaths>` to `ControlState`.** The handler then persists on enqueue directly. Touches the `ControlState` struct + its construction sites (the real one in `cli/run.rs` and the test fixtures). This makes the enqueue→restart window zero (the file is written the instant the request is acknowledged).
- *(rejected) Persist only at iteration start + prune.* Avoids the `ControlState` change but leaves a window: an enqueue followed by an immediate restart (before the next iteration's persist) is still lost. That reintroduces exactly the hole this change closes, so it is not acceptable.

The scheduler's prune site and the spawn-time load already have `paths`/`workspace` in scope, so only the enqueue site needs the `ControlState` field.

### D3 — Orphan reconciliation at load
At task spawn, load the file and drop entries (or whole files) for repos no longer configured, matching the existing startup marker-sweep pattern, so a removed repo's stale queue file never resurrects work.

## Risks / Trade-offs

- **`ControlState` gains a field → touches its constructors/test fixtures.** → Mechanical and compiler-guided; the test fixtures use `crate::testing::test_daemon_paths()`.
- **A corrupt/unparseable queue file.** → Best-effort load: log and start with an empty queue (lose at most the persisted entries, same as today's restart). Never panic.
- **Write amplification (a file write per enqueue/prune).** → Negligible: the queue is tiny and mutations are operator-paced.
