## Why

A repository accumulates security gaps and latent bugs the same way it accumulates test gaps: gradually, invisibly, and faster than humans review. An LLM auditing the codebase periodically can surface plausible issues (injection sinks, unbounded resource use, missing input validation, panicking-on-attacker-input, etc.) and propose fixes.

This audit uses the same shape as missing-tests: read-only analysis followed by writing OpenSpec changes that describe a fix. The same iteration's `walk_queue` implements them. The implementer + reviewer steps in the existing pipeline catch any LLM mistakes before they hit a PR.

By going through the spec-driven flow (rather than directly editing code in an audit), every proposed fix gets the same level of scrutiny as a hand-written change: implementer drives it, reviewer reads the diff, the verifier checks spec alignment.

## What Changes

- **ADDED capability:** `orchestrator-cli` gains a "Security & bug audit" requirement.
- **Audit:** registered as `security_bug_audit`. `requires_head_change() = true`. `WritePolicy::OpenSpecOnly`.
- **Prompt:** embedded default at `prompts/security-bug-audit.md`. Operator overridable via `audits.security_bug_audit.prompt_path`. The prompt instructs the LLM to:
  - Audit the source tree for security issues (injection, auth/authz mistakes, secrets in source, unsafe deserialization, missing input validation at trust boundaries, race conditions, resource leaks, etc.) AND likely bugs (off-by-one, wrong operator, mishandled None/null, missing error propagation).
  - Filter aggressively: only report findings the auditor is reasonably confident about. False positives waste implementer time downstream.
  - For each confirmed finding, write a new OpenSpec change naming the fix.
  - Cap at `audits.security_bug_audit.max_proposals_per_run` (default `2`).
- **Naming convention:** changes prefixed with `fix-` for bug fixes and `secure-` for security hardening (e.g. `secure-sanitize-user-paths`, `fix-off-by-one-in-queue-walker`).
- **Output:** `AuditOutcome::SpecsWritten(...)`. Same-iteration implementation by the queue walk.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/audits/security_bug.rs` (new), `prompts/security-bug-audit.md` (new).
- Cost: one LLM invocation per run, plus downstream implementer invocations for each proposed change. The per-PR change cap bounds how many ship per iteration.
- Operator-visible behavior: at the configured cadence, new `fix-...` or `secure-...` changes appear and immediately enter the queue. The operator's first review point is the implementer's PR (with the spec already merged into the same PR).
- Foundation dependency: requires `periodic-audits-foundation`. Uses `WritePolicy::OpenSpecOnly`, default-prompt mechanism, audit-run log.
- Breaking: no. Default cadence `disabled`.
