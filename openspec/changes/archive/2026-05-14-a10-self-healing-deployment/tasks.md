## 1. Sandbox: block openspec state-mutation commands

- [x] 1.1 Extend `default_disallowed_bash_patterns` in `src/config.rs` with `"openspec archive:*"` and `"openspec unarchive:*"` (preserving the existing entries). Defense in depth; the structural detection in section 3 is the real protection.
- [x] 1.2 **Verify:** `config::tests::sandbox_default_blocks_openspec_archive` asserts the two new patterns are present in `default_disallowed_bash_patterns()`.

## 2. Startup workspace auto-recovery

- [x] 2.1 In `cli::run::repo_passes_startup_check`, replace the dirty → return-false branch with a recovery attempt:
    1. Log `warn` "workspace dirty at startup; attempting recovery" with the dirty count.
    2. `git checkout <repo.base_branch>` (best-effort; log debug on failure).
    3. `git reset --hard origin/<repo.base_branch>` — propagate errors.
    4. `git clean -fd` — propagate errors.
    5. Re-run `git status_porcelain`. If clean, log `info` "workspace recovered" and return true. If still dirty, fall through to the existing skip-for-lifetime log + return false.
- [x] 2.2 Add the helper functions to `git.rs` if not already present:
    - `pub fn reset_hard_to_remote(workspace, base_branch) -> Result<()>` — runs `git reset --hard origin/<base>`.
    - `pub fn clean_force(workspace) -> Result<()>` — runs `git clean -fd`.
    Reuse existing `git::checkout` for the branch switch.
- [x] 2.3 **Verify:** `cli::run::tests::dirty_workspace_recovers_at_startup` — fixture with a dirty workspace (untracked file + modified file); after startup check, `git status` is clean and the function returned `true`.
- [x] 2.4 **Verify:** preserve existing `dirty_workspace_skipped_at_startup` semantics by adding a test where the workspace cannot be cleaned (e.g. `git reset` errors due to a corrupt workspace) and asserting `false` is returned.

## 3. Post-execution lazy-archive detection

- [x] 3.1 Add `pub fn detect_lazy_archive(workspace: &Path) -> Result<bool>` to `polling_loop.rs`. Implementation: run `git status --porcelain` (already wrapped as `git::status_porcelain`); parse each line into `(status_code, paths)`; return true iff the output is non-empty AND every line is a rename (status starts with `R`) AND every rename's destination path begins with `openspec/changes/archive/`.
- [x] 3.2 Wire it into the executor-outcome handling in `polling_loop.rs`. After the executor returns Completed, BEFORE the existing `git status --porcelain → commit` branch, check `detect_lazy_archive(workspace)?`. If true:
    1. Log `warn` "agent appears to have archived `<change>` without implementing; reverting and marking Failed".
    2. Run `git reset --hard HEAD` via a new `git::reset_hard_head` helper (analogous to `reset_hard_to_remote` but resets to current HEAD, discarding the index).
    3. Treat the outcome as `ExecutorOutcome::Failed { reason: "agent appears to have archived without implementing the change".into() }`.
    4. The existing Failed code path then unlocks the change for retry.
- [x] 3.3 Parsing detail: `git status --porcelain` rename lines look like `R  old_path -> new_path`. Implementation parses status codes that contain `R` in either the staged or unstaged columns. Mixed status (some renames + non-rename changes) is NOT a lazy archive — only treat as lazy when ALL entries are archive-renames.
- [x] 3.4 **Verify:** new tests in `polling_loop::tests`:
    - `detect_lazy_archive_returns_true_when_only_archive_renames` — fixture stages only `R  openspec/changes/foo -> openspec/changes/archive/2026-MM-DD-foo`; assert true.
    - `detect_lazy_archive_returns_false_when_real_implementation_present` — fixture stages an archive rename PLUS a modification to `src/foo.rs`; assert false.
    - `detect_lazy_archive_returns_false_when_workspace_clean` — empty status → false.
    - `lazy_archive_outcome_overridden_to_failed` — integration: fixture executor that calls `git mv` to archive a change and exits 0; assert the daemon treats this as Failed, reverts the staged moves, and unlocks the change.

## 4. Documentation

- [x] 4.1 README's Deployment §2 (Create deploy user and authenticate Claude Code) gains an `openspec` install step. Suggested: `sudo -iu autocoder bash -c 'curl -fsSL https://raw.githubusercontent.com/Fission-AI/OpenSpec/main/install.sh | bash'` OR `sudo -u autocoder npm install -g @fission-ai/openspec` (whichever matches the project's recommended install path). Then verify with `sudo -u autocoder openspec --version`.
- [x] 4.2 README's Deployment §2 also documents the required git config: `sudo -u autocoder git config --global user.email autocoder@example.com` and `user.name`. Without these, commits fail at the iteration step.
- [x] 4.3 README's AI Security §8 (executor sandbox) gains a paragraph explaining the lazy-archive detection: structural, pattern-matches archive-rename diffs, complements the sandbox CLI-pattern denials.
- [x] 4.4 README's Operating Notes (recovery section) documents that dirty workspaces now auto-recover at startup, and that operators wishing to inspect a dirty workspace should stop the systemd unit first.

## 5. Verification

- [x] 5.1 `cargo test` passes with no regressions; test count grows by at least: 1 config + 2 startup-recovery + 4 lazy-archive = ~7 new tests.
- [x] 5.2 `cargo build --release` produces a binary that, given a repo whose workspace is dirty from a prior crash, recovers and proceeds; and that catches an executor's attempt to archive without implementing and treats it as Failed.
- [x] 5.3 `openspec validate self-healing-deployment --strict` passes.
