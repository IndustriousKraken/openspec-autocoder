## ADDED Requirements

### Requirement: Dirty workspace auto-recovers at startup
autocoder SHALL attempt automatic recovery before falling back to the existing "skip for the process lifetime" behavior when a repository's workspace is dirty at startup (non-empty `git status --porcelain` output). Recovery consists of `git checkout <base_branch>`, `git reset --hard origin/<base_branch>`, and `git clean -fd`. After recovery, autocoder SHALL re-run the dirty check; if clean, the repository proceeds to normal polling.

#### Scenario: Workspace dirty due to prior failed iteration
- **WHEN** a repository's workspace has uncommitted changes at
  startup (residue from a previous executor run that crashed or
  was killed mid-iteration)
- **THEN** autocoder logs a `warn`-level line naming the dirty
  entry count and indicating recovery is being attempted
- **AND** autocoder runs `git checkout <base_branch>`, then
  `git reset --hard origin/<base_branch>`, then `git clean -fd`
  in the workspace
- **AND** autocoder re-runs `git status --porcelain`; if empty,
  logs `info` "workspace recovered" and the repository proceeds
  to normal polling

#### Scenario: Workspace remains dirty after recovery attempt
- **WHEN** the recovery commands all complete but a subsequent
  `git status --porcelain` is still non-empty (gitignored state,
  read-only mount, file-locking, etc.)
- **THEN** autocoder logs the existing skip-for-lifetime error
  message
- **AND** the repository is skipped for the process lifetime,
  preserving the prior conservative behavior for genuinely
  unrecoverable cases

#### Scenario: Workspace already clean
- **WHEN** the initial `git status --porcelain` is empty
- **THEN** no recovery commands are executed
- **AND** the repository proceeds to normal polling, identical
  to prior behavior

### Requirement: Reject archive-only iterations as Failed
autocoder SHALL treat an iteration as Failed (not Completed), revert the staged moves via `git reset --hard`, and leave the change pending for retry when the executor returns Completed AND the resulting working-tree changes consist *only* of file moves whose destination paths start with `openspec/changes/archive/`. The detection is structural — pattern-matching on rename destinations — and does not depend on which command produced the moves.

#### Scenario: Agent archives the change instead of implementing it
- **WHEN** the executor returns `Completed` for a change AND
  `git status --porcelain` reports a non-empty result AND every
  reported entry is a rename (status code `R`) whose target path
  begins with `openspec/changes/archive/`
- **THEN** autocoder reverts the working tree via
  `git reset --hard HEAD` to discard the staged moves
- **AND** autocoder treats the outcome as
  `Failed { reason: "agent appears to have archived without implementing the change" }`
- **AND** autocoder logs a `warn`-level line naming the change
- **AND** the change's `.in-progress` lock is removed via the
  existing Failed-handling code path so the next iteration
  retries

#### Scenario: Legitimate implementation that also moves an archive file
- **WHEN** the executor returns `Completed` AND the working tree
  contains at least one change that is NOT a rename into
  `openspec/changes/archive/` (e.g. modified `src/foo.rs`, added
  `tests/bar.rs`)
- **THEN** autocoder treats the outcome as Completed as before
- **AND** the commit + push + PR steps proceed normally
- **AND** archive-rename entries, if any, are included in the
  commit unchanged

#### Scenario: Workspace is clean (no changes at all)
- **WHEN** the executor returns `Completed` AND `git status
  --porcelain` is empty
- **THEN** autocoder uses the existing "completed but no diff"
  handling (logs a warning, archives the change without an empty
  commit)
- **AND** the lazy-archive detection does NOT fire (no staged
  moves to revert)
