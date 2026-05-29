# Tasks

## 1. Daemon outcome store + control-socket actions

- [x] 1.1 Define `RecordedOutcome` enum on the daemon side with variants matching the outcome-tool payloads (`Success { final_answer: Option<String> }` AND `SpecNeedsRevision { unimplementable_tasks: Vec<UnimplementableTask>, revision_suggestion: String }`).
- [x] 1.2 Add an `Arc<Mutex<HashMap<(String, String), RecordedOutcome>>>` (workspace_basename + change → outcome) to the daemon's shared-state struct. Construct at daemon startup. Inject into the control-socket handler context.
- [x] 1.3 Implement the `record_outcome` control-socket action. Payload shape: `{ "action": "record_outcome", "workspace_basename": "...", "change": "...", "outcome": { ... variant-tagged ... } }`. Handler writes to the store AND returns `{ "ok": true }`. Returns `{ "ok": false, "error": "..." }` on a malformed payload.
- [x] 1.4 Implement the `consume_outcome` control-socket action. Payload shape: `{ "action": "consume_outcome", "workspace_basename": "...", "change": "..." }`. Handler removes the entry from the store AND returns `{ "ok": true, "outcome": <RecordedOutcome or null> }`.
- [x] 1.5 Unit-test the store: `record_outcome` followed by `consume_outcome` returns the recorded payload AND clears the entry; a second `consume_outcome` for the same key returns `null`; `record_outcome` for an already-occupied key replaces the prior entry (last-writer-wins).
- [x] 1.6 Integration-test the control-socket actions end-to-end: a synthetic client sends `record_outcome`, then `consume_outcome`, AND observes the round-trip.

## 2. MCP outcome tools

- [x] 2.1 Add `outcome_success` AND `outcome_spec_needs_revision` entries to `mcp_askuser_server.rs`'s `tools/list` response. Names, descriptions, input schemas (JSON Schema fragments) as specified in the executor capability deltas.
- [x] 2.2 Add the `outcome_success` branch to the `tools/call` dispatch. Extract optional `final_answer` from arguments. Relay to the daemon via the existing `relay_to_control_socket` helper using a `record_outcome` action with a `Success` variant payload. Return MCP success on relay success; return MCP error code `-32603` (internal error) on relay failure with a clear message.
- [x] 2.3 Add the `outcome_spec_needs_revision` branch to the `tools/call` dispatch. Validate the input AT THE MCP LAYER:
- [x] 2.4 On valid input, relay to the daemon via `record_outcome` with a `SpecNeedsRevision` variant payload. Return MCP success on relay success; MCP error `-32603` on relay failure.
- [x] 2.5 Unit-test each tool's input validation: tool-error paths for missing required fields, wrong types, empty strings, AND placeholder-shaped strings. Each test asserts the JSON-RPC error code AND a substring of the expected message.
- [x] 2.6 Unit-test the relay paths: a mock control-socket server receives the relayed action AND asserts payload shape; the MCP handler returns success.

## 3. Classifier refactor

- [x] 3.1 In `claude_cli.rs`'s `classify_outcome`, insert a `consume_outcome` control-socket call BEFORE the AskUser-marker check. (Ordering: see executor capability deltas.) Use `workspace_basename` resolution consistent with how `mcp_askuser_server::ENV_WORKSPACE_BASENAME` is computed today.
- [x] 3.2 When `consume_outcome` returns a `Success` outcome, return `ExecutorOutcome::Completed { final_answer }` using the recorded `final_answer`. When it returns a `SpecNeedsRevision` outcome, return `ExecutorOutcome::SpecNeedsRevision { ... }` mirroring today's variant shape.
- [x] 3.3 When `consume_outcome` returns `None` (no outcome was tool-recorded), preserve today's classifier ordering exactly: AskUser marker → timeout precedence → stdout sentinel → exit status → completed.
- [x] 3.4 When the stdout-sentinel scan actually matches AND returns a parsed outcome (the legacy path), emit `tracing::warn!` with a deprecation message: `legacy stdout sentinel matched for change <change>; please call outcome_spec_needs_revision tool instead (stdout sentinel parsing is scheduled for removal in a27a2)`.
- [x] 3.5 Unit-test the precedence: a run where `consume_outcome` returns `Success` AND `outcome.stdout` contains a `=== AUTOCODER-OUTCOME ===` block is classified as Completed (the tool-recorded outcome wins).
- [x] 3.6 Unit-test the timeout precedence: a run where `consume_outcome` returns `SpecNeedsRevision` AND `outcome.timed_out` is true is classified as `SpecNeedsRevision` (the tool-recorded outcome wins over timeout).
- [x] 3.7 Unit-test the legacy fallback: a run where `consume_outcome` returns `None` AND `outcome.stdout` contains a parseable `=== AUTOCODER-OUTCOME ===` block produces today's `SpecNeedsRevision` outcome AND emits the deprecation warning (asserted via a tracing-subscriber capture).

## 4. Implementer prompt updates

- [x] 4.1 Add an "Outcome tools" section near the top of `prompts/implementer.md` (above the existing pre-flight sentinel section). The section names `outcome_success` AND `outcome_spec_needs_revision`, gives a one-line purpose for each, AND directs the model to call them at end-of-run instead of emitting the stdout sentinel. Do NOT inline the full schemas; the MCP `tools/list` response is the canonical schema source.
- [x] 4.2 Rewrite the pre-flight unimplementable-task instructions to use `outcome_spec_needs_revision`. The placeholder-rejection guidance stays AND continues to instruct the model to scan field values for `<...>` substrings before calling the tool — but now points out that the MCP server's input validation will reject placeholder-shaped strings with a tool error the model can correct AND retry.
- [x] 4.3 Append a "DEPRECATED" note to the existing `=== AUTOCODER-OUTCOME ===` sentinel section explaining that stdout-sentinel emission is still parsed for backward compatibility for one cycle, but the canonical path is `outcome_spec_needs_revision`. State the deprecation removal target (a27a2).
- [x] 4.4 Update the implementer prompt's "successful completion" guidance: at end-of-run on the success path, the model SHOULD call `outcome_success({ final_answer: "..." })` with its end-of-run summary text. Note that omitting this is not a hard error today (the classifier still falls through to Completed via the diff-presence heuristic) but will be enforced by the acceptance scan in a27a2.
- [x] 4.5 Add or update a parseability self-test for the prompt's tool-related examples: any JSON snippet shown in the prompt SHALL deserialize cleanly into the corresponding Rust type via `serde_json::from_str`.

## 5. Validation

- [x] 5.1 `cargo test` passes.
- [x] 5.2 `cargo clippy` produces no NEW warnings against the existing baseline.
- [x] 5.3 `openspec validate a27a0-outcome-tools-replace-stdout-sentinels --strict` passes.
