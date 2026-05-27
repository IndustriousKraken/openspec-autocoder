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
When `executor.output_format` is `"json"` (the default), the executor SHALL invoke the wrapped Claude CLI with the `--output-format stream-json` argument (or whatever flag name Claude CLI's current release uses for line-delimited JSON event output). The executor SHALL spawn a streaming reader task that reads stdout line-by-line, parses each line as a JSON event, AND dispatches the parsed event to a `StructuredLogWriter` that builds the per-change log file with separate PROMPT / ACTIONS / FINAL ANSWER / STDERR sections. The streaming approach guarantees that on timeout-kill, the log file already contains every event the child emitted before the kill.

#### Scenario: Successful JSON run produces structured log
- **WHEN** Claude CLI is invoked with JSON streaming mode AND the run completes successfully
- **THEN** the per-change log file contains four sections in order: `=== PROMPT (<n> bytes) ===`, `=== ACTIONS ===`, `=== FINAL ANSWER (<n> bytes) ===`, `=== STDERR (<n> bytes) ===`
- **AND** the ACTIONS section contains formatted lines for each tool_use, tool_result, and intermediate assistant text block in the run
- **AND** the FINAL ANSWER section contains the text from the `result` event that closes the run

#### Scenario: Timeout-killed run preserves the ACTIONS up to the kill
- **WHEN** Claude CLI emits N events on stdout AND autocoder's timeout enforcement kills the child before the `result` event arrives
- **THEN** the log file's ACTIONS section contains the N events that arrived
- **AND** the FINAL ANSWER section is empty (the `result` event never arrived to populate it)
- **AND** the log file is structurally complete (all section headers present; size annotations updated)

#### Scenario: Malformed JSON line lands in ACTIONS as raw
- **WHEN** the stdout reader receives a line that fails JSON parsing
- **THEN** the line is appended to the ACTIONS section as `[raw] <line content>`
- **AND** a WARN log is emitted naming the malformed line
- **AND** subsequent lines continue to be parsed normally

#### Scenario: Unknown event type lands in ACTIONS as unknown
- **WHEN** the stdout reader receives a JSON event whose `type` field doesn't match a known variant
- **THEN** the event is appended to the ACTIONS section as `[unknown:<type>] <raw json>`
- **AND** subsequent events continue to be processed normally

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
At daemon startup AND once every 24 hours during operation, the daemon SHALL run a retention pass over the per-change log directory. A log file SHALL be eligible for deletion when its modification time is older than `now - log_retention_days * 86400` seconds AND its corresponding change directory at `<workspace>/openspec/changes/<change>/` does NOT exist (the change has been archived OR removed). Logs for changes that are STILL active SHALL be preserved regardless of age. The default `log_retention_days` value is `30`; operator-configurable; clamped at `365`.

#### Scenario: Stale log for archived change is deleted
- **WHEN** the retention pass runs AND a log file `<change>.log` has mtime more than `log_retention_days` days ago AND no `openspec/changes/<change>/` directory exists for it
- **THEN** the log file is deleted
- **AND** the retention report's `files_deleted` count includes it

#### Scenario: Old log for active change is preserved
- **WHEN** a log file is older than the retention window AND its change directory still exists in the active path
- **THEN** the log file is NOT deleted
- **AND** the retention report's `files_preserved` count includes it

#### Scenario: Recent log is preserved regardless of change state
- **WHEN** a log file's mtime is within the retention window
- **THEN** the log file is NOT deleted regardless of whether the change is active or archived

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

