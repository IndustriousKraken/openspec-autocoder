# orchestrator-cli — delta for a75-canon-contradiction-audit

## ADDED Requirements

### Requirement: Canon-internal contradiction audit
autocoder SHALL register a `canon_contradiction_audit` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI through the shared `agentic_run` primitive with a read-only sandbox (`Read`/`Glob`/`Grep`; NO `Bash`, `Write`, or `Edit`) and `ORCH_MCP_ROLE = canon_contradiction_audit`, scans the canonical specs for pairs of requirements that cannot both hold, and reports them advisorily. The agent SHALL return findings by calling the `submit_canon_internal_contradictions` MCP tool — consumed by the daemon as the audit result — rather than by emitting JSON on stdout. The audit is `requires_head_change = true` AND `WritePolicy::None`. Its default cadence is heavy (`monthly`) given that each run reasons over the whole canon; the cadence is operator-configurable per the cadence schema.

**RAG-assisted detection, best-effort fallback.** The audit enumerates the canonical requirements across `openspec/specs/*/spec.md`. When `a21`'s canonical-spec RAG is enabled, the agent SHALL use `query_canonical_specs` to retrieve, for each requirement, a bounded set of the most semantically-similar requirements AND check that focused bundle for contradiction — bounding the per-call input AND targeting related requirements, where contradictions actually live, rather than attempting an intractable all-pairs sweep over a large canon. When RAG is not configured, the audit SHALL degrade to a best-effort direct read of the canon AND log that coverage is best-effort (subtle cross-capability pairs may be missed without retrieval). The retrieval breadth is an operator-tunable setting with a sensible default.

**Precision over recall.** A finding REQUIRES that the two requirements be logically incompatible — both cannot hold at once. A general requirement together with a *compatible* specialization of it (e.g. "all data in a relational database" AND "use PostgreSQL", since PostgreSQL is relational) is NOT a contradiction AND SHALL NOT be reported; flagging it would be the general-vs-specific information-loss error in reverse. The prompt SHALL confidence-gate toward NOT reporting when uncertain, because a false positive is operator noise. This prompt guidance is design intent verified by the drift audit's semantic judgment; it SHALL NOT be pinned by a unit test asserting verbatim substrings of the prompt (per the project-documentation requirement `Tests assert behavior or derivation, never message wording`).

**Advisory disposition.** On findings the audit SHALL return `AuditOutcome::Reported(findings)`; the framework posts the standard chatops summary. The audit SHALL NOT modify the canon. Each finding's body SHALL name BOTH conflicting requirements (capability + requirement title) AND explain why they conflict, so the maintainer can judge project intent AND — where they choose to heal it — use the existing audit-thread `send it` verb to schedule a triage run that drafts the resolving MODIFY in the chosen direction. The number of findings per run is bounded by an operator-configurable cap with a sensible default; pairs beyond the cap surface on subsequent runs.

**Re-report suppression.** The audit SHALL persist the contradictions it has reported — keyed by an order-independent pair of (capability + requirement title) for the two sides, plus a content hash of each requirement's text — in its audit state. On a later run it SHALL suppress re-reporting a recorded pair whose two requirements are textually unchanged; it SHALL re-surface a pair when either requirement's text has changed since it was recorded; AND it SHALL prune records for pairs no longer detected (healed). This keeps an unhealed contradiction from re-spamming chatops every run while still re-alerting when the conflicting text is edited.

#### Scenario: Runs read-only and agentic with its role
- **WHEN** the audit runs
- **THEN** autocoder spawns the wrapped agent CLI via `agentic_run` with a sandbox that allows only `Read`/`Glob`/`Grep` (`Bash`, `Write`, and `Edit` denied) AND sets `ORCH_MCP_ROLE = canon_contradiction_audit`
- **AND** the prompt is the embedded `prompts/canon-contradiction-audit.md` template OR the operator override at `audits.canon_contradiction_audit.prompt_path`

#### Scenario: Uses RAG when enabled, degrades and logs when not
- **WHEN** the audit runs AND `a21`'s canonical-spec RAG is enabled
- **THEN** the agent retrieves the nearest requirements per requirement via `query_canonical_specs` AND checks those focused bundles
- **WHEN** the audit runs AND RAG is not configured
- **THEN** the audit proceeds with a best-effort direct read of the canon AND logs that coverage is best-effort

#### Scenario: A general rule plus a compatible specialization is not a contradiction
- **WHEN** the canon contains a general requirement AND a compatible specialization of it (e.g. "all data in a relational database" alongside "use PostgreSQL")
- **THEN** the audit does NOT report them as a contradiction

#### Scenario: A logically incompatible pair is reported advisorily without touching the canon
- **WHEN** two canonical requirements cannot both hold (e.g. one mandates a relational database for all data, another mandates a document store for some of it)
- **THEN** the audit returns `AuditOutcome::Reported` with a finding naming BOTH requirements (capability + title) AND the reason they conflict
- **AND** no file under `openspec/specs/` is modified (the `WritePolicy::None` post-hoc clean-tree check holds)

#### Scenario: Previously-reported pairs are suppressed; edited ones re-surface; healed ones are pruned
- **WHEN** a contradiction was reported on a prior run AND both its requirements are textually unchanged on a later run
- **THEN** the later run does NOT re-report that pair
- **WHEN** either requirement's text has changed since the pair was recorded
- **THEN** the later run re-surfaces the pair
- **AND** a recorded pair that is no longer detected is pruned from the audit's report state

#### Scenario: No contradictions produces a silent outcome
- **WHEN** the audit finds no logically incompatible pair
- **THEN** it returns `AuditOutcome::Reported(vec![])`
- **AND** unless `notify_on_clean: true` is set, no chatops message is posted (per the framework-level scenario)

## MODIFIED Requirements

### Requirement: Registered periodic audits
autocoder SHALL register exactly the following audits in its `AuditRegistry` at startup, identified by their `audit_type()` slug: `architecture_brightline`, `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`, `canon_contradiction_audit`. The slug `dependency_update_triage` SHALL NOT be registered. Each registered audit's cadence is independently configurable under `audits.defaults` and per-repo `repositories[].audits` overrides; an unregistered slug present in either location SHALL fail config validation at startup with the existing "unknown audit type" error message that lists the registered slugs.

This enumeration is the canonical contract for which audits exist. Future changes that add or remove an audit MUST update this requirement in the same commit so the spec and the registered set never drift. The `validate_audit_type_names` startup check enforces the spec/code consistency at runtime: an operator's YAML naming an unregistered slug is a startup-time failure with a clear list of valid slugs.

#### Scenario: Startup with default config registers the canonical set
- **WHEN** autocoder starts with a config whose `audits:` block is
  absent OR present but with all-`disabled` cadences
- **THEN** the in-memory `AuditRegistry` contains exactly the six
  audits enumerated above
- **AND** no audit runs (all are `Disabled` by effective cadence),
  preserving prior daemon behavior

#### Scenario: Operator configures a registered audit
- **WHEN** an operator sets a non-`disabled` cadence under
  `audits.defaults.<slug>` for any of the six registered slugs
  OR under `repositories[].audits.<slug>`
- **THEN** config validation succeeds AND the scheduler invokes
  that audit per its cadence on the appropriate iteration

#### Scenario: Operator configures the removed dependency_update_triage slug
- **WHEN** an operator's `audits.defaults` (or
  `repositories[].audits`, or `audits.settings`) contains the key
  `dependency_update_triage` (a slug that was registered in
  earlier versions of autocoder but has since been removed)
- **THEN** `validate_audit_type_names` fails at startup with an
  error naming `dependency_update_triage` as unknown AND listing
  the registered slugs so the operator knows what to use
- **AND** the daemon does NOT start (consistent with the existing
  behavior for typos in audit slugs); the operator must remove the
  entries from their YAML to recover

#### Scenario: Adding or removing an audit requires updating this requirement
- **WHEN** an implementing agent ships a change that registers a
  new audit (extending the registry list) or removes one (deleting
  a registration)
- **THEN** the change's spec delta MUST update this requirement's
  enumeration so the canonical list reflects the new state
- **AND** the change's commit SHOULD also update the
  `validate_audit_type_names` known-slug list, the README audit
  table, and `config.example.yaml` so all four artifacts (spec,
  validator, README, example) stay aligned
