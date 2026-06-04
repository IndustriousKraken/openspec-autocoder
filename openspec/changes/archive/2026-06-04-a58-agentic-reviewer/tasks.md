# Implementation tasks

## 1. `reviewer.kind` + `reviewer.command` config (code-reviewer)

- [x] 1.1 `config.rs` — add `reviewer.kind: ReviewerKind { Oneshot, Agentic }`, default `Oneshot`. Add `reviewer.command: String`, default `"claude"`. Both hot-reloadable via the existing `reviewer:` reload path.
- [x] 1.2 Document that `kind: agentic` is only usable with an Anthropic-shaped provider until the opencode strategy (a60) lands; a non-`claude` `reviewer.command` resolves a strategy via a55/a56 and returns "strategy not yet implemented" until then.

## 2. Agentic reviewer path (code-reviewer)

- [x] 2.1 `code_reviewer.rs` — `review_pr_at_state` dispatches on `reviewer.kind`. The `Oneshot` branch is the existing path unchanged.
- [x] 2.2 Agentic branch: render the prompt from `ReviewContext` as change briefs + the unified diff + the changed-file path list (NO full-file pre-dump; `reviewer.prompt_budget_chars` is not consulted in this branch). Run a56's `agentic_run` with `ClaudeStrategy`, capture mode, a read-only sandbox (`allowed_tools = ["Read","Glob","Grep"]`, `Write`/`Edit`/`Bash` denied) plus the `submit_review` MCP tool and `ORCH_MCP_ROLE = reviewer`.
- [x] 2.3 After the session exits, `consume_submission` (a56) → map the payload to `ReviewResult { verdict, per_concern, raw_output }`. `raw_output` is the rendered markdown (summary + concerns) used for the PR-body `## Code Review` block.
- [x] 2.4 No valid submission → discard the review (write no verdict, do NOT default to `Approve`) AND post the existing reviewer-failure chatops alert. Wire this in the polling-loop caller AND the rerun composer (`revisions.rs`).
- [x] 2.5 Honor `reviewer.mode` in the agentic branch: per_change → one `agentic_run` per change; bundled → one per PR. The `ReviewResult` feeds the same disposition code as the one-shot path.

## 3. `submit_review` MCP tool (executor)

- [x] 3.1 `mcp_askuser_server.rs` — register `submit_review` under a56's per-role framework, gated on `ORCH_MCP_ROLE = reviewer`; not advertised for any other role. Relay via a56's `relay_submission` → `record_submission`.
- [x] 3.2 Register the `submit_review` schema with the control-socket validator: `verdict` enum `Approve | Block`; `summary: string`; `concerns: [{ title, detail, anchor, should_request_revision: bool, actionable_request: string|null }]`; the schema requires a non-empty `actionable_request` whenever `should_request_revision` is true.
- [x] 3.3 Map the consumed payload to `ReviewResult`/`ConcernEntry` (reuse the existing types; `should_request_revision` + `actionable_request` feed the existing reviewer-revision-comment requirement).

## 4. Tests

- [x] 4.1 `kind: oneshot` (default) produces byte-identical reviewer output to the pre-change path for a canned `ReviewContext`.
- [x] 4.2 The agentic sandbox advertises `Read`/`Glob`/`Grep` + `submit_review` and does NOT advertise `Bash`/`Write`/`Edit`.
- [x] 4.3 A schema-valid `submit_review` payload round-trips `record_submission` → `consume_submission` → the expected `ReviewResult` (verdict + concerns + raw_output).
- [x] 4.4 A non-enum verdict, AND a concern with `should_request_revision: true` but empty `actionable_request`, are each rejected as a correctable tool error; a subsequent valid submission in the same execution succeeds.
- [x] 4.5 An agentic session that ends with no valid submission discards the review (no verdict written, no auto-approve) AND fires the reviewer-failure chatops alert.
- [x] 4.6 `submit_review` is advertised only when `ORCH_MCP_ROLE = reviewer`.
- [x] 4.7 `reviewer.mode: per_change` in the agentic branch dispatches one `agentic_run` per change; the per-change `ReviewResult`s drive the same disposition as one-shot.

## 5. Acceptance gate

- [x] 5.1 `cargo test` passes for the autocoder crate.
- [x] 5.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 5.3 `openspec validate a58-agentic-reviewer --strict` passes.
