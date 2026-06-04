# Implementation tasks

## 1. Integration spike (per non-claude strategy)

- [ ] 1.1 Confirm `opencode` headless resume: capture a session ID from a non-interactive `opencode run`, then continue it non-interactively via `--session <id>` (or `--continue`) with a new prompt. Confirm the answer reaches the same conversation.
- [ ] 1.2 Confirm `gemini` headless resume: under the one-session-per-workspace serializer, `gemini --resume` restores the correct prior session for that project hash; confirm a queried UUID (`--list-sessions`) also works if needed.
- [ ] 1.3 Confirm the per-CLI **scoped** session-delete that targets only one session and leaves settings/memory/auth intact: `gemini --delete-session <id>`; the specific Claude `<uuid>` record under `~/.claude/projects/<hash>/`; the `opencode` session-delete path. Confirm a session handle is capturable for EVERY role (implementer AND single-shot audits/reviewer) so each run can delete its own session.

## 2. CliStrategy trait: resume + scoped delete

- [ ] 2.1 Add to the `CliStrategy` trait (a56) a headless-resume mechanism (given a session handle + the answer prompt, build the resume invocation) AND a scoped session-delete (given a session handle, delete ONLY that session's record). Implement for `claude` (`session_id` → `--resume`; delete the `<uuid>` record), `opencode` (`--session`; its delete path), AND `gemini` (`--resume`; `--delete-session`).
- [ ] 2.2 Capture the session handle for each run: `claude` from the streamed `session_id`; `opencode` from its emitted/queryable session ID; `gemini` via "latest for this project hash" (serializer-guaranteed) or a `--list-sessions` UUID. Persist the handle where the cleanup (and, for the implementer, the resume) step can reach it.

## 3. Strategy-agnostic implementer

- [ ] 3.1 Resolve the implementer's strategy from its model (like the other roles) instead of hardcoding `claude`. Run via `agentic_run`: streaming mode for `claude`, capture mode for capture-only strategies.
- [ ] 3.2 In capture mode, take the outcome AND `final_answer` from the MCP outcome relay (do not attempt a streaming-JSON `final_answer` parse). Keep the claude streaming path byte-identical.

## 4. Session cleanup (every agentic role)

- [ ] 4.1 After any `agentic_run` that created a session, call the strategy's scoped session-delete for that session's handle. **Single-shot roles** (advisory audits, reviewer, contradiction check, future agentic roles) prune on run completion. The **implementer** defers its prune to its terminal outcome (§5.4), because it may retain the session across AskUser.
- [ ] 4.2 The prune targets ONLY the created session record (by handle, via the CLI's own delete). It SHALL NOT touch settings, memory/context files (`CLAUDE.md` / `GEMINI.md` / project memories), credentials, or the generated MCP config — never a directory wipe.

## 5. Implementer AskUser resume

- [ ] 5.1 AskUser: submit the question via the outcome relay, enter waiting, retain the session (record its handle; do NOT prune yet).
- [ ] 5.2 Answer: resume the retained session via the strategy's resume mechanism, delivering the answer.
- [ ] 5.3 Resume failure (not found / corrupt / expired): do NOT fresh-run; requeue the change via the existing failure-counter path. No stash-and-recombine code path is added.
- [ ] 5.4 Terminal outcome (completed/archived OR terminal failure): prune the implementer's session via the §4 scoped delete.

## 6. Tests

- [ ] 6.1 A capture-mode strategy runs the implementer end-to-end: outcome + `final_answer` arrive via the relay; the agent branch updates; no streaming-JSON parse occurs (assert behavior, not message text).
- [ ] 6.2 The claude implementer path is unchanged (streaming + `final_answer` + `session_id`); default implementer with no configured CLI is `claude`.
- [ ] 6.3 A single-shot agentic role (e.g. an audit) prunes its session on completion: the created session record is gone (by handle), and a sentinel settings/memory/MCP file is left intact (surgical scope).
- [ ] 6.4 AskUser retains the implementer's session (not pruned); an answer resumes the same handle; resume failure requeues the change (assert the failure-counter increment AND the absence of any fresh-run-with-answer) — no fallback path exists.
- [ ] 6.5 The implementer's terminal-outcome prune removes only its created session record (by handle), leaving settings/memory/MCP config in place.

## 7. Acceptance gate

- [ ] 7.1 `cargo test` passes for the autocoder crate.
- [ ] 7.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] 7.3 `openspec validate a70-capture-mode-implementer --strict` passes.
