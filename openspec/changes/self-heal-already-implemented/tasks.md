## 1. Helpers

- [x] 1.1 Add `pub fn tasks_md_all_complete(workspace: &Path, change: &str) -> Result<bool>` to `polling_loop.rs` (or a small new helper module). Read `openspec/changes/<change>/tasks.md`. Scan each line with regex `^\s*-\s*\[([ x])\]`. Return `Ok(true)` iff at least one match is present AND every match captures `x`. Return `Ok(false)` if any match captures ` ` (space). Return `Err(_)` only on file-read failure or regex-init failure; an empty match-set yields `Ok(false)` (no tasks recorded = not complete).
- [x] 1.2 Add `pub fn openspec_validate_strict_passes(workspace: &Path, change: &str) -> bool` that runs `Command::new("openspec").args(["validate", change, "--strict"]).current_dir(workspace).output()`. Returns `true` iff the call succeeded AND exit status is 0. Any error (binary missing, non-zero exit, transport) returns `false` — the caller will fall through to Failed when self-heal preconditions are unmet, which is the conservative path.

## 2. Wire self-heal into handle_outcome

- [x] 2.1 In `polling_loop::handle_outcome`, the existing `ExecutorOutcome::Completed` branch already has an `if dirty.is_empty() { ... Failed ... }` arm (introduced by `no-op-completion-is-failure`). Before the Failed return, add the self-heal check: call `tasks_md_all_complete(workspace, change)?` AND `openspec_validate_strict_passes(workspace, change)`. If both true, take the self-heal path (§2.2). Otherwise return Failed as today.
- [x] 2.2 Self-heal action sequence:
    1. Log `tracing::info!(url=%repo.url, change=%change, "self-heal: implementation already in HEAD, archiving")`.
    2. Run the archive move via `queue::archive(workspace, change)`. This renames the change directory into `openspec/changes/archive/<YYYY-MM-DD>-<change>/`.
    3. Build the commit subject `format!("archive: {change}: implementation already in base")`.
    4. `git::add_all(workspace)?` and `git::commit(workspace, &subject)?`.
    5. Return `Ok(QueueStep::Archived)` so the change name flows into `processed` and the iteration proceeds through the normal push + PR steps.
- [x] 2.3 If `queue::archive` or the commit fails, log ERROR with the underlying error and return `Failed` — the change stays pending and the next pass retries (or human investigates). Self-heal is best-effort; an archive-step failure is not catastrophic.

## 3. PR body annotation for self-heal passes

- [x] 3.1 The PR body for a pass that includes self-healed changes needs the leading paragraph: `_This PR archives one or more changes whose implementation was already present on the base branch. No code diff is included; only the openspec archive move._` Easiest approach: track a `bool` (`includes_self_heal`) alongside `processed: Vec<String>` through `run_pass_through_commits` → `execute_one_pass` → `build_pr_body`. When set, prepend the paragraph to the existing body.
- [x] 3.2 If a pass mixes self-heal changes AND normally-implemented changes, the paragraph still appears once at the top — operators reading the PR see the disclaimer regardless.

## 4. Tests

- [x] 4.1 `polling_loop::tests::tasks_md_all_complete_*` — unit tests for the helper:
    - All `[x]` → true
    - Mix of `[x]` and `[ ]` → false
    - All `[ ]` → false
    - Empty `tasks.md` (no `- [` lines) → false
    - Missing file → Err
- [x] 4.2 `polling_loop::tests::self_heal_archives_when_preconditions_met` — fixture workspace where:
    - A change directory exists with all tasks `[x]` and a valid `proposal.md`.
    - `openspec validate <change> --strict` exits 0.
    - Executor returns `Completed` without modifying the workspace.
    
    Assert: change is archived (active dir gone, archive dir present), a commit was made on agent-q with the spec-mandated subject, the change name is in `processed`, and the PR body (via `build_pr_body`) contains the self-heal disclaimer paragraph.
- [x] 4.3 `polling_loop::tests::self_heal_falls_through_to_failed_when_tasks_incomplete` — same fixture but one task is `[ ]`. Assert: change is NOT archived, change is back in pending, outcome is Failed.
- [x] 4.4 `polling_loop::tests::self_heal_falls_through_when_openspec_validate_fails` — same fixture but `openspec validate` will error (e.g. `tasks.md` malformed). Assert: change is NOT archived, outcome is Failed.
- [x] 4.5 `polling_loop::tests::self_heal_paragraph_omitted_when_no_self_heals_in_pass` — fixture with a normal Completed-with-diff change AND no self-heal change. Assert: PR body does NOT include the self-heal disclaimer.

## 5. Documentation

- [x] 5.1 README "Operating Notes": add a brief subsection "Self-heal for already-implemented changes" describing the trigger criteria, what the resulting PR looks like, and that the disclaimer paragraph identifies these passes. No kitschy framing.

## 6. Verification

- [x] 6.1 `cargo test` passes.
- [x] 6.2 `openspec validate self-heal-already-implemented --strict` passes.
