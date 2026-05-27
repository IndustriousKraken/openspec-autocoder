## Context

Claude Code supports two layers of tool-use control on the command line:

1. **Tool allowlist/denylist:** `--allowedTools <list>` and
   `--disallowedTools <list>` flags filter the tools the model can
   call by name (e.g. `Bash`, `WebFetch`, `Read`).
2. **Settings file:** `--settings <path>` to a JSON file with
   pattern-based rules under `permissions.allow` and
   `permissions.deny`, e.g. `Bash(curl:*)`, `Read(/etc/shadow)`,
   `Write(/home/**)`.

The settings-file approach is more granular: it can deny a specific
bash command (`Bash(curl:*)` blocks every curl invocation regardless
of arguments) without disabling the entire Bash tool. This is what
autocoder needs — Bash is required for ordinary build/test workflows,
but specific network commands need to be blocked.

The current invocation site (`run_subprocess` in
`executor/claude_cli.rs`) spawns `claude` with `self.args` which is
empty by default. No restrictions of any kind.

## Goals / Non-Goals

**Goals:**

- The default deployment blocks the obvious exfiltration channels
  (network commands, credential file reads, web-fetch tools)
  without operator action.
- Operators can widen or tighten the default sandbox via
  configuration, expressed in autocoder's vocabulary (not raw
  Claude settings JSON).
- The sandbox is enforced at every executor invocation including
  `resume()` — not just initial `run()`. A waiting change that
  resumes after a Slack reply gets the same restrictions.
- The settings file is generated per-iteration in a temp directory,
  not committed to the workspace. It does not contaminate the diff
  or the agent branch.
- Honest about the threat model: the sandbox is a tool-routing
  layer, not an OS-level boundary. Documented.

**Non-Goals:**

- **OS-level sandboxing.** firejail/bubblewrap/systemd `ProtectHome`
  are operator-side concerns; the README points at them but
  autocoder does not wrap the spawn in any sandbox process.
- **Reviewer LLM data-flow control.** The code-reviewer sends the
  diff to its configured LLM provider as a documented data flow.
  Operators opted in by enabling the reviewer. The sandbox does
  not apply to the reviewer's API call.
- **Custom-MCP-server restrictions.** Out of scope; if an operator
  adds custom MCP servers via the workspace's `.mcp.json`, those
  are governed by the operator's own configuration of those
  servers.
- **Network-level egress control.** If an operator needs hard
  network egress prevention (no outbound except to GitHub +
  Anthropic), that's an iptables/firewall concern.
- **Per-change sandbox overrides.** The sandbox is per-executor,
  not per-change. A change cannot ask for relaxed restrictions in
  its proposal.md; the operator decides at config time.

## Decisions

### Config schema

```yaml
executor:
  kind: claude_cli
  command: claude
  timeout_secs: 1800
  sandbox:                              # optional; defaults apply if absent
    allowed_tools:                      # whitelist of Claude Code tool names
      - Read
      - Write
      - Edit
      - Glob
      - Grep
      - Bash
    disallowed_bash_patterns:           # Bash command patterns to deny
      - "curl:*"
      - "wget:*"
      - "nc:*"
      - "ncat:*"
      - "netcat:*"
      - "ssh:*"
      - "scp:*"
      - "sftp:*"
      - "rsync:*"
      - "git push:*"
      - "git remote *"
      - "git fetch *://*"
    disallowed_read_paths:              # path patterns to deny reads on
      - "/home/*/.ssh/**"
      - "/home/*/.claude/**"
      - "/etc/shadow"
      - "/etc/ssl/private/**"
```

All three sub-fields default to the values above when absent. To
**add** to a default list (e.g. extend the denylist with extra
patterns), the operator restates the full list plus their additions.
There is no "merge with default" syntax — explicit is clearer.

To **remove** items (e.g. drop `curl:*` because pip is needed), the
operator writes the list they want with the unwanted item omitted.

### Settings file generation

Per iteration:

1. Generate a temp file `claude-settings-<change>-<uuid>.json` in
   `std::env::temp_dir()`.
2. Write a JSON document of shape:
   ```json
   {
     "permissions": {
       "allow": [],
       "deny": [
         "Bash(curl:*)",
         "Bash(wget:*)",
         "...",
         "Read(/home/*/.ssh/**)",
         "Read(/home/*/.claude/**)",
         "..."
       ]
     }
   }
   ```
   Each `disallowed_bash_patterns` entry becomes `Bash(<pattern>)`;
   each `disallowed_read_paths` entry becomes `Read(<pattern>)`.
3. Spawn `claude --settings <temp-path> --allowedTools <comma-list> --permission-mode acceptEdits ...`.
4. Wait for the child to exit.
5. Delete the temp file (best-effort; failures logged but not
   fatal — temp dir is the OS's problem).

The `--allowedTools` flag duplicates the `allowed_tools` field for
defense-in-depth: if the settings file fails to parse, the
allowedTools flag still constrains the model. If the settings file
applies, the constraints are additive (intersection of both).

### Spawn command shape

Today: `claude` (with prompt on stdin).

After this change: `claude --settings /tmp/claude-settings-<uuid>.json --allowedTools Read,Write,Edit,Glob,Grep,Bash --permission-mode acceptEdits` (with prompt on stdin).

The `--permission-mode acceptEdits` directs Claude CLI to
auto-approve file-edit tool calls without prompting (no human is
present), while still consulting the settings file for the
allow/deny rules on other tools.

### Where in the code

`executor/claude_cli.rs::run_subprocess`:

```rust
async fn run_subprocess(&self, workspace: &Path, prompt: &str) -> Result<SubprocessOutcome> {
    let settings_path = self.write_sandbox_settings()?;     // NEW
    let _settings_guard = TempFileGuard(settings_path.clone()); // NEW (Drop cleans up)

    let mut cmd = Command::new(&self.command);
    cmd.args(&self.args);
    // NEW: sandbox-related flags
    cmd.arg("--settings").arg(&settings_path);
    cmd.arg("--allowedTools").arg(self.sandbox.allowed_tools.join(","));
    cmd.arg("--permission-mode").arg("acceptEdits");
    let mut child = cmd
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(...)?;
    // ... existing wait/timeout logic ...
}
```

`ClaudeCliExecutor` gains a `sandbox: ResolvedSandbox` field
populated from `ExecutorConfig.sandbox` (or the default) at
`new()`/`with_args()` time.

### Resume path

`resume()` calls the same `run_subprocess` helper. No additional
work needed — the sandbox applies uniformly.

### Visibility into denials

When the LLM tries a denied operation, Claude CLI surfaces it in
the model's output (the model is told the tool was denied and
typically narrates its frustration). This shows up in the
iteration's captured stdout, which autocoder already logs at
debug level. No new logging plumbing needed.

If an operator's workflow gets blocked by the defaults, the
iteration's logs make it clear what was denied. The operator
edits `executor.sandbox.disallowed_bash_patterns` to permit
the needed command and restarts.

### Backward compatibility

Existing `config.yaml` files without an `executor.sandbox` block
get the restrictive defaults on first start after upgrade. This
is **intentionally breaking** for operators whose existing
workflows depend on unrestricted tool use — the previous unsafe
default is being replaced with a safe one.

The README documents this clearly under the AI Security section.
Operators who want the prior "no restrictions" behavior can write:

```yaml
executor:
  sandbox:
    allowed_tools: [Read, Write, Edit, Glob, Grep, Bash, WebFetch, WebSearch]
    disallowed_bash_patterns: []
    disallowed_read_paths: []
```

— which is unsafe and the README will flag it as such, but is
available for operators who knowingly accept the trade.

## Risks / Trade-offs

- **Risk:** Tool-routing-layer sandboxes are bypassable in
  principle. A creative model could exec a shell from a binary
  it wrote to disk, or use a tool not in the denylist to achieve
  the same effect. The pattern matchers in Claude CLI's settings
  are best-effort.
  - **Mitigation:** README explicitly recommends OS-level
    sandboxing as the real boundary (firejail, bubblewrap,
    systemd `ProtectHome=`). The autocoder sandbox is a useful
    first layer; not a replacement for filesystem isolation.

- **Risk:** Default denials break operator workflows on upgrade.
  - **Mitigation:** Defaults are chosen to permit the common
    test runners (`cargo test`, `npm test`, `pytest`, `go test`)
    — none of these match the denylist patterns. The README
    walks through the most common widening scenarios (pip
    install, brew install, etc.). Iteration logs surface the
    specific denied command, making remediation a single config
    edit.

- **Risk:** `Read` rules don't bind `Bash(cat /etc/shadow)`.
  Reading a file via `cat` in Bash goes through the Bash tool,
  not the Read tool, so the path-based Read denials only block
  the Read tool. An LLM that wants to read `~/.ssh/id_ed25519`
  via `cat` is constrained only by the bash command patterns.
  - **Mitigation:** Add `Bash(cat /home/*/.ssh/**)`,
    `Bash(cat /home/*/.claude/**)`, etc. to the default
    `disallowed_bash_patterns`. The denylist gets longer but
    closes the loophole.

  Actually, on reflection: the cleaner mitigation is filesystem
  permissions on the autocoder host. `chmod 600` on
  `~autocoder/.ssh/id_ed25519` owned by autocoder means the
  agent (also running as autocoder) CAN read it regardless of
  sandbox — so the sandbox alone can't protect against an LLM
  that wants to read its own user's SSH key. The real protection
  for that scenario is the fork-and-PR workflow: even if the
  attacker reads the key and pushes from it, they can only
  push to the fork, not the upstream. This is why the two
  changes are mutually reinforcing.

- **Risk:** Operators forget to widen the sandbox before adding
  a new repo whose build needs `pip install` or similar.
  - **Mitigation:** Iteration logs include the denied command;
    the failure is visible. The chatops-progress-notifications
    change (queued separately) ensures persistent failures
    surface in the operator's chat channel.

- **Risk:** Claude CLI's settings-file syntax changes in a future
  release, breaking autocoder's generated files.
  - **Mitigation:** Pin Claude CLI to known-good versions in
    operator setup docs. Generated settings file is small and
    well-formed JSON; adapting to syntax changes is a small,
    contained change.
