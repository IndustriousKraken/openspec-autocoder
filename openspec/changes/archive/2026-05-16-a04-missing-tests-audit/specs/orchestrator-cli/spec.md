## ADDED Requirements

### Requirement: Missing-tests audit
autocoder SHALL register a `missing_tests_audit` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with an OpenSpec-only sandbox and a missing-tests prompt; it creates new OpenSpec change directories under `openspec/changes/`, commits them to the agent branch, and returns the created change names so the same iteration's queue walk implements them. The audit is `requires_head_change = true` and `WritePolicy::OpenSpecOnly`.

#### Scenario: Invokes the CLI with an OpenSpec-only sandbox
- **WHEN** the audit runs
- **THEN** autocoder spawns the configured `executor.command` with
  a sandbox whose `allowed_tools` includes `Write` and `Edit`
  alongside the read tools
- **AND** the prompt is the embedded
  `prompts/missing-tests-audit.md` template OR the
  operator-supplied override at
  `audits.missing_tests_audit.prompt_path`

#### Scenario: Prompt instructs additive-only output
- **WHEN** the prompt is loaded
- **THEN** the prompt explicitly states:
  - "Do NOT propose deleting existing tests."
  - "Do NOT propose modifying existing tests unless they are
    factually broken (failing or unreachable). When in doubt,
    leave the existing test alone and propose a NEW test."
  - "Suppress trivial gaps: getters, setters, single-line
    constructors, `Default` impls, `From`/`Into` conversions
    with no behavior."
- **AND** the prompt directs the agent to focus on uncovered
  error paths, edge cases, and branches without assertions

#### Scenario: Audit creates new OpenSpec changes
- **WHEN** the audit identifies N coverage gaps (where N is
  capped by `audits.missing_tests_audit.max_proposals_per_run`,
  default `2`)
- **THEN** the audit creates N change directories at
  `openspec/changes/<change_name>/` where each contains a
  proposal.md, tasks.md, and (when the gap implies a capability
  invariant) a `specs/<capability>/spec.md` delta
- **AND** each created change is named with a `tests-` prefix
  (e.g. `tests-error-paths-in-queue-engine`) so operators can
  recognize audit-produced changes at a glance

#### Scenario: Audit commits created changes to agent branch
- **WHEN** the agent finishes creating files
- **THEN** the audit framework's WritePolicy::OpenSpecOnly check
  passes (every modified path is under `openspec/changes/`)
- **AND** the audit runs `git add openspec/changes/ && git commit
  -m "audit: missing-tests proposals (N change(s))"`
- **AND** the audit returns
  `AuditOutcome::SpecsWritten(change_names)` where
  `change_names` is the list of newly-created change directory
  names

#### Scenario: Same iteration's queue walk picks up created changes
- **WHEN** the audit returns `SpecsWritten(names)` AND the
  iteration proceeds to `list_pending`
- **THEN** `list_pending` observes the new directories (they have
  `proposal.md`, no `.in-progress`, no `.question.json`)
- **AND** the iteration's `walk_queue` includes them in its
  archive cap, ordered by their `proposal.md` mtime
  (per the existing time-based ordering)

#### Scenario: Cap on proposals per run
- **WHEN** the prompt would produce more than
  `max_proposals_per_run` changes
- **THEN** the prompt instructs the agent to pick the N highest-
  priority gaps (by severity / risk) and emit only those
- **AND** the agent does NOT create more than N changes in this
  run; remaining gaps will be re-surfaced on subsequent runs as
  the audit re-evaluates the codebase

#### Scenario: Write outside openspec/changes triggers framework revert
- **WHEN** the agent writes a file outside `openspec/changes/`
  (e.g. a `src/foo.rs` modification or a `README.md` edit)
- **THEN** the foundation's `WritePolicy::OpenSpecOnly` post-hoc
  check fails AND the framework reverts via `git reset --hard
  HEAD + git clean -fd`
- **AND** the audit is treated as failed (state NOT updated,
  chatops alert posted, audit re-runs next iteration)
- **AND** no OpenSpec changes are committed from this run

#### Scenario: Empty findings produce no spec changes and no chatops post
- **WHEN** the audit identifies zero meaningful coverage gaps
- **THEN** the audit returns `AuditOutcome::SpecsWritten(vec![])`
- **AND** no commit is made, no chatops post is sent (per
  framework behavior for spec-writing audits)
