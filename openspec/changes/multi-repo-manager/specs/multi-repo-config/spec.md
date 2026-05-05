## ADDED Requirements

### Requirement: Configuration parsing
The system SHALL parse a YAML configuration file to define watched repositories.

#### Scenario: Loading config
- **WHEN** the orchestrator starts
- **THEN** it reads `config.yaml` to retrieve a list of git repository URLs and their associated polling intervals
