## Why

Production failure observed 2026-05-14: agent returned `Completed` for `chatops-progress-notifications` without modifying the workspace. autocoder logged a warning ("workspace is clean; archiving anyway per spec") and archived the change anyway. The change was not implemented.

The current behavior is a fossil. An earlier draft of the executor spec assumed "Completed + clean workspace" meant "this change was already implemented as a side effect of a sibling change, archive it as a no-op." In practice with Claude that never happens — empty workspace after Completed always means the agent gave up, hit a turn limit, or decided the change was already done when it wasn't.

This is the same failure class as lazy-archive (agent claims success without producing an artifact). The fix is symmetric: treat empty-workspace-after-Completed as Failed, leave the change unlocked, retry on the next poll.

## What Changes

- **MODIFIED capability:** `git-workflow-manager` — the "Completed but produced no diff" scenario stops being a no-op archive and becomes a Failed outcome.
- **MODIFIED capability:** `orchestrator-cli` — the "Workspace is clean (no changes at all)" scenario under the lazy-archive requirement is updated to point at the new Failed handling instead of the removed "archive without commit" path.
- **Code:** `polling_loop::handle_outcome` — the `ExecutorOutcome::Completed` branch's `dirty.is_empty()` arm returns `QueueStep::Failed` instead of falling through to `queue::archive`. Same fix in the resume path (the second occurrence at ~line 300).
- **Tests:** new `polling_loop::tests::completed_with_empty_workspace_is_failed` and matching resume-path test asserting the change is unlocked and stays pending.

## Impact

- Affected specs: `git-workflow-manager`, `orchestrator-cli`
- Affected code: `autocoder/src/polling_loop.rs` (handle_outcome + the resume handler)
- Breaking? No — strictly tighter behavior. A genuine no-op change (which has never been observed) would now fail and retry rather than silently archive; an operator who wants to archive an actually-finished-but-empty change should do it manually via `openspec archive`.
