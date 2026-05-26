## 1. Workspace-validity check

- [ ] 1.1 In `autocoder/src/audits/mod.rs`, add:
  ```rust
  pub fn workspace_is_valid(workspace: &Path) -> bool {
      workspace.is_dir() && workspace.join(".git").is_dir()
  }
  ```
- [ ] 1.2 Tests:
  - Nonexistent path → false.
  - Path that is a file (not a dir) → false.
  - Directory without `.git/` subdir → false.
  - Directory with `.git/` as a file (not a subdir) → false. (Edge case: `git worktree` setups use a `.git` FILE, not a directory. For the autocoder's use case, every workspace is a normal clone so `.git/` is always a directory. Worth documenting this as a known limitation: if autocoder ever supports operator-configured worktree workspaces, this check needs to handle the file case too.)
  - Valid workspace with `.git/` subdir → true.

## 2. New AuditOutcome variant

- [ ] 2.1 In `autocoder/src/audits/mod.rs`, extend `AuditOutcome`:
  ```rust
  pub enum AuditOutcome {
      // existing variants (Reported, NoFindings, etc.)...
      WorkspaceUnavailable {
          audit_type: String,
          workspace_path: PathBuf,
          reason: String,
      },
  }
  ```
- [ ] 2.2 The `reason` is one of three fixed strings: `"workspace directory does not exist"`, `"workspace exists but has no .git/ subdirectory"`, OR `"workspace failed validity check"` (the catch-all for any future additional check). Tests assert each of the three is produced for the right pre-condition.

## 3. Per-audit gate

- [ ] 3.1 In each LLM-driven audit's main function (`autocoder/src/audits/architecture_consultative.rs`, `drift.rs`, `specs_writing.rs`), at the very top — after argument validation but before any file IO or LLM-call setup:
  ```rust
  if !workspace_is_valid(workspace) {
      let reason = if !workspace.exists() {
          "workspace directory does not exist".to_string()
      } else if !workspace.join(".git").is_dir() {
          "workspace exists but has no .git/ subdirectory".to_string()
      } else {
          "workspace failed validity check".to_string()
      };
      tracing::info!(
          audit_type = %audit_type,
          workspace = %workspace.display(),
          reason = %reason,
          "audit skipped: workspace not in a valid state"
      );
      return Ok(AuditOutcome::WorkspaceUnavailable {
          audit_type: audit_type.to_string(),
          workspace_path: workspace.to_path_buf(),
          reason,
      });
  }
  ```
- [ ] 3.2 Same gate at the top of `autocoder/src/audits/brightline.rs` (the non-LLM `architecture_brightline` audit). Even though it doesn't write proposals, gating it universally keeps the audit framework's contract uniform: no audit runs against an invalid workspace.
- [ ] 3.3 Tests per audit type, using fixture workspaces:
  - Fixture: workspace path doesn't exist → audit returns `Ok(WorkspaceUnavailable { reason: "workspace directory does not exist" })`. Critically: assert the workspace path is NOT created as a side effect (use `assert!(!workspace.exists())` after the call).
  - Fixture: workspace exists but has no `.git/` → audit returns `Ok(WorkspaceUnavailable { reason: "workspace exists but has no .git/ subdirectory" })`. Assert no new files or subdirectories were created (snapshot the directory state before + after).
  - Fixture: valid workspace with `.git/` → the gate passes, audit proceeds to its normal logic (or its stub equivalent in a test fixture).

## 4. Scheduler handling

- [ ] 4.1 In `autocoder/src/audits/scheduler.rs`, handle the new `WorkspaceUnavailable` outcome:
  - Log at INFO (NOT WARN — the iteration-level failure log already captures the upstream cause; an audit skip is the expected downstream consequence).
  - Do NOT update the audit's cadence-state file. Skipped runs don't consume cadence; the next iteration's cadence check re-evaluates and may try again if the workspace has become valid.
  - Proceed to the next scheduled audit in the same iteration (sibling audits may be unaffected — they get their own gate, and if the workspace is invalid for one it's likely invalid for all, but the loop continues uniformly).
- [ ] 4.2 Tests:
  - Scheduler receives `WorkspaceUnavailable` from one audit → no cadence-state write, the audit's INFO log fires, sibling audits in the iteration are still attempted.
  - Scheduler receives `WorkspaceUnavailable` from every audit in an iteration → no cadence-state writes overall, iteration moves on to the next phase (push + PR, etc.) normally.

## 5. Iteration-level gate

- [ ] 5.1 In `autocoder/src/polling_loop.rs`, locate where the audit scheduler is invoked (likely inside or near `execute_one_pass`). Add a precondition: only invoke the scheduler if `ensure_initialized` returned Ok for this iteration. If `ensure_initialized` returned Err, skip the scheduler call entirely — the iteration's failure path is already running, no need for the scheduler to even start.
- [ ] 5.2 The iteration-level gate is belt-and-braces with the per-audit gate. Per-audit catches the case where the workspace becomes invalid between iteration start and an individual audit run (rare). Iteration-level catches the case where the workspace was invalid at iteration start (common — the user's incident).
- [ ] 5.3 Tests:
  - Polling iteration where `ensure_initialized` returns Err: the audit scheduler is NOT invoked (assert via captured trace OR a test-only counter on the scheduler).
  - Polling iteration where `ensure_initialized` returns Ok: the audit scheduler IS invoked (existing behaviour).

## 6. Docs updates

- [ ] 6.1 In `docs/OPERATIONS.md`'s audits section, add a paragraph describing the workspace-validity gate. Operators reading about audits learn that audits are skipped (cleanly, with an INFO log) when the workspace isn't valid — they won't see broken-state side effects.
- [ ] 6.2 In `docs/TROUBLESHOOTING.md`, add an entry: "Audit log shows `audit skipped: workspace not in a valid state`. This is informational — the audit declined to run because the workspace is in a broken state. The iteration's workspace-init failure log (a few lines earlier) names the real problem. Fix the workspace init issue and the audit will run on its next cadence."

## 7. Spec delta

- [ ] 7.1 The ADDED requirement in `openspec/changes/audits-require-valid-workspace/specs/orchestrator-cli/spec.md` codifies: the per-audit workspace-validity check, the iteration-level gate on the audit scheduler, the `AuditOutcome::WorkspaceUnavailable` variant, the no-cadence-consumption rule on skip, and the no-chatops-notification rule on skip.

## 8. Verification

- [ ] 8.1 `cargo test` passes (new + existing).
- [ ] 8.2 `openspec validate audits-require-valid-workspace --strict` passes.
- [ ] 8.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
