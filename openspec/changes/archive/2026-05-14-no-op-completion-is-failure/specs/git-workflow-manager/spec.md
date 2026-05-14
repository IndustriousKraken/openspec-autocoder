## MODIFIED Requirements

### Requirement: Serial commit per change
The git workflow manager SHALL produce one commit per successfully implemented change, on the agent branch, in queue order. A change is "successfully implemented" only when the executor returns `Completed` AND `git status --porcelain` returns a non-empty result. If the workspace is clean after a `Completed` outcome, the manager SHALL NOT commit or archive the change; the iteration SHALL be marked Failed and the change SHALL remain pending for retry.

#### Scenario: Committing a change with modifications
- **WHEN** the executor returns `Completed` for `<change>` AND `git status --porcelain` returns a non-empty result inside the workspace
- **THEN** the manager runs `git add -A` followed by `git commit -m "<change>: <summary>"`, where `<summary>` is the first non-empty line of the `## Why` section of `<change>/proposal.md`, truncated to 72 characters total subject length
- **AND** the resulting commit is verifiable as a new commit on `<agent_branch>` whose tree differs from its parent (`git diff-tree --no-commit-id --name-only HEAD` returns a non-empty list)

#### Scenario: Executor reported Completed but produced no diff
- **WHEN** the executor returns `Completed` for `<change>` AND `git status --porcelain` returns empty
- **THEN** the manager logs a warning naming `<change>` ("agent reported Completed without modifying the workspace; marking Failed")
- **AND** the manager does NOT create an empty commit
- **AND** the manager does NOT archive the change
- **AND** the iteration outcome is reported as Failed so the queue engine unlocks `<change>` and the next polling pass retries it
