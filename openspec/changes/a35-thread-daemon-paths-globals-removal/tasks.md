# Tasks

## 1. paths.rs API surgery

- [ ] 1.1 Remove from `autocoder/src/paths.rs`:
  - The `OnceLock<DaemonPaths>` static.
  - `pub fn current()`.
  - `pub fn install_global(paths: DaemonPaths)`.
  - `pub fn install_global_for_tests(paths: DaemonPaths)`.
  - `pub fn test_fallback()`.
  - `pub fn get_global()`.
  - Any private helpers used only by the removed surface.
- [ ] 1.2 Retain in `autocoder/src/paths.rs`:
  - The `DaemonPaths` struct AND its `Clone` derive.
  - The env-driven constructor (e.g. `from_systemd_dirs` OR `resolve_from_env`).
  - All helper methods on `DaemonPaths` (`alert_state_path`, `audit_logs_dir`, `control_socket_path`, `workspaces_dir`, etc.).
- [ ] 1.3 Add a module-level `//!` doc-comment documenting the threading convention: `Arc<DaemonPaths>` is constructed once at daemon startup AND threaded explicitly via constructor fields OR function parameters. No process-global cell. Reference the canonical orchestrator-cli "Production paths SHALL be threaded" requirement.

## 2. Daemon entrypoint plumbing

- [ ] 2.1 In `autocoder/src/main.rs` (OR the equivalent entrypoint module — currently `cli/run.rs::run_daemon`), construct ONE `Arc<DaemonPaths>` at startup via the env-driven resolution.
- [ ] 2.2 Remove the existing `paths::install_global(...)` call site in `autocoder/src/cli/run.rs`.
- [ ] 2.3 Pass the `Arc<DaemonPaths>` to the top-level orchestrator constructor (OR equivalent — the struct that owns the polling tasks, chatops listener, AND control socket).
- [ ] 2.4 Unit-test (entrypoint-shaped): a synthetic daemon startup constructs the `Arc<DaemonPaths>` AND passes it to the top-level type. The test does NOT need to actually run the daemon — it just verifies the constructor signature accepts the value.

## 3. Per-module refactors

For each module, choose the appropriate pattern (constructor field for struct-shaped modules; function parameter for free-function modules):

- [ ] 3.1 `autocoder/src/revisions.rs` — function-parameter pattern. Update `process_one_pr` AND any helpers reading `paths::current()` to accept `paths: &DaemonPaths`. Update polling-loop callers.
- [ ] 3.2 `autocoder/src/alert_state.rs` — function-parameter pattern. Update `AlertState::load_or_default`, `AlertState::save`, AND any helpers.
- [ ] 3.3 `autocoder/src/workspace.rs` — function-parameter pattern. Update `resolve_path` AND the helper at line 217.
- [ ] 3.4 `autocoder/src/busy_marker.rs` — constructor-field pattern. Introduce a `BusyMarker { paths: Arc<DaemonPaths>, ... }` struct (OR equivalent) AND migrate the four call sites to methods.
- [ ] 3.5 `autocoder/src/failure_state.rs` — function-parameter pattern.
- [ ] 3.6 `autocoder/src/control_socket.rs` — constructor-field pattern. The control-socket handler struct gains a `paths: Arc<DaemonPaths>` field.
- [ ] 3.7 `autocoder/src/audits/mod.rs` — constructor-field pattern. The audit framework's top-level type gains the field.
- [ ] 3.8 `autocoder/src/audits/scheduler.rs` — constructor-field pattern. The scheduler struct gains the field.
- [ ] 3.9 `autocoder/src/audits/threads.rs` — function-parameter pattern.
- [ ] 3.10 `autocoder/src/proposal_requests.rs` — function-parameter pattern.
- [ ] 3.11 `autocoder/src/changelog_requests.rs` — function-parameter pattern.
- [ ] 3.12 `autocoder/src/executor/claude_cli.rs` — constructor-field pattern. The `ClaudeCliExecutor` struct gains the field.
- [ ] 3.13 Update every caller in `autocoder/src/` that previously didn't need to pass `DaemonPaths` AND now does. Cascade upward until the caller has access to the threaded value (typically because it's a method of a struct that holds one, OR because main.rs handed it down).

## 4. Test refactors

- [ ] 4.1 Every test that previously invoked production code now requiring a `DaemonPaths` argument SHALL construct one via `test_daemon_paths()` AND pass it explicitly.
- [ ] 4.2 The test-fixture invariant becomes: each test's writes land under its own tempdir, NOT a shared `<system-temp>/autocoder/...` location. Update any test that asserted against the shared location to assert against its tempdir-scoped path.
- [ ] 4.3 For tests deeply nested behind constructors (e.g. tests that construct `Daemon::new()`), thread the `Arc<DaemonPaths>` through the constructor.
- [ ] 4.4 Add one new test that verifies concurrent isolation: `std::thread::spawn` two threads that each construct DIFFERENT `DaemonPaths` via `test_daemon_paths()` AND invoke the same production function (e.g. `AlertState::load_or_default`). Assert: the two threads' writes land in DIFFERENT tempdirs (no cross-contamination). This pins the canonical "Concurrent tests do not collide on disk" scenario.

## 5. CI scanner activation

- [ ] 5.1 In `autocoder/tests/path_literals_audit.rs`, remove the `#[ignore]` attribute from `no_removed_paths_global_accessor_references_in_src`.
- [ ] 5.2 Verify the scanner passes (`cargo test no_removed_paths_global_accessor_references_in_src` exits 0).
- [ ] 5.3 (Sanity check) Inject a synthetic `paths::current()` reference into a scratch file under `autocoder/src/` AND verify the scanner FAILS the build. Revert the scratch reference before committing.

## 6. Validation

- [ ] 6.1 `cargo test` passes (the activated path-literals audit included).
- [ ] 6.2 `cargo clippy` produces no NEW warnings against the existing baseline.
- [ ] 6.3 `openspec validate a35-thread-daemon-paths-globals-removal --strict` passes.
- [ ] 6.4 Grep verifies: `grep -rn "paths::current\|paths::install_global\|paths::test_fallback\|paths::get_global" autocoder/src/` returns ZERO results.
