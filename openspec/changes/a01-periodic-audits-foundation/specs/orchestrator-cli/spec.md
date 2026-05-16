## ADDED Requirements

### Requirement: Periodic audit framework
autocoder SHALL include a periodic audit framework that runs registered audit tasks on per-audit cadences, persists last-run state per workspace, applies per-audit sandbox profiles, enforces post-hoc write restrictions, writes per-invocation logs, and integrates with the polling loop so any specs an audit creates are picked up by the same iteration's queue walk.

#### Scenario: Framework runs registered audits at startup-defined cadence
- **WHEN** a polling iteration completes its `recreate_branch` step
  AND BEFORE it calls `queue::list_pending`
- **THEN** the framework iterates registered audits in declaration
  order
- **AND** for each audit, checks `.audit-state.json` to determine
  whether the configured cadence has elapsed since the last run
- **AND** runs the audit only when due

#### Scenario: requires_head_change suppresses re-runs when HEAD unchanged
- **WHEN** an audit's `requires_head_change()` returns `true` AND
  the recorded `last_run_sha` for that audit equals the current
  `HEAD` SHA on the base branch
- **THEN** the framework skips the audit for this iteration even
  if the cadence interval has elapsed
- **AND** the next iteration after a HEAD change re-evaluates
  cadence and runs the audit if due

#### Scenario: requires_head_change false runs on cadence regardless of HEAD
- **WHEN** an audit's `requires_head_change()` returns `false` AND
  the cadence has elapsed since `last_run_at`
- **THEN** the framework runs the audit regardless of whether
  `HEAD` has changed
- **AND** this allows audits whose inputs are external (e.g.
  package registries, GitHub PR lists) to run periodically without
  depending on local code changes

#### Scenario: WritePolicy::None audit cannot modify the workspace
- **WHEN** an audit declares `WritePolicy::None` AND it runs
- **THEN** the audit's sandbox (when the audit uses the wrapped
  Claude CLI) allows only `Read`, `Glob`, `Grep`, `Bash` —
  `Write` and `Edit` are denied at the tool layer
- **AND** after the audit returns, the framework runs
  `git status --porcelain` and asserts the workspace is clean
- **AND** if either the sandbox blocks a write attempt OR the
  post-hoc diff is non-empty, the audit is treated as failed:
  state is NOT updated (so cadence triggers a re-run next iteration),
  a chatops alert is posted under a new audit-failure category,
  and the unexpected diff is reverted via `git reset --hard HEAD`

#### Scenario: WritePolicy::OpenSpecOnly audit may only write under openspec/changes/
- **WHEN** an audit declares `WritePolicy::OpenSpecOnly` AND
  it runs
- **THEN** the audit's sandbox allows `Write` and `Edit`
- **AND** after the audit returns, the framework inspects
  `git status --porcelain` and asserts every modified or new path
  begins with `openspec/changes/`
- **AND** if any path outside that prefix is touched, the audit
  is treated as failed: state is NOT updated, chatops alert is
  posted, the entire workspace diff is reverted via
  `git reset --hard HEAD` + `git clean -fd`

#### Scenario: Audit-run log written per invocation
- **WHEN** an audit runs (regardless of outcome)
- **THEN** autocoder writes a timestamped log at
  `/tmp/autocoder/logs/<workspace-basename>/audits/<audit_type>-<UTC-RFC3339-with-Z>.log`
  containing: the audit type, the workspace path, the start and
  end timestamps, the resolved cadence + last-run info, the prompt
  used (for LLM audits), the raw audit output, and the final
  `AuditOutcome` variant
- **AND** the log directory is created if absent

#### Scenario: AuditOutcome::Reported posts to chatops
- **WHEN** an audit returns `AuditOutcome::Reported(findings)` AND
  chatops is configured
- **THEN** autocoder posts a single chatops message with a header
  line `📋 <repo>: <audit_type> — <N> finding(s)` followed by a
  bullet list of finding subjects (each truncated to the
  per-finding excerpt limit, default 200 chars)
- **AND** the full body of each finding is preserved in the
  audit-run log

#### Scenario: AuditOutcome::Reported with no findings posts a brief OK
- **WHEN** an audit returns `AuditOutcome::Reported(vec![])` AND
  chatops is configured AND the operator has set
  `audits.<audit_type>.notify_on_clean: true` (default `false`)
- **THEN** autocoder posts `✅ <repo>: <audit_type> — no findings`
- **AND** when `notify_on_clean` is unset or `false`, no chatops
  post is made for an empty-findings outcome (silence is success)

#### Scenario: AuditOutcome::SpecsWritten records the change names
- **WHEN** an audit returns `AuditOutcome::SpecsWritten(names)`
  with non-empty `names`
- **THEN** the framework logs an info line naming each created
  change AND the iteration proceeds to `list_pending` which now
  observes those entries as pending
- **AND** no chatops post is made by the framework itself for
  spec-writing audits — the existing start-of-work +
  PR-opened notifications cover the subsequent flow

#### Scenario: State persists across daemon restarts
- **WHEN** the daemon stops AND restarts later
- **THEN** the framework reads `<workspace>/.audit-state.json` at
  startup AND resumes the existing cadence
- **AND** an audit due during the daemon's downtime runs on the
  first qualifying iteration after restart
- **AND** if `.audit-state.json` is missing or unparseable, the
  framework treats it as "no audits have ever run" — every audit
  is eligible on its next due iteration

#### Scenario: Audit failure does not abort the iteration
- **WHEN** an audit's `run()` returns `Err`
- **THEN** the framework logs the error at ERROR level naming the
  audit type and excerpt
- **AND** `.audit-state.json` is NOT updated for that audit (so
  the cadence will re-trigger it next iteration)
- **AND** the iteration continues to `list_pending` and the rest
  of the normal flow; other audits in the registry still run

### Requirement: Audit cadence config schema
autocoder SHALL accept an optional top-level `audits:` block with `defaults:` (global) and per-repository `audits:` overrides. Each entry maps an audit type name to a `Cadence`. The `Cadence` enum SHALL accept the literal strings `disabled`, `daily`, `every-N-days` (where `N` is a positive integer), `weekly`, `monthly`, `quarterly`. Every audit defaults to `disabled` when unset in both global defaults and per-repo overrides.

#### Scenario: Per-repo cadence overrides global default
- **WHEN** `audits.defaults.architecture_brightline: weekly` AND a
  repository sets `audits.architecture_brightline: every-3-days`
- **THEN** the effective cadence for that repository is
  `every-3-days`

#### Scenario: Audit absent from both global and per-repo is disabled
- **WHEN** the operator's config has no entry for an audit type
  in either `audits.defaults` or any `repositories[].audits`
- **THEN** the audit's effective cadence is `disabled` AND the
  framework never invokes it

#### Scenario: every-N-days requires a positive integer
- **WHEN** a config entry uses `every-N-days` where N is `0` OR
  negative OR non-integer
- **THEN** config load fails at startup with an error naming the
  offending field path AND the parsed value

#### Scenario: Unknown audit type names fail config load
- **WHEN** a config entry under `audits.defaults` or
  `audits` (per-repo) uses a name that does not match a
  registered audit type
- **THEN** config load fails at startup with an error naming
  the field path AND the unknown audit type AND listing the
  known audit type names
- **AND** the daemon does NOT start

### Requirement: Architecture-brightline audit
autocoder SHALL ship an `architecture-brightline` audit in the periodic audit framework. The audit is pure-code (no LLM invocation), `requires_head_change = true`, and `WritePolicy::None`. It SHALL produce `AuditOutcome::Reported(findings)` containing structural metrics that exceed configured (or default) thresholds.

#### Scenario: Reports files exceeding the size threshold
- **WHEN** the audit runs AND a tracked file under the
  repository's source root has more lines than the threshold
  (default `800`)
- **THEN** a finding of severity `medium` is included with
  `subject = "file <path> is <N> lines (threshold: <T>)"` AND
  `anchor = Some("<path>:1")`

#### Scenario: Reports identical function signatures across files
- **WHEN** the audit detects two or more functions with
  identical name + parameter list signatures in different files
  (excluding `mod tests {}` blocks)
- **THEN** a finding of severity `low` lists each occurrence

#### Scenario: Reports dead public items
- **WHEN** the audit (or a static-analysis subprocess it invokes)
  identifies public items with zero references in the
  repository
- **THEN** a finding of severity `low` lists the items

#### Scenario: No findings produces silent outcome
- **WHEN** no metric exceeds its threshold
- **THEN** the audit returns `AuditOutcome::Reported(vec![])`
- **AND** unless `notify_on_clean: true` is set, no chatops
  message is posted (per the framework-level scenario above)
