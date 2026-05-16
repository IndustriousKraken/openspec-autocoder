## 1. Default prompt template

- [x] 1.1 New file `prompts/missing-tests-audit.md`. Contents:
  - Framing: "You are auditing test coverage for this repository. Output: zero or more new OpenSpec change directories under `openspec/changes/`, each describing a meaningful coverage gap and proposing tests to fill it."
  - Survey instructions: glob source files via language-agnostic heuristics (look at file extensions: `.rs`, `.py`, `.cs`, `.go`, `.js`, `.ts`, `.rb`, `.java`, etc.). For each, identify functions/methods and their tests.
  - Filtering: suppress trivial getters, setters, single-line constructors, `Default` impls, conversions with no behavior. Focus on:
    - Error/Result paths with no test.
    - Branches with no assertion (the test runs them but doesn't verify behavior).
    - Edge cases obvious from the function signature (boundary values, None/null/empty, off-by-one).
  - OpenSpec format crash course (since the agent must produce valid OpenSpec changes):
    - Each change is a directory under `openspec/changes/<change_name>/`.
    - `proposal.md` with `## Why`, `## What Changes`, `## Impact` sections.
    - `tasks.md` with checklist items naming specific test functions to add.
    - When the gap implies a capability invariant, `specs/<capability>/spec.md` with `## ADDED Requirements` block.
  - Naming convention: prefix change names with `tests-` so operators recognize audit-produced changes (e.g. `tests-error-paths-in-queue-engine`, `tests-edge-cases-in-busy-marker-recovery`).
  - Hard constraints:
    - "Do NOT modify any file outside `openspec/changes/`."
    - "Do NOT propose deleting tests."
    - "Do NOT propose modifying existing tests unless they are factually broken (the test does not compile, or runs but never asserts)."
    - "Pick at most N gaps for this run, where N is provided as `MAX_PROPOSALS` at the top of this prompt. Order by severity: missing tests on error paths first, then untested branches, then edge cases."
  - The audit's `run()` substitutes `MAX_PROPOSALS` with the configured value before sending the prompt.
- [x] 1.2 Embed at compile time via `include_str!`.

## 2. Audit implementation

- [x] 2.1 New module `autocoder/src/audits/missing_tests.rs`. Define `pub struct MissingTestsAudit { settings: AuditSettings, max_proposals_per_run: u32, executor_command: String, executor_timeout_secs: u64 }`.
- [x] 2.2 `impl Audit` with `audit_type() = "missing_tests_audit"`, `requires_head_change() = true`, `write_policy() = WritePolicy::OpenSpecOnly`.
- [x] 2.3 `run(&self, ctx)`:
  1. Resolve prompt (override or default). Substitute `MAX_PROPOSALS` → string of `self.max_proposals_per_run`.
  2. Build sandbox: allowed_tools = `["Read", "Glob", "Grep", "Bash", "Write", "Edit"]`; standard deny lists.
  3. Spawn CLI with the prompt on stdin, capture stdout/stderr, enforce timeout. Mirror into audit-run log.
  4. After the CLI returns, scan `openspec/changes/` for new directories (those that didn't exist before this run — compare against an `HashSet` captured pre-run).
  5. For each new directory: validate via `openspec validate <name> --strict`. If validation fails, reject the change (delete the directory) and add a Finding to the audit-run log noting the failure. (Do NOT chatops-alert per-change validation failures; the audit's WARN log is sufficient.)
  6. If at least one validated change exists: `git add openspec/changes/ && git commit -m "audit: missing-tests proposals (<N> change(s))"`.
  7. Return `AuditOutcome::SpecsWritten(validated_names)`.
- [x] 2.4 The pre-run snapshot of `openspec/changes/` is captured before spawning the CLI so we can diff post-run reliably.
- [x] 2.5 Tests `audits::missing_tests::tests`:
  - `parses_max_proposals_substitution_into_prompt`
  - `pre_run_snapshot_captures_existing_change_dirs`
  - `post_run_detects_only_new_change_dirs`
  - `validation_failure_rejects_change_and_logs_warning`
  - `validation_success_commits_change_to_agent_branch`
  - `empty_findings_no_commit_no_chatops_post`

## 3. Registration

- [x] 3.1 In `cli/run.rs::build_audit_registry`, append `Arc::new(MissingTestsAudit::new(&audit_settings, &cfg.executor))`.

## 4. Documentation

- [x] 4.1 README "Periodic audits" — add `missing_tests_audit` to the registered-audits list. Document additive-only semantics, the `tests-` naming convention, the per-run cap.
- [x] 4.2 README "Config reference" — under `audits.missing_tests_audit`, document `prompt_path`, `max_proposals_per_run` (default `2`), `notify_on_clean`.

## 5. Verification

- [x] 5.1 `cargo test` passes.
- [x] 5.2 `openspec validate missing-tests-audit --strict` passes.
