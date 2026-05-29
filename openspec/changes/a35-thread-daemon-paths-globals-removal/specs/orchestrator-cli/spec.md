## ADDED Requirements

### Requirement: Path-literals scanner SHALL be enabled unconditionally in CI

The path-literals audit test `no_removed_paths_global_accessor_references_in_src` in `autocoder/tests/path_literals_audit.rs` SHALL run unconditionally as part of `cargo test`. The `#[ignore]` attribute that was applied during the `a27-thread-daemon-paths` archive (with the comment `"enable once a27 removes all paths::current()/install_global()/test_fallback()/get_global() call sites"`) SHALL be removed in this change AND SHALL NOT be reintroduced.

The scanner enforces the canonical `Production paths SHALL be threaded through APIs, NOT read from a process-global` requirement's "CI scanner blocks reintroduction" scenario. Without active enforcement, the canonical's hard rule is unenforced — any future change can reintroduce a global path accessor without surfacing the regression.

The scanner SHALL match the literal substrings `paths::current`, `paths::install_global`, `paths::test_fallback`, AND `paths::get_global` against `autocoder/src/**/*.rs` source files. The scanner's own constants are constructed at runtime from fragments so it does NOT match itself (existing canonical behavior, preserved).

#### Scenario: Scanner runs unconditionally as part of `cargo test`
- **WHEN** a developer runs `cargo test` against the autocoder crate
- **THEN** `no_removed_paths_global_accessor_references_in_src` runs (NOT skipped via `#[ignore]`)
- **AND** the test passes IF the source tree contains no forbidden symbol references
- **AND** the test FAILS the build IF any forbidden symbol appears in any `autocoder/src/**/*.rs` file

#### Scenario: Scanner failure names the offending file AND symbol
- **WHEN** a synthetic `paths::current()` reference is inserted into `autocoder/src/some_module.rs`
- **AND** `cargo test no_removed_paths_global_accessor_references_in_src` runs
- **THEN** the test fails with a message naming `autocoder/src/some_module.rs` AND the symbol `paths::current`
- **AND** the failure message references the canonical `Production paths SHALL be threaded through APIs` requirement as the resolution

#### Scenario: Reintroduction of `#[ignore]` is forbidden
- **WHEN** a future change attempts to add `#[ignore = "..."]` back onto `no_removed_paths_global_accessor_references_in_src`
- **THEN** the change SHALL be rejected at review time (this requirement explicitly forbids the reintroduction)
- **AND** the canonical "CI scanner blocks reintroduction" scenario from the `Production paths SHALL be threaded` requirement remains active

### Requirement: Per-module migration of the production files that previously read paths globals

The 13 production files enumerated below SHALL be migrated to threaded `DaemonPaths` per the canonical "Production paths SHALL be threaded through APIs" requirement's two-pattern model (constructor-field pattern for struct-shaped modules; function-parameter pattern for free-function modules). After this change lands, EACH of the enumerated files SHALL be free of references to the four forbidden symbols.

The enumeration captures the per-file migration scope in the canonical record. The canonical's CI scanner enforces the invariant generically; this requirement makes the per-file scope auditable AND prevents accidental partial migration.

Files SHALL be migrated to the canonical pattern indicated:

| File | Pattern |
|------|---------|
| `autocoder/src/revisions.rs` | function-parameter |
| `autocoder/src/alert_state.rs` | function-parameter |
| `autocoder/src/workspace.rs` | function-parameter |
| `autocoder/src/busy_marker.rs` | constructor-field |
| `autocoder/src/failure_state.rs` | function-parameter |
| `autocoder/src/control_socket.rs` | constructor-field |
| `autocoder/src/audits/mod.rs` | constructor-field |
| `autocoder/src/audits/scheduler.rs` | constructor-field |
| `autocoder/src/audits/threads.rs` | function-parameter |
| `autocoder/src/proposal_requests.rs` | function-parameter |
| `autocoder/src/changelog_requests.rs` | function-parameter |
| `autocoder/src/executor/claude_cli.rs` | constructor-field |
| `autocoder/src/cli/run.rs` | entrypoint-construction (removes the `install_global` call site) |

The pattern assignment is a recommendation, NOT a binding rule — the implementer MAY choose a different pattern per file IF the chosen pattern still satisfies the canonical "Production paths SHALL be threaded" requirement's two scenarios (constructor-field OR function-parameter). The end state is what's binding: zero references to forbidden symbols in any of the enumerated files (AND in every other `autocoder/src/**/*.rs` file).

This change ships fully runnable. The implementer SHALL NOT split the migration across multiple changes: a half-migrated state where some modules thread paths AND others read the global cannot pass the CI scanner activation requirement above (because the scanner is unconditional). Partial migration is a non-shippable state.

#### Scenario: Every enumerated file is free of forbidden symbols after this change
- **WHEN** the build runs against the state of `autocoder/src/` after this change merges
- **THEN** grepping the source tree for `paths::current`, `paths::install_global`, `paths::test_fallback`, OR `paths::get_global` returns ZERO matches
- **AND** the path-literals audit (now unconditionally active per the requirement above) passes

#### Scenario: Daemon entrypoint constructs the single `Arc<DaemonPaths>` instance
- **WHEN** the daemon starts up after this change
- **THEN** the entrypoint module (`autocoder/src/main.rs` OR `autocoder/src/cli/run.rs::run_daemon`) constructs ONE `Arc<DaemonPaths>` via the env-driven resolution
- **AND** the value is passed to the top-level orchestrator constructor
- **AND** no other code path constructs a second `DaemonPaths` for production use
- **AND** no `install_global` call remains in any source file

#### Scenario: Concurrent test isolation invariant verified
- **WHEN** the test suite runs the new concurrent-isolation test (per task 4.4)
- **AND** two `std::thread::spawn`-spawned threads each invoke `AlertState::load_or_default` with DIFFERENT `Arc<DaemonPaths>` values (constructed via `test_daemon_paths()`)
- **THEN** each thread's write lands under its OWN tempdir
- **AND** neither thread can see the other thread's writes
- **AND** the test passes

#### Scenario: Implementation completion satisfies canonical's existing scenarios
- **WHEN** the build runs after this change
- **THEN** all six existing scenarios in the canonical `Production paths SHALL be threaded through APIs` requirement evaluate as TRUE against the actual code state:
  - "Daemon entrypoint constructs the single instance" — verified by §2.x tasks.
  - "Module constructor accepts paths" — verified by §3.x constructor-field tasks.
  - "Free function accepts paths as parameter" — verified by §3.x function-parameter tasks.
  - "Test constructs its own DaemonPaths" — verified by §4.x test-refactor tasks.
  - "Concurrent tests do not collide on disk" — verified by §4.4 concurrent-isolation test.
  - "CI scanner blocks reintroduction" — verified by §5.x scanner-activation tasks.
