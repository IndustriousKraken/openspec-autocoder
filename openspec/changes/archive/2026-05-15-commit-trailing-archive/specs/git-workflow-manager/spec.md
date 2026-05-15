## MODIFIED Requirements

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
