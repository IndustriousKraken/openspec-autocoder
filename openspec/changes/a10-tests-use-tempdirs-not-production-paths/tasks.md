## 1. Test helper

- [ ] 1.1 In `autocoder/src/testing.rs` (create if absent; mark `#[cfg(test)]` AND/OR put behind a `test-support` feature so it doesn't bloat the production binary), add:
  ```rust
  use tempfile::TempDir;
  use crate::paths::DaemonPaths;

  pub fn test_daemon_paths() -> (TempDir, DaemonPaths) {
      let tempdir = TempDir::new().expect("create tempdir");
      let root = tempdir.path();
      let paths = DaemonPaths::from_explicit(
          /* state_dir */ root.join("state"),
          /* cache_dir */ root.join("cache"),
          /* logs_dir */ root.join("logs"),
          /* runtime_dir */ root.join("runtime"),
      );
      for dir in [paths.state_dir(), paths.cache_dir(), paths.logs_dir(), paths.runtime_dir()] {
          std::fs::create_dir_all(dir).unwrap();
      }
      (tempdir, paths)
  }
  ```
- [ ] 1.2 The `TempDir` must be returned (not dropped inside the helper) so the test's binding keeps it alive for the test's duration. Drop on test end auto-cleans.
- [ ] 1.3 Tests for the helper itself: assert the four subdirectories exist; assert tempdir is cleaned up after drop.

## 2. Sweep + refactor existing tests

- [ ] 2.1 Run a sweep to identify tests writing to literal `/tmp/autocoder/...` paths:
  ```bash
  grep -rln '/tmp/autocoder' autocoder/src/ | xargs -I{} grep -l '#\[test\]\|#\[tokio::test\]' {}
  ```
- [ ] 2.2 For each test surfaced, refactor:
  - Replace `let path = Path::new("/tmp/autocoder/audit-threads")` with `let (_temp, paths) = test_daemon_paths(); let path = paths.audit_threads_dir();`.
  - The leading `let _temp = ...` binding keeps the tempdir alive for the test's scope.
  - Pass `paths` (or `DaemonPaths` reference) into the daemon code being tested via whatever constructor parameter it uses.
- [ ] 2.3 Tests that set env vars (`AUTOCODER_STATE_DIR` etc.): replace direct `std::env::set_var` with `temp_env::with_var(name, value, || { ... })` to ensure the env var is scoped to the test AND doesn't leak.
- [ ] 2.4 Run `cargo test` after each batch of edits to catch regressions early.

## 3. Extend the path-literals audit to test code

- [ ] 3.1 Modify `autocoder/tests/path_literals_audit.rs` (from `a09`) to scan `autocoder/tests/` AND any test modules in `autocoder/src/` for the literal substring.
- [ ] 3.2 The allowlist for test code is empty (no test should reference the production path literal). Any hit is a refactor target.
- [ ] 3.3 Confirm the test passes against the swept codebase.

## 4. Cleanup note in test-reliability.md

- [ ] 4.1 In `docs/test-reliability.md`, add an entry to the disposition table:
  ```
  | Pattern                         | Module       | Category | Disposition           | Note                                                               |
  |---------------------------------|--------------|----------|-----------------------|--------------------------------------------------------------------|
  | Tests writing to /tmp/autocoder | (sweep wide) | filesystem | fixed-in-a13         | Test helper test_daemon_paths() introduced; CI grep prevents recurrence |
  ```
- [ ] 4.2 Add a "Test isolation" section above the table briefly explaining the rule: tests use `test_daemon_paths()` (or `temp_env::with_var(...)` for env-var-driven cases) AND never reference `/tmp/autocoder/...` literally. CI enforces.
- [ ] 4.3 Add a one-liner: existing dev machines may have stale `/tmp/autocoder/audit-threads/*.json` test fixtures from before this spec. `rm -rf /tmp/autocoder/` is safe (the daemon never reads from there post-`a09`).

## 5. Spec deltas

- [ ] 5.1 `openspec/changes/a10-tests-use-tempdirs-not-production-paths/specs/project-documentation/spec.md` ADDs one requirement covering the test-isolation rule, the helper, AND the CI check extension.

## 6. Verification

- [ ] 6.1 `cargo test` passes (new + existing).
- [ ] 6.2 `openspec validate a13-tests-use-tempdirs-not-production-paths --strict` passes.
- [ ] 6.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
- [ ] 6.4 Manual verification: after running `cargo test` on a dev machine, `ls /tmp/autocoder/` shows no test-fixture leakage.
