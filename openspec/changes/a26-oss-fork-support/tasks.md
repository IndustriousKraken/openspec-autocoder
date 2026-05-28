## 1. Config schema extension

- [ ] 1.1 In `autocoder/src/config.rs`, extend the per-repo config struct with:
  - `spec_storage: Option<SpecStorageConfig>` where `SpecStorageConfig { path: String }`.
  - `upstream: Option<UpstreamConfig>` where `UpstreamConfig { remote: String (default "upstream"), branch: String (default "main"), url: String }`.
  - `auto_submit_pr: bool` (default `true`).
- [ ] 1.2 Config-load validation:
  - When `spec_storage` is present: resolve `path` (workspace-relative OR absolute), verify the directory exists, verify it contains a `.git` subdirectory (OR is a valid git working tree per `git -C <path> rev-parse --is-inside-work-tree`). Verify `<path>/openspec/` exists. Fail-fast on any check failure with a clear error.
  - When `upstream` is present: verify `url` is a non-empty string. (Reachability is NOT checked at config-load — that's the polling iteration's concern.)
- [ ] 1.3 Tests: each field round-trips through serde; each validation failure produces the expected error; default values resolve correctly when fields are omitted.

## 2. SpecRoot resolver

- [ ] 2.1 New module `autocoder/src/workspace/spec_root.rs` exposing `SpecRoot { code_workspace: PathBuf, spec_root_dir: PathBuf }` where `spec_root_dir` is `code_workspace.join("openspec")` (default) OR `spec_storage.path.join("openspec")` (when configured).
- [ ] 2.2 Public methods: `canonical_specs_dir()`, `changes_dir()`, `archive_dir()`. Each composes the spec root with the standard suffix.
- [ ] 2.3 Refactor every existing call site that constructs paths under `<workspace>/openspec/...`:
  - Implementer prompt's canonical-spec reads.
  - Audit framework's spec discovery.
  - `openspec validate` invocation paths.
  - Brownfield draft writes.
  - Scout spec-it triage writes.
  - `openspec archive` invocations.
- [ ] 2.4 Tests:
  - Resolver returns workspace-internal paths when `spec_storage` unset.
  - Resolver returns external-path-based paths when `spec_storage` set.
  - Each refactored call site uses the resolver correctly.

## 3. Spec-storage commit/push/PR routing

- [ ] 3.1 When `spec_storage` is configured AND a polling iteration produces spec changes (brownfield, scout spec-it, archive), the iteration SHALL:
  - Commit the changes in the spec_storage git working tree (NOT the code workspace).
  - Determine the spec_storage repo's remote AND base branch via `git -C <spec_storage.path> remote -v` + the existing base-branch-resolution mechanism applied to the spec_storage repo's config (`spec_storage` may borrow the parent repo's `base_branch` field OR have its own; v1 reuses the parent's).
  - Apply `auto_submit_pr` per the standard rule: when true, push the spec-storage branch AND open a PR against the spec_storage repo's base branch; when false, push AND post the branch URL + `gh pr create` suggestion.
- [ ] 3.2 The spec-storage PR uses the standard reviewer + implementer-summary mechanics inherited from `git-workflow-manager`.
- [ ] 3.3 Tests:
  - With `spec_storage` set AND `auto_submit_pr: true`, a brownfield iteration creates a PR in the spec_storage repo, not the code workspace.
  - With `spec_storage` set AND `auto_submit_pr: false`, the spec branch is pushed but no PR is created.

## 4. Opportunistic upstream fetch

- [ ] 4.1 In the polling iteration's startup sequence (after the existing `git fetch origin`), when `upstream` is configured:
  - Ensure the workspace has a remote named `upstream.remote` pointing at `upstream.url`. If absent, add it via `git remote add`. If present with a different URL, update it via `git remote set-url`.
  - Run `git fetch <upstream.remote>` with a 30-second timeout.
  - On failure (timeout, network, auth), log a WARN naming the failure AND continue with the iteration. The fetch is best-effort.
- [ ] 4.2 Tests:
  - Upstream-absent: opportunistic fetch is skipped, no remote-management calls fire.
  - Upstream-configured-missing-remote: remote is added, fetch runs.
  - Upstream-configured-wrong-url: remote URL is corrected, fetch runs.
  - Upstream-fetch-failure: WARN is logged, iteration proceeds.

## 5. sync-upstream chatops verb

- [ ] 5.1 In the chatops inbound listener, add `sync-upstream` to the recognized verb list. Parse `@<bot> sync-upstream <repo-substring>` per the existing match rule. Emit `SyncUpstreamAction { repo_url, channel, thread_ts, request_id }`.
- [ ] 5.2 In `autocoder/src/control_socket/actions.rs`, add `SyncUpstreamAction` variant.
- [ ] 5.3 New module `autocoder/src/polling/sync_upstream.rs` exposing `handle_sync_upstream(action, repo_state, daemon_ctx) -> Result<()>`. Behavior:
  - Verify `upstream` is configured for the repo; if not, post `✗ sync-upstream: no upstream configured for this repo. Set the upstream block in config.yaml.` AND return.
  - Verify the workspace is not currently busy with another iteration (existing busy-marker rule); if busy, queue OR refuse per the established convention.
  - Acquire the workspace busy marker for the sync operation.
  - Run `git fetch <upstream.remote>` with a 60-second timeout.
  - Identify the base branch (the configured base, typically `main`).
  - Checkout the base branch.
  - Run `git rebase <upstream.remote>/<upstream.branch>`.
  - On conflict: run `git rebase --abort`; post `✗ sync-upstream: rebase conflict on <files>. Aborted. Resolve manually in the workspace AND re-run, OR merge manually.` AND release the busy marker.
  - On success: count commits via `git rev-list --count <previous-head>..HEAD` AND `git rev-list --count <upstream.remote>/<upstream.branch>..HEAD` (to indicate ahead status); post `✓ sync-upstream: pulled <N> commit(s) from <upstream.remote>/<upstream.branch>. Workspace is <M> commit(s) ahead.` AND release the busy marker.
  - The handler SHALL NOT push the rebased base branch (the operator decides when to push to origin/their fork).
- [ ] 5.4 Tests:
  - Verb-parse happy path AND ambiguous-repo path.
  - Handler refuses with the misconfiguration reply when upstream is absent.
  - Handler completes happy-path rebase AND posts the success reply.
  - Handler handles conflict path AND posts the conflict reply.
  - Handler releases the busy marker even on conflict.

## 6. auto_submit_pr gate in git-workflow-manager

- [ ] 6.1 In the PR-creation module, branch on `auto_submit_pr`:
  - `true` (default): existing behavior unchanged — push AND open PR per the canonical "Monolithic PR at end of pass" requirement.
  - `false`: push the branch per the existing rules (direct-push OR fork-PR mode), then RETURN `PullRequestOutcome::BranchPushedNoPr { branch_url, suggested_pr_command }` to the caller. The `suggested_pr_command` is templated as `gh pr create --base <upstream-branch> --head <branch-name>` where `<upstream-branch>` is the configured upstream branch if `upstream` is configured, OR the workspace's base branch otherwise.
- [ ] 6.2 Update the polling iteration's chatops notification step:
  - On `PullRequestOpened`: post the existing `✅ PR opened: <url>` message.
  - On `BranchPushedNoPr`: post `📦 Branch pushed: <branch-url>\nRun: <suggested-pr-command>`.
- [ ] 6.3 Tests:
  - `auto_submit_pr: true` produces the standard PR.
  - `auto_submit_pr: false` skips the PR API call; branch is pushed; outcome carries the branch URL AND command.
  - The chatops notification differs as documented.

## 7. Help-verb output AND chatops emoji updates

- [ ] 7.1 Update the help-verb's output to include `sync-upstream` as a fork-workflow verb with its one-line description.
- [ ] 7.2 No new per-audit emoji needed — `sync-upstream` produces inline thread replies, not audit-style notifications.

## 8. Docs

- [ ] 8.1 `docs/CHATOPS.md`: add `### sync-upstream` under operator-driven verbs, describing the rebase behavior, conflict handling, AND the no-push guarantee.
- [ ] 8.2 `docs/OPERATIONS.md`: add an "OSS contribution workflow" section describing the recommended setup:
  - Fork the upstream project on GitHub.
  - Clone the fork as the autocoder workspace.
  - Set `upstream` config block pointing at the upstream repo.
  - Set `auto_submit_pr: false`.
  - Configure `spec_storage.path` pointing at a sibling specs repo.
  - Recommended snippet for `executor.implementer.prompt_path` emphasizing minimal-diff + follow-conventions style (with sample text the operator can adapt).
  - The typical loop: scout → spec-it → review → merge fork PR → manually `gh pr create` to upstream.
- [ ] 8.3 `docs/CONFIG.md`: document each new field with default, validation rules, AND a cross-link to the OPERATIONS.md OSS-workflow section.
- [ ] 8.4 `config.example.yaml`: include all three blocks commented out, with each field's default in a comment.

## 9. Spec deltas

- [ ] 9.1 `openspec/changes/a26-oss-fork-support/specs/chatops-manager/spec.md` ADDs the sync-upstream verb requirement.
- [ ] 9.2 `openspec/changes/a26-oss-fork-support/specs/orchestrator-cli/spec.md` ADDs spec_storage, upstream, auto_submit_pr, AND sync-upstream-handler requirements.
- [ ] 9.3 `openspec/changes/a26-oss-fork-support/specs/git-workflow-manager/spec.md` MODIFIES `Monolithic PR at end of pass` (preserving all 5 canonical scenarios + adding 2 new scenarios for the auto_submit_pr: false branch).
- [ ] 9.4 `openspec/changes/a26-oss-fork-support/specs/project-documentation/spec.md` ADDs the docs requirement.

## 10. Verification

- [ ] 10.1 `cargo test` passes.
- [ ] 10.2 `openspec validate a26-oss-fork-support --strict` passes.
- [ ] 10.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
- [ ] 10.4 Manual verification on an actual OSS fork:
  - Fork a small public repo; clone locally; configure autocoder with the OSS-workflow knobs.
  - Run `@<bot> scout <fork>` AND pick an item via `spec-it`.
  - Verify the resulting branch is pushed AND the PR is NOT auto-opened.
  - Verify the suggested `gh pr create` command in the thread reply works when run manually.
  - Run `@<bot> sync-upstream <fork>` AND verify a clean rebase.
