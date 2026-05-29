# openspec-queue-engine Specification

## Purpose
TBD - created by archiving change orchestrator-architecture. Update Purpose after archive.
## Requirements
### Requirement: Enumerate ready changes
The queue engine SHALL list pending OpenSpec changes in the workspace, excluding archived, locked, waiting, perma-stuck, dotfile, and non-directory entries. The returned list SHALL be sorted by a two-tier ordering: changes with an `.iteration-pending.json` marker SHALL come first (sorted by the marker's `iteration_number` ascending), followed by unmarked changes sorted ascending by entry name (UTF-8 byte order, which is also alphabetical for ASCII names).

The `.iteration-pending.json` marker (written by the polling loop's `IterationRequested` arm per the orchestrator-cli capability) indicates that the change is mid-iteration AND its continuation SHALL preempt other pending work in the same repo. The marker is NOT an exclusion (unlike `.question.json`, which IS a block) — iteration-pending changes are still pending AND eligible for processing; they simply sort ahead of unmarked entries.

A corrupt `.iteration-pending.json` (truncated JSON, missing `iteration_number` field, parse failure) SHALL be treated as `iteration_number: 0` for ordering purposes, placing the entry first within the marked tier. The enumeration SHALL NOT fail on a corrupt marker; the polling loop AND the prompt-builder each handle corrupt-marker recovery per their respective capability requirements.

#### Scenario: Listing the queue
- **WHEN** the queue engine is queried for pending changes in a workspace
- **THEN** it returns the names of every direct subdirectory of `<workspace>/openspec/changes/` that satisfies ALL of the following:
  - the entry is a directory (not a file or symlink)
  - the entry name is not the literal string `archive`
  - the entry name does not begin with `.`
  - the entry does NOT contain a file named `.in-progress`
  - the entry does NOT contain a file named `.question.json`
  - the entry does NOT contain a file named `.perma-stuck.json`
  - the entry contains at least a regular file named `proposal.md`
- **AND** the returned list is sorted ascending by entry name

#### Scenario: Alphabetical order is deterministic across git operations
- **WHEN** the workspace state is altered by any git operation
  (clone, fetch, pull, checkout, reset, merge) that changes
  proposal.md mtimes
- **THEN** `list_pending` returns the same order as before the
  operation (entry names are stable across git operations)
- **AND** operators who require explicit sequencing prepend a
  numeric or alphabetical prefix to change names (e.g.
  `01-rename-foo`, `02-extract-bar`) to control order

#### Scenario: Iteration-pending marker preempts alphabetical order
- **WHEN** the queue engine is queried for pending changes in a workspace containing `a30-foo/` (no marker) AND `a31-bar/.iteration-pending.json` (marker with `iteration_number: 2`)
- **THEN** the returned list is `["a31-bar", "a30-foo"]`
- **AND** the iteration-pending entry comes first despite alphabetical disadvantage
- **AND** the unmarked entry follows in its normal alphabetical slot

#### Scenario: Multiple iteration-pending changes sort by iteration_number ascending
- **WHEN** the queue engine is queried for pending changes in a workspace containing `a30-foo/.iteration-pending.json` (marker with `iteration_number: 3`) AND `a31-bar/.iteration-pending.json` (marker with `iteration_number: 2`)
- **THEN** the returned list is `["a31-bar", "a30-foo"]`
- **AND** the lower iteration_number sorts first within the marked tier

#### Scenario: Corrupt iteration-pending marker does not break enumeration
- **WHEN** the queue engine is queried for pending changes AND one entry's `.iteration-pending.json` is truncated mid-JSON
- **THEN** the enumeration does NOT error
- **AND** the corrupt entry is treated as `iteration_number: 0` for ordering (sorts first within the marked tier)
- **AND** subsequent valid markers sort by their actual iteration_number ascending behind the corrupt entry

#### Scenario: Iteration-pending marker is NOT an exclusion
- **WHEN** the queue engine is queried for pending changes AND one entry has `.iteration-pending.json` present
- **THEN** that entry IS returned in the pending list (not excluded)
- **AND** the existing `.question.json` AND `.perma-stuck.json` exclusion behaviour is unchanged for entries with those markers

### Requirement: Lock state management
The queue engine SHALL atomically lock and unlock changes via filesystem markers to prevent duplicate execution and to signal in-progress state to humans inspecting the workspace.

#### Scenario: Locking a change
- **WHEN** the orchestrator selects a change for execution
- **THEN** the queue engine creates an empty file at `<workspace>/openspec/changes/<change>/.in-progress` BEFORE invoking the executor
- **AND** the file is verifiable on disk via standard filesystem inspection (e.g. `ls -a`)

#### Scenario: Unlocking after any executor outcome
- **WHEN** the executor returns ANY outcome (`Completed`, `AskUser`, `Failed`) OR the executor invocation panics
- **THEN** the queue engine deletes the `.in-progress` file
- **AND** the deletion is idempotent (no error if the file is already absent)

#### Scenario: Stale lock cleanup on startup
- **WHEN** the orchestrator initializes a workspace at process startup
- **THEN** any pre-existing `.in-progress` files inside `<workspace>/openspec/changes/<change>/` are deleted before the polling loop for that repository begins
- **AND** a log line is emitted for each lock cleared, naming the change

### Requirement: Archive on completion
The queue engine SHALL move successfully implemented changes into a dated archive subdirectory only after the corresponding git commit has been recorded.

#### Scenario: Archiving a completed change
- **WHEN** the executor returns `Completed` for `<change>` AND the git workflow manager has recorded a commit on the agent branch attributable to that change
- **THEN** the queue engine renames `<workspace>/openspec/changes/<change>/` to `<workspace>/openspec/changes/archive/<YYYY-MM-DD>-<change>/`, where `<YYYY-MM-DD>` is the UTC date of the rename
- **AND** if the destination path already exists, the engine returns an error naming the conflict and does NOT overwrite the existing archive entry

### Requirement: Unarchive on rewind
The queue engine SHALL move a previously archived change back into the active queue when requested by the rewind subcommand.

#### Scenario: Unarchiving a single change
- **WHEN** `unarchive_change(<name>)` is called against a workspace
- **THEN** the engine searches `<workspace>/openspec/changes/archive/` for directory names matching the regex `^\d{4}-\d{2}-\d{2}-<name>$`, selects the lexicographically highest match (most recently archived), strips the date prefix, and renames it to `<workspace>/openspec/changes/<name>/`
- **AND** if no match is found, the engine returns an error naming the requested change
- **AND** if the destination `<workspace>/openspec/changes/<name>/` already exists, the engine returns an error and does NOT overwrite

### Requirement: Enumerate waiting changes
The queue engine SHALL provide a separate enumeration of changes currently waiting on a human answer (i.e. those containing a `.question.json` file).

#### Scenario: Listing waiting changes
- **WHEN** `list_waiting(workspace)` is called
- **THEN** it returns the names of every direct subdirectory of `<workspace>/openspec/changes/` that contains a `.question.json` file (regardless of whether `.answer.json` is also present)
- **AND** the returned list is sorted ascending by entry name
- **AND** archived directories are excluded
- **AND** entries beginning with `.` are excluded

### Requirement: Perma-stuck marker write guards against missing change directory
The `perma_stuck::write_marker` helper SHALL refuse to write the marker file when the change's directory under `<workspace>/openspec/changes/` does not exist. The error SHALL name the missing directory so a caller looking at logs can tell the change was deleted out-of-band rather than blaming the marker-write step itself. The guard SHALL fire BEFORE any filesystem write so a failed call leaves no partial state behind.

#### Scenario: write_marker is called for a change directory that does not exist
- **WHEN** `perma_stuck::write_marker(workspace, "foo", &entry)` is called AND `<workspace>/openspec/changes/foo/` does not exist
- **THEN** the call returns `Err(_)` whose message contains the substring `change directory does not exist` AND the change name `foo`
- **AND** `<workspace>/openspec/changes/foo/.perma-stuck.json` does NOT exist on the filesystem after the failed call (the guard runs before any tempfile is created)

