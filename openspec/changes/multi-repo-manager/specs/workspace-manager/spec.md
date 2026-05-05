## ADDED Requirements

### Requirement: Workspace cloning
The system SHALL maintain a local clone of each watched repository in a temporary workspace directory.

#### Scenario: Initializing a workspace
- **WHEN** a polling loop starts for a repository
- **THEN** the system checks if a local clone exists in `/tmp/workspaces/<repo>`, and if not, performs a `git clone`
