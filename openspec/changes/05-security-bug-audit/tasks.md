## 1. Default prompt template

- [ ] 1.1 New file `prompts/security-bug-audit.md`. Contents:
  - Framing: "You are auditing this repository for security issues and likely bugs. Output: zero or more new OpenSpec change directories under `openspec/changes/`, each describing one confirmed issue and proposing a fix."
  - **In-scope categories** (each with one-line examples):
    - Injection (SQL, command, path, template)
    - Authentication / authorization mistakes (missing checks, bypasses)
    - Hard-coded secrets, keys, tokens
    - Unsafe deserialization
    - Missing input validation at trust boundaries (HTTP handlers, file uploads, IPC)
    - Race conditions / TOCTOU
    - Resource leaks (file handles, sockets, async tasks)
    - Off-by-one, wrong operator, mishandled None/null/empty
    - Missing error propagation that silently swallows failures
    - Panicking on attacker-controlled input
  - **Out-of-scope**: code style, naming, architectural preferences, micro-optimizations, anything the project has explicitly accepted.
  - **Confidence filter**: "Only emit a change for findings you are highly confident about. A false positive wastes downstream implementer work and can introduce regressions. When in doubt, DON'T emit."
  - **OpenSpec format** crash course (same as missing-tests prompt).
  - **Naming convention**: `fix-<short-issue-desc>` for bugs, `secure-<short-issue-desc>` for security hardening.
  - **Per-change content**:
    - `proposal.md`: cite the source location (file:line), describe the issue, describe the fix.
    - `tasks.md`: implementation steps.
    - `specs/<capability>/spec.md`: when the fix implies an invariant (e.g. "all user inputs SHALL be validated against schema Y"), MODIFY or ADD a requirement.
  - **Hard constraints**:
    - "Do NOT modify any file outside `openspec/changes/`."
    - "Do NOT fix bugs directly — propose them as changes for the implementer to drive."
    - "Pick at most MAX_PROPOSALS gaps; order by severity (high first)."
- [ ] 1.2 Embed at compile time via `include_str!`.

## 2. Audit implementation

- [ ] 2.1 New module `autocoder/src/audits/security_bug.rs`. Define `pub struct SecurityBugAudit { settings: AuditSettings, max_proposals_per_run: u32, executor_command: String, executor_timeout_secs: u64 }`.
- [ ] 2.2 `impl Audit` with `audit_type() = "security_bug_audit"`, `requires_head_change() = true`, `write_policy() = WritePolicy::OpenSpecOnly`.
- [ ] 2.3 `run(&self, ctx)`: identical algorithm to missing-tests-audit (see that change's tasks.md §2.3) with these differences:
  - Different prompt (`prompts/security-bug-audit.md`).
  - Different `MAX_PROPOSALS` value source.
  - Different audit_type label.
- [ ] 2.4 Both audits share so much logic that the algorithm SHOULD be extracted into a shared helper in `audits/mod.rs`: `run_specs_writing_audit(audit_type, prompt, max_proposals, ctx) -> Result<AuditOutcome>`. Both `MissingTestsAudit` and `SecurityBugAudit` delegate. (Refactor the missing-tests audit when this change lands; it's a small move.)
- [ ] 2.5 Tests `audits::security_bug::tests`:
  - `prompt_substitution_includes_max_proposals`
  - `change_with_fix_prefix_validates_and_commits`
  - `change_with_secure_prefix_validates_and_commits`
  - `oversized_run_truncated_to_cap_with_warn_log`
  - `low_confidence_finding_filtering_explicit_in_prompt` (asserts prompt text contains the confidence-filter instructions — protects against accidental prompt drift)

## 3. Registration

- [ ] 3.1 In `cli/run.rs::build_audit_registry`, append `Arc::new(SecurityBugAudit::new(&audit_settings, &cfg.executor))`.

## 4. Documentation

- [ ] 4.1 README "Periodic audits" — add `security_bug_audit` to the registered-audits list. Document spec-driven flow + `fix-`/`secure-` naming.
- [ ] 4.2 README "Config reference" — under `audits.security_bug_audit`, document `prompt_path`, `max_proposals_per_run` (default `2`), `notify_on_clean`.
- [ ] 4.3 README operator-warning paragraph: "This audit can be noisy in early iterations on an unfamiliar codebase. Operators are advised to monitor the first few invocations and tighten the prompt (or disable the audit) if false-positive rate is high."

## 5. Verification

- [ ] 5.1 `cargo test` passes.
- [ ] 5.2 `openspec validate security-bug-audit --strict` passes.
