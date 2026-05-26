## ADDED Requirements

### Requirement: Workspace init auto-recovers from partial-clone artifacts
When `workspace::ensure_initialized` detects a workspace directory that exists but contains no `.git/` subdirectory, the daemon SHALL attempt automatic recovery: verify the directory contains no operator-meaningful state, delete the partial directory, and re-attempt the clone. The recovery is a single self-healing pass — if the re-clone also fails, the iteration reports the real clone failure (not the secondary "exists but no .git" detection). A WARN log per recovery attempt names the workspace path and the action so operators see the auto-cleanup happened.

#### Scenario: Partial-clone artifact triggers auto-cleanup and re-clone
- **WHEN** the workspace directory exists AND it contains no `.git/` AND the safety check passes (no operator-meaningful state)
- **THEN** the daemon logs a WARN naming the workspace path, the repo URL, and the action ("partial clone artifact detected. Deleting and re-cloning.")
- **AND** the daemon calls `std::fs::remove_dir_all` on the workspace path
- **AND** the daemon re-attempts the normal clone path
- **AND** if the re-clone succeeds, `ensure_initialized` returns Ok
- **AND** the polling iteration's reported outcome is Completed (not Failed)

#### Scenario: Re-clone after auto-cleanup fails surfaces the real error
- **WHEN** auto-cleanup runs AND the re-clone attempt fails
- **THEN** the returned Err contains the actual clone-failure message from git (e.g. `Permission denied (publickey)` or `Could not resolve host github.com`)
- **AND** the returned Err does NOT contain the "exists but no .git" secondary detection text
- **AND** the chatops `workspace_init_failure` alert's `last_error_excerpt` field reflects the real clone failure

#### Scenario: Auto-cleanup itself fails on permissions or disk-full
- **WHEN** auto-cleanup runs AND `fs::remove_dir_all` returns an error
- **THEN** the daemon logs at ERROR naming the workspace path and the OS error
- **AND** the returned Err includes the OS error message
- **AND** the iteration reports Failed

### Requirement: Safety check protects operator-meaningful state from auto-cleanup
Before deleting a partial-clone artifact, the daemon SHALL verify the directory contains no operator-meaningful state. The check identifies tripwires that would indicate the broken state is NOT a freshly-failed clone but rather something the operator might care about preserving. If any tripwire fires, auto-cleanup is refused and a more informative error is returned.

#### Scenario: Marker file blocks auto-cleanup
- **WHEN** the partial-clone-state directory contains `openspec/changes/<slug>/.perma-stuck.json` OR `openspec/changes/<slug>/.needs-spec-revision.json` at any depth
- **THEN** the safety check returns Err naming the tripwire
- **AND** `ensure_initialized` returns Err with the existing "exists but no .git" message extended with `(partial cleanup refused: contains .perma-stuck.json or .needs-spec-revision.json marker; manual operator inspection required)`
- **AND** the directory is NOT deleted

#### Scenario: AskUser marker blocks auto-cleanup
- **WHEN** the directory contains `openspec/changes/<slug>/.question.json` OR `.answer.json`
- **THEN** the safety check returns Err naming the marker
- **AND** auto-cleanup is refused

#### Scenario: In-progress lock blocks auto-cleanup
- **WHEN** the directory contains a `.in-progress*` file at any depth
- **THEN** the safety check returns Err naming the lock
- **AND** auto-cleanup is refused

#### Scenario: Alert-state file is NOT a tripwire
- **WHEN** the directory contains `.alert-state.json` at the root AND has no other tripwires
- **THEN** the safety check returns Ok
- **AND** auto-cleanup proceeds normally
- **AND** `.alert-state.json` is deleted along with the partial workspace (it is daemon-written and will be re-created on the next failure if any)

#### Scenario: Empty directory passes the safety check
- **WHEN** the directory exists AND is empty (or contains only daemon-written artifacts)
- **THEN** the safety check returns Ok
- **AND** auto-cleanup proceeds

### Requirement: Existing workspace paths take the appropriate code path unchanged
The auto-cleanup branch SHALL fire ONLY when the workspace directory exists AND has no `.git/` subdirectory. Workspaces that don't exist at all SHALL continue to take the fresh-clone path (unchanged from prior behaviour). Workspaces with a valid `.git/` SHALL continue to take the fetch-and-pull path (unchanged from prior behaviour). The auto-cleanup must NOT alter either of these existing code paths.

#### Scenario: Workspace doesn't exist → fresh clone, no auto-cleanup path
- **WHEN** the workspace directory does not exist on disk
- **THEN** the auto-cleanup branch is NOT entered
- **AND** the normal clone path runs (creates the directory and clones into it)

#### Scenario: Workspace has valid .git/ → fetch/pull, no auto-cleanup path
- **WHEN** the workspace directory exists AND contains a `.git/` subdirectory
- **THEN** the auto-cleanup branch is NOT entered
- **AND** the normal fetch + pull path runs
