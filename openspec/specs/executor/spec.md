# executor Specification

## Purpose
TBD - created by archiving change orchestrator-architecture. Update Purpose after archive.
## Requirements
### Requirement: Backend-agnostic execution contract
The orchestrator SHALL invoke implementations through a trait-shaped abstraction that takes a workspace path and an OpenSpec change name and returns an outcome enum. The architecture-level spec does NOT name a concrete backend; concrete implementations (CLI wrappers, MCP-connected agents, future native loops) are introduced by separate implementation changes.

#### Scenario: Successful implementation
- **WHEN** the orchestrator calls `Executor::run(workspace, change_name)` with a valid workspace path and an unarchived change name
- **AND** the underlying backend reports successful completion of the implementation
- **THEN** the call returns `Ok(ExecutorOutcome::Completed)`
- **AND** the workspace working tree contains modifications attributable to the executor, verifiable via `git status --porcelain` returning a non-empty result inside the workspace

#### Scenario: Agent requires clarification
- **WHEN** the underlying backend signals ambiguity through any backend-specific mechanism (tool call, exit code, structured output, etc.)
- **THEN** the call returns `Ok(ExecutorOutcome::AskUser { question, resume_handle })` where `question` is a non-empty human-readable string and `resume_handle` is a value implementing `serde::Serialize` and `serde::Deserialize` so it can be persisted to `.question.json` and restored after a daemon restart
- **AND** no commits are produced on the agent branch as a side effect of the halted implementation
- **AND** the orchestrator (NOT the executor) is responsible for writing `.question.json` and posting the question to ChatOps

#### Scenario: Backend failure
- **WHEN** the underlying backend terminates abnormally (non-zero exit, crash, malformed output, network error, or an enclosing timeout fires)
- **THEN** the call returns `Ok(ExecutorOutcome::Failed { reason })` with a non-empty `reason` string OR `Err(_)` for unrecoverable infrastructure errors that prevent the executor from determining outcome
- **AND** the orchestrator unlocks the change (removes `.in-progress`) and does NOT archive it

### Requirement: Resume after ask-user
The executor SHALL support resuming a previously halted implementation when a human answer becomes available.

#### Scenario: Resuming with an answer
- **WHEN** the orchestrator calls `Executor::resume(resume_handle, answer)` with a `resume_handle` previously returned from `run` and a non-empty `answer` string
- **THEN** the call returns one of `Ok(ExecutorOutcome::Completed)`, `Ok(ExecutorOutcome::AskUser { ... })`, `Ok(ExecutorOutcome::Failed { ... })`, or `Err(_)`, with the same observable side-effect contracts as `run`
- **AND** the orchestrator MUST consume (delete or mark answered) the prior `.question.json` before invoking `resume`, so the executor cannot observe a stale escalation

#### Scenario: Resume after daemon restart
- **WHEN** the orchestrator restarts and finds a `.question.json` file alongside a corresponding `.answer.json` in `<workspace>/openspec/changes/<change>/`
- **THEN** the orchestrator deserializes the stored `resume_handle` from `.question.json` and calls `Executor::resume(handle, answer)` to continue execution
- **AND** the executor backend MUST tolerate a `resume_handle` that was serialized by a prior process invocation

### Requirement: CLI-wrapping executor backend (`claude_cli`)
autocoder SHALL provide a concrete `Executor` implementation that wraps
an external command-line agent tool as a child process. The backend is
selected via `executor.kind: claude_cli` in the configuration. Every
spawn SHALL include the sandbox flags described under "Tool-use
sandbox is applied at every spawn".

#### Scenario: ClaudeCliExecutor instantiation
- **WHEN** autocoder initializes AND `executor.kind` is `claude_cli`
- **THEN** autocoder instantiates a `ClaudeCliExecutor` configured
  from `executor.command` (default `claude`), `executor.timeout_secs`
  (default 1800), and a resolved `ExecutorSandboxConfig` (operator
  value or per-field default)
- **AND** the executor is wrapped in `Arc<dyn Executor>` and shared
  across all spawned polling tasks

#### Scenario: Outcome mapping from CLI exit code
- **WHEN** `Executor::run(workspace, change)` is called
- **THEN** the executor generates the per-iteration sandbox settings
  file in a temp dir, then spawns the configured command as a tokio
  child process inside the workspace with the sandbox flags and
  the prompt on stdin
- **AND** on child exit code 0, the call returns
  `Ok(ExecutorOutcome::Completed)` (the executor does NOT inspect
  the workspace for diff)
- **AND** on non-zero child exit, the call returns
  `Ok(ExecutorOutcome::Failed { reason })` where `reason` contains
  the first 200 characters of captured stderr
- **AND** if the configured `executor.timeout_secs` elapses, the
  child process is killed and the call returns
  `Ok(ExecutorOutcome::Failed { reason: "timeout" })`
- **AND** the temp settings file is deleted after the child exits

#### Scenario: Resume not supported in this phase
- **WHEN** `Executor::resume(handle, answer)` is called on the
  foundation `ClaudeCliExecutor` (prior to the
  `chatops-escalation` change)
- **THEN** the call returns `Err(_)` whose text indicates resume
  is not supported until the `chatops-escalation` change retrofits
  real resume semantics
- **AND** no child process is spawned and no workspace state is
  modified

(Note: in the in-tree implementation today, `resume` is wired
through `chatops-escalation` already. This scenario reflects the
historical foundation-phase contract preserved for spec
continuity. The active `resume` path uses the same sandbox
generation as `run`, per the "Resume applies the same sandbox"
scenario above.)

### Requirement: Executor output persistence and visibility
The `ClaudeCliExecutor` SHALL persist every subprocess invocation's prompt, captured stdout, and captured stderr to a per-change log file outside the workspace, and SHALL emit a WARN-level diagnostic tail when an exit-0 run produced no working-tree changes. Additionally, `build_prompt` SHALL log a WARN naming the reason whenever it falls back to raw-markdown concatenation. The executor SHALL record the spawned child's PID to a sidecar file alongside the busy marker so stuck-state recovery can target the right process group.

#### Scenario: Persistent log file written on every run
- **WHEN** `ClaudeCliExecutor::run` completes a subprocess invocation
  (any outcome: success, non-zero, or timeout)
- **THEN** the prompt sent to the subprocess, the captured stdout, and
  the captured stderr are written to
  `<system-temp>/autocoder/logs/<workspace-basename>/<change>.log`
  where `<workspace-basename>` is the last path component of the
  workspace and `<change>` is the change name
- **AND** the file format is plain text consisting of a
  `=== PROMPT (<p> bytes) ===` header followed by the verbatim
  prompt, a `=== STDOUT (<n> bytes) ===` header followed by the
  verbatim stdout, and a `=== STDERR (<m> bytes) ===` header
  followed by the verbatim stderr
- **AND** any prior contents of that file are overwritten (the file
  represents the most recent run for that change)
- **AND** the parent directories are created on demand
- **AND** errors writing the log file are logged at WARN but do NOT
  fail the executor outcome (logging is best-effort)

#### Scenario: Inline tail logged on suspicious empty-workspace exit
- **WHEN** the subprocess exits 0 AND `git status --porcelain` is
  empty AND no AskUser marker (layer-1) was written AND no
  layer-2 clarification phrase was matched
- **THEN** the executor logs a single WARN-level message naming the
  change and including the trailing ~2KB of stdout and trailing
  ~2KB of stderr (whichever is shorter), so the operator can read
  the agent's apparent reasoning directly from `journalctl` without
  opening the per-change log file
- **AND** the message also includes the per-change log-file path so
  the operator can find the full output if the inline tail is
  truncated mid-thought

#### Scenario: build_prompt logs WARN on each fallback path
- **WHEN** `build_prompt` cannot use `openspec instructions apply`
  output for any reason
- **THEN** the executor logs a WARN naming the change and a
  structured `reason` field whose value is exactly one of:
  `openspec_not_found` (the `openspec` binary could not be spawned,
  typically because it is not on autocoder's PATH),
  `openspec_exited_nonzero` (the binary spawned but returned a
  non-zero exit status), or `openspec_empty_stdout` (the binary
  exited 0 but produced no stdout)
- **AND** in the `openspec_exited_nonzero` case the log also
  includes the exit code and a tail of stderr (up to 200 chars) to
  speed diagnosis
- **AND** `build_prompt` then proceeds with raw-markdown
  concatenation as before, returning a non-empty prompt or an Err
  if no change material exists

#### Scenario: Spawned child runs in its own process group
- **WHEN** `run_subprocess` spawns the wrapped CLI as a child
  process
- **THEN** the child is launched as the leader of a new process
  group via `pre_exec` calling `setsid()` (Unix), so the per-repo
  busy marker can record the child's PGID and the daemon can use
  `killpg(pgid, signal)` to terminate the entire subprocess tree
  (including any MCP servers spawned by the agent) if a stuck
  state is detected
- **AND** this has no effect on the executor's normal
  exit-mapping behavior; it only enables process-group signaling
  during stuck-state recovery

#### Scenario: Subprocess sidecar file tracks Claude's PID
- **WHEN** `run_subprocess` successfully spawns the wrapped CLI
- **THEN** the executor writes the child's PID (which equals its
  PGID because of `process_group(0)`) to
  `<system-temp>/autocoder/busy/<workspace-basename>.subprocess`
  as plain decimal text followed by a newline
- **AND** the file is removed when the child exits (RAII guard
  scoped to `run_subprocess`)
- **AND** a daemon crash that bypasses the guard leaves the
  sidecar file in place, so the next pass's busy-marker stuck-
  state recovery can read it and `killpg` the orphaned subprocess
  tree (the original busy marker's `pgid` field records autocoder's
  group, which is not the kill target an orphaned subprocess
  requires)
- **AND** errors writing the sidecar file are logged at WARN but
  do NOT fail the executor outcome

### Requirement: Implementer prompt template loading
The executor SHALL load an implementer prompt template at construction. The template wraps the openspec change content with a role-establishing imperative so the wrapped CLI knows it is acting as an autonomous implementer and not a chat assistant. The default template is compiled into the binary; deployments may override it by setting `executor.implementer_prompt_path` in `config.yaml` to a readable file path.

#### Scenario: Default template used when override is absent
- **WHEN** `executor.implementer_prompt_path` is unset in `config.yaml`
- **THEN** the executor uses the template compiled into the binary
  (sourced from `prompts/implementer.md` at build time)
- **AND** no filesystem access for the template occurs at runtime

#### Scenario: Override path is loaded at construction
- **WHEN** `executor.implementer_prompt_path` is set to a file path
- **THEN** the executor reads the file at construction (before the
  polling loop starts) and uses its contents as the template
- **AND** if the file is missing, unreadable, or empty, daemon
  startup fails with an error message naming the path

#### Scenario: Template substitution
- **WHEN** the executor renders the prompt for a change
- **THEN** every literal occurrence of `{{change_body}}` in the
  template is replaced with the output of
  `openspec instructions apply --change <change>`
- **AND** the rendered prompt is sent to the wrapped CLI on stdin

### Requirement: Tool-use sandbox is applied at every spawn
The CLI-wrapping executor backend SHALL apply tool-use restrictions to
every spawned child process via a per-iteration Claude Code settings
file derived from `executor.sandbox` config. The settings file is
generated in the OS temp directory (not the workspace), passed to
the spawned CLI via `--settings <path>`, and deleted after the child
exits.

#### Scenario: Default sandbox applies when block is absent
- **WHEN** `config.yaml` has no `executor.sandbox` block
- **THEN** at each `run` and `resume` invocation, the executor
  generates a temp Claude Code settings file containing the
  default-deny patterns for network commands and credential paths,
  AND spawns `claude` with `--settings <temp-path>
  --allowedTools <default-list> --permission-mode acceptEdits` as
  additional flags
- **AND** the default-deny list contains at minimum
  `Bash(curl:*)`, `Bash(wget:*)`, `Bash(ssh:*)`,
  `Bash(scp:*)`, `Bash(nc:*)`, `Bash(git push:*)`,
  `Bash(git remote *)`, `Read(/home/*/.ssh/**)`,
  `Read(/home/*/.claude/**)`

#### Scenario: Operator-customized sandbox is honored
- **WHEN** `config.yaml`'s `executor.sandbox` block explicitly lists
  `allowed_tools`, `disallowed_bash_patterns`, AND
  `disallowed_read_paths`
- **THEN** the generated settings file's `permissions.deny` contains
  exactly the operator's `Bash(...)` and `Read(...)` patterns
- **AND** the `--allowedTools` flag value is exactly the operator's
  `allowed_tools` list joined by commas
- **AND** no default values are merged in (operators express the
  full intended list)

#### Scenario: Partially-specified sandbox falls back to defaults per-field
- **WHEN** `executor.sandbox` is present but omits one of the three
  fields (e.g. specifies `allowed_tools` but not
  `disallowed_bash_patterns`)
- **THEN** the omitted field defaults to its safe baseline
- **AND** the specified field uses the operator's value verbatim

#### Scenario: Settings file is per-iteration and cleaned up
- **WHEN** the executor spawns the child
- **THEN** the settings file path is in the OS temp directory
  (`std::env::temp_dir()`), not inside the workspace
- **AND** the file is deleted after the child exits, regardless of
  exit status
- **AND** failure to delete the temp file is logged at warn level
  but does NOT propagate as an error

#### Scenario: Resume applies the same sandbox
- **WHEN** `Executor::resume(handle, answer)` spawns the child
- **THEN** the same sandbox-flag-and-settings-file generation runs,
  with the same defaults / operator config as the original `run`
  call

### Requirement: Sandbox config schema
autocoder SHALL accept an optional `executor.sandbox` block with three
optional sub-fields, each with a documented safe default applied when
absent. The default `disallowed_bash_patterns` SHALL include patterns
blocking openspec state-mutation operations so the executor cannot
short-circuit a change by archiving it.

#### Scenario: `allowed_tools` field
- **WHEN** `executor.sandbox.allowed_tools` is set
- **THEN** the value is a YAML list of Claude Code tool names (e.g.
  `["Read", "Write", "Edit", "Glob", "Grep", "Bash"]`)
- **AND** the value is passed verbatim to the `--allowedTools` flag
  joined by commas

#### Scenario: `disallowed_bash_patterns` field
- **WHEN** `executor.sandbox.disallowed_bash_patterns` is set
- **THEN** each entry becomes `Bash(<pattern>)` in the generated
  settings file's `permissions.deny` array

#### Scenario: `disallowed_read_paths` field
- **WHEN** `executor.sandbox.disallowed_read_paths` is set
- **THEN** each entry becomes `Read(<pattern>)` in the generated
  settings file's `permissions.deny` array

#### Scenario: Default `allowed_tools`
- **WHEN** `executor.sandbox.allowed_tools` is absent
- **THEN** the default is `["Read", "Write", "Edit", "Glob", "Grep", "Bash"]`
- **AND** notable exclusions are `WebFetch` and `WebSearch`

#### Scenario: Default `disallowed_bash_patterns` includes network egress
- **WHEN** `executor.sandbox.disallowed_bash_patterns` is absent
- **THEN** the default includes at minimum: `curl:*`, `wget:*`,
  `nc:*`, `ncat:*`, `netcat:*`, `ssh:*`, `scp:*`, `sftp:*`,
  `rsync:*`, `git push:*`, `git remote *`, `git fetch *://*`

#### Scenario: Default `disallowed_bash_patterns` blocks openspec state mutation
- **WHEN** `executor.sandbox.disallowed_bash_patterns` is absent
- **THEN** the default also includes `openspec archive:*` AND
  `openspec unarchive:*`
- **AND** read-only `openspec` operations (validate, list, status,
  show, instructions) are NOT in the denylist; the executor needs
  them to inspect change state

#### Scenario: Default `disallowed_read_paths`
- **WHEN** `executor.sandbox.disallowed_read_paths` is absent
- **THEN** the default includes at minimum: `/home/*/.ssh/**`,
  `/home/*/.claude/**`, `/etc/shadow`, `/etc/ssl/private/**`

### Requirement: Sandbox does not bind the code-reviewer
The tool-use sandbox SHALL apply only to the executor's spawned
agent CLI subprocess, NOT to the code-reviewer's LLM API calls. The
code-reviewer operates via direct HTTP requests under operator
configuration (provider, api_key, api_base_url, model) and is a
separate data flow.

#### Scenario: Reviewer call is unaffected by sandbox
- **WHEN** the code-reviewer is enabled AND
  `code_reviewer::review(diff, summary)` is called
- **THEN** the HTTP call to the configured LLM provider proceeds
  per the reviewer's config without consulting
  `executor.sandbox`
- **AND** the diff content (which the operator's reviewer config
  authorized for upload) is sent as configured

### Requirement: Executor invokes Claude CLI in JSON event streaming mode and captures events to a structured log
When `executor.output_format` is `"json"` (the default), the executor SHALL invoke the wrapped Claude CLI with the `--output-format stream-json` argument (or whatever flag name Claude CLI's current release uses for line-delimited JSON event output). The executor SHALL spawn a streaming reader task that reads stdout line-by-line, parses each line as a JSON event, AND dispatches the parsed event to a `StructuredLogWriter` that builds TWO sibling files per change:

- **Summary log** at `<logs_dir>/runs/<basename>/<change>.log` containing `PROMPT`, `ACTIONS` (replaced with a single pointer line, NOT the action stream), `FINAL ANSWER`, AND `STDERR` sections in that order. The ACTIONS slot SHALL contain exactly one line: `=== ACTIONS (see <change>.stream.log) ===`. Operators reading the summary log see a short, signal-dense file with the agent's prompt input AND the agent's deliberate end-of-run emission, plus a pointer to where the verbose action stream lives.
- **Stream log** at `<logs_dir>/runs/<basename>/<change>.stream.log` containing the verbose action stream — `[tool_use] ...`, `[tool_result] (N bytes returned)`, `[assistant] ...`, `[raw] ...`, `[unknown:<type>] ...` lines as today's single-file ACTIONS section. No section headers. One continuous stream.

Dispatch routing happens at event-classification time inside the writer; no buffering of the full stream in memory is required. The streaming approach guarantees that on timeout-kill, both files already contain every event the child emitted before the kill — the summary log is structurally complete (all four section headers present) AND the stream log contains whatever action events arrived.

Daemon-internal consumers of per-change log content SHALL NOT read the stream log for daemon-meaningful markers. The PR-comment composer reads the summary log's FINAL ANSWER section (per the canonical "PR-comment Agent implementation notes body uses the FINAL ANSWER" requirement). The sentinel scanner reads `outcome.final_answer` directly from the executor's structured outcome (per the `a20a1`-narrowed scoping). The stream log is operator-diagnostic only.

#### Scenario: Successful JSON run produces structured log
- **WHEN** Claude CLI is invoked with JSON streaming mode AND the run completes successfully
- **THEN** the summary log file contains four section markers in order: `=== PROMPT (<n> bytes) ===`, `=== ACTIONS (see <change>.stream.log) ===`, `=== FINAL ANSWER (<n> bytes) ===`, `=== STDERR (<n> bytes) ===`
- **AND** the stream log file contains formatted lines for each tool_use, tool_result, and intermediate assistant text block in the run
- **AND** the FINAL ANSWER section in the summary log contains the text from the `result` event that closes the run
- **AND** the summary log's ACTIONS slot contains ONLY the pointer line — no `[tool_*]` or `[assistant]` content

#### Scenario: Timeout-killed run preserves the ACTIONS up to the kill
- **WHEN** Claude CLI emits N events on stdout AND autocoder's timeout enforcement kills the child before the `result` event arrives
- **THEN** the stream log file contains the N events that arrived
- **AND** the summary log's FINAL ANSWER section is empty (the `result` event never arrived to populate it)
- **AND** both files are structurally complete: the summary log has all four section headers with size annotations updated; the stream log contains whatever lines arrived before the kill

#### Scenario: Malformed JSON line lands in the stream log as raw
- **WHEN** the stdout reader receives a line that fails JSON parsing
- **THEN** the line is appended to the stream log as `[raw] <line content>`
- **AND** a WARN log is emitted naming the malformed line
- **AND** subsequent lines continue to be parsed normally
- **AND** the summary log is unaffected (the line does not appear in any of its sections)

#### Scenario: Unknown event type lands in the stream log as unknown
- **WHEN** the stdout reader receives a JSON event whose `type` field doesn't match a known variant
- **THEN** the event is appended to the stream log as `[unknown:<type>] <raw json>`
- **AND** subsequent events continue to be processed normally
- **AND** the summary log is unaffected

#### Scenario: Zero-action run still creates both files
- **WHEN** a run completes with zero `tool_use` / `tool_result` events AND no intermediate assistant text (e.g. the agent processed the prompt purely via internal reasoning AND emitted only a `result` event)
- **THEN** the summary log is created with all four section markers
- **AND** the stream log is created AS AN EMPTY FILE (no `[tool_*]` lines) so the operator's `<change>.stream.log` path resolves AND the diagnostic-consistency invariant holds
- **AND** the summary log's ACTIONS pointer line still reads `=== ACTIONS (see <change>.stream.log) ===`

#### Scenario: Stream log path is sibling to summary log
- **WHEN** the writer creates the per-change log files for change `<slug>` in workspace `<basename>`
- **THEN** the summary log path is `<logs_dir>/runs/<basename>/<slug>.log`
- **AND** the stream log path is `<logs_dir>/runs/<basename>/<slug>.stream.log`
- **AND** the two paths share the same parent directory

### Requirement: PR-comment "Agent implementation notes" body uses the FINAL ANSWER, not the action stream
The polling-loop code that constructs the `## Agent implementation notes` PR comment SHALL read the FINAL ANSWER section's content from the per-change log file AND use it as the comment body. The ACTIONS section's content (tool calls, intermediate assistant text) SHALL NOT appear in the PR comment under any circumstance — it is operator-diagnostic content only. When the FINAL ANSWER section is empty (timeout case OR any other reason the run didn't reach the `result` event), the comment body uses the fallback string `(executor timed out before final summary; see daemon log for action stream)`.

#### Scenario: Successful run's PR comment matches FINAL ANSWER exactly
- **WHEN** a successful change's log file has a FINAL ANSWER section with text `<X>`
- **THEN** the PR's "Agent implementation notes" comment body for that change is `<X>` (verbatim, modulo Markdown formatting around it)
- **AND** the comment body does NOT contain any tool_use, tool_result, or intermediate assistant text from the ACTIONS section

#### Scenario: Empty FINAL ANSWER uses the fallback string
- **WHEN** a change's log file's FINAL ANSWER section is empty (timeout-kill before the run completed)
- **THEN** the comment body is `(executor timed out before final summary; see daemon log for action stream)`
- **AND** the PR is created normally if any commits landed; the comment just notes the missing summary

### Requirement: Per-change log files are pruned after `executor.log_retention_days` days, preserving active-change logs
At daemon startup AND once every 24 hours during operation, the daemon SHALL run a retention pass over the per-change log directory. A summary log file `<change>.log` SHALL be eligible for deletion when its modification time is older than `now - log_retention_days * 86400` seconds AND its corresponding change directory at `<workspace>/openspec/changes/<change>/` does NOT exist (the change has been archived OR removed). Logs for changes that are STILL active SHALL be preserved regardless of age. The default `log_retention_days` value is `30`; operator-configurable; clamped at `365`.

The retention pass operates on log-file PAIRS: when a summary log is eligible for deletion, the sibling `<change>.stream.log` file (if present) SHALL be deleted in the same retention pass. The order is summary-first, then stream; partial-success cases (summary deleted, stream-delete failed due to filesystem error) log WARN naming the orphan AND the retention pass continues processing remaining changes. Active-change preservation extends to the pair: when `<change>.log` is preserved, its sibling stream log is also preserved.

An orphan stream log (a `<change>.stream.log` file present WITHOUT its summary log — e.g. from a partial pre-spec migration OR manual operator action) SHALL be eligible for deletion when its OWN mtime exceeds the retention window AND no `openspec/changes/<change>/` directory exists. Orphan cleanup logs WARN naming the file so operators see the cleanup happen.

#### Scenario: Stale log for archived change is deleted
- **WHEN** the retention pass runs AND a summary log file `<change>.log` has mtime more than `log_retention_days` days ago AND no `openspec/changes/<change>/` directory exists for it
- **THEN** the summary log file is deleted
- **AND** the sibling `<change>.stream.log` is also deleted in the same pass (if present)
- **AND** the retention report's `files_deleted` count includes both files (counted separately)

#### Scenario: Old log for active change is preserved
- **WHEN** a summary log file is older than the retention window AND its change directory still exists in the active path
- **THEN** the summary log file is NOT deleted
- **AND** the sibling stream log file is also NOT deleted
- **AND** the retention report's `files_preserved` count includes both files

#### Scenario: Recent log is preserved regardless of change state
- **WHEN** a summary log file's mtime is within the retention window
- **THEN** the summary log file is NOT deleted regardless of whether the change is active or archived
- **AND** the sibling stream log file is also NOT deleted

#### Scenario: Orphan stream log cleanup
- **WHEN** the retention pass encounters a `<change>.stream.log` file whose corresponding summary log `<change>.log` does NOT exist AND whose mtime exceeds the retention window AND whose change directory does NOT exist
- **THEN** the orphan stream log file is deleted
- **AND** a WARN log fires naming the orphan path AND noting the cleanup
- **AND** the retention report's `files_deleted` count includes the orphan

#### Scenario: Partial-success on stream deletion logs WARN
- **WHEN** the summary log is deleted successfully BUT the sibling stream log deletion fails (e.g. permission denied, transient filesystem error)
- **THEN** a WARN log fires naming the orphan stream log path
- **AND** the retention pass continues processing remaining changes (no abort)
- **AND** the next retention pass picks up the orphan via the orphan-cleanup scenario above

### Requirement: `executor.output_format: "text"` preserves the legacy at-exit capture behavior
When `executor.output_format` is `"text"`, the executor SHALL omit the `--output-format stream-json` flag from the spawn command AND fall back to today's at-exit-capture pattern. The log file shape uses the legacy `=== STDOUT ===` / `=== STDERR ===` section names instead of the new `=== ACTIONS ===` / `=== FINAL ANSWER ===` shape. The PR-comment construction path detects the legacy section names AND reads raw stdout as the comment body (today's behavior).

#### Scenario: Text-mode opt-out uses legacy log shape
- **WHEN** the config has `executor.output_format: "text"`
- **THEN** the spawn command lacks `--output-format stream-json`
- **AND** the log file uses `=== STDOUT (<n> bytes) ===` and `=== STDERR (<n> bytes) ===` section names
- **AND** the PR-comment construction path reads raw stdout from the STDOUT section as the comment body

#### Scenario: Text-mode opt-out on timeout produces today's zero-bytes outcome
- **WHEN** the config has `executor.output_format: "text"` AND a run times out
- **THEN** the log file's STDOUT section reads `=== STDOUT (0 bytes) ===` (the legacy behavior of losing the buffer on kill is preserved verbatim)

### Requirement: Per-execution MCP child exposes `query_canonical_specs` tool via control-socket relay
The per-execution stdio MCP server (the child process autocoder launches per polling iteration via `.mcp.json`, currently `autocoder/src/mcp_askuser_server.rs`) SHALL advertise a `query_canonical_specs` tool alongside the existing `ask_user` tool. The tool's surface as seen by the wrapped agent:

- Name: `query_canonical_specs`.
- Input schema: `{ query: string, top_k?: number }`. `query` is required. `top_k` defaults to `canonical_rag.top_k` from the daemon's config (default 10), clamped per the orchestrator spec.
- Output: a JSON object `{ hits: Array<RagHit>, error_hint?: string }` where each `RagHit` is shaped `{ capability: string, requirement_title: string, requirement_body: string, scenario_titles: string[], relevance_score: number }`, sorted by descending `relevance_score`.

The tool's handler SHALL NOT compute results locally. Instead it SHALL relay the request to the daemon via the existing control socket (per the canonical `orchestrator-cli` "Control socket for runtime daemon interaction" requirement) using a new `query_canonical_specs` action defined in the orchestrator-cli spec deltas. The daemon owns the `CanonicalRagStore` AND answers via its in-memory state; the MCP child is a thin synchronous relay.

The relay is configured via two env vars set by `ClaudeCliExecutor::write_mcp_config` when launching the MCP child:

- `ORCH_DAEMON_CONTROL_SOCKET` — absolute path to the daemon's Unix-domain control socket. When absent (i.e., RAG is not configured for this workspace), the tool returns `{ hits: [], error_hint: "rag not configured for this execution" }` AND does NOT attempt a socket connection.
- `ORCH_MCP_WORKSPACE_BASENAME` — the sanitized basename the daemon uses as the `CanonicalRagStore` registry key. Routed verbatim into the control-socket request.

Connection timeout: 10 seconds. On timeout OR socket error, the tool returns `{ hits: [], error_hint: "control socket unreachable: <error>" }` AND surfaces the error so the agent can fall back to non-RAG behavior. The control-socket relay is fail-open in every error path; the agent never blocks indefinitely AND never sees a tool-call failure.

The implementer prompt template (`prompts/implementer.md`) SHALL contain a paragraph naming the tool AND describing when to use it (working on a capability with a canonical spec). Operators with custom implementer prompt overrides MAY remove the mention to suppress agent use; the tool stays registered regardless, just unused.

#### Scenario: Tool advertised in the MCP child's `tools/list`
- **WHEN** an agent connects to the MCP child AND sends a `tools/list` request
- **THEN** the response lists BOTH `ask_user` (existing) AND `query_canonical_specs` (new)
- **AND** `query_canonical_specs`'s `inputSchema` matches the documented `{ query: string, top_k?: number }` shape

#### Scenario: Tool returns ranked hits via control-socket relay
- **WHEN** an agent invokes `query_canonical_specs({ query: "audit framework cadence", top_k: 5 })`
- **AND** `ORCH_DAEMON_CONTROL_SOCKET` AND `ORCH_MCP_WORKSPACE_BASENAME` are set in the child's env
- **AND** the daemon has a `CanonicalRagStore` registered for that workspace_basename
- **THEN** the MCP child opens a connection to the socket AND sends `{"action":"query_canonical_specs","workspace_basename":"<basename>","query":"audit framework cadence","top_k":5}`
- **AND** the daemon's handler returns `{"ok":true,"hits":[...]}` with up to 5 results
- **AND** the MCP child returns the `hits` array to the agent as the tool-call result

#### Scenario: RAG not configured — tool returns empty with hint
- **WHEN** the workspace's config has no `canonical_rag:` block (RAG disabled)
- **AND** `ClaudeCliExecutor::write_mcp_config` omits `ORCH_DAEMON_CONTROL_SOCKET` from the spawn env
- **AND** an agent invokes `query_canonical_specs({ query: "..." })`
- **THEN** the tool returns `{ hits: [], error_hint: "rag not configured for this execution" }`
- **AND** no socket connection is attempted

#### Scenario: Control socket unreachable — tool returns empty with hint
- **WHEN** `ORCH_DAEMON_CONTROL_SOCKET` is set BUT the socket is unreachable (file missing, permission denied, daemon down)
- **AND** an agent invokes `query_canonical_specs({ query: "..." })`
- **THEN** the tool returns `{ hits: [], error_hint: "control socket unreachable: <error>" }`
- **AND** the connect attempt times out after 10 seconds at most

#### Scenario: Store missing for workspace — daemon surfaces hint
- **WHEN** RAG is configured BUT workspace-init's embed call failed earlier (provider unreachable)
- **AND** the daemon's `CanonicalRagStore` registry has no entry for the workspace_basename
- **AND** an agent invokes `query_canonical_specs({ query: "..." })`
- **THEN** the daemon's control-socket handler returns `{"ok":true,"hits":[],"error_hint":"rag init failed; see daemon log"}`
- **AND** the MCP child surfaces the hint to the agent
- **AND** the daemon log contains the original failure's WARN line for operator diagnosis

#### Scenario: Per-workspace isolation enforced by daemon
- **WHEN** two workspaces are managed by the same daemon AND both have `CanonicalRagStore` registered
- **AND** an MCP child spawned for workspace 1 (with its `ORCH_MCP_WORKSPACE_BASENAME` env var set to workspace 1's basename) invokes `query_canonical_specs(...)`
- **THEN** the control-socket request carries workspace 1's basename
- **AND** the daemon's handler queries ONLY workspace 1's store
- **AND** workspace 2's entries do NOT appear in the response
- **AND** the routing is enforced by the daemon, not the child (the child cannot accidentally query another workspace's store even if its env var is spoofed — the daemon's handler is the source of truth)

#### Scenario: Default `top_k` from config when omitted
- **WHEN** an agent invokes `query_canonical_specs({ query: "..." })` with NO `top_k` argument
- **AND** `canonical_rag.top_k` is set to `15`
- **THEN** the control-socket request omits `top_k`; the daemon's handler applies the config default
- **AND** the tool returns up to 15 results
- **AND** the agent's explicit `top_k` (when present) overrides the config default

#### Scenario: Implementer prompt mentions the tool
- **WHEN** the daemon assembles the implementer prompt for an executor invocation
- **AND** the embedded `prompts/implementer.md` (OR an operator override) is loaded
- **THEN** the prompt contains a paragraph naming `query_canonical_specs` AND its purpose (retrieve canonical-spec chunks for the change's capability context)
- **AND** the operator MAY override the prompt template to remove the mention if they prefer the agent not call the tool — the tool stays registered in the MCP child regardless, just unused

### Requirement: Prompt loader applies a uniform embedded → per-workspace → daemon-level → embedded fallback precedence
The daemon SHALL load every embedded prompt template through a single `PromptLoader` helper. The loader SHALL accept a `PromptId` enum value (one variant per embedded prompt) AND the resolved per-repo configuration, AND SHALL return the prompt's content string. For each `(PromptId, config)` call the loader SHALL resolve in this precedence:

1. The per-workspace override path (when configured AND the file exists at the workspace-relative location).
2. The per-workspace LEGACY flat-name path (when the modernized nested form is unset AND a legacy field exists for this prompt AND its file exists).
3. The daemon-level legacy override path (when set AND the file exists).
4. The embedded default loaded via `include_str!` at compile time.

When a configured override path is present BUT the file at that path does NOT exist, the loader SHALL log a one-shot WARN naming the `(PromptId, missing-path)` pair AND fall through to the next precedence level. The one-shot tracking SHALL persist for the daemon's lifetime; repeated loads of the same `(PromptId, path)` SHALL NOT re-emit the WARN.

Every consumer of an embedded prompt — audits, the implementer executor mode, the implementer-revision flow, the code reviewer, the changelog stylist, the audit-triage flow, the chat-request-triage flow, the brownfield handler, AND any prompt added by future changes — SHALL invoke `PromptLoader::load(PromptId::X, &workspace_config)` instead of inlining `include_str!` at the call site.

#### Scenario: Embedded default loads when no override configured
- **WHEN** the workspace config has no override for `PromptId::Implementer` AND no daemon-level legacy field is set
- **THEN** `PromptLoader::load(PromptId::Implementer, &cfg)` returns the `include_str!`-embedded `prompts/implementer.md` contents

#### Scenario: Per-workspace nested override wins
- **WHEN** the workspace config has `executor.implementer.prompt_path: "./prompts/implementer-custom.md"` AND that file exists
- **THEN** the loader returns the file's contents
- **AND** does NOT consult the embedded default OR any legacy field

#### Scenario: Legacy daemon-level override applies when no per-workspace override exists
- **WHEN** the workspace config has no `executor.implementer.prompt_path` AND no `executor.implementer_prompt_path` AND the daemon-level config has `executor.implementer_prompt_path: /etc/autocoder/implementer.md` AND that file exists
- **THEN** the loader returns the daemon-level file's contents

#### Scenario: Per-workspace overrides preempt daemon-level legacy
- **WHEN** the workspace config has `executor.implementer.prompt_path: "./workspace-implementer.md"` AND the daemon-level config has `executor.implementer_prompt_path: /etc/autocoder/implementer.md` AND both files exist
- **THEN** the loader returns the workspace file's contents
- **AND** the daemon-level path is not read

#### Scenario: Missing override file logs WARN once and falls back
- **WHEN** the workspace config has `executor.implementer.prompt_path: "./missing.md"` AND that file does NOT exist
- **THEN** the loader logs a WARN naming `PromptId::Implementer` AND the missing path
- **AND** falls through to the next precedence level (daemon-level, then embedded)
- **WHEN** the same `(PromptId::Implementer, "./missing.md")` is loaded again later in the daemon's lifetime
- **THEN** no further WARN is logged

#### Scenario: Each embedded prompt has a `PromptId` variant
- **WHEN** the test suite enumerates `prompts/*.md` files via `std::fs::read_dir` at test time
- **THEN** every file corresponds to exactly one `PromptId` enum variant
- **AND** the registry-completeness test fails if a `prompts/<new>.md` file is added without a matching variant

### Requirement: `executor.audit_triage.prompt_path`, `executor.chat_request_triage.prompt_path`, AND `executor.implementer_revision.prompt_path` are per-workspace overrides for the three previously-unoverridable prompts
The per-repo config schema SHALL accept three new optional override blocks under `executor`:

- `audit_triage.prompt_path: Option<String>` — override for `prompts/audit-triage.md` (used by the polling-iteration triage flow that handles `send it` requests).
- `chat_request_triage.prompt_path: Option<String>` — override for `prompts/chat-request-triage.md` (used by the polling-iteration triage flow that handles `propose` requests).
- `implementer_revision.prompt_path: Option<String>` — override for `prompts/implementer-revision.md` (used by the implementer when iterating on revision-loop comments).

Each path is workspace-relative when set. Each defaults to `None`. The `PromptLoader` resolves them per the uniform precedence above.

#### Scenario: audit_triage override resolves
- **WHEN** the workspace config has `executor.audit_triage.prompt_path: "./prompts/triage-custom.md"` AND the file exists
- **THEN** the polling iteration's triage invocation loads the override
- **AND** the LLM receives the custom template

#### Scenario: chat_request_triage override resolves
- **WHEN** the workspace config has `executor.chat_request_triage.prompt_path: "./prompts/chat-triage-custom.md"` AND the file exists
- **THEN** the polling iteration's `propose`-flow triage invocation loads the override

#### Scenario: implementer_revision override resolves
- **WHEN** the workspace config has `executor.implementer_revision.prompt_path: "./prompts/revision-custom.md"` AND the file exists
- **THEN** the implementer-revision flow loads the override

#### Scenario: Missing override path falls back to embedded
- **WHEN** any of the three new override paths is configured to a path that does NOT exist
- **THEN** the loader logs the one-shot WARN per the uniform precedence
- **AND** the embedded default is used

### Requirement: New prompts SHALL declare their override field via the nested naming convention
Any new embedded prompt added in future changes SHALL declare its override field using the nested `<area>.<thing>.prompt_path` form (matching `audits.settings.<slug>.prompt_path` AND `features.brownfield.prompt_path` AND the three new fields above). Flat suffix forms (`<area>.<thing>_prompt_path`) MAY remain in use ONLY for the existing legacy fields documented in the registry; new prompts SHALL NOT introduce additional flat-suffix overrides.

#### Scenario: New prompt adds nested override field
- **WHEN** a future change introduces a new embedded prompt (e.g., `prompts/scout.md`)
- **THEN** its override field is named `<area>.scout.prompt_path` (nested), NOT `<area>.scout_prompt_path` (flat)

#### Scenario: Existing legacy fields remain accepted
- **WHEN** an operator config sets `executor.implementer_prompt_path` (the legacy flat field)
- **THEN** the config parses successfully AND the loader honors the field per the uniform precedence
- **AND** no deprecation error fires (the field is accepted indefinitely for backward compatibility)

### Requirement: Revision prompt is constructed from PR-sourced material; no degraded-prompt fallback is permitted
The executor's revision-mode prompt builder SHALL construct its prompt body solely from material sourced from the PR being revised. The pre-`a20a5` approach — calling `openspec instructions apply --change <X>` against the workspace's current state to load "the original change material" — SHALL be removed entirely. The workspace's current state at the moment the revise dispatcher runs is the agent branch's tip, which by the canonical "Implementer prompt template loading" requirement's instruction (`openspec archive is denied in this sandbox. Leave the working tree dirty — autocoder will commit your diff and archive on success.`) always contains the post-archive layout where `openspec/changes/<X>/` does not exist. The `openspec instructions apply` call therefore could never succeed for any change that had ever been in a PR — the placeholder fallback the pre-`a20a5` code fired in this case constituted a degraded-prompt path operating in 100% of production revise invocations.

The revision prompt template (`prompts/implementer-revision.md`) SHALL define five placeholders, all required:

- `{{pr_body}}` — the PR's full body text verbatim. Contains the `## Code Review` section (when the reviewer is enabled) AND the "Changes implemented in this pass" section.
- `{{pr_change_list}}` — newline-separated change slugs extracted from the PR body via the existing `extract_change_list_from_pr_body` helper.
- `{{agent_implementation_notes}}` — concatenated `## Agent implementation notes` issue-comment bodies from the PR, in posted order, separated by `\n\n---\n\n`. These are the canonical implementer-summary comments mandated by the `Implementer-summary PR comment` requirement; one is posted per change in multi-change passes.
- `{{revision_diff}}` — the PR's unified diff (existing field; unchanged). Contains the spec deltas via the archive moves.
- `{{revision_request}}` — the operator's revision text from the triggering PR comment (existing field; unchanged).

The template's prose SHALL instruct the LLM to:

- Identify which change(s) in `{{pr_change_list}}` the operator's `{{revision_request}}` targets. If the request names a specific slug, target that change. If the request is generic (does not name a slug), apply the revision to the change(s) whose content matches the request.
- Use `{{revision_diff}}` as the implementation already in flight; the revision modifies that diff rather than producing a fresh implementation.
- Use `{{agent_implementation_notes}}` to understand what the original implementer claimed to do, which is the gap the operator is closing.
- Use the code review portion of `{{pr_body}}` (when present) to understand what the reviewer flagged.

The builder SHALL NOT substitute placeholder text, fallback strings, OR "best-effort" content for any of the five placeholders. If the caller cannot provide all five inputs as non-error values, the caller SHALL NOT invoke the builder; the dispatcher refusal path defined in `orchestrator-cli` handles that case. This invariant — **no degraded-prompt path is permitted for missing required input** — applies to every prompt builder in autocoder, not only revision-mode. Future prompt builders SHALL inherit the same discipline at their construction sites.

#### Scenario: Builder substitutes all five placeholders from RevisionContext
- **WHEN** `build_revision_prompt` is called with a `RevisionContext` carrying populated `pr_body`, `pr_change_list`, `agent_implementation_notes`, `pr_diff`, AND `revision_text` fields
- **THEN** the rendered prompt contains the verbatim content of all five fields in their template positions
- **AND** the rendered prompt contains NO instance of the pre-`a20a5` placeholder string `_(original change material unavailable — ...)_`
- **AND** the rendered prompt contains NO instance of the pre-`a20a5` `{{change_body}}` placeholder name

#### Scenario: Builder does not invoke openspec
- **WHEN** an automated test wraps `build_revision_prompt` with a process-spawn observer
- **THEN** no `openspec` subprocess is spawned during prompt construction
- **AND** no `Command::new("openspec")` call is reachable from the revision-prompt code path

#### Scenario: Template documents the multi-change resolution rule
- **WHEN** a maintainer reads `prompts/implementer-revision.md`
- **THEN** the template's prose explicitly instructs the LLM on the multi-change resolution: name-match the operator's request to a slug, OR apply the request to all listed changes if no slug is named
- **AND** the template instructs the LLM to leave the workspace dirty for autocoder to commit; the LLM does NOT invoke `git` or `openspec archive` directly

#### Scenario: Operator-override revision templates inherit the new placeholder set
- **WHEN** an operator configures `executor.implementer_revision.prompt_path` (per `a24`'s uniform PromptLoader pattern) pointing at a custom revision-prompt template
- **AND** that template contains the new five placeholders
- **THEN** the builder substitutes them per the standard substitution rules
- **AND** operators migrating from pre-`a20a5` templates see a clear documentation pointer in `docs/CONFIG.md`'s Prompt overrides table (`a24`) naming the placeholder migration

### Requirement: Prompt construction is gated by an explicit availability check at the caller
For every embedded prompt template the daemon ships (revision-mode, implementer-mode, audit-triage, chat-request-triage, brownfield-draft, scout, documentation-audit, sentinel emission), the call site that invokes `build_X_prompt(...)` SHALL first verify that every required input is available as a non-error value. Missing-input cases SHALL be handled by the caller — typically by posting an operator-facing message via the appropriate channel (PR comment, chatops post, control-socket reply) AND refusing to invoke the executor — NOT by the builder substituting placeholder content.

This requirement is the architectural invariant that prevents the `a20a5`-fixed bug class from recurring. The construction-site discipline mirrors the `a20a4` head-qualifier pattern: explicit checks at the site where the dependency is consumed, no silent fallback inside the helper.

#### Scenario: Future prompt builder rejects placeholder fallback
- **WHEN** a future change introduces a new prompt builder (e.g. `build_scout_prompt`, `build_brownfield_survey_prompt`)
- **THEN** the builder's contract documents that every required input must be provided AS a non-error value
- **AND** the builder does NOT contain any "best-effort," "fall back to placeholder," OR "substitute stub" code path for missing required input
- **AND** every call site of the builder is preceded by explicit availability checks for every required input

#### Scenario: Code review surfaces violations of the construction-site discipline
- **WHEN** a future change introduces code that mutates a prompt builder to accept a `None` for what was previously a required input
- **THEN** the reviewer (per the canonical code-reviewer flow) flags the change as violating this requirement
- **AND** the canonical reference to "no degraded-prompt path" appears in the review feedback so the maintainer can locate the architectural reason

### Requirement: Per-execution MCP child exposes outcome tools via control-socket relay

The per-execution stdio MCP server (the child process autocoder launches per polling iteration via `.mcp.json`, currently `autocoder/src/mcp_askuser_server.rs`) SHALL advertise two outcome-signaling tools alongside the existing `ask_user` AND `query_canonical_specs` tools:

- **`outcome_success`** — the implementer's explicit successful-completion signal.
  - Input schema: `{ final_answer?: string }`. The optional `final_answer` carries the implementer's end-of-run summary text (the content that today's JSON-streaming `result` event provides) for log capture AND PR-comment rendering. When omitted, the daemon uses an empty string.
  - Output: a JSON object `{ ok: true }`. The agent does NOT need to inspect the result; calling the tool IS the signal.
- **`outcome_spec_needs_revision`** — the implementer's "this change names tasks I cannot complete in this sandbox" signal (the same semantic as the legacy `=== AUTOCODER-OUTCOME ===` `spec_needs_revision` payload).
  - Input schema: `{ unimplementable_tasks: Array<{ task_id: string, task_text: string, reason: string }>, revision_suggestion: string }`. All fields required. `unimplementable_tasks` SHALL be non-empty. NO string field may contain a `<...>`-shaped substring (the placeholder-detection refinement).
  - Output: a JSON object `{ ok: true }` on success; on validation failure, the MCP layer returns a JSON-RPC error code `-32602` (invalid params) with a `message` naming the offending field AND the specific failure mode (missing, empty, wrong type, placeholder-shaped). The control socket is NOT contacted on validation failure.

Both tools' handlers SHALL NOT compute results locally. Instead they SHALL relay the input to the daemon via the existing control socket (per the canonical `orchestrator-cli` "Control socket for runtime daemon interaction" requirement) using a new `record_outcome` action defined in the orchestrator-cli spec deltas. The daemon owns the outcome store AND records the outcome; the MCP child is a thin synchronous relay.

The relay uses the same env-var contract as the existing `query_canonical_specs` tool: `ORCH_DAEMON_CONTROL_SOCKET` for the socket path, `ORCH_MCP_WORKSPACE` AND `ORCH_MCP_CHANGE` for the routing keys. The MCP child resolves `workspace_basename` from `ORCH_MCP_WORKSPACE_BASENAME` (already set by `ClaudeCliExecutor::write_mcp_config`).

Connection timeout: 10 seconds (the same constant the `ask_user` AND `query_canonical_specs` relays use). On socket error OR timeout, the MCP layer returns a JSON-RPC error code `-32603` (internal error) with a `message` naming the relay failure. The wrapped agent SHALL surface the error AND MAY retry the tool call in the same session.

Validation is performed AT THE MCP LAYER, NOT at the daemon's `record_outcome` handler. The MCP layer is in-process with the agent AND the only writer to the control socket for this action; two-layer validation would create maintenance cost without payoff. The daemon's handler trusts the relayed payload AND stores it.

#### Scenario: Both tools advertised in the MCP child's `tools/list`
- **WHEN** an agent connects to the MCP child AND sends a `tools/list` request
- **THEN** the response lists `ask_user`, `query_canonical_specs`, `outcome_success`, AND `outcome_spec_needs_revision`
- **AND** `outcome_success`'s `inputSchema` matches the documented `{ final_answer?: string }` shape
- **AND** `outcome_spec_needs_revision`'s `inputSchema` matches the documented `{ unimplementable_tasks: Array<...>, revision_suggestion: string }` shape

#### Scenario: `outcome_success` relays to daemon AND records outcome
- **WHEN** an agent invokes `outcome_success({ final_answer: "Implementation complete; all tests pass." })`
- **AND** `ORCH_DAEMON_CONTROL_SOCKET`, `ORCH_MCP_WORKSPACE_BASENAME`, AND `ORCH_MCP_CHANGE` are set in the child's env
- **THEN** the MCP child opens a connection to the socket AND sends a `record_outcome` action with the `Success` variant AND the relayed `final_answer`
- **AND** the daemon's handler returns `{"ok":true}`
- **AND** the MCP child returns `{ ok: true }` to the agent as the tool-call result

#### Scenario: `outcome_spec_needs_revision` validates input before relaying
- **WHEN** an agent invokes `outcome_spec_needs_revision({ unimplementable_tasks: [{ task_id: "6.4", task_text: "Manual: SSH into the production host...", reason: "executor sandbox has no real SSH credentials" }], revision_suggestion: "Replace task 6.4 with a unit test..." })`
- **THEN** the MCP layer validates the input AND finds no schema violation
- **AND** the MCP child relays a `record_outcome` action with the `SpecNeedsRevision` variant carrying the full payload
- **AND** the daemon returns `{"ok":true}`
- **AND** the MCP child returns `{ ok: true }` to the agent

#### Scenario: `outcome_spec_needs_revision` rejects placeholder-shaped strings at the MCP layer
- **WHEN** an agent invokes `outcome_spec_needs_revision({ unimplementable_tasks: [{ task_id: "<id-from-tasks-md>", task_text: "<verbatim quote>", reason: "<one-line reason>" }], revision_suggestion: "<concrete edit>" })`
- **THEN** the MCP layer returns a JSON-RPC error code `-32602` with a `message` naming the placeholder-shaped field
- **AND** the control socket is NOT contacted
- **AND** the daemon's outcome store remains unchanged
- **AND** the wrapped agent receives the tool-error result AND can retry the tool call with corrected fields in the same session

#### Scenario: `outcome_spec_needs_revision` rejects missing required field at the MCP layer
- **WHEN** an agent invokes `outcome_spec_needs_revision({ unimplementable_tasks: [{ task_id: "6.4", task_text: "Manual: SSH...", reason: "no SSH access" }] })` (missing `revision_suggestion`)
- **THEN** the MCP layer returns a JSON-RPC error code `-32602` with a `message` naming `revision_suggestion` as the missing field
- **AND** the control socket is NOT contacted

#### Scenario: Control-socket relay failure surfaces as tool error
- **WHEN** an agent invokes `outcome_success({ final_answer: "done" })`
- **AND** the daemon's control socket is unreachable (daemon not running, socket path invalid, etc.)
- **THEN** the MCP layer returns a JSON-RPC error code `-32603` with a `message` naming the relay failure
- **AND** the wrapped agent receives the tool-error result

### Requirement: Tool-recorded outcomes take precedence over all heuristic classification in `classify_outcome`

The executor's outcome-dispatch path (`classify_outcome` in the CLI-wrapping executor backend) SHALL consult the daemon's outcome store via a `consume_outcome` control-socket action BEFORE applying any other classification step. The ordering is:

1. **Tool-recorded outcome lookup.** The classifier sends a `consume_outcome` action keyed by `(workspace_basename, change)`. When the daemon returns a recorded outcome:
   - A `Success` variant maps to `ExecutorOutcome::Completed { final_answer }` using the recorded `final_answer`.
   - A `SpecNeedsRevision` variant maps to the existing `ExecutorOutcome::SpecNeedsRevision { ... }` shape.
   - An `IterationRequest` variant maps to `ExecutorOutcome::IterationRequested { ... }` per the `a27a1` cap-enforcement rules.
   - The classifier returns the mapped outcome immediately. No further heuristic is applied.
2. **AskUser marker check** (unchanged from canonical executor behavior).
3. **Timeout precedence.** When `outcome.timed_out` is `true` AND no tool-recorded outcome was returned, the classifier returns `Failed { reason: "timeout" }` (OR the canonical timeout-reason format).
4. **Exit-status path** (unchanged).
5. **Layer-2 stdout heuristic + Completed fallback** (unchanged).

The legacy stdout-sentinel scan that previously sat between steps 3 AND 4 (per the original `a27a0` ordering) is REMOVED in this change. The acceptance scan + recovery loop introduced below replace its role: the only narrative-deferral path the classifier still produces (Completed via diff-presence heuristic) is gated by the acceptance scan in `Executor::run`'s post-classification step.

The precedence rule is anchored in the semantics of the signal: a tool-recorded outcome is the agent's deliberate, schema-validated end-of-run emission. It is more authoritative than ANY inferred state (timeout flag, exit status, stdout content). A run that called an outcome tool AND subsequently timed out is classified by the outcome, not the timeout.

When the daemon's `consume_outcome` action returns `None` (no outcome was recorded), the classifier proceeds to step 2 AND the existing canonical behavior is preserved exactly.

#### Scenario: Tool-recorded `Success` outcome takes precedence over diff-presence heuristic
- **WHEN** the classifier runs for a change whose daemon outcome store contains a `Success` outcome from a prior `outcome_success` tool call
- **AND** the workspace has a non-empty diff (would otherwise trigger today's Completed-via-diff-presence path with possibly different `final_answer` content)
- **THEN** the classifier returns `ExecutorOutcome::Completed { final_answer: <recorded final_answer> }`
- **AND** the recorded `final_answer` (NOT a heuristically-extracted alternative) is the outcome's content
- **AND** the daemon's outcome store entry for this `(workspace_basename, change)` is cleared (drained by `consume_outcome`)

#### Scenario: Tool-recorded `SpecNeedsRevision` outcome takes precedence over timeout
- **WHEN** the classifier runs for a change whose daemon outcome store contains a `SpecNeedsRevision` outcome (the agent called `outcome_spec_needs_revision` AND then was killed by the wall-clock timeout before clean exit)
- **AND** `outcome.timed_out` is `true`
- **THEN** the classifier returns `ExecutorOutcome::SpecNeedsRevision { ... }` populated from the recorded payload
- **AND** the timeout flag is NOT used
- **AND** no `Failed { reason: "timeout" }` outcome is produced

#### Scenario: Absent tool-recorded outcome falls through to AskUser → timeout → exit-status path
- **WHEN** the classifier runs for a change whose daemon outcome store contains no entry (the agent did not call any outcome tool)
- **AND** `outcome.timed_out` is `false`
- **AND** no AskUser marker is present
- **THEN** the classifier's `consume_outcome` call returns `None`
- **AND** the classifier proceeds through the simplified ordering (AskUser → timeout → exit status → diff-presence/Completed)
- **AND** no stdout-sentinel scan is attempted (the legacy path has been removed)

### Requirement: Implementer prompt documents the outcome tools by name AND uses them as the canonical end-of-run signal

The bundled `prompts/implementer.md` template SHALL contain an "Outcome tools" section that:

- Names all three outcome tools: `outcome_success`, `outcome_spec_needs_revision`, AND `outcome_request_iteration`.
- Provides a one-line purpose statement for each tool.
- Directs the agent to call `outcome_success` (with the agent's end-of-run summary as `final_answer`) at the end of a successful implementation run, BEFORE exiting.
- Directs the agent to call `outcome_spec_needs_revision` for the pre-flight unimplementable-task case.
- Directs the agent to call `outcome_request_iteration` (per `a27a1`) when honest scope-overflow means another iteration is needed.
- Notes that input-validation errors from any outcome tool are recoverable: the model receives the error as the tool-call result AND can retry the call with corrected fields in the same session.

The section SHALL NOT inline the full input schemas; the MCP `tools/list` response is the canonical schema source AND duplicating it in the prompt creates a maintenance hazard. Tool names + one-line purposes are sufficient: a model that knows the tool exists AND its purpose can attempt the call AND converge via tool-error feedback if its argument shape is wrong.

The legacy stdout-sentinel section (the `=== AUTOCODER-OUTCOME ===` block AND its DEPRECATED-prefixed retention from `a27a0`) is REMOVED. The implementer prompt SHALL NOT contain any reference to `=== AUTOCODER-OUTCOME ===`, the legacy `spec_needs_revision` JSON sentinel format, OR the substitution-instruction-plus-worked-example structural-elements discipline that bound the sentinel section.

Operator-customizable override prompts (loaded via the uniform `PromptLoader` per `a24`'s spec) MAY use any structure the operator prefers — the canonical rule binds the bundled default only.

#### Scenario: Bundled prompt names all three outcome tools
- **WHEN** a maintainer reads `prompts/implementer.md`
- **THEN** the prompt contains an "Outcome tools" section
- **AND** the section names `outcome_success`, `outcome_spec_needs_revision`, AND `outcome_request_iteration`
- **AND** each tool has a one-line purpose statement

#### Scenario: Bundled prompt's outcome-tool example deserializes cleanly
- **WHEN** an automated test extracts any JSON-shaped example from the prompt's outcome-tool sections AND deserializes it into the corresponding tool-argument Rust type
- **THEN** the deserialization succeeds without error
- **AND** every string field contains a concrete value (no angle-bracket markers, no template variables)

#### Scenario: Stdout-sentinel section is removed from the bundled prompt
- **WHEN** a maintainer reads `prompts/implementer.md`
- **THEN** the prompt contains NO occurrence of the string `=== AUTOCODER-OUTCOME ===`
- **AND** the prompt contains NO section describing the legacy `spec_needs_revision` stdout-block format
- **AND** the prompt contains NO DEPRECATED-prefixed retention of the legacy section

### Requirement: Per-execution MCP child exposes `outcome_request_iteration` tool

The per-execution stdio MCP server SHALL advertise an `outcome_request_iteration` tool alongside `outcome_success` AND `outcome_spec_needs_revision` (added in `a27a0`).

- Name: `outcome_request_iteration`.
- Purpose (operator-facing summary, also documented in the bundled implementer prompt): the agent has completed some tasks AND wants another iteration to finish the rest. NOT for unimplementable tasks (use `outcome_spec_needs_revision` for those).
- Input schema: `{ completed_tasks: Array<string>, remaining_tasks: Array<string>, reason: string }`. All three fields required. Both arrays SHALL be non-empty. Every array element SHALL be a non-empty string. `reason` SHALL be non-empty. NO string field (top-level, array element, or otherwise) may contain a `<...>`-shaped substring (the same placeholder-detection refinement applied to `outcome_spec_needs_revision`).
- Output on success: `{ ok: true }`. On any input-validation failure, the MCP layer returns a JSON-RPC error code `-32602` (invalid params) with a `message` naming the offending field AND the specific failure mode. The control socket is NOT contacted on validation failure. The wrapped agent receives the error AND can retry the tool call with corrected fields in the same session.

The tool's handler SHALL relay validated input to the daemon via the existing `record_outcome` control-socket action using the `iteration_request` variant tag (per the orchestrator-cli deltas in this change).

#### Scenario: Tool advertised in `tools/list`
- **WHEN** an agent sends a `tools/list` request to the MCP child
- **THEN** the response lists `outcome_request_iteration` alongside `outcome_success`, `outcome_spec_needs_revision`, `ask_user`, AND `query_canonical_specs`
- **AND** the tool's `inputSchema` matches the documented `{ completed_tasks, remaining_tasks, reason }` shape

#### Scenario: Valid invocation relays to daemon
- **WHEN** an agent invokes `outcome_request_iteration({ completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "task 3 needs a refactor I want to plan more carefully" })`
- **THEN** the MCP layer validates the input successfully
- **AND** relays a `record_outcome` control-socket action with the `iteration_request` variant AND the input fields
- **AND** returns `{ ok: true }` to the agent

#### Scenario: Empty `completed_tasks` rejected at MCP layer
- **WHEN** an agent invokes `outcome_request_iteration({ completed_tasks: [], remaining_tasks: ["3"], reason: "..." })`
- **THEN** the MCP layer returns JSON-RPC error code `-32602` with a `message` naming `completed_tasks` as empty
- **AND** the control socket is NOT contacted

#### Scenario: Empty `remaining_tasks` rejected at MCP layer
- **WHEN** an agent invokes `outcome_request_iteration({ completed_tasks: ["1"], remaining_tasks: [], reason: "..." })`
- **THEN** the MCP layer returns JSON-RPC error code `-32602` with a `message` naming `remaining_tasks` as empty
- **AND** the control socket is NOT contacted

#### Scenario: Placeholder-shaped string rejected at MCP layer
- **WHEN** an agent invokes `outcome_request_iteration({ completed_tasks: ["1"], remaining_tasks: ["3"], reason: "<concrete blocker>" })`
- **THEN** the MCP layer returns JSON-RPC error code `-32602` with a `message` naming `reason` AND the placeholder-shaped failure mode
- **AND** the control socket is NOT contacted

### Requirement: `ExecutorOutcome::IterationRequested` variant carries cumulative state AND the next iteration number

The `ExecutorOutcome` enum (per the canonical executor architecture spec) SHALL gain an `IterationRequested { completed_tasks: Vec<String>, remaining_tasks: Vec<String>, reason: String, iteration_number: u32 }` variant.

- `completed_tasks` AND `remaining_tasks` carry the agent's cumulative-as-of-this-iteration lists verbatim from the recorded outcome.
- `reason` carries the agent's stated blocker verbatim.
- `iteration_number` is the iteration number the NEXT polling cycle will observe AND inject into the next iteration's prompt. The classifier computes it as `prior_iteration_number + 1` where `prior_iteration_number` comes from the workspace's `.iteration-pending.json` marker (0 when no marker is present, so the first request produces `iteration_number: 2` — meaning "the upcoming iteration is the 2nd").

Downstream polling-loop code that branches on `ExecutorOutcome` SHALL handle the new variant per the orchestrator-cli deltas in this change.

#### Scenario: First iteration request produces iteration_number 2
- **WHEN** the classifier consumes a recorded `iteration_request` outcome AND the workspace has no `.iteration-pending.json` marker
- **THEN** the returned `ExecutorOutcome::IterationRequested` has `iteration_number: 2`

#### Scenario: Subsequent iteration request increments the count
- **WHEN** the classifier consumes a recorded `iteration_request` outcome AND the workspace's marker file shows `iteration_number: 3`
- **THEN** the returned `ExecutorOutcome::IterationRequested` has `iteration_number: 4`

### Requirement: Classifier enforces iteration cap of 5

Before mapping a recorded `iteration_request` outcome to `ExecutorOutcome::IterationRequested`, the classifier SHALL compute the prospective `iteration_number` (per the rule above) AND check it against the iteration cap of 5.

When `iteration_number > 5`, the classifier SHALL:

- Emit `tracing::warn!` naming the change AND the cap.
- Return `ExecutorOutcome::Failed { reason: "exceeded iteration-request cap (5); WIP on agent branch — review or restart from scratch" }` (exact wording REQUIRED so operators can grep AND scripts can match).
- NOT modify, replace, OR delete the `.iteration-pending.json` marker file. The marker's preservation lets the operator inspect cumulative state for triage.

The cap is fixed at 5 in this change. A future spec MAY make it configurable; doing so does NOT require revising this requirement (the requirement binds the implementation-default cap AND the override semantics).

#### Scenario: 5th iteration is permitted
- **WHEN** the classifier consumes a recorded `iteration_request` outcome AND the workspace's marker file shows `iteration_number: 4`
- **THEN** the classifier computes `iteration_number: 5` AND returns `ExecutorOutcome::IterationRequested` (the 5th iteration runs)

#### Scenario: 6th iteration is capped
- **WHEN** the classifier consumes a recorded `iteration_request` outcome AND the workspace's marker file shows `iteration_number: 5`
- **THEN** the classifier returns `ExecutorOutcome::Failed { reason: "exceeded iteration-request cap (5); WIP on agent branch — review or restart from scratch" }`
- **AND** the `.iteration-pending.json` marker file is preserved unchanged
- **AND** the `tracing::warn!` log line names the change AND the cap

#### Scenario: Cap counts span multiple subprocess runs
- **WHEN** a change has gone through iteration_request outcomes in iterations 1, 2, 3, AND 4 (each producing a marker file with the corresponding incremented number)
- **AND** iteration 5 runs successfully (the agent calls `outcome_success`)
- **THEN** the marker file is deleted (per the lifecycle requirement below) AND the iteration sequence terminates without hitting the cap

### Requirement: Iteration-pending marker file in the change directory carries state across iteration boundaries

When the polling loop handles an `ExecutorOutcome::IterationRequested`, it SHALL write the marker file `<workspace>/openspec/changes/<change>/.iteration-pending.json` AFTER successfully committing AND force-pushing the WIP to the agent branch.

Marker file shape:

```json
{
  "completed_tasks": ["1", "2"],
  "remaining_tasks": ["3"],
  "reason": "task 3 needs a refactor I want to plan more carefully",
  "iteration_number": 2
}
```

Marker write SHALL use atomic tempfile + rename to avoid partial-write corruption (the same pattern `mcp_askuser_server::write_marker` uses for `.askuser-pending.json`).

Marker lifecycle in each `ExecutorOutcome` arm:

- `IterationRequested`: write/replace marker with the new iteration's cumulative state AND incremented iteration_number (after WIP commit + push).
- `Completed`: delete marker after WIP commit + push completes successfully. Deletion is idempotent (no error if marker absent).
- `SpecNeedsRevision`: delete marker. The iteration sequence is conceptually terminated; operator action is required.
- `Failed`: leave marker untouched. A subsequent retry of the same change preserves the continuation context.
- `AskUser`: leave marker untouched. The agent's question may resolve into a continuation.

The marker is filesystem-inspectable (`ls -a <workspace>/openspec/changes/<change>/`) for operators debugging an in-progress iteration sequence.

#### Scenario: Marker written on IterationRequested AFTER successful push
- **WHEN** the polling loop handles `ExecutorOutcome::IterationRequested { completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "...", iteration_number: 2 }`
- **AND** the WIP commit AND push to the agent branch both succeed
- **THEN** `.iteration-pending.json` is written atomically AND contains the documented fields with `iteration_number: 2`

#### Scenario: Marker deleted on Completed
- **WHEN** the polling loop handles `ExecutorOutcome::Completed` for a change whose `.iteration-pending.json` is present
- **AND** the WIP commit AND push complete successfully
- **THEN** `.iteration-pending.json` is deleted
- **AND** subsequent operator inspection of the change directory shows no marker

#### Scenario: Marker preserved on Failed
- **WHEN** the polling loop handles `ExecutorOutcome::Failed { reason: "timeout" }` (OR any other Failed reason) for a change whose `.iteration-pending.json` is present
- **THEN** the marker is NOT deleted
- **AND** the next polling iteration that processes this change sees the marker AND injects continuation context

#### Scenario: Marker NOT written if push fails
- **WHEN** the polling loop handles `ExecutorOutcome::IterationRequested` AND the force-push to the agent branch fails
- **THEN** `.iteration-pending.json` is NOT written
- **AND** the polling loop emits `tracing::error!` naming the push failure
- **AND** the change reverts to normal pending behavior on the next polling cycle (no front-insertion preference, no continuation context)

### Requirement: Implementer prompt includes a "Prior iteration summary" block when an iteration-pending marker is present

The bundled `prompts/implementer.md` rendering pipeline SHALL read `<workspace>/openspec/changes/<change>/.iteration-pending.json` at prompt-build time. When the marker is present AND parseable:

- The rendered prompt SHALL append a "Prior iteration summary" block AFTER the change body (NOT before — placement is load-bearing per the design rationale).
- The block SHALL contain the marker's cumulative `completed_tasks`, `remaining_tasks`, `reason`, AND `iteration_number` verbatim.
- The block SHALL frame the prior state as already-done (the agent does NOT re-implement completed tasks).
- The block SHALL instruct the agent to re-evaluate the prior blocker with fresh eyes (do NOT inherit the prior pessimism).
- The block SHALL name the cap (`Current iteration: N of 5`) so the agent knows the channel is finite.
- The block SHALL direct the agent to call `outcome_success` at end-of-run when remaining tasks are done OR `outcome_request_iteration` again with updated cumulative state if another iteration is honestly needed.

Block content (canonical text the bundled prompt SHALL produce; substitution of `<list>`, `<reason>`, `N` with marker values is required):

```
--- BEGIN PRIOR ITERATION SUMMARY ---

A previous iteration of this same change reached a structured stopping
point. Your job is to overcome the prior blocker AND finish the
remaining tasks. The previous iteration's working tree has already been
committed AND pushed to the agent branch — your starting state already
includes its progress.

Cumulative completed (do NOT re-implement): <completed_tasks>
Remaining: <remaining_tasks>
Prior iteration's stated reason for stopping: <reason>
Current iteration: N of 5 (cap)

Do NOT assume the prior reason still holds. Re-evaluate the blocker
with fresh eyes — the prior iteration's model may have miscalibrated
the scope, AND a different angle of attack may resolve the work in
this iteration. If you genuinely cannot finish in this iteration,
call outcome_request_iteration again with an updated cumulative state
AND a more specific reason. Note that the iteration cap is 5; runs
beyond that are auto-failed.

--- END PRIOR ITERATION SUMMARY ---
```

When the marker is absent, the prompt is built as today with no continuation block. The first-iteration prompt's shape is unchanged.

When the marker is present BUT corrupt (truncated JSON, missing required field, parse failure), the prompt-builder SHALL:

- Emit `tracing::warn!` naming the change AND the corruption mode.
- Fall back to building the prompt as if no marker were present (no continuation block).
- Leave the corrupt marker on disk (operator can inspect AND repair OR delete).

Operator-customizable override prompts (loaded via the uniform `PromptLoader` per `a24`'s spec) MAY use any structure the operator prefers — the canonical rule binds the bundled default only.

The `outcome_request_iteration` tool SHALL be named in the prompt's "Outcome tools" section (added in `a27a0`) alongside `outcome_success` AND `outcome_spec_needs_revision`. Each tool's one-line purpose AND when-to-use guidance is sufficient; full schemas remain in the MCP `tools/list` response per a27a0's documentation discipline.

#### Scenario: Continuation block injected when marker is present
- **WHEN** the prompt-builder runs for a change whose `.iteration-pending.json` contains `{ completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "task 3 needs a refactor I want to plan more carefully", iteration_number: 2 }`
- **THEN** the rendered prompt contains the "Prior iteration summary" block AFTER the change body
- **AND** the block contains `Cumulative completed (do NOT re-implement): 1, 2`
- **AND** the block contains `Remaining: 3`
- **AND** the block contains `Prior iteration's stated reason for stopping: task 3 needs a refactor I want to plan more carefully`
- **AND** the block contains `Current iteration: 2 of 5 (cap)`

#### Scenario: First-iteration prompt has no continuation block
- **WHEN** the prompt-builder runs for a change whose `.iteration-pending.json` does NOT exist
- **THEN** the rendered prompt is built as today with no continuation block
- **AND** the prompt's shape matches the pre-spec first-iteration shape verbatim

#### Scenario: Corrupt marker is logged AND ignored
- **WHEN** the prompt-builder runs for a change whose `.iteration-pending.json` is truncated mid-JSON
- **THEN** a `tracing::warn!` log line names the change AND the corruption
- **AND** the rendered prompt has no continuation block
- **AND** the corrupt marker file is NOT modified OR deleted by the prompt-builder

#### Scenario: Bundled prompt names the new outcome tool
- **WHEN** a maintainer reads `prompts/implementer.md`'s "Outcome tools" section
- **THEN** `outcome_request_iteration` is named alongside `outcome_success` AND `outcome_spec_needs_revision`
- **AND** the section gives a one-line purpose ("you started implementation but want another iteration to finish — NOT for unimplementable tasks") for the new tool

### Requirement: Acceptance scan rejects implementer runs that ship unchecked tasks without a structured outcome

`Executor::run` (the implementer-first-pass entry point, against a real change directory) SHALL apply an acceptance scan AFTER `classify_outcome` returns AND BEFORE finalizing the outcome. The scan SHALL fire ONLY when:

1. The classified outcome is `ExecutorOutcome::Completed`.
2. The run did NOT produce a tool-recorded outcome (`consume_outcome` returned `None` during classification — i.e. the agent exited without calling any outcome tool).

If either condition does NOT hold, the scan SHALL be skipped AND the classified outcome SHALL be returned unchanged.

When the scan fires, it SHALL count unchecked tasks in `<workspace>/openspec/changes/<change>/tasks.md`. Parsing rules:

- Lines matching `^[ \t]*- \[ \] ` outside fenced code blocks count as unchecked.
- Lines matching `^[ \t]*- \[x\] ` (case-insensitive on `x`) count as checked AND are ignored.
- Content inside ` ``` ` fenced blocks is ignored entirely.
- The parser extracts the trailing text (everything after `- [ ] `) for each unchecked line, paired with the source line number.

If `tasks.md` is absent OR unparseable, the scan SHALL treat the unchecked count as zero (defensive default — absent/corrupt tasks.md is its own diagnostic AND the polling loop's existing validation catches it elsewhere).

When the unchecked count is zero, the classified `Completed` outcome SHALL be returned unchanged. When the unchecked count is non-zero, the recovery loop (per the requirement below) SHALL fire.

The acceptance scan SHALL NOT fire in `run_revision`, `run_triage`, `run_chat_triage`, `run_brownfield_draft`, `run_scout`, OR `run_changelog`. Those flows do not operate against a per-change `tasks.md` in the implementer sense; their existing classification path is preserved.

#### Scenario: All tasks checked AND outcome_success called — no scan triggered
- **WHEN** `Executor::run` finishes a run where the agent called `outcome_success` AND `tasks.md` has zero unchecked items
- **THEN** the classified outcome is `Completed` via the tool-outcome precedence path
- **AND** the acceptance scan does NOT fire (condition 2: tool-recorded outcome was produced)
- **AND** the finalized outcome is `Completed` unchanged

#### Scenario: Unchecked tasks AND outcome_success called — no scan triggered
- **WHEN** `Executor::run` finishes a run where the agent called `outcome_success` AND `tasks.md` has unchecked items
- **THEN** the classified outcome is `Completed` via the tool-outcome precedence path
- **AND** the acceptance scan does NOT fire (condition 2: tool-recorded outcome was produced)
- **AND** the finalized outcome is `Completed` unchanged (the agent's structured signal wins over the daemon's heuristic disagreement)

#### Scenario: No outcome tool call AND zero unchecked tasks — Completed unchanged
- **WHEN** `Executor::run` finishes a run where no outcome tool was called AND `tasks.md` has zero unchecked items
- **AND** the diff-presence heuristic classifies the outcome as `Completed`
- **THEN** the acceptance scan fires (condition 1 met, condition 2 met — both triggers true)
- **AND** the scan returns zero unchecked items
- **AND** the finalized outcome is `Completed` unchanged

#### Scenario: No outcome tool call AND unchecked tasks present — recovery loop fires
- **WHEN** `Executor::run` finishes a run where no outcome tool was called AND `tasks.md` has unchecked items (e.g. `- [ ] 3.1 thread Arc<DaemonPaths> through polling_loop::run`)
- **AND** the diff-presence heuristic would have classified the outcome as `Completed`
- **THEN** the acceptance scan fires AND returns the non-zero unchecked-item list
- **AND** the recovery loop (per the requirement below) is invoked

#### Scenario: Absent tasks.md does not trigger acceptance failure
- **WHEN** `Executor::run` finishes a run AND `<workspace>/openspec/changes/<change>/tasks.md` does NOT exist
- **THEN** the scan treats the unchecked count as zero
- **AND** no recovery loop fires
- **AND** the classified outcome is returned unchanged

#### Scenario: `run_revision` does NOT trigger acceptance scan
- **WHEN** `Executor::run_revision` finishes a run for an archived change (whose `tasks.md` lives under `archive/<date>-<change>/`, NOT under `openspec/changes/<change>/`)
- **THEN** the acceptance scan does NOT fire regardless of workspace content
- **AND** the classification path proceeds via the existing canonical behavior

#### Scenario: Non-implementer flows do NOT trigger acceptance scan
- **WHEN** `run_triage`, `run_chat_triage`, `run_brownfield_draft`, `run_scout`, OR `run_changelog` finishes a run
- **THEN** the acceptance scan does NOT fire regardless of workspace content
- **AND** the classification path proceeds via the existing canonical behavior

### Requirement: Recovery loop re-prompts the same Claude session on acceptance failure; one retry only

When the acceptance scan returns a non-zero unchecked-item list, `Executor::run` SHALL launch a single recovery turn against the original session via `claude --resume <session_id>` (the same mechanism `Executor::resume` uses for AskUser-flow resumption).

The recovery turn's input SHALL be a structured user-message constructed from the canonical template:

```
Acceptance check failed: your run ended without finishing the change.

tasks.md still has unchecked items:
  - <line_text_1>
  - <line_text_2>
  ...

You did not call any outcome tool to conclude the session. Narrative
"Deferred:" notes in the final-answer text are not accepted; the
daemon enforces a structured outcome.

Decide which of the following applies AND call the corresponding tool:

1. The unchecked items are actually done in code — you forgot to mark
   tasks.md. Update tasks.md to check them, then call:
       outcome_success({ final_answer: "..." })

2. You completed part AND want another iteration to finish the rest.
   Call:
       outcome_request_iteration({
         completed_tasks: [...],
         remaining_tasks: [<unchecked list>],
         reason: "<concrete blocker>"
       })

3. The unchecked items are unimplementable in this sandbox. Call:
       outcome_spec_needs_revision({
         unimplementable_tasks: [...],
         revision_suggestion: "..."
       })

Do NOT exit without calling exactly one outcome tool. If you call one
AND it returns a validation error, fix the error AND retry the call.
```

The `<line_text_*>` substitutions are the trailing text from each unchecked-item line extracted by the acceptance scan. The list SHALL include every unchecked item the scan returned, in source-order.

The recovery turn SHALL run with the same MCP config (outcome tools available) AND a fresh wall-clock budget equal to the per-run timeout. Within the recovery turn the existing classifier ordering applies: a tool-recorded outcome wins over any heuristic.

After the recovery turn exits, `classify_outcome` SHALL classify its result. If the recovery turn produced a tool-recorded outcome (one of `outcome_success`, `outcome_spec_needs_revision`, `outcome_request_iteration`), that outcome SHALL be returned as `Executor::run`'s final result. The acceptance scan SHALL NOT re-fire on the recovery turn's result.

If the recovery turn did NOT produce a tool-recorded outcome, `Executor::run` SHALL return `ExecutorOutcome::Failed { reason: "acceptance check failed; recovery loop did not produce a structured outcome" }` (exact wording REQUIRED so operators can grep AND scripts can match).

The recovery loop SHALL fire AT MOST ONCE per `Executor::run` invocation. A recovery turn whose own output triggers acceptance failure does NOT fire a second recovery turn.

The recovery turn's stdout/stderr stream SHALL be appended to the per-change run log with a clear divider line. In the summary log: `=== RECOVERY TURN ===` followed by the recovery turn's `final_answer`. In the stream log: `=== RECOVERY TURN ===` followed by the recovery turn's `[tool_use]` / `[tool_result]` / `[assistant]` lines.

#### Scenario: Recovery turn calls outcome_success — final Completed
- **WHEN** the acceptance scan fires AND the recovery turn launches via `claude --resume <session_id>`
- **AND** the agent in the recovery turn marks the unchecked tasks complete in `tasks.md` AND calls `outcome_success({ final_answer: "..." })`
- **THEN** the recovery turn's `consume_outcome` returns a `Success` outcome
- **AND** `Executor::run` returns `Completed { final_answer: <recovery's final_answer> }`
- **AND** the run log contains both the original transcript AND the recovery transcript with the `=== RECOVERY TURN ===` divider

#### Scenario: Recovery turn calls outcome_request_iteration — final IterationRequested
- **WHEN** the acceptance scan fires AND the recovery turn launches
- **AND** the agent calls `outcome_request_iteration({ completed_tasks: [...], remaining_tasks: [...], reason: "..." })`
- **THEN** the recovery turn's `consume_outcome` returns an `IterationRequest` outcome
- **AND** `Executor::run` returns `IterationRequested { ..., iteration_number: <computed per a27a1 rules> }`
- **AND** the run log contains both transcripts

#### Scenario: Recovery turn produces no outcome tool call — final Failed
- **WHEN** the acceptance scan fires AND the recovery turn launches
- **AND** the agent in the recovery turn produces no `outcome_*` tool call AND exits
- **THEN** `Executor::run` returns `Failed { reason: "acceptance check failed; recovery loop did not produce a structured outcome" }`
- **AND** the run log contains both transcripts so the operator can review the agent's reasoning across both phases

#### Scenario: Recovery loop fires at most once per run
- **WHEN** the recovery turn's own result triggers acceptance scan conditions (Completed via diff-presence AND no outcome tool call AND unchecked tasks still present)
- **THEN** a SECOND recovery turn is NOT launched
- **AND** `Executor::run` returns `Failed { reason: "acceptance check failed; recovery loop did not produce a structured outcome" }`

### Requirement: Implementer prompt forbids narrative deferral AND describes the acceptance-scan + recovery-loop enforcement

The bundled `prompts/implementer.md` template SHALL contain an "Anti-narrative-deferral" section near the top of the prompt (above the existing pre-flight outcome-tool section). The section SHALL:

- Direct the agent NOT to narrate "Deferred:" sections in the final-answer text.
- State that the daemon enforces a structured outcome via the outcome tools (`outcome_success`, `outcome_request_iteration`, `outcome_spec_needs_revision`).
- Describe the acceptance scan: at end-of-run, `tasks.md` is scanned for unchecked items; if any are found AND no outcome tool was called, a recovery turn fires.
- Describe the recovery turn: it appends a structured message to the session naming the unchecked items AND requesting an outcome-tool call. The recovery turn has one retry; a recovery turn that ALSO does not call an outcome tool produces a Failed run.

The section's tone is informational, NOT scolding. The text SHALL motivate the structural enforcement (narrative deferral was previously the path of least resistance AND produced corrosive PR shipping) so an agent reading the prompt understands WHY the channel exists AND how to use the right tool the first time.

Canonical text the bundled prompt SHALL produce (section heading + body — the heading SHALL be a top-level prompt section but is rendered here without the `##` markdown prefix to avoid confusing the spec parser):

```
Anti-narrative-deferral discipline

Do NOT narrate "Deferred:" sections in your final-answer text. The
daemon enforces a structured outcome via the outcome tools (see the
"Outcome tools" section below). If you have remaining work, call
`outcome_request_iteration`. If a task is genuinely unimplementable,
call `outcome_spec_needs_revision`. If you finished, call
`outcome_success`. Narrative deferral was previously the path of
least resistance AND produced corrosive PR shipping (unchecked tasks
AND apologetic prose buried in the PR comment); the acceptance scan
now catches this AND triggers a recovery turn that fails the run if
you persist.

At end-of-run, the daemon scans tasks.md for unchecked items. If
unchecked items are present AND you did not call any outcome tool,
the daemon launches a recovery turn that re-prompts you with the
list of unchecked items AND directs you to call exactly one outcome
tool. The recovery turn has one retry; if it ALSO produces no
outcome-tool call, the run is classified as Failed.
```

Operator-customizable override prompts MAY remove OR rewrite this section — the canonical rule binds the bundled default only. Operators who remove this guidance see the structural enforcement (acceptance scan + recovery loop) continue to apply, but their custom implementer agents may not know to expect it.

#### Scenario: Bundled prompt contains the anti-narrative-deferral section
- **WHEN** a maintainer reads `prompts/implementer.md`
- **THEN** the prompt contains an "Anti-narrative-deferral discipline" section near the top (above the pre-flight outcome-tool section)
- **AND** the section names all three outcome tools (`outcome_success`, `outcome_request_iteration`, `outcome_spec_needs_revision`)
- **AND** the section describes both the acceptance scan AND the recovery turn

#### Scenario: Bundled prompt's canonical text matches the requirement
- **WHEN** an automated test extracts the "Anti-narrative-deferral discipline" section from `prompts/implementer.md`
- **THEN** the extracted text matches the canonical text specified above (the structural elements: warning + tool list + acceptance-scan description + recovery-turn description)

### Requirement: `ExecutorOutcome::Aborted` distinguishes operator-shutdown-initiated subprocess kills from real failures

The `ExecutorOutcome` enum SHALL gain a new variant `Aborted { reason: String }`. This variant represents an executor subprocess that exited because the daemon itself was being shut down (the SIGTERM the daemon received cascaded to the executor's process group, killing the wrapped CLI child by signal — the reaped `ExitStatus` reports `signal() == Some(15)`, i.e. killed by SIGTERM/signal 15). The variant is structurally distinct from `Failed` so the polling-loop AND the failure-counter mechanism can treat it differently — specifically, `Aborted` SHALL NOT count against `executor.perma_stuck_threshold`.

The polling-loop's outcome dispatcher SHALL handle `Aborted` by:

1. Logging INFO `executor aborted: {reason}` naming the change.
2. Dropping `.in-progress` per the canonical openspec-queue-engine "Unlocking after any executor outcome" requirement.
3. NOT incrementing the per-change failure counter.
4. NOT writing `.perma-stuck.json`.
5. NOT posting a chatops failure alert (operator initiated the shutdown; they don't need notification).
6. Leaving `.iteration-pending.json` untouched (mirrors the `Failed` arm's preservation of continuation context).
7. Returning `Ok(())` from the per-change processing function. The polling loop continues its shutdown sequence normally; the change remains pending AND retries on the next polling cycle after restart.

#### Scenario: `Aborted` does NOT increment failure counter
- **WHEN** the polling-loop's outcome dispatcher receives `Ok(ExecutorOutcome::Aborted { reason: "daemon shutdown (SIGTERM cascade)" })` for change `a35-foo`
- **THEN** the `consecutive_failures` counter for `a35-foo` is NOT incremented
- **AND** `.perma-stuck.json` is NOT written for `a35-foo`
- **AND** no `❌ Failed` OR `:no_entry: perma-stuck` chatops alert fires
- **AND** the `.in-progress` lock is dropped
- **AND** `.iteration-pending.json` (if present) is preserved

#### Scenario: Two consecutive `Aborted` outcomes do NOT perma-stuck
- **WHEN** the same change receives `Aborted` outcomes in two consecutive polling iterations (e.g., operator restarts the daemon twice in a row while the change is mid-iteration)
- **THEN** the change is NOT perma-stuck
- **AND** the failure counter remains at 0 throughout
- **AND** the third polling iteration picks up the change fresh AND attempts implementation normally

### Requirement: Classifier returns `Aborted` for a SIGTERM-killed subprocess during daemon shutdown; preserves `Failed` for external-source SIGTERMs

The `classify_outcome` path in `claude_cli.rs` SHALL inspect a process-wide shutdown flag (`crate::daemon::SHUTDOWN_REQUESTED: AtomicBool`) when the wrapped CLI was killed by SIGTERM. Because the daemon spawns the wrapped CLI directly in its own process group, a SIGTERM cascade reaps the child *by signal*, so the reaped `ExitStatus` reports `signal() == Some(15)` (`code()` returns `None` for any signal-killed process). The classifier SHALL detect the SIGTERM kill as `status.signal() == Some(15) || status.code() == Some(143)` — the former is the production shape; the latter is accepted defensively for the shell "128 + 15" convention that surfaces only if a wrapper OR the CLI itself catches SIGTERM and `exit(143)`s. The flag SHALL be set to `true` by the daemon's SIGTERM handler BEFORE the daemon initiates shutdown of child tasks (so classifier checks happening during the shutdown cascade observe the flag as true). The flag is one-way per process lifetime (false → true; never reset).

Classification rules for a SIGTERM-killed subprocess (`signal() == Some(15)` OR `code() == Some(143)`):

- `SHUTDOWN_REQUESTED == true` → return `ExecutorOutcome::Aborted { reason: "daemon shutdown (SIGTERM cascade)" }`. The subprocess was killed by the cascade from the daemon's own SIGTERM; not the change's fault.
- `SHUTDOWN_REQUESTED == false` → return `ExecutorOutcome::Failed { reason: <stderr excerpt, OR the Display of the status — e.g. "executor exited with signal: 15 (SIGTERM)"> }` (today's behavior, preserved). An external source (OOM killer, manual `kill -TERM <pid>`, container orchestrator) sent the executor a SIGTERM; this is treated as a real failure AND counts against the failure budget.

Classification rules for subprocesses NOT killed by SIGTERM are UNCHANGED by this requirement. The shutdown flag SHALL NOT affect:

- Exit status `0` paths (still classified as `Completed` per the diff-presence heuristic OR existing happy-path rules).
- Other non-zero exit codes / other signals (still classified as `Failed` with the stderr-derived reason).
- Timeout cases (still classified per the canonical timeout-precedence requirement).
- Tool-recorded outcomes (still classified per the canonical "Tool-recorded outcomes take precedence" requirement from `a27a0`).

The flag's purpose is narrow: distinguish the one specific case where the daemon's own shutdown caused the executor's death. Every other classification path is preserved.

#### Scenario: SIGTERM-killed subprocess during daemon shutdown classifies as Aborted
- **WHEN** `classify_outcome` is called with `outcome.exit_status` reporting `signal() == Some(15)` (a SIGTERM-killed child) AND `SHUTDOWN_REQUESTED.load(SeqCst) == true`
- **THEN** the classifier returns `Ok(ExecutorOutcome::Aborted { reason: "daemon shutdown (SIGTERM cascade)" })`
- **AND** the same result holds for the defensive `code() == Some(143)` form (a wrapper/CLI catching SIGTERM and exiting 143)
- **AND** no failure-counter increment OR alert fires downstream

#### Scenario: SIGTERM-killed subprocess without daemon shutdown classifies as Failed (today's behavior)
- **WHEN** `classify_outcome` is called with `outcome.exit_status` reporting `signal() == Some(15)` AND `SHUTDOWN_REQUESTED.load(SeqCst) == false`
- **THEN** the classifier returns `Ok(ExecutorOutcome::Failed { reason })` where `reason` is the stderr excerpt (or the Display of the signal-killed status when stderr is empty, naming `signal: 15`)
- **AND** the existing failure-counter + perma-stuck protections fire normally (external SIGTERMs from OOM killer, manual kill, etc., remain protected against loop)

#### Scenario: Exit-1 with shutdown flag set still classifies as Failed
- **WHEN** `classify_outcome` is called with `outcome.exit_status: ExitStatus(code: 1)` AND `SHUTDOWN_REQUESTED.load(SeqCst) == true` (e.g., the executor genuinely failed with a non-SIGTERM exit code during the shutdown window)
- **THEN** the classifier returns `Ok(ExecutorOutcome::Failed { reason: <stderr excerpt> })` per the existing behavior
- **AND** the shutdown flag does NOT override non-SIGTERM exit codes (the flag's gate is specifically on signal-15 / exit-143 deaths, not all exit codes during shutdown)

#### Scenario: Exit-0 with shutdown flag set still classifies via existing happy-path rules
- **WHEN** `classify_outcome` is called with `outcome.exit_status: ExitStatus(code: 0)` AND `SHUTDOWN_REQUESTED.load(SeqCst) == true` (e.g., the executor completed cleanly just before the daemon's shutdown timing)
- **THEN** the classifier proceeds through the existing exit-0 path (Completed-via-tool-outcome, OR Completed-via-diff, OR the canonical Layer-2 heuristic)
- **AND** the shutdown flag does NOT mask a legitimate Completed outcome

### Requirement: Daemon's SIGTERM handler sets the shutdown flag as its first action

The daemon's SIGTERM signal handler SHALL set `SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst)` as its FIRST action, BEFORE initiating the shutdown of child tasks (polling-loop futures, chatops listener, control-socket listener, etc.). This ordering is load-bearing: the SIGTERM cascade to executor subprocesses happens AFTER the daemon's children begin shutting down, AND those subprocesses' classifier checks must observe `SHUTDOWN_REQUESTED == true`.

The flag SHALL NOT be reset during the process's lifetime (one-way false → true). A subsequent daemon restart starts a new process with the flag at its `AtomicBool::new(false)` default.

#### Scenario: SIGTERM handler sets flag before cascading
- **WHEN** the daemon receives SIGTERM
- **THEN** the SIGTERM handler's first action is `SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst)`
- **AND** subsequent shutdown actions (graceful child cancellation, socket close, etc.) happen AFTER the flag store

#### Scenario: Flag persists for the rest of the process lifetime
- **WHEN** the flag has been set to `true` AND the daemon's shutdown sequence proceeds
- **THEN** the flag remains `true` until the process exits
- **AND** any classifier call happening during the shutdown cascade observes the flag as `true`

#### Scenario: Fresh daemon process starts with flag false
- **WHEN** a new daemon process spawns (e.g., post-restart)
- **THEN** `SHUTDOWN_REQUESTED.load(Ordering::SeqCst)` returns `false`
- **AND** the next iteration's classifier calls classify exit codes per the non-shutdown path

### Requirement: MCP outcome-tool description fields encourage substantive content AND drop narrative history
The `description` field of each outcome tool advertised by the per-execution MCP child (currently `autocoder/src/mcp_askuser_server.rs`) SHALL be operationally focused — directing the agent what to do AND what content to produce — without narrative history about prior failure modes OR legacy mechanisms. The agent reads the `description` field from the MCP `tools/list` response to decide how to use the tool; that text is the primary surface for shaping agent behavior, so it SHALL carry the load-bearing operational guidance:

- `outcome_success` — names the `final_answer` field AND its reviewer-facing destination (the PR's implementation-notes section), AND directs the agent to pass a substantive end-of-run summary rather than treating the bare call as sufficient.
- `outcome_request_iteration` — names the cumulative completed/remaining state AND the blocker-naming `reason` field, AND distinguishes the tool from `outcome_spec_needs_revision`.
- `outcome_spec_needs_revision` — names the file the agent reads (`tasks.md`), the placeholder-rejection rule, AND where input validation runs (the MCP layer).

This is design intent for human-authored message content. It is verified by review AND the drift audit's semantic judgment — NOT by a unit test asserting substrings of the descriptions (per the project-documentation requirement `Tests assert behavior or derivation, never message wording`). A test that read the descriptions and asserted hand-authored wording is a change-detector that breaks on meaning-preserving rewrites; the descriptions' fitness is a judgment the drift audit makes against this requirement. The required/forbidden-substring contract AND the substring regression test mandated by the prior version of this requirement are removed.

This requirement covers description CONTENT ONLY. The tool schemas (`inputSchema`), behaviors (control-socket relay), AND output shapes are governed by the existing canonical "Per-execution MCP child exposes outcome tools via control-socket relay" AND "Per-execution MCP child exposes `outcome_request_iteration` tool" requirements AND are unchanged by this requirement.

#### Scenario: Descriptions carry operational guidance and omit narrative history
- **WHEN** the outcome-tool descriptions are reviewed against this requirement (by a human reviewer OR the drift audit)
- **THEN** each description directs the agent how to use the tool AND what content to produce
- **AND** `outcome_success`'s description directs the agent to pass a substantive `final_answer` summary AND names its reviewer-facing destination
- **AND** no description carries narrative history about prior failure modes OR superseded mechanisms (e.g. a stdout-block predecessor)

#### Scenario: Each outcome tool is advertised with a non-empty description
- **WHEN** the per-execution MCP child serves its `tools/list` response
- **THEN** each of `outcome_success`, `outcome_request_iteration`, AND `outcome_spec_needs_revision` is advertised with a non-empty `description` field
- **AND** this structural property is verified by a behavior test against the served `tools/list` output, independent of the description wording

#### Scenario: Description content intent is independent of tool schema
- **GIVEN** a future change rewrites a description AND inadvertently breaks the tool's `inputSchema` shape
- **WHEN** the change is evaluated
- **THEN** the schema violation surfaces via the existing canonical "Per-execution MCP child exposes outcome tools via control-socket relay" requirement's scenarios
- **AND** the description-content intent is governed by this requirement (review AND drift audit), independently of the schema

### Requirement: Revision prompt instructs critical evaluation of the reviewer's request
`prompts/implementer-revision.md` SHALL instruct the revision agent to evaluate the triggering request critically rather than assume it is correct. Before applying a requested change, the agent reads the actual code at the cited location, verifies the request's claim against the current state, and — when the claim is wrong (mistaken about the code, would break a passing or spec-traced test, references a symbol that does not exist, or churns working idiomatic code for protection that does not apply) — declines OR partially honors the request AND reports what it declined and why via the `outcome_success` `final_answer` summary.

Declining a wrong request is a valid, successful outcome the agent reports; it is NOT a failure AND NOT grounds to fabricate a change that satisfies the literal request at the cost of correctness. The agent reports its evaluation through the existing `final_answer` surface (no new outcome tool); the no-change declination path is handled by the orchestrator-cli `Revision execution updates the agent branch and posts a reply comment` requirement.

The guidance SHALL be language-neutral — it references "the project's test and lint commands" rather than a specific toolchain, so it applies to any managed repository.

This is design intent for the revision prompt's content. It is verified by review AND the drift audit's semantic judgment — NOT by a unit test asserting the prompt's wording (per the project-documentation requirement `Tests assert behavior or derivation, never message wording`).

#### Scenario: Revision prompt instructs claim verification before applying
- **WHEN** the revision prompt is reviewed against this requirement (by a human reviewer OR the drift audit)
- **THEN** it instructs the agent to read the cited code AND verify the request's claim against the current state before applying any change
- **AND** it states that declining or partially honoring a wrong request is a valid outcome the agent SHALL report via `final_answer`, not a failure and not grounds to fabricate a change

#### Scenario: A reasoned declination is reported, not engineered around
- **GIVEN** a request whose claim is mistaken (e.g. it references a test or symbol that does not exist, or asks to remove a spec-traced test)
- **WHEN** the agent evaluates it per the prompt's guidance
- **THEN** the prompt directs the agent NOT to make a change that satisfies the literal request at the cost of correctness
- **AND** to call `outcome_success` with a `final_answer` naming the request, the verification it performed, AND why it declined or partially honored the request

### Requirement: Executor prompt builders use single-pass substitution
The executor's multi-placeholder prompt builders — `build_revision_prompt`, `build_triage_prompt`, `build_chat_triage_prompt`, AND `build_changelog_prompt` — SHALL render their templates with the single-pass substitution helper (per the orchestrator-cli `Prompt-template substitution is single-pass` requirement), so a `{{…}}` token appearing inside an injected value (a PR body, a PR diff, an operator's revision/request text, audit findings, a canonical-specs index, OR changelog JSON) is NOT re-expanded by a later substitution. Single-replace builders (`build_prompt`, which substitutes only `{{change_body}}`) AND append-based builders (`build_recovery_prompt`) are unaffected — a single replace cannot re-expand.

This closes a self-hosting hazard: `prompts/implementer-revision.md` itself contains `{{pr_diff}}`, `{{revision_request}}`, AND `{{pr_body}}`, so revising a PR whose diff touches that template would, under chained `.replace`, re-expand those tokens inside the injected diff.

#### Scenario: A placeholder token in the PR diff is not re-expanded
- **WHEN** `build_revision_prompt` renders with a `pr_diff` whose text contains the literal `{{revision_request}}` AND `{{pr_body}}` (e.g. the PR under revision edits `prompts/implementer-revision.md`)
- **THEN** those literals appear verbatim in the rendered diff section
- **AND** the operator's revision request AND the PR body are each inserted exactly once, at the template's own placeholders
- **AND** the rendered prompt size does not grow by the number of placeholder literals carried in the diff

#### Scenario: Operator request text is not re-expanded
- **WHEN** `build_chat_triage_prompt` renders with a `request_text` that contains the literal `{{repo_url}}` OR `{{canonical_specs_index}}`
- **THEN** those literals appear verbatim
- **AND** the real `{{repo_url}}` / `{{canonical_specs_index}}` placeholders are each substituted exactly once

#### Scenario: Ordinary executor prompts are unchanged
- **WHEN** any of the four builders renders with injected values that contain no placeholder tokens
- **THEN** each placeholder is substituted exactly once
- **AND** the rendered prompt is byte-identical to the prior chained-`.replace` output

### Requirement: Shared agentic-run primitive
The daemon SHALL provide a single agentic-run primitive that wraps a CLI as a subprocess, hands it a prompt, and runs an agentic session to completion. Every CLI-wrapping role — the executor AND every audit, AND the agentic roles added by later changes — SHALL use this primitive; the per-module `run_subprocess` functions AND their duplicated `SubprocessOutcome` structs SHALL be removed.

The primitive SHALL accept the workspace, a `CliStrategy`, the prompt (delivered on stdin), the sandbox configuration (allowed-tools list AND disallowed bash/read patterns), an optional MCP configuration (which tools to expose AND the control-socket relay environment), an output mode (streaming-JSON OR simple-capture), AND a timeout. It SHALL spawn the child in its own process group, enforce the timeout via the existing select-and-kill pattern, AND return a unified `AgenticRunOutcome` carrying `timed_out`, `exit_status`, `stdout`, `stderr`, an optional `final_answer`, an optional `session_id`, AND whether a streamed log was written. The streaming-JSON event parsing (`final_answer`, `session_id`, incremental log) SHALL run ONLY in streaming mode; simple-capture mode reads stdout/stderr at exit.

The refactor SHALL be behavior-neutral: the executor retains streaming-JSON + MCP + the recovery/session-reuse path; each audit retains simple-capture + no-MCP + its existing read-only tool list AND its ETXTBSY retry.

#### Scenario: Executor path is behavior-identical through the primitive
- **WHEN** the executor runs a canned change through the primitive in streaming mode with MCP enabled
- **THEN** the streamed per-change log, the parsed `final_answer`, AND the outcome classification are identical to the pre-refactor `run_subprocess` for the same inputs

#### Scenario: Audit path is behavior-identical through the primitive
- **WHEN** an audit runs through the primitive in simple-capture mode with no MCP AND its existing allowed-tools list
- **THEN** the returned `stdout` AND `exit_status` are identical to the pre-refactor audit `run_subprocess`
- **AND** no `.mcp.json` is written for that run

#### Scenario: Single source of truth
- **WHEN** the codebase is searched after this change
- **THEN** no `run_subprocess` or `SubprocessOutcome` definition exists outside the agentic-run module

### Requirement: CliStrategy trait with the claude implementation
The agentic-run primitive SHALL select its CLI invocation through a `CliStrategy` trait so a model's provider can determine the CLI without role code changing. The trait SHALL do two jobs: build the invocation (binary, flags, the allowed-tools/sandbox-settings format, AND the MCP-config-file format) AND translate a resolved `(provider, model, api_base_url, api_key)` into that CLI's model-selection mechanism. A role's strategy SHALL be resolved from the model's provider via the model registry's `provider → default CLI` rule.

This change SHALL implement the `claude` strategy AND reproduce today's invocation exactly: `--settings <sandbox-file>`, `--allowedTools <combined>`, `--permission-mode acceptEdits`, AND — in streaming mode — `--verbose --output-format stream-json`, with MCP delivered via `.mcp.json`. The `claude` strategy SHALL select the model via `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` ONLY when a model is configured; when no model is configured it SHALL set none of them, preserving the executor's current CLI-default behavior. A role whose provider resolves to a CLI with no registered strategy SHALL return a clear error naming that CLI; this change registers only the `claude` strategy, so any non-`claude` resolution errors until that CLI's strategy is added (the `opencode` strategy is added by a later change).

#### Scenario: Claude strategy with no model preserves CLI-default behavior
- **WHEN** the `claude` strategy builds an invocation with `model: None` (the executor's current state)
- **THEN** none of `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` is set
- **AND** the invocation is byte-identical to the pre-refactor executor command

#### Scenario: Claude strategy with a model sets the selection env
- **WHEN** the `claude` strategy builds an invocation with a resolved model `(anthropic, claude-opus-4-8, base, key)`
- **THEN** `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, AND `ANTHROPIC_MODEL` are set from the resolved tuple

#### Scenario: A CLI with no registered strategy returns a clear error
- **WHEN** a role's model resolves (via the registry rule) to a CLI that has no registered strategy (e.g. `opencode`, before its strategy is added)
- **THEN** strategy resolution returns an error naming the CLI
- **AND** no subprocess is spawned

### Requirement: Per-execution MCP child exposes a per-role submission tool via control-socket relay
The per-execution MCP child SHALL support a per-role structured-submission tool family that relays a schema-validated payload to the daemon over the control socket, paralleling the existing `outcome_*` / `record_outcome` relay. The MCP child SHALL read an `ORCH_MCP_ROLE` value from its environment (written into `.mcp.json` by the config writer) AND advertise only that role's `submit_*` tool alongside the common tools; a child with no role advertises no submission tool.

This change establishes the framework AND the relay helper only. The concrete per-role tools (`submit_findings`, `submit_review`, `submit_contradictions`, `submit_verdict`) AND their schemas SHALL be added by the changes that consume them, each following this pattern. The relay SHALL send a control-socket request naming the role AND the payload, AND SHALL surface a tool error to the agent when the daemon rejects the submission (e.g. schema-invalid), so the agent can correct AND retry in the same session.

#### Scenario: Role-scoped advertisement
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE` set to a role that has a registered submission tool
- **THEN** the `tools/list` response advertises that role's `submit_*` tool AND the common tools (e.g. `query_canonical_specs`)
- **AND** it does NOT advertise submission tools for other roles

#### Scenario: Submission relays to the daemon
- **WHEN** an agent calls its role's `submit_*` tool with a valid payload
- **THEN** the MCP child relays a `record_submission` request over the control socket naming the role AND the payload
- **AND** a daemon rejection (e.g. schema-invalid) is surfaced to the agent as a correctable tool error

### Requirement: submit_findings MCP tool returns advisory-audit findings
The per-execution MCP child SHALL advertise a `submit_findings` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE` names an advisory audit (`drift_audit`, `architecture_consultative`, OR `documentation_audit`). The tool's payload schema is the audit-specific finding shape registered for that role: drift findings carry `{capability, requirement, severity, code_anchors, divergence}`; architecture findings carry `{subject, body, anchor, severity}` with the array capped at 5 entries; documentation findings carry `{category, severity, anchor, body}`. A non-advisory role (the executor `implementer`, the specs-writing audits `missing_tests` / `security_bug`) SHALL NOT advertise `submit_findings`.

The three advisory audits SHALL run through a56's `agentic_run` primitive WITH MCP enabled (capture mode retained, existing read-only allowed-tools list) so the tool is reachable; this supersedes a56's interim "audits run with no MCP" for these three roles ONLY. The agent returns findings by calling `submit_findings`; after the audit subprocess exits the daemon `consume_submission`s the stored payload (a56) to produce the `AuditOutcome::Reported` findings. A `submit_findings` call whose payload fails the role schema is rejected by `record_submission` AND surfaced to the agent as a correctable tool error it can retry in the same session; an audit run that ends with NO stored submission is an audit failure.

#### Scenario: Advertised only for advisory roles
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = architecture_consultative`
- **THEN** the `tools/list` response advertises `submit_findings` with the architecture finding schema alongside the common tools
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = implementer`, `missing_tests`, OR `security_bug`
- **THEN** `submit_findings` is NOT advertised

#### Scenario: Submission becomes the audit result
- **WHEN** an advisory audit's agent calls `submit_findings` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the subprocess exits the daemon `consume_submission`s the stored payload into `Finding` values for `AuditOutcome::Reported`

#### Scenario: Schema-invalid submission is correctable, not fatal
- **WHEN** a `submit_findings` payload violates the role schema (a missing required field, OR more than 5 architecture findings)
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session
- **AND** a single rejection does NOT fail the audit on its own — a subsequent valid submission in the same execution is accepted

#### Scenario: No submission fails the audit
- **WHEN** an advisory-audit subprocess exits with no stored submission for the execution
- **THEN** the audit returns `Err` (audit failure: state not updated, chatops audit-failure alert posts, the next iteration retries)

#### Scenario: Advisory audits gain MCP; specs-writing audits do not
- **WHEN** a `drift_audit`, `architecture_consultative`, OR `documentation_audit` run is built
- **THEN** it invokes `agentic_run` with MCP enabled (the `submit_findings` tool + `ORCH_MCP_ROLE`), in capture mode, with the audit's existing read-only allowed-tools list
- **WHEN** a `missing_tests` OR `security_bug` run is built
- **THEN** it invokes `agentic_run` with NO MCP (unchanged from a56), producing its on-disk proposal as before

### Requirement: submit_review MCP tool returns the reviewer verdict
The per-execution MCP child SHALL advertise a `submit_review` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE = reviewer`, AND SHALL NOT advertise it for any other role. The tool's payload schema SHALL be `{ verdict: "Approve" | "Block", summary: string, concerns: [{ title: string, detail: string, anchor: string, should_request_revision: bool, actionable_request: string|null }] }`. The schema SHALL enforce the `verdict` enum AND SHALL require a non-empty `actionable_request` whenever `should_request_revision` is `true`. The tool relays through a56's `relay_submission` → `record_submission`.

A schema-invalid `submit_review` payload (a verdict outside the enum, a `should_request_revision` concern with no `actionable_request`, a malformed shape) SHALL be rejected by `record_submission` AND surfaced to the agent as a correctable tool error it can retry in the same session. After the reviewer session exits the daemon `consume_submission`s the stored payload into a `ReviewResult` (`verdict`, `per_concern`, `raw_output`). A reviewer session that ends with NO stored submission SHALL cause the caller to discard the review AND alert the operator (it SHALL NOT be treated as an implicit `Approve`). This is the structural retirement of the malformed-verdict-defaults-to-approve behavior: the verdict can only enter the daemon through the schema-validated tool.

#### Scenario: Advertised only for the reviewer role
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = reviewer`
- **THEN** the `tools/list` response advertises `submit_review` with the review schema alongside the common tools
- **WHEN** the MCP child starts with any other role (`implementer`, an advisory audit, a specs-writing audit)
- **THEN** `submit_review` is NOT advertised

#### Scenario: Valid submission becomes the ReviewResult
- **WHEN** the reviewer agent calls `submit_review` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the session exits the daemon `consume_submission`s the payload into a `ReviewResult` whose `verdict` AND `per_concern` come from the submission

#### Scenario: Schema-invalid submission is correctable, not fatal
- **WHEN** a `submit_review` payload has a `verdict` outside `{Approve, Block}`, OR a concern with `should_request_revision: true` AND an empty `actionable_request`
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session
- **AND** a single rejection does NOT discard the review on its own — a subsequent valid submission in the same execution is accepted

#### Scenario: No submission discards the review, never auto-approves
- **WHEN** a reviewer session exits with no stored submission for the execution
- **THEN** the caller discards the review (writes no verdict) AND posts the reviewer-failure operator alert
- **AND** the outcome is NOT an implicit `Approve`

### Requirement: submit_contradictions MCP tool returns change-internal contradictions
The per-execution MCP child SHALL advertise a `submit_contradictions` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE = contradiction_check`, AND SHALL NOT advertise it for any other role. The tool's payload schema SHALL be `{ contradictions: [{ requirement_a: string, requirement_b: string, summary: string }] }`. The tool relays through a56's `relay_submission` → `record_submission`; a schema-invalid payload is rejected AND surfaced to the agent as a correctable tool error it can retry in the same session.

Because the contradiction check is fail-open (per the orchestrator-cli requirement), a session that ends with no stored submission SHALL be consumed as an empty result rather than an error — the fail-open WARN-and-proceed decision lives in the orchestrator-cli caller, not in this tool. A non-empty consumed submission carries the contradictions the caller turns into the `.needs-spec-revision.json` marker.

#### Scenario: Advertised only for the contradiction-check role
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = contradiction_check`
- **THEN** the `tools/list` response advertises `submit_contradictions` with the contradictions schema alongside the common tools
- **WHEN** the MCP child starts with any other role (`implementer`, `reviewer`, an advisory audit)
- **THEN** `submit_contradictions` is NOT advertised

#### Scenario: Valid submission is consumed by the caller
- **WHEN** the agent calls `submit_contradictions` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the session exits the daemon `consume_submission`s the stored payload for the orchestrator-cli caller to act on

#### Scenario: Schema-invalid submission is correctable
- **WHEN** a `submit_contradictions` payload fails the schema (missing field, non-array `contradictions`)
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session

#### Scenario: Missing submission consumed as empty, not an error
- **WHEN** a contradiction-check session exits with no stored submission for the execution
- **THEN** `consume_submission` returns an empty result (no contradictions)
- **AND** the tool layer does NOT raise an error — the orchestrator-cli caller's fail-open policy decides the WARN-and-proceed outcome

### Requirement: OpencodeStrategy implements the opencode CLI for agentic roles
The daemon SHALL provide a second `CliStrategy` (a56), `OpencodeStrategy`, for the `opencode` CLI, so a role whose model provider resolves to `opencode` (a55's `provider → CLI` rule for `openai_compatible`/`ollama`, OR an explicit registry `cli: opencode`) runs agentically instead of erroring with "no registered strategy."

`OpencodeStrategy` SHALL build an `opencode run` invocation whose model selection follows opencode's own contract: opencode's `--model` is `<opencode-provider-id>/<model>`, where the provider is one opencode actually knows. Autocoder's `LlmProvider` value (e.g. `openai_compatible`) is an API *type*, NOT an opencode provider id, AND SHALL NOT be used as the `--model` provider segment (`opencode models openai_compatible` returns "Provider not found"). Two cases follow:

- **autocoder DEFINES the provider** — `ollama` (always: opencode is not `auth login`-ed to a local daemon, so its base URL must be supplied) AND `openai_compatible` WHEN an `api_key` is supplied (autocoder injects it). The strategy SHALL write an `opencode.json` `provider` block carrying the base URL (and, for a supplied key, `apiKey` as an `{env:…}` REFERENCE — never the raw secret, which rides the subprocess env) AND select `--model <provider-id>/<model>`, where `<provider-id>` matches the block it wrote.
- **autocoder DEFERS to opencode's own auth** — an authenticating provider (`openai_compatible`) with NO `api_key` (the operator authenticated it out-of-band via `opencode auth login`). The strategy SHALL write NO `provider` block — a key-less block would shadow opencode's own stored credentials for that provider and break authentication ("No cookie auth credentials found") — AND SHALL pass the operator-configured model to `--model` VERBATIM. The operator's `model` value MUST therefore be the real opencode id (e.g. `openrouter/qwen/qwen3-max`); autocoder neither assumes nor infers the provider.

In all cases the strategy SHALL write the `opencode.json` `mcp` block (`type: local`, the MCP-child command, AND env including `ORCH_MCP_ROLE`) AND map a56's sandbox (allowed-tools list + deny patterns) onto opencode's permission configuration so a read-only role keeps its read-only profile. It SHALL set NO `ANTHROPIC_*` env (that is the `claude` strategy's mechanism), AND SHALL NOT write `.mcp.json` (the `claude` MCP format). The role's prompt SHALL be delivered by whichever mechanism headless `opencode run` accepts.

Every agentic role that drives opencode — the verifier gates AND the agentic reviewer — SHALL pass its resolved model to the strategy (NOT `None`), so opencode runs the operator-configured model rather than opencode's own default. (A role that passes `None` would silently run opencode's default while any verdict attribution named the configured model.)

`OpencodeStrategy` SHALL run in capture mode; the streaming-JSON event path (`final_answer` / `session_id` / incremental log) is `claude`-specific. opencode therefore serves the capture-mode structured-submission roles (the advisory audits, the reviewer, the contradiction check); the executor's streaming implementer path remains on the `claude` strategy. The opencode integration SHALL surface MCP tool calls AND surface a daemon-rejected submission to the model as a correctable tool error it can retry in the same session — the same submission contract a56 requires of the `claude` path.

Registering `opencode` unblocks the non-Anthropic agentic paths of the reviewer (a58) AND the contradiction check (a59); it does NOT change any role's default transport.

#### Scenario: Opencode provider resolves to a working strategy
- **WHEN** a role's model resolves (via a55's `provider → CLI` rule, OR an explicit `cli: opencode`) to the `opencode` CLI
- **THEN** strategy resolution returns `OpencodeStrategy` (NOT a "no registered strategy" error)
- **AND** it builds an `opencode run` invocation

#### Scenario: MCP and role env are delivered via opencode.json
- **WHEN** an `opencode` role runs with a structured-submission tool (e.g. `submit_review`)
- **THEN** the strategy writes `opencode.json` with an `mcp` block (`type: local`, the MCP-child command, env including `ORCH_MCP_ROLE`) so the role's `submit_*` tool is reachable
- **AND** NO `.mcp.json` is written for that run

#### Scenario: A login-authed provider defers to opencode (no block, verbatim model)
- **WHEN** the resolved model is an `openai_compatible` provider with NO `api_key` AND `model` `openrouter/qwen/qwen3-max`
- **THEN** `opencode.json` carries NO `provider` block (opencode resolves the provider + credentials from its own `auth login` + config)
- **AND** the invocation selects `--model openrouter/qwen/qwen3-max` VERBATIM — autocoder's `openai_compatible` type is NOT prefixed
- **AND** none of `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` is set

#### Scenario: A keyed provider is defined by autocoder
- **WHEN** the resolved model is `(openai_compatible, <model>, <base_url>, <key>)` (a key IS supplied)
- **THEN** `opencode.json` carries a `provider` block with the base URL AND `apiKey` as an `{env:…}` reference (the secret on the subprocess env, never raw in the file)
- **AND** the invocation selects `--model openai_compatible/<model>` (matching the block autocoder wrote)
- **AND** none of `ANTHROPIC_*` is set

#### Scenario: Ollama is always defined by autocoder
- **WHEN** the resolved model is `(ollama, <model>, <base_url>, "")`
- **THEN** `opencode.json` carries an `ollama` `provider` block with the base URL (no `apiKey`)
- **AND** the invocation selects `--model ollama/<model>`

#### Scenario: Agentic roles run their configured model, not opencode's default
- **WHEN** the agentic reviewer OR a verifier gate runs through `OpencodeStrategy`
- **THEN** it passes its resolved model to the strategy (not `None`)
- **AND** opencode runs that model, not opencode's own default

#### Scenario: Read-only sandbox is enforced via opencode permissions
- **WHEN** a read-only role (a56 sandbox: allow Read/Glob/Grep; deny Write/Edit/Bash) runs under opencode
- **THEN** the generated opencode permission configuration denies Write, Edit, AND Bash
- **AND** exposes only the read tools plus the role's MCP submission tool

#### Scenario: Capture mode only; streaming stays on claude
- **WHEN** an `opencode` role runs through `agentic_run`
- **THEN** it uses capture mode (stdout/stderr read at exit), NOT the streaming-JSON parse path
- **AND** the executor's streaming implementer path continues to use the `claude` strategy

#### Scenario: Submission contract holds under opencode
- **WHEN** an `opencode` role's agent calls its `submit_*` tool AND the daemon rejects the payload (schema-invalid)
- **THEN** the rejection reaches the model as a tool error it can correct AND retry within the same `opencode run` session
- **AND** this matches the correctable-tool-error contract a56 requires of the `claude` path

#### Scenario: Non-Anthropic agentic roles now function
- **WHEN** the reviewer (`reviewer.kind: agentic`) OR the contradiction check is configured with a model whose provider resolves to `opencode`
- **THEN** the role runs agentically via `OpencodeStrategy`
- **AND** it no longer errors / fails open on "no registered strategy"

### Requirement: submit_canon_contradictions MCP tool returns change-vs-canonical contradictions
The per-execution MCP child SHALL advertise a `submit_canon_contradictions` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE = canon_contradiction_check`, AND SHALL NOT advertise it for any other role. The tool's payload schema SHALL be `{ contradictions: [{ change_requirement: string, canonical_capability: string, canonical_requirement: string, summary: string }] }` — each finding names the canonical requirement (by capability AND title) that the change's requirement conflicts with, distinguishing it from the `[in]` gate's within-change `submit_contradictions`. The tool relays through a56's `relay_submission` → `record_submission`; a schema-invalid payload is rejected AND surfaced to the agent as a correctable tool error it can retry in the same session.

Because the `[canon]` gate is fail-open (per the orchestrator-cli requirement AND the a61 framework), a session that ends with no stored submission SHALL be consumed as an empty result rather than an error — the fail-open WARN-and-proceed decision lives in the orchestrator-cli caller.

#### Scenario: Advertised only for the canon-check role
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = canon_contradiction_check`
- **THEN** the `tools/list` response advertises `submit_canon_contradictions` with the canon-contradictions schema alongside the common tools
- **WHEN** the MCP child starts with any other role (`implementer`, `reviewer`, `contradiction_check`, an advisory audit)
- **THEN** `submit_canon_contradictions` is NOT advertised

#### Scenario: Valid submission is consumed by the caller
- **WHEN** the agent calls `submit_canon_contradictions` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the session exits the daemon `consume_submission`s the stored payload for the orchestrator-cli caller to turn into the marker

#### Scenario: Schema-invalid submission is correctable
- **WHEN** a `submit_canon_contradictions` payload fails the schema (missing `canonical_requirement`, non-array `contradictions`)
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session

#### Scenario: Missing submission consumed as empty, not an error
- **WHEN** a `[canon]` session exits with no stored submission for the execution
- **THEN** `consume_submission` returns an empty result
- **AND** the tool layer does NOT raise an error — the orchestrator-cli caller's fail-open policy decides the outcome

### Requirement: submit_verdict MCP tool returns the code-implements-spec verdict
The per-execution MCP child SHALL advertise a `submit_verdict` tool — the last of a56's reserved per-role submission tools, built on the same framework — whenever `ORCH_MCP_ROLE = code_implements_spec`, AND SHALL NOT advertise it for any other role. The tool's payload schema SHALL be `{ verdict: "implemented" | "gaps_found", summary: string, gaps: [{ requirement: string, scenario: string|null, status: "missing" | "partial", evidence: string }] }`. The schema SHALL enforce the `verdict` enum AND SHALL require a non-empty `gaps` array whenever `verdict: gaps_found`. The tool relays through a56's `relay_submission` → `record_submission`; a schema-invalid payload is rejected AND surfaced to the agent as a correctable tool error it can retry in the same session.

Because the `[out]` gate is advisory (per the orchestrator-cli requirement AND the a61 framework), a session that ends with no stored submission SHALL be consumed as an empty result rather than an error — the caller omits the `## Spec Verification` section AND logs a WARN; it never blocks. A consumed `gaps_found` verdict drives the advisory annotation AND the chatops heads-up, never a revision.

#### Scenario: Advertised only for the code-implements-spec role
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = code_implements_spec`
- **THEN** the `tools/list` response advertises `submit_verdict` with the verdict schema alongside the common tools
- **WHEN** the MCP child starts with any other role (`implementer`, `reviewer`, a contradiction gate, an advisory audit)
- **THEN** `submit_verdict` is NOT advertised

#### Scenario: Valid verdict is consumed by the caller
- **WHEN** the agent calls `submit_verdict` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the session exits the daemon `consume_submission`s the payload for the orchestrator-cli caller to render the advisory section

#### Scenario: gaps_found requires a non-empty gaps array
- **WHEN** a `submit_verdict` payload has `verdict: "gaps_found"` AND an empty `gaps` array, OR a `verdict` outside the enum
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session

#### Scenario: Missing submission consumed as empty, never blocks
- **WHEN** a `[out]` session exits with no stored submission for the execution
- **THEN** `consume_submission` returns an empty result
- **AND** the tool layer does NOT raise an error — the orchestrator-cli caller omits the advisory section AND proceeds (the gate never blocks)

### Requirement: CLI strategies pass no LLM credential to the wrapped subprocess
An agentic CLI role's credential handling SHALL depend on whether the operator supplied an `api_key`, per the two cases below:

- **No key (the default).** No `CliStrategy` SHALL place any LLM credential in the wrapped subprocess — NOT in a workspace file (`opencode.json`, `mcp_config.json`, `.gemini/*`, etc.), AND NOT in the subprocess environment. The strategy SHALL select the model (e.g. `--model`) AND rely on the CLI's **own** authentication — its credential store or login (`claude login`, opencode / its provider config, `agy` login), or the operator's out-of-band CLI provider config (e.g. opencode → OpenRouter). This is the safe default: no credential ever reaches the model.
- **Key supplied (an explicit opt-in).** When a CLI role has a configured `api_key`, the strategy SHALL pass it to the CLI so the CLI uses that key — uniformly across the three CLIs: `claude` via `ANTHROPIC_API_KEY`, the `opencode` strategy via opencode's own provider config, AND `agy` via `AV_API_KEY`. A supplied key SHALL be placed where the existing config-store protection covers it — the CLI's own config store, reached by the `engine_deny` tool denylist — AND SHALL NEVER be written into a workspace file (a workspace file can be committed AND is freely readable by the model).

The supplied-key path cannot fully isolate the credential from the model: the model AND the wrapped CLI are the **same process AND uid**, so a key the CLI can use is one the model can ultimately reach. `engine_deny` is deterrence, not a bound (see the os-hide/engine-deny requirement), AND a CLI that accepts a key only via the subprocess environment (e.g. `claude` → `ANTHROPIC_API_KEY`) leaves the key readable from the model's own environment. Supplying a key is therefore an explicit operator opt-in to that exposure; the no-key default preserves the no-credential posture. The daemon SHALL document this residual rather than claim isolation it cannot provide.

A resolved `api_key` SHALL still flow to autocoder's **in-process** HTTP clients (the non-agentic `oneshot` reviewer AND any RAG/embedding HTTP call), which the daemon calls directly so the key stays in the daemon's process; those are not subprocesses AND are unaffected by the CLI-role rules above.

#### Scenario: No-key CLI role passes no credential to the subprocess
- **WHEN** a CLI role's resolved model has no `api_key`
- **THEN** the strategy writes no credential into any workspace file
- **AND** sets no credential in the subprocess environment
- **AND** the CLI authenticates from its own login / credential store

#### Scenario: A supplied key is passed to the CLI
- **WHEN** a CLI role's resolved model has a non-empty `api_key`
- **THEN** the strategy makes the CLI use that key — `claude` via `ANTHROPIC_API_KEY`, `opencode` via its provider config, `agy` via `AV_API_KEY`

#### Scenario: A supplied key is never written to a workspace file
- **WHEN** any `CliStrategy` writes its config for a role whose resolved model has an `api_key`
- **THEN** no credential appears in any file written into the workspace (e.g. the workspace `opencode.json` carries the MCP block, the permission/sandbox config, AND the provider's model + base URL, but NOT the `api_key`)
- **AND** a supplied key is placed only in a location covered by `engine_deny` (the CLI's own config store) OR, for a CLI that accepts a key only via the environment, in the subprocess environment with the residual documented

#### Scenario: The supplied-key location is engine-deny covered
- **WHEN** a key is supplied AND written to the CLI's config store
- **THEN** that location is included in the `engine_deny` tool-denylist applied for the run
- **AND** the protection is understood as deterrence, not a bound (same-process / same-uid residual)

#### Scenario: In-process HTTP roles still receive the key
- **WHEN** the non-agentic `oneshot` reviewer (or a RAG/embedding HTTP call) runs with a configured `api_key`
- **THEN** the key is used by the daemon's in-process HTTP client for that call
- **AND** the key is never passed to a subprocess (file or env)

### Requirement: Every agentic subprocess runs inside an OS-level sandbox
Every role that spawns a CLI through the shared `agentic_run` primitive — the executor, every audit, AND any agentic role added by other changes (e.g. an agentic reviewer) — SHALL have that subprocess wrapped in an OS-level sandbox enforced by the kernel, NOT by the wrapped CLI's own sandbox. The wrap is a property of the single `agentic_run` spawn seam, so no role can opt out. The in-process HTTP roles (the non-agentic `oneshot` reviewer AND the contradiction-check LLM block) spawn no subprocess and are out of scope. This requirement governs the OS-level sandbox; it does not change the canonical tool-use-sandbox scoping (the CLI permission layer), which sits beside it.

The sandbox SHALL be applied by a **platform-appropriate mechanism**: on Linux via `systemd-run` in transient-service mode (so PID 1 applies the filesystem and namespace properties; stdout captured with `--pipe --wait --collect`), with a bubblewrap (`bwrap`) fallback for hosts without a usable system manager; on macOS via `sandbox-exec` (the Seatbelt sandbox) with a generated profile.

**The default filesystem policy is the exposed-home denylist for every role**, because a wrapped CLI and the toolchains it drives live under `$HOME` (node/pyenv/rbenv/cargo, and the CLI's own install + session + caches). Roles differ only in **workspace** writability:

- **Exposed home, default-deny mask-list (denylist) — executor AND read-only roles.** The home directory SHALL be present AND writable, so toolchains installed under `$HOME` (`~/.cargo`, `~/.rustup`, `~/.nvm`, `~/.pyenv`, `~/.rbenv`, the CLI's own install + session, caches, …) work without enumeration — EXCEPT a default-deny **mask-list** of sensitive paths which SHALL be masked (replaced with empty or inaccessible mounts). The mask-list covers **credential paths** (read-protection: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.netrc`, cloud-token dirs, other CLIs' config stores, package-manager credential files such as `~/.cargo/credentials.toml` / `~/.npmrc`) AND **shell-init/persistence paths** (write-protection: `~/.bashrc`, `~/.profile`, `~/.ssh/authorized_keys`, autostart/cron). It ships with defaults AND is operator-editable (see the orchestrator-cli config requirement). System paths outside `$HOME` are visible read-only.
- **Strict mode — opt-in masked-home allowlist.** An operator MAY opt into the masked-home allowlist for high-compliance hosts: the home directory SHALL be masked; the subprocess sees only the workspace, the running role's own CLI config store (read-only, for authentication), the **resolved CLI binary AND its runtime dependency closure** (following symlinks, even when installed under `~/.local/bin`), AND the minimal runtime. This is NOT the default, and it accepts that a toolchain-heavy CLI (e.g. a Node app whose runtime sprawls under `$HOME`) may be unable to start under the mask.

The workspace SHALL be read-write for the executor AND read-only for read-only roles, in every policy — EXCEPT that a read-only role's workspace SHALL expose a writable, ephemeral project-scratch subtree where the running CLI requires one (e.g. opencode writes `<workspace>/.opencode/` and crashes if it cannot). That subtree SHALL be overlaid writable (a tmpfs, discarded after the run, on the Linux mechanisms) so the CLI's project scratch works while the repo files stay read-only; it SHALL be derived from the role's resolved CLI, NOT operator-supplied. The home directory's read-WRITE exposure under the denylist applies to read-only roles too: their "read-only" is the workspace's tracked files, not the home — a read-only role may read the home AND write its own caches/session there, but SHALL NOT modify the repo.

- **Capability / operation restriction.** On Linux: drop `CAP_NET_RAW` (no raw-socket sniffing), `CAP_NET_ADMIN` (no route/iptables hijack), AND `CAP_SYS_PTRACE` (no reading another process's memory); `NoNewPrivileges`; address families restricted to exclude `AF_PACKET`. On macOS the generated Seatbelt profile SHALL deny the equivalents where the platform exposes them — raw/packet networking, inspection of other processes, AND privilege elevation.
- **Process-table restriction.** On Linux, `/proc` mounted so the subprocess cannot read another process's `environ` or `mem`. On macOS (which has no `/proc`), the Seatbelt profile SHALL deny process-information access to other processes.

Outbound network egress SHALL NOT be restricted by this sandbox: network egress control belongs to the host firewall, not the daemon. The sandbox does filesystem and host isolation, not a network allowlist.

#### Scenario: The executor sees the host toolchains under an exposed home
- **WHEN** the executor spawns through `agentic_run` under the default (denylist) policy
- **THEN** the home directory and its build toolchains (e.g. `~/.cargo`, `~/.pyenv`, `~/.nvm`) are readable AND tool caches are writable
- **AND** the workspace is read-write

#### Scenario: A masked credential is unreadable even via Bash
- **WHEN** the spawned agent attempts to read a mask-listed credential (e.g. `~/.ssh/id_ed25519`, another CLI's store, or `~/.cargo/credentials.toml`) through any tool, including a `Bash` command such as `cat`, `head`, or `python -c open()`
- **THEN** the read fails because the path is masked
- **AND** the failure does not depend on the wrapped CLI's own permission rules

#### Scenario: A masked persistence file cannot be written
- **WHEN** the spawned agent attempts to write a mask-listed persistence file (e.g. `~/.bashrc` or `~/.ssh/authorized_keys`)
- **THEN** the write does not persist to the real file because the path is masked

#### Scenario: Read-only roles get the exposed home with a read-only workspace
- **WHEN** a read-only role (an audit, an agentic reviewer, or a verifier gate) spawns through `agentic_run` under the default policy
- **THEN** the home directory is present — readable so the CLI finds its toolchain runtime, AND writable so the CLI can write its own session/cache — with the credential mask-list still masked
- **AND** the workspace's tracked files are read-only: an attempt by that role to modify a repo file fails
- **AND** an attempt to read a mask-listed credential still fails

#### Scenario: A read-only role's CLI writes its project scratch
- **WHEN** a read-only role runs a CLI that writes a project-local scratch directory in its working directory (e.g. opencode writing `<workspace>/.opencode/`)
- **THEN** that scratch subtree is writable (overlaid on the read-only workspace), so the CLI does not crash on the write
- **AND** the rest of the workspace stays read-only (a repo-file write still fails)
- **AND** the scratch is ephemeral on the tmpfs mechanisms (its writes are discarded after the run, not persisted to the host workspace)

#### Scenario: The CLI binary is reachable regardless of policy
- **WHEN** the running role's CLI binary is installed under the home directory (e.g. `~/.local/bin/<cli>` or `~/.opencode/bin/<cli>`)
- **THEN** under the default denylist policy (the executor OR a read-only role) it is simply visible, with its runtime, because the home is present
- **AND** under the strict-mode allowlist it is bound — following symlinks — with its dependency closure, read-only and executable, so the wrapped CLI execs

#### Scenario: Capability drops block sniffing and cross-process reads
- **WHEN** the spawned agent attempts to open a raw/packet socket OR to ptrace or read another process's memory
- **THEN** the operation fails because the capability is not in the subprocess's bounding set

#### Scenario: Enforcement is external to the CLI
- **WHEN** the wrapped CLI's own sandbox configuration would otherwise permit a masked or out-of-allowlist read
- **THEN** the read still fails, because the OS-level policy is enforced by the kernel around the subprocess regardless of the CLI's settings

#### Scenario: Fallback on a host without a usable system manager
- **WHEN** the daemon runs where `systemd-run` cannot apply the sandbox (unprivileged or non-systemd environment) AND `bwrap` is available
- **THEN** `agentic_run` applies the equivalent policy via the `bwrap` fallback
- **AND** no unsandboxed subprocess is spawned

#### Scenario: macOS applies the sandbox via sandbox-exec
- **WHEN** the daemon runs on macOS
- **THEN** `agentic_run` applies the OS-level policy via `sandbox-exec` with a generated Seatbelt profile (exposed-home-minus-mask-list for the executor AND read-only roles, with the workspace write-denied for read-only roles; the masked-home allowlist for strict mode)
- **AND** no unsandboxed subprocess is spawned

#### Scenario: Strict mode masks all of home for the executor
- **WHEN** the operator opts the executor into strict mode
- **THEN** the executor runs under the allowlist (home masked; only the workspace read-write, the role's own store, the resolved CLI binary + toolchain, and the minimal runtime bound)

### Requirement: CLI config stores are protected by OS-hide and engine-deny
A model running as one CLI SHALL NOT be able to read another CLI's credential/config store, AND SHALL be deterred from reading its own. Two complementary layers enforce this, each independently toggleable (`os_hide`, `engine_deny`), both ON by default:

- **`os_hide` (mask-list membership).** The config store of every CLI OTHER than the running role's own is in the sandbox **mask-list**, so it is masked (absent) from the subprocess regardless of the role's filesystem policy. It cannot protect the running role's OWN store, which must stay readable for the CLI to authenticate; it protects every other store. Turning `os_hide` off removes the other CLI stores from the mask-list (exposing them, for the wrapper-development case).
- **`engine_deny` (the wrapped CLI's own permission denylist; fail-open).** The per-invocation tool-use settings the executor already supplies to the CLI (the canonical "Tool-use sandbox is applied at every spawn" mechanism) SHALL deny the agent's file-reading tools (`Read`, AND the corresponding `Bash` patterns) on the config store of EVERY registered CLI strategy — the running role's own included. This covers the self-store that `os_hide` cannot, but is a string-pattern speed bump that determined shell indirection can evade: it deters, it does not bound.

The engine-deny rules SHALL be supplied per-invocation through each CLI's own settings mechanism (as the existing tool-use sandbox already does for `claude`), NOT by mutating the operator's global CLI configuration.

The running role's own CLI store stays readable by that same-uid subprocess because the CLI must read it to authenticate; disclosure of that one store means a model could impersonate that CLI to its own provider, never reach another provider's credential or another secret. This residual is NOT network-contained (egress is out of scope); it is bounded by the single-store blast radius AND by `engine_deny` deterrence.

#### Scenario: Under the default, another CLI's store is unreadable
- **WHEN** a role running as one CLI attempts to read a different registered CLI's config store under the default (`os_hide` on)
- **THEN** that store is absent from the namespace AND the read fails

#### Scenario: With os_hide off, other stores are still engine-denied
- **WHEN** `os_hide` is off for the run AND `engine_deny` is on
- **THEN** another CLI's config store is present in the namespace (so a nested CLI of that kind could authenticate)
- **AND** the agent's `Read`/`Bash` tools are denied that store's paths at the CLI permission layer

#### Scenario: The self-store authenticates but is engine-denied to the agent
- **WHEN** a role runs as a CLI whose own config store is in the namespace read-only for authentication
- **THEN** the CLI authenticates from that store
- **AND** the agent's `Read`/`Bash` tools are denied that store's paths at the CLI permission layer

#### Scenario: Deny rules are per-invocation, not global mutation
- **WHEN** the engine-deny rules are applied for a run
- **THEN** they are delivered via the per-invocation settings mechanism (e.g. the temp Claude Code settings file)
- **AND** the operator's global CLI configuration is not modified

### Requirement: Issue-flavored implementer prompt verifies against existing canon
When the executor runs an issue (an `issues/<slug>/` unit, NOT a change), it SHALL use an issue-flavored implementer prompt that instructs: fix the code to match the EXISTING specification; do NOT invent or write a spec change; AND if the fix actually requires new or changed behavior, report that the item belongs in the changes lane (kick it back) rather than altering any spec. The prompt SHALL be loaded through the uniform PromptLoader AND declare its override field via the nested naming convention. Acceptance for an issue run SHALL be verified against the existing canon, not a spec delta.

#### Scenario: An issue run uses the issue-flavored prompt
- **WHEN** the executor runs an `issues/<slug>/` unit
- **THEN** it uses the issue-flavored implementer prompt (fix-to-existing-spec framing)
- **AND** not the change implementer prompt

#### Scenario: A behavior-change fix is kicked back to changes
- **WHEN** an issue's fix would require new or changed behavior
- **THEN** the run reports that the item belongs in the changes lane
- **AND** it does NOT modify any spec

#### Scenario: Acceptance is evaluated against canon
- **WHEN** an issue run completes
- **THEN** its acceptance is evaluated against the existing specification, not a spec delta

### Requirement: Public issue body is quarantined as untrusted data in the implementer prompt
When an issue originates from a public author, the implementer prompt SHALL embed the issue body as DATA inside a robust delimiter — NOT a markdown fence the body can break out of — with an explicit untrusted-report framing. The task AND scope SHALL come from the lane and the maintainer-approved classification, NEVER from the body. Single-pass substitution SHALL prevent `{{token}}` expansion of placeholder-looking text inside the body.

#### Scenario: The body is embedded as untrusted data
- **WHEN** a public-origin issue is run
- **THEN** its body is placed in a delimited untrusted-data region distinct from the instruction region
- **AND** the delimiter is not a markdown fence the body can break out of

#### Scenario: Body instructions do not become the task
- **WHEN** the issue body contains instruction-like text
- **THEN** the task is taken from the maintainer-approved classification, not from the body

#### Scenario: No token expansion inside the body
- **WHEN** the issue body contains `{{token}}`-looking text
- **THEN** it is not expanded during prompt construction

### Requirement: Agentic subprocesses inherit the operator's activated toolchain environment, credential-filtered
The daemon SHALL capture the operator's login-shell environment — the activated `PATH` AND toolchain-activation variables (e.g. `PYENV_ROOT`, `RBENV_ROOT`, `NVM_DIR`, `CARGO_HOME`, `GOPATH`, `POETRY_*`) that shell initialization (`~/.bashrc` / `~/.profile`) sets up — AND provide it to every agentic subprocess through `agentic_run`, so toolchains activated by shell init (pyenv, rbenv, poetry, nvm) are usable, not merely present on disk. Capture SHALL be best-effort (dumping a login shell's environment) AND SHALL degrade gracefully: a partial or empty capture still proceeds with the base environment rather than failing the run.

The captured environment SHALL be **credential-filtered**: it propagates `PATH` and toolchain-activation variables but SHALL NOT propagate variables matching credential patterns — names containing `TOKEN`, `SECRET`, `KEY`, or `PASSWORD`, or known provider prefixes such as `AWS_` / `ANTHROPIC_` — so secrets the operator's shell exports never reach the model, including provider API keys (which as an env value would also bill the wrapped CLI off its subscription, per the key-flow requirement). The exclusion set SHALL ship with defaults AND be operator-editable. Where a captured variable conflicts with a variable the run itself sets (sandbox or strategy), the run's value SHALL take precedence.

#### Scenario: A shell-activated toolchain is runnable in the subprocess
- **WHEN** a toolchain is activated only by the operator's shell init (e.g. `pyenv` / `poetry` via `~/.bashrc`) AND the captured environment is provided to the agentic subprocess
- **THEN** the toolchain's commands resolve and run in the subprocess (the managed `python` / `poetry`), not the bare system fallback

#### Scenario: Credential variables are not propagated
- **WHEN** the operator's login-shell environment exports a credential-bearing variable (e.g. `FOO_TOKEN` or `ANTHROPIC_API_KEY`)
- **THEN** that variable is excluded from the environment provided to the agentic subprocess

#### Scenario: Run-set variables take precedence
- **WHEN** a captured variable conflicts with one the sandbox or strategy sets for the run
- **THEN** the run's value is used, not the captured one

#### Scenario: Partial capture degrades gracefully
- **WHEN** the login-shell environment capture fails or returns only a partial environment
- **THEN** the agentic run still proceeds with the base environment, without crashing or aborting

### Requirement: AntigravityStrategy implements the `agy` CLI for agentic roles
The daemon SHALL provide a third `CliStrategy` (a56), `AntigravityStrategy`, for Google's Antigravity CLI (`agy`), so a role whose model provider resolves to `antigravity` (a55's `provider → CLI` rule for the Google/Antigravity provider, OR an explicit registry `cli: antigravity`) runs agentically instead of erroring with "no registered strategy." Antigravity CLI is the successor to the sunset Gemini CLI; the strategy targets `agy`, NOT `gemini`.

`AntigravityStrategy` SHALL build an `agy` invocation that: runs single-shot command mode (`agy -p "<prompt>"`, capture); selects the model via `--model <model>` (default `gemini-3-pro`); writes an `mcp_config.json` into the workspace carrying the MCP server entry (the MCP-child `command`/`args`, AND `env` including `ORCH_MCP_ROLE`, local stdio transport); AND maps a56's sandbox (allowed-tools list + deny patterns) onto Antigravity's tool restriction so a read-only role exposes only the read tools plus the role's `submit_*` tool and denies shell/write/edit. It SHALL set Antigravity's auth env (`AV_API_KEY`), NOT any `ANTHROPIC_*` (the claude strategy's mechanism), AND SHALL write neither `.mcp.json` (claude) NOR `opencode.json` (opencode).

`AntigravityStrategy` SHALL run in capture mode; the streaming-JSON event path (`final_answer` / `session_id` / incremental log) is claude-specific (Antigravity's `--stream` emits SSE, a different format). agy therefore serves the capture-mode structured-submission roles (the advisory audits, the reviewer, the contradiction check); the executor's streaming implementer path remains on the claude strategy until the strategy-agnostic-implementer change generalizes it. The agy integration SHALL surface MCP tool calls AND surface a daemon-rejected submission to the model as a correctable tool error it can retry in the same session — the same submission contract a56 requires of the claude path.

Because the exact non-interactive tool-restriction mechanism is confirmed by the integration spike, a read-only agy role SHALL NOT rely on the tool restriction alone: the existing read-only post-hoc write enforcement (`WritePolicy::None` — a non-empty post-run `git status --porcelain` reverts via `git reset --hard HEAD` AND fails the run) applies, so any write that escapes is caught and reverted rather than corrupting the workspace. The integration spike SHALL verify the restriction holds under `agy -p`.

Registering `agy` unblocks the non-Anthropic agentic paths of the reviewer (a58) AND the contradiction check (a59) for Google models; it does NOT change any role's default transport.

#### Scenario: Antigravity provider resolves to a working strategy
- **WHEN** a role's model resolves (via a55's `provider → CLI` rule, OR an explicit `cli: antigravity`) to the `agy` CLI
- **THEN** strategy resolution returns `AntigravityStrategy` (NOT a "no registered strategy" error)
- **AND** it builds an `agy -p` invocation selecting the model via `--model <model>`

#### Scenario: MCP and role env are delivered via mcp_config.json
- **WHEN** an `agy` role runs with a structured-submission tool (e.g. `submit_review`)
- **THEN** the strategy writes `mcp_config.json` with the MCP server entry (the MCP-child `command`/`args`, AND `env` including `ORCH_MCP_ROLE`, local stdio) so the role's `submit_*` tool is reachable
- **AND** neither `.mcp.json` NOR `opencode.json` is written for that run

#### Scenario: Model selection targets Antigravity auth, not Anthropic env
- **WHEN** the resolved model is a Google/Antigravity model (e.g. `gemini-3-pro`)
- **THEN** the invocation selects it via `--model <model>` AND the Antigravity auth env (`AV_API_KEY`) is set
- **AND** none of `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` is set

#### Scenario: Read-only sandbox denies write/edit/shell
- **WHEN** a read-only role (a56 sandbox: allow Read/Glob/Grep; deny Write/Edit/Bash) runs under agy
- **THEN** the generated Antigravity tool restriction exposes only the read tools plus the role's `submit_*` tool
- **AND** it denies shell, write, AND edit tools

#### Scenario: A write that escapes the restriction is caught by the post-hoc revert
- **WHEN** a read-only agy role nonetheless produces a non-empty post-run `git status --porcelain` (the non-interactive policy gap the spike probes)
- **THEN** the `WritePolicy::None` enforcement reverts the workspace via `git reset --hard HEAD` AND fails the run
- **AND** the escaped write does NOT persist into the workspace

#### Scenario: Capture mode only; streaming stays on claude
- **WHEN** an `agy` role runs through `agentic_run`
- **THEN** it uses capture mode (stdout/stderr read at exit), NOT the streaming-JSON parse path
- **AND** the executor's streaming implementer path continues to use the `claude` strategy

#### Scenario: Submission contract holds under agy
- **WHEN** an `agy` role's agent calls its `submit_*` tool AND the daemon rejects the payload (schema-invalid)
- **THEN** the rejection reaches the model as a tool error it can correct AND retry within the same `agy` session
- **AND** this matches the correctable-tool-error contract a56 requires of the `claude` path

#### Scenario: Non-Anthropic agentic roles function under agy
- **WHEN** the reviewer (`reviewer.kind: agentic`) OR the contradiction check is configured with a Google/Antigravity model
- **THEN** the role runs agentically via `AntigravityStrategy`
- **AND** it no longer errors / fails open on "no registered strategy"

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

### Requirement: Agentic run surfaces a precondition-unmet failure distinct from a run failure
When an agentic run cannot start because a required precondition is unmet — the agent subprocess never spawns (e.g. no usable OS-level sandbox mechanism is available, per the sandbox-mechanism gate) — the executor SHALL surface a classifiable **precondition-unmet** failure, distinct from a substantive `Failed` outcome where the subprocess ran and then the task failed. The distinction SHALL be carried by the outcome/error **kind**, NOT by matching a substring of the message, so callers can branch on it reliably.

#### Scenario: The sandbox-mechanism gate yields a precondition-unmet failure
- **WHEN** an agentic run is attempted on a host with no usable sandbox mechanism AND the operator has not opted into unsandboxed operation
- **THEN** the executor surfaces a precondition-unmet failure (the subprocess never started)
- **AND** it is distinguishable by kind from a substantive run failure

#### Scenario: A substantive run failure is not precondition-unmet
- **WHEN** the agent subprocess starts and then fails (e.g. a non-zero exit after running)
- **THEN** the executor surfaces a substantive `Failed` outcome
- **AND** it is NOT classified as precondition-unmet

### Requirement: submit_canon_internal_contradictions MCP tool returns canon-internal contradictions
The per-execution MCP child SHALL advertise a `submit_canon_internal_contradictions` tool — built on a56's per-role submission framework — whenever `ORCH_MCP_ROLE = canon_contradiction_audit`, AND SHALL NOT advertise it for any other role. The tool's payload schema SHALL be `{ contradictions: [{ capability_a: string, requirement_a: string, capability_b: string, requirement_b: string, summary: string }] }` — each finding names BOTH conflicting canonical requirements (by capability AND title). The schema is symmetric (both sides canonical), distinguishing it from a62's `submit_canon_contradictions`, which names a change requirement against a canonical one. The tool relays through a56's `relay_submission` → `record_submission`; a schema-invalid payload is rejected AND surfaced to the agent as a correctable tool error it can retry in the same session.

Because the audit reports advisorily (an empty result is a clean canon, not a failure), a session that ends with no stored submission SHALL be consumed as an empty result rather than an error.

#### Scenario: Advertised only for the canon-contradiction-audit role
- **WHEN** the MCP child starts with `ORCH_MCP_ROLE = canon_contradiction_audit`
- **THEN** the `tools/list` response advertises `submit_canon_internal_contradictions` with the canon-internal-contradictions schema alongside the common tools
- **WHEN** the MCP child starts with any other role (`implementer`, `reviewer`, `canon_contradiction_check`, an advisory audit)
- **THEN** `submit_canon_internal_contradictions` is NOT advertised

#### Scenario: Valid submission is consumed by the caller
- **WHEN** the agent calls `submit_canon_internal_contradictions` with a schema-valid payload
- **THEN** the MCP child relays it via `record_submission` (a56)
- **AND** after the session exits the daemon `consume_submission`s the stored payload for the audit to turn into `AuditOutcome::Reported` findings

#### Scenario: Schema-invalid submission is correctable
- **WHEN** a `submit_canon_internal_contradictions` payload fails the schema (missing `requirement_b`, non-array `contradictions`)
- **THEN** `record_submission` rejects it (a56) AND the agent observes a correctable tool error it can retry in the same session

#### Scenario: Missing submission consumed as empty, not an error
- **WHEN** a `canon_contradiction_audit` session exits with no stored submission for the execution
- **THEN** `consume_submission` returns an empty result
- **AND** the tool layer does NOT raise an error — the audit reports a clean canon

### Requirement: Agentic run model resolution for audits
The agentic run primitive SHALL accept an optional `ResolvedModel` parameter when invoked for periodic audits. When a model is provided, the audit runner SHALL dynamically select the appropriate `CliStrategy` (e.g., `ClaudeStrategy`, `OpencodeStrategy`, `AntigravityStrategy`) based on the resolved model's provider using the `strategy_for_provider` function, rather than hardcoding a single strategy. The resolved model SHALL be passed to the CLI execution command, ensuring the CLI receives the appropriate `--model <provider>/<model>` flag (or equivalent) when supported by the strategy.

#### Scenario: Audit runs with a resolved OpenRouter model
- **WHEN** an audit is executed with a `ResolvedModel` where the provider is `openai_compatible`
- **THEN** the audit runner selects the `OpencodeStrategy`
- **AND** the CLI command includes the `--model openai_compatible/<model_name>` flag
- **AND** the CLI is invoked with the provider's configured API key and base URL

#### Scenario: Audit runs with a resolved Anthropic model
- **WHEN** an audit is executed with a `ResolvedModel` where the provider is `anthropic`
- **THEN** the audit runner selects the `ClaudeStrategy`
- **AND** the CLI command includes the `--model anthropic/<model_name>` flag (if applicable to the CLI)
- **AND** the CLI is invoked with the provider's configured API key

#### Scenario: Audit runs without a model (backward compatibility)
- **WHEN** an audit is executed with `None` for the model parameter
- **THEN** the audit runner defaults to the `ClaudeStrategy`
- **AND** no `--model` flag is appended to the CLI command
- **AND** the CLI uses its locally configured default model and authentication

### Requirement: OS sandbox exposes the daemon control socket to the sandboxed relay
Every agentic role relays its structured result to the daemon over the Unix-domain control socket via the per-execution MCP child, which runs INSIDE the OS-level sandbox. The OS sandbox SHALL therefore bind the daemon's control socket into the child's mount namespace, read-only, so the relay can `connect()` to it. (A read-only bind is sufficient: connecting to a Unix-domain socket is a socket operation, not a filesystem write.) The bind SHALL be applied in every mechanism — `systemd-run`, `bwrap`, AND `sandbox-exec` — AND under every filesystem policy — the executor's denylist AND the read-only roles' allowlist.

The bind SHALL be applied so that it survives the policy's masking steps: the private `/tmp` (systemd `PrivateTmp=yes` / bwrap `--tmpfs /tmp`) AND the masked home (allowlist `ProtectHome=tmpfs` / `--tmpfs <home>`). A control socket residing under `/tmp` or under a masked home SHALL remain connectable from inside the sandbox.

When no control socket is configured for the run (the relay env var is unset), no such bind SHALL be added.

This does not widen the sandbox trust boundary: the control socket is the intended, already-authorized relay channel for these roles, and the daemon validates every request it receives — exposing the socket only lets the sanctioned relay succeed.

#### Scenario: Control socket is bound under the executor (denylist) policy
- **WHEN** the executor spawns under the OS sandbox AND a control socket is configured for the run
- **THEN** the constructed sandbox invocation binds the control-socket path into the namespace read-only
- **AND** the relay's `connect()` to the socket succeeds from inside the sandbox

#### Scenario: Control socket is bound under a read-only role (allowlist) policy
- **WHEN** a read-only role (an audit or an agentic reviewer) spawns under the OS sandbox AND a control socket is configured
- **THEN** the constructed sandbox invocation binds the control-socket path into the namespace read-only, even though the home directory is masked

#### Scenario: A control socket under /tmp survives the private-tmp masking
- **WHEN** the control socket resides under `/tmp` (the runtime directory fell back to the per-uid temp location) AND the sandbox applies a private `/tmp`
- **THEN** the control-socket bind is applied AFTER the private-`/tmp` masking
- **AND** the socket remains present AND connectable inside the namespace

#### Scenario: No control socket configured adds no bind
- **WHEN** no control socket is configured for the run (the relay env var is unset)
- **THEN** the constructed sandbox invocation adds no control-socket bind

