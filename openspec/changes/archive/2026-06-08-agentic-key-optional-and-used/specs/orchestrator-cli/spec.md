# orchestrator-cli — delta for agentic-key-optional-and-used

## MODIFIED Requirements

### Requirement: Canonical `LlmProvider` enum AND per-provider auth semantics

The autocoder config schema SHALL define a single canonical `LlmProvider` enum with three variants AND their YAML strings:

- `anthropic` — Anthropic's hosted API (`https://api.anthropic.com` default).
- `openai_compatible` — Any OpenAI-API-shaped endpoint (OpenAI itself, Grok, OpenRouter, vLLM, local OpenAI-compat shims, etc.).
- `ollama` — Ollama's native API (`<base>/api/chat` for completion, `<base>/api/embed` for embeddings).

`LlmProvider` SHALL be the type of the `provider` field across every LLM-touching config block: `reviewer:`, `canonical_rag:`, AND `executor.change_internal_contradiction_check_llm:`. Backward compatibility: the existing `RagProvider` AND `ReviewerProvider` enum names SHALL be retained as type aliases (`pub type RagProvider = LlmProvider;` etc.) so external-crate or test-code consumers compile unchanged. Existing config files using `provider: anthropic`, `provider: openai_compatible`, AND `provider: ollama` parse identically post-spec.

The `api_key` field's mandatory-ness SHALL be determined by the resolved **consumer** first, then the provider:

- **CLI / agentic consumer** (the resolved model is driven by a CLI strategy — `claude` / `opencode` / `agy`): `api_key` is **OPTIONAL for every provider**. The CLI self-authenticates from its own login/store, AND a supplied key is passed to the CLI (see the executor "CLI strategies … credential" requirement). Config-load SHALL NOT fail for a missing key on a CLI/agentic role.
- **In-process HTTP consumer** (the non-agentic `oneshot` reviewer OR a RAG/embedding call — the daemon calls the provider directly): the provider rule applies, since an HTTP call needs the key in the daemon's process:
  - `anthropic` → `api_key` REQUIRED (inline `api_key.value` OR `api_key_env` pointing at a set env var). Config-load fails-fast if absent.
  - `openai_compatible` → `api_key` REQUIRED. Same fail-fast rule.
  - `ollama` → `api_key` FORBIDDEN (Ollama does not authenticate). Config-load fails-fast if one is set, with `<subsystem>: ollama does not authenticate; remove api_key field`. (For a CLI/agentic ollama role the key is simply optional AND ignored — Ollama has no auth to use it.)

The `api_base_url` field's mandatory-ness SHALL be provider-driven:

- `anthropic` → OPTIONAL (defaults to `https://api.anthropic.com`).
- `openai_compatible` → REQUIRED (no sensible default for a generic compat endpoint).
- `ollama` → REQUIRED (operator's Ollama host).

The `api_base_url` SHALL be treated as the API root by every provider's client. Each client knows what protocol-specific path to append:

- `anthropic` → `<base>/v1/messages`.
- `openai_compatible` → `<base>/chat/completions` (for chat) OR `<base>/embeddings` (for embeddings).
- `ollama` → `<base>/api/chat` (for chat) OR `<base>/api/embed` (for embeddings).

Operators using `openai_compatible` against hosted services that require `/v1` in the URL (OpenAI, Grok, OpenRouter) SHALL include `/v1` in their `api_base_url`. The client does NOT auto-append `/v1`; the convention is "operator owns the API root."

Validation runs ONCE at config-load (not lazily). A misconfigured provider surfaces as a fail-fast error at `systemctl restart autocoder`, not as a 404 OR permission error on first feature trigger.

#### Scenario: `LlmProvider` round-trips through serde
- **WHEN** a config file contains `provider: anthropic` (OR `openai_compatible`, OR `ollama`)
- **THEN** the field deserializes into `LlmProvider::Anthropic` (resp. `OpenAiCompatible`, `Ollama`)
- **AND** re-serializing produces the same YAML string

#### Scenario: `RagProvider` AND `ReviewerProvider` aliases compile
- **WHEN** code references the type names `RagProvider` OR `ReviewerProvider`
- **THEN** the names resolve to `LlmProvider` via type aliases
- **AND** no source-code change is required to consumers of the old type names

#### Scenario: A CLI/agentic role does not require `api_key`
- **WHEN** a CLI/agentic role (e.g. a verifier gate or the agentic reviewer) resolves to a model whose `provider` is `anthropic` OR `openai_compatible` AND no `api_key` / `api_key_env` is configured
- **THEN** config-load succeeds
- **AND** no key is required (the CLI self-authenticates at run time)

#### Scenario: `anthropic` requires `api_key` for an in-process HTTP consumer
- **WHEN** an in-process HTTP consumer (the `oneshot` reviewer OR a RAG/embedding call) sets `provider: anthropic` AND omits both `api_key` AND `api_key_env`
- **THEN** config-load fails with `<subsystem>: anthropic requires api_key; set <subsystem>.api_key.value or <subsystem>.api_key_env`
- **AND** the daemon exits non-zero before any polling task is spawned

#### Scenario: `openai_compatible` requires `api_key` for an in-process HTTP consumer
- **WHEN** an in-process HTTP consumer sets `provider: openai_compatible` AND omits both `api_key` AND `api_key_env`
- **THEN** config-load fails with `<subsystem>: openai_compatible requires api_key; set <subsystem>.api_key.value or <subsystem>.api_key_env`

#### Scenario: `openai_compatible` requires `api_base_url`
- **WHEN** a config block sets `provider: openai_compatible` AND omits `api_base_url`
- **THEN** config-load fails with `<subsystem>: openai_compatible requires api_base_url; set the field to e.g. https://api.openai.com/v1`

#### Scenario: `ollama` forbids `api_key` for an in-process HTTP consumer
- **WHEN** an in-process HTTP consumer sets `provider: ollama` AND sets `api_key.value` OR `api_key_env`
- **THEN** config-load fails with `<subsystem>: ollama does not authenticate; remove api_key field`
- **AND** the failure message names that Ollama silently ignores Authorization headers

#### Scenario: `ollama` requires `api_base_url`
- **WHEN** a config block sets `provider: ollama` AND omits `api_base_url`
- **THEN** config-load fails with `<subsystem>: ollama requires api_base_url; set the field to e.g. http://localhost:11434`

#### Scenario: `anthropic` defaults `api_base_url` cleanly
- **WHEN** a config block sets `provider: anthropic`, `api_key.value: <some-key>`, AND omits `api_base_url`
- **THEN** config-load succeeds
- **AND** the resolved `api_base_url` is `https://api.anthropic.com`
