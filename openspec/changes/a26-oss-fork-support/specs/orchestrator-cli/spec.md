## ADDED Requirements

### Requirement: `spec_storage.path` config redirects spec reads AND writes to an external git working tree
The per-repo config schema SHALL accept an optional `spec_storage` block with one required field, `path: String` (workspace-relative OR absolute). When set, autocoder SHALL treat `<spec_storage.path>/openspec/` as the canonical-spec source AND as the destination for spec-change writes, INSTEAD OF `<workspace>/openspec/`.

Config-load SHALL fail-fast if any of the following holds when `spec_storage` is set:

- The resolved path does NOT exist OR is not a directory.
- The directory at the path is not a git working tree (verified via `git -C <path> rev-parse --is-inside-work-tree` returning a non-zero exit OR a value other than `true`).
- The subdirectory `<path>/openspec/` does NOT exist.

Every path-resolution call site that previously composed `<workspace>/openspec/...` SHALL go through a `SpecRoot` resolver that returns the correct root for the current config. When `spec_storage` is unset, the resolver returns `<workspace>/openspec/` (existing behavior preserved).

Spec-change commits (brownfield draft, scout spec-it, `openspec archive`) SHALL be made in the spec_storage repo's working tree when `spec_storage` is set; the spec_storage repo's remote AND base branch determine the push target AND PR base. Code-change commits continue to live in the code workspace repo.

#### Scenario: Default — no spec_storage configured
- **WHEN** a per-repo config omits the `spec_storage` block
- **THEN** the `SpecRoot` resolver returns `<workspace>/openspec/` for all spec-path queries
- **AND** spec-change commits target the code workspace repo's working tree (existing behavior unchanged)

#### Scenario: spec_storage configured — reads redirect
- **WHEN** a per-repo config sets `spec_storage.path: "../my-specs"` AND that directory is a valid git working tree containing `openspec/`
- **THEN** the implementer prompt's canonical-spec reads load from `../my-specs/openspec/`
- **AND** the audit framework discovers spec files via `../my-specs/openspec/specs/<cap>/spec.md`
- **AND** `openspec validate` invocations are run with `cwd: ../my-specs`

#### Scenario: spec_storage configured — writes redirect
- **WHEN** a per-repo config sets `spec_storage.path: "/abs/path/to/specs"` AND a brownfield iteration completes successfully
- **THEN** the change-directory `openspec/changes/brownfield-<cap>/` is created inside `/abs/path/to/specs/`
- **AND** the commit is made in the spec_storage repo's working tree
- **AND** the code workspace's `openspec/` directory is NOT modified

#### Scenario: spec_storage path is not a git working tree
- **WHEN** config-load encounters `spec_storage.path: "/tmp/not-a-repo"` AND that directory exists but is not a git working tree
- **THEN** config-load fails with `spec_storage.path: /tmp/not-a-repo is not a git working tree (git -C ... rev-parse --is-inside-work-tree failed)`
- **AND** the daemon exits non-zero before any polling task is spawned

#### Scenario: spec_storage path lacks openspec subdirectory
- **WHEN** config-load encounters `spec_storage.path: "../some-other-repo"` AND that path is a git working tree but contains no `openspec/` subdirectory
- **THEN** config-load fails naming the missing `openspec/` subdirectory
- **AND** the daemon exits non-zero

### Requirement: `upstream` config block declares a fetch-only remote AND opportunistic fetch on iteration start
The per-repo config schema SHALL accept an optional `upstream` block with fields:

- `remote: String` (default `"upstream"`) — the git remote name to use.
- `branch: String` (default `"main"`) — the upstream's primary branch.
- `url: String` (required when block is present) — the upstream repo's git URL (SSH OR HTTPS).

When `upstream` is configured, the polling iteration's startup sequence SHALL, AFTER the existing `git fetch origin` step:

1. Ensure the workspace has a remote named `<upstream.remote>` pointing at `<upstream.url>`. If absent, add it via `git remote add`. If present with a different URL, correct it via `git remote set-url`.
2. Run `git fetch <upstream.remote>` with a 30-second timeout.
3. On success: continue with the iteration.
4. On failure (timeout, network, auth): log a WARN naming the failure AND continue with the iteration. The fetch is best-effort.

The opportunistic fetch SHALL NOT trigger any rebase OR merge — it only updates remote-tracking branches so the workspace has fresh upstream state when the operator runs `sync-upstream`.

#### Scenario: Upstream unconfigured — no fetch
- **WHEN** a per-repo config omits the `upstream` block
- **THEN** the iteration's startup sequence runs only the existing `git fetch origin`
- **AND** no `upstream` remote is added OR fetched

#### Scenario: Upstream configured, remote missing — added on iteration start
- **WHEN** the per-repo config sets `upstream: { remote: "upstream", branch: "main", url: "https://github.com/foo/bar.git" }` AND the workspace has no remote named `upstream`
- **THEN** the polling iteration adds the remote via `git remote add upstream https://github.com/foo/bar.git`
- **AND** the subsequent `git fetch upstream` runs

#### Scenario: Upstream configured, remote URL drifted — corrected on iteration start
- **WHEN** the workspace's `upstream` remote points at a URL different from `upstream.url` (e.g., the config was updated)
- **THEN** the polling iteration corrects the remote via `git remote set-url upstream <upstream.url>`
- **AND** the subsequent `git fetch upstream` runs

#### Scenario: Upstream fetch failure does not block
- **WHEN** the `git fetch upstream` call returns non-zero (network, auth, timeout)
- **THEN** a WARN is logged naming the failure AND the remote URL
- **AND** the iteration proceeds to its normal change-processing pass

### Requirement: `sync-upstream` polling-iteration handler rebases the base branch onto upstream/<branch>
The polling iteration SHALL handle `SyncUpstreamAction` requests via a dedicated handler. The handler SHALL:

1. Verify `upstream` is configured for the repo. If not, post a thread reply `✗ sync-upstream: no upstream configured for this repo. Set the upstream block in config.yaml.` AND return without acquiring the busy marker.
2. Respect the per-repo busy-marker rule: if another iteration is currently working on this repo, the handler SHALL queue the request OR refuse with the standard busy reply per the existing convention.
3. Acquire the workspace busy marker.
4. Run `git fetch <upstream.remote>` with a 60-second timeout. On failure, post `✗ sync-upstream: fetch failed: <reason>.` AND release the busy marker.
5. Checkout the configured base branch.
6. Run `git rebase <upstream.remote>/<upstream.branch>`.
7. **On conflict**: run `git rebase --abort` to restore the workspace; post `✗ sync-upstream: rebase conflict on <list-of-conflicting-files>. Aborted. Resolve manually in the workspace AND re-run, OR merge manually.`; release the busy marker.
8. **On success**: post `✓ sync-upstream: pulled <N> commit(s) from <upstream.remote>/<upstream.branch>. Base branch is <M> commit(s) ahead of upstream.` where `<N>` is the rebase's incorporated-commit count AND `<M>` is `git rev-list --count <upstream.remote>/<upstream.branch>..HEAD`; release the busy marker.

The handler SHALL NOT push the rebased base branch — the operator decides when to push to their fork. The `auto_submit_pr` field is unrelated; sync-upstream does not produce PRs.

#### Scenario: No upstream configured
- **WHEN** the handler processes a `SyncUpstreamAction` for a repo whose config has no `upstream` block
- **THEN** the handler posts `✗ sync-upstream: no upstream configured for this repo. Set the upstream block in config.yaml.`
- **AND** no busy marker is acquired
- **AND** no git operations are run

#### Scenario: Happy-path rebase
- **WHEN** the handler runs for a configured repo AND `git fetch upstream` succeeds AND the subsequent rebase incorporates 7 commits cleanly AND the result is 0 commits ahead of upstream
- **THEN** the handler posts `✓ sync-upstream: pulled 7 commit(s) from upstream/main. Base branch is 0 commit(s) ahead of upstream.`
- **AND** the busy marker is released

#### Scenario: Rebase conflict aborts AND surfaces files
- **WHEN** the rebase encounters merge conflicts in `src/lib.rs` AND `tests/integration.rs`
- **THEN** the handler runs `git rebase --abort` so the workspace returns to its pre-rebase HEAD
- **AND** the handler posts `✗ sync-upstream: rebase conflict on src/lib.rs, tests/integration.rs. Aborted. Resolve manually in the workspace AND re-run, OR merge manually.`
- **AND** the busy marker is released

#### Scenario: No push by the handler
- **WHEN** the handler completes a happy-path rebase
- **THEN** the rebased base branch is NOT pushed to any remote
- **AND** the operator is responsible for pushing to the fork's remote when ready

### Requirement: `auto_submit_pr` config field gates PR creation per repo
The per-repo config schema SHALL accept an optional `auto_submit_pr: bool` field (default `true`). The git workflow manager SHALL honor this field at the end-of-iteration PR-creation step:

- `true` (default): existing behavior unchanged — push the agent branch AND open a PR per the canonical "Monolithic PR at end of pass" requirement.
- `false`: push the agent branch per the existing rules (direct-push OR fork-PR mode) BUT skip the PR-creation API call entirely. Return a `BranchPushedNoPr { branch_url, suggested_pr_command }` outcome where `suggested_pr_command` is `gh pr create --base <upstream.branch | base-branch> --head <agent-branch>`. If `upstream` is configured, the suggested base is `upstream.branch`; otherwise it is the workspace's configured base branch.

The polling iteration's chatops notification step SHALL post:

- On `PullRequestOpened`: the existing `✅ PR opened: <url>` thread reply.
- On `BranchPushedNoPr`: `📦 Branch pushed: <branch-url>\nRun: <suggested-pr-command>`.

`auto_submit_pr` applies UNIFORMLY to both code-workspace PR creation AND spec_storage PR creation (when `spec_storage` is also configured). Operators wanting different behavior for the two cases SHALL split the workspace into separate per-repo configurations.

#### Scenario: Default — auto_submit_pr true
- **WHEN** a per-repo config omits the `auto_submit_pr` field
- **THEN** the value resolves to `true`
- **AND** end-of-iteration behavior matches the existing "Monolithic PR at end of pass" requirement

#### Scenario: Explicit auto_submit_pr false
- **WHEN** a per-repo config sets `auto_submit_pr: false` AND an iteration produces a commit
- **THEN** the agent branch is pushed per the existing push rules
- **AND** no GitHub PR-creation API call is made
- **AND** the iteration's chatops thread reply contains `📦 Branch pushed: <branch-url>` followed by the templated `gh pr create` command

#### Scenario: Suggested gh-pr-create base comes from upstream config
- **WHEN** `auto_submit_pr: false` AND `upstream.branch: "main"` are configured
- **THEN** the suggested command is `gh pr create --base main --head <agent-branch>`

#### Scenario: Suggested gh-pr-create base falls back to base branch
- **WHEN** `auto_submit_pr: false` AND no `upstream` block is configured
- **THEN** the suggested command uses the workspace's configured base branch as `--base`
