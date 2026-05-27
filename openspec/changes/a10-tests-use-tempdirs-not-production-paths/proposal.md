## Why

On a production deployment we observed test-fixture state files (`42.0.json`, `9999.0001.json`) appearing in `/tmp/autocoder/audit-threads/`. Investigation: the daemon's wrapped agent (claude CLI) runs `cargo test` against the autocoder workspace as part of implementing changes. Autocoder's test suite uses `/tmp/autocoder/audit-threads/` as a fixture path — the literal production daemon path — instead of per-test tempdirs. The result: tests writing to the SAME directory the live daemon reads / writes.

This is benign on most hosts (the fixture filenames `42.0.json` look obviously synthetic AND the production daemon's lookups by Slack `thread_ts` don't match them). But it's exactly the kind of latent bug that becomes catastrophic when a future test happens to use a Slack-ts-shaped filename, OR when two daemons on the same host run tests in different workspaces and trample each other's state, OR when a test's cleanup logic deletes real production state by accident.

The fix is a discipline rule + CI enforcement: tests NEVER write to the daemon's production state paths. Each test uses `tempfile::tempdir()` (or sets `AUTOCODER_*_DIR` env vars to point at per-test temp paths) AND scopes all writes there.

## What Changes

**Test discipline: per-test tempdirs.** Every test that needs a state directory uses `tempfile::tempdir()` (returning a `TempDir` that auto-cleans on drop). State writes go to paths under that tempdir, NOT under `/tmp/autocoder/...`. Where the daemon's path resolver is involved, tests construct a `DaemonPaths` instance pointing at the tempdir AND pass it through — the same surface production code uses via `a09`'s rule.

**Helper for test fixtures.** A new `autocoder/src/testing.rs` (or extension to an existing test-support module) provides a `test_daemon_paths()` helper that returns a `(TempDir, DaemonPaths)` tuple, with the four daemon dirs (state, cache, logs, runtime) scoped under the tempdir's root. Tests use this helper as the single onramp; `TempDir` is dropped at test end and cleans up.

**CI-enforceable check.** Extending the path-literals audit from `a09`: a new test (or an extension) scans `autocoder/src/**/*.rs` AND `autocoder/tests/**/*.rs` for the literal substring `/tmp/autocoder` AND fails if any hit appears outside the allowlist. The allowlist for tests is empty (no test should reference the production path literal).

**Audit and cleanup of existing test code.** A sweep over the existing test surface (every `#[test]` AND `#[tokio::test]` function) identifies sites that write outside a tempdir. Each gets refactored to use `test_daemon_paths()` (or equivalent). Tests that depend on `AUTOCODER_*_DIR` env vars set them in the test's own scope (via `temp_env::with_var(...)` or similar) so the test's effects don't leak across tests OR to the production daemon.

**Existing legacy artifacts.** Tests may have left `/tmp/autocoder/audit-threads/42.0.json` etc. files on existing dev machines. These don't actively harm anything but are clutter. A one-line note in `docs/test-reliability.md` says operators can `rm -rf /tmp/autocoder/audit-threads/*` to clean up if they want; the daemon never reads from that path post-`a09`.

## Impact

- **Affected specs:**
  - `project-documentation` — one ADDED requirement: `Test suite uses per-test tempdirs; CI grep enforces no /tmp/autocoder literals in test code`.
- **Affected code:**
  - `autocoder/src/testing.rs` (new, or extension to existing) — `test_daemon_paths()` helper.
  - Every `autocoder/src/**/*.rs` test that currently writes to `/tmp/autocoder/...` — refactor to use the helper. The sweep identifies the affected sites:
    ```bash
    grep -rn '/tmp/autocoder' autocoder/src/ | grep -E '#\[(test|tokio::test)\]|fn .*test' -B 5 -A 50
    ```
  - `autocoder/tests/path_literals_audit.rs` (the test from `a09`) — extended to also scan test code paths.
  - `docs/test-reliability.md` — add a "Test isolation" disposition entry naming the rule.
- **Operator-visible behavior:**
  - On hosts where autocoder runs its own test suite (e.g., when autocoder works on itself), no test-fixture files leak into `/tmp/autocoder/...`. Operators inspecting their daemon's state paths see only real daemon state.
  - Future tests can't accidentally reintroduce the leak — the CI check fails immediately.
- **Breaking:** no functional change to the daemon. Test-code refactor only.
- **Acceptance:** `cargo test` passes (existing tests still pass after refactor). The extended path-literals audit test passes against the swept test surface. `openspec validate a13-tests-use-tempdirs-not-production-paths --strict` passes.
