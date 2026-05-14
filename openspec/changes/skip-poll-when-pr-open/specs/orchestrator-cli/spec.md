## ADDED Requirements

### Requirement: Skip iteration when an open PR exists for the agent branch
autocoder SHALL query GitHub for open PRs whose `head` matches the configured agent branch before running the executor on any pending changes. When such a PR exists, the iteration SHALL be skipped entirely — no executor invocation, no `recreate_branch` (which would obliterate the open PR's branch on the next force-push), no commit work. The skip persists across iterations until the open PR is closed or merged. This prevents redundant Claude executions, PR-diff thrashing under reviewers, and the 422 "PR already exists" loop that would otherwise occur every polling pass after a PR is opened but not resolved.

#### Scenario: An open PR exists for the agent branch
- **WHEN** the daemon completes workspace init and `pull --ff-only`
  succeeds AND a `GET /repos/{owner}/{repo}/pulls?state=open&head=<head>&base=<base>` query returns one or more PRs
- **THEN** the daemon logs an INFO line naming each PR number and
  the URL, and returns from the iteration without invoking
  `recreate_branch`, `walk_queue`, or any executor
- **AND** the polling task continues with its normal sleep + next-iteration cycle

#### Scenario: No open PR exists for the agent branch
- **WHEN** the GitHub query returns an empty list
- **THEN** the daemon proceeds with `recreate_branch` and the
  normal iteration as before

#### Scenario: GitHub query fails
- **WHEN** the `pulls` query errors at the transport layer or
  returns a non-2xx status
- **THEN** the daemon logs a WARN naming the failure (status code
  and/or error text) and proceeds with the iteration as if no PR
  existed
- **AND** the iteration is NOT blocked by a transient GitHub
  failure (the check is best-effort — false negatives just degrade
  to the prior pre-check behavior)

#### Scenario: Fork-PR mode head qualifier
- **WHEN** `github.fork_owner` is set
- **THEN** the `head` query parameter is
  `<fork_owner>:<agent_branch>` so GitHub disambiguates correctly
  against the upstream repo's PR list

#### Scenario: Direct mode head qualifier
- **WHEN** `github.fork_owner` is unset
- **THEN** the `head` query parameter is
  `<repo_owner>:<agent_branch>` where `<repo_owner>` is parsed
  from `repo.url`
