## 1. Config schema

- [x] 1.1 Add `pub struct ExecutorSandboxConfig` to `src/config.rs` with three optional fields: `allowed_tools: Option<Vec<String>>`, `disallowed_bash_patterns: Option<Vec<String>>`, `disallowed_read_paths: Option<Vec<String>>`. Use `#[serde(default)]` and `#[serde(deny_unknown_fields)]`.
- [x] 1.2 Add `pub sandbox: Option<ExecutorSandboxConfig>` to `ExecutorConfig` with `#[serde(default)]`.
- [x] 1.3 Define default-value functions returning the safe baselines documented in proposal.md:
    - `default_allowed_tools()` → `["Read", "Write", "Edit", "Glob", "Grep", "Bash"]`
    - `default_disallowed_bash_patterns()` → the curl/wget/nc/ssh/scp/git-push/git-remote/git-fetch list
    - `default_disallowed_read_paths()` → the ssh/.claude/shadow/ssl-private list
- [x] 1.4 Define a `pub struct ResolvedSandbox` (or similar name) that holds the **resolved** (post-default-substitution) values: three plain `Vec<String>` fields. Add `pub fn resolve(cfg: Option<&ExecutorSandboxConfig>) -> Self` that produces a fully-populated `ResolvedSandbox` by applying per-field defaults.
- [x] 1.5 **Verify:** `config::tests::sandbox_absent_uses_defaults` (no block → ResolvedSandbox matches default constants); `config::tests::sandbox_partial_override_uses_defaults_per_field` (operator sets `allowed_tools` but omits the others; the omitted ones default while the specified one wins); `config::tests::sandbox_full_override` (all three set; ResolvedSandbox matches operator values exactly).

## 2. Settings file generation

- [x] 2.1 Add a private function in `executor/claude_cli.rs`:
    ```rust
    fn write_sandbox_settings(sandbox: &ResolvedSandbox) -> Result<PathBuf>
    ```
    that generates a unique-named JSON file in `std::env::temp_dir()` (use `tempfile::NamedTempFile::new()` or a similar approach with a UUID component) of shape:
    ```json
    {
      "permissions": {
        "allow": [],
        "deny": [
          "Bash(curl:*)",
          ...
          "Read(/home/*/.ssh/**)",
          ...
        ]
      }
    }
    ```
    The `disallowed_bash_patterns` entries are wrapped in `Bash(...)`; the `disallowed_read_paths` entries are wrapped in `Read(...)`.
- [x] 2.2 Add a `struct TempFileGuard(PathBuf)` with a `Drop` impl that removes the file. Use it RAII-style around the child-process lifetime.
- [x] 2.3 **Verify:** `executor::claude_cli::tests::settings_file_contents_match_resolved_sandbox` — given a `ResolvedSandbox` fixture, assert the generated JSON's `permissions.deny` array contains the expected `Bash(...)` and `Read(...)` entries.
- [x] 2.4 **Verify:** `executor::claude_cli::tests::settings_file_cleaned_up_after_run` — after a (mocked) subprocess exits, the temp file no longer exists on disk.

## 3. Wiring into spawn

- [x] 3.1 Extend `ClaudeCliExecutor` struct with a `sandbox: ResolvedSandbox` field. Update `ClaudeCliExecutor::new(command, timeout_secs)` to take a default sandbox (from `ResolvedSandbox::resolve(None)`); add `ClaudeCliExecutor::new_with_sandbox(command, timeout_secs, sandbox: ResolvedSandbox)` or pass it through.
- [x] 3.2 In `cli/run.rs`, when instantiating `ClaudeCliExecutor`, call `ResolvedSandbox::resolve(cfg.executor.sandbox.as_ref())` and pass the resolved value.
- [x] 3.3 In `run_subprocess`, generate the settings file (and TempFileGuard) before spawning. Add to the command:
    - `--settings <temp-path>`
    - `--allowedTools <comma-separated allowed_tools>`
    - `--permission-mode acceptEdits`
- [x] 3.4 **Verify:** add an integration test `executor::claude_cli::tests::spawn_includes_sandbox_flags` using a fake command (e.g. `echo` or a shell script) and asserting that the child process received the expected `--settings`, `--allowedTools`, `--permission-mode` arguments. Inspect via a wrapper script that dumps `$@` to a file, then assert the file's content.

## 4. Resume path

- [x] 4.1 Verify `Executor::resume` also calls `run_subprocess` (or an equivalent helper that generates the sandbox settings). If `resume` has its own spawn path, replicate the sandbox flag application there.
- [x] 4.2 **Verify:** `executor::claude_cli::tests::resume_applies_sandbox` — same fake-command assertion for the resume invocation.

## 5. Documentation

- [x] 5.1 README: under "AI Security & Guardrails", add a new subsection "Executor tool sandbox" describing:
    - Why it exists (LLM tool-use restrictions to limit exfiltration)
    - The default-deny list (network commands, credential paths)
    - How to widen the sandbox (full example for a project needing `pip install`)
    - Honest caveat: tool-routing sandbox is not OS-level isolation; recommend firejail/bubblewrap/systemd `ProtectHome=` for hard isolation
    - Cross-reference: the code-reviewer's LLM API call is a SEPARATE data flow not governed by this sandbox
- [x] 5.2 README: extend Configuration Reference's `executor:` table with a `sandbox` row pointing at the new subsection.
- [x] 5.3 `config.example.yaml`: add a commented `sandbox:` block under `executor:` showing the default values inline (so operators see the safe baseline at a glance), with a one-line pointer to the README.

## 6. Verification

- [x] 6.1 `cargo test` passes; test count grows by at least: 3 config + 2 settings-file + 2 spawn-flags + 1 resume = ~8 new tests.
- [x] 6.2 `cargo build --release` produces a binary that, when invoked with the default sandbox, generates a temp settings file containing the documented deny rules and passes it to claude.
- [x] 6.3 `openspec validate executor-tool-sandbox --strict` passes.
