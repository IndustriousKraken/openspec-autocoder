## ADDED Requirements

### Requirement: OPERATIONS.md describes the new iteration ordering and the audit-to-implementation one-iteration delay
`docs/OPERATIONS.md`'s `## Periodic audits` section SHALL be updated to reflect that audits run AFTER the pending change queue walk (not before, as the pre-spec text stated). The same section SHALL include a paragraph explaining the one-iteration delay for audit-generated changes' implementation AND why the trade-off is favorable.

#### Scenario: OPERATIONS.md correctly names the new ordering
- **WHEN** an operator reads `docs/OPERATIONS.md`'s `## Periodic audits` section
- **THEN** the "When audits fire" paragraph reads "audits run AFTER `list_pending`" (or equivalent), not "BEFORE `list_pending`"
- **AND** the paragraph notes the motivation (preventing audit-storm monopolization when many audits become eligible at once)

#### Scenario: OPERATIONS.md explains the audit-to-implementation delay
- **WHEN** an operator reads the same section
- **THEN** a paragraph describes the one-iteration delay: an audit running in iteration N creates proposals that the implementer picks up in iteration N+1
- **AND** the paragraph explains the operator-visible effect: audit creation commits ship in one PR, audit-generated change implementations ship in a follow-up PR
- **AND** the paragraph names the benefit: reviewers see proposal contents before implementation, and can `@<bot> revise <text>` the proposals before implementer runs in the next iteration
