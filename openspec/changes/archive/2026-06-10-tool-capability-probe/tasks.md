# Tasks

## 1. Spec

- [x] 1.1 ADD `Startup tool-capability probe for agentic model endpoints` (orchestrator-cli) with scenarios (toolless flagged, capable info, unreachable non-blocking, CLI-self-auth skipped, not in check-config).

## 2. Code

- [x] 2.1 `tool_probe.rs`: `classify_probe_response` (pure: tool_call → Supported; prose/4xx → NoToolSupport; 5xx/undecodable → Unreachable), `probe_endpoint` (OpenAI-compat `/chat/completions` tools request, timeout), `resolve_entry_key`, `run_tool_capability_preflight`.
- [x] 2.2 `main.rs`: register `mod tool_probe;`.
- [x] 2.3 `cli/run.rs`: call `run_tool_capability_preflight(&cfg).await` at startup after `dependency_preflight` (best-effort; never blocks).

## 3. Tests

- [x] 3.1 `classify_probe_response` unit tests: tool-call → Supported; prose-only → NoToolSupport; 4xx → NoToolSupport; 5xx + undecodable-2xx → Unreachable; empty tool_calls array → NoToolSupport.

## 4. Docs

- [x] 4.1 `docs/CONFIG.md`: note that the daemon probes agentic registry models for tool support at startup and WARNs when a model can't emit tool calls.

## 5. Acceptance

- [x] 5.1 `cargo test` passes (probe tests + full suite green).
- [x] 5.2 `openspec validate tool-capability-probe --strict` passes.
