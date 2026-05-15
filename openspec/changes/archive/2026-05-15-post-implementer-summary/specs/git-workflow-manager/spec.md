## ADDED Requirements

### Requirement: Implementer-summary PR comment
After a Pull Request is successfully created at the end of a polling iteration, the git workflow manager SHALL post a single follow-up issue comment to that PR containing the implementer agent's captured stdout for each change that shipped in the pass. The comment is best-effort: any failure to post is logged and ignored, and SHALL NOT roll back or affect the PR's existence. The comment exists to surface the agent's own narrative (modules touched, test counts, deviations from the spec it had to make, meta-observations) directly on the PR page so reviewers can read it without inspecting server-local log files.

#### Scenario: Comment posted after successful PR creation
- **WHEN** `open_pull_request` returns `Ok` with a PR number
- **THEN** the manager reads the per-change run-log file
  `<system-temp>/autocoder/logs/<workspace-basename>/<change>.log`
  for each archived change in the pass
- **AND** extracts the `=== STDOUT (n bytes) ===` block (only
  stdout; stderr is operator-facing log noise and is excluded)
- **AND** assembles a markdown comment with the structure:
  - heading `## Agent implementation notes`
  - one `### <change-name>` subsection per change, each carrying
    the extracted stdout
- **AND** POSTs the comment via
  `POST /repos/<upstream-owner>/<upstream-repo>/issues/<pr-number>/comments`
  using the same upstream-owner-routed token as the PR creation
- **AND** logs an INFO line on success with the PR number and
  comment count

#### Scenario: Comment posting fails
- **WHEN** the issue-comment POST returns a non-2xx status or the
  request errors at the transport layer
- **THEN** the manager logs an ERROR naming the failure (status
  code or transport error)
- **AND** the iteration overall is reported as Ok — the PR was
  successfully created; the missing comment is enrichment, not
  contract

#### Scenario: A change's log file is missing or unreadable
- **WHEN** the run-log file for a change cannot be read (file
  absent, permission denied, etc.)
- **THEN** the manager logs WARN naming the change and the path
- **AND** the comment is still posted, omitting that change's
  section
- **AND** if ALL changes' logs are missing, the comment is NOT
  posted (nothing useful to say) and an INFO line records the skip

#### Scenario: Empty stdout for a change
- **WHEN** the extracted stdout for a change is empty
- **THEN** the change's section content is `_(no implementer output captured)_`

#### Scenario: Comment body exceeds GitHub size limit
- **WHEN** the assembled comment body exceeds 60,000 characters
- **THEN** the body is truncated at 60,000 characters
- **AND** the truncation point is followed by `\n\n_[implementer summary truncated to fit GitHub comment limit; full output at /tmp/autocoder/logs/<workspace-basename>/<change>.log]_`
- **AND** the truncated body is posted as a single comment (NOT
  split across multiple comments)

## MODIFIED Requirements

### Requirement: Monolithic PR at end of pass
The git workflow manager SHALL push the agent branch and create a single Pull Request via the GitHub REST API at the end of each polling iteration that produced at least one commit, AND SHALL surface the new PR's number to its caller so a follow-up implementer-summary comment can be posted. The push target and PR `head` format depend on whether fork-PR mode is active:

- **Direct-push mode (`github.fork_owner` unset):** push to `origin`;
  PR `head` is the agent branch name alone.
- **Fork-PR mode (`github.fork_owner` set):** push to `fork`; PR
  `head` is `<fork-owner>:<agent-branch>` (cross-repo PR syntax).

In both modes the PR is posted to the upstream repository's `/pulls` endpoint. **When the code-reviewer is enabled, the PR body SHALL include the reviewer's report under a `## Code Review` heading, and a `Block` verdict SHALL cause the PR to be created as a draft (with a `do-not-merge` label fallback if the host rejects drafts).** **`github::create_pull_request` SHALL return both the `html_url` AND the `number` of the created PR.**

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
