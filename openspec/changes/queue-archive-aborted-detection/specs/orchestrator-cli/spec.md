## ADDED Requirements

### Requirement: Shared archive-with-postcondition helper covers every in-iteration openspec archive call
Every call site that runs `openspec archive <slug> -y` from inside the daemon SHALL go through a shared `openspec_archive_with_postcondition` helper that inspects stdout for the `Aborted.` marker AND verifies the post-condition (`openspec/changes/<slug>/` is gone AND exactly one `openspec/changes/archive/*-<slug>/` directory exists) before reporting success. The helper SHALL return a structured `ArchiveFailure` value naming the specific failure mode; each caller maps that to a domain-appropriate error type whose message includes the openspec output excerpt explaining the cause.

#### Scenario: Self-heal silent-skip surfaces the openspec cause
- **WHEN** an iteration enters self-heal AND `openspec archive <slug> -y` exits 0 AND its stdout contains a line beginning with `Aborted.`
- **THEN** `queue::archive` returns `Err` whose message contains `aborted by openspec:` and the preceding diagnostic line from openspec's stdout
- **AND** the self-heal call site's failure_reason is `self-heal archive failed: openspec archive `<slug>` aborted by openspec: <reason>; full output: <excerpt>`
- **AND** the change is NOT marked archived
- **AND** git commit is NOT attempted (the failure short-circuits before staging or commit)

#### Scenario: Rebuild path uses the same helper
- **WHEN** the rebuild loop processes any archived change and invokes the archive helper
- **THEN** the helper's `Err(AbortedMarker { .. })` triggers the existing rebuild rollback contract from `sync-specs-rebuild-atomicity` AND the existing failure-reason format from `sync-specs-detect-aborted-output`
- **AND** the rebuild behaviour is observationally identical to the pre-consolidation behaviour

#### Scenario: Active-path-still-present detection without marker
- **WHEN** `openspec archive <slug> -y` exits 0 AND stdout does NOT contain the `Aborted.` marker AND `openspec/changes/<slug>/` still exists
- **THEN** the helper returns `Err(ArchiveFailure::ActivePathStillPresent { path, full_output })`
- **AND** the caller's failure message reads `openspec archive `<slug>` reported success but the change directory at <path> still exists`

#### Scenario: Data-loss-shaped detection
- **WHEN** `openspec archive <slug> -y` exits 0 AND stdout has no marker AND `openspec/changes/<slug>/` is gone AND no `openspec/changes/archive/*-<slug>/` matches
- **THEN** the helper returns `Err(ArchiveFailure::NoArchiveEntryFound { full_output })`
- **AND** the caller's failure message names the data-loss condition explicitly

### Requirement: `run_git` surfaces stdout when stderr is empty or as supplementary context
The `run_git` helper SHALL include the failed command's stdout in the returned error message when stderr is empty, AND SHALL include both streams labelled `stderr:` / `stdout:` when both are non-empty. When both streams are empty (rare; failures with no diagnostic output), the error SHALL name the exit code in parentheses so the operator at least knows the command failed without producing output.

#### Scenario: `git commit` "nothing to commit" surfaces meaningfully
- **WHEN** `run_git` runs `git commit -m <subject>` against a workspace where `git status --porcelain` is empty, AND git exits non-zero with stdout `nothing to commit, working tree clean` and empty stderr
- **THEN** the returned `Err` contains the text `nothing to commit, working tree clean`
- **AND** the error message format is `git commit failed: nothing to commit, working tree clean`
- **AND** the error message does NOT end in a bare colon-space

#### Scenario: Both streams populated
- **WHEN** `run_git` runs a command that fails with non-empty stderr AND non-empty stdout
- **THEN** the returned `Err` contains both excerpts prefixed `stderr:` and `stdout:`

#### Scenario: Neither stream populated
- **WHEN** `run_git` runs a command that fails with both streams empty
- **THEN** the returned `Err` contains a parenthetical naming the exit code (e.g. `git commit failed: (no output; exit Some(1))`)
- **AND** the error does NOT end in a bare colon-space
