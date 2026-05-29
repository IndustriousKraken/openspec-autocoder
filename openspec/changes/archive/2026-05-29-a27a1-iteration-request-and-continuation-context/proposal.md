## Why

`a27a0-outcome-tools-replace-stdout-sentinels` introduces the MCP-tool surface for outcome signaling AND adds two outcomes: `outcome_success` AND `outcome_spec_needs_revision`. Those cover "I finished" AND "the spec contains tasks my sandbox cannot complete." Neither covers the third class of honest end-of-run state that the implementer has actually been hitting in production: **"I started, completed some tasks, AND want another iteration to finish the rest."**

Today the implementer that hits this state has no structured way to declare it. The path-of-least-resistance is to:

1. Leave the remaining tasks unchecked in `tasks.md`.
2. Narrate a "Deferred:" section in the final-answer text.
3. Exit zero.

autocoder accepts that as success; the PR ships with unchecked tasks AND a buried apology in the implementation-notes PR comment. Recent production iterations have done exactly this on substantial mechanical-refactor scope (`a26-oss-fork-support` task 2.3, `a27-thread-daemon-paths` tasks 1.x–4.x). The pattern is corrosive: the spec says "these are threaded in such-and-such a way" AND the code does not match, so every subsequent change layered on top inherits a latent contradiction. Operators only discover the gap when a downstream change fails to integrate.

The structural fix is to give the agent an honest channel for "I want another iteration" — paired with operator-friendly guardrails:

1. **A third MCP outcome tool: `outcome_request_iteration`.** Schema-validated, placeholder-rejected, recorded via the same control-socket pipeline a27a0 built. Carries the agent's cumulative-completed-task list, the remaining-task list, AND the concrete reason for stopping.
2. **A per-change iteration-pending marker** (`<workspace>/openspec/changes/<change>/.iteration-pending.json`) that records the iteration state across the subprocess-exit ↔ next-poll-cycle gap. The marker has the same operator-visibility properties as the existing `.in-progress`, `.question.json`, AND `.perma-stuck.json` files (filesystem-inspectable, no daemon-internal state needed).
3. **Queue front-insertion for iteration-pending-marked changes.** The next polling iteration on the same repo picks up the iteration-pending change AHEAD of any alphabetically-earlier pending change. Other pending changes wait until the iteration sequence concludes (success, spec-needs-revision, or cap-exceeded).
4. **A continuation-context block in the implementer prompt.** When the implementer is invoked for a change with an iteration-pending marker, the prompt includes a summary of what the prior iteration completed, what remains, the prior reason, AND the current iteration number. The block is framed to encourage the model to overcome the prior blocker with fresh eyes rather than inherit the prior pessimism.
5. **An iteration cap.** A change MAY emit `outcome_request_iteration` up to 4 times (yielding iterations 2–5 of the change). A 5th `outcome_request_iteration` is overridden by the classifier to `ExecutorOutcome::Failed { reason: "exceeded iteration-request cap (5)" }`, preserving the WIP on the agent branch AND posting an operator-visible failure. The cap exists because indefinite iteration is indistinguishable from a calibration loop the model cannot escape; surfacing the failure to the operator is the right move at that point.
6. **Lifecycle rules for the iteration-pending marker.** On `outcome_request_iteration`: the marker is replaced with the new iteration's cumulative state. On `outcome_success` OR `outcome_spec_needs_revision`: the marker is deleted. On any failure path (timeout, exit non-zero, classifier override): the marker is left untouched, so a retry of the same change still sees the prior continuation context.

This change builds on a27a0's outcome-store + control-socket actions AND requires a27a0 to be merged first. It introduces no new transport or protocol primitives.

## What Changes

**New MCP tool: `outcome_request_iteration`.** Added to the per-execution stdio MCP server. Schema:

```
outcome_request_iteration({
  completed_tasks: string[],   // cumulative across iterations, non-empty
  remaining_tasks: string[],   // non-empty
  reason: string               // concrete blocker; placeholder-shaped strings rejected
})
```

The MCP layer validates the input at the tool boundary (no placeholder-shaped strings, all arrays non-empty, `reason` non-empty) AND relays to the daemon via the existing `record_outcome` control-socket action using a new `iteration_request` variant tag. The daemon's outcome store records the payload alongside the iteration number (incremented per recorded iteration-request for the same key).

**New `ExecutorOutcome::IterationRequested` variant.** Carries the cumulative completed/remaining task lists, the agent's reason, AND the new iteration number (the next iteration's number — the one the operator will observe in journalctl). The classifier consumes the daemon-recorded `iteration_request` outcome via `consume_outcome` (the same path a27a0 introduced for the other outcomes) AND maps it to this variant. Polling-loop code that branches on `ExecutorOutcome` gains an `IterationRequested` arm.

**Iteration cap enforcement.** Before mapping `iteration_request` to `IterationRequested`, the classifier reads the current iteration count from the workspace's `.iteration-pending.json` marker (if present) AND adds one. If the resulting iteration_number exceeds the cap (5), the classifier emits `tracing::warn!` naming the change AND the cap, AND overrides the outcome to `ExecutorOutcome::Failed { reason: "exceeded iteration-request cap (5); WIP on agent branch — review or restart from scratch" }`. The marker file is NOT deleted on cap-exceeded — operator can review the cumulative state AND decide whether to restart.

**Iteration-pending marker file.** Written by the polling loop when handling `IterationRequested`: `<workspace>/openspec/changes/<change>/.iteration-pending.json` containing `{ "completed_tasks": [...], "remaining_tasks": [...], "reason": "...", "iteration_number": N }`. The marker's presence makes the change visible in the pending list (with front-insertion priority) without removing it from the pending set. Other pending changes in the same repo wait behind it.

**Queue front-insertion.** The queue engine's "Enumerate ready changes" requirement is MODIFIED to add an ordering preference: changes with a `.iteration-pending.json` marker come FIRST in the returned list (sorted by iteration_number ascending for the rare case of multiple iteration-pending changes), followed by alphabetical for unmarked changes. The marker is NOT a block; iteration-pending changes are still pending (eligible for processing). The existing filter rules (exclude `.in-progress`, `.question.json`, `.perma-stuck.json`, etc.) are preserved verbatim.

**Polling-loop behavior on `IterationRequested`:**

- Commit + force-push the WIP to the change's agent branch (so the next iteration starts from this iteration's progress).
- Write `.iteration-pending.json` with the cumulative state AND incremented iteration_number.
- Do NOT open a PR if none exists. Do NOT touch any open PR's state.
- Drop `.in-progress` (per the existing canonical unlocking requirement; the change is no longer in-progress on this poll cycle).
- Continue the polling loop. The next iteration on this repo picks up the iteration-pending change ahead of any alphabetically-earlier pending sibling.

**Implementer-prompt continuation block.** The bundled `prompts/implementer.md` template's render pipeline reads `.iteration-pending.json` at prompt-build time. If the marker is present, the rendered prompt appends a "Prior iteration summary" block AFTER the change body, naming the cumulative completed tasks, the remaining tasks, the prior reason, AND the current iteration number. The block instructs the agent to:

- Treat the prior progress as already-done (don't re-implement completed tasks).
- Re-evaluate the prior blocker with fresh eyes (don't inherit the prior pessimism).
- Mark tasks complete in `tasks.md` as work finishes.
- Call `outcome_success` at end-of-run when remaining tasks are done; call `outcome_request_iteration` again if another iteration is honestly needed.

The block does NOT instruct the agent to compute the iteration number; the daemon manages that bookkeeping. The agent reports cumulative completed/remaining each time; the daemon stores verbatim AND injects into the next iteration.

**Bottom of `prompts/implementer.md` documents `outcome_request_iteration` alongside the other outcome tools** (per a27a0's documentation discipline). The section names the tool, gives a one-line purpose, AND directs use ("when you started implementation but want another iteration to finish honest-scope-overflow remaining work — NOT for unimplementable tasks; use `outcome_spec_needs_revision` for those"). Schema details are NOT inlined; the MCP `tools/list` response is the canonical schema source.

## Impact

- **Affected specs:**
  - `executor` — ADDED requirements for the `outcome_request_iteration` tool surface, the `IterationRequested` outcome variant, the classifier's iteration-cap enforcement, the iteration-pending marker file lifecycle, AND the implementer-prompt continuation block. The existing a27a0 requirements for tool-recorded outcome precedence AND the deprecated stdout-sentinel path are unchanged.
  - `orchestrator-cli` — ADDED requirement for the `iteration_request` variant on the `record_outcome` control-socket action (extending a27a0's variant tag set). ADDED requirement for the polling-loop behavior on `IterationRequested` (commit + push WIP, write marker, no PR open/close, drop `.in-progress`, continue polling).
  - `openspec-queue-engine` — MODIFIED "Enumerate ready changes" requirement to add iteration-pending front-insertion ordering. Existing scenarios are preserved verbatim; a new scenario covers the iteration-pending case.
- **Affected code:**
  - `autocoder/src/mcp_askuser_server.rs` — `tools/list` gains one new entry; `tools/call` dispatch gains one new branch with input validation AND relay.
  - `autocoder/src/control_socket.rs` — `record_outcome` handler accepts the new `iteration_request` variant tag.
  - Daemon outcome store (from a27a0) gains an `IterationRequest` variant on `RecordedOutcome`.
  - `autocoder/src/executor/claude_cli.rs` — `classify_outcome` maps the new `IterationRequest` recorded outcome to `ExecutorOutcome::IterationRequested`, including cap enforcement that reads the workspace's marker file.
  - New `ExecutorOutcome::IterationRequested` variant.
  - `autocoder/src/polling_loop.rs` (OR equivalent) — `IterationRequested` arm handles commit + force-push, marker write, no-PR-touch, lock drop.
  - Queue engine's `list_pending` (currently in `autocoder/src/queue.rs`) — adds iteration-pending preference ordering.
  - Prompt-builder in `ClaudeCliExecutor` (currently `claude_cli.rs::build_prompt` or similar) — reads `.iteration-pending.json` if present AND appends continuation block.
  - `prompts/implementer.md` — outcome-tools section gains the `outcome_request_iteration` entry.
- **Operator-visible behavior:**
  - `journalctl` shows `outcome recorded via outcome_request_iteration` lines for iteration-pending runs (NEW).
  - `journalctl` shows `iteration N/5 of <change>` context on the next iteration's run-start log (NEW).
  - The iteration-pending marker is filesystem-inspectable (`ls -a <workspace>/openspec/changes/<change>/`) for operators debugging an in-progress iteration sequence.
  - Cap-exceeded runs surface as `Failed { reason: "exceeded iteration-request cap (5); WIP on agent branch — review or restart from scratch" }` AND post the standard `Failed` notification.
  - PRs are NOT opened, closed, OR modified during iteration sequences. They are opened by the polling loop on the FIRST `Completed` outcome (today's behavior).
- **Backward compatibility:** This change extends a27a0's tool surface AND outcome-store schema additively. Implementers that never call `outcome_request_iteration` see no behavioral change. The queue engine's ordering change is opt-in by marker presence; pending changes without the marker continue to enumerate in alphabetical order exactly as today.
- **Dependencies:** a27a0 must be merged first. This change extends a27a0's MCP tool list, outcome-store variants, classifier ordering, AND prompt-tools section.
- **Acceptance:** `cargo test` passes; `openspec validate a27a1-iteration-request-and-continuation-context --strict` passes. Tests:
  - Tool-error path: `outcome_request_iteration` called with an angle-bracket placeholder in `reason` returns MCP error `-32602`; no `record_outcome` is sent.
  - Tool-error path: `outcome_request_iteration` called with empty `completed_tasks` OR empty `remaining_tasks` returns MCP error `-32602`.
  - Outcome-store round-trip: an `iteration_request` payload survives `record_outcome` → `consume_outcome` byte-for-byte.
  - Classifier mapping: a recorded `iteration_request` outcome maps to `ExecutorOutcome::IterationRequested` with the cumulative completed/remaining lists AND the next iteration number.
  - Cap enforcement: a recorded `iteration_request` outcome with the marker file showing `iteration_number: 4` maps to `IterationRequested` (iteration 5 still allowed); `iteration_number: 5` maps to `Failed { reason: "exceeded iteration-request cap (5)..." }`.
  - Marker lifecycle: an `IterationRequested` outcome writes the marker with incremented iteration_number; a subsequent `Completed` outcome on the same change deletes the marker; a subsequent `Failed` outcome leaves the marker in place.
  - Queue ordering: a workspace with both `a30-foo` (alphabetically earlier, unmarked) AND `a31-bar` (alphabetically later, iteration-pending-marked) returns `a31-bar` first from `list_pending`.
  - Queue ordering: the existing alphabetical-order-among-unmarked behavior is preserved (covered by the existing scenarios, which the MODIFIED block preserves verbatim).
  - Prompt continuation: a prompt-builder invocation for a change with a marker present produces output containing the "Prior iteration summary" block with the marker's cumulative completed_tasks AND reason.
  - No marker, no block: a prompt-builder invocation for a change without a marker produces output WITHOUT the continuation block (clean first-iteration prompt unchanged).
