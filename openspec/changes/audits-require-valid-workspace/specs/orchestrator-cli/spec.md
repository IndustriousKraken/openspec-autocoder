## ADDED Requirements

### Requirement: Audits do not run against an invalid workspace
Every audit (LLM-driven and pure-data) SHALL verify the workspace is valid before performing any file IO or LLM-call setup. "Valid" means the workspace directory exists AND it contains a `.git/` subdirectory. When the check fails, the audit SHALL return `Ok(AuditOutcome::WorkspaceUnavailable { audit_type, workspace_path, reason })` immediately AND SHALL log a single INFO line naming the audit, the workspace path, and the reason. No file IO, no LLM call, no state mutation, and crucially no `fs::create_dir_all` (which would create the workspace's parent directories without a clone, producing exactly the broken state the gate exists to prevent).

#### Scenario: Audit skipped when workspace directory does not exist
- **WHEN** an audit is invoked AND the workspace directory does not exist on disk
- **THEN** the audit returns `Ok(AuditOutcome::WorkspaceUnavailable { reason: "workspace directory does not exist", .. })`
- **AND** no `fs::create_dir_all` was called against the workspace path
- **AND** the workspace path still does not exist after the call returns
- **AND** an INFO log fires naming the audit, the workspace, and the reason

#### Scenario: Audit skipped when workspace exists but has no .git/
- **WHEN** an audit is invoked AND the workspace directory exists AND it contains no `.git/` subdirectory
- **THEN** the audit returns `Ok(AuditOutcome::WorkspaceUnavailable { reason: "workspace exists but has no .git/ subdirectory", .. })`
- **AND** no new files or subdirectories were created in the workspace as a side effect of the audit call
- **AND** an INFO log fires naming the audit, the workspace, and the reason

#### Scenario: Audit proceeds normally against a valid workspace
- **WHEN** an audit is invoked AND the workspace exists AND it contains a `.git/` subdirectory
- **THEN** the workspace-validity gate passes
- **AND** the audit proceeds to its normal logic (LLM call, file IO, etc.)
- **AND** no `WorkspaceUnavailable` outcome is returned

### Requirement: Polling iteration gates audit-scheduler invocation on workspace-init success
The polling iteration SHALL invoke the audit scheduler only when its `ensure_initialized` call returned Ok. When `ensure_initialized` returns Err, the iteration SHALL skip the audit scheduler entirely AND proceed to its own existing failure path. The iteration-level gate is belt-and-braces with the per-audit gate: per-audit catches mid-iteration corruption; iteration-level catches the case where the workspace was already broken at iteration start.

#### Scenario: ensure_initialized failure skips the audit scheduler
- **WHEN** a polling iteration calls `ensure_initialized` AND it returns Err
- **THEN** the audit scheduler is NOT invoked in that iteration
- **AND** the iteration logs its failure as today (the workspace-init alert path) without any audit-related log lines for that iteration

#### Scenario: ensure_initialized success invokes the audit scheduler normally
- **WHEN** a polling iteration calls `ensure_initialized` AND it returns Ok
- **THEN** the audit scheduler is invoked as today
- **AND** each scheduled audit's per-audit gate runs (and almost always passes — `ensure_initialized` Ok means the workspace is valid)

### Requirement: Skipped audits do not consume cadence or trigger chatops notifications
A `WorkspaceUnavailable` outcome SHALL NOT update the audit's cadence-state file. The next iteration's cadence check re-evaluates and may attempt the audit again if the workspace has become valid (e.g. via `workspace-self-heal-partial-clone`'s auto-recovery or an operator's manual fix). Additionally, no chatops notification SHALL fire for a skipped audit — the iteration's own workspace-init alert is the operator-facing signal of the upstream problem; per-audit skip notifications would just flood the channel.

#### Scenario: Skipped audit's cadence state is unchanged
- **WHEN** an audit returns `WorkspaceUnavailable` AND its cadence-state file at `<state_dir>/audit-state/<audit-type>.json` previously recorded `last_run: <30 days ago>`
- **THEN** after the audit returns, the cadence-state file's `last_run` is still `<30 days ago>` (unchanged)
- **AND** the next polling iteration's cadence check sees the unchanged timestamp AND treats the audit as still-due

#### Scenario: No chatops notification on workspace-unavailable skip
- **WHEN** an audit returns `WorkspaceUnavailable` AND the chatops backend is configured AND the audit's `notify_on_clean` is `true`
- **THEN** no chatops `post_notification` call fires for the skipped audit
- **AND** the operator's signal of the underlying issue remains the iteration-level `workspace_init_failure` alert (which fires independently per existing behaviour)

#### Scenario: Multiple audits skipped in the same iteration produce no notification flood
- **WHEN** an iteration runs against an invalid workspace AND every scheduled audit returns `WorkspaceUnavailable`
- **THEN** zero chatops notifications fire for those skips
- **AND** the daemon logs one INFO line per skipped audit (operator can `journalctl` to see exactly which audits were skipped)
