## 1. Persist captured output to a per-change log file

- [x] 1.1 Add a private helper `write_run_log(workspace: &Path, change: &str, stdout: &str, stderr: &str) -> Result<PathBuf>` to `claude_cli.rs`. It computes the log path as `std::env::temp_dir().join("autocoder-logs").join(<workspace-basename>).join(format!("{change}.log"))`, creates parents on demand, and writes `=== STDOUT (n bytes) ===\n{stdout}\n=== STDERR (m bytes) ===\n{stderr}\n`. Returns the path on success.
- [x] 1.2 Call `write_run_log` from `ClaudeCliExecutor::run` after `run_subprocess` returns but before `classify_outcome`. Log the resulting path at INFO. If `write_run_log` errors, log at WARN and continue — the run outcome is not affected by the log-file write.
- [x] 1.3 Apply the same call in the resume path (`ClaudeCliExecutor::resume`) so resumed iterations are also captured.

## 2. Inline tail on suspicious outcomes

- [x] 2.1 In `classify_outcome`, add a branch: after the existing exit-0 + clean-workspace + no-layer-1-marker + no-layer-2-match checks, before returning `Ok(ExecutorOutcome::Completed)`, log a WARN message including:
    - the change name
    - the trailing ~2KB of stdout (use `&s[s.len().saturating_sub(2048)..]`; if `s` is empty say `(empty)`)
    - the trailing ~2KB of stderr (same)
    - the log-file path computed from the same scheme used in §1
- [x] 2.2 Helper: `fn tail(s: &str, max: usize) -> &str` that returns the last `max` bytes (snapping to a UTF-8 char boundary so the log doesn't break).

## 3. Tests

- [x] 3.1 `claude_cli::tests::run_log_is_written_with_expected_format` — fixture subprocess (shell script) that writes "hello-out" to stdout and "hello-err" to stderr and exits 0. After `run`, read the log file from the computed path and assert it contains both delimiters and both strings.
- [x] 3.2 `claude_cli::tests::run_log_path_is_under_workspace_basename` — verify the path layout is `<temp>/autocoder-logs/<basename>/<change>.log` for a fixture workspace whose basename is unique.
- [x] 3.3 `claude_cli::tests::tail_snaps_to_char_boundary` — unit test for the helper with a multi-byte UTF-8 string where the naive byte slice would split a codepoint.
- [x] 3.4 **Verify:** run `cargo test --quiet` — net new tests = at least 3, full suite passes.

## 4. Verification

- [x] 4.1 `openspec validate executor-output-visibility --strict` passes.
