## ADDED Requirements

### Requirement: OPERATIONS.md and CONFIG.md document `max_audits_per_iteration`
`docs/OPERATIONS.md`'s `## Periodic audits` section SHALL include a paragraph describing the `audits.max_audits_per_iteration` bound, its default (`1`), the rationale (prevent storm patterns), the override pattern, AND the interaction with on-demand queued runs. `docs/CONFIG.md`'s `audits:` table SHALL gain a row for the field.

#### Scenario: OPERATIONS.md describes the bound and its rationale
- **WHEN** an operator reads `docs/OPERATIONS.md`'s `## Periodic audits` section
- **THEN** a paragraph names `audits.max_audits_per_iteration` AND its default `1`
- **AND** the paragraph explains the rationale (preventing audit storms when many audits become eligible simultaneously, e.g. after a HEAD change)
- **AND** the paragraph names the typical override values (e.g. `3` for fast drainage during onboarding) AND the trade-off (longer iteration wall-clock per cycle)
- **AND** the paragraph explains that on-demand queued audits count against the bound — operators queuing many audits via `@<bot> audit ...` see them drain one per iteration at the default

#### Scenario: CONFIG.md documents the field
- **WHEN** an operator reads `docs/CONFIG.md`'s `audits:` table
- **THEN** the table contains a row for `max_audits_per_iteration` (type `usize`, default `1`, max `<count of registered audits>`)
- **AND** the row cross-links to the OPERATIONS.md section for the full discussion
