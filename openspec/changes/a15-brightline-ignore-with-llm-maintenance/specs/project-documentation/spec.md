## ADDED Requirements

### Requirement: OPERATIONS.md describes the `.brightline-ignore` file and CHATOPS.md cross-links from `send it`
`docs/OPERATIONS.md`'s `architecture_brightline` audit section SHALL include a `.brightline-ignore` subsection describing the file's purpose, location, YAML schema, match-suppression behavior, stale-entry handling, AND the `send it` integration. `docs/CHATOPS.md`'s `send it` section SHALL cross-link to the OPERATIONS.md subsection so operators discovering one find the other.

#### Scenario: OPERATIONS.md describes the ignore file completely
- **WHEN** an operator reads `docs/OPERATIONS.md`'s `architecture_brightline` section
- **THEN** a `.brightline-ignore` subsection appears with the workspace-root path, the YAML schema, AND examples
- **AND** the section describes the match-suppression rule (all sites match → suppress; partial → emit unmatched only)
- **AND** the section describes the stale-entry handling (informational chatops clause; operator removes entries manually)
- **AND** the section describes the `send it` integration (the LLM populates entries when classifying findings as intentional)

#### Scenario: CHATOPS.md `send it` section cross-links to `.brightline-ignore`
- **WHEN** an operator reads `docs/CHATOPS.md`'s `send it` section
- **THEN** the section's brightline-handling paragraph cross-links to `OPERATIONS.md#brightline-ignore`
- **AND** the cross-link explains that `send it` on brightline findings can produce `.brightline-ignore` updates instead of (or in addition to) code fixes
