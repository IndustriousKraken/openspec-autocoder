## Why

`sync-specs-detect-aborted-output` taught the rebuild path to treat openspec's `Aborted. No files were changed.` stdout marker as a failure regardless of exit code. The same class of silent skip exists on a second code path that was not covered: `queue::archive`, the helper that runs `openspec archive <change> -y` from the polling iteration's self-heal flow and from any other in-iteration archive call.

When the self-heal path hits a change whose deltas openspec refuses to apply (the most common cause being a `## MODIFIED Requirements` block whose target header does not exist in canonical state), the chain unfolds like this:

1. Polling iteration enumerates the change as pending (it sits at `openspec/changes/<slug>/` because a prior iteration's archive also silently skipped, or because an audit just created it with broken deltas).
2. Executor returns `Completed` with an empty diff (work was already merged on the base branch, or the executor reads "this change is already done").
3. Self-heal probe passes (`tasks.md` all `[x]`, `openspec validate <change> --strict` exits 0).
4. Self-heal calls `queue::archive`, which shells out to `openspec archive <change> -y`.
5. openspec prints `<delta> failed for header "..." - not found\nAborted. No files were changed.` to stdout and exits 0.
6. `queue::archive` checks only `out.status.success()` and returns `Ok(())`.
7. `git add -A` stages nothing (the archive move did not actually happen on disk).
8. `git::commit` runs; git writes "nothing to commit, working tree clean" to **stdout** and exits non-zero.
9. `run_git` captures only stderr (which is empty) and returns `Err("git commit failed: ")`.
10. The operator-facing error is `self-heal git commit failed: git commit failed:` with nothing after the colon. The actual cause (openspec refused to apply the delta) is invisible.
11. Two such iterations in a row trigger the perma-stuck marker. The operator is now stuck with an opaque error.

Two distinct gaps compound:

- **`queue::archive` does not check the post-condition** the way the rebuild path does. The same `Aborted.` marker is in the openspec output but never inspected; the same "did the change directory actually move" filesystem check is never performed.
- **`run_git` discards stdout from failed commands.** For most git failures stderr carries the message, but `git commit`'s "nothing to commit, working tree clean" is stdout-only. Operators see an empty error string.

Fixing only one of these leaves the other manifesting in some other context. Both fixes are small and belong together because they are how the same operator-visible perma-stuck loop gets honest reporting.

## What Changes

**Extract a shared `openspec_archive_with_postcondition` helper.** Move the post-condition + abort-marker detection logic out of the rebuild-specific code path into a shared helper that takes `(workspace, change_slug)` and returns a structured result (`Ok(archive_dir_path)` on real success, `Err(ArchiveFailure)` on any failure mode). Both callers — the rebuild loop and `queue::archive` — use this helper. The result of consolidation is one place to update if openspec's marker text changes, one place to update if a new failure mode appears, and a guarantee that both code paths apply the same rigour.

`ArchiveFailure` covers four cases:

- `NonZeroExit { code, stderr, stdout }` — openspec returned non-zero.
- `AbortedMarker { reason, full_output }` — exit 0 but stdout contained the `Aborted.` line. `reason` is the most informative preceding line, matching `sync-specs-detect-aborted-output`'s convention.
- `ActivePathStillPresent { path, full_output }` — exit 0 with no `Aborted.` marker, but `openspec/changes/<slug>/` still exists (the post-condition check finds the silent skip without a marker).
- `NoArchiveEntryFound { full_output }` — exit 0 with no marker AND active path is gone, but no `openspec/changes/archive/*-<slug>/` directory matches. Data-loss-shaped; should be very rare.

**`queue::archive` consumes the helper.** Replace the current direct `Command::new("openspec")` invocation with a call to `openspec_archive_with_postcondition`. On `Err(ArchiveFailure)`, return `Err(anyhow!("openspec archive failed: {detailed-message}"))` where the message names the failure variant AND includes the relevant openspec output excerpt. The self-heal caller's existing `Err` handling continues to fire; the only thing that changes is the error string is now actionable instead of empty.

**`run_git` captures stdout alongside stderr.** Extend the failure-path error message to include `stdout` when stderr is empty: `git {op} failed: {stderr_or_stdout}` where `stderr_or_stdout = if stderr.is_empty() { stdout } else { stderr }`. When both are non-empty, the message reads `git {op} failed: stderr: {stderr}; stdout: {stdout}`. The `Ok` path is unchanged (caller already has the `Output` if it cares about stdout on success).

**Net operator-visible effect.** The exact perma-stuck loop above now surfaces a clear cause at iteration 1. The failure message reads `self-heal: openspec archive failed: aborted by openspec — <cap> MODIFIED failed for header "..." - not found; full output: ...` instead of `self-heal git commit failed: git commit failed:`. Operators reading the chatops perma-stuck alert know what to fix in the broken spec delta.

## Impact

- **Affected specs:** `orchestrator-cli` — one ADDED requirement codifying the shared archive-with-postcondition contract and the git error-formatting rule. The existing rebuild-aborted-detection requirement remains; it now refers to the shared helper rather than duplicating the marker-detection scenario.
- **Affected code:**
  - New module-internal helper `openspec_archive_with_postcondition(workspace: &Path, slug: &str) -> Result<PathBuf, ArchiveFailure>` placed in `autocoder/src/openspec_archive.rs` (new file) so both `queue.rs` and `cli/sync_specs.rs` can call it without circular imports. The function performs: spawn `openspec archive <slug> -y`, capture stdout+stderr+exit, scan stdout for the `Aborted.` marker, verify the post-condition (active path gone + glob match for `archive/*-<slug>/`), return the structured result.
  - `autocoder/src/queue.rs` — `pub fn archive` now delegates to the new helper, maps `ArchiveFailure` to the existing `anyhow::Error` return type with a descriptive message.
  - `autocoder/src/cli/sync_specs.rs` — the rebuild loop's existing post-condition + marker-detection blocks are deleted and replaced by a single call to the new helper. Behaviour is unchanged for the rebuild path (same outcomes, same failure messages); the code path is now shared.
  - `autocoder/src/git.rs::run_git` — failure path includes stdout when stderr is empty, or both when both are non-empty. The function signature is unchanged.
  - Tests:
    - Unit tests on `openspec_archive_with_postcondition` using the existing `ArchiveRunner` trait injection pattern. Stubs for happy path, `Aborted.` marker, active-path-still-present silent skip, non-zero exit, data-loss case.
    - Unit test on `run_git`: feed a fake `Command` (or wrap via a trait) that exits 1 with stdout `"nothing to commit, working tree clean"` and empty stderr; assert the error message contains that stdout. Same shape for both-non-empty.
    - Integration test in the self-heal path: stub openspec to emit `Aborted.`, run the self-heal code path against a fixture, assert the iteration's failure reason contains `openspec archive failed: aborted by openspec — <reason>` rather than `git commit failed:`.

- **Operator-visible behavior:** the exact perma-stuck scenario above produces an honest error at iteration 1. Operators see openspec's actual abort reason; the perma-stuck loop is broken either by (a) the operator fixing the delta or (b) a higher-level retry policy (out of scope for this change).
- **Breaking:** no. The existing rebuild path's behaviour is unchanged (it had post-condition + marker detection; the consolidation does not lose any scenario). The self-heal path's behaviour gains the same coverage. `run_git` is more verbose on failure when stdout has content, but no caller depends on the old empty-stdout format.
- **Acceptance:** `cargo test` passes (new + existing). A self-heal iteration against a fixture archive whose openspec emits `Aborted.` produces a `Failed` outcome whose `reason` contains both `openspec archive failed` and the openspec-supplied cause line. A self-heal iteration where `git commit` would fail with "nothing to commit" produces a `Failed` outcome whose `reason` contains the "nothing to commit" text.
