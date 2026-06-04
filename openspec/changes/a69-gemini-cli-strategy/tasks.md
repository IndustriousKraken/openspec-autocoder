# Implementation tasks

## 1. Integration spike (do first — it sizes the rest)

- [ ] 1.1 Confirm non-interactive `gemini` invocation: the prompt-delivery mechanism (stdin vs `-p`/positional), the flag set for headless one-shot runs, and the exit/output behavior in capture mode.
- [ ] 1.2 Confirm MCP wiring: a `.gemini/settings.json` `mcpServers` entry (`command`/`args`/`env`) connects the orchestrator MCP child headlessly, and the role's `submit_*` tool is callable, with `ORCH_MCP_ROLE` reaching the child via `env`.
- [ ] 1.3 Confirm the `coreTools` allowlist actually restricts tools in non-interactive mode (the reported policy-enforcement gap): a read-only allowlist must prevent shell/write/edit. Record the exact allowlist that yields read-only + working MCP submission.

## 2. GeminiStrategy

- [ ] 2.1 Implement `GeminiStrategy` as a third `CliStrategy` (a56). Build the `gemini` invocation: `--model <model>`, the spike-confirmed headless flags and prompt delivery, capture mode.
- [ ] 2.2 Write `.gemini/settings.json` into the workspace with the MCP `mcpServers` block (MCP-child `command`/`args` + `env` including `ORCH_MCP_ROLE`). Write neither `.mcp.json` nor `opencode.json`.
- [ ] 2.3 Map a56's sandbox onto Gemini config: read-only roles get a `coreTools` allowlist of the read tools + the role's `submit_*` tool, excluding shell/write/edit.
- [ ] 2.4 Set Gemini auth env (`GEMINI_API_KEY` / Vertex per the resolved model); set no `ANTHROPIC_*`.
- [ ] 2.5 Confirm the read-only roles' existing `WritePolicy::None` post-hoc enforcement (non-empty `git status --porcelain` → `git reset --hard HEAD` + fail) applies to gemini runs, as the backstop for the non-interactive policy gap.

## 3. Registry wiring

- [ ] 3.1 Ensure a role can resolve to `GeminiStrategy` via an explicit registry `cli: gemini`, AND via a55's `provider → CLI` rule mapping the Gemini provider to the `gemini` CLI (add the one-line mapping in coordination with a55 if not already present).

## 4. Tests

- [ ] 4.1 Strategy resolution: a Gemini-provider model (or explicit `cli: gemini`) resolves to `GeminiStrategy`, not a "no registered strategy" error; the invocation selects `--model <model>`.
- [ ] 4.2 Config emission: a role run writes `.gemini/settings.json` with an `mcpServers` entry carrying `ORCH_MCP_ROLE` in `env`; no `.mcp.json` / no `opencode.json` is written.
- [ ] 4.3 Auth env: a Gemini model sets the Gemini auth env and none of `ANTHROPIC_*`.
- [ ] 4.4 Read-only sandbox: a read-only role's generated `coreTools` allowlist contains the read tools + the `submit_*` tool and excludes shell/write/edit.
- [ ] 4.5 Backstop: a read-only gemini run that leaves a non-empty `git status` triggers the `WritePolicy::None` revert + run failure (assert the revert + failure, using a synthetic write).
- [ ] 4.6 Submission contract: a schema-invalid `submit_*` payload surfaces a correctable tool error retryable in the same session.

## 5. Acceptance gate

- [ ] 5.1 `cargo test` passes for the autocoder crate.
- [ ] 5.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] 5.3 `openspec validate a69-gemini-cli-strategy --strict` passes.
