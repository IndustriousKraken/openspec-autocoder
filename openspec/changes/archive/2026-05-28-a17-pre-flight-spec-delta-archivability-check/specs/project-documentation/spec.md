## ADDED Requirements

### Requirement: OPERATIONS.md and TROUBLESHOOTING.md document the spec-delta pre-flight and the unarchivable-deltas marker shape
`docs/OPERATIONS.md`'s "Spec marked as needing revision" section SHALL be extended with a paragraph describing the new pre-flight failure mode (unarchivable spec deltas) AND the extended marker schema. `docs/TROUBLESHOOTING.md` SHALL include a new entry naming the specific archive-time error this pre-flight prevents.

#### Scenario: OPERATIONS.md describes the new failure mode
- **WHEN** an operator reads `docs/OPERATIONS.md`'s "Spec marked as needing revision" section
- **THEN** a paragraph names the pre-flight check, the four delta kinds it validates, AND the `unarchivable_deltas` field in the marker schema
- **AND** the paragraph explains the recovery workflow: edit the spec on the operator's machine, push to the base branch, `@<bot> clear-revision <repo> <change>` from chat
- **AND** the paragraph notes that the marker's `revision_suggestion` field is auto-generated AND names exactly which deltas need to be fixed

#### Scenario: TROUBLESHOOTING.md replaces a known operator-pain-point entry
- **WHEN** an operator reads `docs/TROUBLESHOOTING.md`
- **THEN** an entry titled "openspec archive aborts with 'MODIFIED failed for header'" exists
- **AND** the entry contrasts pre-a17 behavior (archive failed late; LLM cost wasted; change perma-stuck) with post-a17 behavior (pre-flight catches the issue early; no LLM cost; needs-spec-revision marker written immediately with actionable diagnostic)
- **AND** the entry references the marker's `unarchivable_deltas` array as the canonical place to find what's wrong
