# Tasks

## 1. Outcome-store + control-socket variant extension

- [x] 1.1 Extend `RecordedOutcome` (introduced in a27a0) with an `IterationRequest { completed_tasks: Vec<String>, remaining_tasks: Vec<String>, reason: String }` variant.
- [x] 1.2 Extend the `record_outcome` control-socket handler to accept `{ "type": "iteration_request", "completed_tasks": [...], "remaining_tasks": [...], "reason": "..." }` AS an `outcome` payload AND store the corresponding `RecordedOutcome::IterationRequest` variant. Unknown variant tags continue to return `{ "ok": false, "error": "..." }`.
- [x] 1.3 Extend the `consume_outcome` control-socket handler to return the new variant tag in its response shape (no new handler logic needed; this is automatic from D1.1's enum extension if the response uses `serde::Serialize`).
- [x] 1.4 Unit-test: a `record_outcome` for the new variant followed by `consume_outcome` round-trips the payload byte-for-byte.

## 2. MCP `outcome_request_iteration` tool

- [x] 2.1 Add `outcome_request_iteration` to `mcp_askuser_server.rs`'s `tools/list` response with the documented input schema (`completed_tasks: string[]` non-empty, `remaining_tasks: string[]` non-empty, `reason: string` non-empty AND no `<...>`-shaped substrings).
- [x] 2.2 Add the `outcome_request_iteration` branch to the `tools/call` dispatch. Validate at the MCP layer:
  - Both arrays present AND non-empty.
  - Every array element a non-empty string.
  - `reason` present, non-empty.
  - NO string field contains a `<...>`-shaped substring.
- [x] 2.3 On validation failure, return MCP error code `-32602` (invalid params) with a `message` naming the offending field. The control socket is NOT contacted.
- [x] 2.4 On valid input, relay via `record_outcome` with the `iteration_request` variant. Return MCP success on relay success; MCP error `-32603` on relay failure.
- [x] 2.5 Unit-test each validation rejection (empty `completed_tasks`, empty `remaining_tasks`, empty `reason`, placeholder-shaped `reason`, placeholder-shaped element in either array).
- [x] 2.6 Unit-test the relay path: a mock control-socket server receives the relayed action AND asserts the payload's variant tag AND fields.

## 3. Classifier mapping + cap enforcement

- [x] 3.1 Add `IterationRequested { completed_tasks: Vec<String>, remaining_tasks: Vec<String>, reason: String, iteration_number: u32 }` variant to `ExecutorOutcome`.
- [x] 3.2 In `claude_cli.rs::classify_outcome`, when `consume_outcome` returns a `RecordedOutcome::IterationRequest`:
  - Read the workspace's `.iteration-pending.json` (if present) to determine the prior `iteration_number`. Treat absent / unreadable / corrupt as iteration 1 (first iteration that requested another).
  - Compute `next_iteration_number = prior_iteration_number + 1` (so a first request produces `iteration_number: 2`, meaning "the upcoming iteration will be the 2nd").
  - If `next_iteration_number > 5`: emit `tracing::warn!` naming the change AND the cap, AND return `ExecutorOutcome::Failed { reason: "exceeded iteration-request cap (5); WIP on agent branch — review or restart from scratch" }`. The marker is NOT modified (the polling-loop's `Failed` arm leaves it in place).
  - Otherwise: return `ExecutorOutcome::IterationRequested { ..., iteration_number: next_iteration_number }`.
- [x] 3.3 Unit-test: a `RecordedOutcome::IterationRequest` with no marker present maps to `IterationRequested { iteration_number: 2, ... }`.
- [x] 3.4 Unit-test: a `RecordedOutcome::IterationRequest` with a marker showing `iteration_number: 4` maps to `IterationRequested { iteration_number: 5, ... }`.
- [x] 3.5 Unit-test: a `RecordedOutcome::IterationRequest` with a marker showing `iteration_number: 5` maps to `Failed { reason: "exceeded iteration-request cap (5)..." }`. Marker is NOT modified.
- [x] 3.6 Unit-test: a `RecordedOutcome::IterationRequest` with a corrupt marker file (truncated JSON) maps to `IterationRequested { iteration_number: 2, ... }` (treats corrupt-as-absent).

## 4. Polling-loop `IterationRequested` arm

- [x] 4.1 Add the `ExecutorOutcome::IterationRequested` arm to the polling-loop outcome dispatcher (currently in `polling_loop.rs` OR equivalent).
- [x] 4.2 The arm performs, in order:
  1. Commit the workspace's diff to the agent branch with a commit message naming the iteration number AND the change (e.g. `iteration <N> of <change>: <reason-first-80-chars>`).
  2. Force-push the agent branch to the remote.
  3. Write `.iteration-pending.json` with `{ completed_tasks, remaining_tasks, reason, iteration_number }` using atomic tempfile + rename.
  4. Drop `.in-progress` per the existing canonical unlocking requirement.
- [x] 4.3 The arm SHALL NOT call any PR-open OR PR-comment routine. PRs are not touched during iteration sequences.
- [x] 4.4 If the commit step fails (e.g. clean working tree — agent emitted iteration-request without modifying anything): emit `tracing::warn!` naming the anomaly AND proceed to write the marker anyway. The next iteration will see the marker AND the unchanged tree AND can proceed; the prior iteration produced no progress AND will count against the cap as expected.
- [x] 4.5 If the push step fails: emit `tracing::error!`, do NOT write the marker (per D5 in design.md), drop `.in-progress`, AND let the polling loop continue. The change reverts to normal pending behavior on the next cycle.
- [x] 4.6 Unit-test the marker-write helper (atomic tempfile + rename) with a corrupt-state injection.
- [x] 4.7 Integration-test (against a temp workspace + a local bare repo as the "remote"): an `IterationRequested` outcome produces a commit on the agent branch, a force-push, AND a marker file with the documented payload.

## 5. Queue ordering with iteration-pending preference

- [x] 5.1 Update `list_pending` in the queue engine module to apply two-tier ordering:
  - First tier: entries with `.iteration-pending.json` present, sorted by their marker's `iteration_number` ascending.
  - Second tier: entries WITHOUT the marker, sorted alphabetically (unchanged from today).
  - Output is the concatenation of the two tiers.
- [x] 5.2 The filter rules (exclude `.in-progress`, `.question.json`, `.perma-stuck.json`, etc.) are UNCHANGED. `.iteration-pending.json` is NOT in the exclusion list.
- [x] 5.3 Unit-test: a workspace with `a30-foo` (unmarked) AND `a31-bar` (marked with iteration_number: 2) returns `["a31-bar", "a30-foo"]` from `list_pending`.
- [x] 5.4 Unit-test: a workspace with `a30-foo` (marked iteration_number: 3) AND `a31-bar` (marked iteration_number: 2) returns `["a31-bar", "a30-foo"]` (lower iteration_number first within the marked tier).
- [x] 5.5 Unit-test: the existing alphabetical-among-unmarked behavior is preserved (regression test against today's enumeration).
- [x] 5.6 Unit-test: a corrupt marker (unparseable JSON) is treated as "iteration_number: 0" for ordering purposes (so the marked-tier slot is taken, but the entry sorts ahead of any valid marked entries). The corrupt marker does NOT cause `list_pending` to error.

## 6. Iteration-pending marker lifecycle in non-IterationRequested arms

- [x] 6.1 The polling-loop's `Completed` arm SHALL delete `.iteration-pending.json` (if present) AFTER the commit + push step completes successfully. Deletion is idempotent.
- [x] 6.2 The polling-loop's `SpecNeedsRevision` arm SHALL delete `.iteration-pending.json` (if present). Operator action is now required AND the iteration sequence is conceptually terminated.
- [x] 6.3 The polling-loop's `Failed` arm SHALL leave `.iteration-pending.json` (if present) untouched. A retry of the same change preserves the iteration context.
- [x] 6.4 The polling-loop's `AskUser` arm SHALL leave `.iteration-pending.json` (if present) untouched. The agent's question may resolve into a continuation; the iteration context stays available for the resumed run.
- [x] 6.5 Unit-test each lifecycle case (Completed deletes, SpecNeedsRevision deletes, Failed preserves, AskUser preserves).

## 7. Implementer-prompt continuation block

- [x] 7.1 Update the prompt-builder (currently in `claude_cli.rs::build_prompt` OR the `PromptLoader` rendering pipeline) to read `<workspace>/openspec/changes/<change>/.iteration-pending.json` at prompt-build time.
- [x] 7.2 When the marker is present, append the "Prior iteration summary" block AFTER the change body. The block content follows the format specified in the executor capability deltas (substitution of `<list>`, `<reason>`, `N` with the marker's values).
- [x] 7.3 When the marker is absent (first iteration), the prompt is built as today with no continuation block. The first-iteration prompt is unchanged.
- [x] 7.4 When the marker is present BUT corrupt (truncated JSON, missing required field): emit `tracing::warn!` naming the change AND fall back to building the prompt as if no marker were present. The corrupt marker is NOT deleted (operator can inspect AND repair if desired); the next iteration proceeds without continuation context.
- [x] 7.5 Add `outcome_request_iteration` to the prompt's "Outcome tools" section (added in a27a0) with a one-line purpose statement AND the "use this when you started implementation but want another iteration to finish — NOT for unimplementable tasks" guidance.
- [x] 7.6 Unit-test: a prompt-builder invocation for a change with a present-AND-valid marker produces output containing the continuation block AND every marker-field value verbatim.
- [x] 7.7 Unit-test: a prompt-builder invocation for a change with no marker produces output WITHOUT the continuation block.
- [x] 7.8 Unit-test: a prompt-builder invocation for a change with a corrupt marker logs a warning AND produces output WITHOUT the continuation block.

## 8. Validation

- [x] 8.1 `cargo test` passes.
- [x] 8.2 `cargo clippy` produces no NEW warnings against the existing baseline.
- [x] 8.3 `openspec validate a27a1-iteration-request-and-continuation-context --strict` passes.
