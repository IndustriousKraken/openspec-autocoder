## Why

`workspace::ensure_initialized` treats "directory exists but contains no `.git/`" as a hard error: it returns `workspace path exists but is not a git repository (no .git directory): <path>` and the polling iteration fails. The next iteration sees the same state and fails the same way. The daemon never attempts recovery. The only way out is operator-side manual `rm -rf` of the workspace.

This produces a hard-stuck repo from a recoverable condition. The "exists but no `.git`" state has one realistic cause: a previous clone attempt partially populated the directory before failing (network drop, transient auth blip, signal interrupt, etc.). Git creates the destination directory and starts writing the working tree as part of its clone flow; if anything interrupts it before it finishes setting up `.git/`, the partial directory remains. There is no operator-meaningful data in this state — the directory was just created moments ago by a failed clone; nothing has been worked on, no markers exist, no commits live there.

A real-world incident: operator wiped a repo's workspace via `@<bot> wipe-workspace <repo>`. The next iteration's clone partially failed. Every subsequent iteration logged the same secondary error (`exists but is not a git repository`) without ever attempting to clean up and re-clone. Two operator wipes both succeeded at deleting the directory, but each was followed by another partial-clone-failure that re-created the same broken state. The operator concluded "wipe doesn't work" — but wipe was working; the daemon was just consistently failing to clone afresh, with no auto-recovery from the partial state.

The fix is small and structural. The detection is already in place; the recovery path is missing.

## What Changes

**`workspace::ensure_initialized` detects partial-clone state and auto-cleans.** When the workspace directory exists AND does NOT contain a `.git/` subdirectory, the daemon SHALL:

1. Log a WARN naming the workspace path and the auto-cleanup intent.
2. Delete the partial directory via `std::fs::remove_dir_all`.
3. Re-attempt the clone as if the workspace had not existed at all.
4. If the second clone succeeds → return Ok; iteration proceeds normally with a freshly-cloned workspace.
5. If the second clone ALSO fails → return Err with the SECOND clone's actual failure (not the secondary "exists but no .git" detection). The operator now sees the real underlying clone error (auth, network, etc.) in the log line and in any chatops alert that fires.

**Auto-cleanup safety check.** Before deleting, the daemon SHALL verify the partial directory contains NO entries that look like git-tracked working-tree content with operator-meaningful state. The realistic check: the directory must have no `.git/` (confirmed by the trigger) AND no `.in-progress*` lock files (would suggest an active iteration somehow racing this path) AND no `.perma-stuck.json` or `.needs-spec-revision.json` markers at any depth (which would suggest operator-meaningful state survived a previous successful clone). Files unrelated to the workspace's normal contents — like an orphan `.alert-state.json` from the post-failure alert path — are NOT a safety blocker since they're daemon-written rather than operator-written.

In practice, a partial-clone artifact from a failed `git clone` will contain only what git itself wrote (typically a top-level dir + whatever working-tree content git copied before failing). None of the safety-check tripwires apply, so the auto-cleanup proceeds.

If the safety check DOES trigger (highly unusual; would indicate the partial state came from something other than a failed clone), the daemon SHALL refuse to auto-clean and return the existing "exists but no .git" error with an extra hint: `... (partial cleanup refused: directory contains <tripwire description>; manual operator inspection required)`. This preserves operator data in the edge case where the broken state isn't actually a partial clone.

**WARN log structure.** The auto-cleanup WARN includes the workspace path, the trigger ("partial clone artifact detected"), and the action ("deleting and re-cloning"). Operators reading journalctl see exactly what happened and why. Pattern:

```
WARN workspace: <workspace-path> exists without .git; partial clone artifact detected. Deleting and re-cloning from <repo-url>.
```

If the auto-cleanup itself fails (permissions, disk full), the daemon logs at ERROR with the underlying OS error and returns Err. Recovery is no worse than today's behaviour in that pathological case.

**Iteration outcome reporting.** When auto-cleanup runs and the second clone succeeds, the iteration reports a normal Completed outcome (not a Failure with a recovery side-note). The auto-cleanup is internal plumbing; the operator-visible iteration succeeded. The WARN log is the only externally-visible signal that recovery happened.

When auto-cleanup runs and the second clone fails, the iteration reports Failed with the real clone error. This is operator-actionable: they see "auth failed" or "network unreachable" or whatever, not the misleading "exists but no .git" message that previously hid the cause.

**Implicit benefit: wipe-workspace becomes idempotently-recoverable.** With auto-cleanup in place, the wipe → partial-clone-failure → stuck-state loop the user hit cannot recur. Each polling iteration after a failed clone either succeeds the re-clone or surfaces the real failure reason; no iteration is silently stuck on a stale partial-state detection.

## Impact

- **Affected specs:** `workspace-manager` — one ADDED requirement covering the partial-clone detection, the safety check, the auto-cleanup, the re-clone attempt, and the failure-reporting contract.
- **Affected code:**
  - `autocoder/src/workspace.rs::ensure_initialized` — at the existing "exists but no .git" detection point, run the safety check; if safe, log WARN, `fs::remove_dir_all`, re-call the clone path; if not safe, return Err with the extended hint.
  - New private helper `fn safe_to_auto_clean(workspace: &Path) -> Result<(), &'static str>` returning `Ok(())` when the directory is structurally a partial-clone artifact OR `Err(tripwire_description)` when it isn't. The function checks for `.in-progress*` files at any depth, `.perma-stuck.json` / `.needs-spec-revision.json` at any depth under `openspec/changes/`, and any `*.json` answer/question marker pattern. The `.alert-state.json` at the root is explicitly NOT a tripwire.
  - Tests:
    - Fixture: directory exists, no `.git/`, no other content → `ensure_initialized` runs auto-cleanup AND re-attempts clone. Assert the WARN log fires (use `tracing-test`).
    - Fixture: directory exists, no `.git/`, `openspec/changes/foo/proposal.md` present (a partial-clone artifact where git got far enough to write tree content) → auto-cleanup proceeds; nothing in the partial tree is operator-meaningful since markers weren't there.
    - Fixture: directory exists, no `.git/`, `openspec/changes/foo/.perma-stuck.json` present → safety check triggers, auto-cleanup refused, Err returned with the partial-cleanup-refused hint.
    - Fixture: directory exists, no `.git/`, `.alert-state.json` present at root → auto-cleanup proceeds (alert-state is daemon-written, not a tripwire).
    - Fixture: re-clone fails after auto-cleanup → Err returned with the real clone error, not the secondary "exists but no .git" detection.
    - Permissions failure during `fs::remove_dir_all` → ERROR log, Err returned, no infinite loop.

- **Operator-visible behavior:** the "wipe-then-stuck-on-partial-clone" failure mode is gone. Iterations after a wipe (or after any other condition that produces a partial-clone artifact) either succeed via auto-cleanup-then-re-clone OR surface the real clone failure. Operators reading journalctl during a partial-clone recovery see a clear WARN explaining what happened.
- **Breaking:** no. Workspaces that were already in a clean state (either present-with-`.git` OR absent entirely) take the same code path as today. The auto-cleanup fires only in the previously-stuck case, which today produces a hard error.
- **Acceptance:** `cargo test` passes (new + existing). A fixture polling iteration against a workspace whose directory exists but has no `.git/` succeeds via auto-cleanup + re-clone, and the next iteration finds a normal workspace and proceeds without further intervention.
