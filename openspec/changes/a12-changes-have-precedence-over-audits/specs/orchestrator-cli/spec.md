## MODIFIED Requirements

### Requirement: Periodic audit framework
autocoder SHALL include a periodic audit framework that runs registered audit tasks on per-audit cadences, persists last-run state per workspace, applies per-audit sandbox profiles, enforces post-hoc write restrictions, writes per-invocation logs, AND integrates with the polling loop. **The audit phase SHALL run AFTER the pending change queue walk completes, not before.** This change prevents an audit storm (e.g., 5 audits becoming eligible simultaneously after a HEAD change) from monopolizing the daemon for hours and blocking pending changes. Spec-writing audits' generated changes wait one iteration for implementation — the audit's creation commits ship in iteration N's PR; the implementer's commits for those generated changes ship in iteration N+1's PR.

#### Scenario: Framework runs registered audits after the pending queue walk
- **WHEN** a polling iteration completes its `recreate_branch` step
  AND completes `queue::list_waiting` AND `queue::list_pending`
- **AND** the iteration has remaining wall-clock budget AND has not been gated by an open PR
- **THEN** the framework iterates registered audits in declaration order
- **AND** for each audit, checks `.audit-state.json` to determine whether the configured cadence has elapsed AND `requires_head_change` is satisfied
- **AND** runs the audit only when due

#### Scenario: requires_head_change suppresses re-runs when HEAD unchanged
- **WHEN** an audit's `requires_head_change()` returns `true` AND the recorded `last_run_sha` for that audit equals the current `HEAD` SHA on the base branch
- **THEN** the framework skips the audit for this iteration even if the cadence interval has elapsed
- **AND** the next iteration after a HEAD change re-evaluates cadence and runs the audit if due

#### Scenario: requires_head_change false runs on cadence regardless of HEAD
- **WHEN** an audit's `requires_head_change()` returns `false` AND the cadence has elapsed since `last_run_at`
- **THEN** the framework runs the audit regardless of whether `HEAD` has changed
- **AND** this allows audits whose inputs are external (e.g. package registries, GitHub PR lists) to run periodically without depending on local code changes

#### Scenario: WritePolicy::None audit cannot modify the workspace
- **WHEN** an audit declares `WritePolicy::None` AND it runs
- **THEN** the audit's sandbox allows only `Read`, `Glob`, `Grep`, `Bash` — `Write` and `Edit` are denied at the tool layer
- **AND** after the audit returns, the framework runs `git status --porcelain` and asserts the workspace is clean
- **AND** if either the sandbox blocks a write attempt OR the post-hoc diff is non-empty, the audit is treated as failed: state is NOT updated, a chatops alert is posted, and the diff is reverted via `git reset --hard HEAD`

#### Scenario: WritePolicy::OpenSpecOnly audit may only write under openspec/changes/
- **WHEN** an audit declares `WritePolicy::OpenSpecOnly` AND it runs
- **THEN** the audit's sandbox allows `Write` and `Edit`
- **AND** after the audit returns, the framework inspects `git status --porcelain` and asserts every modified or new path begins with `openspec/changes/`
- **AND** if any path outside that prefix is touched, the audit is treated as failed: state is NOT updated, chatops alert is posted, the entire workspace diff is reverted

#### Scenario: Audit-run log written per invocation
- **WHEN** an audit runs (regardless of outcome)
- **THEN** autocoder writes a timestamped log at the resolved logs-dir path
- **AND** the log contains the audit type, workspace path, start AND end timestamps, resolved cadence + last-run info, the prompt used (for LLM audits), the raw audit output, AND the final `AuditOutcome` variant

#### Scenario: AuditOutcome::Reported posts to chatops
- **WHEN** an audit returns `AuditOutcome::Reported(findings)` AND chatops is configured
- **THEN** autocoder posts a single chatops message with a header line `📋 <repo>: <audit_type> — <N> finding(s)` followed by a bullet list of finding subjects

#### Scenario: AuditOutcome::SpecsWritten records the change names; implementation waits one iteration
- **WHEN** an audit returns `AuditOutcome::SpecsWritten(names)` with non-empty `names`
- **THEN** the framework logs an info line naming each created change
- **AND** the audit's creation commit (one commit titled `audit: <type> proposals (N change(s))`) is on the agent branch when the iteration's push+PR step runs
- **AND** the new changes are NOT processed by THIS iteration's queue walk (because the queue walk already completed before the audit ran)
- **AND** the new changes ARE picked up by the NEXT iteration's `queue::list_pending` for normal implementer processing
- **AND** the implementer's commits for those changes ship in iteration N+1's PR — separable from iteration N's PR which contains only the audit creation commits

#### Scenario: State persists across daemon restarts
- **WHEN** the daemon stops AND restarts later
- **THEN** the framework reads `<workspace>/.audit-state.json` at startup AND resumes the existing cadence
- **AND** an audit due during the daemon's downtime runs on the first qualifying iteration after restart

#### Scenario: Audit failure does not abort the iteration
- **WHEN** an audit's `run()` returns `Err`
- **THEN** the framework logs the error at ERROR level naming the audit type and excerpt
- **AND** `.audit-state.json` is NOT updated for that audit
- **AND** the iteration continues to the push+PR step normally — the audit failure is isolated to that audit; other audits AND the push step are unaffected

#### Scenario: Iteration with pending changes processes them before audits
- **WHEN** an iteration begins AND has 2 pending changes in the queue AND 1 audit eligible to run
- **THEN** the iteration first processes both pending changes via the implementer (commits + archives)
- **AND** THEN runs the eligible audit
- **AND** the push+PR step at iteration end includes commits from both phases
- **AND** an operator watching chatops sees `🚀 starting work on <change>` BEFORE any `🔍 created proposal` or `📋 audit findings` messages for that iteration

#### Scenario: Iteration with only audits processes them when no pending exist
- **WHEN** an iteration begins AND has 0 pending changes AND 1 audit eligible to run
- **THEN** the iteration runs the audit
- **AND** the push+PR step ships the audit's commits (if any)
- **AND** if the audit created new proposals, those become pending for next iteration's queue walk
