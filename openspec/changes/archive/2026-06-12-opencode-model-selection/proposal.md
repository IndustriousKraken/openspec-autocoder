## Why

The `OpencodeStrategy` (and its spec) selected the model with `--model <LlmProvider>/<model>` — e.g. `openai_compatible/qwen/qwen3-max`. But `openai_compatible` is autocoder's API *type*, not an opencode provider id; `opencode models openai_compatible` returns "Provider not found". opencode's `--model` is `<opencode-provider-id>/<model>`, where the provider is one opencode knows (a built-in or a defined one, e.g. `openrouter`). So:

- A keyless `openai_compatible` model (the common case — the operator authenticated a provider via `opencode auth login`) failed: autocoder wrote a key-less `provider` block that SHADOWED opencode's stored credentials → "No cookie auth credentials found"; and even with the block omitted, `--model openai_compatible/...` resolves to a non-existent provider.
- The agentic reviewer passed `model: None`, so it silently ran opencode's *default* model while its ledger line named the configured model — the verdict attribution was cosmetic.

The new gate-verdict ledger surfaced this precisely: reviewer and `[out]` used the same model, the reviewer "passed" (on opencode's default), `[out]` failed (malformed selection). autocoder must use opencode's real provider ids and never assume one.

## What Changes

- **`--model` follows opencode's contract.** When autocoder DEFINES the provider — `ollama` (always, for its base URL) or `openai_compatible` WITH a key (autocoder injects it) — it writes the `opencode.json` `provider` block and selects `--model <provider-id>/<model>` matching that block. When autocoder DEFERS to opencode's own auth — keyless `openai_compatible` (login-authed) — it writes NO provider block (a key-less block shadows the login) and passes the operator's `model` to `--model` **verbatim**, which MUST be the real opencode id (e.g. `openrouter/qwen/qwen3-max`). autocoder never infers the provider.
- **Agentic roles pass their configured model.** The verifier gates already do; the agentic reviewer now resolves a `ResolvedModel` (`resolve_reviewer_model`) and passes it (was `model: None`), so opencode runs the configured model and the ledger attribution is truthful.
- **Docs:** `docs/CONFIG.md` + `config.example.yaml` document the per-case `model:` convention (login-authed keyless → full opencode id; keyed custom endpoint → bare id + base + key; ollama → bare id + base).

## Impact

- **Affected specs:** `executor` — MODIFY `OpencodeStrategy implements the opencode CLI for agentic roles` (model-selection + provider-block by case; roles pass their resolved model).
- **Affected code (implemented):** `agentic_run.rs` (`writes_provider_block` helper; `provider_block` omits the key-less authenticating block; `apply_model_selection` passes the model verbatim when deferring, `<provider-id>/<model>` when autocoder defines the provider); `code_reviewer.rs` (`resolved_model` field + `with_resolved_model`, threaded into `run_session`); `llm.rs` (`resolve_reviewer_model`). Tests updated/added at all three layers.
- **Operator-visible:** a login-authed (keyless) `openai_compatible` model now works — `model:` must be the full opencode id; `api_base_url` is unused for that case. The reviewer now runs its configured model rather than opencode's default.
- **Relationship:** supersedes the interim "omit the key-less block" fix (issue-class) by specifying the full convention. Pairs with `verifier-gates-fail-closed` (the ledger that surfaced this).
- **Acceptance:** `cargo test` (strategy + reviewer + resolver tests) + `openspec validate opencode-model-selection --strict`.
