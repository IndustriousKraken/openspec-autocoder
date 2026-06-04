## Why

A user has requested Gemini CLI support. The swappable-CLI design already abstracts this: `a56`'s `CliStrategy` trait + `a55`'s model registry resolve a role's CLI from its model provider, with `claude` (a56) and `opencode` (a60) implemented. Gemini is a natural third strategy. For an operator who has a Gemini subscription it is a first-class option (better than a local Ollama model for the structured-submission roles), and a non-Anthropic model family adds genuine cross-check value to the reviewer and audit roles — the model-diversity principle that the verification layers are built on.

Gemini CLI supports the load-bearing primitives: non-interactive invocation, MCP servers (`.gemini/settings.json` → `mcpServers` with `command`/`args`/`env`), model selection (`--model`), and a `coreTools` allowlist for tool restriction. The one caveat is that Gemini's tool-policy enforcement has reported gaps in non-interactive mode, which this change handles with a spike plus the existing read-only write-revert backstop.

## What Changes

**A third `CliStrategy`, `GeminiStrategy`, for the `gemini` CLI**, mirroring `a60`'s `OpencodeStrategy`:
- selects the model via `--model <model>`;
- writes `.gemini/settings.json` carrying the MCP `mcpServers` block (the MCP-child `command`/`args`, AND `env` including `ORCH_MCP_ROLE`);
- maps `a56`'s sandbox onto Gemini's `coreTools` **allowlist** — a read-only role exposes the read tools plus the role's `submit_*` tool and excludes shell/write/edit (allowlisting is Gemini's recommended, more-secure form over blocklisting);
- sets Gemini's auth env (`GEMINI_API_KEY` / Vertex), NO `ANTHROPIC_*`, and writes neither `.mcp.json` nor `opencode.json`;
- runs in **capture mode** (the streaming-JSON path is claude-specific), so Gemini serves the capture-mode structured-submission roles — the advisory audits, the reviewer, the contradiction check.

**Read-only safety under Gemini's non-interactive policy gaps.** A read-only Gemini role does NOT rely on the `coreTools` allowlist alone: the existing read-only post-hoc enforcement (`WritePolicy::None` — a non-empty post-run `git status` reverts via `git reset --hard HEAD` AND fails the run) applies, so any write that escapes the allowlist is caught and reverted rather than corrupting the workspace. An integration spike verifies the allowlist actually holds in non-interactive mode.

This does NOT change any role's default transport (operator opt-in, per a60). It covers the capture-mode roles; **Gemini (and OpenCode) as the streaming implementer is unlocked separately** by the capture-mode-implementer change.

## Impact

- **Affected specs:** `executor` — ADD `GeminiStrategy implements the gemini CLI for agentic roles`.
- **Affected code:** `GeminiStrategy` in the strategy module; a `.gemini/settings.json` writer (MCP `mcpServers` + env); the `coreTools`-allowlist sandbox mapping; Gemini auth env; capture-mode wiring through `agentic_run`.
- **Registry:** a role resolves to `GeminiStrategy` via an explicit registry `cli: gemini`, OR `a55`'s `provider → CLI` rule mapping the Gemini provider to the `gemini` CLI (a one-line registry mapping; coordinated with a55, not re-modified here).
- **Operator-visible behavior:** Gemini becomes an opt-in CLI for the reviewer, advisory audits, and contradiction check; defaults unchanged.
- **Dependencies:** `a56` (the `CliStrategy` trait + `agentic_run`) AND `a55` (the registry / `provider → CLI` rule). Sibling of `a60`. Processes after `a56`.
- **Acceptance:** `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean; `openspec validate a69-gemini-cli-strategy --strict` passes; the spike confirms the read-only allowlist holds in non-interactive mode (and the post-hoc revert backstop is in place regardless).
