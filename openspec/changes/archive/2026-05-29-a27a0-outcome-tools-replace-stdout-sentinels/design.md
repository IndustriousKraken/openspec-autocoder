# Design

## Decisions to lock in

### D1. Extend the existing hand-rolled MCP server. Do NOT introduce a framework.

The codebase already hosts a hand-rolled stdio MCP server at `autocoder/src/mcp_askuser_server.rs` (578 lines, JSON-RPC 2.0 over stdio, `initialize` / `tools/list` / `tools/call` subset). It hosts the `ask_user` AND `query_canonical_specs` tools today. The relay-to-daemon-via-control-socket pattern is established AND tested.

This change adds two more tools (`outcome_success`, `outcome_spec_needs_revision`) using the same JSON-RPC handler dispatch AND the same control-socket relay primitive. The marginal surface is roughly:

- Two new entries in the `tools/list` response (one schema each).
- Two new branches in the `tools/call` `match call.name.as_str()` dispatch.
- One new control-socket action (`record_outcome`) with a per-payload-variant relay.

This is a sub-100-line addition. Anything heavier would be misjudged scope.

`Rig` is NOT in `autocoder/Cargo.toml` AND there is no MCP-server abstraction crate (`rmcp`, etc.) in the dependency tree. Adopting one would require porting the existing two tools to its abstractions, re-wiring the env-var-based configuration, re-wiring the control-socket relay, AND validating its protocol coverage against Claude Code's MCP client. The payoff would be marginal abstraction reuse for what is already a two-tool extension. The cost-benefit does not justify the rewrite at this scope. This decision is revisitable in a future change if the MCP server grows past ~10 tools OR if a second concrete agent backend (gemini-cli, codex, etc.) needs a different transport mode that an abstraction crate would simplify.

### D2. Per-process MCP relay, consolidated daemon state. Do NOT introduce a standalone MCP daemon.

The architecture established in `a21-canonical-spec-rag` (the revision that replaced an earlier centralized-MCP-server design with the per-process relay) already split this question: transport / tool schema / dispatch / isolation live in the per-process MCP child; the daemon owns shared state. Outcome tools fit the same pattern with zero additional architectural decisions.

Two consequences worth naming:

- **Authorization is implicit.** The MCP child knows which `(workspace, change)` it was launched for from its environment variables (`ORCH_MCP_WORKSPACE`, `ORCH_MCP_CHANGE`). It cannot relay an outcome for a different change. The daemon's `record_outcome` handler can trust the relayed key without additional auth.
- **Crash domain is one change.** A bug in the outcome-tool handler takes down one MCP child (which the wrapped agent observes as a tool error). Sibling executors in the same daemon are unaffected.

### D3. Daemon outcome store is in-memory, NOT a file marker.

The existing `ask_user` tool uses a file marker (`<workspace>/openspec/changes/<change>/.askuser-pending.json`) because the operator's answer is asynchronous AND may arrive across an autocoder restart. Outcome reporting is fundamentally different:

- The outcome tool call happens during the agent's emission phase, milliseconds before the wrapped CLI exits.
- The daemon's `classify_outcome` call happens in the same parent process, microseconds after the wrapped CLI exits.
- No restart-survives durability is required. No cross-process state-handoff is required.

An in-memory `Arc<Mutex<HashMap<(WorkspaceBasename, ChangeName), RecordedOutcome>>>` (OR equivalent — `RwLock` is also acceptable; the contention profile is single-writer-single-reader-per-key) lives on the daemon's shared-state struct, gets injected into the control-socket handlers, AND is drained by `consume_outcome`. Total state surface is ~20 lines.

### D4. Outcome precedence ordering: tool-recorded > AskUser marker > timeout > stdout-sentinel-fallback > exit-status.

The order matters AND has consequences:

- **Tool-recorded outcome before AskUser marker.** If the agent calls `outcome_success` AND `ask_user` in the same session, the deliberate end-of-run signal (the outcome tool) wins. This is the same precedence the agent would observe via tool-call ordering anyway; we just make it explicit at the classifier.
  - This contradicts today's classifier ordering, which checks the AskUser marker first. The contradiction is intentional: today, the only way the agent signals "I'm done" is exit; the AskUser marker is checked first because it's the only structured signal available. Once outcome tools exist, they ARE the structured "I'm done" signal AND must outrank the AskUser marker.
- **Tool-recorded outcome before timeout precedence.** A timed-out run that DID call an outcome tool clearly emitted its signal before the timeout fired; the signal is more authoritative than the wall-clock cutoff.
- **Timeout precedence before stdout-sentinel scan.** Preserved verbatim from a20a1. A timed-out run that did NOT call an outcome tool is classified as timeout, NOT scanned for stdout sentinels (which by definition cannot be deliberate emissions in a timed-out run).
- **Stdout-sentinel scan after timeout, before exit-status.** Preserved verbatim from the existing classifier for the one-cycle deprecation window.
- **Exit-status path unchanged** at the end.

### D5. Stdout-sentinel parser remains for one cycle. Removal is a27a2.

The deprecation window exists for two reasons:

- **Production servers running mid-stack.** An operator who deploys this change before deploying the updated implementer prompt receives a daemon that advertises new tools but a prompt that emits stdout sentinels. The deprecation window prevents this combination from regressing.
- **a27a2's recovery loop is the natural removal trigger.** Once a27a2 lands, the "model exited without calling an outcome tool" case has a structured recovery (re-prompt the same session, ask the model to call an outcome tool). At that point the stdout sentinel adds no resilience over the recovery loop AND can be removed.

The deprecation warning IS load-bearing: it produces operator-visible signal that the implementer prompt update is overdue, which is the trigger for closing the deprecation window in a27a2.

### D6. Schema validation lives in the MCP tool handler. Daemon trusts validated payloads.

Validation happens once, at the boundary, in the `tools/call` handler:

- `outcome_success`: optional `final_answer` MUST be a string if present; no further validation.
- `outcome_spec_needs_revision`: `unimplementable_tasks` MUST be a non-empty array of objects each having string `task_id`, `task_text`, AND `reason`; `revision_suggestion` MUST be a non-empty string; NO string field may contain a `<...>`-shaped substring (the existing placeholder-detection refinement, now applied at the MCP layer instead of the post-exit parser).

On validation failure, the MCP handler returns a JSON-RPC error with `code: -32602` (invalid params) AND a `message` naming the specific failure. The control socket is NOT contacted. The model sees the error in the tool result AND retries the tool call in the same session.

The daemon's `record_outcome` handler does NOT re-validate. The handler stores the relayed payload AND returns success. Two-layer validation would create maintenance cost without payoff; the MCP child is in-process with the agent AND the only writer to the control socket for this action.

### D7. The implementer prompt names the new tools but does NOT inline schemas.

The user raised the context-compression concern: if the harness evicts the `tools/list` response from working context, the model loses access to schema details. The mitigation is redundancy in the prompt, but ONLY at the granularity that actually helps:

- **Tool names and one-line purposes are in the prompt.** A model that knows `outcome_success` exists AND is for "successful completion" can call it AND, if its arguments are wrong, retry on the tool error. Knowing the tool exists is the load-bearing fact.
- **Full schemas are NOT in the prompt.** Duplicating the schema in two places creates a maintenance hazard (the prompt drifts from the canonical schema in `tools/list`). The MCP server's tool-error response is the canonical recovery path for schema mistakes.

The model's general training on MCP tool semantics handles "I don't know the exact shape; let me try a reasonable shape AND let the tool tell me if I'm wrong." This is exactly the same recovery pattern a developer would use against a poorly-documented API. Prompt redundancy gives the model the name to try; the MCP server's error message gives the model the shape to converge on.

## Migration path

The three-change stack adoption is intentional AND each change ships fully runnable:

- **a27a0 (this change):** new tools exist AND are preferred; legacy stdout sentinel still works with deprecation warning. Implementer prompt updated to instruct the new tools. A daemon running a27a0 with a stale implementer prompt continues to work via the legacy path. A daemon running the predecessor without a27a0 with the new implementer prompt sees `method not found` from the agent's tool calls AND falls through to legacy fallback semantics (degraded but not broken).
- **a27a1:** adds `outcome_request_iteration` + queue front-insertion + prior-iteration continuation block. Builds on a27a0's outcome-store + control-socket actions. Does NOT change a27a0's ordering or remove the legacy fallback.
- **a27a2:** adds the post-run acceptance scan + recovery loop. Removes the legacy stdout sentinel fallback at this point: by a27a2 the recovery loop covers the no-outcome-tool-call case AND the deprecation window has been long enough for any out-of-date implementer prompt to have rolled forward.

The stack is sequentially-dependent (a27a1 needs a27a0's outcome-store; a27a2 needs both) but a27a1 AND a27a2 are themselves independent of each other AND could ship in either order if implementation order favors it.

## Open questions for the implementer

- **`RecordedOutcome` representation.** The simplest shape is an enum mirroring the existing `ExecutorOutcome` variants (`Completed { final_answer }`, `SpecNeedsRevision { ... }`). The implementer SHOULD avoid leaking implementation-detail fields from `ExecutorOutcome` (e.g. `resume_handle` makes no sense at the outcome-record layer); a small dedicated enum is preferable to re-using `ExecutorOutcome` directly.
- **Outcome-store eviction.** The map COULD grow unboundedly if `consume_outcome` is never called for a recorded key (e.g. autocoder crashes between subprocess exit AND classify_outcome's drain). A periodic sweep that evicts entries older than a coarse threshold (60 minutes) is sufficient AND avoids any need for explicit TTL tracking. The implementer SHOULD add this if the store is shared across many concurrent changes; the implementer MAY defer it to a follow-on if the immediate scope feels narrow.
- **Tool-error error codes.** JSON-RPC `-32602` (invalid params) is the right code for schema-validation failures. The `message` field SHOULD name the specific failure. The MCP host (claude-cli) surfaces the error to the model verbatim; the model converges fastest when the message names the offending field AND the fix.
