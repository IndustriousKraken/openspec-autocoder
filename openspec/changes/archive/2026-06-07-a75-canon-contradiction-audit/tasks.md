# Implementation tasks

## 1. Register the audit

- [x] 1.1 Add `canon_contradiction_audit` to the `AuditRegistry` with `audit_type()` slug `canon_contradiction_audit`, `requires_head_change = true`, `WritePolicy::None`, and a read-only sandbox (`Read`/`Glob`/`Grep`; no `Bash`/`Write`/`Edit`).
- [x] 1.2 Add the slug to the `validate_audit_type_names` known-slug list, the README audit table, and `config.example.yaml` (per the `Registered periodic audits` requirement's alignment mandate). Default cadence `monthly`.

## 2. Detection driver

- [x] 2.1 Run the audit through the shared `agentic_run` primitive (a56) with `ORCH_MCP_ROLE = canon_contradiction_audit` and the embedded `prompts/canon-contradiction-audit.md` (override at `audits.canon_contradiction_audit.prompt_path`).
- [x] 2.2 Enumerate canonical requirements across `openspec/specs/*/spec.md`. When a21 RAG is enabled, retrieve the nearest requirements per requirement via `query_canonical_specs` and check each focused bundle. Retrieval breadth is a tunable setting with a sensible default.
- [x] 2.3 When RAG is not configured, degrade to a best-effort direct read of the canon and log that coverage is best-effort.

## 3. Submission tool

- [x] 3.1 Register `submit_canon_internal_contradictions` in the MCP server, advertised only when `ORCH_MCP_ROLE = canon_contradiction_audit`; payload `{ contradictions: [{ capability_a, requirement_a, capability_b, requirement_b, summary }] }`; relay via `record_submission`; schema-invalid payload is a correctable tool error.
- [x] 3.2 Consume the submission after the session into `AuditOutcome::Reported` findings; a missing submission consumes as empty (clean canon).

## 4. Disposition and suppression

- [x] 4.1 Compose each finding body to name both requirements (capability + title) and the conflict reason, so the operator can judge intent and heal via the existing audit-thread `send it`. Bound findings per run by an operator-configurable cap with a sensible default.
- [x] 4.2 Persist reported pairs in `.audit-state.json` keyed by an order-independent (capability + requirement-title) pair plus a content hash of each requirement; suppress unchanged recorded pairs, re-surface a pair when either requirement's text changed, and prune pairs no longer detected.

## 5. Prompt

- [x] 5.1 Write `prompts/canon-contradiction-audit.md`: define contradiction as logical incompatibility (both cannot hold); explicitly exclude general-plus-compatible-specialization pairs (the relational/PostgreSQL case); confidence-gate toward not reporting; instruct the agent to use `query_canonical_specs` when available and to return findings via `submit_canon_internal_contradictions`.

## 6. Tests

- [x] 6.1 The audit registers with the read-only sandbox and the `canon_contradiction_audit` role; default config leaves it disabled.
- [x] 6.2 With RAG enabled the driver calls `query_canonical_specs`; with RAG off it proceeds best-effort and logs the degradation.
- [x] 6.3 A general+compatible-specific pair is not reported; a logically incompatible pair is reported with both requirements named and the canon unmodified (clean-tree check holds).
- [x] 6.4 A previously-reported unchanged pair is suppressed; an edited pair re-surfaces; a healed pair is pruned.
- [x] 6.5 An empty result is silent (no chatops unless `notify_on_clean`).
- [x] 6.6 `submit_canon_internal_contradictions` is advertised only for the role; a schema-invalid payload is correctable; a missing submission consumes as empty.
- [x] 6.7 `validate_audit_type_names` accepts the new slug and still rejects an unknown one, listing the six registered slugs.

## 7. Acceptance gate

- [x] 7.1 `cargo test` passes for the autocoder crate.
- [x] 7.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 7.3 `openspec validate a75-canon-contradiction-audit --strict` passes.
