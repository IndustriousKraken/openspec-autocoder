# executor — delta for a70-capture-mode-implementer

## ADDED Requirements

### Requirement: Implementer runs through any CliStrategy (capture-mode path)
The implementer SHALL run through whichever `CliStrategy` its model resolves to (per a55's `provider → CLI` rule / an explicit `cli:`), not the `claude` strategy alone. For a capture-mode strategy (e.g. `opencode`, `gemini`), the implementer SHALL run via `agentic_run` in capture mode: the structured outcome (Completed / AskUser / Failed) AND the agent's `final_answer` summary SHALL be delivered via the MCP outcome relay (`outcome_*` / `record_outcome`) rather than parsed from streaming-JSON, since the streaming-JSON event path is claude-specific.

The streaming (live-log) implementer path remains claude-specific (per a60's `OpencodeStrategy` requirement); a capture-mode implementer runs WITHOUT the live incremental log. This is additive: the default implementer remains `claude` (streaming + `final_answer` + `session_id` unchanged), AND no role's default transport changes. It unblocks `opencode` AND `gemini` as operator-selectable implementers.

#### Scenario: A capture-mode strategy implements a change end-to-end
- **WHEN** the implementer's model resolves to a capture-mode strategy (`opencode` OR `gemini`) AND it runs a change through `agentic_run`
- **THEN** it runs in capture mode (no streaming-JSON parse, no live log)
- **AND** a `Completed` outcome AND the `final_answer` summary arrive via the MCP outcome relay
- **AND** the agent branch is updated exactly as it is for the claude implementer

#### Scenario: Capture-mode final_answer comes via the relay, not stream-JSON
- **WHEN** a capture-mode implementer finishes
- **THEN** its `final_answer` is taken from the outcome submission payload
- **AND** no streaming-JSON `final_answer` parse is attempted for that run

#### Scenario: The claude implementer is unchanged
- **WHEN** the implementer's model resolves to the `claude` strategy (the default)
- **THEN** it runs in streaming mode with the live log, parsed `final_answer`, AND `session_id` exactly as before
- **AND** an operator who configures no implementer CLI gets `claude`

### Requirement: Agentic implementer session lifecycle: resume, requeue-on-failure, surgical prune
The agentic implementer SHALL own the lifecycle of the session it creates per change.

**AskUser retains the session.** On an AskUser outcome, the implementer SHALL submit the question via the outcome relay AND end the run with the change in the waiting state, retaining (NOT pruning) the agentic session.

**The operator's answer resumes the same session.** When the operator answers, the implementer SHALL resume the same agentic session via the resolved strategy's native headless resume mechanism — `claude` via the captured `session_id`, `opencode` via `--session <id>`, `gemini` via `--resume` — delivering the answer into that session.

**Resume failure requeues; there is no stash fallback.** If the session cannot be restored (not found, corrupt, OR expired by the CLI's own retention), the implementer SHALL NOT fall back to a fresh-run-with-answer. It SHALL treat the attempt as a retryable failure AND requeue the change via the existing failure-counter path (repeated failures escalate per the existing perma-stuck policy). No stash-and-recombine path exists.

**Terminal outcome prunes only the created session.** On a terminal outcome (the change archives/completes OR fails terminally), the implementer SHALL prune ONLY the specific session it created — addressed by that session's identifier, via the CLI's own session-delete mechanism (`claude`: the session's `<uuid>` record under `~/.claude/projects/<hash>/`; `gemini --delete-session <id>`; `opencode` session deletion). The prune SHALL NOT remove settings, memory/context files (`CLAUDE.md` / `GEMINI.md` / project memories), credentials, OR the generated MCP config — only the session record. (Claude's store is known to grow unbounded and to risk destroying settings and auth when the disk fills, so the prune is deliberately surgical rather than a directory wipe.)

#### Scenario: AskUser retains the session and waits
- **WHEN** a capture-mode implementer returns an AskUser outcome
- **THEN** the question is posted via the outcome relay AND the change enters the waiting state
- **AND** the agentic session is NOT pruned

#### Scenario: The operator's answer resumes the same session
- **WHEN** the operator answers a waiting AskUser AND the session is restorable
- **THEN** the implementer resumes that same session via the strategy's native mechanism (`session_id` / `--session` / `--resume`) AND delivers the answer into it

#### Scenario: Resume failure requeues the change with no fallback
- **WHEN** the operator answers but the session cannot be restored (not found / corrupt / expired)
- **THEN** the implementer does NOT start a fresh-run-with-answer
- **AND** the change is requeued as a retryable failure via the existing failure-counter path
- **AND** repeated resume failures escalate under the existing perma-stuck policy

#### Scenario: Session prune is scoped to the created session
- **WHEN** the implementer prunes its session on a terminal outcome
- **THEN** only that session's record is removed, addressed by its identifier via the CLI's session-delete mechanism
- **AND** settings, memory/context files, credentials, AND the generated MCP config remain intact
