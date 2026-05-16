## Why

Test coverage gaps tend to be invisible until they bite. An LLM auditing a codebase can identify code paths that lack meaningful tests — branches, error-handling paths, edge-case handlers — and propose tests for them. Done well, this turns a chronic maintenance task into a stream of small, reviewable, queue-driven changes.

Unlike the drift audit, this one writes new OpenSpec changes proposing the tests. The same iteration's `walk_queue` picks them up and the normal implementer flow drives them to a PR. One audit invocation can produce N changes; the per-PR cap (`max_changes_per_pr`) bounds how many ship per iteration.

The audit explicitly does NOT propose deleting existing tests or "fixing" tests it doesn't understand — that's a different category of work and high-risk. Additive only.

## What Changes

- **ADDED capability:** `orchestrator-cli` gains a "Missing-tests audit" requirement.
- **Audit:** registered as `missing_tests_audit`. `requires_head_change() = true`. `WritePolicy::OpenSpecOnly` (the audit is allowed to write under `openspec/changes/` but nowhere else; foundation post-hoc check enforces this).
- **Prompt:** embedded default at `prompts/missing-tests-audit.md`. Operator overridable via `audits.missing_tests_audit.prompt_path`. The prompt instructs the LLM to:
  - Survey the source tree, identifying functions/methods with no tests AND functions whose tests don't exercise their error/edge paths.
  - For each meaningful gap (suppress trivial getters, suppress experimental modules), produce one OpenSpec change with:
    - A proposal naming the gap.
    - A spec delta — typically a MODIFIED requirement on the relevant capability adding a scenario describing the test invariant.
    - A tasks.md listing the test functions to add (test names + assertions to make).
  - Only create up to `audits.missing_tests_audit.max_proposals_per_run` changes per invocation (default `2`).
- **Output:** `AuditOutcome::SpecsWritten(vec![...])`. The same iteration's queue walk picks them up and implements.
- **Sandbox:** allows `Read`, `Glob`, `Grep`, `Bash`, `Write`, `Edit`. The foundation post-hoc check rejects any diff whose path isn't under `openspec/changes/`.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/audits/missing_tests.rs` (new), `prompts/missing-tests-audit.md` (new).
- Cost: one Claude CLI invocation per run; the produced changes then drive normal implementer invocations downstream.
- Operator-visible behavior: at the configured cadence, new "test-coverage-for-X" changes appear in `openspec/changes/`, get committed by the audit, and immediately enter the queue. Within the same iteration, they're implemented and a single PR ships both the spec creation and the test additions.
- Foundation dependency: requires `periodic-audits-foundation`. Specifically uses `WritePolicy::OpenSpecOnly`, default-prompt mechanism, audit-run log.
- Breaking: no. Default cadence `disabled`.
