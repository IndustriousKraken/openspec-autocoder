## ADDED Requirements

### Requirement: Living Documentation
The system's implementation agents SHALL update the `README.md` and related `docs/` files to accurately reflect the current state of the application.

#### Scenario: Implementing a new feature
- **WHEN** an implementation agent adds a new user-facing feature, CLI command, or configuration option
- **THEN** it updates the documentation to explain how to use it, noting any planned but unimplemented elements as explicitly "aspirational".

### Requirement: Reviewer Documentation Verification
The reviewer agent SHALL verify that significant changes include corresponding documentation updates.

#### Scenario: Reviewing a PR without docs
- **WHEN** the reviewer agent analyzes a PR containing architectural or configuration changes
- **THEN** it flags the PR if the `README.md` or relevant `docs/` files were not updated to match the new behavior.
