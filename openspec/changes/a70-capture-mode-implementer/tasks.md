# Implementation tasks

## 1. Integration spike (per non-claude strategy)

- [ ] 1.1 Confirm `opencode` headless resume: capture a session ID from a non-interactive `opencode run`, then continue it non-interactively via `--session <id>` (or `--continue`) with a new prompt. Confirm the answer reaches the same conversation.
- [ ] 1.2 Confirm `gemini` headless resume: under the one-session-per-workspace serializer, `gemini --resume` restores the correct prior session for that project hash; confirm a queried UUID (`--list-sessions`) also works if needed.
- [ ] 1.3 Confirm the per-CLI **scoped** session-delete that targets only the created session and leaves settings/memory/auth intact: `gemini --delete-session <id>`; the specific Claude session record under `~/.claude/projects/<hash>/`; the `opencode` session-delete path. Record the exact identifier/command for each.

## 2. CliStrategy trait: resume + scoped delete

- [ ] 2.1 Add to the `CliStrategy` trait (a56) a headless-resume mechanism (given a session handle + the answer prompt, build the resume invocation) AND a scoped session-delete (given a session handle, delete only that session's record). Implement for `claude` (`session_id` → `--resume`), `opencode` (`--session`), AND `gemini` (`--resume` / `--delete-session`).
- [ ] 2.2 The session handle SHALL be captured for each strategy: `claude` from the streamed `session_id`; `opencode` from its emitted/queryable session ID; `gemini` via "latest for this project hash" (serializer-guaranteed) or a `--list-sessions` UUID. Persist the handle in the per-change state for the resume + prune steps.

## 3. Strategy-agnostic implementer

- [ ] 3.1 Resolve the implementer's strategy from its model (like the other roles) instead of hardcoding `claude`. Run via `agentic_run`: streaming mode for `claude`, capture mode for capture-only strategies.
- [ ] 3.2 In capture mode, take the outcome AND `final_answer` from the MCP outcome relay (do not attempt a streaming-JSON `final_answer` parse). Keep the claude streaming path byte-identical.

## 4. Session lifecycle

- [ ] 4.1 AskUser: submit the question via the outcome relay, enter waiting, retain the session (record its handle).
- [ ] 4.2 Answer: resume the retained session via the strategy's resume mechanism, delivering the answer.
- [ ] 4.3 Resume failure (session not found / corrupt / expired): do NOT fresh-run; requeue the change via the existing failure-counter path. No stash-and-recombine code path is added.
- [ ] 4.4 Terminal outcome (completed/archived OR terminal failure): call the strategy's scoped session-delete for the created session only. Never touch settings, memory/context files, credentials, or the MCP config.

## 5. Tests

- [ ] 5.1 A capture-mode strategy runs the implementer end-to-end: outcome + `final_answer` arrive via the relay; the agent branch updates; no streaming-JSON parse occurs (assert behavior, not message text).
- [ ] 5.2 The claude implementer path is unchanged (streaming + `final_answer` + `session_id`); default implementer with no configured CLI is `claude`.
- [ ] 5.3 AskUser retains the session; an answer resumes the same handle.
- [ ] 5.4 Resume failure requeues the change (assert the requeue/failure-counter increment and the absence of any fresh-run-with-answer); no fallback path exists.
- [ ] 5.5 Terminal-outcome prune removes only the created session record (by handle) and leaves a sentinel settings file / memory file / MCP config in place (assert the surgical scope).

## 6. Acceptance gate

- [ ] 6.1 `cargo test` passes for the autocoder crate.
- [ ] 6.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] 6.3 `openspec validate a70-capture-mode-implementer --strict` passes.
