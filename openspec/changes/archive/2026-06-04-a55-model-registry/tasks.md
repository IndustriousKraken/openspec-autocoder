# Implementation tasks

## 1. Registry schema + resolver

- [x] 1.1 In `autocoder/src/config.rs`, add `pub struct ModelEntry { provider: LlmProvider, model: String, api_base_url: Option<String>, api_key: Option<SecretSource>, api_key_env: Option<String>, cli: Option<CliKind> }` AND `enum CliKind { Claude, Opencode }`.
- [x] 1.2 Add a top-level `models: Option<BTreeMap<String, ModelEntry>>` field to the root config struct (deterministic ordering for diagnostics).
- [x] 1.3 Make `provider` OPTIONAL on the three LLM blocks (`ReviewerConfig`, the contradiction-check LLM block, `CanonicalRagConfig`). At config-load, for a block whose `provider` is `None`, resolve its `model` against `models:` — populate `(provider, model, api_base_url, api_key/api_key_env)` from the entry. A block whose `provider` is `Some` is the legacy inline form; do NOT consult the registry.
- [x] 1.4 Add `fn default_cli_for(provider: LlmProvider) -> CliKind` (`Anthropic → Claude`; `OpenAiCompatible`/`Ollama → Opencode`) AND a resolver that returns the entry's `cli` override when set, else `default_cli_for(provider)`. (The strategies consuming this land in a later change; this change defines the rule.)

## 2. Validation

- [x] 2.1 A block with no inline `provider` whose `model` is not a `models:` key SHALL fail config-load with an error naming the missing nickname AND the referencing block.
- [x] 2.2 The resolved provider SHALL pass the existing `validate_provider_for_subsystem` / `SubsystemKind` gate exactly as an inline provider would (e.g. a RAG block resolving to `anthropic` fails).
- [x] 2.3 `api_key`/`api_key_env` precedence (inline wins, dual-set WARN) applies to the resolved entry, reusing the existing `SecretSource` logic.
- [x] 2.4 A `models:` entry that is itself invalid (e.g. `ollama` with an `api_key`) fails config-load via the existing per-provider auth validation, regardless of whether any block references it.

## 3. Docs

- [x] 3.1 `config.example.yaml` — add a `models:` block with two example nicknames AND show one role referencing a nickname (no `provider`) alongside one legacy inline role. One-line note that inline blocks remain valid.
- [x] 3.2 `docs/CONFIG.md` — document the registry, the nickname-reference form (omit `provider`), the `cli` override, AND the provider→default-CLI rule.

## 4. Tests

- [x] 4.1 A `reviewer` block with `model: beefy_security` and no `provider` resolves to the registry entry's full tuple.
- [x] 4.2 A block with inline `provider` loads identically to today (registry not consulted).
- [x] 4.3 `model:` naming a non-existent nickname fails config-load with a diagnostic naming the nickname and the block.
- [x] 4.4 A RAG block resolving (via registry) to `anthropic` fails the subsystem-validity gate.
- [x] 4.5 `default_cli_for`: `anthropic → claude`; `ollama`/`openai_compatible → opencode`; a registry entry's `cli: claude` override on an `openai_compatible` model resolves to `claude`.

## 5. Acceptance gate

- [x] 5.1 `cargo test` passes for the autocoder crate.
- [x] 5.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 5.3 `openspec validate a55-model-registry --strict` passes.
