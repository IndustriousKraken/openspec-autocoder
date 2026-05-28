## Why

The per-change log file currently bundles four sections into one file: `PROMPT`, `ACTIONS`, `FINAL ANSWER`, `STDERR`. The `ACTIONS` section dominates: it captures every tool_use, tool_result (with byte counts AND truncated content), AND intermediate assistant text the wrapped CLI emits across the run. On non-trivial changes this is dozens of kilobytes; on autocoder's own self-implementation iterations it routinely exceeds 100 KB.

Three problems with the current bundling:

1. **Self-signal confusion.** Any autocoder code that scans the captured event stream for daemon-meaningful markers is vulnerable to false-positive matches against content the agent received as tool-result data. The `a20a1` perma-stuck incident is the canonical example: the `SpecNeedsRevision` sentinel scanner false-matched on a prompt-template echo in stdout because `final_answer` was empty on timeout AND the scanner fell back to scrolling through the entire stream. `a20a1` fixes that specific scanner, but the same class of bug recurs whenever a future autocoder feature scans event-stream content for any daemon-meaningful pattern. Architectural isolation prevents the recurrence without each consumer having to defend independently.

2. **Operator-readable log is too long.** Operators reading the per-change log to understand what happened (or to triage a failure) wade through hundreds of lines of `[tool_use] Read <path>` / `[tool_result] (N bytes returned)` / intermediate assistant text before reaching the FINAL ANSWER section. The signal-to-noise ratio for casual operator inspection is poor. Tooling that grep / tail / less the file pays the same length cost.

3. **Operator-controllable bytes flow through naive consumers.** The ACTIONS stream contains output from arbitrary `Bash` invocations — including ANSI escape sequences, cursor-movement codes, clear-screen sequences, AND any other terminal-control bytes that bash commands produce. Operators tailing the log via `tail -f` see their terminal disturbed when those bytes flow through. Operators piping the log through `awk` / `sed` / `jq` / `grep` with unusual delimiters can see parsing disrupted. Each downstream consumer should sanitize, but defense-in-depth says isolate at the source: keep the high-volume agent-controllable stream out of the file operators routinely scan.

The fix isolates ACTIONS into its own file. The operator-facing log keeps PROMPT, FINAL ANSWER, AND STDERR — small, scannable, agent-emission-free except for the FINAL ANSWER which is bounded AND already-scoped agent content. The verbose stream lives in a sibling file consulted only when diagnosing.

## What Changes

**Split the per-change log into two files.** Same directory; sibling paths:

- **Summary log** (operator-facing): `<logs_dir>/runs/<workspace-basename>/<change>.log` — keeps the existing path so operator tooling, journalctl-style grep patterns, AND documentation references continue to work. Contains `PROMPT`, `FINAL ANSWER`, AND `STDERR` sections in that order. The `ACTIONS` section header is replaced with a single one-line pointer: `=== ACTIONS (see <change>.stream.log) ===`.
- **Stream log** (diagnostic): `<logs_dir>/runs/<workspace-basename>/<change>.stream.log` — new file. Contains the full `ACTIONS` content as today's format (`[tool_use] ...`, `[tool_result] (N bytes returned)`, `[assistant] ...`, `[raw] ...`, `[unknown:<type>] ...`). No section headers — the file is one continuous action stream.

Both files share the same retention policy AND lifecycle: the existing retention pass deletes both atomically when the corresponding change directory is gone AND mtimes exceed `executor.log_retention_days`. Active-change logs are preserved regardless of age, same as today.

**Daemon-internal consumers of the event stream SHALL NOT read the stream file by default.** The PR-comment composer reads the summary log's FINAL ANSWER section (per the canonical "PR-comment Agent implementation notes body uses the FINAL ANSWER" requirement) — unchanged. The sentinel scanner reads `outcome.final_answer` per `a20a1`'s narrowing — unchanged. No daemon code path SHALL grep, scan, OR pattern-match the stream file for daemon-meaningful markers UNLESS that consumer is explicitly diagnostic-only AND documented to handle agent-controllable content carefully.

**`StructuredLogWriter` becomes a dual-file writer.** The existing component that builds the per-change log from the JSON event stream SHALL split its dispatch: PROMPT events, FINAL ANSWER text, AND STDERR bytes write to the summary log; tool_use / tool_result / intermediate assistant text events write to the stream log. The split happens at event-classification time; no buffering of the full stream in memory is required.

**Backward compatibility.** Operators with tooling that grep / tail / parse the existing single-file log have one transition cost: tools that grep for `[tool_use]` / `[tool_result]` / `[assistant]` patterns SHALL be redirected to the `.stream.log` file. The summary log no longer contains those patterns (only the pointer line). Documentation guides operators through the migration.

**Docs update.** `docs/OPERATIONS.md`'s "Per-change run log shape" section (reorganized into the "Internals & debugging" group by the recent doc-reorg work) is updated to describe the two-file layout, the pointer line, AND when to consult each file. Sample CLI snippets for `tail -f` / grep usage are updated to point at the correct file per workflow.

## Impact

- **Affected specs:**
  - `executor` — MODIFIED requirement: `Executor invokes Claude CLI in JSON event streaming mode and captures events to a structured log`. The existing 4 canonical scenarios are preserved (with content updated to reflect the new file structure where they reference section locations); 2 new scenarios cover the stream-file path AND the summary-log pointer line.
  - `executor` — MODIFIED requirement: `Per-change log files are pruned after executor.log_retention_days days, preserving active-change logs`. The existing scenarios are preserved (with content updated to reference both files atomically); 1 new scenario covers atomic-pair retention.
- **Affected code:**
  - `autocoder/src/executor/claude_cli.rs` (OR wherever `StructuredLogWriter` lives) — dual-file writer logic; route events at classification time; both files created lazily on first relevant event.
  - `autocoder/src/executor/log_retention.rs` (OR wherever the retention pass lives) — when deleting `<change>.log`, also delete `<change>.stream.log` if present. When preserving an active-change log, preserve both.
  - `docs/OPERATIONS.md` — update the "Per-change run log shape" section per the docs description above.
- **Operator-visible behavior:**
  - The path `<logs_dir>/runs/<basename>/<change>.log` continues to exist. Its content is shorter AND signal-dense; the ACTIONS section is replaced with the pointer line.
  - New path `<logs_dir>/runs/<basename>/<change>.stream.log` appears alongside, containing the verbose action stream.
  - Operators who used to `tail -f <change>.log` to watch the agent work see only the summary stream now; they switch to `tail -f <change>.stream.log` for the live action view.
  - Operators who used to `grep '[tool_use]' <change>.log` redirect to `.stream.log`.
- **Breaking:** no for the daemon (the canonical PR-comment consumer reads FINAL ANSWER which stays in the summary log). Operator tooling that greps the unified log for `[tool_use]` patterns DOES need to update the file path — the migration is documented AND mechanical (`.log` → `.stream.log` for action-pattern greps).
- **Acceptance:** `cargo test` passes (existing log-writer tests + new tests for the dual-file split AND retention-pair atomicity); `openspec validate a20a2-split-actions-stream-log --strict` passes; `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
