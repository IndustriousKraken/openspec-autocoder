# Implementation tasks

## 1. Integration spike — probe the installed `agy` (do first; it sizes the rest)

> `agy` (Antigravity CLI) is installed and logged in on the host, and the implementer's Bash can run it (the sandbox denies only `curl` and `git push`). Confirm every item below by **probing the installed binary** — `agy --help`, minimal test invocations, and inspecting `~/.antigravity/` plus the config files it reads — NOT from model training, which predates Antigravity. The installed binary is the source of truth; do not guess flags or schemas. (Test `agy -p` runs consume the operator's Antigravity quota — keep them minimal. `agy` must be on the autocoder user's `PATH` and its login/`AV_API_KEY` accessible to that user; if not, surface that as a blocker rather than guessing.)

- [ ] 1.1 Probe non-interactive invocation: confirm `agy -p "<prompt>"` single-shot command mode (via `agy --help`), the headless flag set, and the exit/output behavior in capture mode (exit codes are non-zero on tool-use failures — the daemon's outcome detection must account for that).
- [ ] 1.2 Probe MCP wiring: confirm the exact `mcp_config.json` schema `agy` actually reads (server entry: `command`/`args`/`env`, local stdio), wire the orchestrator MCP child, and verify the role's `submit_*` tool is callable with `ORCH_MCP_ROLE` reaching the child via `env`.
- [ ] 1.3 Probe the read-only restriction: confirm the mechanism that denies shell/write/edit while allowing the read tools + the `submit_*` tool, AND test it — run a restricted `agy -p` session that attempts a write and verify the write is actually blocked (the OS Terminal Sandbox and/or an allow/deny config). Record the exact configuration.

## 2. AntigravityStrategy

- [ ] 2.1 Implement `AntigravityStrategy` as a third `CliStrategy` (a56). Build the `agy -p` invocation: `--model <model>` (default `gemini-3-pro`), the spike-confirmed headless flags, capture mode.
- [ ] 2.2 Write `mcp_config.json` into the workspace with the MCP server entry (MCP-child `command`/`args` + `env` including `ORCH_MCP_ROLE`, local stdio). Write neither `.mcp.json` nor `opencode.json`.
- [ ] 2.3 Map a56's sandbox onto Antigravity's tool restriction: read-only roles get the read tools + the role's `submit_*` tool, denying shell/write/edit (per the spike).
- [ ] 2.4 Set Antigravity auth env (`AV_API_KEY` per the resolved model); set no `ANTHROPIC_*`.
- [ ] 2.5 Confirm the read-only roles' existing `WritePolicy::None` post-hoc enforcement (non-empty `git status --porcelain` → `git reset --hard HEAD` + fail) applies to agy runs, as the backstop for the non-interactive policy gap.

## 3. Registry wiring

- [ ] 3.1 Ensure a role can resolve to `AntigravityStrategy` via an explicit registry `cli: antigravity`, AND via a55's `provider → CLI` rule mapping the Google/Antigravity provider to the `agy` CLI (add the one-line mapping in coordination with a55 if not already present).

## 4. Tests

- [ ] 4.1 Strategy resolution: a Google/Antigravity-provider model (or explicit `cli: antigravity`) resolves to `AntigravityStrategy`, not a "no registered strategy" error; the invocation selects `--model <model>` and `agy -p`.
- [ ] 4.2 Config emission: a role run writes `mcp_config.json` with an MCP-server entry carrying `ORCH_MCP_ROLE` in `env`; no `.mcp.json` / no `opencode.json` is written.
- [ ] 4.3 Auth env: an Antigravity model sets `AV_API_KEY` and none of `ANTHROPIC_*`.
- [ ] 4.4 Read-only sandbox: a read-only role's generated tool restriction contains the read tools + the `submit_*` tool and denies shell/write/edit.
- [ ] 4.5 Backstop: a read-only agy run that leaves a non-empty `git status` triggers the `WritePolicy::None` revert + run failure (assert the revert + failure, using a synthetic write).
- [ ] 4.6 Submission contract: a schema-invalid `submit_*` payload surfaces a correctable tool error retryable in the same session.

## 5. Acceptance gate

- [ ] 5.1 `cargo test` passes for the autocoder crate.
- [ ] 5.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] 5.3 `openspec validate a69-antigravity-cli-strategy --strict` passes.
