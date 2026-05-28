## 1. Sweep the codebase

- [x] 1.1 Run `grep -rn '/tmp/autocoder' autocoder/src/` AND collect every hit into a working list.
- [x] 1.2 For each hit, classify:
  - **Category A: hard-coded read or write path.** Replace with a `DaemonPaths` call.
  - **Category B: legacy-path reference for migration.** Keep AS-IS; add to the CI check's allowlist with a comment.
  - **Category C: test code.** Move to per-test tempdirs (this overlaps with `a10`; if `a10` lands first, this category is empty).
- [x] 1.3 The expected categories of A hits (the swept fixes):
  - Audit-thread state reader/writer.
  - Busy-marker reader (writers should already be correct per the state-paths-out-of-tmp spec).
  - Failure-state reader/writer (verify per-workspace path resolution).
  - Revision-state reader/writer.
  - Per-change run log paths.
  - Audit-run log paths.
  - Alert-throttle state reader/writer.
  - Proposal-request state reader/writer.

## 2. Extend `DaemonPaths` with missing helpers

- [x] 2.1 In `autocoder/src/paths.rs` (or wherever `DaemonPaths` lives), add helper methods for every state-file category surfaced by the sweep. Pattern:
  ```rust
  impl DaemonPaths {
      pub fn audit_threads_dir(&self) -> PathBuf { self.state_dir().join("audit-threads") }
      pub fn busy_markers_dir(&self) -> PathBuf { self.runtime_dir().join("busy") }
      pub fn proposal_requests_dir(&self) -> PathBuf { self.state_dir().join("proposal-requests") }
      pub fn failure_state_dir(&self) -> PathBuf { self.state_dir().join("failure-state") }
      pub fn revisions_dir(&self) -> PathBuf { self.state_dir().join("revisions") }
      pub fn audit_logs_dir(&self, workspace_basename: &str) -> PathBuf {
          self.logs_dir().join("runs").join(workspace_basename).join("audits")
      }
      pub fn run_logs_dir(&self, workspace_basename: &str) -> PathBuf {
          self.logs_dir().join("runs").join(workspace_basename)
      }
      // ... add more as needed per the sweep
  }
  ```
- [x] 2.2 Each new helper returns the directory; callers append the per-entry filename themselves (avoids needing one helper per file).
- [x] 2.3 Tests: each helper returns the expected path for a fixture `DaemonPaths` instance.

## 3. Replace literals with helpers (the sweep fixes)

- [x] 3.1 For each Category A hit from §1.2, edit the file to call the appropriate helper. Pattern:
  ```rust
  // Before:
  let path = Path::new("/tmp/autocoder/audit-threads").join(format!("{ts}.json"));
  // After:
  let path = paths.audit_threads_dir().join(format!("{ts}.json"));
  ```
- [x] 3.2 Where the function doesn't yet receive `DaemonPaths` (or an equivalent), thread it through. Most code paths that need state files already have access to the daemon's path resolver via an existing struct (e.g., the polling loop has the daemon-paths reference in its context).
- [x] 3.3 Run `cargo test` after each batch of edits to confirm no regression. Smaller batches are safer than one big sweep.

## 4. CI-enforceable path-literals audit

- [x] 4.1 New test file `autocoder/tests/path_literals_audit.rs`:
  ```rust
  use std::fs;
  use std::path::Path;
  use walkdir::WalkDir;

  #[test]
  fn no_hardcoded_tmp_autocoder_literals_outside_allowlist() {
      let allowlist: &[&str] = &[
          "autocoder/src/state/migration.rs",  // legacy-path scan is the whole point
          // any other legitimate site
      ];
      let mut violations = Vec::new();
      for entry in WalkDir::new("autocoder/src") {
          let entry = entry.unwrap();
          let path = entry.path();
          if path.extension().map_or(true, |e| e != "rs") { continue; }
          let rel = path.strip_prefix("autocoder/").unwrap().to_str().unwrap();
          if allowlist.contains(&rel) { continue; }
          let contents = fs::read_to_string(path).unwrap();
          for (lineno, line) in contents.lines().enumerate() {
              if line.contains("/tmp/autocoder") {
                  violations.push(format!("{}:{}: {}", rel, lineno + 1, line.trim()));
              }
          }
      }
      assert!(violations.is_empty(),
              "Hard-coded /tmp/autocoder literals found outside allowlist:\n{}\n\nUse the DaemonPaths resolver instead.",
              violations.join("\n"));
  }
  ```
- [x] 4.2 The allowlist is intentionally narrow: only files that legitimately reference the legacy path for migration purposes.
- [x] 4.3 Run the test against the swept codebase; assert it passes. If it fails, the remaining hits are bugs the sweep missed.

## 5. Docs

- [x] 5.1 In `docs/STATE-LAYOUT.md`, add a section "Path resolution rule":
  - Every daemon-side state-file read or write SHALL route through the `DaemonPaths` resolver.
  - A CI test enforces "no hard-coded `/tmp/autocoder/` literals" outside an allowlist.
  - Future contributors who add new state-file shapes add a helper to `DaemonPaths` AND use it from the consumer side.
- [x] 5.2 In `docs/OPERATIONS.md`'s `## Busy marker` section, add a note: "If you're seeing operator-visible inconsistencies between writers and readers (`status` says idle while busy marker exists; `send it` returns `?` on a real thread), check `journalctl` AND the resolved paths; this class of bug is prevented by the path-literals CI test introduced in `a09`."

## 6. Spec deltas

- [x] 6.1 `openspec/changes/a09-state-path-resolution-consistent/specs/orchestrator-cli/spec.md` ADDs one requirement covering the resolver-only rule, the CI check, AND the expected consequence (read/write path consistency).
- [x] 6.2 `openspec/changes/a09-state-path-resolution-consistent/specs/project-documentation/spec.md` ADDs one requirement covering the STATE-LAYOUT.md section.

## 7. Verification

- [x] 7.1 `cargo test` passes (new + existing). The new path-literals audit test is part of `cargo test`.
- [x] 7.2 `openspec validate a12-state-path-resolution-consistent --strict` passes. (Note: change is `a09`, not `a12` — `openspec validate a09-state-path-resolution-consistent --strict` passes.)
- [x] 7.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings. (87 errors pre- and post-change; baseline is unchanged.)
- [ ] 7.4 Manual verification on a production-shaped deployment: `send it` on a fresh audit thread produces a PR (no `?` reaction). (NOT performed inside the autocoder sandbox — requires a live deployment with a configured Slack workspace and audit-thread state.)
