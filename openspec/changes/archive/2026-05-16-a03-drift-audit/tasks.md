## 1. Default prompt template

- [x] 1.1 New file `prompts/drift-audit.md`. Contents:
  - System framing: "You are auditing alignment between OpenSpec specs and code. Output ONLY findings via the structured JSON format described below."
  - Instructions:
    - Glob `openspec/specs/*/spec.md`. For each capability, read every requirement.
    - For each requirement, identify the code that implements it via `Grep`/`Read`. Cite paths.
    - Classify divergences:
      - `high`: SHALL/MUST clause has no implementation OR code does the opposite.
      - `medium`: SHOULD clause has a meaningful gap.
      - Suppress: wording-only differences with no behavioral consequence.
    - Ignore `openspec/changes/` and `openspec/changes/archive/`.
  - Output format: single JSON object `{ "findings": [...] }`. No commentary outside the JSON.
  - Anti-noise: explicit list of what NOT to report (wording, formatting, stylistic).
  - Hard constraints: "Do NOT use the `Write` or `Edit` tools. Do NOT create files. Do NOT modify the workspace. Your job is to report only."
- [x] 1.2 Embed at compile time via `include_str!("../../prompts/drift-audit.md")` in `audits/drift.rs`.

## 2. Audit implementation

- [x] 2.1 New module `autocoder/src/audits/drift.rs`. Define `pub struct DriftAudit { settings: AuditSettings, executor_command: String, executor_timeout_secs: u64 }`.
- [x] 2.2 `impl Audit for DriftAudit` with `audit_type() = "drift_audit"`, `requires_head_change() = true`, `write_policy() = WritePolicy::None`.
- [x] 2.3 `run(&self, ctx)` algorithm:
  1. Resolve prompt: `settings.prompt_path` (if set, read file; reject empty) else `DEFAULT_DRIFT_PROMPT`.
  2. Construct a `ResolvedSandbox` with `allowed_tools = ["Read", "Glob", "Grep", "Bash"]` and the existing default deny lists.
  3. Write a one-shot settings file via the same mechanism as `ClaudeCliExecutor::write_sandbox_settings`. Use a per-invocation tempdir to avoid collisions (mirroring the earlier sandbox-settings test isolation pattern).
  4. Spawn the CLI with the prompt on stdin, capture stdout/stderr, enforce `executor.timeout_secs`.
  5. Mirror stdout/stderr into the audit-run log writer.
  6. Parse stdout as `{ "findings": [...] }`. On parse failure, return `Err`.
  7. Map each parsed entry to a `Finding`. Severity strings `"high" | "medium" | "low"` map to `Severity` variants; unknown values are treated as `low` with a logged WARN.
  8. Return `AuditOutcome::Reported(findings)`.
- [x] 2.4 Reuse `ClaudeCliExecutor::write_sandbox_settings`-style logic if cleanly extractable; otherwise duplicate the small block locally. Prefer a shared helper in `audits/mod.rs` that returns the path + a `TempFileGuard`, so the drift audit and future LLM audits don't each reimplement it.

## 3. Registration

- [x] 3.1 In `cli/run.rs::build_audit_registry`, append `Arc::new(DriftAudit::new(&audit_settings, &cfg.executor))`.
- [x] 3.2 Tests in `audits::drift::tests`:
  - `parses_well_formed_findings_json`
  - `parses_empty_findings_array_to_no_findings_outcome`
  - `malformed_json_returns_err_with_excerpt`
  - `unknown_severity_string_maps_to_low_with_warn_log`
  - `run_writes_full_stdout_to_audit_log` (use a fake CLI command, e.g. `/bin/echo` invoked with a canned response, to keep the test offline)
  - `sandbox_settings_file_cleaned_up_after_run` (per-invocation tempdir, isolated like the earlier executor cleanup test)

## 4. Documentation

- [x] 4.1 README "Periodic audits" — add `drift_audit` to the registered-audits list. Document: triggers on HEAD change at the configured cadence, is purely advisory, never modifies code or specs.
- [x] 4.2 README "Config reference" — under `audits.drift_audit`, document `prompt_path` (override default), `notify_on_clean` (post a "no findings" message), and any drift-specific knobs.

## 5. Verification

- [x] 5.1 `cargo test` passes.
- [x] 5.2 `openspec validate drift-audit --strict` passes.
