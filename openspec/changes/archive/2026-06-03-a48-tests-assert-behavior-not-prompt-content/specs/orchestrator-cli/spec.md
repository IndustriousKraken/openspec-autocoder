# orchestrator-cli — delta for a48-tests-assert-behavior-not-prompt-content

## MODIFIED Requirements

### Requirement: Security & bug audit
autocoder SHALL register a `security_bug_audit` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with an OpenSpec-only sandbox and a security-and-bug-detection prompt; it creates new OpenSpec change directories under `openspec/changes/` describing proposed fixes, commits them, and returns the change names so the same iteration implements them. The audit is `requires_head_change = true` and `WritePolicy::OpenSpecOnly`.

The prompt's confidence-filtering and scope guidance below is design intent verified by the drift audit's semantic judgment; it SHALL NOT be pinned by a unit test asserting verbatim substrings of the prompt (per the project-documentation requirement `Tests assert behavior or derivation, never message wording`).

#### Scenario: Prompt steers the agent toward high-confidence, in-scope findings
- **WHEN** the security-bug audit prompt is loaded
- **THEN** it instructs the agent to report only findings it is
  reasonably confident about and to err toward NOT reporting when
  uncertain, because a false positive becomes wasted implementer
  work downstream
- **AND** it instructs the agent not to propose stylistic
  "best-practice" changes that do not address a concrete security
  issue or bug
- **AND** it scopes findings to concrete in-scope categories
  (injection, auth/authz mistakes, hard-coded secrets, unsafe
  deserialization, missing input validation at trust boundaries,
  race conditions, resource leaks, off-by-one, wrong operator,
  mishandled None/null, missing error propagation) and excludes
  out-of-scope categories (code style, naming, architectural
  opinions, performance unless measurable, anything the project
  has explicitly accepted)

#### Scenario: Created changes use fix- or secure- prefix
- **WHEN** the audit creates a change for a proposed fix
- **THEN** the change directory name uses `fix-` prefix for bug
  fixes (e.g. `fix-off-by-one-in-queue-walker`) AND `secure-`
  prefix for security hardening (e.g.
  `secure-sanitize-user-paths`)
- **AND** the operator can recognize audit-produced security/bug
  changes by their prefix at a glance

#### Scenario: Each proposed change includes a fix specification
- **WHEN** the audit creates a change
- **THEN** the change SHALL contain:
  - `proposal.md` naming the issue, citing the source location,
    and explaining the fix.
  - `tasks.md` listing the implementation steps.
  - When the fix implies a capability invariant (e.g. "every
    operation X SHALL validate Y"), a `specs/<capability>/spec.md`
    delta MODIFYING the relevant requirement OR adding a new
    requirement.
- **AND** validation via `openspec validate <name> --strict`
  passes before the audit commits the change

#### Scenario: Validation failure rejects the change without committing
- **WHEN** the agent produces a change that fails `openspec
  validate --strict`
- **THEN** the audit deletes the offending change directory AND
  records a WARN log entry naming the validation error
- **AND** the audit does NOT chatops-alert per-change validation
  failures (the audit-run log is sufficient operator signal)
- **AND** if every proposed change fails validation, the audit
  returns `AuditOutcome::SpecsWritten(vec![])` and no commit
  is made

#### Scenario: Per-run proposal cap
- **WHEN** the agent would produce more than
  `max_proposals_per_run` (default `2`) changes
- **THEN** the prompt instructs the agent to pick the
  highest-severity issues and emit only those
- **AND** the cap is enforced post-hoc: if the agent produces
  more, the audit keeps the first N (in directory-listing order
  after the post-run snapshot) and deletes the rest with a WARN
  log

#### Scenario: Write outside openspec/changes triggers framework revert
- **WHEN** the agent writes a file outside `openspec/changes/`
  (attempts to fix the bug directly, edits a source file, etc.)
- **THEN** the foundation's `WritePolicy::OpenSpecOnly` post-hoc
  check fails AND the framework reverts via
  `git reset --hard HEAD + git clean -fd`
- **AND** the audit is treated as failed; chatops alert posted;
  the audit re-runs next iteration

#### Scenario: Empty findings produce no spec changes and no chatops post
- **WHEN** the agent identifies zero confident security or bug
  issues
- **THEN** the audit returns `AuditOutcome::SpecsWritten(vec![])`
- **AND** no commit, no chatops post, the iteration proceeds
  normally
