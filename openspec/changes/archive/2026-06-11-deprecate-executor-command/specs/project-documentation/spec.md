# project-documentation — delta for deprecate-executor-command

## MODIFIED Requirements

### Requirement: config.example.yaml is the canonical operator reference for the YAML schema
The repository SHALL maintain `config.example.yaml` at the repo root as the operator-facing reference for every configurable field accepted by `Config` and its nested types. Every YAML-deserializable field — including fields whose default behavior makes them safe to omit — SHALL appear in the example, either as an active default value or as a commented annotation explaining what it does and what values are accepted, EXCEPT a field marked **deprecated** in the source. When a change ships a new configurable field, the change's commit MUST also update `config.example.yaml` so the example never lags the schema.

A field marked deprecated in the source (a `DEPRECATED:` doc comment on the field) is **intentionally absent** from the operator-facing surfaces: it SHALL be removed from `config.example.yaml` AND from `docs/CONFIG.md`, so those references carry no cruft. A deprecated field SHALL remain **accepted by the deserializer AND honored** (no behavior change) so existing configs do not break; it is simply no longer advertised. New operators are steered to the supported alternative in the relevant docs section.

A CI-enforceable check (typically a unit test under `config::tests`) SHALL fail when a NON-deprecated documented field name does not appear as a substring in the example file. This catches omissions at build time rather than at operator-onboarding time. A deprecated field is NOT required to appear; when its name is not shared with a live field, it SHALL be removed from the check's field-name list (when the name is still live via another field, it MAY remain).

#### Scenario: Adding a new configurable field
- **WHEN** an implementing agent adds a new YAML-deserializable field
  to any struct used in `Config` deserialization (top-level
  `Config`, `RepositoryConfig`, `ExecutorConfig`, `GithubConfig`,
  `ReviewerConfig`, `ChatOpsConfig`, `AuditsConfig`, etc.)
- **THEN** the same commit SHALL update `config.example.yaml` with
  a corresponding entry — either active (showing the default value)
  or commented (showing typical usage with an explanatory comment)
- **AND** the same commit SHALL update the coverage test's field-name
  list so the test continues to assert the new field is present
- **AND** the change's commit message or PR description names the
  new field so reviewers can confirm all three artifacts (struct
  field, example entry, test list entry) landed together

#### Scenario: Coverage test catches a missing field
- **WHEN** a developer adds a new field to the schema AND updates
  the example AND updates the test field-name list, but the example
  entry has a typo (e.g., `recreate_fork_on_init` instead of
  `recreate_fork_on_reinit`)
- **THEN** the coverage test fails with a message naming the
  missing field name AND pointing the developer at both
  `config.example.yaml` and the test's field-name list so the
  source of truth is unambiguous

#### Scenario: A field is genuinely never useful in the example
- **WHEN** a new field is added that has no plausible operator-set
  value (e.g., an internal-only flag that only autocoder itself
  flips at runtime, exposed in the struct purely for serde
  round-tripping)
- **THEN** the field is still added to `config.example.yaml` as a
  commented entry whose comment explicitly notes "internal — do
  not set" so the operator knows it exists AND that they should
  not configure it
- **AND** the coverage test continues to assert the field name
  appears in the file (the comment counts as a mention)

#### Scenario: Existing optional features ship un-commented in the example
- **WHEN** the example file documents an optional feature (e.g.,
  `reviewer:`, `chatops:`, `audits:`) that is disabled by default
- **THEN** the entire feature block SHALL appear commented out,
  with a header comment explaining what the feature does and a
  pointer to the relevant README section
- **AND** each nested field within the commented block SHALL appear
  at least once so an operator who uncomments the block sees every
  knob the feature exposes

#### Scenario: A deprecated field is undocumented
- **WHEN** a configurable field is marked deprecated in the source (a `DEPRECATED:` doc comment)
- **THEN** it is removed from `config.example.yaml` AND from `docs/CONFIG.md`, so neither operator-facing reference advertises it
- **AND** it remains accepted by the deserializer AND honored at runtime, so an existing config that sets it does NOT break
- **AND** the coverage test does NOT require it to appear in the example (it is removed from the field-name list unless the name is still live via another, non-deprecated field)
