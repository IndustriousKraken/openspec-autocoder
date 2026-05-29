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

### Requirement: Sentinel emission instructions in the implementer prompt include a concrete worked example AND a self-check hint
Every outcome-sentinel format documented in `prompts/implementer.md` (currently the `SpecNeedsRevision` sentinel; future formats SHALL follow the same pattern) SHALL be presented with three structural elements:

1. **A substitution instruction** appearing IMMEDIATELY BEFORE the example, naming the rule that the example is a pattern AND that emitting it verbatim is a parse failure.
2. **A worked example with no angle-bracket placeholders** showing what a complete, parseable sentinel looks like. The example SHALL deserialize cleanly into the corresponding Rust type via `serde_json::from_str` AND SHALL contain realistic task ids, prose, AND reasoning that the agent can model.
3. **A self-check hint** appearing AFTER the example, instructing the agent to scan its emitted sentinel for `<...>` patterns inside string values before emitting AND describing the daemon's placeholder-detection diagnostic.

The implementer prompt SHALL NOT use angle-bracket placeholders (`<id-from-tasks-md>`, `<verbatim quote>`, etc.) inside string values in any sentinel example. Earlier versions of the prompt used this pattern AND triggered literal-emission failures; the lesson is preserved as a hard rule.

Operator-customizable override prompts (loaded via the uniform `PromptLoader` per `a24`'s spec) MAY use any structure the operator prefers — the canonical rule binds the bundled default only. Operators whose customized templates regress to placeholder-style examples will hit the same failure mode the bundled prompt previously hit; the placeholder-detection requirement in `orchestrator-cli` surfaces the diagnostic AND points the operator at the bundled default for reference.

#### Scenario: Bundled prompt's sentinel example is parseable
- **WHEN** an automated test deserializes the worked-example JSON from `prompts/implementer.md`'s sentinel section into `SpecNeedsRevisionDetail`
- **THEN** the deserialization succeeds without error
- **AND** every field's value is a concrete string (no angle-bracket markers, no template variables)

#### Scenario: Bundled prompt contains the three structural elements
- **WHEN** a maintainer reads `prompts/implementer.md`'s sentinel section
- **THEN** the section contains a substitution instruction paragraph IMMEDIATELY BEFORE the example
- **AND** the example itself contains no angle-bracket placeholders inside string values
- **AND** a self-check hint paragraph appears AFTER the example naming the daemon's placeholder-detection diagnostic

#### Scenario: Future sentinel formats follow the same pattern
- **WHEN** a future change introduces a new sentinel format in `prompts/implementer.md` (OR a new operator-aimed prompt template added by the daemon)
- **THEN** the new format's documentation in the prompt follows the substitution-instruction + worked-example + self-check-hint structure
- **AND** the new format's example deserializes cleanly into its corresponding Rust type

### Requirement: Timeout classification takes precedence over sentinel extraction; sentinel scan is scoped to deliberate-emission content
The executor's outcome-dispatch path SHALL check `outcome.timed_out` BEFORE attempting any sentinel extraction OR sentinel-parse fallback. When `outcome.timed_out` is `true`, the executor SHALL return `Failed { reason: "timeout" }` (OR the canonical timeout-reason format) WITHOUT scanning for, extracting, OR attempting to parse any sentinel-shaped substring in the captured event stream. The sentinel is by definition a deliberate end-of-run emission; a timed-out run did not reach end-of-run, so no sentinel-shaped scrollback content is semantically the agent's emission.

When the run did NOT time out AND a sentinel scan is performed, the scan's input scope depends on the configured output format:

- **JSON streaming mode** (`executor.output_format: json`, the default): the scanner reads ONLY `outcome.final_answer`. When `final_answer` is `None` (the agent never reached the `result` event for any reason — crash, protocol error, etc.), the sentinel scan returns `None` AND the normal exit-status path handles the outcome. The scanner SHALL NOT fall back to `outcome.stdout`. Rationale: the `result` event's text is the agent's deliberate end-of-run emission; tool-result echoes, prompt-context echoes, AND other event-stream content are NOT deliberate emissions AND must not be matched against the sentinel.
- **Text mode** (`executor.output_format: text`, the legacy opt-out): the scanner reads `outcome.stdout`. This mode has no separate `result`-event channel, so stdout IS the agent's emission stream. Timeout precedence still applies — a timed-out text-mode run is classified as timeout BEFORE the sentinel scan runs.

This requirement narrows the canonical "Malformed outcome sentinel falls back to Failed" scenario WITHOUT changing it: a malformed sentinel that genuinely appears in the agent's deliberate emission still triggers the canonical fallback. The change is what counts as "the agent's deliberate emission" — sentinel-shaped substrings in tool-result echoes OR prompt-context echoes are no longer in scope.

#### Scenario: Timed-out run with sentinel-shaped scrollback returns timeout
- **WHEN** the executor invocation completes with `outcome.timed_out: true` AND `outcome.stdout` contains a well-formed `=== AUTOCODER-OUTCOME ===` block followed by valid JSON (the worst-case false-match: sentinel content present, would-be-parseable)
- **THEN** the executor returns `Failed { reason: "timeout" }`
- **AND** no sentinel-extraction attempt is made
- **AND** no `agent emitted unparseable SpecNeedsRevision sentinel` log line fires
- **AND** the perma-stuck counter increments against a transient-infrastructure category (the canonical "predictable failure" set) if the operator has configured that classification, NOT against a genuine agent failure

#### Scenario: Timed-out run with prompt-template echo in stdout returns timeout
- **WHEN** the executor invocation completes with `outcome.timed_out: true`, `outcome.final_answer: None`, AND `outcome.stdout` contains a tool-result echo of `prompts/implementer.md` (including the sentinel example block with `\n31\t`-style line-number prefixes)
- **THEN** the executor returns `Failed { reason: "timeout" }`
- **AND** the line-number-prefixed pseudo-sentinel content is NOT parsed
- **AND** no misleading `unparseable sentinel` reason is surfaced to the operator

#### Scenario: JSON streaming mode scans only final_answer
- **WHEN** the executor invocation completes with `output_format: Json`, `outcome.timed_out: false`, `outcome.final_answer: Some("Implementation complete; all tests pass.")` (no sentinel), AND `outcome.stdout` contains a sentinel-shaped block from a tool-result echo
- **THEN** the sentinel scanner reads ONLY `final_answer`
- **AND** the scan returns `None`
- **AND** the executor proceeds to the normal exit-status path
- **AND** the stdout echo's sentinel-shaped content is ignored

#### Scenario: JSON streaming mode with sentinel in final_answer parses correctly
- **WHEN** `output_format: Json`, `outcome.timed_out: false`, AND `outcome.final_answer: Some("=== AUTOCODER-OUTCOME ===\n{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[...],...}")`
- **THEN** the sentinel scanner extracts the payload from `final_answer` AND parses it
- **AND** a well-formed payload returns `SpecNeedsRevision { ... }` per the canonical outcome
- **AND** a malformed payload triggers the canonical "Malformed outcome sentinel falls back to Failed" path

#### Scenario: Text mode preserves stdout scan for non-timeout runs
- **WHEN** `output_format: Text`, `outcome.timed_out: false`, AND `outcome.stdout` contains a sentinel block
- **THEN** the sentinel scanner reads `outcome.stdout` AND extracts the block
- **AND** the existing parse + dispatch behaviour is unchanged from pre-spec text-mode behaviour
- **AND** text mode's stdout-as-emission semantic is preserved

#### Scenario: JSON streaming mode with final_answer absent skips the sentinel scan
- **WHEN** `output_format: Json`, `outcome.timed_out: false` (run completed normally per exit status), AND `outcome.final_answer: None` (no `result` event was captured for some non-timeout reason — protocol error, missing event type, etc.)
- **THEN** the sentinel scan returns `None` without consulting `outcome.stdout`
- **AND** the executor proceeds to the normal exit-status path (which may classify as Failed for other reasons)
- **AND** stdout echo content is not considered for sentinel matching even when final_answer is unexpectedly empty

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

1. **Tool-recorded outcome lookup** (NEW). The classifier sends a `consume_outcome` action keyed by `(workspace_basename, change)`. When the daemon returns a recorded outcome:
   - A `Success` variant maps to `ExecutorOutcome::Completed { final_answer }` using the recorded `final_answer`.
   - A `SpecNeedsRevision` variant maps to the existing `ExecutorOutcome::SpecNeedsRevision { ... }` shape.
   - The classifier returns the mapped outcome immediately. No further heuristic is applied.
2. **AskUser marker check** (UNCHANGED from today's behavior; only the ordering shifts — it now runs only when no outcome was tool-recorded).
3. **Timeout precedence** (UNCHANGED — the existing canonical "Timeout classification takes precedence over sentinel extraction" requirement governs this layer AND its scope-narrowing remains in force).
4. **Stdout-sentinel scan** (UNCHANGED in extraction behavior; gains an operator-visible deprecation warning per the requirement below).
5. **Exit-status path** (UNCHANGED).
6. **Layer-2 stdout heuristic + Completed fallback** (UNCHANGED).

The precedence rule is anchored in the semantics of the signal: a tool-recorded outcome is the agent's deliberate, schema-validated end-of-run emission. It is more authoritative than ANY inferred state (timeout flag, exit status, stdout content). A run that called an outcome tool AND subsequently timed out is classified by the outcome, not the timeout — the agent emitted its signal; the kill happened after.

When the daemon's `consume_outcome` action returns `None` (no outcome was recorded), the classifier proceeds to step 2 AND the existing behavior is preserved exactly. This is the path that all pre-a27a0 implementer prompts continue to take.

#### Scenario: Tool-recorded `Success` outcome takes precedence over stdout sentinel
- **WHEN** the classifier runs for a change whose daemon outcome store contains a `Success` outcome from a prior `outcome_success` tool call
- **AND** `outcome.stdout` ALSO contains a well-formed `=== AUTOCODER-OUTCOME ===` block with a `spec_needs_revision` payload (the worst-case ambiguity: both signals present)
- **THEN** the classifier returns `ExecutorOutcome::Completed { final_answer: <recorded final_answer> }`
- **AND** the stdout sentinel is NOT extracted, parsed, OR considered for the outcome
- **AND** the daemon's outcome store entry for this `(workspace_basename, change)` is cleared (drained by `consume_outcome`)

#### Scenario: Tool-recorded `SpecNeedsRevision` outcome takes precedence over timeout
- **WHEN** the classifier runs for a change whose daemon outcome store contains a `SpecNeedsRevision` outcome (the agent called `outcome_spec_needs_revision` AND then was killed by the wall-clock timeout before clean exit)
- **AND** `outcome.timed_out` is `true`
- **THEN** the classifier returns `ExecutorOutcome::SpecNeedsRevision { ... }` populated from the recorded payload
- **AND** the timeout flag is NOT used
- **AND** no `Failed { reason: "timeout" }` outcome is produced

#### Scenario: Absent tool-recorded outcome falls through to legacy classifier
- **WHEN** the classifier runs for a change whose daemon outcome store contains no entry (the agent did not call any outcome tool)
- **AND** `outcome.stdout` contains a well-formed `=== AUTOCODER-OUTCOME ===` block with a valid `spec_needs_revision` payload
- **AND** `outcome.timed_out` is `false`
- **THEN** the classifier's `consume_outcome` call returns `None`
- **AND** the classifier proceeds through the existing ordering (AskUser → timeout → stdout sentinel → exit)
- **AND** the stdout sentinel scan extracts the payload AND returns `ExecutorOutcome::SpecNeedsRevision { ... }` (the legacy path's exact behavior)
- **AND** the legacy-path deprecation warning is emitted per the requirement below

### Requirement: Legacy stdout-sentinel scan is deprecated; matches emit an operator-visible warning during the transition cycle

The stdout-sentinel scan (the `extract_outcome_sentinel` + `try_parse_spec_needs_revision` pair invoked from `classify_outcome`) SHALL remain functionally unchanged in this change for backward compatibility with the previous-cycle implementer prompt. When the scan actually matches AND returns a parsed `SpecNeedsRevision` outcome (the legacy path is taken because `consume_outcome` returned `None`), the classifier SHALL emit a `tracing::warn!` log line containing:

- The phrase `legacy stdout sentinel matched` (operator-greppable canonical marker).
- The change name.
- A directive sentence naming the canonical replacement tool: `please call the outcome_spec_needs_revision MCP tool instead`.
- The planned removal target: `(stdout sentinel parsing is scheduled for removal in a27a2)`.

The warning IS load-bearing: it produces operator-visible signal that an out-of-date implementer prompt is in use, which is the trigger for closing the deprecation window in a27a2. Operators MAY filter the warning by changing the log level if they accept the legacy behavior; the warning's continued emission for the cycle's duration is the intended operator-feedback channel.

The stdout-sentinel scan's extraction logic, JSON parsing logic, placeholder-detection logic, AND the parse-failure fallback to `Failed { reason: "..." }` are ALL unchanged. The only behavioral delta is the additional warning emission on successful match.

The deprecation is REMOVED in `a27a2`, at which point the stdout-sentinel scan's match path returns the same outcome but the warning is replaced by a hard error (the legacy path becomes unreachable; the scan's continued presence is dead code at that point, removed as a separate task in a27a2).

#### Scenario: Legacy stdout-sentinel match emits the deprecation warning
- **WHEN** the classifier's `consume_outcome` returns `None`
- **AND** `outcome.timed_out` is `false`
- **AND** the stdout-sentinel scan extracts a payload AND parses it successfully as `spec_needs_revision`
- **THEN** the classifier emits a `tracing::warn!` log line containing the phrase `legacy stdout sentinel matched`, the change name, the `please call the outcome_spec_needs_revision MCP tool instead` directive, AND the planned-removal-target note
- **AND** the returned outcome is `ExecutorOutcome::SpecNeedsRevision { ... }` (the legacy path's exact result)

#### Scenario: Legacy stdout-sentinel parse failure surfaces unchanged
- **WHEN** the classifier's `consume_outcome` returns `None`
- **AND** `outcome.timed_out` is `false`
- **AND** the stdout-sentinel scan extracts a payload BUT parsing fails (malformed JSON, placeholder-shaped strings, etc.)
- **THEN** the classifier returns `ExecutorOutcome::Failed { reason: "agent emitted unparseable SpecNeedsRevision sentinel: ..." }` (the existing behavior, verbatim)
- **AND** no deprecation warning is emitted (the warning is scoped to successful matches; a parse failure is its own failure mode)

### Requirement: Implementer prompt documents the outcome tools by name AND uses them as the canonical end-of-run signal

The bundled `prompts/implementer.md` template SHALL contain an "Outcome tools" section that:

- Names both outcome tools: `outcome_success` AND `outcome_spec_needs_revision`.
- Provides a one-line purpose statement for each tool.
- Directs the agent to call `outcome_success` (with the agent's end-of-run summary as `final_answer`) at the end of a successful implementation run, BEFORE exiting.
- Directs the agent to call `outcome_spec_needs_revision` (instead of emitting the `=== AUTOCODER-OUTCOME ===` stdout block) for the pre-flight unimplementable-task case.
- Notes that input-validation errors from the MCP tool are recoverable: the model receives the error as the tool-call result AND can retry the call with corrected fields in the same session.

The section SHALL NOT inline the full input schemas; the MCP `tools/list` response is the canonical schema source AND duplicating it in the prompt creates a maintenance hazard. Tool names + one-line purposes are sufficient: a model that knows the tool exists AND its purpose can attempt the call AND converge via tool-error feedback if its argument shape is wrong.

The pre-flight unimplementable-task section SHALL be rewritten to use `outcome_spec_needs_revision`. The substitution-instruction + worked-example + self-check-hint structure (per the existing canonical "Sentinel emission instructions in the implementer prompt include a concrete worked example AND a self-check hint" requirement) SHALL be preserved, but the worked example becomes a tool-call shape (a JSON object the agent passes to the tool) rather than a stdout block, AND the self-check hint references the MCP layer's input validation instead of the daemon's post-exit placeholder detection.

The existing `=== AUTOCODER-OUTCOME ===` stdout-sentinel section SHALL be retained for the deprecation cycle, prefixed with a "DEPRECATED" note that names `outcome_spec_needs_revision` as the canonical replacement AND `a27a2` as the planned removal target.

Operator-customizable override prompts (loaded via the uniform `PromptLoader` per `a24`'s spec) MAY use any structure the operator prefers — the canonical rule binds the bundled default only.

#### Scenario: Bundled prompt names both outcome tools
- **WHEN** a maintainer reads `prompts/implementer.md`
- **THEN** the prompt contains an "Outcome tools" section
- **AND** the section names both `outcome_success` AND `outcome_spec_needs_revision`
- **AND** each tool has a one-line purpose statement
- **AND** the section directs end-of-run `outcome_success` use AND pre-flight `outcome_spec_needs_revision` use

#### Scenario: Bundled prompt's outcome-tool example deserializes cleanly
- **WHEN** an automated test extracts any JSON-shaped example from the prompt's outcome-tool sections AND deserializes it into the corresponding tool-argument Rust type
- **THEN** the deserialization succeeds without error
- **AND** every string field contains a concrete value (no angle-bracket markers, no template variables)

#### Scenario: Existing stdout-sentinel section retained with DEPRECATED note
- **WHEN** a maintainer reads `prompts/implementer.md`
- **THEN** the existing `=== AUTOCODER-OUTCOME ===` sentinel section is still present (for the deprecation cycle)
- **AND** the section is prefixed with a "DEPRECATED" note naming `outcome_spec_needs_revision` as the canonical replacement
- **AND** the note names `a27a2` as the planned-removal target for the legacy path

