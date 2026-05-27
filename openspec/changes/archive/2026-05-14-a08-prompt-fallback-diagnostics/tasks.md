## 1. WARN logging on each build_prompt fallback path

- [x] 1.1 Restructure `claude_cli::build_prompt` to inspect the result of `Command::new("openspec")...output()` explicitly:
    - On `Err(e)` where `e.kind() == ErrorKind::NotFound` → log WARN with `reason="openspec_not_found"`, fall through to fallback.
    - On `Err(e)` other → log WARN with `reason="openspec_spawn_error"` and `error=%e`, fall through.
    - On `Ok(out)` where `!out.status.success()` → log WARN with `reason="openspec_exited_nonzero"`, `code=<exit code>`, and `stderr_tail=<first 200 chars of stderr>`, fall through.
    - On `Ok(out)` where `s.trim().is_empty()` → log WARN with `reason="openspec_empty_stdout"`, fall through.
    - On `Ok(out)` with non-empty stdout → return as before.
- [x] 1.2 Each WARN must include the `change` field so multi-repo logs are unambiguous.

## 2. Persist the prompt to the run log

- [x] 2.1 Change `persist_run_log` signature to accept the prompt: `fn persist_run_log(workspace: &Path, change: &str, prompt: &str, outcome: &SubprocessOutcome)`.
- [x] 2.2 Update the format to: `=== PROMPT (n bytes) ===\n{prompt}\n=== STDOUT (n bytes) ===\n{stdout}\n=== STDERR (m bytes) ===\n{stderr}\n`.
- [x] 2.3 Update both call sites (`run`, `resume`) to pass `&prompt` through.

## 3. README: document PATH requirement in systemd unit

- [x] 3.1 In the Deployment §3 (systemd unit) snippet, add an explicit `Environment="PATH=/usr/local/bin:/usr/bin:/bin"` line and a brief note: "PATH must include the directories containing `claude` and `openspec` — both are invoked by name. `npm install -g @fission-ai/openspec` (NodeSource Node) typically places the binary at `/usr/bin/openspec`; a manual install may land at `/usr/local/bin/openspec`. `which openspec claude` as the deploy user is the authoritative check."

## 4. Tests

- [x] 4.1 `build_prompt_logs_warn_when_openspec_missing` — use `tracing_test::traced_test` (or capture via subscriber) with PATH temporarily cleared so `openspec` cannot be spawned; assert the WARN containing `openspec_not_found` was emitted AND a non-empty prompt was still returned (fallback works).
- [x] 4.2 `build_prompt_logs_warn_when_openspec_exits_nonzero` — write a fake `openspec` shell script that exits 1 with stderr text, put it first on PATH; assert WARN with `openspec_exited_nonzero` is emitted.
- [x] 4.3 `run_log_contains_prompt_section` — fixture executor run; read the persisted log; assert it contains `=== PROMPT (` AND the prompt text observed by `build_prompt`.
- [x] 4.4 **Verify:** `cargo test` passes; net new tests = at least 3.

## 5. Verification

- [x] 5.1 `openspec validate prompt-fallback-diagnostics --strict` passes.
