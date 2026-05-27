## ADDED Requirements

### Requirement: Startup preflight for openspec availability
autocoder SHALL verify that the `openspec` binary is available before the polling loop starts. A failed preflight aborts daemon startup with a non-zero exit code, ensuring a misconfigured deployment fails loudly instead of looping forever producing nothing.

#### Scenario: openspec is available
- **WHEN** the daemon starts and `Command::new("openspec").arg("--version")` exits 0
- **THEN** the preflight passes and the polling loop starts normally

#### Scenario: openspec binary not on PATH
- **WHEN** the daemon starts and spawning `openspec --version`
  returns a `NotFound` I/O error
- **THEN** the daemon exits non-zero before the polling loop starts
- **AND** stderr names the failure: `openspec preflight failed:
  binary not found on PATH. Install openspec and ensure the
  systemd unit's PATH covers its install directory.`

#### Scenario: openspec spawns but exits non-zero
- **WHEN** the daemon starts, `openspec --version` spawns
  successfully, but exits non-zero
- **THEN** the daemon exits non-zero before the polling loop starts
- **AND** stderr names the exit code and includes a tail of
  `openspec --version`'s stderr output (up to 200 chars)
