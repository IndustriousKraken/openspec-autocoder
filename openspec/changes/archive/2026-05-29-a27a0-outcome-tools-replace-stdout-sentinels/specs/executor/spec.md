## ADDED Requirements

### Requirement: Per-execution MCP child exposes outcome tools via control-socket relay

The per-execution stdio MCP server (the child process autocoder launches per polling iteration via `.mcp.json`, currently `autocoder/src/mcp_askuser_server.rs`) SHALL advertise two outcome-signaling tools alongside the existing `ask_user` AND `query_canonical_specs` tools:

- **`outcome_success`** — the implementer's explicit successful-completion signal.
  - Input schema: `{ final_answer?: string }`. The optional `final_answer` carries the implementer's end-of-run summary text (the content that today's JSON-streaming `result` event provides) for log capture AND PR-comment rendering. When omitted, the daemon uses an empty string.
  - Output: a JSON object `{ ok: true }`. The agent does NOT need to inspect the result; calling the tool IS the signal.
- **`outcome_spec_needs_revision`** — the implementer's "this change names tasks I cannot complete in this sandbox" signal (the same semantic as the legacy `=== AUTOCODER-OUTCOME ===` `spec_needs_revision` payload).
  - Input schema: `{ unimplementable_tasks: Array<{ task_id: string, task_text: string, reason: string }>, revision_suggestion: string }`. All fields required. `unimplementable_tasks` SHALL be non-empty. NO string field may contain a `<...>`-shaped substring (the placeholder-detection refinement).
  - Output: a JSON object `{ ok: true }` on success; on validation failure, the MCP layer returns a JSON-RPC error code `-32602` (invalid params) with a `message` naming the offending field AND the specific failure mode (missing, empty, wrong type, placeholder-shaped). The control socket is NOT contacted on validation failure.

Both tools' handlers SHALL NOT compute results locally. Instead they SHALL relay the input to the daemon via the existing control socket (per the canonical `orchestrator-cli` "Control socket for runtime daemon interaction" requirement) using a new `record_outcome` action defined in the orchestrator-cli spec deltas. The daemon owns the outcome store AND records the outcome; the MCP child is a thin synchronous relay.

The relay uses the same env-var contract as the existing `query_canonical_specs` tool: `ORCH_DAEMON_CONTROL_SOCKET` for the socket path, `ORCH_MCP_WORKSPACE` AND `ORCH_MCP_CHANGE` for the routing keys. The MCP child resolves `workspace_basename` from `ORCH_MCP_WORKSPACE_BASENAME` (already set by `ClaudeCliExecutor::write_mcp_config`).

Connection timeout: 10 seconds (the same constant the `ask_user` AND `query_canonical_specs` relays use). On socket error OR timeout, the MCP layer returns a JSON-RPC error code `-32603` (internal error) with a `message` naming the relay failure. The wrapped agent SHALL surface the error AND MAY retry the tool call in the same session.

Validation is performed AT THE MCP LAYER, NOT at the daemon's `record_outcome` handler. The MCP layer is in-process with the agent AND the only writer to the control socket for this action; two-layer validation would create maintenance cost without payoff. The daemon's handler trusts the relayed payload AND stores it.

#### Scenario: Both tools advertised in the MCP child's `tools/list`
- **WHEN** an agent connects to the MCP child AND sends a `tools/list` request
- **THEN** the response lists `ask_user`, `query_canonical_specs`, `outcome_success`, AND `outcome_spec_needs_revision`
- **AND** `outcome_success`'s `inputSchema` matches the documented `{ final_answer?: string }` shape
- **AND** `outcome_spec_needs_revision`'s `inputSchema` matches the documented `{ unimplementable_tasks: Array<...>, revision_suggestion: string }` shape

#### Scenario: `outcome_success` relays to daemon AND records outcome
- **WHEN** an agent invokes `outcome_success({ final_answer: "Implementation complete; all tests pass." })`
- **AND** `ORCH_DAEMON_CONTROL_SOCKET`, `ORCH_MCP_WORKSPACE_BASENAME`, AND `ORCH_MCP_CHANGE` are set in the child's env
- **THEN** the MCP child opens a connection to the socket AND sends a `record_outcome` action with the `Success` variant AND the relayed `final_answer`
- **AND** the daemon's handler returns `{"ok":true}`
- **AND** the MCP child returns `{ ok: true }` to the agent as the tool-call result

#### Scenario: `outcome_spec_needs_revision` validates input before relaying
- **WHEN** an agent invokes `outcome_spec_needs_revision({ unimplementable_tasks: [{ task_id: "6.4", task_text: "Manual: SSH into the production host...", reason: "executor sandbox has no real SSH credentials" }], revision_suggestion: "Replace task 6.4 with a unit test..." })`
- **THEN** the MCP layer validates the input AND finds no schema violation
- **AND** the MCP child relays a `record_outcome` action with the `SpecNeedsRevision` variant carrying the full payload
- **AND** the daemon returns `{"ok":true}`
- **AND** the MCP child returns `{ ok: true }` to the agent

#### Scenario: `outcome_spec_needs_revision` rejects placeholder-shaped strings at the MCP layer
- **WHEN** an agent invokes `outcome_spec_needs_revision({ unimplementable_tasks: [{ task_id: "<id-from-tasks-md>", task_text: "<verbatim quote>", reason: "<one-line reason>" }], revision_suggestion: "<concrete edit>" })`
- **THEN** the MCP layer returns a JSON-RPC error code `-32602` with a `message` naming the placeholder-shaped field
- **AND** the control socket is NOT contacted
- **AND** the daemon's outcome store remains unchanged
- **AND** the wrapped agent receives the tool-error result AND can retry the tool call with corrected fields in the same session

#### Scenario: `outcome_spec_needs_revision` rejects missing required field at the MCP layer
- **WHEN** an agent invokes `outcome_spec_needs_revision({ unimplementable_tasks: [{ task_id: "6.4", task_text: "Manual: SSH...", reason: "no SSH access" }] })` (missing `revision_suggestion`)
- **THEN** the MCP layer returns a JSON-RPC error code `-32602` with a `message` naming `revision_suggestion` as the missing field
- **AND** the control socket is NOT contacted

#### Scenario: Control-socket relay failure surfaces as tool error
- **WHEN** an agent invokes `outcome_success({ final_answer: "done" })`
- **AND** the daemon's control socket is unreachable (daemon not running, socket path invalid, etc.)
- **THEN** the MCP layer returns a JSON-RPC error code `-32603` with a `message` naming the relay failure
- **AND** the wrapped agent receives the tool-error result

### Requirement: Tool-recorded outcomes take precedence over all heuristic classification in `classify_outcome`

The executor's outcome-dispatch path (`classify_outcome` in the CLI-wrapping executor backend) SHALL consult the daemon's outcome store via a `consume_outcome` control-socket action BEFORE applying any other classification step. The ordering is:

1. **Tool-recorded outcome lookup** (NEW). The classifier sends a `consume_outcome` action keyed by `(workspace_basename, change)`. When the daemon returns a recorded outcome:
   - A `Success` variant maps to `ExecutorOutcome::Completed { final_answer }` using the recorded `final_answer`.
   - A `SpecNeedsRevision` variant maps to the existing `ExecutorOutcome::SpecNeedsRevision { ... }` shape.
   - The classifier returns the mapped outcome immediately. No further heuristic is applied.
2. **AskUser marker check** (UNCHANGED from today's behavior; only the ordering shifts — it now runs only when no outcome was tool-recorded).
3. **Timeout precedence** (UNCHANGED — the existing canonical "Timeout classification takes precedence over sentinel extraction" requirement governs this layer AND its scope-narrowing remains in force).
4. **Stdout-sentinel scan** (UNCHANGED in extraction behavior; gains an operator-visible deprecation warning per the requirement below).
5. **Exit-status path** (UNCHANGED).
6. **Layer-2 stdout heuristic + Completed fallback** (UNCHANGED).

The precedence rule is anchored in the semantics of the signal: a tool-recorded outcome is the agent's deliberate, schema-validated end-of-run emission. It is more authoritative than ANY inferred state (timeout flag, exit status, stdout content). A run that called an outcome tool AND subsequently timed out is classified by the outcome, not the timeout — the agent emitted its signal; the kill happened after.

When the daemon's `consume_outcome` action returns `None` (no outcome was recorded), the classifier proceeds to step 2 AND the existing behavior is preserved exactly. This is the path that all pre-a27a0 implementer prompts continue to take.

#### Scenario: Tool-recorded `Success` outcome takes precedence over stdout sentinel
- **WHEN** the classifier runs for a change whose daemon outcome store contains a `Success` outcome from a prior `outcome_success` tool call
- **AND** `outcome.stdout` ALSO contains a well-formed `=== AUTOCODER-OUTCOME ===` block with a `spec_needs_revision` payload (the worst-case ambiguity: both signals present)
- **THEN** the classifier returns `ExecutorOutcome::Completed { final_answer: <recorded final_answer> }`
- **AND** the stdout sentinel is NOT extracted, parsed, OR considered for the outcome
- **AND** the daemon's outcome store entry for this `(workspace_basename, change)` is cleared (drained by `consume_outcome`)

#### Scenario: Tool-recorded `SpecNeedsRevision` outcome takes precedence over timeout
- **WHEN** the classifier runs for a change whose daemon outcome store contains a `SpecNeedsRevision` outcome (the agent called `outcome_spec_needs_revision` AND then was killed by the wall-clock timeout before clean exit)
- **AND** `outcome.timed_out` is `true`
- **THEN** the classifier returns `ExecutorOutcome::SpecNeedsRevision { ... }` populated from the recorded payload
- **AND** the timeout flag is NOT used
- **AND** no `Failed { reason: "timeout" }` outcome is produced

#### Scenario: Absent tool-recorded outcome falls through to legacy classifier
- **WHEN** the classifier runs for a change whose daemon outcome store contains no entry (the agent did not call any outcome tool)
- **AND** `outcome.stdout` contains a well-formed `=== AUTOCODER-OUTCOME ===` block with a valid `spec_needs_revision` payload
- **AND** `outcome.timed_out` is `false`
- **THEN** the classifier's `consume_outcome` call returns `None`
- **AND** the classifier proceeds through the existing ordering (AskUser → timeout → stdout sentinel → exit)
- **AND** the stdout sentinel scan extracts the payload AND returns `ExecutorOutcome::SpecNeedsRevision { ... }` (the legacy path's exact behavior)
- **AND** the legacy-path deprecation warning is emitted per the requirement below

### Requirement: Legacy stdout-sentinel scan is deprecated; matches emit an operator-visible warning during the transition cycle

The stdout-sentinel scan (the `extract_outcome_sentinel` + `try_parse_spec_needs_revision` pair invoked from `classify_outcome`) SHALL remain functionally unchanged in this change for backward compatibility with the previous-cycle implementer prompt. When the scan actually matches AND returns a parsed `SpecNeedsRevision` outcome (the legacy path is taken because `consume_outcome` returned `None`), the classifier SHALL emit a `tracing::warn!` log line containing:

- The phrase `legacy stdout sentinel matched` (operator-greppable canonical marker).
- The change name.
- A directive sentence naming the canonical replacement tool: `please call the outcome_spec_needs_revision MCP tool instead`.
- The planned removal target: `(stdout sentinel parsing is scheduled for removal in a27a2)`.

The warning IS load-bearing: it produces operator-visible signal that an out-of-date implementer prompt is in use, which is the trigger for closing the deprecation window in a27a2. Operators MAY filter the warning by changing the log level if they accept the legacy behavior; the warning's continued emission for the cycle's duration is the intended operator-feedback channel.

The stdout-sentinel scan's extraction logic, JSON parsing logic, placeholder-detection logic, AND the parse-failure fallback to `Failed { reason: "..." }` are ALL unchanged. The only behavioral delta is the additional warning emission on successful match.

The deprecation is REMOVED in `a27a2`, at which point the stdout-sentinel scan's match path returns the same outcome but the warning is replaced by a hard error (the legacy path becomes unreachable; the scan's continued presence is dead code at that point, removed as a separate task in a27a2).

#### Scenario: Legacy stdout-sentinel match emits the deprecation warning
- **WHEN** the classifier's `consume_outcome` returns `None`
- **AND** `outcome.timed_out` is `false`
- **AND** the stdout-sentinel scan extracts a payload AND parses it successfully as `spec_needs_revision`
- **THEN** the classifier emits a `tracing::warn!` log line containing the phrase `legacy stdout sentinel matched`, the change name, the `please call the outcome_spec_needs_revision MCP tool instead` directive, AND the planned-removal-target note
- **AND** the returned outcome is `ExecutorOutcome::SpecNeedsRevision { ... }` (the legacy path's exact result)

#### Scenario: Legacy stdout-sentinel parse failure surfaces unchanged
- **WHEN** the classifier's `consume_outcome` returns `None`
- **AND** `outcome.timed_out` is `false`
- **AND** the stdout-sentinel scan extracts a payload BUT parsing fails (malformed JSON, placeholder-shaped strings, etc.)
- **THEN** the classifier returns `ExecutorOutcome::Failed { reason: "agent emitted unparseable SpecNeedsRevision sentinel: ..." }` (the existing behavior, verbatim)
- **AND** no deprecation warning is emitted (the warning is scoped to successful matches; a parse failure is its own failure mode)

### Requirement: Implementer prompt documents the outcome tools by name AND uses them as the canonical end-of-run signal

The bundled `prompts/implementer.md` template SHALL contain an "Outcome tools" section that:

- Names both outcome tools: `outcome_success` AND `outcome_spec_needs_revision`.
- Provides a one-line purpose statement for each tool.
- Directs the agent to call `outcome_success` (with the agent's end-of-run summary as `final_answer`) at the end of a successful implementation run, BEFORE exiting.
- Directs the agent to call `outcome_spec_needs_revision` (instead of emitting the `=== AUTOCODER-OUTCOME ===` stdout block) for the pre-flight unimplementable-task case.
- Notes that input-validation errors from the MCP tool are recoverable: the model receives the error as the tool-call result AND can retry the call with corrected fields in the same session.

The section SHALL NOT inline the full input schemas; the MCP `tools/list` response is the canonical schema source AND duplicating it in the prompt creates a maintenance hazard. Tool names + one-line purposes are sufficient: a model that knows the tool exists AND its purpose can attempt the call AND converge via tool-error feedback if its argument shape is wrong.

The pre-flight unimplementable-task section SHALL be rewritten to use `outcome_spec_needs_revision`. The substitution-instruction + worked-example + self-check-hint structure (per the existing canonical "Sentinel emission instructions in the implementer prompt include a concrete worked example AND a self-check hint" requirement) SHALL be preserved, but the worked example becomes a tool-call shape (a JSON object the agent passes to the tool) rather than a stdout block, AND the self-check hint references the MCP layer's input validation instead of the daemon's post-exit placeholder detection.

The existing `=== AUTOCODER-OUTCOME ===` stdout-sentinel section SHALL be retained for the deprecation cycle, prefixed with a "DEPRECATED" note that names `outcome_spec_needs_revision` as the canonical replacement AND `a27a2` as the planned removal target.

Operator-customizable override prompts (loaded via the uniform `PromptLoader` per `a24`'s spec) MAY use any structure the operator prefers — the canonical rule binds the bundled default only.

#### Scenario: Bundled prompt names both outcome tools
- **WHEN** a maintainer reads `prompts/implementer.md`
- **THEN** the prompt contains an "Outcome tools" section
- **AND** the section names both `outcome_success` AND `outcome_spec_needs_revision`
- **AND** each tool has a one-line purpose statement
- **AND** the section directs end-of-run `outcome_success` use AND pre-flight `outcome_spec_needs_revision` use

#### Scenario: Bundled prompt's outcome-tool example deserializes cleanly
- **WHEN** an automated test extracts any JSON-shaped example from the prompt's outcome-tool sections AND deserializes it into the corresponding tool-argument Rust type
- **THEN** the deserialization succeeds without error
- **AND** every string field contains a concrete value (no angle-bracket markers, no template variables)

#### Scenario: Existing stdout-sentinel section retained with DEPRECATED note
- **WHEN** a maintainer reads `prompts/implementer.md`
- **THEN** the existing `=== AUTOCODER-OUTCOME ===` sentinel section is still present (for the deprecation cycle)
- **AND** the section is prefixed with a "DEPRECATED" note naming `outcome_spec_needs_revision` as the canonical replacement
- **AND** the note names `a27a2` as the planned-removal target for the legacy path
