## MODIFIED Requirements

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
