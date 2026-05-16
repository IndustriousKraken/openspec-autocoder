## Why

OpenSpec specs are supposed to describe what the code does. In practice, both drift over time: a bugfix lands without updating the spec; a spec gets edited to clarify intent and the code falls a step behind; an archived change's requirement subtly diverges from current behavior. Neither side is "wrong" — but the operator should know they've diverged so they can decide which one to update.

A drift audit runs an LLM read-only over the current canonical specs (`openspec/specs/<capability>/spec.md`) and the relevant code, looking for places where the SHALL/SHOULD/MUST language doesn't match observable code behavior. It reports findings via chatops. It never auto-fixes either side — that's a judgment call that requires the operator's context.

## What Changes

- **ADDED capability:** `orchestrator-cli` gains a "Drift audit" requirement.
- **Audit:** registered as `drift_audit`. `requires_head_change() = true` (no code change → no drift to find). `WritePolicy::None`.
- **Prompt:** embedded default at `prompts/drift-audit.md`. Operator can override via `audits.drift_audit.prompt_path`. The default prompt is strictly language-agnostic and instructs the LLM to:
  - Read each canonical spec under `openspec/specs/`.
  - For each requirement, identify the code surface that implements it (best-effort grep).
  - Flag mismatches with severity:
    - `high`: a SHALL/MUST clause has no corresponding code OR the code does something contradicting the spec.
    - `medium`: a SHOULD clause has a meaningful gap.
    - `low`: wording differences that don't affect behavior (these are noise; the prompt instructs the LLM to filter them out).
  - Output ONLY findings; produce NO code edits, NO spec edits, NO new files.
- **Output:** `AuditOutcome::Reported(findings)`. Each finding includes the spec capability, the requirement title, the affected code paths (best-effort), and a one-paragraph divergence description. Full output also lands in the audit-run log.
- **Iteration order:** runs in the existing audit slot (after `recreate_branch`, before `list_pending`). Drift findings are observations, not work — they don't enter the queue automatically. The operator decides whether each finding becomes a code-fix change, a spec-fix change, or is dismissed.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/audits/drift.rs` (new), `prompts/drift-audit.md` (new template).
- Cost: one Claude CLI invocation per audit run, sandboxed to Read+Glob+Grep+Bash. The post-hoc diff check enforces zero workspace writes.
- Operator-visible behavior: at the configured cadence (default `disabled` per foundation pattern), each audit run produces a chatops post with findings — or silence if no drift is detected.
- Foundation dependency: requires `periodic-audits-foundation`. Specifically uses `WritePolicy::None`, the audit-run log, the chatops Reported-findings format, and the default-prompt mechanism.
- Breaking: no. Default cadence `disabled`.
