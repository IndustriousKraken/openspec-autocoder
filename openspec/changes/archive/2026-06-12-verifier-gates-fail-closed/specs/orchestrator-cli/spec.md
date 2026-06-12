# orchestrator-cli — delta for verifier-gates-fail-closed

## MODIFIED Requirements

### Requirement: Verifier-gate framework
autocoder's change-lifecycle consistency checks SHALL be organized as a verifier-gate framework of exactly three named gates positioned around the executor run:

- the `[in]` gate — change-internal consistency, run BEFORE the executor;
- the `[canon]` gate — change-vs-canonical consistency, run BEFORE the executor;
- the `[out]` gate — code-implements-spec, run AFTER the executor.

Each gate SHALL be individually opt-in AND SHALL own its disposition, but NO gate treats an inability to run as a pass (the gatekeepers-fail-closed standard). The pre-executor gates (`[in]`, `[canon]`) FAIL CLOSED: a gate's own failure (transport, parse, unregistered strategy, no submission) does NOT proceed as "no findings" — it holds the change in an explicit failed-to-run state (the change was NOT evaluated), surfaces a distinct "gate FAILED TO RUN — change held" alert, AND halts the iteration; an operator clears the hold (after fixing the gate) to retry. The `[out]` gate is advisory — it never auto-acts (no revision, no block) — AND fails to a VISIBLE state: on its own failure it renders an explicit "FAILED TO RUN" section rather than silently omitting one. Each gate's diagnostics (log lines AND any operator surface it writes) SHALL carry the gate's stable identifier so a finding — OR a held/failed-to-run state — is attributable to the gate that produced it.

The `[in]` gate IS the existing change-internal contradiction pre-flight check (its own requirement defines its behavior, opt-in gating, fail-closed posture, marker, AND alert); this framework reframes that check under the `[in]` identifier. The `[canon]` AND `[out]` gates are realized by their own requirements; until a gate is realized the framework treats it as absent AND invokes nothing for it.

#### Scenario: The `[in]` gate runs the contradiction check, labeled
- **WHEN** the `[in]` gate runs for a change
- **THEN** it executes the change-internal contradiction pre-flight check (same opt-in gating, fail-closed posture, marker, AND alert category)
- **AND** its emitted log / diagnostic lines carry the `[in]` gate identifier so the finding is attributable to that gate

#### Scenario: An unrealized gate is inert
- **WHEN** the `[canon]` OR `[out]` gate has not been realized by a subsequent change
- **THEN** resolving that gate yields "no installed gate"
- **AND** the framework invokes nothing for it — no gate is run speculatively

#### Scenario: Gate disposition follows the gate's lifecycle position
- **WHEN** a pre-executor gate (`[in]` or `[canon]`) fails for its own reasons (transport, parse, unregistered strategy, no submission)
- **THEN** the framework treats it as fail-CLOSED: it holds the change in an explicit failed-to-run state, surfaces it, AND does NOT proceed to the executor as if the gate passed
- **WHEN** the `[out]` gate fails for its own reasons
- **THEN** the framework renders an explicit "FAILED TO RUN" section (advisory, never blocking) rather than omitting one
- **WHEN** the `[out]` gate produces findings
- **THEN** the framework treats them as advisory: they annotate operator surfaces AND do NOT auto-trigger a revision or block

### Requirement: Change-internal contradiction pre-flight check (opt-in)
autocoder SHALL provide an opt-in pre-flight check that detects semantic contradictions among the requirements WITHIN a single OpenSpec change before the executor is invoked. The check runs a CLI-wrapped agentic session through the shared `agentic_run` primitive (a56) in a read-only sandbox that reads the change's spec-delta files on demand AND returns a structured listing of contradictions (requirements that cannot all hold simultaneously) via the `submit_contradictions` MCP tool. On non-empty findings, autocoder SHALL write `.needs-spec-revision.json` with `revision_suggestion` populated from the contradictions narrative, post the existing `AlertCategory::SpecNeedsRevision` chatops alert, AND halt the queue walk for this iteration. The executor SHALL NOT be invoked when contradictions are found.

The check SHALL be gated by `executor.change_internal_contradiction_check` (`disabled` default, `enabled` opt-in). The model is configured via `executor.change_internal_contradiction_check_llm` (parallel to the `reviewer:` config block — provider, model, api_key source, optional api_base_url), which a56's CLI strategy translates into the wrapped CLI's model-selection mechanism. The `claude` strategy reaches only Anthropic-shaped endpoints; a model whose provider resolves to a CLI with no registered strategy makes the check FAIL CLOSED (per the fail-closed posture below) until that strategy is registered.  Enabling the check without configuring the model SHALL fail at daemon startup with a fail-fast validation error.

The check SHALL FAIL CLOSED (gatekeepers-fail-closed standard): an agentic-session error (spawn, timeout, OR a resolved CLI strategy that is not registered), a schema-rejected submission the agent never corrects, a session that ends with no submission, OR any other could-not-run failure SHALL NOT be treated as "no contradictions found." It SHALL log a WARN AND hold the change in an explicit failed-to-run state: write `.needs-spec-revision.json` with a structured `gate_error` population (the gate label AND the cause) distinct from a findings-based revision, post a distinct "gate FAILED TO RUN — change held" chatops alert (under `AlertCategory::SpecNeedsRevision`), AND halt the queue walk. The change is held because it was NOT evaluated — NOT because a problem was found; an operator clears the marker (after fixing the gate) to retry. A schema-invalid `submit_contradictions` call mid-session is a correctable tool error the agent can retry (a56). A successful session that returns an empty array is a clean result AND proceeds to the executor.

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
- **AND** the marker's `unarchivable_deltas`, `unimplementable_tasks`, AND `gate_error` populations are empty (this case is semantic findings, not structural AND not a gate error)
- **AND** the chatops alert under `AlertCategory::SpecNeedsRevision` fires (subject to the 24h throttle)
- **AND** the executor is NOT invoked for this change OR any subsequent change in this iteration

#### Scenario: Session failure holds the change (fail closed)
- **WHEN** the agentic session fails (spawn error, timeout, OR the resolved CLI strategy is not registered — e.g. a non-`claude` command whose strategy has not been added)
- **THEN** the pipeline logs a WARN (carrying the `[in]` label) naming the cause
- **AND** writes `.needs-spec-revision.json` with a structured `gate_error` (gate label + cause), NOT a "no contradictions found" result
- **AND** posts a distinct "gate FAILED TO RUN — change held" chatops alert
- **AND** the executor is NOT invoked — the change is held until an operator clears the marker

#### Scenario: No valid submission holds the change (fail closed)
- **WHEN** the session ends with no schema-valid `submit_contradictions` call (the agent never submits, OR every submission is schema-rejected and never corrected)
- **THEN** the pipeline logs a WARN (carrying the `[in]` label) with a truncated session-output excerpt
- **AND** writes the `.needs-spec-revision.json` marker with a `gate_error` population AND halts the iteration (the same fail-closed hold)

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

### Requirement: Change-vs-canonical contradiction pre-flight check (the [canon] gate)
autocoder SHALL provide an opt-in pre-flight check — the `[canon]` gate of the verifier framework — that detects semantic contradictions between a single OpenSpec change's spec deltas AND the project's EXISTING canonical specs, before the executor is invoked. The check runs a CLI-wrapped agentic session through the shared `agentic_run` primitive (a56) in a read-only sandbox that reads the change's spec-delta files AND the canonical specs on demand, AND returns its findings via the `submit_canon_contradictions` MCP tool. On non-empty findings, autocoder SHALL write `.needs-spec-revision.json` with `revision_suggestion` populated from the canon-contradiction narrative, post the existing `AlertCategory::SpecNeedsRevision` chatops alert, AND halt the queue walk for this iteration. The executor SHALL NOT be invoked when contradictions are found. The gate's disposition is identical to the `[in]` gate's; the gates differ only in what they read (deltas-only vs deltas-plus-canon) AND what each finding names.

The check SHALL be gated by `executor.change_canonical_contradiction_check` (`disabled` default, `enabled` opt-in). The model is configured via `executor.change_canonical_contradiction_check_llm` (parallel to the `[in]` gate's block), which a56's CLI strategy translates into the wrapped CLI's model-selection mechanism. Enabling the check without configuring the model SHALL fail at daemon startup with a fail-fast validation error.

Canon access SHALL follow the documentation-audit pattern: the gate reads `openspec/specs/*/spec.md` directly through the sandbox AND additionally uses the `query_canonical_specs` MCP tool when a21's RAG is enabled (focused retrieval for large canon). The gate SHALL function correctly with OR without RAG.

Per the verifier framework, the `[canon]` gate SHALL FAIL CLOSED (gatekeepers-fail-closed standard) AND SHALL label its diagnostics with the `[canon]` identifier: an agentic-session error (spawn, timeout, OR a resolved CLI strategy that is not registered), a schema-rejected submission the agent never corrects, a session that ends with no submission, OR any other could-not-run failure SHALL NOT be treated as "no contradictions found." It SHALL log a WARN AND hold the change in an explicit failed-to-run state — write `.needs-spec-revision.json` with a structured `gate_error` population, post a distinct "gate FAILED TO RUN — change held" alert, AND halt the iteration; an operator clears the marker (after fixing the gate) to retry. A schema-invalid `submit_canon_contradictions` call mid-session is a correctable tool error the agent can retry (a56). A successful session that returns an empty array is a clean result AND proceeds to the executor.

#### Scenario: Default-disabled produces no [canon] session
- **WHEN** `executor.change_canonical_contradiction_check` is unset (default `disabled`)
- **AND** any change reaches the pre-executor pipeline
- **THEN** no `[canon]` session is spawned
- **AND** the executor is invoked normally (assuming the earlier gates passed)

#### Scenario: Enabled mode checks the deltas against canon
- **WHEN** `executor.change_canonical_contradiction_check: enabled` AND the model config is set
- **AND** a change reaches the pre-executor pipeline
- **THEN** the gate runs an `agentic_run` session (a56) in a read-only sandbox (`Read`/`Glob`/`Grep`, `ORCH_MCP_ROLE = canon_contradiction_check`, the `submit_canon_contradictions` MCP tool) with the embedded `prompts/change-vs-canonical-check.md` prompt (OR the configured override)
- **AND** the agent reads the change's spec-delta files AND the canonical specs on demand AND returns contradictions by calling `submit_canon_contradictions` with `{ contradictions: [{ change_requirement, canonical_capability, canonical_requirement, summary }] }`

#### Scenario: Empty submission proceeds to executor
- **WHEN** the agent calls `submit_canon_contradictions` with an empty `contradictions` array
- **THEN** the pipeline proceeds to the executor
- **AND** no marker is written AND no chatops alert fires

#### Scenario: Non-empty submission writes marker and halts
- **WHEN** the agent submits one or more change-vs-canonical contradictions
- **THEN** the pipeline writes `.needs-spec-revision.json` with `revision_suggestion` text populated from the contradictions narrative (each finding naming the conflicting canonical requirement)
- **AND** the marker's structural arrays (`unarchivable_deltas`, `unimplementable_tasks`) AND the `gate_error` population are empty (this case is semantic findings)
- **AND** the chatops alert under `AlertCategory::SpecNeedsRevision` fires (subject to the throttle)
- **AND** the executor is NOT invoked for this change OR any subsequent change in this iteration

#### Scenario: Runs with and without a21 RAG
- **WHEN** a21's `canonical_rag` is enabled AND the gate runs
- **THEN** the session has `query_canonical_specs` available AND the prompt MAY use it for focused canonical retrieval
- **WHEN** `canonical_rag` is disabled AND the gate runs
- **THEN** the gate reads canon directly via the sandbox's `Read` of `openspec/specs/*/spec.md` AND still produces valid findings

#### Scenario: Session failure holds the change (fail closed)
- **WHEN** the agentic session fails (spawn error, timeout, OR the resolved CLI strategy is not registered)
- **THEN** the gate logs a WARN (carrying the `[canon]` label) naming the cause
- **AND** writes `.needs-spec-revision.json` with a structured `gate_error`, posts the "gate FAILED TO RUN — change held" alert, AND does NOT proceed to the executor

#### Scenario: No valid submission holds the change (fail closed)
- **WHEN** the session ends with no schema-valid `submit_canon_contradictions` call (never submitted, OR every submission schema-rejected and never corrected)
- **THEN** the gate logs a WARN (carrying the `[canon]` label) with a truncated session-output excerpt
- **AND** writes the `gate_error` hold marker AND halts the iteration (the same fail-closed hold)

#### Scenario: Enabled without model config fails fast at startup
- **WHEN** `config.yaml` sets `executor.change_canonical_contradiction_check: enabled`
- **AND** `executor.change_canonical_contradiction_check_llm` is unset
- **THEN** daemon startup fails with a named error AND does NOT begin polling
- **AND** the operator sees the error on stderr AND in journalctl

### Requirement: Code-implements-spec verification (the [out] gate, advisory)
autocoder SHALL provide an opt-in post-executor check — the `[out]` gate of the verifier framework — that judges whether the executor's implementation satisfies the change's spec delta, requirement by requirement AND scenario by scenario. This is the verifier step the code-reviewer requirement defers to ("Do NOT assess whether the diff implements the spec; that is handled separately by the verifier step"). The gate runs a CLI-wrapped agentic session through the shared `agentic_run` primitive (a56) AFTER the executor implements the change, in a read-only sandbox that reads the spec delta, the diff, AND source on demand, AND returns its verdict via the `submit_verdict` MCP tool.

The gate SHALL be advisory: it annotates AND never auto-acts. It renders the verdict as a `## Spec Verification` section in the PR body (parallel to the reviewer's `## Code Review` block) AND posts a chatops note ONLY when gaps are found. It SHALL NEVER open a revision AND SHALL NEVER block PR creation. Per the gatekeepers-fail-closed standard, the gate fails CLOSED to a VISIBLE state rather than silence: a gate failure (session error, a resolved CLI strategy that is not registered, a schema-rejected submission never corrected, OR no submission) logs a WARN carrying the `[out]` label AND renders an explicit `## Spec Verification: FAILED TO RUN` section naming the cause — making clear the change was NOT verified (NOT a pass) — rather than omitting the section. It still never blocks PR creation. A schema-invalid `submit_verdict` call mid-session is a correctable tool error the agent can retry (a56).

The check SHALL be gated by `executor.code_implements_spec_check` (`disabled` default, `enabled` opt-in). The model is configured via `executor.code_implements_spec_check_llm`, which a56's CLI strategy translates into the wrapped CLI's model-selection mechanism. Enabling the check without configuring the model SHALL fail at daemon startup with a fail-fast validation error.

#### Scenario: Default-disabled produces no [out] session
- **WHEN** `executor.code_implements_spec_check` is unset (default `disabled`)
- **AND** the executor implements a change
- **THEN** no `[out]` session is spawned AND PR assembly is unchanged

#### Scenario: Enabled mode verifies the implementation against the spec
- **WHEN** `executor.code_implements_spec_check: enabled` AND the model config is set
- **AND** the executor has implemented a change
- **THEN** the gate runs an `agentic_run` session (a56) in a read-only sandbox (`Read`/`Glob`/`Grep`, `ORCH_MCP_ROLE = code_implements_spec`, the `submit_verdict` MCP tool) with the embedded `prompts/code-implements-spec-check.md` prompt (OR the configured override), carrying the spec-delta files, the unified diff, AND the changed-file list
- **AND** the agent reads source on demand AND returns its verdict by calling `submit_verdict` with `{ verdict, summary, gaps }`

#### Scenario: Implemented verdict renders a clean section, no chatops
- **WHEN** the agent submits `{ verdict: "implemented", ... }`
- **THEN** the PR body's `## Spec Verification` section reports the implementation as complete
- **AND** no chatops note is posted
- **AND** no revision is opened AND PR creation proceeds normally

#### Scenario: Gaps-found verdict annotates and notifies but never acts
- **WHEN** the agent submits `{ verdict: "gaps_found", gaps: [ ... ] }`
- **THEN** the PR body's `## Spec Verification` section lists each gap (`requirement`, optional `scenario`, `status`, `evidence`)
- **AND** a chatops note is posted as an advisory heads-up
- **AND** NO revision is opened AND PR creation is NOT blocked — the operator decides what to do

#### Scenario: Gate failure renders FAILED TO RUN, never blocking
- **WHEN** the agentic session fails (spawn error, timeout, unregistered strategy, OR no valid `submit_verdict`)
- **THEN** the gate logs a WARN carrying the `[out]` label
- **AND** renders an explicit `## Spec Verification: FAILED TO RUN` section naming the cause (the change is NOT verified — NOT a pass), rather than omitting the section
- **AND** PR creation proceeds — the gate never blocks

#### Scenario: Enabled without model config fails fast at startup
- **WHEN** `config.yaml` sets `executor.code_implements_spec_check: enabled`
- **AND** `executor.code_implements_spec_check_llm` is unset
- **THEN** daemon startup fails with a named error AND does NOT begin polling
- **AND** the operator sees the error on stderr AND in journalctl

## ADDED Requirements

### Requirement: Gate dispositions are enforced by a default-deny verdict ledger rendered in the PR
The verifier gates' fail-closed disposition SHALL be enforced **structurally** — by a per-change gate-verdict ledger whose default is non-passing — NOT by per-path inspection of a gate's result. Inspection requires every code path (every result arm, every error, every future early-return) to be classified correctly; a single missed path inherits whatever the fall-through is, which is how a gate silently fails open. A default-deny ledger removes that class of bug: "open" requires an affirmative, completed `PASS`, so a crash, an unhandled path, or a runner that never ran leaves the change held by construction.

For each change under gate evaluation, every gate slot (`[in]`, `[canon]`, `[out]`) SHALL have a verdict in the ledger, INITIALIZED to `PENDING` (a non-passing state). A verdict SHALL become `PASS` ONLY by an explicit, completed clean result. The verdict set is: `PENDING` (default — a runner that never recorded a verdict; treated as held), `PASS` (ran, clean), `FAIL` (ran, findings), `FAILED_TO_RUN` (ran, could not produce a verdict), `DISABLED` (gate not configured; non-blocking).

There SHALL be no skip/absent code path for a gate slot: every slot — whether its gate is enabled OR disabled — SHALL run a runner that affirmatively writes a verdict. A disabled gate's runner is a STUB that writes `DISABLED`. This eliminates the disabled-vs-failed ambiguity at the structural level — "disabled" is an explicit recorded verdict, never an absence that a reader must remember to treat as a pass.

The executor SHALL be invoked ONLY when every BLOCKING gate (`[in]`, `[canon]`) is `PASS` or `DISABLED`. A blocking gate that is `PENDING`, `FAIL`, or `FAILED_TO_RUN` SHALL hold the change. Because the default is `PENDING`, any failure to affirmatively record `PASS` holds the change without the holding code having to anticipate the specific failure.

The ledger SHALL be rendered into the PR body as a compliance record: per gate, its identifier, the model that ran it, AND its verdict (with a one-line summary for `FAIL` / `FAILED_TO_RUN`). A `PASS` is therefore VISIBLE in the PR — the operator can see which gate ran, with which model, and that it passed — rather than inferred from the silent absence of an alert. The agentic reviewer's verdict SHALL likewise appear in the PR record.

#### Scenario: A blocking gate left PENDING holds the change
- **WHEN** a blocking gate's runner does not record a verdict (it crashes, an unhandled path is taken, or it never runs) so the ledger entry remains `PENDING`
- **THEN** the change is HELD (the executor is NOT invoked) — `PENDING` is non-passing by construction
- **AND** no code path needs to anticipate the specific failure for the hold to occur

#### Scenario: A disabled gate records DISABLED via a stub
- **WHEN** a gate is not configured (disabled)
- **THEN** its slot's stub runner records `DISABLED` (a non-blocking verdict), NOT an absence
- **AND** the executor proceeds (a disabled gate does not hold the change)

#### Scenario: The executor runs only when blocking gates are PASS or DISABLED
- **WHEN** the gate ledger for a change is evaluated before the executor
- **THEN** the executor is invoked ONLY if every blocking gate (`[in]`, `[canon]`) is `PASS` or `DISABLED`
- **AND** any blocking gate that is `PENDING`, `FAIL`, or `FAILED_TO_RUN` holds the change

#### Scenario: The PR body renders the gate ledger as a compliance record
- **WHEN** a change reaches PR creation
- **THEN** the PR body contains a gate-verdict section listing, per gate, its identifier, the model that ran it, AND its verdict
- **AND** a `PASS` is visible there (not inferred from silence), so an operator can judge whether a verdict came from a model they trust
