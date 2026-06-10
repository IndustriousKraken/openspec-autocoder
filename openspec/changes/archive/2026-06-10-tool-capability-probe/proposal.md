## Why

The verifier gates ([in]/[canon]/[out]) and the agentic reviewer drive their model through a tool-using CLI session: the model must call the `Read` tool to open the change, then a `submit_*` MCP tool to return its verdict. A model whose endpoint cannot emit tool calls — an older family with no function-calling template, an abliterated finetune that broke the template — never reads the change and never submits, so the fail-closed gate holds every change with an inscrutable cause ("models can't use tools", stray prose like "there's no cookie"). The operator only discovers this after configuring the model, triggering a change, and reading a cryptic held-marker — a long, opaque loop. Intelligence is not the gate here; the tool template is, and nothing surfaced its absence up front.

## What Changes

- At startup (after the dependency preflight, before polling), the daemon probes each `models:` registry entry whose provider is `openai_compatible`/`ollama` by sending ONE tool-calling request to `<api_base_url>/chat/completions` (the exact path opencode uses) and inspecting the response for a tool call.
- It emits a **WARN** (never blocks startup) when the endpoint returns no tool call or rejects the tools request — naming the model and the remedy ("use a model whose template supports tools; `ollama show <model>` should list `tools`"). It logs an info line when the endpoint emits a tool call, and a could-not-run WARN when the probe cannot complete (unreachable/timeout/5xx).
- Scoped to the registry (always agentic). `anthropic`/`google` entries drive the `claude`/`agy` CLIs, which self-authenticate AND are known tool-capable, so they are not probed; an `openai_compatible` entry with no resolvable config key is skipped (no way to authenticate the probe).
- Best-effort and time-bounded; it is a startup-only network behavior and is NOT part of the side-effect-free `check-config`.

## Impact

- **Affected specs:** `orchestrator-cli` — ADD `Startup tool-capability probe for agentic model endpoints`.
- **Affected code:** new `tool_probe.rs` (`classify_probe_response`, `probe_endpoint`, `run_tool_capability_preflight`); wired into `cli/run.rs` startup after `dependency_preflight`. Five classification unit tests.
- **Affected docs:** `docs/CONFIG.md` (models registry note).
- **Operator-visible:** a startup WARN/info line per agentic registry model; a toolless model is flagged before any change is held. No behavior change to a daemon whose models already support tools.
- **Acceptance:** `cargo test` (classification tests + full suite) + `openspec validate tool-capability-probe --strict`.
