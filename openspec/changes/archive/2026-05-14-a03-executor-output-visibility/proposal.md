## Why

When the daemon classifies an executor run as Completed-with-no-diff (now Failed by `no-op-completion-is-failure`), the operator has no way to tell *why* Claude exited 0 without doing anything. The CLI's stdout and stderr are captured into Strings inside `run_subprocess`, used only for narrow purposes (Layer-2 AskUser heuristic, first-200-chars of stderr on non-zero exit), and then dropped on the floor.

Production symptom: two consecutive Failed-no-diff outcomes in the same pass for unrelated changes, indicating a systemic problem (likely sandbox config, missing openspec install, or environment-related). The operator cannot diagnose without re-running by hand or attaching a tracer.

## What Changes

- **MODIFIED capability:** `executor` — the `ClaudeCliExecutor` SHALL persist every subprocess invocation's stdout and stderr to a per-change log file outside the workspace, and SHALL log a tail of both streams at WARN when the run produced no working-tree changes despite exit 0.
- **Code:**
  - `claude_cli::run_subprocess` (or its caller) writes the captured stdout/stderr to `<system-temp>/autocoder-logs/<workspace-basename>/<change>.log` after the child exits, regardless of outcome. The file is overwritten on each run (last-run-wins). Format is plain text: `=== STDOUT (n bytes) ===\n<stdout>\n=== STDERR (m bytes) ===\n<stderr>\n`. The log file path is recorded once at INFO so journalctl shows where to look.
  - `classify_outcome` (the empty-workspace + exit-0 branch, after AskUser layer-1/2 checks) emits a WARN with the last ~2KB of stdout and ~2KB of stderr inline, so the immediate journalctl trail surfaces *what Claude said* without requiring file access.
- **Tests:**
  - Unit test: a fixture subprocess (shell script) that writes known text to stdout and stderr; assert the persisted log file contains both, with the expected delimiters.
  - Unit test: same fixture but exit 0 with no diff in the workspace; assert the WARN log line contains the stdout tail (use `tracing_test` or capture via subscriber).

## Impact

- Affected specs: `executor` (one scenario added).
- Affected code: `autocoder/src/executor/claude_cli.rs` (subprocess output handling).
- New filesystem writes outside the workspace: `<temp>/autocoder-logs/<workspace-basename>/<change>.log`. These accumulate across changes but stay bounded per-change (one file per active change, overwritten each run). No rotation, no opt-out.
- Privacy/security: the log file contains whatever Claude wrote to stdout/stderr, which can include source-code fragments from the workspace. Living under the system temp dir means it inherits temp's standard permissions; on multi-user hosts an operator should ensure the autocoder user's umask is sane. Not a regression — the same data is already in journalctl when exit was non-zero.
