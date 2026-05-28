## Why

Two operator-visible bugs both trace to the same root cause: the daemon has multiple code paths that read OR write daemon state files using DIFFERENT path-resolution logic. The bugs:

1. **`send it` returns `?` reaction** for real audit threads. Investigation showed that audit-thread state files are stamped at `<state_dir>/audit-threads/` (the migrated/correct path) but the lookup path reads from `/tmp/autocoder/audit-threads/` (the legacy path), finding only test fixtures and no real entries.
2. **`@<bot> status` reports `idle`** while the busy marker is held by the daemon. Same class: the status reader uses a different path than the marker writer, OR the marker is at a path that doesn't match the operator's expected location.

Both bugs are symptoms of the same defect class: **hard-coded `/tmp/autocoder/...` literals scattered across the codebase that bypass the daemon's path resolver.** Every place that reads or writes a state file SHOULD route through the resolved `state_dir` / `runtime_dir` / `cache_dir` / `logs_dir` from `DaemonPathsConfig`. When some sites hard-code `/tmp/autocoder/...` and others use the resolver, the two paths drift apart after the legacy-to-standard migration AND operators see "read finds nothing, write succeeded" symptoms.

The fix is a codebase sweep: every reference to a literal `/tmp/autocoder/` prefix becomes a call through the resolver. A CI check enforces the rule going forward.

## What Changes

**Audit-side sweep.** A grep for `/tmp/autocoder` across `autocoder/src/` turns up every hard-coded literal. Each becomes a call through the daemon's `DaemonPaths` resolver (the existing struct that gives `state_dir()`, `cache_dir()`, `logs_dir()`, `runtime_dir()`). Specifically affected surfaces (non-exhaustive list — the sweep produces the actual list):

- Audit-thread state lookup (the `send it` bug — reads `/tmp/autocoder/audit-threads/` instead of `<state_dir>/audit-threads/`).
- Busy-marker reader paths (the `status idle` bug — reads `/tmp/autocoder/busy/` or similar instead of `<runtime_dir>/busy/`).
- Audit-state reader paths (per-workspace `.audit-state.json` lookups; verify they all go through the workspace-resolution helper, not assumed paths).
- Failure-state reader/writer paths.
- Revision-state reader/writer paths.
- Per-change run log paths (mentioned in the executor and the PR-body composer).
- Audit-run log paths (the audit-thread lookup's secondary input).
- Alert-throttle state files (`.alert-state.json`).
- Audit-thread state files (`.json` files keyed by Slack `thread_ts`).
- Proposal-request state files (the `propose` verb's state, parallel to audit-thread).
- Changelog-request state files (when `a06` lands — but it's stacked above `a09`).

**Fix shape: a single `DaemonPaths` helper.** The struct already exists (per the state-paths-out-of-tmp spec); the fix is ensuring every reader AND writer routes through it instead of constructing paths directly. Where helper functions don't yet exist for a specific state-file shape, add them (e.g., `paths.audit_threads_dir()`, `paths.busy_markers_dir()`).

**CI-enforceable check.** A new `cargo test` test scans `autocoder/src/**/*.rs` for the literal substring `/tmp/autocoder` outside the resolver itself (and outside test helpers that need to reference the legacy path for migration tests). Any hit outside the allowlist fails the test with a pointer at the offending file:line AND the `DaemonPaths` helper the operator should use instead.

**Migration is unchanged.** This spec is about read/write path consistency, NOT about adding new categories to the migration spec. If the operator's audit-thread state files are at `/tmp/autocoder/audit-threads/` because the migration didn't sweep them, that's a separate fix (a sibling migration-completeness spec could land it). What `a09` does is ensure that ONCE the migration runs (or wouldn't have anything to migrate on a fresh install), every reader looks at the right place.

**Operator-visible side effects.**
- `send it` starts working on real audit threads (no longer reads `/tmp` and finds only test fixtures).
- `@<bot> status` correctly reports `running audit X` / `working on <change>` instead of `idle` when the daemon is busy.
- Any other diagnostic bug rooted in read/write path drift gets fixed at the same time.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one ADDED requirement: `All daemon state-file reads and writes route through the DaemonPaths resolver; a CI check enforces no hard-coded path literals`.
  - `project-documentation` — one ADDED requirement: `OPERATIONS.md or STATE-LAYOUT.md documents the resolver-only rule and the CI check`.
- **Affected code:**
  - The sweep: every `*.rs` file under `autocoder/src/` that constructs a state-file path. Concrete grep:
    ```bash
    grep -rn '/tmp/autocoder' autocoder/src/
    ```
    Each hit is either replaced with a `DaemonPaths` call OR moved to an allowlist (if the literal is the LEGACY path being referenced for migration purposes).
  - `autocoder/src/paths.rs` (or wherever `DaemonPaths` lives) — extend with helper methods for any state-file shape that doesn't have one today:
    ```rust
    impl DaemonPaths {
        pub fn audit_threads_dir(&self) -> PathBuf { self.state_dir().join("audit-threads") }
        pub fn busy_markers_dir(&self) -> PathBuf { self.runtime_dir().join("busy") }
        pub fn proposal_requests_dir(&self) -> PathBuf { self.state_dir().join("proposal-requests") }
        // ... etc.
    }
    ```
  - `autocoder/tests/path_literals_audit.rs` (new) — the CI test that greps the source tree and fails on hits outside the allowlist.
  - `docs/STATE-LAYOUT.md` — add a section "Path resolution rule" describing the constraint AND the CI check.
- **Operator-visible behavior:**
  - `send it` starts producing PRs from real audit threads instead of `?` reactions.
  - `@<bot> status` correctly identifies in-flight work.
  - Future contributors (human or LLM) can't accidentally introduce the same defect class — the CI check fails on hard-coded literals.
- **Breaking:** no. Existing on-disk state files are at the resolved path already (per the migration spec); the fix just makes the readers consistent.
- **Acceptance:** `cargo test` passes (new + existing). The new path-literals audit test passes against the swept codebase. `openspec validate a12-state-path-resolution-consistent --strict` passes. Manual verification: `send it` on a fresh audit thread produces a PR.
