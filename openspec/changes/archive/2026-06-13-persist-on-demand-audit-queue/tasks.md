## 1. Storage primitive

- [x] 1.1 Add `DaemonPaths::pending_audit_runs_path(&self, workspace_basename: &str) -> PathBuf` returning `<state>/pending-audit-runs/<basename>.json`, mirroring `alert_state_path` (`paths.rs`).
- [x] 1.2 Add `save_pending_audit_runs(paths, workspace, &[QueuedAudit])` and `load_pending_audit_runs(paths, workspace_basename) -> Vec<QueuedAudit>` helpers (`polling_loop/mod.rs`): atomic tempfile + rename on save; best-effort load that returns an empty vec on a missing/unparseable file with a logged WARN. `QueuedAudit` already derives `Serialize`/`Deserialize`.

## 2. Reach `paths` from the enqueue site

- [x] 2.1 Add `paths: Arc<DaemonPaths>` to `ControlState` (`control_socket.rs`); populate it at the real construction site (`cli/run.rs`) and the test fixtures (use `crate::testing::test_daemon_paths()`).

## 3. Persist on every mutation

- [x] 3.1 In the `queue_audit` control-socket handler, after pushing the `QueuedAudit`, call `save_pending_audit_runs` for the repo (resolve the workspace from the repo config via `workspace::resolve_path`).
- [x] 3.2 In the scheduler's queued-drain (`run_due_audits`), after pruning ran/unregistered entries from the shared handle, call `save_pending_audit_runs` so the durable copy reflects what remains.

## 4. Load + reconcile at spawn

- [x] 4.1 When a repo's polling task is spawned (`cli/run.rs`), initialize `pending_audit_runs` from `load_pending_audit_runs` instead of an empty `Vec`.
- [x] 4.2 At load, drop persisted entries for repos no longer in the configured set (startup orphan reconciliation), matching the existing startup marker-sweep pattern; persist the reconciled queue back.

## 5. Tests

- [x] 5.1 Round-trip: `save_pending_audit_runs` then `load_pending_audit_runs` returns the same entries (including `origin`).
- [x] 5.2 Restart simulation: a saved queue file is loaded into a fresh task handle and the audit runs; after it runs + prunes, the file no longer contains it.
- [x] 5.3 Corrupt-file load returns an empty queue and logs, without panicking.
- [x] 5.4 Orphan reconciliation: a persisted entry for an unconfigured repo is dropped at load.

## 6. Documentation + acceptance

- [x] 6.1 Update `docs/OPERATIONS.md` (on-demand audit triggers) and `docs/CHATOPS.md` (the `audit` verb durability note) to state the queue now survives a daemon restart, replacing the "in-memory; cross-restart persistence is a planned follow-on" caveats.
- [x] 6.2 `openspec validate persist-on-demand-audit-queue --strict` passes AND the full `cargo test` suite is green.
