## Why

`handle_outcome`'s regular Archived path does `git::add_all` → `git::commit` → `queue::archive` in that order. Because `queue::archive` is a `std::fs::rename` (not a `git mv`), the rename leaves the working tree dirty: deleted entries under the original `openspec/changes/<name>/` path and an untracked directory at `openspec/changes/archive/<YYYY-MM-DD>-<name>/`. When N changes archive in one pass, the next iteration's `add_all` *happens to* stage the previous archive rename alongside the next change's implementation, masking the bug for changes 1..N-1. But the **last change of every pass** has nothing following it — its archive rename is never committed, never pushed, never in the PR.

This bites observable state in production: a pass that archives one change (single change in queue, OR the last change of a multi-change pass that runs cap=N or runs the queue dry) leaves the local workspace permanently dirty. The pushed PR shows the implementation but not the archive move. After the operator merges the PR, the local working tree still has the dangling rename. The next polling iteration's dirty-workspace check refuses to proceed; the daemon spins forever logging ERROR.

The self-heal path (`polling_loop.rs:1267-1296`) already has the correct order (`archive` → `add_all` → `commit`); only the regular Archived path is buggy.

## What Changes

- **MODIFIED capability:** `git-workflow-manager`'s "Serial commit per change" requirement. The commit SHALL include both the executor's implementation files AND the archive move of the change directory, in a single commit. After commit, the working tree SHALL be clean.
- **Code:** In `polling_loop.rs::handle_outcome`, swap the order of `queue::archive` and the `add_all`/`commit` pair in the regular Archived branch. The result mirrors the self-heal path:
  ```rust
  let subject = build_commit_subject(workspace, change)?;
  queue::archive(workspace, change)?;
  git::add_all(workspace)?;
  git::commit(workspace, &subject)?;
  ```
  The `has_executor_changes` and `is_lazy_archive` checks still run BEFORE the archive (no change), so their semantics are unaffected. `build_commit_subject` reads `openspec/changes/<change>/proposal.md` — it must run before the archive rename moves that file.

## Impact

- Affected specs: `git-workflow-manager` (one MODIFIED requirement).
- Affected code: `autocoder/src/polling_loop.rs::handle_outcome` (three lines reordered in the regular Archived branch). No new types, no signature changes.
- Behavior change: the iteration's commit count is unchanged (still one commit per archived change); each commit now contains both the implementation diff and the archive rename. PRs are now fully self-contained — merging a PR leaves no dangling local state.
- Operator action for the currently-stuck daemon: `sudo systemctl restart autocoder`. The startup `repo_passes_startup_check` runs `git reset --hard origin/<base> + git clean -fd`, which scrubs the dangling archive rename. After restart, iterations proceed normally.
- Breaking: no. Existing PRs already merged are unaffected; their workspaces will be scrubbed at next daemon restart. Future PRs will be self-contained.
