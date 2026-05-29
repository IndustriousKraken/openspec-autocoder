## ADDED Requirements

### Requirement: Polling iteration classifies outcome as spec-only, code-only, OR dual-tree before commit + push + PR

The polling iteration's commit + push + PR step SHALL begin with a working-tree-status classification:

1. Run `git -C <code_workspace> status --porcelain` AND check for non-empty output.
2. When `spec_storage` is configured for the repo, run `git -C <spec_storage.path> status --porcelain` AND check for non-empty output.
3. Classify the iteration's outcome as:
   - **Code-only**: code workspace dirty AND spec_storage clean (OR not configured).
   - **Spec-only**: code workspace clean AND spec_storage dirty.
   - **Dual-tree**: both dirty.
   - **Clean**: both clean. (No commit + push + PR happens; the iteration's outcome was Completed with no diff, handled by the existing "exit-0 without modifying workspace" path.)

The classification determines which working trees are committed AND pushed AND which PRs are opened.

#### Scenario: Spec-only iteration commits to spec_storage tree only
- **WHEN** a polling iteration completes AND the code workspace is clean AND the spec_storage working tree has new files (e.g. brownfield draft, scout spec-it write, OR `openspec archive` rename)
- **THEN** the iteration's commit step runs `git -C <spec_storage.path> commit ...`
- **AND** the code workspace's working tree is NOT committed
- **AND** the push step targets the spec_storage repo's remote (per the resolution requirement below)

#### Scenario: Code-only iteration commits to code workspace tree only
- **WHEN** a polling iteration completes AND the code workspace is dirty AND the spec_storage working tree is clean (OR not configured)
- **THEN** the iteration's commit step runs `git -C <code_workspace> commit ...` (existing canonical behavior)
- **AND** the spec_storage repo is NOT committed (when configured AND clean)

#### Scenario: Dual-tree iteration produces TWO PRs
- **WHEN** a polling iteration completes AND BOTH the code workspace AND spec_storage working tree are dirty (the iteration drafted spec changes AND modified code-workspace fixtures)
- **THEN** the commit + push + PR step runs against BOTH working trees independently
- **AND** TWO PRs are opened (one per repo) with their respective title shapes (per the title-prefix requirement below)
- **AND** chatops notifications fire for each PR independently

### Requirement: Spec-storage push remote AND base branch resolution rules

When the polling iteration's classification is spec-only OR dual-tree AND `spec_storage` is configured, the push remote AND PR base branch SHALL be resolved per the following rules:

- **Push remote**: `spec_storage.push_remote` (new optional field; default `None`). When `None`, the runtime uses `"origin"`. The resolved value MUST exist in `git -C <spec_storage.path> remote` output; config-load SHALL fail-fast if the field is set to a non-existent remote name.
- **Base branch**: `spec_storage.base_branch` (new optional field; default `None`). When `None`, the runtime queries `git -C <spec_storage.path> symbolic-ref refs/remotes/<push_remote>/HEAD` AND parses the branch name (e.g. `refs/remotes/origin/main` → `main`). When the symbolic-ref query fails, fall back to `"main"`.
- **Spec-repo owner/name**: parsed from `git -C <spec_storage.path> remote get-url <push_remote>`. SSH (`git@github.com:owner/name.git`) AND HTTPS (`https://github.com/owner/name.git`) URL forms SHALL both be parsed. On parse failure, the iteration SHALL log WARN AND fall back to the code workspace's owner/name (degrades to opening the PR against the wrong repo; clearly visible to the operator).

The resolution SHALL happen once per polling iteration AND the resolved values SHALL be threaded through the commit + push + PR steps explicitly (no re-resolution mid-step).

#### Scenario: Default resolution uses `origin` AND remote-tracked HEAD
- **WHEN** a spec-only iteration runs AND `spec_storage.push_remote` AND `spec_storage.base_branch` are both unset
- **THEN** the resolved push remote is `"origin"`
- **AND** the resolved base branch is the branch name parsed from `git -C <spec_storage.path> symbolic-ref refs/remotes/origin/HEAD`

#### Scenario: Operator overrides take precedence
- **WHEN** `spec_storage.push_remote: "upstream-fork"` AND `spec_storage.base_branch: "develop"` are set
- **THEN** the resolved push remote is `"upstream-fork"`
- **AND** the resolved base branch is `"develop"`
- **AND** the iteration's `git push` targets `upstream-fork` AND the PR's `--base` is `develop`

#### Scenario: Push-remote validation at config-load
- **WHEN** config-load encounters `spec_storage.push_remote: "nonexistent-remote"` AND running `git -C <spec_storage.path> remote` returns a set that does NOT include `nonexistent-remote`
- **THEN** config-load fails with a message naming the missing remote AND the available remotes
- **AND** the daemon exits non-zero before any polling task is spawned

#### Scenario: Symbolic-ref query failure falls back to `main`
- **WHEN** `spec_storage.base_branch` is unset AND `git -C <spec_storage.path> symbolic-ref refs/remotes/origin/HEAD` returns non-zero (e.g. the remote has no default branch set)
- **THEN** the iteration logs WARN naming the failure
- **AND** the resolved base branch is `"main"` (the documented fallback)

### Requirement: Spec-only AND dual-tree's spec PR title is prefixed `[specs] `

PRs whose entire diff lives under `openspec/` SHALL have their titles prefixed with `[specs] `. This applies to:

- Spec-only iterations' PRs.
- The spec-storage PR half of dual-tree iterations.

Code-only iterations' PRs AND the code PR half of dual-tree iterations SHALL remain unprefixed (existing format preserved).

The prefix is operator-visible AND lets operators sort PR lists by title to find spec-only PRs quickly. It does NOT affect any automated processing — the revisions dispatcher, the reviewer, AND the chatops notifications all key on PR number, not title.

#### Scenario: Spec-only PR title carries the prefix
- **WHEN** a spec-only iteration produces a PR for a brownfield draft change `a36-brownfield-foo` (+ 0 more)
- **THEN** the PR title is `[specs] a36-brownfield-foo`

#### Scenario: Code-only PR title is unprefixed
- **WHEN** a code-only iteration produces a PR for change `a40-fix-bar` (+ 0 more)
- **THEN** the PR title is `a40-fix-bar` (no `[specs] ` prefix)

#### Scenario: Dual-tree produces one prefixed AND one unprefixed PR
- **WHEN** a dual-tree iteration produces two PRs for change `a42-mixed-baz`
- **THEN** the spec PR title is `[specs] a42-mixed-baz`
- **AND** the code PR title is `a42-mixed-baz`
