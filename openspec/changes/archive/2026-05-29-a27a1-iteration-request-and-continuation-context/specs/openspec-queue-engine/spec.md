## MODIFIED Requirements

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
