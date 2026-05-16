## ADDED Requirements

### Requirement: Developer-facing test-reliability reference
The repository SHALL include a developer-facing reference document at `docs/test-reliability.md` that enumerates known sources of test-suite flakiness, their root causes (where determinable), and their dispositions. The document is a living artifact: implementing agents that introduce new tests, fix existing flakes, or discover new flake patterns SHALL update the disposition table.

The reference is NOT a user-facing spec — it does not describe runtime behavior — but it is in-scope for `project-documentation` because it serves the same audience (operators and implementing agents) and the same purpose (preserving non-obvious knowledge that would otherwise have to be re-derived from logs and grep).

#### Scenario: Adding a new test that's expected to be reliable
- **WHEN** an implementing agent adds a new test that uses deterministic primitives (no wall-clock, no env mutation without a lock, no shared mockito server, no hard-coded /tmp paths)
- **THEN** no update to `docs/test-reliability.md` is required — the document tracks known risks, not every test that's correctly written

#### Scenario: Discovering a new flake
- **WHEN** an implementing agent observes a test failing intermittently AND can characterize the root cause (timing race, env race, mockito port collision, filesystem collision, etc.)
- **THEN** the agent SHALL add an entry to the disposition table in `docs/test-reliability.md` with the test name, module, category, and chosen disposition (one of `fixed-in-this-change`, `mitigated`, `accepted-known-flaky`, `unfixable-needs-architecture-change`, `not-flaky-on-inspection`)
- **AND** if the disposition is `fixed-in-this-change`, the agent's commit MUST include the fix and the entry MAY be moved to a "Resolved flakes" section in a follow-up cleanup
- **AND** if the disposition is `unfixable-needs-architecture-change`, the entry SHALL describe the architectural change required (e.g. "wire an injectable clock through `AuditScheduler::run`") so a future change has a starting point

#### Scenario: Investigating a reported flake whose name cannot be located in the tree
- **WHEN** an operator reports a flake by name AND the name cannot be matched in the current tree or git history
- **THEN** the investigating agent SHALL document the negative result in `docs/test-reliability.md` (with the grep commands tried) AND proceed with a category-based audit rather than blocking on the named test
- **AND** the report MAY note that the originally-named test was unlocatable, so future operators don't reopen the same investigation looking for the same ghost
