## ADDED Requirements

### Requirement: CHATOPS.md status reply documentation enumerates the new `currently:` line variants
`docs/CHATOPS.md`'s operator-recovery-commands section (where the `status` verb's reply shape is documented) SHALL include examples of every `currently:` line variant introduced by this spec AND explain the diagnostic value of each.

#### Scenario: Reply-shape examples include every variant
- **WHEN** an operator reads `docs/CHATOPS.md`'s `status` reply-shape examples
- **THEN** at least one example each appears for: `idle`, `working on <change>`, `running audit <type>`, `<stage> in progress`, `recovery in progress`, `stale marker from pid <pid> (... recovery eligible now)`, `stale marker from pid <pid> (... recovery in <duration>)`

#### Scenario: Section explains the diagnostic value
- **WHEN** an operator reads the section
- **THEN** a paragraph explains that the `currently:` line distinguishes "audit in flight, just wait" from "stale marker, need recovery to fire (or manual `rm`)" from "truly idle"
- **AND** the paragraph cross-links to `OPERATIONS.md`'s busy-marker section for the underlying classification logic
- **AND** the paragraph cross-links to `TROUBLESHOOTING.md`'s stale-marker section for the immediate-fix-by-hand path
