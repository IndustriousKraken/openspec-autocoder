# Tasks

## 1. Config extensions

- [ ] 1.1 In `autocoder/src/config.rs`, extend `SpecStorageConfig` with:
  - `push_remote: Option<String>` (default `None`; runtime resolves to `"origin"` if unset).
  - `base_branch: Option<String>` (default `None`; runtime resolves to remote-tracked default branch if unset).
- [ ] 1.2 Add `skip_spec_only_prs: bool` to `ReviewerConfig` (default `false`).
- [ ] 1.3 Config-load: when `spec_storage.push_remote` is set, verify the remote exists in `git -C <spec_storage.path> remote` output. Fail-fast with a clear message if not.
- [ ] 1.4 Document all three new fields in `config.example.yaml` with examples covering the defaults AND the override cases.
- [ ] 1.5 Unit-test: default-config round-trip preserves the new fields as documented defaults.

## 2. Git helpers for arbitrary working trees

- [ ] 2.1 In `autocoder/src/git.rs`, add `commit_in_tree(tree_path: &Path, message: &str) -> Result<String>` that runs `git -C <tree_path> commit -m <message>` AND returns the commit SHA.
- [ ] 2.2 Add `push_in_tree(tree_path: &Path, remote: &str, branch: &str, force: bool) -> Result<()>` that runs `git -C <tree_path> push [--force] <remote> <branch>`.
- [ ] 2.3 Add `default_branch_for_remote(tree_path: &Path, remote: &str) -> Result<String>` that returns the remote-tracked default branch (e.g. `git -C <tree_path> symbolic-ref refs/remotes/<remote>/HEAD` parsed to extract the branch name).
- [ ] 2.4 Unit-test each helper against a temp git repo fixture.

## 3. PR creation with explicit `--repo`

- [ ] 3.1 In `autocoder/src/github.rs`, extend the `create_pr` helper (OR its equivalent shape) with an optional `repo: Option<&str>` parameter. When `Some(owner/name)`, the underlying `gh pr create` invocation receives `--repo <owner>/<name>`. When `None`, the existing behavior is preserved (no `--repo` flag → `gh` uses the current working tree's origin).
- [ ] 3.2 Unit-test: `create_pr` with `repo: Some("foo/bar")` records `--repo foo/bar` in the captured `gh` argv.
- [ ] 3.3 Unit-test: `create_pr` with `repo: None` does NOT include `--repo` in the argv (regression).

## 4. Polling-iteration routing

- [ ] 4.1 In `autocoder/src/polling_loop.rs` (OR equivalent), the iteration's commit + push + PR step gains a "detect working-tree state" prelude:
  - Run `git -C <code_workspace> status --porcelain` AND check for non-empty output.
  - When `spec_storage` is configured, run `git -C <spec_storage.path> status --porcelain` AND check for non-empty output.
  - Classify the iteration's outcome as:
    - **Code-only**: code workspace dirty, spec_storage clean (OR not configured).
    - **Spec-only**: code workspace clean, spec_storage dirty.
    - **Dual-tree**: both dirty.
- [ ] 4.2 Code-only path: existing behavior. No change.
- [ ] 4.3 Spec-only path: 
  - Resolve push_remote (config OR `"origin"`).
  - Resolve base_branch (config OR `default_branch_for_remote`).
  - Resolve spec-repo `<owner>/<name>` from `git -C <spec_storage.path> remote get-url <remote>` (parse SSH OR HTTPS URL).
  - `commit_in_tree(spec_storage.path, <impl-summary-message>)`.
  - `push_in_tree(spec_storage.path, push_remote, agent_branch, force: true)`.
  - On `auto_submit_pr: true`: `create_pr(..., repo: Some(spec-owner/name), base: base_branch, title: "[specs] <change-list-summary>", ...)`.
  - On `auto_submit_pr: false`: post the canonical `gh pr create --repo <spec-owner>/<name> --base ...` notification per the chatops-manager deltas (no new requirement; the existing "auto_submit_pr: false produces a gh pr create suggestion" requirement applies, with the `--repo` argument added).
- [ ] 4.4 Dual-tree path:
  - Run the code-only path against the code workspace (existing logic, existing commit message format).
  - Run the spec-only path against the spec_storage (per 4.3).
  - Two PRs result. Both fire their own chatops notifications independently.

## 5. PR title prefix

- [ ] 5.1 In the PR-title construction logic, when the iteration's outcome is spec-only OR the dual-tree path's spec-storage half is being processed, prefix the title with `[specs] `. Code-only PRs are unprefixed.
- [ ] 5.2 Unit-test: spec-only iteration produces PR title `[specs] <change-list-summary>`.
- [ ] 5.3 Unit-test: code-only iteration produces unprefixed PR title (regression).
- [ ] 5.4 Unit-test: dual-tree iteration produces TWO PRs — the code PR unprefixed, the spec PR prefixed `[specs] `.

## 6. Reviewer skip-spec-only-prs gate

- [ ] 6.1 In the polling-iteration's reviewer-invocation step, when `reviewer.skip_spec_only_prs: true` AND the PR's diff is entirely under `openspec/`, skip the reviewer call AND post no `## Code Review` section. Log at INFO `reviewer: skipping spec-only PR per skip_spec_only_prs config`.
- [ ] 6.2 Unit-test: `skip_spec_only_prs: true` AND a brownfield iteration's PR has only `openspec/changes/<change>/...` diff → reviewer is NOT invoked.
- [ ] 6.3 Unit-test: `skip_spec_only_prs: true` AND a dual-tree iteration's code PR has `autocoder/src/foo.rs` diff → reviewer IS invoked (the gate applies only to spec-only PRs).
- [ ] 6.4 Unit-test: `skip_spec_only_prs: false` (default) → reviewer is invoked for spec-only PRs (preserves canonical reviewer-runs-once behavior).

## 7. Integration test

- [ ] 7.1 Add an integration test that exercises the spec-only commit + push + PR path end-to-end:
  - Construct a code workspace AND a sibling spec_storage workspace (both as git repos with `origin` remotes pointing at temp bare repos).
  - Configure `spec_storage.path` pointing at the spec_storage workspace.
  - Run a brownfield iteration (stubbed executor returning a brownfield draft).
  - Assert: the spec_storage workspace has one new commit; the code workspace has no commits; the spec_storage bare-repo's `agent_branch` ref matches the new commit; the captured `gh pr create` argv includes `--repo <spec-owner>/<name>` AND `--title "[specs] ..."`.

## 8. Validation

- [ ] 8.1 `cargo test` passes.
- [ ] 8.2 `cargo clippy` produces no NEW warnings against the existing baseline.
- [ ] 8.3 `openspec validate a34-spec-storage-commit-push-pr-routing --strict` passes.
