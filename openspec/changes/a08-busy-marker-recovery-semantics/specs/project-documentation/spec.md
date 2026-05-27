## ADDED Requirements

### Requirement: OPERATIONS.md, CONFIG.md, and TROUBLESHOOTING.md document the busy-marker-stale-threshold field and the decoupled recovery semantics
`docs/OPERATIONS.md`'s `## Busy marker` section SHALL be updated to reflect the new classification ordering (dead-pid immediate, decoupled threshold). `docs/CONFIG.md`'s `executor:` table SHALL gain a row for `busy_marker_stale_threshold_secs`. `docs/TROUBLESHOOTING.md` SHALL include a "Repo stuck on stale busy marker after daemon restart" diagnostic section.

#### Scenario: OPERATIONS.md classification table reflects the new ordering
- **WHEN** an operator reads `docs/OPERATIONS.md`'s `## Busy marker` section
- **THEN** the classification table lists the branches in the spec's order
- **AND** the "PID dead" row notes that recovery fires immediately with no age check
- **AND** a paragraph explains that the threshold is the new `executor.busy_marker_stale_threshold_secs` field (default 600s) rather than the pre-spec `timeout_secs + 10 min` formula
- **AND** the paragraph names the migration log line operators will see if their pre-spec config had a longer implicit threshold

#### Scenario: CONFIG.md documents the new field
- **WHEN** an operator reads `docs/CONFIG.md`'s `executor:` table
- **THEN** the table contains a row for `busy_marker_stale_threshold_secs` (type `u64`, default `600`, max `7200`)
- **AND** the row describes the field's purpose (stale-threshold for the live-pid recovery branch) AND cross-links to the OPERATIONS.md section

#### Scenario: TROUBLESHOOTING.md helps operators diagnose stale-marker symptoms
- **WHEN** an operator reads `docs/TROUBLESHOOTING.md`
- **THEN** a section titled `Repo stuck on stale busy marker after daemon restart` describes the symptom (status shows `currently: idle`, queue shows pending changes, but every polling iteration logs `busy marker present; skipping`)
- **AND** the section gives the diagnostic commands (`ls`, `cat`, `ps -p <pid>`)
- **AND** the section gives the immediate fix (`rm` the marker file)
- **AND** the section notes that the underlying cause for dead-pid markers is fixed in this spec — operators upgrading to this version no longer hit the symptom for daemon-restart scenarios
