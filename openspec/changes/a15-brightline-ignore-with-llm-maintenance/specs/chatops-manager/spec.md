## ADDED Requirements

### Requirement: Brightline chatops top-line admits a stale-ignore-cleanup clause
The brightline audit's chatops top-line (the `📐` notification) SHALL include a trailing `; <K> stale ignore entries to clean up` clause when the audit detected `K > 0` stale entries in the workspace's `.brightline-ignore` file. The threaded body SHALL list each stale entry's `file + function + reason` so the operator can identify what to remove. This clause is informational only — the audit does NOT modify `.brightline-ignore` (brightline declares `WritePolicy::None`).

#### Scenario: Stale entries surface in the top-line and body
- **WHEN** a brightline run finds 1 oversize file, 2 duplicate signatures (1 fully ignored, 1 not), AND 3 stale ignore entries
- **THEN** the chatops top-line reads `📐 architecture_brightline on <repo>: 1 file(s) over line threshold; 1 duplicate signature(s); 3 stale ignore entries to clean up`
- **AND** the threaded body lists each stale entry with `file + function + reason`

#### Scenario: No stale entries produces no clause
- **WHEN** a brightline run finds no stale ignore entries (every entry validates against the current workspace)
- **THEN** the chatops top-line is the pre-spec format without the trailing stale-cleanup clause
