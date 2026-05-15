# git-workflow-manager Specification

## Purpose
TBD - created by archiving change orchestrator-architecture. Update Purpose after archive.
## Requirements
### Requirement: Per-pass agent branch
The git workflow manager SHALL ensure each polling pass starts from a
clean branch off the configured base branch, recreating the agent
branch each pass. The branch source remains `origin/<base_branch>` in
both direct-push and fork-PR modes — the fork's view of the base
branch is never consulted.

#### Scenario: Branch initialization at start of pass
- **WHEN** a polling pass begins for a repository AND the queue
  contains at least one ready change
- **THEN** the manager runs, in order: `git fetch origin`,
  `git checkout <base_branch>`,
  `git pull --ff-only origin <base_branch>`,
  `git checkout -B <agent_branch>`
- **AND** the resulting `HEAD` of `<agent_branch>` is verifiable as
  identical to the post-pull `HEAD` of `<base_branch>` (`git rev-parse
  <agent_branch>` equals `git rev-parse <base_branch>`)
- **AND** prior local content on `<agent_branch>` is overwritten
  without warning — this is by design
- **AND** in fork-PR mode, the `fork` remote is NEVER consulted
  during branch initialization (it is push-only)

#### Scenario: Pull conflict on base branch
- **WHEN** `git pull --ff-only origin <base_branch>` exits non-zero
  (non-fast-forward, network error, etc.)
- **THEN** the manager aborts the polling pass for this repository
- **AND** the workspace is left in its pre-pull state (no agent
  branch is created or modified for this pass)
- **AND** the captured stderr from the failing git command is logged
  verbatim

### Requirement: Serial commit per change
The git workflow manager SHALL produce one commit per successfully implemented change, on the agent branch, in queue order. A change is "successfully implemented" only when the executor returns `Completed` AND `git status --porcelain` returns a non-empty result. If the workspace is clean after a `Completed` outcome, the manager SHALL NOT commit or archive the change; the iteration SHALL be marked Failed and the change SHALL remain pending for retry. The single commit per change SHALL include both the executor's working-tree modifications AND the archive move of `openspec/changes/<change>/` to `openspec/changes/archive/<YYYY-MM-DD>-<change>/`, so after the commit the working tree is clean and the change's archive move is fully captured in git history.

#### Scenario: Committing a change with modifications
- **WHEN** the executor returns `Completed` for `<change>` AND `git status --porcelain` returns a non-empty result inside the workspace
- **THEN** the manager builds `<change>: <summary>` (where `<summary>` is the first non-empty line of the `## Why` section of the change's `proposal.md`, truncated so the total subject is ≤ 72 characters)
- **AND** the manager moves `openspec/changes/<change>/` to `openspec/changes/archive/<YYYY-MM-DD>-<change>/` before staging
- **AND** the manager runs `git add -A` followed by `git commit -m "<subject>"`
- **AND** the resulting commit contains both the executor's modifications AND the archive rename
- **AND** `git status --porcelain` returns empty immediately after the commit

#### Scenario: Executor reported Completed but produced no diff
- **WHEN** the executor returns `Completed` for `<change>` AND `git status --porcelain` returns empty
- **THEN** the manager logs a warning naming `<change>` ("agent reported Completed without modifying the workspace; marking Failed")
- **AND** the manager does NOT create an empty commit
- **AND** the manager does NOT archive the change
- **AND** the iteration outcome is reported as Failed so the queue engine unlocks `<change>` and the next polling pass retries it

#### Scenario: Working tree clean after every archived change
- **WHEN** the manager has successfully committed any change in the
  current pass
- **THEN** `git status --porcelain` immediately after the commit
  returns empty
- **AND** this invariant holds for every archived change in the pass,
  including the last one, so no archive rename is ever left dangling
  in the working tree

### Requirement: Monolithic PR at end of pass
The git workflow manager SHALL push the agent branch and create a
single Pull Request via the GitHub REST API at the end of each polling
iteration that produced at least one commit. The push target and PR
`head` format depend on whether fork-PR mode is active:

- **Direct-push mode (`github.fork_owner` unset):** push to `origin`;
  PR `head` is the agent branch name alone.
- **Fork-PR mode (`github.fork_owner` set):** push to `fork`; PR
  `head` is `<fork-owner>:<agent-branch>` (cross-repo PR syntax).

In both modes the PR is posted to the upstream repository's `/pulls`
endpoint. **When the code-reviewer is enabled, the PR body SHALL
include the reviewer's report under a `## Code Review` heading, and a
`Block` verdict SHALL cause the PR to be created as a draft (with a
`do-not-merge` label fallback if the host rejects drafts).**

#### Scenario: Opening a PR in direct-push mode
- **WHEN** an iteration completes AND the agent branch contains at
  least one commit ahead of base AND `github.fork_owner` is unset
- **THEN** the manager pushes with
  `git push --force-with-lease origin <agent-branch>`
- **AND** POSTs to
  `https://api.github.com/repos/<upstream-owner>/<upstream-repo>/pulls`
  with body containing `"head": "<agent-branch>"` and
  `"base": "<base-branch>"`

#### Scenario: Opening a PR in fork-PR mode
- **WHEN** an iteration completes AND the agent branch contains at
  least one commit ahead of base AND `github.fork_owner` is set to
  `<fork-owner>`
- **THEN** the manager pushes with
  `git push --force-with-lease fork <agent-branch>`
- **AND** POSTs to
  `https://api.github.com/repos/<upstream-owner>/<upstream-repo>/pulls`
  with body containing `"head": "<fork-owner>:<agent-branch>"` and
  `"base": "<base-branch>"`
- **AND** the API call's authentication token is resolved from
  `github.owner_tokens[<upstream-owner>]` (or the configured
  fallback), per the existing per-owner token routing — the upstream
  owner is the owner of the repository the PR targets, regardless
  of which account owns the fork

#### Scenario: Opening a PR with a passing review
- **WHEN** an iteration completes AND the agent branch contains at
  least one commit ahead of base AND `reviewer.enabled` is true AND
  `code_reviewer.review` returns `Ok(ReviewReport { verdict: Pass, .. })`
- **THEN** the manager pushes (to `origin` or `fork` per the mode)
  with `--force-with-lease` and POSTs to the GitHub PR API with
  `draft: false` and a body whose final section is `## Code Review`
  followed by the reviewer's `markdown`
- **AND** the `head` parameter formatting follows the mode rules
  above

#### Scenario: Opening a PR with a Block verdict
- **WHEN** an iteration completes AND the reviewer returns
  `Ok(ReviewReport { verdict: Block, .. })`
- **THEN** the manager pushes the agent branch (to `origin` or
  `fork` per the mode) and POSTs to the GitHub PR API with
  `draft: true`
- **AND** the PR body's final section is `## Code Review` followed
  by the reviewer's `markdown`

#### Scenario: Reviewer disabled or absent
- **WHEN** the `reviewer` config block is absent OR
  `reviewer.enabled` is false
- **THEN** the manager pushes the agent branch (to `origin` or
  `fork` per the mode) and POSTs to the GitHub PR API with
  `draft: false` and a body that does NOT include a `## Code Review`
  section
- **AND** no LLM API call is made

#### Scenario: Reviewer failure
- **WHEN** `reviewer.enabled` is true AND `code_reviewer.review`
  returns `Err(_)`
- **THEN** the manager logs `"reviewer failed: {error}"` naming the
  reason
- **AND** the manager pushes the agent branch (to `origin` or
  `fork` per the mode) and POSTs to the GitHub PR API with
  `draft: false`
- **AND** the PR body's `## Code Review` section contains only the
  line `(reviewer failed: <reason>)`

#### Scenario: Draft creation falls back to label
- **WHEN** `Block` verdict requires `draft: true` AND the GitHub
  API rejects the draft flag
- **THEN** the manager retries the PR creation request with
  `draft: false`
- **AND** on success, the manager POSTs to
  `https://api.github.com/repos/<upstream-owner>/<upstream-repo>/issues/<pr_number>/labels`
  with body `{ "labels": ["do-not-merge"] }`
- **AND** the manager logs
  `"draft unsupported; applied do-not-merge label as fallback"`
- **AND** in fork-PR mode, the label is applied to the PR on
  upstream (not the fork) — the PR lives on upstream regardless
  of mode

