## MODIFIED Requirements

### Requirement: Monolithic PR at end of pass
The git workflow manager SHALL push the agent branch at the end of each polling iteration that produced at least one commit, AND, when `auto_submit_pr` is `true` (the default), SHALL create a single Pull Request via the GitHub REST API AND surface the new PR's number to its caller so a follow-up implementer-summary comment can be posted. When `auto_submit_pr` is `false`, the manager SHALL push the branch but SKIP the PR-creation API call, instead returning a `BranchPushedNoPr { branch_url, suggested_pr_command }` outcome to the caller. The push target and PR `head` format depend on whether fork-PR mode is active:

- **Direct-push mode (`github.fork_owner` unset):** push to `origin`;
  PR `head` is the agent branch name alone.
- **Fork-PR mode (`github.fork_owner` set):** push to `fork`; PR
  `head` is `<fork-owner>:<agent-branch>` (cross-repo PR syntax).

In both modes (when PR creation is not skipped) the PR is posted to the upstream repository's `/pulls` endpoint. **When the code-reviewer is enabled, the PR body SHALL include the reviewer's report under a `## Code Review` heading, and a `Block` verdict SHALL cause the PR to be created as a draft (with a `do-not-merge` label fallback if the host rejects drafts).** **`github::create_pull_request` SHALL return both the `html_url` AND the `number` of the created PR.**

When `auto_submit_pr` is `false`, the `suggested_pr_command` SHALL be templated as `gh pr create --base <base> --head <agent-branch>` where `<base>` is `upstream.branch` if the `upstream` config block is set, OR the workspace's configured base branch otherwise. The reviewer-run AND implementer-summary-capture steps still execute when `auto_submit_pr` is `false`; their outputs are surfaced via the polling iteration's chatops notification rather than via the (skipped) PR body.

#### Scenario: Opening a PR in direct-push mode
- **WHEN** an iteration completes AND the agent branch contains at
  least one commit ahead of base AND `github.fork_owner` is unset
- **THEN** the manager pushes with
  `git push --force-with-lease origin <agent-branch>`
- **AND** POSTs to
  `https://api.github.com/repos/<upstream-owner>/<upstream-repo>/pulls`
  with body containing `"head": "<agent-branch>"` and
  `"base": "<base-branch>"`
- **AND** the response's `number` field is returned to the caller
  alongside the `html_url` so the implementer-summary comment
  step can target this PR

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
- **AND** the response's `number` field is returned to the caller
  alongside the `html_url`

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
- **AND** the response's `number` is returned to the caller

#### Scenario: Opening a PR with a Block verdict
- **WHEN** an iteration completes AND the reviewer returns
  `Ok(ReviewReport { verdict: Block, .. })`
- **THEN** the manager pushes the agent branch (to `origin` or
  `fork` per the mode) and POSTs to the GitHub PR API with
  `draft: true`
- **AND** the PR body's final section is `## Code Review` followed
  by the reviewer's `markdown`
- **AND** the response's `number` is returned to the caller

#### Scenario: Reviewer disabled or absent
- **WHEN** the `reviewer` config block is absent OR
  `reviewer.enabled` is false
- **THEN** the manager pushes the agent branch (to `origin` or
  `fork` per the mode) and POSTs to the GitHub PR API with
  `draft: false` and a body that does NOT include a `## Code Review`
  section
- **AND** no LLM API call is made
- **AND** the response's `number` is returned to the caller

#### Scenario: auto_submit_pr false — push without PR creation
- **WHEN** an iteration completes AND the agent branch contains at least one commit ahead of base AND `auto_submit_pr: false` is configured for the repo
- **THEN** the manager pushes the agent branch per the existing direct-push OR fork-PR rule (same push target as it would use with `auto_submit_pr: true`)
- **AND** NO POST to the GitHub PR API is made
- **AND** the manager returns `BranchPushedNoPr { branch_url, suggested_pr_command }` to the caller
- **AND** `branch_url` is `https://github.com/<owner>/<repo>/tree/<agent-branch>` (resolved from the push target's remote URL)
- **AND** `suggested_pr_command` is `gh pr create --base <upstream.branch | base-branch> --head <agent-branch>`

#### Scenario: auto_submit_pr false — reviewer output surfaced without PR body
- **WHEN** `auto_submit_pr: false` AND `reviewer.enabled: true` AND `code_reviewer.review` returns `Ok(ReviewReport { verdict: Block, markdown: ... })`
- **THEN** the manager pushes the branch but does NOT create a PR (the canonical PR-body posting site is skipped because no PR exists)
- **AND** the reviewer's report is surfaced via the polling iteration's chatops notification (per the polling-iteration's existing notification mechanics) so the operator sees the report before manually running `gh pr create`
- **AND** the `BranchPushedNoPr` outcome carries the reviewer report alongside the branch URL so the caller can format the chatops notification accordingly
