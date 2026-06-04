# orchestrator-cli — delta for a59-agentic-contradiction-check

## MODIFIED Requirements

### Requirement: Change-internal contradiction pre-flight check (opt-in)
autocoder SHALL provide an opt-in pre-flight check that detects semantic contradictions among the requirements WITHIN a single OpenSpec change before the executor is invoked. The check runs a CLI-wrapped agentic session through the shared `agentic_run` primitive (a56) in a read-only sandbox that reads the change's spec-delta files on demand AND returns a structured listing of contradictions (requirements that cannot all hold simultaneously) via the `submit_contradictions` MCP tool. On non-empty findings, autocoder SHALL write `.needs-spec-revision.json` with `revision_suggestion` populated from the contradictions narrative, post the existing `AlertCategory::SpecNeedsRevision` chatops alert, AND halt the queue walk for this iteration. The executor SHALL NOT be invoked when contradictions are found.

The check SHALL be gated by `executor.change_internal_contradiction_check` (`disabled` default, `enabled` opt-in). The model is configured via `executor.change_internal_contradiction_check_llm` (parallel to the `reviewer:` config block — provider, model, api_key source, optional api_base_url), which a56's CLI strategy translates into the wrapped CLI's model-selection mechanism. The `claude` strategy reaches only Anthropic-shaped endpoints; a model whose provider resolves to a CLI with no registered strategy makes the check fail open (per the fail-open posture below) until that strategy is registered. Enabling the check without configuring the model SHALL fail at daemon startup with a fail-fast validation error.

The check SHALL fail-open: an agentic-session error (spawn, timeout, OR a resolved CLI strategy that is not registered), a schema-rejected submission the agent never corrects, a session that ends with no submission, OR any other failure log a WARN AND treat the check as "no contradictions found." A schema-invalid `submit_contradictions` call mid-session is a correctable tool error the agent can retry (a56). The daemon does NOT gate work on a failed check — operators see the WARN in journalctl AND can investigate; the executor proceeds.

The check runs AFTER `a17`'s mechanical archivability check AND BEFORE the executor. The two checks are layered: `a17` catches structural defects (header mismatches), `a19` catches semantic ones (self-contradictions). Most clean changes pass both with no LLM cost beyond the contradiction check's own.

#### Scenario: Default-disabled produces no contradiction-check session
- **WHEN** `executor.change_internal_contradiction_check` is unset (default `disabled`)
- **AND** any change reaches the pre-executor pipeline
- **THEN** no contradiction-check session is spawned (no LLM cost)
- **AND** the executor is invoked normally (assuming `a17`'s archivability check passed)

#### Scenario: Enabled mode runs an agentic session over the change's deltas
- **WHEN** `executor.change_internal_contradiction_check: enabled` AND the model config is set
- **AND** a change passes `a17`'s archivability check
- **THEN** the pipeline runs an `agentic_run` session (a56) in a read-only sandbox (`Read`/`Glob`/`Grep`, `ORCH_MCP_ROLE = contradiction_check`, the `submit_contradictions` MCP tool) with the embedded `prompts/change-contradiction-check.md` prompt (OR the configured override)
- **AND** the agent reads the change's spec-delta files on demand AND returns contradictions by calling `submit_contradictions` with `{ contradictions: [{ requirement_a, requirement_b, summary }] }`

#### Scenario: Empty contradictions submission proceeds to executor
- **WHEN** the agent calls `submit_contradictions` with an empty `contradictions` array
- **THEN** the pipeline proceeds to the executor
- **AND** no marker is written
- **AND** no chatops alert fires

#### Scenario: Non-empty contradictions submission writes marker and skips executor
- **WHEN** the agent submits one or more contradictions
- **THEN** the pipeline writes `.needs-spec-revision.json` with `revision_suggestion` text populated from the contradictions narrative (per the documented format)
- **AND** the marker's `unarchivable_deltas` AND `unimplementable_tasks` arrays are empty (this case is semantic, not structural)
- **AND** the chatops alert under `AlertCategory::SpecNeedsRevision` fires (subject to the 24h throttle)
- **AND** the executor is NOT invoked for this change OR any subsequent change in this iteration

#### Scenario: Session failure fails open
- **WHEN** the agentic session fails (spawn error, timeout, OR the resolved CLI strategy is not registered — e.g. a non-`claude` command whose strategy has not been added)
- **THEN** the pipeline logs a WARN naming the error
- **AND** treats the check as "no contradictions found"
- **AND** proceeds to the executor
- **AND** the daemon does NOT gate iteration progress on the failed check

#### Scenario: No valid submission fails open
- **WHEN** the session ends with no schema-valid `submit_contradictions` call (the agent never submits, OR every submission is schema-rejected and never corrected)
- **THEN** the pipeline logs a WARN naming a truncated session-output excerpt (200 chars)
- **AND** treats the check as "no contradictions found" AND proceeds to the executor (the same fail-open posture)

#### Scenario: Enabled without model config fails fast at startup
- **WHEN** `config.yaml` sets `executor.change_internal_contradiction_check: enabled`
- **AND** `executor.change_internal_contradiction_check_llm` is unset
- **THEN** daemon startup fails with the error `executor.change_internal_contradiction_check is enabled but executor.change_internal_contradiction_check_llm is not configured`
- **AND** the daemon does NOT begin polling
- **AND** the operator sees the error message on stderr AND in journalctl

#### Scenario: Prompt override replaces the embedded default
- **WHEN** `executor.change_internal_contradiction_check_prompt_path` points to an override file
- **THEN** the pipeline reads the override file AND uses its contents as the prompt template
- **AND** an empty override file produces an error at use time (the daemon does not feed an empty prompt to the session)

#### Scenario: Marker `revision_suggestion` enumerates findings clearly
- **WHEN** the agent submits 2 contradictions
- **THEN** the marker's `revision_suggestion` text contains both findings numbered 1 AND 2, each with `requirement_a`, `requirement_b`, AND `summary` fields
- **AND** the text ends with operator guidance (`Edit the conflicting requirements... clear via @<bot> clear-revision`)

#### Scenario: Operator clearing the marker without spec edits is permitted
- **WHEN** the operator assesses the findings as a false positive AND runs `@<bot> clear-revision <repo> <change>` without editing the spec
- **THEN** the next polling iteration retries the change AND re-runs the contradiction check
- **AND** the operator's tolerance for false positives shapes their decision to enable the check OR keep it disabled
