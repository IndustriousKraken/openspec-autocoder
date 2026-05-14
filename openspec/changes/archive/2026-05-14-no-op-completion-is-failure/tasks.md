## 1. Code: treat Completed-with-clean-workspace as Failed

- [x] 1.1 In `polling_loop::handle_outcome`, the `ExecutorOutcome::Completed` branch (around line 454): when `dirty.is_empty()`, change the behavior from "log warning + fall through to archive" to "log warning + return `Ok(QueueStep::Failed)`". The lock has already been removed at line 459 so the change re-enters pending on the next pass. Log message: `"agent reported Completed for `{change}` without modifying the workspace; marking Failed"`.
- [x] 1.2 Apply the symmetric fix to the resume path (around line 300, search for the matching `archiving anyway per spec` log message). Same shape: empty workspace → return `QueueStep::Failed` instead of archiving. Log message: `"resume of `{change}` returned Completed without modifying the workspace; marking Failed"`.
- [x] 1.3 Remove the now-dead "archiving anyway per spec" strings from both call sites.

## 2. Tests

- [x] 2.1 Add `polling_loop::tests::completed_with_empty_workspace_is_failed`. Use the existing `FixtureExecutor` (or equivalent test scaffolding) that returns `Completed` without touching the workspace. Assert `handle_outcome` returns `Ok(QueueStep::Failed)`, the change's `.in-progress` file is gone, and the archive directory does NOT contain the change.
- [x] 2.2 Add `polling_loop::tests::resume_with_empty_workspace_is_failed` covering the resume code path with the same shape (Completed-from-resume + clean workspace → Failed, no archive).
- [x] 2.3 **Verify:** existing `polling_loop::tests` continue to pass — in particular `commit_subject_matches_spec_format` (Completed + non-empty dirty → commit + archive) and the lazy-archive suite (Completed + archive-only renames → Failed). The new behavior strictly applies to the previously-unguarded empty-workspace case.

## 3. Verification

- [x] 3.1 `cargo test` passes; net new tests = 2.
- [x] 3.2 `openspec validate no-op-completion-is-failure --strict` passes.
