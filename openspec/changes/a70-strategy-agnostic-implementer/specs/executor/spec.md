# executor — delta for a70-strategy-agnostic-implementer

## ADDED Requirements

### Requirement: Implementer runs through any CliStrategy
The implementer SHALL run through whichever `CliStrategy` its model resolves to (per a55's `provider → CLI` rule / an explicit `cli:`), not the `claude` strategy alone. For a capture-mode strategy (e.g. `opencode`, `antigravity`), the implementer SHALL run via `agentic_run` in capture mode: the structured outcome (Completed / AskUser / Failed) AND the agent's `final_answer` summary SHALL be delivered via the MCP outcome relay (`outcome_*` / `record_outcome`) rather than parsed from streaming-JSON, since the streaming-JSON event path is claude-specific.

The streaming (live-log) implementer path remains claude-specific (per a60's `OpencodeStrategy` requirement); a capture-mode implementer runs WITHOUT the live incremental log. This is additive: the default implementer remains `claude` (streaming + `final_answer` + `session_id` unchanged), AND no role's default transport changes. It unblocks `opencode` AND `antigravity` as operator-selectable implementers.

#### Scenario: A capture-mode strategy implements a change end-to-end
- **WHEN** the implementer's model resolves to a capture-mode strategy (`opencode` OR `antigravity`) AND it runs a change through `agentic_run`
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

### Requirement: Every agentic role cleans up the session it creates
Any role that runs through `agentic_run` — the implementer AND every single-shot agentic role (the advisory audits, the reviewer, the contradiction check, AND any future agentic role) — SHALL remove the session it created from the CLI's session store when the role is done with it. The CLIs persist a transcript per invocation in the operator's home directory (`~/.claude/projects/<hash>/`, `~/.antigravity/<hash>/`, OpenCode's store); left alone these accumulate without bound. The principle: a run that creates litter — even when it is upstream software writing into the home directory — cleans it up at the end.

"Done with it" is role-dependent: a single-shot role (which never resumes) prunes on run completion; the implementer (which may retain a session across AskUser — see the implementer-resume requirement) prunes on its terminal outcome (the change archives/completes OR fails terminally).

The prune SHALL be surgical: it removes ONLY the specific session record the run created, addressed by that session's identifier, via the CLI's own session-delete mechanism (Antigravity's session delete under `~/.antigravity/`; the specific Claude `<uuid>` record under `~/.claude/projects/<hash>/`; OpenCode's session deletion). It SHALL NOT remove settings, memory/context files (`CLAUDE.md` / `AGENTS.md` / project memories), credentials, OR the generated MCP config — only the session record. (Claude's store is known to grow unbounded and to risk destroying settings and auth when the disk fills, so the prune is deliberately surgical rather than a directory wipe.)

#### Scenario: A single-shot agentic role prunes its session on completion
- **WHEN** an advisory audit, the reviewer, OR the contradiction check finishes its agentic run
- **THEN** the session record it created is removed by its identifier via the CLI's session-delete mechanism
- **AND** nothing it created persists in the CLI's session store

#### Scenario: The implementer prunes on terminal outcome, not while waiting
- **WHEN** the implementer reaches a terminal outcome (archives/completes OR fails terminally)
- **THEN** the session it created is removed
- **AND** while the change is instead waiting on an AskUser answer, the session is retained (NOT pruned)

#### Scenario: The prune is surgical
- **WHEN** any agentic role prunes the session it created
- **THEN** only that session's record is removed, addressed by its identifier
- **AND** settings, memory/context files, credentials, AND the generated MCP config remain intact

### Requirement: Implementer resumes its session on AskUser; resume failure requeues
On an AskUser outcome, the implementer SHALL submit the question via the outcome relay AND end the run with the change in the waiting state, retaining the agentic session (the cleanup requirement does NOT prune a retained session until the implementer's terminal outcome). When the operator answers, the implementer SHALL resume the same agentic session via the resolved strategy's native headless resume mechanism — `claude` via the captured `session_id`, `opencode` via `--session <id>`, `antigravity` via its session-resume mechanism — delivering the answer into that session.

If the session cannot be restored (not found, corrupt, OR expired by the CLI's own retention), the implementer SHALL NOT fall back to a fresh-run-with-answer. It SHALL treat the attempt as a retryable failure AND requeue the change via the existing failure-counter path (repeated failures escalate per the existing perma-stuck policy). No stash-and-recombine path exists.

#### Scenario: AskUser retains the session and waits
- **WHEN** the implementer returns an AskUser outcome
- **THEN** the question is posted via the outcome relay AND the change enters the waiting state
- **AND** the agentic session is retained

#### Scenario: The operator's answer resumes the same session
- **WHEN** the operator answers a waiting AskUser AND the session is restorable
- **THEN** the implementer resumes that same session via the strategy's native mechanism (`session_id` / `--session` / `--resume`) AND delivers the answer into it

#### Scenario: Resume failure requeues the change with no fallback
- **WHEN** the operator answers but the session cannot be restored (not found / corrupt / expired)
- **THEN** the implementer does NOT start a fresh-run-with-answer
- **AND** the change is requeued as a retryable failure via the existing failure-counter path
- **AND** repeated resume failures escalate under the existing perma-stuck policy
