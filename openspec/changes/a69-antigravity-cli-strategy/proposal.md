## Why

A user requested Gemini CLI support — but Google is **sunsetting Gemini CLI on June 18, 2026** (free / Pro / Ultra / individual Code Assist tiers stop serving requests; only enterprise Code Assist licenses retain it) and transitioning everyone to **Antigravity CLI** (`agy`). So the Google-model strategy should target Antigravity CLI, not the soon-dead Gemini CLI.

Antigravity CLI is a clean fit — the same wrapped-headless-agent shape as `claude` and `opencode`, and arguably better suited to the daemon (pure Go, no Node dependency chain): single-shot command mode (`agy -p "<prompt>"`), MCP servers (local stdio + remote HTTP via `mcp_config.json`), model selection (`--model`, default `gemini-3-pro`), and an OS-level Terminal Sandbox. For an operator with a Google subscription it's a first-class option, and a non-Anthropic model family adds cross-check value to the reviewer/audit roles.

The one caveat is read-only enforcement for the no-write roles: the OS sandbox protects the host, but the role still needs Write/Edit/Bash *denied*; the exact tool-restriction mechanism is confirmed by the integration spike, with the existing read-only write-revert backstop as the safety net regardless.

Caution carried over: Antigravity drives `gemini-3-pro`, a Gemini-family model, and a prior Gemini-as-implementer attempt produced placeholder code. So Antigravity is offered **per-role** — strong for reviewer/auditor cross-check; the implementer role is the operator's deliberate choice (unlocked by a70), gated by the reviewer / verifier / no-stubs controls.

## What Changes

**A third `CliStrategy`, `AntigravityStrategy`, for the `agy` CLI**, mirroring `a60`'s `OpencodeStrategy`:
- runs single-shot command mode (`agy -p "<prompt>"`, capture) AND selects the model via `--model <model>` (default `gemini-3-pro`);
- writes an `mcp_config.json` into the workspace carrying the MCP server entry (the MCP-child `command`/`args`, AND `env` including `ORCH_MCP_ROLE`, local stdio transport);
- maps `a56`'s sandbox onto Antigravity's tool restriction — a read-only role exposes only the read tools plus the role's `submit_*` tool and denies shell/write/edit (exact mechanism per the spike), atop the OS-level Terminal Sandbox;
- sets Antigravity's auth env (`AV_API_KEY`), NO `ANTHROPIC_*`, and writes neither `.mcp.json` nor `opencode.json`;
- runs in **capture mode** (`--stream`/SSE is Antigravity's own streaming, not claude's stream-json), so it serves the capture-mode structured-submission roles — advisory audits, reviewer, contradiction check.

**Read-only safety.** A read-only Antigravity role does NOT rely on the tool restriction alone: the existing read-only post-hoc enforcement (`WritePolicy::None` — non-empty post-run `git status` reverts via `git reset --hard HEAD` AND fails the run) applies, so any escaped write is caught and reverted. The spike verifies the restriction holds under `agy -p`.

This does NOT change any role's default transport (operator opt-in). It covers the capture-mode roles; **Antigravity (and OpenCode) as the streaming implementer is unlocked separately** by a70.

## Impact

- **Affected specs:** `executor` — ADD `AntigravityStrategy implements the agy CLI for agentic roles`.
- **Affected code:** `AntigravityStrategy` in the strategy module; an `mcp_config.json` writer (MCP server + `env`); the tool-restriction sandbox mapping; `AV_API_KEY` auth env; capture-mode wiring through `agentic_run`.
- **Registry:** a role resolves to `AntigravityStrategy` via an explicit registry `cli: antigravity`, OR `a55`'s `provider → CLI` rule mapping the Google/Antigravity provider to the `agy` CLI.
- **Operator-visible behavior:** Antigravity becomes an opt-in CLI for the reviewer, advisory audits, and contradiction check; defaults unchanged.
- **Dependencies:** `a56` (the `CliStrategy` trait + `agentic_run`) AND `a55` (registry). Sibling of `a60`. Processes after `a56`.
- **Acceptance:** `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean; `openspec validate a69-antigravity-cli-strategy --strict` passes; the spike confirms the read-only tool restriction holds in non-interactive `agy -p` mode (and the post-hoc revert backstop is in place regardless).
