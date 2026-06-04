# orchestrator-cli — delta for a55-model-registry

## ADDED Requirements

### Requirement: Top-level model registry with nickname references
Config SHALL accept an optional top-level `models:` map from nickname to a model definition carrying `provider` (an `LlmProvider`), `model`, `api_base_url`, `api_key`/`api_key_env`, AND an optional `cli` override (`claude` | `opencode`). Each LLM-consuming config block (`reviewer`, the contradiction-check LLM block, `canonical_rag`, AND any future agentic-role block) SHALL be loadable either inline (the legacy four-field form) OR as a registry reference, discriminated by the presence of `provider`:

- A block that SETS `provider` is the legacy inline form; the registry SHALL NOT be consulted for it. Every existing config takes this path unchanged.
- A block that OMITS `provider` SHALL have its `model` field interpreted as a `models:` nickname AND resolved to that entry's `(provider, model, api_base_url, api_key/api_key_env)` before the block's downstream consumer runs.

The registry is the deduped form, not a forced migration; inline blocks remain valid indefinitely. No role block carries a `command:` or CLI-naming field — CLI selection is resolved from the model's provider (below), never named per role.

The resolved `(provider, …)` SHALL pass the existing per-subsystem validity gate (`SubsystemKind`) exactly as an inline provider would, AND the existing `api_key`/`api_key_env` precedence (inline value wins; dual-set emits a WARN) applies to the resolved entry.

The registry also defines the `provider → default CLI` rule that the agentic-run primitive consumes: `anthropic` → the `claude` CLI; `openai_compatible` AND `ollama` → the provider-agnostic CLI (`opencode`). A registry entry's optional `cli` field overrides this default for that model. The CLI strategies themselves are introduced by a later change; this requirement defines the rule so model selection and CLI selection are specified together.

#### Scenario: Block referencing a nickname resolves the full tuple
- **GIVEN** a `models:` entry `beefy_security` with `provider: openai_compatible`, `model: moonshotai/kimi-k2`, an `api_base_url`, AND an `api_key_env`
- **WHEN** a `reviewer` block sets `model: beefy_security` AND omits `provider`
- **THEN** config-load resolves the reviewer's `(provider, model, api_base_url, api_key)` from the `beefy_security` entry
- **AND** the reviewer's downstream consumer sees the same resolved tuple it would have seen from an equivalent inline block

#### Scenario: Inline block is unchanged by the registry
- **GIVEN** a `contradiction_check_llm` block that sets `provider: anthropic`, `model: claude-opus-4-8`, AND `api_key_env`
- **WHEN** config loads
- **THEN** the block is treated as legacy inline AND the registry is not consulted
- **AND** the block loads byte-identically to its pre-registry behavior

#### Scenario: Missing nickname fails config-load
- **WHEN** a block omits `provider` AND its `model` does not match any `models:` key
- **THEN** config-load fails with an error naming the missing nickname AND the referencing block

#### Scenario: Resolved provider is gated by subsystem validity
- **GIVEN** a `models:` entry whose `provider` is `anthropic`
- **WHEN** the `canonical_rag` block references that entry (omitting `provider`)
- **THEN** config-load fails the `SubsystemKind::CanonicalRag` validity gate (Anthropic exposes no embeddings API) exactly as an inline `provider: anthropic` would

#### Scenario: Provider determines the default CLI, with per-entry override
- **WHEN** the provider→CLI resolver runs against registry entries
- **THEN** an `anthropic` entry resolves to the `claude` CLI
- **AND** an `ollama` OR `openai_compatible` entry resolves to the `opencode` CLI
- **AND** an entry with an explicit `cli: claude` resolves to `claude` regardless of its provider
