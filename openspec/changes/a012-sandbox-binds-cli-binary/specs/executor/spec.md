# executor — delta for a012-sandbox-binds-cli-binary

## ADDED Requirements

### Requirement: The sandbox binds the wrapped CLI binary even when installed under the home directory
The sandbox's filesystem allowlist SHALL include the resolved CLI binary for the running strategy AND its runtime dependency closure (the interpreter, bundled modules, AND shared libraries it loads), wherever they are installed — INCLUDING under the home directory (e.g. `~/.local/bin/<cli>`, the default install location for `claude` / `agy` / `opencode`) — bound read-only AND executable so the wrapped CLI can be exec'd inside the sandbox. Binary resolution SHALL follow symlinks to the real target.

This is the resolution of the "minimal runtime (binaries/libraries)" element of the allowlist for user-local installs. The binding SHALL be surgical: the specific binary AND its dependencies, NOT the whole home directory — the rest of the home directory (other CLIs' config stores, `~/.ssh`, autocoder's own config and state) stays masked. It applies on every mechanism: `bwrap` (bound back after the home tmpfs), `systemd-run` (`BindReadOnlyPaths`), AND the macOS `sandbox-exec` profile (read+exec on the binary path). A CLI binary that is masked and therefore cannot be exec'd inside the sandbox (`execvp … No such file or directory`) is a sandbox-allowlist defect, not a missing dependency.

#### Scenario: A CLI installed under the home directory is exec-able in the sandbox
- **WHEN** the running strategy's CLI binary is installed under the home directory (e.g. `~/.local/bin/<cli>`)
- **THEN** the sandbox binds that binary — following symlinks to its real target — AND its runtime dependency closure, read-only and executable
- **AND** the wrapped CLI execs successfully inside the sandbox

#### Scenario: The rest of the home directory stays masked
- **WHEN** the CLI binary under the home directory is bound into the sandbox
- **THEN** only that binary and its dependencies are bound from the home directory
- **AND** other CLIs' config stores, `~/.ssh`, AND autocoder's own config and state remain masked

#### Scenario: A system-path CLI is also resolved
- **WHEN** the running strategy's CLI binary is installed on a system path (e.g. `/usr/local/bin/<cli>`)
- **THEN** the sandbox resolves and includes it as well, with no regression for system installs
