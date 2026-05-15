## 1. Code

- [x] 1.1 In `autocoder/src/polling_loop.rs::handle_outcome`, find the regular Archived branch (currently around line 1318-1324):
  ```rust
  } else {
      let subject = build_commit_subject(workspace, change)?;
      git::add_all(workspace)?;
      git::commit(workspace, &subject)?;
  }
  queue::archive(workspace, change)?;
  Ok(QueueStep::Archived)
  ```
- [x] 1.2 Reorder so the archive happens BEFORE the add+commit, inside the `else` branch. `build_commit_subject` must stay BEFORE the archive (it reads `openspec/changes/<change>/proposal.md`). Result:
  ```rust
  } else {
      let subject = build_commit_subject(workspace, change)?;
      queue::archive(workspace, change)?;
      git::add_all(workspace)?;
      git::commit(workspace, &subject)?;
  }
  Ok(QueueStep::Archived)
  ```
- [x] 1.3 Confirm: the `has_executor_changes` and `is_lazy_archive` checks earlier in the function operate on the pre-archive `dirty` string, so their behavior is unchanged.

## 2. Tests

- [x] 2.1 `polling_loop::tests::archived_change_leaves_clean_working_tree` — fixture: one pending change, executor archives it (e.g. `PerChangeArtifactExecutor`). After `run_pass_through_commits`, assert `git status --porcelain` is empty (no `D` lines, no `??` entries under `openspec/changes/archive/`).
- [x] 2.2 `polling_loop::tests::commit_contains_both_impl_and_archive_rename` — fixture: one pending change. After the pass, inspect `git diff-tree --no-commit-id --name-status -r -M HEAD^..HEAD`. Assert the commit has: the executor's artifact path (as `A`), AND the renamed proposal.md (showing as `R` if git detects the rename, or as `D openspec/changes/<name>/...` + `A openspec/changes/archive/...` pair).
- [x] 2.3 `polling_loop::tests::multi_change_pass_clean_after_each` — fixture: 3 pending changes, cap=u32::MAX. After the pass, assert: (a) 3 commits ahead of main, (b) `git status --porcelain` is empty.
- [x] 2.4 **Verify:** the existing `commit_subject_matches_spec_format` test (which inspects HEAD's subject on agent-q after a one-change pass) still passes. `build_commit_subject` reads `proposal.md` BEFORE the archive rename — should be identical behavior. Confirmed: full suite passes 367/368 (1 ignored, unrelated).

## 3. Verification

- [x] 3.1 `cargo test` passes.
- [x] 3.2 `openspec validate commit-trailing-archive --strict` passes.
