## Why

`run_pass_through_commits` has a defensive dirty-workspace check (`polling_loop.rs:533-542`): if `git status --porcelain` is non-empty at the start of a pass, the function returns `Err(anyhow!("workspace ... is dirty before pass; refusing to proceed: ..."))`. The polling loop's caller logs this at ERROR and moves on. **No chatops alert fires.** A daemon stuck in this state loops forever in silence.

The other three predictable failure sites in `run_pass_through_commits` (workspace init, branch push, PR creation) all route through `handle_predictable_failure`, which emits a throttled chatops post via `AlertCategory::{WorkspaceInitFailure, BranchPushFailure, PrCreationFailure}`. The mid-iteration dirty-workspace path was overlooked when that alert system was wired up.

The companion `commit-trailing-archive` change addresses the root cause that was making this state arise. This change is a defense-in-depth observability fix: if any future bug, operator intervention, or external process produces a mid-iteration dirty workspace, the operator hears about it instead of the daemon spinning silently.

## What Changes

- **MODIFIED capability:** `orchestrator-cli`'s "Iteration-level error tolerance" requirement. A mid-iteration dirty-workspace error SHALL emit a throttled chatops alert (via the existing `AlertCategory` + `handle_predictable_failure` mechanism) in addition to the existing log line.
- **Code:** Add `AlertCategory::WorkspaceDirtyMidIteration` to `autocoder/src/alert_state.rs`. Update the dirty-workspace branch in `run_pass_through_commits` to call `handle_predictable_failure` before returning `Err`, mirroring the pattern used by `WorkspaceInitFailure`/`BranchPushFailure`/`PrCreationFailure`.
- **Throttle:** uses the existing per-category 24-hour throttle baked into `handle_predictable_failure`. No new throttle configuration.

## Impact

- Affected specs: `orchestrator-cli` (one MODIFIED requirement, "Iteration-level error tolerance").
- Affected code: `autocoder/src/alert_state.rs` (one new enum variant + label), `autocoder/src/polling_loop.rs::run_pass_through_commits` (~6 lines: wrap the dirty Err in handle_predictable_failure).
- Behavior change: when a workspace is dirty mid-iteration (which `commit-trailing-archive` should make impossible, but defense-in-depth), operators receive exactly one chatops alert per 24 hours per repo until they intervene. No effect on production behavior in the common case.
- Breaking: no. Adds a new alert category; existing categories unchanged.
