## 1. Per-iteration cancel infrastructure

- [x] 1.1 Extend `RepoTaskHandle` (in `autocoder/src/control_socket.rs`) with:
  ```rust
  pub iteration_cancel: Arc<Mutex<Option<CancellationToken>>>,
  pub iteration_drained: Arc<tokio::sync::Notify>,
  ```
  Both default to `Arc::new(Mutex::new(None))` and `Arc::new(Notify::new())` respectively at handle creation.
- [x] 1.2 In `autocoder/src/polling_loop.rs::run` (the per-repo polling loop), at the start of each iteration body:
  ```rust
  let iter_cancel = global_cancel.child_token();
  *handle.iteration_cancel.lock().unwrap() = Some(iter_cancel.clone());
  ```
  Replace every `cancel.cancelled()` check inside the iteration body with `iter_cancel.cancelled()`. The child token fires when either the parent (global cancel) OR the child (per-iteration cancel) is cancelled — preserving the existing SIGINT/SIGTERM propagation.
- [x] 1.3 At every iteration exit path (normal completion, executor Failed, executor AskUser, cancel-cancelled-mid-iteration, any error early-return):
  ```rust
  *handle.iteration_cancel.lock().unwrap() = None;
  handle.iteration_drained.notify_waiters();
  ```
  Centralize this via a `drop` guard struct so every exit path runs the cleanup without manual repetition:
  ```rust
  struct IterationGuard<'a> { handle: &'a RepoTaskHandle }
  impl Drop for IterationGuard<'_> {
      fn drop(&mut self) {
          *self.handle.iteration_cancel.lock().unwrap() = None;
          self.handle.iteration_drained.notify_waiters();
      }
  }
  ```
  Construct `let _guard = IterationGuard { handle: &handle };` after storing the token; the guard's drop fires the cleanup on any control-flow exit including panics.
- [x] 1.4 Tests:
  - Polling task at iteration start has `iteration_cancel: Some(_)` in the handle.
  - Polling task between iterations has `iteration_cancel: None`.
  - Firing the stored token causes the iteration body to exit at the next safety point (the existing `iter_cancel.cancelled()` checks).
  - Iteration exit fires the `iteration_drained` Notify (verify via `notify.notified()` await with a short timeout).
  - Panic inside the iteration body still triggers the guard's cleanup (Notify fires, iteration_cancel cleared).

## 2. Wipe-workspace control-socket handler updates

- [x] 2.1 In `autocoder/src/control_socket.rs`, update the `wipe_workspace` action handler:
  ```rust
  // 1. Look up the per-repo handle from `repo_tasks` keyed by url.
  // 2. Read iteration_cancel.lock() → Option<CancellationToken>.
  // 3. If Some(token):
  //    - notify = handle.iteration_drained.clone();
  //    - let notified = notify.notified();
  //    - token.cancel();
  //    - let _ = tokio::time::timeout(drain_timeout, notified).await;
  //    - record drain_outcome: "drained cleanly in {elapsed}" OR "drain timeout"
  // 4. If None: record drain_outcome: "no iteration in flight"
  // 5. Perform std::fs::remove_dir_all(workspace_path)
  // 6. Return response body with drain_outcome AND wipe result
  ```
- [x] 2.2 The response payload extends today's shape:
  ```rust
  pub struct WipeWorkspaceResponse {
      pub ok: bool,
      pub path: String,
      pub already_absent: bool,
      pub drain_outcome: String,  // NEW: "drained cleanly in 1.2s" | "drain timeout — iteration may have been stuck" | "no iteration in flight"
  }
  ```
- [x] 2.3 Tests (using stubbed per-repo handles):
  - Handle with `iteration_cancel: Some(_)` + a fake polling task that responds quickly: drain_outcome is `drained cleanly in <Xs>`.
  - Handle with `iteration_cancel: Some(_)` + a fake polling task that ignores the cancel: drain_outcome is `drain timeout — iteration may have been stuck`.
  - Handle with `iteration_cancel: None`: drain_outcome is `no iteration in flight`; no Notify is awaited.
  - Workspace already absent: `already_absent: true`, wipe is no-op, drain still runs first.

## 3. Drain timeout config

- [x] 3.1 In `autocoder/src/config.rs`, extend `ExecutorConfig` with `pub wipe_drain_timeout_secs: u64` defaulting to `30` via `#[serde(default = "default_wipe_drain_timeout_secs")]`.
- [x] 3.2 Clamp values above `300` to `300` with a WARN log at startup (drain timeouts longer than 5 minutes are almost certainly operator misconfiguration; allowing them risks the wipe handler holding the chatops listener busy for too long).
- [x] 3.3 Tests: default → 30; explicit 0 → 0 (skips the await; wipe runs immediately whether the iteration responded or not); explicit 300 → 300 no WARN; explicit 600 → 300 with WARN.

## 4. Enriched confirmation message

- [x] 4.1 In `autocoder/src/chatops/operator_commands.rs`, extend the wipe-workspace first-step handler to fetch the live `RepoStatusResponse` for the resolved repo (via the existing `repo_status` action). The dispatcher already calls into the control socket for actions; this is one extra call before the confirmation text is built.
- [x] 4.2 New formatter `fn format_wipe_confirmation(workspace_path: &Path, repo_url: &str, status: &RepoStatusResponse) -> String` producing the documented shape:
  ```
  ⚠️ Wipe-workspace requested for <repo_url>
  This will delete <workspace_path> (forces a re-clone on the next iteration).

  Currently: <busy_clause>
  Queue (continues after wipe): <queue_clause>
  [Active markers (git-tracked; preserved across the wipe):
    • <change> (<marker-file>)
    ...]

  Reply 'confirm' within 60 seconds to proceed.
  ```
- [x] 4.3 Busy clause:
  - `currently_busy == None`: render `idle`.
  - `currently_busy == Some(BusySummary)`: render `working on \`<change>\` (started <age> ago) — will be cancelled`.
- [x] 4.4 Queue clause: reuse the existing one-line queue formatter from `chatops-status-enrichment` (the `2 pending (a06, a07), 0 waiting, 0 excluded` form, with the same `empty queue` collapse for the all-zero case).
- [x] 4.5 Active-markers section: only present when `perma_stuck_changes.len() + revision_marked_changes.len() > 0`. Lists each marker entry as `• <change> (<marker-file>)` where `<marker-file>` is `.perma-stuck.json` or `.needs-spec-revision.json` matching the entry's source.
- [x] 4.6 The user-facing fields (change names, repo URL, workspace path) pass through `slack_escape` before assembly, matching the conventions established in `chatops-status-enrichment` and `chatops-status-menu`.
- [x] 4.7 Tests:
  - Idle + empty queue + no markers: rendered text contains `Currently: idle`, `Queue (continues after wipe): empty queue`, and no Active-markers section.
  - Busy + non-empty queue + markers: rendered text contains the full form including the busy clause's `— will be cancelled` suffix and the markers section.
  - Slack-escape: change name containing `<` survives the escape pass (renders as `&lt;`).

## 5. Wipe success-reply text updates

- [x] 5.1 The existing success-reply text after a confirmed wipe currently reads roughly `✓ Wiped /tmp/workspaces/...`. Extend it with the drain outcome:
  - `✓ Wiped <path> (drained cleanly in <Xs>)` — when the iteration exited within the timeout.
  - `✓ Wiped <path> (drain timeout — iteration may have been stuck)` — when the timeout fired. This is a yellow flag for the operator.
  - `✓ Wiped <path> (no iteration in flight)` — when no per-iteration cancel handle existed at confirm time.
  - `✓ Wiped <path> (already absent)` — when the directory was already missing AND no iteration was in flight (preserves the existing already-absent reporting).
- [x] 5.2 Tests: each of the four cases produces the documented reply text.

## 6. README + docs updates

- [x] 6.1 In `docs/CHATOPS.md`'s "Two-step confirmation for wipe-workspace" subsection, update the example confirmation message to the new enriched form. Add a paragraph explaining what each new section means.
- [x] 6.2 In `docs/CONFIG.md`, document the new `executor.wipe_drain_timeout_secs` field.
- [x] 6.3 Add a paragraph in `docs/TROUBLESHOOTING.md` for the `drain timeout — iteration may have been stuck` outcome: it usually means the iteration was in a blocking syscall (a hung executor subprocess, a long git fetch, etc.). The wipe still succeeded; the operator can investigate the stuck iteration's log at `/tmp/autocoder/logs/<workspace>/<change>.log` after the wipe to understand why it didn't drain.

## 7. Spec delta

- [x] 7.1 The ADDED requirement in `openspec/changes/chatops-wipe-workspace-iteration-aware/specs/chatops-manager/spec.md` codifies: the enriched confirmation message shape and its section-collapse rules, the drain-then-wipe sequence on confirm, the per-iteration cancel infrastructure, the timeout configuration, the four success-reply outcome variants, and the no-iteration-in-flight short-circuit.

## 8. Verification

- [x] 8.1 `cargo test` passes (new + existing).
- [x] 8.2 `openspec validate chatops-wipe-workspace-iteration-aware --strict` passes.
- [x] 8.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
