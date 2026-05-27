## ADDED Requirements

### Requirement: Test suite uses per-test tempdirs; CI grep enforces no `/tmp/autocoder` literals in test code
The autocoder test suite SHALL NOT write to any path the live daemon would legitimately use. Every test that needs a state directory SHALL use the `test_daemon_paths()` helper (which returns a tempdir-scoped `DaemonPaths`) OR an equivalent per-test tempdir. Tests setting `AUTOCODER_*_DIR` env vars SHALL use a scoped mechanism (e.g., `temp_env::with_var(...)`) so the env var doesn't leak across tests. The path-literals CI audit from `a09` SHALL be extended to scan test code; the test-code allowlist is empty.

The rule prevents two failure modes: (a) test fixtures leaking into production state paths when autocoder works on itself (the wrapped agent runs `cargo test` AND tests writing to `/tmp/autocoder/...` would land alongside live daemon state); (b) tests on parallel hosts trampling each other's state via shared `/tmp` paths.

#### Scenario: `test_daemon_paths()` returns a usable tempdir-scoped DaemonPaths
- **WHEN** a test calls `let (_temp, paths) = test_daemon_paths();`
- **THEN** the returned `DaemonPaths` has its four directories under the tempdir's root
- **AND** the four directories exist on disk
- **AND** dropping the `_temp` binding (at end of test) auto-cleans every file the test wrote

#### Scenario: CI grep catches new `/tmp/autocoder` literals in test code
- **WHEN** a contributor adds a hard-coded `/tmp/autocoder/...` path inside a test function
- **AND** `cargo test` runs
- **THEN** the `path_literals_audit` test fails with the offending file:line listed
- **AND** the failure message points at `test_daemon_paths()` as the correct fix

#### Scenario: Existing test surface is swept clean
- **WHEN** the path-literals audit runs against `autocoder/src/` AND `autocoder/tests/`
- **THEN** zero hits are found in test code
- **AND** every previously-offending test has been refactored to use `test_daemon_paths()` OR an equivalent per-test tempdir

#### Scenario: Env-var-setting tests are scoped
- **WHEN** a test needs to set `AUTOCODER_STATE_DIR` (or similar) to exercise a daemon code path that reads from env
- **THEN** the test uses a scoped mechanism (e.g., `temp_env::with_var("AUTOCODER_STATE_DIR", value, || { ... })`)
- **AND** the env var is unset when the closure returns
- **AND** parallel tests AND production daemons running on the same host are unaffected

#### Scenario: test-reliability.md documents the rule and the cleanup hint
- **WHEN** an operator reads `docs/test-reliability.md`
- **THEN** a "Test isolation" section names the per-test tempdir rule
- **AND** the disposition table contains an entry for the swept-and-fixed pattern
- **AND** a one-liner notes that operators with pre-spec dev machines can `rm -rf /tmp/autocoder/` to clean up stale test fixtures (the daemon never reads from there post-`a09`)
