## 1. Default prompt template

- [ ] 1.1 New file `prompts/architecture-consultative.md`. Contents:
  - **Framing**: "You are providing a senior-engineer's architecture read of this codebase. Output 0-5 anchored observations phrased as questions. Your audience is one operator who knows this code; surface things they may have stopped noticing, not things they'd find on day 1."
  - **Anti-patterns to AVOID** (explicit, prominent):
    - Do NOT suggest splitting the codebase into microservices, separate processes, or separate binaries.
    - Do NOT suggest a rewrite in a different programming language.
    - Do NOT suggest new infrastructure dependencies (message queues, databases, caches, RPC frameworks, container orchestrators) unless the project already uses one of equivalent shape.
    - Do NOT suggest patterns implying team-of-50 scale (event sourcing for a single-operator daemon, CQRS where a function would do, hexagonal architecture overlay when not needed, etc.).
    - Do NOT suggest stylistic refactorings (renaming, formatting, idiomatic preferences).
    - Do NOT suggest changes whose implementation would add more code than it removes.
  - **Output expectations**:
    - Frame each observation as a question, not a directive. "Should X be its own module?" not "Split X into a module."
    - Anchor each observation to a specific `file:line-line` range.
    - Provide one paragraph of context per observation.
    - Maximum 5 observations per run; aim for 3.
    - If you have nothing high-quality to say, emit an empty findings array. Silence is acceptable.
  - **Language-agnostic survey method**:
    - Glob source files via extension heuristics.
    - Read directory structure to identify modules / packages / namespaces.
    - Examine boundaries between modules: are responsibilities aligned with cohesion?
    - Note files whose imports / dependencies suggest they straddle concerns.
  - **Polyglot awareness**: codebases with frontend + backend are normal; bridges between languages are expected; don't flag the polyglot nature itself.
  - **Output format** (strict JSON):
    ```json
    {
      "findings": [
        {
          "subject": "...question...",
          "body": "...one paragraph...",
          "anchor": "path/to/file.ext:120-180",
          "severity": "low" | "medium"
        }
      ]
    }
    ```
  - **Hard constraints**:
    - "Do NOT use the `Write` or `Edit` tools. Do NOT create files. Do NOT modify the workspace."
- [ ] 1.2 Embed at compile time via `include_str!`.

## 2. Audit implementation

- [ ] 2.1 New module `autocoder/src/audits/architecture_consultative.rs`. Define `pub struct ArchitectureConsultativeAudit { settings: AuditSettings, executor_command: String, executor_timeout_secs: u64 }`.
- [ ] 2.2 `impl Audit` with `audit_type() = "architecture_consultative"`, `requires_head_change() = true`, `write_policy() = WritePolicy::None`.
- [ ] 2.3 `run(&self, ctx)` follows the drift-audit shape:
  1. Resolve prompt (override or default).
  2. Build read-only sandbox: `["Read", "Glob", "Grep", "Bash"]`.
  3. Spawn CLI, capture output, mirror to audit-run log.
  4. Parse output as `{ "findings": [...] }`.
  5. Reject parse failures. Reject runs with more than 5 findings (return `Err`).
  6. Map to `Finding` values; return `AuditOutcome::Reported(findings)`.
- [ ] 2.4 If the drift-audit landed first with a shared LLM-audit helper, reuse it here. Otherwise consider extracting after both land.
- [ ] 2.5 Tests `audits::architecture_consultative::tests`:
  - `parses_well_formed_findings_json`
  - `parses_zero_findings_as_no_findings_outcome`
  - `rejects_runs_with_more_than_5_findings`
  - `malformed_json_returns_err_with_excerpt`
  - `prompt_contains_anti_microservices_clause` (asserts prompt text — protects against accidental prompt drift)
  - `prompt_contains_language_agnostic_clause` (same protection)

## 3. Registration

- [ ] 3.1 In `cli/run.rs::build_audit_registry`, append `Arc::new(ArchitectureConsultativeAudit::new(&audit_settings, &cfg.executor))`.

## 4. Documentation

- [ ] 4.1 README "Periodic audits" — add `architecture_consultative` to the registered-audits list. Document the consultative nature, the recommended `monthly`/`quarterly` cadence, the prompt's anti-pattern guardrails.
- [ ] 4.2 README operator-guidance: "If the consultative audit produces noisy output, tighten the prompt (operator override at `audits.architecture_consultative.prompt_path`) before reaching for `disabled`. The prompt's anti-pattern list exists specifically to mitigate common LLM failure modes; if the output still misfires, the prompt is where to fix it."

## 5. Verification

- [ ] 5.1 `cargo test` passes.
- [ ] 5.2 `openspec validate architecture-consultative-audit --strict` passes.
