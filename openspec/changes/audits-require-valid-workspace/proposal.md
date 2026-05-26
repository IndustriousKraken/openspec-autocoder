## Why

A real-world incident exposed an upstream gap in the audit-scheduling code path: audits ran against a workspace that had no `.git/` directory, three audits in a row logged `git status --porcelain -uall failed: fatal: not a git repository`, and the next polling iteration's `ensure_initialized` then reported the same "exists but no `.git/`" condition. The audits had no business running in that state — but more importantly, **the audits writing to the workspace are the agent of corruption**.

The mechanism:

1. Operator (or an unrelated process) clears `/tmp/workspaces/<sanitized>/` via `rm -rf` (or autocoder's wipe-workspace command).
2. Before the next polling iteration's `ensure_initialized` can re-clone, an audit runs. Audits that generate proposals (`missing_tests_audit`, `security_bug_audit`, the LLM-driven family) call `fs::create_dir_all(<workspace>/openspec/changes/<new-slug>)` to write their output. `create_dir_all` recursively creates every missing parent — including the workspace root and `openspec/`, both of which now exist on disk.
3. The audit's own validation (`openspec validate --strict`) may then reject the proposal and delete the change directory. But the parent directories — workspace root and `openspec/` — remain.
4. Subsequent iterations of `ensure_initialized` see "directory exists, no `.git/`" and refuse to proceed. The daemon is now stuck.

The `workspace-self-heal-partial-clone` spec (separately drafted) adds auto-recovery for the stuck state once we're in it. This spec closes the upstream gap: prevent audits from ever creating the broken state in the first place. An audit operating against an invalid workspace is meaningless — it can't read the repo, can't read its own past state, can't produce a valid proposal. The right behaviour is to skip the audit entirely with a brief INFO log, the same way other operations skip when their preconditions don't hold.

This isn't just about the wipe-then-restart race. The same gap allows any post-restart audit storm (per `a01-audit-proposal-self-validation`'s motivating case — every audit's cadence fires on the first iteration after daemon restart because in-memory cadence state is empty) to do real damage if any repo's workspace happens to be missing at startup. Pre-fix, that race is silent corruption; post-fix, it's a quiet log entry.

## What Changes

**Each LLM-driven audit's entry point checks workspace validity before doing any work.** "Valid" means: the workspace directory exists AND it contains a `.git/` subdirectory. If the check fails, the audit immediately returns a new `AuditOutcome::WorkspaceUnavailable { audit_type, workspace_path, reason }` variant — no file IO, no LLM call, no state mutation.

The check is cheap: a single `Path::is_dir()` on `<workspace>/.git`. It runs at the top of every LLM-driven audit's main function, before any other work.

**The audit scheduler logs WorkspaceUnavailable at INFO and skips to the next audit.** Same iteration handling as a successful audit run with no findings — the scheduler proceeds to the next audit-type. The skipped audit's cadence-state is NOT updated (it didn't actually run; the next iteration's cadence check will re-evaluate and may try again if the workspace has become valid in the meantime).

**The polling iteration's audit-scheduler call is also gated on `ensure_initialized` success.** Belt-and-braces: if `ensure_initialized` returned Err, the iteration must not invoke the audit scheduler at all. Per-audit validity checks (above) catch the case where the workspace becomes invalid mid-iteration (rare but possible). The iteration-level gate catches the case where the workspace was invalid at iteration start.

**The non-LLM `architecture_brightline` audit is also gated.** Even though it doesn't write proposals (it's a pure-data file-line counter), running it against a missing workspace produces useless garbage (zero file counts) AND its file walker would call `fs::read_dir` on the workspace; if it ever happens to call `fs::create_dir_all` for any output path, it'd contribute to the broken-state creation. The gate is universal across all audit types.

**No chatops notification when an audit is skipped.** The skip is a quiet failure mode — the operator's signal of the underlying problem comes from the iteration-level workspace-init alert (which already exists). Adding another chatops message per skipped audit would just flood the channel.

## Impact

- **Affected specs:** `orchestrator-cli` — one ADDED requirement covering the per-audit workspace-validity check, the iteration-level gate, and the new `AuditOutcome::WorkspaceUnavailable` variant.
- **Affected code:**
  - `autocoder/src/audits/mod.rs` — add `pub fn workspace_is_valid(workspace: &Path) -> bool` returning `workspace.is_dir() && workspace.join(".git").is_dir()`. Add `AuditOutcome::WorkspaceUnavailable { audit_type: String, workspace_path: PathBuf, reason: String }` variant.
  - `autocoder/src/audits/{architecture_consultative,drift,specs_writing,brightline}.rs` — at the top of each audit's main function, after argument validation but before any file IO or LLM calls:
    ```rust
    if !workspace_is_valid(workspace) {
        let reason = if !workspace.exists() {
            "workspace directory does not exist".into()
        } else if !workspace.join(".git").is_dir() {
            "workspace exists but has no .git/ subdirectory".into()
        } else {
            "workspace failed validity check".into()
        };
        tracing::info!(
            audit_type = %audit_type,
            workspace = %workspace.display(),
            reason = %reason,
            "audit skipped: workspace not in a valid state"
        );
        return Ok(AuditOutcome::WorkspaceUnavailable { audit_type, workspace_path: workspace.to_path_buf(), reason });
    }
    ```
  - `autocoder/src/polling_loop.rs` — wherever the audit scheduler is invoked, gate the call on the iteration's `ensure_initialized` result. If init failed, do NOT call the scheduler; the iteration's failure is logged as today.
  - `autocoder/src/audits/scheduler.rs` — handle the new `WorkspaceUnavailable` outcome: log at INFO (not WARN; this is expected when the iteration's workspace init has already failed), do NOT update the audit's cadence-state file (skipped runs don't consume cadence; the next valid iteration's cadence check will re-evaluate).
  - Tests:
    - `workspace_is_valid` returns false for nonexistent path, true for a fixture path with `.git/` subdir, false for a fixture path without `.git/`.
    - Each audit's entry: invoking with a fixture path that doesn't exist returns `Ok(WorkspaceUnavailable { reason: "workspace directory does not exist" })` immediately with no file IO observable in the fixture filesystem (no `create_dir_all` artefacts left behind).
    - Each audit's entry: invoking with a fixture path that exists but has no `.git/` returns `Ok(WorkspaceUnavailable { reason: "workspace exists but has no .git/ subdirectory" })`, again with no `create_dir_all` artefacts.
    - Each audit's entry: invoking with a valid fixture workspace passes the check and proceeds to the existing audit logic (or its stub equivalent in test fixtures).
    - Scheduler test: `WorkspaceUnavailable` outcome logs at INFO, does NOT write to the audit's cadence-state file, the scheduler proceeds to the next audit-type.
    - Polling-loop integration test: iteration where `ensure_initialized` returns Err runs to completion without invoking the audit scheduler; assert no audit-related log lines appear in the test's captured trace for that iteration.

- **Operator-visible behavior:** broken-workspace situations no longer accumulate audit-created partial state. Operators wiping a workspace (via chatops or manual `rm -rf`) see a clean state on the next iteration: either `ensure_initialized` re-clones successfully OR the iteration fails with the real init error, but in neither case do audits run and leave broken-state artefacts behind. The audit storm after a daemon restart (per `a01-audit-proposal-self-validation`'s motivation) becomes structurally harmless even if the storm itself isn't fully solved.
- **Breaking:** no. Audits running against valid workspaces (the overwhelming common case) take the same code path as today. Audits skipped due to invalid workspace produce one INFO log line per skipped audit-type instead of garbage output (or worse, broken-state side-effects).
- **Acceptance:** `cargo test` passes (new + existing). A daemon iteration against a deliberately-missing workspace runs without leaving `<workspace>/openspec/` or any other audit-created directory behind; the iteration's failure log shows the workspace init error, the audit log shows one INFO `audit skipped: workspace not in a valid state` per scheduled audit, and the next iteration with a valid workspace (after the operator manually re-clones OR after `workspace-self-heal-partial-clone`'s auto-recovery kicks in) runs the audits normally.
