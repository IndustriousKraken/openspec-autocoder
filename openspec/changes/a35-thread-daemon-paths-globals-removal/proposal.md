## Why

`a27-thread-daemon-paths` archived on 2026-05-29 with the canonical `Production paths SHALL be threaded through APIs, NOT read from a process-global` requirement fully specified, including six scenarios covering the daemon entrypoint, per-module constructor pattern, free-function pattern, test isolation, AND the CI scanner. The §5.1 CI scanner (extension of the a10 path-literals audit) landed but is `#[ignore]`'d with the comment `"enable once a27 removes all paths::current()/install_global()/test_fallback()/get_global() call sites"`. §1.x (paths.rs API surgery), §2.x (daemon entrypoint plumbing), §3.x (per-module refactors across 11 modules), AND §4.x (test refactors) did NOT land. Today's production code still uses `paths::current()` in 13 source files:

```
autocoder/src/revisions.rs
autocoder/src/alert_state.rs
autocoder/src/workspace.rs
autocoder/src/busy_marker.rs
autocoder/src/failure_state.rs
autocoder/src/control_socket.rs
autocoder/src/audits/mod.rs
autocoder/src/audits/scheduler.rs
autocoder/src/audits/threads.rs
autocoder/src/proposal_requests.rs
autocoder/src/changelog_requests.rs
autocoder/src/executor/claude_cli.rs
autocoder/src/cli/run.rs  (the install_global call site)
```

The canonical `Production paths SHALL be threaded through APIs` requirement is currently in violation across all 13 files. The CI scanner that would surface the violation is gated off. The implementation gap is silent.

This change closes the gap. It is implementation-completion work for an existing canonical requirement, NOT new behavioral specification. The spec deltas in this change ADD enforcement requirements that pin the implementation timeline AND the CI gate's activation, NOT new behavior. The existing canonical text remains the authoritative description of the threaded-paths model.

The work is mechanical AND large in line-count but small in conceptual scope:

- Remove the four forbidden functions AND the `OnceLock<DaemonPaths>` static from `paths.rs`.
- Construct one `Arc<DaemonPaths>` in `main.rs` at daemon startup.
- Thread it through the daemon's top-level orchestrator constructor.
- Refactor each of the 13 production files to accept `paths: &Arc<DaemonPaths>` (OR equivalent) on the public API surface.
- Refactor every caller to pass the threaded value.
- Update each test that previously relied on the global to construct its own `test_daemon_paths()` AND pass it explicitly.
- Remove the `#[ignore]` from the path-literals audit's regression test.

The scope is bounded — the canonical's six scenarios provide concrete acceptance criteria — but the ripple is wide because EVERY caller of an affected module's public API also needs to thread the paths value. This is the work the prior implementer iterations bailed on as "multi-day refactor that cascades across 20 files." It is genuinely substantial, but it is NOT multi-day for a focused effort: the changes are mechanical AND the canonical patterns (constructor field, function parameter) are documented.

## What Changes

**Implementation-completion of the canonical `Production paths SHALL be threaded through APIs` requirement.** No new behavioral spec — the existing canonical text describes the end state AND its scenarios pin the acceptance bar. The work is mechanical:

1. **`paths.rs` API surgery.** Remove `current()`, `install_global()`, `install_global_for_tests()`, `test_fallback()`, `get_global()`, AND the `OnceLock<DaemonPaths>` static. Retain `DaemonPaths` struct, its helper methods, AND the env-driven constructor. Document the threading convention in a `//!` module-level doc-comment.

2. **`main.rs` entrypoint.** Construct one `Arc<DaemonPaths>` via the existing env-driven resolution at startup. Pass it to the top-level orchestrator constructor.

3. **Per-module refactors.** Each of the 13 production files SHALL be migrated to one of two patterns documented in the canonical:
   - **Constructor field pattern**: struct-shaped modules (e.g. `BusyMarker`, `AuditScheduler`) gain a `paths: Arc<DaemonPaths>` field, populated by the constructor.
   - **Function parameter pattern**: free-function modules (e.g. helpers in `audits/threads.rs`) gain a `paths: &DaemonPaths` parameter on every public function.

4. **Test refactors.** Every test that exercised production code now needing a `DaemonPaths` argument SHALL construct one via `test_daemon_paths()` AND pass it explicitly. Tests no longer share a `<system-temp>/autocoder/...` location.

5. **CI scanner activation.** Remove `#[ignore]` from `no_removed_paths_global_accessor_references_in_src` in `autocoder/tests/path_literals_audit.rs`. The test runs unconditionally going forward AND fails the build if any of the forbidden symbols reappears.

**New ADDED requirements** layered above the existing canonical:

- **Per-module migration enforcement.** A new requirement enumerates the 13 production files that SHALL be migrated AND SHALL NOT contain any of the forbidden symbols after this change lands. The canonical's CI scanner enforces this generically; this requirement makes the per-file scope auditable in the canonical record.
- **Path-literals scanner activation.** A new requirement pins the activation of the scanner: the `#[ignore]` SHALL be removed in this change AND SHALL NOT be reintroduced. (The canonical says the scanner exists; this requirement says it RUNS.)

## Impact

- **Affected specs:**
  - `orchestrator-cli` — ADDED requirements for: per-module migration enforcement (enumerating the 13 files), AND the path-literals scanner's permanent activation. The existing canonical `Production paths SHALL be threaded through APIs, NOT read from a process-global` requirement is unchanged in body; its six existing scenarios continue to govern the end-state behavior.
- **Affected code:**
  - `autocoder/src/paths.rs` — remove the forbidden symbols AND the `OnceLock` static. Retain the struct, helpers, env-driven constructor, AND add a module-level doc comment documenting the threading convention.
  - `autocoder/src/main.rs` (OR equivalent entrypoint module) — construct `Arc<DaemonPaths>` at startup; pass to top-level orchestrator.
  - `autocoder/src/revisions.rs` — migrate per the function-parameter pattern (lines 95, 105, 110 per current grep).
  - `autocoder/src/alert_state.rs` — migrate per the function-parameter pattern (lines 104, 116).
  - `autocoder/src/workspace.rs` — migrate per the function-parameter pattern (lines 20, 217).
  - `autocoder/src/busy_marker.rs` — migrate per the constructor-field pattern (4 call sites: 135, 151, 288, 1138).
  - `autocoder/src/failure_state.rs` — migrate per the function-parameter pattern (lines 48, 57).
  - `autocoder/src/control_socket.rs` — migrate per the constructor-field pattern (line 330).
  - `autocoder/src/audits/mod.rs` — migrate per the constructor-field pattern (line 307).
  - `autocoder/src/audits/scheduler.rs` — migrate per the constructor-field pattern (lines 1452, 1467).
  - `autocoder/src/audits/threads.rs` — migrate per the function-parameter pattern (line 85).
  - `autocoder/src/proposal_requests.rs` — migrate per the function-parameter pattern (line 110).
  - `autocoder/src/changelog_requests.rs` — migrate per the function-parameter pattern (line 91).
  - `autocoder/src/executor/claude_cli.rs` — migrate per the constructor-field pattern (line 1565).
  - `autocoder/src/cli/run.rs` — remove the `install_global` call site (the new daemon-entrypoint plumbing replaces it).
  - Tests across the affected modules' test cfgs — update to construct `test_daemon_paths()` AND pass explicitly.
  - `autocoder/tests/path_literals_audit.rs` — remove `#[ignore]` from `no_removed_paths_global_accessor_references_in_src`.
- **Operator-visible behavior:** ZERO. The threading model is an internal refactor. No config knobs. No new chatops notifications. No PR-comment format changes. `journalctl` output is unchanged. Performance characteristics are unchanged (one heap allocation for `Arc<DaemonPaths>` at startup, then ref-count clones per consumer).
- **Backward compatibility:** N/A — internal refactor with no operator-facing surface. State files, PR comments, chatops notifications, AND config schemas are unchanged.
- **Dependencies:** none. Independent of `a2705`, the `a27a*` outcome-tools stack, `a31`, `a33`, AND `a34`. Can land in any order.
- **Acceptance:** `cargo test` passes (the activated path-literals audit included); `cargo clippy` produces no NEW warnings; `openspec validate a35-thread-daemon-paths-globals-removal --strict` passes. Tests:
  - `path_literals_audit::no_removed_paths_global_accessor_references_in_src` is NOT `#[ignore]`-marked AND runs as part of `cargo test`.
  - The scanner passes (no forbidden symbol references in `autocoder/src/**/*.rs`).
  - Every existing test in the affected modules passes after the refactor.
  - One new test verifies that two concurrent production-function invocations via `std::thread::spawn` with DIFFERENT `DaemonPaths` values do NOT collide on disk (the per-test isolation invariant the canonical's "Concurrent tests do not collide on disk" scenario describes).
- **Implementation notes:**
  - The refactor is mechanical. The canonical's two patterns (constructor field, function parameter) AND its CI scanner enforcement provide an unambiguous target.
  - The scope is ~13 source files + their test cfgs + the ripple of every caller. Estimate: a focused-session-day of work for an experienced Rust engineer.
  - The implementer SHALL NOT split this into multiple smaller changes. Half-refactors leave the codebase in a state where some modules thread paths AND others read the global — the canonical scanner cannot be activated in that state, AND the implementation gap persists. This change ships fully runnable OR not at all.
