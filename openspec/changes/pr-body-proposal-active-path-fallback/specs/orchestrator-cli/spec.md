## ADDED Requirements

### Requirement: PR-body proposal lookup falls back to the active path
The polling iteration's PR-body assembly SHALL look up each change's `proposal.md` in two steps: first under `openspec/changes/archive/*-<change>/proposal.md` (the established archived-change location), and on miss, second under `openspec/changes/<change>/proposal.md` (the active-path location). When the active-path fallback finds a proposal with a parseable `## Why` section, the lookup SHALL succeed AND the daemon SHALL emit a WARN log naming the change so operators can correlate the PR with the upstream archive-failure that left the change unarchived. When both paths miss OR neither yields a parseable `## Why`, the existing `_(no proposal.md available)_` PR-body fallback continues to render.

#### Scenario: Archive path wins when present
- **WHEN** a change's `proposal.md` exists at `openspec/changes/archive/<date>-<change>/proposal.md` with a parseable `## Why` section
- **THEN** the PR-body assembly returns the archive-path `## Why` content
- **AND** no active-path fallback is attempted
- **AND** no WARN log is emitted (the archived case is the happy path)

#### Scenario: Active path is consulted when archive is empty
- **WHEN** no `openspec/changes/archive/*-<change>/proposal.md` exists AND `openspec/changes/<change>/proposal.md` exists with a parseable `## Why` section
- **THEN** the PR-body assembly returns the active-path `## Why` content
- **AND** the daemon emits a single WARN log naming the change with text indicating the proposal was read from the active path

#### Scenario: Both paths missing
- **WHEN** neither the archive-path nor the active-path proposal file exists
- **THEN** the PR-body assembly returns no content for that change
- **AND** no WARN log is emitted (the operator already sees `_(no proposal.md available)_` in the PR body; a journal WARN for genuinely-missing files would be noise)

#### Scenario: Active path exists but lacks a `## Why` section
- **WHEN** no archive-path proposal exists AND `openspec/changes/<change>/proposal.md` exists but does NOT contain a `## Why` heading
- **THEN** the PR-body assembly returns no content for that change
- **AND** no WARN log is emitted (the fallback found a file but extracted no content, identical to the archive-path-with-malformed-proposal case)

#### Scenario: Archive present, active also present
- **WHEN** both `openspec/changes/archive/<date>-<change>/proposal.md` AND `openspec/changes/<change>/proposal.md` exist
- **THEN** the archive-path `## Why` content is returned (deterministic preference)
- **AND** no WARN log is emitted
