## ADDED Requirements

### Requirement: Per-execution MCP child exposes `outcome_request_iteration` tool

The per-execution stdio MCP server SHALL advertise an `outcome_request_iteration` tool alongside `outcome_success` AND `outcome_spec_needs_revision` (added in `a27a0`).

- Name: `outcome_request_iteration`.
- Purpose (operator-facing summary, also documented in the bundled implementer prompt): the agent has completed some tasks AND wants another iteration to finish the rest. NOT for unimplementable tasks (use `outcome_spec_needs_revision` for those).
- Input schema: `{ completed_tasks: Array<string>, remaining_tasks: Array<string>, reason: string }`. All three fields required. Both arrays SHALL be non-empty. Every array element SHALL be a non-empty string. `reason` SHALL be non-empty. NO string field (top-level, array element, or otherwise) may contain a `<...>`-shaped substring (the same placeholder-detection refinement applied to `outcome_spec_needs_revision`).
- Output on success: `{ ok: true }`. On any input-validation failure, the MCP layer returns a JSON-RPC error code `-32602` (invalid params) with a `message` naming the offending field AND the specific failure mode. The control socket is NOT contacted on validation failure. The wrapped agent receives the error AND can retry the tool call with corrected fields in the same session.

The tool's handler SHALL relay validated input to the daemon via the existing `record_outcome` control-socket action using the `iteration_request` variant tag (per the orchestrator-cli deltas in this change).

#### Scenario: Tool advertised in `tools/list`
- **WHEN** an agent sends a `tools/list` request to the MCP child
- **THEN** the response lists `outcome_request_iteration` alongside `outcome_success`, `outcome_spec_needs_revision`, `ask_user`, AND `query_canonical_specs`
- **AND** the tool's `inputSchema` matches the documented `{ completed_tasks, remaining_tasks, reason }` shape

#### Scenario: Valid invocation relays to daemon
- **WHEN** an agent invokes `outcome_request_iteration({ completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "task 3 needs a refactor I want to plan more carefully" })`
- **THEN** the MCP layer validates the input successfully
- **AND** relays a `record_outcome` control-socket action with the `iteration_request` variant AND the input fields
- **AND** returns `{ ok: true }` to the agent

#### Scenario: Empty `completed_tasks` rejected at MCP layer
- **WHEN** an agent invokes `outcome_request_iteration({ completed_tasks: [], remaining_tasks: ["3"], reason: "..." })`
- **THEN** the MCP layer returns JSON-RPC error code `-32602` with a `message` naming `completed_tasks` as empty
- **AND** the control socket is NOT contacted

#### Scenario: Empty `remaining_tasks` rejected at MCP layer
- **WHEN** an agent invokes `outcome_request_iteration({ completed_tasks: ["1"], remaining_tasks: [], reason: "..." })`
- **THEN** the MCP layer returns JSON-RPC error code `-32602` with a `message` naming `remaining_tasks` as empty
- **AND** the control socket is NOT contacted

#### Scenario: Placeholder-shaped string rejected at MCP layer
- **WHEN** an agent invokes `outcome_request_iteration({ completed_tasks: ["1"], remaining_tasks: ["3"], reason: "<concrete blocker>" })`
- **THEN** the MCP layer returns JSON-RPC error code `-32602` with a `message` naming `reason` AND the placeholder-shaped failure mode
- **AND** the control socket is NOT contacted

### Requirement: `ExecutorOutcome::IterationRequested` variant carries cumulative state AND the next iteration number

The `ExecutorOutcome` enum (per the canonical executor architecture spec) SHALL gain an `IterationRequested { completed_tasks: Vec<String>, remaining_tasks: Vec<String>, reason: String, iteration_number: u32 }` variant.

- `completed_tasks` AND `remaining_tasks` carry the agent's cumulative-as-of-this-iteration lists verbatim from the recorded outcome.
- `reason` carries the agent's stated blocker verbatim.
- `iteration_number` is the iteration number the NEXT polling cycle will observe AND inject into the next iteration's prompt. The classifier computes it as `prior_iteration_number + 1` where `prior_iteration_number` comes from the workspace's `.iteration-pending.json` marker (0 when no marker is present, so the first request produces `iteration_number: 2` — meaning "the upcoming iteration is the 2nd").

Downstream polling-loop code that branches on `ExecutorOutcome` SHALL handle the new variant per the orchestrator-cli deltas in this change.

#### Scenario: First iteration request produces iteration_number 2
- **WHEN** the classifier consumes a recorded `iteration_request` outcome AND the workspace has no `.iteration-pending.json` marker
- **THEN** the returned `ExecutorOutcome::IterationRequested` has `iteration_number: 2`

#### Scenario: Subsequent iteration request increments the count
- **WHEN** the classifier consumes a recorded `iteration_request` outcome AND the workspace's marker file shows `iteration_number: 3`
- **THEN** the returned `ExecutorOutcome::IterationRequested` has `iteration_number: 4`

### Requirement: Classifier enforces iteration cap of 5

Before mapping a recorded `iteration_request` outcome to `ExecutorOutcome::IterationRequested`, the classifier SHALL compute the prospective `iteration_number` (per the rule above) AND check it against the iteration cap of 5.

When `iteration_number > 5`, the classifier SHALL:

- Emit `tracing::warn!` naming the change AND the cap.
- Return `ExecutorOutcome::Failed { reason: "exceeded iteration-request cap (5); WIP on agent branch — review or restart from scratch" }` (exact wording REQUIRED so operators can grep AND scripts can match).
- NOT modify, replace, OR delete the `.iteration-pending.json` marker file. The marker's preservation lets the operator inspect cumulative state for triage.

The cap is fixed at 5 in this change. A future spec MAY make it configurable; doing so does NOT require revising this requirement (the requirement binds the implementation-default cap AND the override semantics).

#### Scenario: 5th iteration is permitted
- **WHEN** the classifier consumes a recorded `iteration_request` outcome AND the workspace's marker file shows `iteration_number: 4`
- **THEN** the classifier computes `iteration_number: 5` AND returns `ExecutorOutcome::IterationRequested` (the 5th iteration runs)

#### Scenario: 6th iteration is capped
- **WHEN** the classifier consumes a recorded `iteration_request` outcome AND the workspace's marker file shows `iteration_number: 5`
- **THEN** the classifier returns `ExecutorOutcome::Failed { reason: "exceeded iteration-request cap (5); WIP on agent branch — review or restart from scratch" }`
- **AND** the `.iteration-pending.json` marker file is preserved unchanged
- **AND** the `tracing::warn!` log line names the change AND the cap

#### Scenario: Cap counts span multiple subprocess runs
- **WHEN** a change has gone through iteration_request outcomes in iterations 1, 2, 3, AND 4 (each producing a marker file with the corresponding incremented number)
- **AND** iteration 5 runs successfully (the agent calls `outcome_success`)
- **THEN** the marker file is deleted (per the lifecycle requirement below) AND the iteration sequence terminates without hitting the cap

### Requirement: Iteration-pending marker file in the change directory carries state across iteration boundaries

When the polling loop handles an `ExecutorOutcome::IterationRequested`, it SHALL write the marker file `<workspace>/openspec/changes/<change>/.iteration-pending.json` AFTER successfully committing AND force-pushing the WIP to the agent branch.

Marker file shape:

```json
{
  "completed_tasks": ["1", "2"],
  "remaining_tasks": ["3"],
  "reason": "task 3 needs a refactor I want to plan more carefully",
  "iteration_number": 2
}
```

Marker write SHALL use atomic tempfile + rename to avoid partial-write corruption (the same pattern `mcp_askuser_server::write_marker` uses for `.askuser-pending.json`).

Marker lifecycle in each `ExecutorOutcome` arm:

- `IterationRequested`: write/replace marker with the new iteration's cumulative state AND incremented iteration_number (after WIP commit + push).
- `Completed`: delete marker after WIP commit + push completes successfully. Deletion is idempotent (no error if marker absent).
- `SpecNeedsRevision`: delete marker. The iteration sequence is conceptually terminated; operator action is required.
- `Failed`: leave marker untouched. A subsequent retry of the same change preserves the continuation context.
- `AskUser`: leave marker untouched. The agent's question may resolve into a continuation.

The marker is filesystem-inspectable (`ls -a <workspace>/openspec/changes/<change>/`) for operators debugging an in-progress iteration sequence.

#### Scenario: Marker written on IterationRequested AFTER successful push
- **WHEN** the polling loop handles `ExecutorOutcome::IterationRequested { completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "...", iteration_number: 2 }`
- **AND** the WIP commit AND push to the agent branch both succeed
- **THEN** `.iteration-pending.json` is written atomically AND contains the documented fields with `iteration_number: 2`

#### Scenario: Marker deleted on Completed
- **WHEN** the polling loop handles `ExecutorOutcome::Completed` for a change whose `.iteration-pending.json` is present
- **AND** the WIP commit AND push complete successfully
- **THEN** `.iteration-pending.json` is deleted
- **AND** subsequent operator inspection of the change directory shows no marker

#### Scenario: Marker preserved on Failed
- **WHEN** the polling loop handles `ExecutorOutcome::Failed { reason: "timeout" }` (OR any other Failed reason) for a change whose `.iteration-pending.json` is present
- **THEN** the marker is NOT deleted
- **AND** the next polling iteration that processes this change sees the marker AND injects continuation context

#### Scenario: Marker NOT written if push fails
- **WHEN** the polling loop handles `ExecutorOutcome::IterationRequested` AND the force-push to the agent branch fails
- **THEN** `.iteration-pending.json` is NOT written
- **AND** the polling loop emits `tracing::error!` naming the push failure
- **AND** the change reverts to normal pending behavior on the next polling cycle (no front-insertion preference, no continuation context)

### Requirement: Implementer prompt includes a "Prior iteration summary" block when an iteration-pending marker is present

The bundled `prompts/implementer.md` rendering pipeline SHALL read `<workspace>/openspec/changes/<change>/.iteration-pending.json` at prompt-build time. When the marker is present AND parseable:

- The rendered prompt SHALL append a "Prior iteration summary" block AFTER the change body (NOT before — placement is load-bearing per the design rationale).
- The block SHALL contain the marker's cumulative `completed_tasks`, `remaining_tasks`, `reason`, AND `iteration_number` verbatim.
- The block SHALL frame the prior state as already-done (the agent does NOT re-implement completed tasks).
- The block SHALL instruct the agent to re-evaluate the prior blocker with fresh eyes (do NOT inherit the prior pessimism).
- The block SHALL name the cap (`Current iteration: N of 5`) so the agent knows the channel is finite.
- The block SHALL direct the agent to call `outcome_success` at end-of-run when remaining tasks are done OR `outcome_request_iteration` again with updated cumulative state if another iteration is honestly needed.

Block content (canonical text the bundled prompt SHALL produce; substitution of `<list>`, `<reason>`, `N` with marker values is required):

```
--- BEGIN PRIOR ITERATION SUMMARY ---

A previous iteration of this same change reached a structured stopping
point. Your job is to overcome the prior blocker AND finish the
remaining tasks. The previous iteration's working tree has already been
committed AND pushed to the agent branch — your starting state already
includes its progress.

Cumulative completed (do NOT re-implement): <completed_tasks>
Remaining: <remaining_tasks>
Prior iteration's stated reason for stopping: <reason>
Current iteration: N of 5 (cap)

Do NOT assume the prior reason still holds. Re-evaluate the blocker
with fresh eyes — the prior iteration's model may have miscalibrated
the scope, AND a different angle of attack may resolve the work in
this iteration. If you genuinely cannot finish in this iteration,
call outcome_request_iteration again with an updated cumulative state
AND a more specific reason. Note that the iteration cap is 5; runs
beyond that are auto-failed.

--- END PRIOR ITERATION SUMMARY ---
```

When the marker is absent, the prompt is built as today with no continuation block. The first-iteration prompt's shape is unchanged.

When the marker is present BUT corrupt (truncated JSON, missing required field, parse failure), the prompt-builder SHALL:

- Emit `tracing::warn!` naming the change AND the corruption mode.
- Fall back to building the prompt as if no marker were present (no continuation block).
- Leave the corrupt marker on disk (operator can inspect AND repair OR delete).

Operator-customizable override prompts (loaded via the uniform `PromptLoader` per `a24`'s spec) MAY use any structure the operator prefers — the canonical rule binds the bundled default only.

The `outcome_request_iteration` tool SHALL be named in the prompt's "Outcome tools" section (added in `a27a0`) alongside `outcome_success` AND `outcome_spec_needs_revision`. Each tool's one-line purpose AND when-to-use guidance is sufficient; full schemas remain in the MCP `tools/list` response per a27a0's documentation discipline.

#### Scenario: Continuation block injected when marker is present
- **WHEN** the prompt-builder runs for a change whose `.iteration-pending.json` contains `{ completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "task 3 needs a refactor I want to plan more carefully", iteration_number: 2 }`
- **THEN** the rendered prompt contains the "Prior iteration summary" block AFTER the change body
- **AND** the block contains `Cumulative completed (do NOT re-implement): 1, 2`
- **AND** the block contains `Remaining: 3`
- **AND** the block contains `Prior iteration's stated reason for stopping: task 3 needs a refactor I want to plan more carefully`
- **AND** the block contains `Current iteration: 2 of 5 (cap)`

#### Scenario: First-iteration prompt has no continuation block
- **WHEN** the prompt-builder runs for a change whose `.iteration-pending.json` does NOT exist
- **THEN** the rendered prompt is built as today with no continuation block
- **AND** the prompt's shape matches the pre-spec first-iteration shape verbatim

#### Scenario: Corrupt marker is logged AND ignored
- **WHEN** the prompt-builder runs for a change whose `.iteration-pending.json` is truncated mid-JSON
- **THEN** a `tracing::warn!` log line names the change AND the corruption
- **AND** the rendered prompt has no continuation block
- **AND** the corrupt marker file is NOT modified OR deleted by the prompt-builder

#### Scenario: Bundled prompt names the new outcome tool
- **WHEN** a maintainer reads `prompts/implementer.md`'s "Outcome tools" section
- **THEN** `outcome_request_iteration` is named alongside `outcome_success` AND `outcome_spec_needs_revision`
- **AND** the section gives a one-line purpose ("you started implementation but want another iteration to finish — NOT for unimplementable tasks") for the new tool
