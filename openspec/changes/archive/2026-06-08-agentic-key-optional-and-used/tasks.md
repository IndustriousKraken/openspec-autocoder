# Tasks

Both parts are implemented: **Part 1** — `api_key` optional for CLI/agentic roles (the boot fix). **Part 2** — a supplied key is passed to the wrapped CLI, uniformly across claude / opencode / agy.

## 1. Config-load: api_key optional for CLI/agentic roles

- [x] 1.1 Consumer-aware validation: `validate_llm_provider_config_cli` (key optional, base-url still enforced) vs `validate_llm_provider_config` (HTTP: key required). Registry entries, the verifier gates, AND the agentic reviewer use the `_cli` variant; canonical_rag AND a oneshot reviewer use the HTTP variant.
- [x] 1.2 Verifier-gate LLM blocks never require `api_key` at config-load.
- [x] 1.3 An ollama CLI/agentic role tolerates (and ignores) a key; the forbid stays for an in-process HTTP ollama consumer.

## 2. Strategies pass a supplied key

- [x] 2.1 `ClaudeStrategy::apply_model_selection`: sets `ANTHROPIC_API_KEY` when the resolved model has a key; nothing when empty (own login).
- [x] 2.2 `opencode` strategy: writes `options.apiKey: "{env:AUTOCODER_OPENCODE_API_KEY}"` (a REFERENCE) into the workspace `opencode.json` AND sets that env var to the secret on the subprocess — the raw secret never enters the committed file. Never touches `~/.config/opencode/auth.json`.
- [x] 2.3 `agy` (antigravity) already passes `AV_API_KEY` when present — covered by an existing test.

## 3. Best-effort hiding + residual

- [x] 3.1 The raw key is never written to a workspace file; the `opencode.json` reference resolves from the engine-deny/env path. Live-verified: opencode interpolated `{env:...}` and sent `Authorization: Bearer <bogus key>` against a local capture server.
- [x] 3.2 Env residual documented: the supplied key reaches the subprocess env, where the same-uid model can read it (claude/agy direct, opencode via interpolation). `SECURITY.md` states this; the daemon logs one startup WARN per keyed CLI role (`cli_role_key_exposure_warning`).

## 4. Tests (placement = always-on; live = manual)

- [x] 4.1 Keyless CLI role loads (`cli_validator_*`, `keyless_cli_roles_load_end_to_end`, `agentic_reviewer_loads_without_api_key`).
- [x] 4.2 A supplied key is passed: `cli_role_with_key_warns_exposure_and_strategy_passes_it` (claude → `ANTHROPIC_API_KEY`), `opencode_passes_supplied_key_via_env_reference` (opencode.json ref + env, no raw secret), existing agy `AV_API_KEY` test. No raw key in any workspace file: `no_strategy_writes_raw_key_to_workspace_file`. No-key default: `claude_strategy_without_key_sets_endpoint_and_model_no_credential`, `opencode_strategy_without_key_writes_no_api_key`. All inspect built output — no CLI/sandbox needed, run in any CI mode. Live opencode interpolation verified manually (the bogus-key capture probe), kept out of the always-on suite per the CLI/sandbox-mode split.
- [x] 4.3 Config-load no longer requires a key for a CLI-driven role but STILL requires it for the oneshot reviewer / RAG (`config_load_rejects_reviewer_*` use `kind: oneshot`).
- [x] 4.4 Placement is asserted on the written `opencode.json` (env reference present, raw secret absent) + the subprocess env.

## 5. Documentation

- [x] 5.1 `docs/SECURITY.md` § "Supplied LLM keys for CLI roles (opt-in exposure)" — no-key default vs supplied-key exposure; same-uid residual; in-process HTTP roles unaffected.
- [x] 5.2 `docs/CONFIG.md` + `config.example.yaml`: `api_key` optional for CLI/agentic roles; if supplied, passed-and-exposed (links to SECURITY).

## 6. Acceptance

- [x] 6.1 `cargo test` passes (the only full-suite failure is the pre-existing `event_dedup` / `json_streaming_timeout_kill` timing flakes — both pass isolated).
- [x] 6.2 `openspec validate agentic-key-optional-and-used --strict` passes.
