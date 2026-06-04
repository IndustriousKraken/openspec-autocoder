## Why

Every LLM-consuming config block repeats the same four fields — `provider`, `model`, `api_base_url`, `api_key`/`api_key_env`. After a37 there are three such blocks (`reviewer`, the contradiction-check LLM block, `canonical_rag`), and the agentic-fleet migration is about to add more (agentic audits, the agentic reviewer and contradiction-check, the verifier roles). An operator running the same model across several roles must duplicate the tuple — and its secret reference — in each block. There is also no single place that says "this model is driven by which CLI," which the agentic-run primitive needs.

A top-level model registry fixes both: define a model once under a nickname, reference it by name everywhere, and let the registry entry's provider determine the default agentic CLI. This is the foundational change of the stream — it lands before the agentic roles proliferate so they reference nicknames from birth, and it defines the `provider → default CLI` resolution rule the primitive consumes.

## What Changes

**A top-level `models:` registry (orchestrator-cli config schema).** Config MAY define `models:` as a map from nickname to an LLM definition carrying the same four fields the per-subsystem blocks use today, plus an optional `cli` override:

```yaml
models:
  beefy_security:
    provider: openai_compatible
    model: moonshotai/kimi-k2
    api_base_url: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_KEY
  fast_local:
    provider: ollama
    model: qwen2.5-coder:32b
    api_base_url: http://localhost:11434
    # cli: opencode   # optional; defaults from provider
```

**Nickname references, discriminated by the presence of `provider`.** An LLM block that OMITS `provider` has its `model` field interpreted as a registry nickname and resolved to that entry's `(provider, model, api_base_url, api_key)`. A block that sets `provider` inline is the legacy form and the registry is not consulted. This is fully backward-compatible — every existing config sets `provider`, so it stays inline — and it realizes the manifest's `model: <nickname>` shorthand without overloading any field's meaning ambiguously:

```yaml
reviewer:
  enabled: true
  model: beefy_security        # no provider → registry nickname
contradiction_check_llm:
  provider: anthropic          # provider present → legacy inline, unchanged
  model: claude-opus-4-8
  api_key_env: ANTHROPIC_API_KEY
```

**Provider → default CLI rule.** Each registry entry's `provider` defines the default agentic CLI for that model: `anthropic` → the `claude` CLI; `openai_compatible` / `ollama` → the provider-agnostic CLI (`opencode`). The optional per-entry `cli` field overrides this default (e.g. drive an Anthropic model through `opencode`). The CLI strategies themselves land with the agentic-run primitive (a later change); the registry defines the rule so model and CLI selection are specified in one place.

**Dual-acceptance, indefinitely.** Inline blocks remain valid forever; the registry is the deduped form, not a forced migration. No `command:`/CLI field appears on any role block — CLI selection is resolved from the model's provider, never named per role.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — ADDED `Top-level model registry with nickname references`. The existing `Canonical LlmProvider enum AND per-provider auth semantics` and `Per-subsystem provider validity is enforced at config-load` requirements are unchanged; the registry resolves to the same `(provider, model, …)` they already validate.
- **Affected code:**
  - `autocoder/src/config.rs` — add a top-level `models: Option<BTreeMap<String, ModelEntry>>` field; `ModelEntry { provider, model, api_base_url, api_key/api_key_env, cli: Option<CliKind> }`. At config-load, for each LLM block with no inline `provider`, resolve its `model` against the registry, populating the four fields; validate the resolved provider through the existing `SubsystemKind` gate. Add a `provider → default CLI` resolver (`CliKind::{Claude, Opencode}`) honoring the per-entry `cli` override.
  - The three LLM blocks (`ReviewerConfig`, the contradiction-check block, `CanonicalRagConfig`) gain the "provider optional → nickname" load path; their downstream consumers see the same resolved tuple as today.
  - `config.example.yaml` — document the `models:` block and the nickname-reference form, noting inline blocks remain valid.
- **Operator-visible behavior:** none at runtime. Configs may be deduplicated via the registry; existing inline configs load identically.
- **Acceptance:** `cargo test` passes; `openspec validate a55-model-registry --strict` passes. Tests: a block omitting `provider` resolves its `model` from the registry; an inline block (with `provider`) is unaffected; a missing nickname fails config-load with a naming error; a resolved provider is gated by subsystem validity (RAG rejects `anthropic`); the provider→CLI resolver returns `claude` for `anthropic`, `opencode` for `ollama`/`openai_compatible`, and honors the `cli` override.
- **Dependencies:** none. Foundational for the agentic-fleet stream (the primitive, agentic roles, and verifier roles reference nicknames + the CLI rule).
