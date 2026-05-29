## Why

The implementer signals its run outcome to autocoder via an `=== AUTOCODER-OUTCOME ===` stdout sentinel followed by a JSON payload. The pattern works on the happy path AND has been hardened against several false-match modes (timeout precedence, JSON-mode `final_answer` scoping, placeholder detection). It still has three structural problems that recurring incidents have made unavoidable:

1. **Format mistakes are unrecoverable from the model's side.** When the model emits an unparseable sentinel — angle-bracket placeholders, missing fields, malformed JSON, sentinel-without-payload — the subprocess has already exited by the time the daemon's parser runs. The parse failure becomes a perma-stuck reason. The model that made the typo has no opportunity to correct it; the human operator must intervene. Multiple production incidents have hit this mode (a21 placeholder emission, miscellaneous JSON-mode false-matches), each requiring operator triage for what is fundamentally an off-by-one in the model's emission.

2. **There is no honest channel for "I started but want to stop early."** The existing `spec_needs_revision` sentinel is scoped to sandbox-incompatible tasks (sudo on prod, real GitHub pushes, browser flows) that the agent identifies in pre-flight WITHOUT modifying any files. The implementer prompt restricts the sentinel to that pre-flight use. An agent that begins implementation, completes some tasks, AND hits a scope or calibration limit on the remainder has no structured way to signal this. The path-of-least-resistance is to narrate a "Deferred:" section in the final-answer text, leave tasks.md unchecked, and exit zero. autocoder accepts that as success; the PR ships with unchecked tasks AND a narrative apology buried in the implementation-notes comment. Recent production iterations have done this on substantial mechanical-refactor scope (`a26-oss-fork-support` task 2.3, `a27-thread-daemon-paths` tasks 1.x–4.x), where the deferred work was neither sandbox-incompatible nor genuinely multi-day; the model self-imposed a scope ceiling AND used narrative-text as the escape hatch.

3. **End-of-session detection is heuristic, not structural.** Today's classifier infers "the model is done" from "the subprocess exited." The model has no way to say "I'm done" in a way the daemon can structurally distinguish from "I gave up and exited." This blocks the downstream recovery primitive (a27a2): an acceptance scan that finds unchecked tasks AND wants to re-prompt the same session cannot tell whether the model intended to conclude or merely stopped emitting.

All three issues resolve cleanly under the same shift: outcome signaling moves from a stdout text pattern (parsed post-exit) to MCP tool calls (parsed live, with real validation feedback). The per-process MCP relay infrastructure (`autocoder/src/mcp_askuser_server.rs`, established in the a21 canonical-spec-RAG change) is the natural host — its `ask_user` and `query_canonical_specs` tools already use the relay-to-daemon-via-control-socket pattern that outcome tools require.

This change is the foundation of a three-change stack:

- **a27a0** (this change): outcome tools replace the stdout sentinel for the existing `spec_needs_revision` payload AND introduce `outcome_success` as the explicit completion signal. The stdout sentinel parser remains as a deprecated fallback for one cycle.
- **a27a1**: adds `outcome_request_iteration` for honest scope-overflow signaling AND the prior-iteration continuation-context block in the implementer prompt.
- **a27a2**: adds the post-run acceptance scan (unchecked-tasks detection) AND the recovery-loop primitive (re-prompt the same session via `claude --resume <session_id>` when no outcome tool was called). Removes the stdout sentinel fallback at the end of its one-cycle deprecation window.

## What Changes

**Outcome signaling moves to MCP tools.** The per-process MCP relay SHALL advertise two new tools alongside the existing `ask_user` and `query_canonical_specs`:

- `outcome_success(final_answer?: string)` — explicit successful completion. Optional `final_answer` carries the implementer's end-of-run summary text (today's `result`-event content) for log capture AND PR-comment rendering.

- `outcome_spec_needs_revision(unimplementable_tasks: [{task_id, task_text, reason}], revision_suggestion: string)` — the existing `spec_needs_revision` payload, now schema-validated at the tool layer rather than the post-exit parser. Placeholder rejection (no `<...>`-shaped substrings in string fields) moves into the MCP handler's input validation, so the model receives a structured tool error if it emits placeholders AND can retry the tool call in the same session.

Both tools relay to the daemon via the existing Unix-domain control socket using a new `record_outcome` action. The daemon stores the outcome in an execution-scoped in-memory map keyed by `(workspace_basename, change)`. A sibling `consume_outcome` action lets the classifier drain the recorded outcome after the subprocess exits.

**`classify_outcome` consults the daemon-recorded outcome before any stdout sentinel scan.** The new ordering in the executor's classifier:

1. AskUser marker check (unchanged).
2. Daemon-recorded outcome via `consume_outcome` (NEW). If present, the classifier returns the corresponding `ExecutorOutcome` immediately. The recorded outcome's authority overrides both timeout AND exit-status heuristics — a tool-recorded outcome IS the agent's deliberate emission AND is more authoritative than any inferred state.
3. Timeout precedence (unchanged; preserves a20a1's narrowing for the case where no outcome was tool-recorded).
4. Legacy stdout sentinel scan (UNCHANGED in behavior; gains a deprecation warning when it actually matches, so operator logs surface "this agent invocation used the legacy stdout sentinel instead of `outcome_spec_needs_revision`" for the duration of the one-cycle deprecation).
5. Exit-status path (unchanged).
6. Layer-2 stdout heuristic + Completed fallback (unchanged).

**Stdout sentinel parser remains for one cycle as a deprecated fallback.** The parser stays bit-for-bit compatible with today's `=== AUTOCODER-OUTCOME ===` extraction. Its only behavioral change: when it actually matches AND returns a parsed outcome, the classifier emits `tracing::warn!` with a deprecation message naming the change AND directing the maintainer at the new tool. Planned removal is a27a2; outcome-tool adoption is the prerequisite for the acceptance scan + recovery loop that a27a2 introduces.

**Implementer prompt documents the new tools.** `prompts/implementer.md` gains a section above the existing sentinel section describing `outcome_success` AND `outcome_spec_needs_revision`: names, one-line purposes, when to use each. Schema details are NOT duplicated (MCP's tools/list response is the canonical schema); the prompt names the tools so the model knows they exist AND knows what each one is for even if context compression evicts the tools/list payload from working memory. The existing sentinel section gains a "DEPRECATED — use `outcome_spec_needs_revision` instead. Stdout sentinels still parsed for one release cycle but emit a deprecation warning." note. The pre-flight unimplementable-task instructions (which sentinel to emit, when to scan for placeholders) are rewritten in terms of the new tool.

## Impact

- **Affected specs:**
  - `executor` — ADDED requirements for the outcome-tools surface, the tool-outcome precedence layer above the existing classifier ordering, the legacy stdout-sentinel deprecation warning, AND the implementer-prompt documentation discipline for the new tools. The existing "Timeout classification takes precedence over sentinel extraction" requirement is preserved verbatim AND continues to govern the stdout-scan path; the new tool-outcome precedence layer sits above it without modifying its scope. The existing "Sentinel emission instructions in the implementer prompt include a concrete worked example AND a self-check hint" requirement is similarly preserved AND continues to bind the bundled prompt's stdout-sentinel section for the deprecation cycle; the new tool-documentation discipline applies in parallel.
  - `orchestrator-cli` — ADDED requirement for the `record_outcome` AND `consume_outcome` control-socket actions, modeled on the existing `query_canonical_specs` action's relay pattern.
- **Affected code:**
  - `autocoder/src/mcp_askuser_server.rs` — `tools/list` gains two new entries; `tools/call` dispatch gains two new branches; both branches relay to the daemon via the existing control-socket transport with a new `record_outcome` action payload. Placeholder detection moves into the `outcome_spec_needs_revision` branch's input validation.
  - `autocoder/src/control_socket.rs` — new `record_outcome` action handler (writes to daemon outcome store) AND new `consume_outcome` action handler (drains the store for a given workspace + change). The handlers follow the existing `query_canonical_specs` shape.
  - New daemon-side outcome store: an `Arc<Mutex<HashMap<(WorkspaceBasename, ChangeName), RecordedOutcome>>>` (OR equivalent) created at daemon startup, shared with the control-socket handlers, drained by `consume_outcome` calls from `classify_outcome`.
  - `autocoder/src/executor/claude_cli.rs` — `classify_outcome` gains a `consume_outcome` control-socket call early in the dispatch ordering. The legacy stdout sentinel branch gains a `tracing::warn!` deprecation message when it actually matches.
  - `prompts/implementer.md` — adds the outcome-tools section described above; updates the existing sentinel section with the deprecation note; rewrites the pre-flight unimplementable-task instructions to use `outcome_spec_needs_revision`.
- **Operator-visible behavior:**
  - Implementer runs that emit a malformed outcome payload (placeholder text, missing field, wrong type) now receive an MCP tool error AND can retry the tool call in the same session. The perma-stuck "unparseable sentinel" failure mode disappears for the tool-using path; the legacy stdout path retains it for the deprecation window.
  - `journalctl` shows `outcome recorded via outcome_<tool>` log lines (NEW) AND `legacy stdout sentinel matched; please call outcome_spec_needs_revision instead` warnings (NEW) during the deprecation window.
  - No change in the success path's behavior for runs that don't hit either failure mode: `Completed` outcomes look identical to today, AND the PR-comment composition reads the same `final_answer` content from the same place.
- **Backward compatibility:** the legacy stdout sentinel format is parsed unchanged for one cycle. Implementers running the old prompt against this version of the daemon continue to work. Implementers running the new prompt against an older daemon (impossible in production, but worth naming) would emit tool calls that the older daemon's MCP server doesn't advertise; the model would receive `method not found` AND fall through to either the stdout sentinel (if the agent retried via that path) OR to no-outcome (handled by today's Completed-with-clean-diff path). This is a degraded mode but not a hard failure.
- **Acceptance:** `cargo test` passes; `openspec validate a27a0-outcome-tools-replace-stdout-sentinels --strict` passes. Tests:
  - Tool-error path: `outcome_spec_needs_revision` called with an angle-bracket placeholder in `task_id`, `task_text`, OR `reason` returns a structured tool error naming the offending field; no `record_outcome` action is sent to the daemon.
  - Tool-error path: `outcome_spec_needs_revision` called with missing required field returns a structured tool error naming the field; no `record_outcome` action is sent to the daemon.
  - Daemon precedence path: a recorded outcome from `outcome_success` takes precedence over a stdout sentinel block present in the same run's captured event stream.
  - Daemon precedence path: a recorded outcome from `outcome_spec_needs_revision` takes precedence over the timeout flag (the recorded outcome IS the deliberate emission; the timeout happened AFTER the tool call but BEFORE clean exit).
  - Legacy compatibility path: a run that emits the existing `=== AUTOCODER-OUTCOME ===` stdout sentinel AND does NOT call any outcome tool is classified the same way it is today, AND emits a deprecation warning in the daemon log.
  - Round-trip: a unit test of the daemon outcome store proves `record_outcome` followed by `consume_outcome` returns the recorded payload AND clears the store, so a subsequent `consume_outcome` for the same key returns `None`.
