## Why

The architecture-brightline audit (in `periodic-audits-foundation`) catches deterministic structural issues — file size, signature collisions, dead public items. But codebase complexity also accumulates in forms that aren't bright-line: cohesion problems, leaky abstractions, hidden coupling, modules whose responsibilities have drifted. Operators notice these issues weeks or months after they accrue, when the cost is high.

A consultative architecture audit uses an LLM to produce 3-5 questions per run anchored to specific files. It's lower-cadence than the brightline (monthly or quarterly typically) and is strictly read-only. The output is *questions*, not directives — the operator decides which (if any) are worth acting on. The framing exists precisely to avoid the failure mode of LLMs proposing "rewrite as microservices" or framework-replacement suggestions that suit no real project.

## What Changes

- **ADDED capability:** `orchestrator-cli` gains an "Architecture consultative audit" requirement.
- **Audit:** registered as `architecture_consultative`. `requires_head_change() = true`. `WritePolicy::None`.
- **Prompt:** embedded default at `prompts/architecture-consultative.md`. Operator overridable via `audits.architecture_consultative.prompt_path`. The default prompt is language-agnostic and includes explicit anti-patterns the LLM must avoid:
  - Do NOT suggest splitting the codebase into microservices, separate processes, or separate binaries.
  - Do NOT suggest a rewrite in a different language.
  - Do NOT suggest new dependencies (message queues, databases, caches, RPC frameworks, etc.) unless the project already uses one of equivalent shape.
  - Do NOT suggest "industry-standard" architectural patterns that imply a team-of-50 scale.
  - DO frame observations as questions ("should X be its own module?"), not directives.
  - DO anchor each observation to a specific file:line range.
  - DO penalize complexity: if a suggestion adds more code than it removes, drop it.
  - DO produce 3-5 observations maximum.
- **Output:** `AuditOutcome::Reported(findings)`. Each finding's `subject` is one question; `body` is one paragraph of context; `anchor` cites the file:line range.
- **Cadence intent:** designed for `monthly` or `quarterly` cadence. Daily/weekly invocations produce noise.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/audits/architecture_consultative.rs` (new), `prompts/architecture-consultative.md` (new).
- Cost: one Claude CLI invocation per run, sandboxed to Read+Glob+Grep+Bash. Strictly read-only.
- Operator-visible behavior: at the configured cadence (default `disabled`), a chatops post lists 0-5 architecture questions with file anchors. The audit-run log contains the full agent output for follow-up.
- Foundation dependency: requires `periodic-audits-foundation`. Uses `WritePolicy::None`, default-prompt mechanism, audit-run log, chatops Reported-findings format.
- Breaking: no. Default cadence `disabled`.
