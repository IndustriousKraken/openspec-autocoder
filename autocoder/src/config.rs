use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// A secret value sourced from EITHER an environment variable name (bare
/// YAML string) OR an inline value (`{ value: "..." }` object). Used for
/// any config field that carries a credential.
///
/// Parsing relies on `#[serde(untagged)]`: a YAML string deserializes to
/// `EnvVar(name)`; a YAML mapping with a `value` key deserializes to
/// `Inline { value }`. Any other shape produces a deserialize error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SecretSource {
    /// Bare string: names an environment variable holding the secret.
    EnvVar(String),
    /// `{ value: "..." }`: the secret value itself, verbatim.
    Inline { value: String },
}

impl SecretSource {
    /// Read the secret. For `EnvVar`, reads the named env var and errors if
    /// unset, naming both the env var and the originating config field. For
    /// `Inline`, returns the value verbatim.
    pub fn resolve(&self, field_label: &str) -> Result<String> {
        match self {
            Self::EnvVar(name) => std::env::var(name).map_err(|_| {
                anyhow!("secret env var `{name}` for `{field_label}` is not set")
            }),
            Self::Inline { value } => Ok(value.clone()),
        }
    }

    /// Source description for startup logs. NEVER returns the secret value.
    pub fn describe(&self, field_label: &str) -> String {
        match self {
            Self::EnvVar(name) => format!("env var {name}"),
            Self::Inline { .. } => format!("inline ({field_label})"),
        }
    }

    /// True when this source is an inline value (used to detect "both forms
    /// set" precedence warnings at startup).
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repositories: Vec<RepositoryConfig>,
    pub executor: ExecutorConfig,
    pub github: GithubConfig,
    #[serde(default)]
    pub reviewer: Option<ReviewerConfig>,
    #[serde(default)]
    pub chatops: Option<ChatOpsConfig>,
    /// Optional periodic-audit framework configuration. When the entire
    /// block is absent, every audit's effective cadence is `Disabled` and
    /// the daemon behaves exactly as it did before the framework existed.
    /// Operators opt in explicitly by listing audit type names with a
    /// non-`disabled` cadence under `audits.defaults`. Serialized only when
    /// some audit is enabled so the install wizard's "operator declined all
    /// audits" path produces a YAML file without an empty `audits:` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audits: Option<AuditsConfig>,
    /// Optional explicit overrides for the four daemon data
    /// directories. Each field is optional; absent fields fall through
    /// the resolution priority (`AUTOCODER_*_DIR` env var → systemd
    /// `$STATE_DIRECTORY` family → XDG defaults → hard fallback). An
    /// absent block is equivalent to all fields being `None`.
    #[serde(default, skip_serializing_if = "DaemonPathsConfig::is_empty")]
    pub paths: DaemonPathsConfig,
    /// Optional workspace-cache bounding (a65). Today this carries only
    /// `workspaces_max_gb`, the optional cap on the total size of
    /// `<cache>/workspaces/`. An absent block (the default) is equivalent
    /// to all fields being `None` — the cache is unbounded, as it has
    /// always been. Eligible for the hot-reload subset so a reload applies
    /// a new cap at the next iteration.
    #[serde(default, skip_serializing_if = "CacheConfig::is_empty")]
    pub cache: CacheConfig,
    /// Optional per-workspace feature flags. Each sub-block is opt-in;
    /// absent fields take their type-default. Today this block carries
    /// only the `brownfield` toggle (a23); future per-workspace
    /// feature flags land here so the schema scales without sprinkling
    /// one-off top-level keys.
    #[serde(default, skip_serializing_if = "FeaturesConfig::is_default")]
    pub features: FeaturesConfig,
    /// Optional canonical-spec RAG (retrieval-augmented context) block.
    /// Absent → feature disabled (no embed calls, MCP tool returns empty
    /// Vec with `rag disabled in config` hint). Present with
    /// `enabled: false` → also disabled, but the block is preserved for
    /// documentation purposes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_rag: Option<CanonicalRagConfig>,
    /// Optional top-level model registry (a55). Maps a nickname to a model
    /// definition carrying the same four LLM fields the per-subsystem
    /// blocks use (`provider`, `model`, `api_base_url`, `api_key`/
    /// `api_key_env`) plus an optional `cli` override. Any LLM-consuming
    /// block that OMITS its inline `provider` has its `model` field
    /// resolved against this registry at config-load. A `BTreeMap` (not
    /// `HashMap`) so iteration order is deterministic for diagnostics.
    /// Absent → no registry; every block must be inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<BTreeMap<String, ModelEntry>>,
}

/// A single entry in the top-level `models:` registry (a55). Carries the
/// same `(provider, model, api_base_url, api_key/api_key_env)` tuple every
/// LLM-consuming block uses, plus an optional `cli` override that selects
/// the agentic CLI for this model (see [`ModelEntry::resolved_cli`]).
///
/// Unlike the per-subsystem blocks, `provider` is REQUIRED here — a
/// registry entry always declares its provider; the nickname-reference
/// shorthand lives on the referencing block, not on the entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    pub provider: LlmProvider,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliKind>,
}

impl ModelEntry {
    /// The agentic CLI that drives this model: the explicit `cli` override
    /// when set, else the provider's default ([`default_cli_for`]).
    ///
    /// Consumed by the agentic-run primitive (a later change); no
    /// production call site exists in this change.
    #[allow(dead_code)]
    pub fn resolved_cli(&self) -> CliKind {
        self.cli.unwrap_or_else(|| default_cli_for(self.provider))
    }
}

/// The agentic CLI a model is driven through (a55). The CLI strategies
/// themselves land with the agentic-run primitive (a later change); this
/// enum and [`default_cli_for`] define the `provider → CLI` rule so model
/// selection and CLI selection are specified in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliKind {
    /// Anthropic's `claude` CLI.
    Claude,
    /// The provider-agnostic `opencode` CLI.
    Opencode,
    /// Google's Antigravity CLI (`agy`), successor to the sunset Gemini CLI
    /// (a69). Drives Google/Gemini-family models agentically.
    Antigravity,
}

impl CliKind {
    /// Every registered CLI kind. The OS-sandbox credential layers (a006)
    /// iterate this so the protected config-store set grows automatically as
    /// strategies are added — never a hardcoded literal list (task 5.2).
    pub const ALL: [CliKind; 3] = [CliKind::Claude, CliKind::Opencode, CliKind::Antigravity];

    /// Operator-facing YAML string. Matches the `#[serde]` rename rules
    /// (`cli: claude` / `cli: opencode` / `cli: antigravity`). NOT necessarily
    /// the binary name — see [`CliKind::default_command`] (the Antigravity CLI
    /// is configured as `antigravity` but the binary is `agy`). Kept as the
    /// type's operator-facing accessor / serde-parity mirror even when no
    /// direct call site exists.
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Antigravity => "antigravity",
        }
    }

    /// The default binary name for this CLI on `PATH`. For `claude`/`opencode`
    /// this matches [`as_str`](Self::as_str); the Antigravity CLI ships as the
    /// `agy` binary even though it is selected with `cli: antigravity` (a69).
    /// Used by the startup dependency preflight to probe the model registry's
    /// driving CLIs.
    pub fn default_command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Antigravity => "agy",
        }
    }
}

/// The default agentic CLI for a provider (a55): `anthropic` is driven by
/// the `claude` CLI; `openai_compatible` AND `ollama` are driven by the
/// provider-agnostic `opencode` CLI. A registry entry's optional `cli`
/// field overrides this default (see [`ModelEntry::resolved_cli`]).
///
/// Defined here, consumed later: the CLI strategies that read this rule
/// land with the agentic-run primitive (a later change in the stream), so
/// no production call site exists yet.
#[allow(dead_code)]
pub fn default_cli_for(provider: LlmProvider) -> CliKind {
    match provider {
        LlmProvider::Anthropic => CliKind::Claude,
        LlmProvider::OpenAiCompatible | LlmProvider::Ollama => CliKind::Opencode,
        // a69: the Google/Antigravity provider is driven by the `agy` CLI
        // (Antigravity), the successor to the sunset Gemini CLI.
        LlmProvider::Google => CliKind::Antigravity,
    }
}

/// The binary a role should spawn for its resolved `cli`. The `configured`
/// command — `executor.command` for the gates/audits, the reviewer's `command`
/// — defaults to (and the daemon's only executor is) the `claude` binary. That
/// is correct ONLY for a claude role: an `opencode` / `agy` strategy cannot run
/// the `claude` binary (it would invoke claude with foreign flags → claude fails
/// to authenticate / never submits, which the gates then hold on). So a
/// non-claude CLI uses its OWN default binary ([`CliKind::default_command`],
/// resolved on `PATH`); a claude role keeps the configured command (honoring a
/// custom claude path). There is no per-CLI command override for the non-claude
/// CLIs — they use the standard binary name.
pub fn resolve_cli_command(configured: &str, cli: CliKind) -> String {
    if cli == CliKind::Claude {
        configured.to_string()
    } else {
        cli.default_command().to_string()
    }
}

/// Canonical-spec RAG configuration (a21).
///
/// When `Some` with `enabled: true`, the daemon embeds every
/// `openspec/specs/<capability>/spec.md` at workspace init and re-embeds
/// affected capabilities after archives that touch canonical specs. The
/// per-execution MCP child exposes `query_canonical_specs` to the
/// implementer, which relays to the daemon via the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalRagConfig {
    #[serde(default)]
    pub enabled: bool,
    /// LLM provider. OPTIONAL (a55): when omitted, `model` is interpreted
    /// as a top-level `models:` nickname and the provider (plus the rest
    /// of the tuple) is resolved from the registry at config-load. When
    /// present, the block is the legacy inline form and the registry is
    /// not consulted. Always `Some` after [`Config::load_from`] resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<RagProvider>,
    pub model: String,
    /// API root. Defaulted (a55) so a nickname-reference block — which
    /// carries only `model` — deserializes; the real value is populated
    /// from the registry entry at config-load. An inline block that omits
    /// it deserializes to empty AND fails per-provider validation (which
    /// requires a base URL for both valid RAG providers).
    #[serde(default)]
    pub api_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretSource>,
    #[serde(default = "default_rag_top_k")]
    pub top_k: usize,
    #[serde(default)]
    pub chunk_strategy: ChunkStrategy,
    #[serde(default = "default_reembed_on_archive")]
    pub reembed_on_archive: bool,
}

/// Canonical LLM-provider enum. Single enum referenced by every
/// LLM-touching config block (`reviewer:`, `canonical_rag:`,
/// `executor.change_internal_contradiction_check_llm:`). Per-subsystem
/// validity (e.g. anthropic-for-RAG rejection) AND per-provider auth
/// semantics (e.g. ollama-forbids-api-key) are enforced at config-load
/// by [`validate_llm_provider_config`] AND
/// [`validate_provider_for_subsystem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProvider {
    /// Anthropic's hosted API (`https://api.anthropic.com` default).
    /// Valid for completion subsystems (reviewer, contradiction-check).
    /// INVALID for canonical_rag (Anthropic exposes no embeddings API).
    Anthropic,
    /// Any OpenAI-API-shaped endpoint (OpenAI itself, Grok, OpenRouter,
    /// vLLM, local OpenAI-compat shims). Valid for every subsystem.
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    /// Ollama native API (`<base>/api/chat` for completion,
    /// `<base>/api/embed` for embeddings). Valid for every subsystem;
    /// does not authenticate (api_key forbidden).
    Ollama,
    /// Google's Gemini-family models, driven agentically through the
    /// Antigravity CLI (`agy`) — the successor to the sunset Gemini CLI
    /// (a69). This is a CLI-only (agentic) provider: it has NO in-process
    /// HTTP client (the `oneshot` reviewer / RAG embedding paths reject it)
    /// and NO embeddings API. `agy` authenticates from its own OAuth login /
    /// credential store; an optional `api_key` becomes the `AV_API_KEY`
    /// auth env the strategy sets. Valid for the agentic completion
    /// subsystems (reviewer, contradiction checks, code-implements-spec);
    /// INVALID for canonical_rag.
    Google,
}

impl LlmProvider {
    /// Operator-facing YAML string. Matches the `#[serde]` rename rules.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiCompatible => "openai_compatible",
            Self::Ollama => "ollama",
            Self::Google => "google",
        }
    }
}

/// Backward-compatible alias. The pre-spec `RagProvider` was a 2-variant
/// enum (`ollama | openai_compatible`); the alias preserves source-code
/// compatibility for callers that imported the type name. Per-subsystem
/// validity (RAG forbids `anthropic`) is enforced at config-load, not at
/// the type level.
pub type RagProvider = LlmProvider;

/// Backward-compatible alias for the pre-spec `ReviewerProvider` (which
/// was 2-variant: `anthropic | openai_compatible`). The reviewer now
/// accepts `ollama` too; validity is enforced at config-load.
pub type ReviewerProvider = LlmProvider;

/// Per-subsystem identity used by [`validate_provider_for_subsystem`].
/// Different subsystems have different supported provider sets; this
/// enum names which subsystem is being checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubsystemKind {
    /// AI code-quality review (`reviewer:`). All three providers valid.
    Reviewer,
    /// Canonical-spec RAG embedding pipeline (`canonical_rag:`).
    /// `anthropic` is INVALID — Anthropic does not expose embeddings.
    CanonicalRag,
    /// Change-internal contradiction check (`executor.change_internal_contradiction_check_llm:`).
    /// All three providers valid.
    ContradictionCheck,
    /// Change-vs-canonical contradiction check — the `[canon]` gate
    /// (`executor.change_canonical_contradiction_check_llm:`, a62). All three
    /// providers valid.
    CanonContradictionCheck,
    /// Code-implements-spec verification — the `[out]` gate
    /// (`executor.code_implements_spec_check_llm:`, a63). All three providers
    /// valid.
    CodeImplementsSpecCheck,
}

impl SubsystemKind {
    fn config_label(self) -> &'static str {
        match self {
            Self::Reviewer => "reviewer",
            Self::CanonicalRag => "canonical_rag",
            Self::ContradictionCheck => "change_internal_contradiction_check_llm",
            Self::CanonContradictionCheck => "change_canonical_contradiction_check_llm",
            Self::CodeImplementsSpecCheck => "code_implements_spec_check_llm",
        }
    }

    fn valid_providers(self) -> &'static [LlmProvider] {
        match self {
            Self::Reviewer
            | Self::ContradictionCheck
            | Self::CanonContradictionCheck
            | Self::CodeImplementsSpecCheck => &[
                LlmProvider::Anthropic,
                LlmProvider::OpenAiCompatible,
                LlmProvider::Ollama,
                // a69: Google/Antigravity runs these roles agentically via the
                // `agy` CLI (the `oneshot` in-process path rejects it).
                LlmProvider::Google,
            ],
            // canonical_rag is embeddings-only; Google/Antigravity exposes no
            // embeddings API to autocoder, so it stays excluded (alongside
            // anthropic).
            Self::CanonicalRag => &[LlmProvider::Ollama, LlmProvider::OpenAiCompatible],
        }
    }
}

/// Per-provider auth + base-URL validation. Called by each subsystem's
/// config-load helper with the subsystem's name (used verbatim in error
/// messages). Validation rules:
///
/// - `Anthropic`: `api_key` REQUIRED (inline or env-var name). `api_base_url`
///   OPTIONAL (defaults to `https://api.anthropic.com`).
/// - `OpenAiCompatible`: `api_key` REQUIRED. `api_base_url` REQUIRED.
/// - `Ollama`: `api_key` FORBIDDEN. `api_base_url` REQUIRED.
///
/// `api_key_present` is `true` when EITHER `api_key` (inline `SecretSource`)
/// OR `api_key_env` (env var name) is set; callers pass this rolled-up
/// flag so the validator does not need to know the per-subsystem field
/// names. Error messages name the `subsystem` AND the offending field so
/// the operator can locate the YAML quickly.
pub fn validate_llm_provider_config(
    provider: LlmProvider,
    api_key_present: bool,
    api_base_url: Option<&str>,
    subsystem: &str,
) -> Result<()> {
    // In-process HTTP consumer (oneshot reviewer, RAG/embedding): the daemon
    // calls the provider directly, so a key is genuinely required.
    validate_llm_provider_config_inner(provider, api_key_present, api_base_url, subsystem, true)
}

/// Like [`validate_llm_provider_config`] but for a **CLI / agentic** consumer
/// (a model driven by `claude` / `opencode` / `agy`). The CLI self-authenticates
/// from its own login/store, AND a supplied key is passed to the CLI (see the
/// executor credential requirement), so `api_key` is OPTIONAL for every provider
/// — including ollama, where a key is simply ignored rather than forbidden. The
/// `api_base_url` requirements are unchanged (a base URL is not a credential).
pub fn validate_llm_provider_config_cli(
    provider: LlmProvider,
    api_key_present: bool,
    api_base_url: Option<&str>,
    subsystem: &str,
) -> Result<()> {
    validate_llm_provider_config_inner(provider, api_key_present, api_base_url, subsystem, false)
}

fn validate_llm_provider_config_inner(
    provider: LlmProvider,
    api_key_present: bool,
    api_base_url: Option<&str>,
    subsystem: &str,
    require_key: bool,
) -> Result<()> {
    let has_base = api_base_url.map(|s| !s.trim().is_empty()).unwrap_or(false);
    match provider {
        LlmProvider::Anthropic => {
            if require_key && !api_key_present {
                return Err(anyhow!(
                    "{subsystem}: anthropic requires api_key; set {subsystem}.api_key.value or {subsystem}.api_key_env"
                ));
            }
        }
        LlmProvider::OpenAiCompatible => {
            if require_key && !api_key_present {
                return Err(anyhow!(
                    "{subsystem}: openai_compatible requires api_key; set {subsystem}.api_key.value or {subsystem}.api_key_env"
                ));
            }
            if !has_base {
                return Err(anyhow!(
                    "{subsystem}: openai_compatible requires api_base_url; set the field to e.g. https://api.openai.com/v1"
                ));
            }
        }
        LlmProvider::Ollama => {
            // Ollama never authenticates. For an in-process HTTP consumer a key
            // is a footgun (rejected). For a CLI/agentic consumer a key is
            // optional AND ignored — no forbid — so the rule is uniform.
            if require_key && api_key_present {
                return Err(anyhow!(
                    "{subsystem}: ollama does not authenticate; remove api_key field (Ollama silently ignores Authorization headers, so a configured key is a footgun)"
                ));
            }
            if !has_base {
                return Err(anyhow!(
                    "{subsystem}: ollama requires api_base_url; set the field to e.g. http://localhost:11434"
                ));
            }
        }
        LlmProvider::Google => {
            // a69: the Google/Antigravity provider is CLI-only (agentic). `agy`
            // authenticates from its own OAuth login / credential store; an
            // optional `api_key` (→ `AV_API_KEY`) and `api_base_url` are
            // permitted but not required. No in-process HTTP requirements, so
            // no fields are mandatory here.
            let _ = (api_key_present, has_base);
        }
    }
    Ok(())
}

/// Per-subsystem provider-validity check. Each subsystem has a
/// per-subsystem supported provider set ([`SubsystemKind::valid_providers`]);
/// providers outside that set fail config-load with a fully-specified
/// message naming the rejected provider AND the valid alternatives.
pub fn validate_provider_for_subsystem(
    provider: LlmProvider,
    subsystem: SubsystemKind,
) -> Result<()> {
    let valid = subsystem.valid_providers();
    if valid.contains(&provider) {
        return Ok(());
    }
    let list = valid
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(anyhow!(
        "{} does not support provider '{}'; available providers: {}",
        subsystem.config_label(),
        provider.as_str(),
        list
    ))
}

/// The `(provider, model, api_base_url, api_key/api_key_env)` tuple a
/// nickname-reference LLM block inherits from its `models:` registry entry
/// (a55). Returned by [`resolve_model_reference`] for the caller to write
/// back into the block's fields.
struct ResolvedModelFields {
    provider: LlmProvider,
    model: String,
    api_base_url: Option<String>,
    api_key: Option<SecretSource>,
    api_key_env: Option<String>,
}

/// Resolve a possibly-nickname LLM block against the top-level `models:`
/// registry (a55). The block's `inline_provider` discriminates the two
/// forms:
///
/// - `Some(_)` — the legacy inline form. The registry is NOT consulted;
///   returns `Ok(None)` so the caller leaves the block's fields untouched.
/// - `None` — `model` is interpreted as a `models:` nickname AND resolved
///   to the entry's full tuple, returned as `Ok(Some(_))`. A nickname that
///   names no registry entry fails config-load with an error naming both
///   the missing nickname AND the referencing block.
fn resolve_model_reference(
    inline_provider: Option<LlmProvider>,
    model: &str,
    models: Option<&BTreeMap<String, ModelEntry>>,
    block_label: &str,
) -> Result<Option<ResolvedModelFields>> {
    if inline_provider.is_some() {
        return Ok(None);
    }
    let entry = models.and_then(|m| m.get(model)).ok_or_else(|| {
        anyhow!(
            "{block_label}: `model: {model}` omits `provider` and names no entry in the \
             top-level `models:` registry (define `models.{model}`, or set \
             `{block_label}.provider` inline)"
        )
    })?;
    Ok(Some(ResolvedModelFields {
        provider: entry.provider,
        model: entry.model.clone(),
        api_base_url: entry.api_base_url.clone(),
        api_key: entry.api_key.clone(),
        api_key_env: entry.api_key_env.clone(),
    }))
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStrategy {
    #[default]
    PerRequirement,
    PerScenario,
    PerCapability,
}

pub fn default_rag_top_k() -> usize {
    10
}

pub fn default_reembed_on_archive() -> bool {
    true
}

/// Lower bound on `canonical_rag.top_k`. Values below clamp up.
pub const RAG_TOP_K_FLOOR: usize = 1;
/// Upper bound on `canonical_rag.top_k`. Values above clamp down with a WARN.
pub const RAG_TOP_K_CEILING: usize = 100;

/// Clamp `top_k` to `[1, 100]`. Returns `(clamped, Option<warn_message>)`.
pub fn clamp_rag_top_k(requested: usize) -> (usize, Option<String>) {
    if requested < RAG_TOP_K_FLOOR {
        let msg = format!(
            "canonical_rag.top_k ({requested}) is below the floor of {RAG_TOP_K_FLOOR}; \
             clamping to {RAG_TOP_K_FLOOR}"
        );
        tracing::warn!("{msg}");
        (RAG_TOP_K_FLOOR, Some(msg))
    } else if requested > RAG_TOP_K_CEILING {
        let msg = format!(
            "canonical_rag.top_k ({requested}) is above the ceiling of {RAG_TOP_K_CEILING}; \
             clamping to {RAG_TOP_K_CEILING}"
        );
        tracing::warn!("{msg}");
        (RAG_TOP_K_CEILING, Some(msg))
    } else {
        (requested, None)
    }
}

impl CanonicalRagConfig {
    /// Resolve the API key for the OpenAI-compatible provider. Inline
    /// `api_key` wins over `api_key_env` with a WARN if both are set
    /// (same pattern as `reviewer:`). Returns `None` if neither is set —
    /// the Ollama provider permits an unset key; the OpenAI-compatible
    /// provider's adapter rejects `None` at build time.
    pub fn resolve_api_key(&self) -> Result<Option<String>> {
        let inline = self
            .api_key
            .as_ref()
            .map(|s| s.is_inline())
            .unwrap_or(false);
        if inline {
            if self.api_key_env.is_some() {
                tracing::warn!(
                    "canonical_rag.api_key (inline) AND canonical_rag.api_key_env both set; \
                     inline value wins, env var ignored"
                );
            }
            return Ok(Some(
                self.api_key
                    .as_ref()
                    .expect("inline path")
                    .resolve("canonical_rag.api_key")?,
            ));
        }
        if let Some(env_name) = self.api_key_env.as_deref() {
            return std::env::var(env_name)
                .map(Some)
                .map_err(|_| anyhow!("canonical_rag.api_key_env `{env_name}` is not set"));
        }
        if let Some(src) = self.api_key.as_ref() {
            return Ok(Some(src.resolve("canonical_rag.api_key")?));
        }
        Ok(None)
    }

    /// `true` when the block is present AND `enabled: true`. The single
    /// gate used by callers to decide whether to invoke the RAG pipeline.
    pub fn is_active(&self) -> bool {
        self.enabled
    }
}

/// Top-level feature-flag block. Each sub-block is opt-in; absent
/// sub-blocks take their type-default behaviour.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeaturesConfig {
    #[serde(default)]
    pub brownfield: BrownfieldFeatureConfig,
    #[serde(default)]
    pub scout: ScoutFeatureConfig,
    #[serde(default)]
    pub brownfield_survey: BrownfieldSurveyFeatureConfig,
    #[serde(default)]
    pub issues: IssuesFeatureConfig,
}

impl FeaturesConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Config for the `brownfield` chatops verb (a23). The verb is enabled
/// per-workspace by default; operators opt out by setting
/// `enabled: false`. The optional `prompt_path` points the brownfield-
/// draft polling handler at a custom prompt template; when unset OR
/// the file does not exist at run time, the handler falls back to the
/// embedded default `prompts/brownfield-draft.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrownfieldFeatureConfig {
    #[serde(default = "default_brownfield_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
}

impl Default for BrownfieldFeatureConfig {
    fn default() -> Self {
        Self {
            enabled: default_brownfield_enabled(),
            prompt_path: None,
        }
    }
}

fn default_brownfield_enabled() -> bool {
    true
}

/// Config for the `scout` chatops verb (a25). The verb is enabled
/// per-workspace by default; operators opt out by setting
/// `enabled: false`. `prompt_path` overrides the embedded scout prompt
/// template per the uniform a24 pattern. `max_items` caps the size of
/// the executor's returned opportunity list (valid range `1..=50`).
/// `include_issues` controls whether the handler attempts a `gh api`
/// fetch for open issues. `staleness_warn_days` is the threshold that
/// triggers the spec-it staleness warning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScoutFeatureConfig {
    #[serde(default = "default_scout_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
    #[serde(default = "default_scout_max_items")]
    pub max_items: usize,
    #[serde(default = "default_scout_include_issues")]
    pub include_issues: bool,
    #[serde(default = "default_scout_staleness_warn_days")]
    pub staleness_warn_days: u64,
}

impl Default for ScoutFeatureConfig {
    fn default() -> Self {
        Self {
            enabled: default_scout_enabled(),
            prompt_path: None,
            max_items: default_scout_max_items(),
            include_issues: default_scout_include_issues(),
            staleness_warn_days: default_scout_staleness_warn_days(),
        }
    }
}

fn default_scout_enabled() -> bool {
    true
}

fn default_scout_max_items() -> usize {
    30
}

fn default_scout_include_issues() -> bool {
    true
}

fn default_scout_staleness_warn_days() -> u64 {
    7
}

/// Valid range for `features.scout.max_items`. Values outside this
/// range fail config-load.
pub const SCOUT_MAX_ITEMS_MIN: usize = 1;
pub const SCOUT_MAX_ITEMS_MAX: usize = 50;

impl ScoutFeatureConfig {
    /// Validate the resolved scout config. Returns `Err(msg)` when
    /// `max_items` is outside the documented range.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_items < SCOUT_MAX_ITEMS_MIN || self.max_items > SCOUT_MAX_ITEMS_MAX {
            return Err(format!(
                "features.scout.max_items ({}) outside valid range {}..={}",
                self.max_items, SCOUT_MAX_ITEMS_MIN, SCOUT_MAX_ITEMS_MAX
            ));
        }
        Ok(())
    }
}

/// Config for the `brownfield-survey` chatops verb (a29). The verb is
/// enabled per-workspace by default; operators opt out by setting
/// `enabled: false`. `prompt_path` overrides the embedded survey
/// prompt template per the uniform a24 pattern. `max_capabilities`
/// caps the size of the executor's returned proposed-capability list
/// (valid range `1..=50`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrownfieldSurveyFeatureConfig {
    #[serde(default = "default_brownfield_survey_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
    #[serde(default = "default_brownfield_survey_max_capabilities")]
    pub max_capabilities: usize,
}

impl Default for BrownfieldSurveyFeatureConfig {
    fn default() -> Self {
        Self {
            enabled: default_brownfield_survey_enabled(),
            prompt_path: None,
            max_capabilities: default_brownfield_survey_max_capabilities(),
        }
    }
}

fn default_brownfield_survey_enabled() -> bool {
    true
}

fn default_brownfield_survey_max_capabilities() -> usize {
    20
}

/// Valid range for `features.brownfield_survey.max_capabilities`.
pub const BROWNFIELD_SURVEY_MAX_CAPABILITIES_MIN: usize = 1;
pub const BROWNFIELD_SURVEY_MAX_CAPABILITIES_MAX: usize = 50;

impl BrownfieldSurveyFeatureConfig {
    /// Validate the resolved brownfield-survey config. Returns
    /// `Err(msg)` when `max_capabilities` is outside the documented
    /// range.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_capabilities < BROWNFIELD_SURVEY_MAX_CAPABILITIES_MIN
            || self.max_capabilities > BROWNFIELD_SURVEY_MAX_CAPABILITIES_MAX
        {
            return Err(format!(
                "features.brownfield_survey.max_capabilities ({}) outside valid range {}..={}",
                self.max_capabilities,
                BROWNFIELD_SURVEY_MAX_CAPABILITIES_MIN,
                BROWNFIELD_SURVEY_MAX_CAPABILITIES_MAX
            ));
        }
        Ok(())
    }
}

/// Config for the issues lane (a009). The lane is gated by this flag,
/// OFF by default — unlike the chatops-verb features above, an enabled
/// issues lane changes the daemon's per-iteration unit selection
/// (`issues > changes > audits`), so it is opt-in. `prompt_path`
/// overrides the embedded issue-flavored implementer prompt template
/// (`prompts/implementer-issue.md`) per the uniform a24 pattern.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssuesFeatureConfig {
    #[serde(default = "default_issues_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
}

impl Default for IssuesFeatureConfig {
    fn default() -> Self {
        Self {
            enabled: default_issues_enabled(),
            prompt_path: None,
        }
    }
}

fn default_issues_enabled() -> bool {
    false
}

/// Modernized nested prompt-override block (a24). Used as the value
/// type for every `<area>.<thing>` field that overrides an embedded
/// prompt template. The single `prompt_path` field is workspace-
/// relative when not absolute; the [`crate::prompts::PromptLoader`]
/// resolves it AND emits a one-shot WARN when the file is missing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromptOverrideBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
}

/// Operator-visible override for the four daemon data paths. Each
/// field is optional; the absent-field path means "use the default
/// resolution chain" (see [`crate::paths::resolve_daemon_paths`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonPathsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_dir: Option<PathBuf>,
}

impl DaemonPathsConfig {
    /// `true` when every field is `None`. Used by the serializer to
    /// suppress empty `paths: {}` blocks from the rendered YAML.
    pub fn is_empty(&self) -> bool {
        self.state_dir.is_none()
            && self.cache_dir.is_none()
            && self.logs_dir.is_none()
            && self.runtime_dir.is_none()
    }
}

/// Workspace-cache bounding config (a65). Optional top-level `cache`
/// block. Today it carries only `workspaces_max_gb`; future cache-tuning
/// knobs land here so the schema scales without one-off top-level keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Optional cap on the TOTAL size of `<cache>/workspaces/`, in
    /// gigabytes. `None` (the default) = unbounded — the daemon never
    /// evicts a workspace, matching pre-a65 behaviour. When set, the
    /// daemon keeps the cache under the cap by evicting least-recently-
    /// used IDLE workspaces at each repo's iteration start (see
    /// `crate::workspace_cache`). A configured `0` is rejected at
    /// config-load (an unbounded cache is expressed by omitting the
    /// field, not by a zero cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces_max_gb: Option<u64>,
}

impl CacheConfig {
    /// `true` when every field is `None`. Used by the serializer to
    /// suppress an empty `cache: {}` block from the rendered YAML.
    pub fn is_empty(&self) -> bool {
        self.workspaces_max_gb.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub url: String,
    #[serde(default)]
    pub local_path: Option<PathBuf>,
    pub base_branch: String,
    pub agent_branch: String,
    pub poll_interval_sec: u64,
    #[serde(default)]
    pub chatops_channel_id: Option<String>,
    /// Per-repo upper bound on the number of archived changes committed
    /// in one iteration's PR. When unset, falls back to
    /// `executor.max_changes_per_pr` and finally to a global default of
    /// `3`. A configured value of `0` is a misconfiguration and is
    /// clamped to `1` with a WARN log at startup. See
    /// `Config::resolved_max_changes_per_pr` for the resolved value.
    #[serde(default)]
    pub max_changes_per_pr: Option<u32>,
    /// Per-repository audit cadence overrides. Keys are audit type names
    /// (matching a registered audit's `audit_type()` slug). Each value
    /// overrides the global `audits.defaults` entry for the same type for
    /// this repository only. Absent → fall back to the global default →
    /// `Disabled`.
    #[serde(default)]
    pub audits: Option<HashMap<String, Cadence>>,
    /// OSS-fork support (a26): when set, canonical specs live in an
    /// external git working tree rather than alongside the code. The
    /// `SpecRoot` resolver consults this field to compose every spec-
    /// path query. Absent → specs live at `<workspace>/openspec/`.
    /// See `docs/OPERATIONS.md` "OSS contribution workflow".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_storage: Option<SpecStorageConfig>,
    /// OSS-fork support (a26): when set, the polling iteration ensures
    /// an `upstream` git remote pointing at the upstream repo AND
    /// opportunistically fetches it at iteration start. This block
    /// enables — but does NOT trigger — automatic upstream syncing;
    /// syncing is operator-initiated via the `sync-upstream` chatops
    /// verb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamConfig>,
    /// OSS-fork support (a26): when `false`, the git-workflow-manager
    /// pushes the agent branch but skips the PR-creation API call,
    /// returning a `BranchPushedNoPr` outcome so the operator can run
    /// `gh pr create` after local review. Defaults to `true`
    /// (preserves existing auto-submit behavior).
    #[serde(default = "default_auto_submit_pr")]
    pub auto_submit_pr: bool,
    /// a006: per-repository override of the credential-protection toggles
    /// (`os_hide`, `engine_deny`). Each set field overrides the global
    /// `executor.sandbox` value for this repository only; unset fields inherit
    /// global, then the secure default (ON). Loosening either is explicit and
    /// logged at startup. See [`RepositoryConfig::resolved_sandbox_toggles`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<RepoSandboxConfig>,
    /// a008: optional per-repo forge-provider selection + configuration.
    /// When present, this block is authoritative for provider selection (see
    /// [`ForgeConfig`] AND `crate::forge::resolve_forge`). Absent → the
    /// provider defaults to GitHub against `github.com`; existing GitHub
    /// configurations need no block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<ForgeConfig>,
}

fn default_auto_submit_pr() -> bool {
    true
}

/// a008: which forge-provider implementation serves a repository's forge
/// operations. Selected by the per-repo [`ForgeConfig::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    /// GitHub (`github.com`) OR a GitHub-Enterprise endpoint (via `api_base`).
    Github,
    /// GitLab SaaS (`gitlab.com`) OR a self-hosted GitLab endpoint.
    Gitlab,
}

/// a008: per-repo `forge:` block. Declares AND configures the forge provider
/// for a repository. When present it is **authoritative** for provider
/// selection (see `crate::forge::resolve_forge`); when absent the provider
/// defaults to GitHub against `github.com`, so existing GitHub configurations
/// need no block. The `api_base` additionally enables GitHub Enterprise
/// (`kind: github` against a self-hosted endpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeConfig {
    /// The provider implementation (`github` | `gitlab`).
    pub kind: ForgeKind,
    /// The forge host (e.g. `gitlab.example.com`). Optional: when omitted the
    /// host is inferred from the repository URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Explicit REST API base (e.g. `https://gitlab.example.com/api/v4`, or a
    /// GitHub-Enterprise `https://ghe.example.com/api/v3`). Optional: when
    /// omitted it is derived from `host`/`kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    /// The provider token, sourced through the existing [`SecretSource`]
    /// mechanism (an inline value OR an env-var name). Optional: when omitted,
    /// `token_env` is consulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<SecretSource>,
    /// Fallback env-var name holding the provider token, used when `token` is
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
}

impl ForgeConfig {
    /// Resolve the provider token through the forge block's token route: the
    /// inline/env `token` [`SecretSource`] when present, else the `token_env`
    /// env var. Errors (naming the field AND, for env-var sources, the env
    /// var) when neither route yields a value. `#[allow(dead_code)]`: the
    /// token-fetch entry point for the Phase-3 GitLab API call path
    /// (validation uses [`ForgeConfig::token_route_resolves`]).
    #[allow(dead_code)]
    pub fn resolve_token(&self) -> Result<String> {
        if let Some(src) = self.token.as_ref() {
            return src.resolve("forge.token");
        }
        if let Some(env) = self.token_env.as_ref() {
            return SecretSource::EnvVar(env.clone())
                .resolve(&format!("forge.token_env={env}"));
        }
        Err(anyhow!(
            "forge block declares no token route: set `forge.token` (an inline value or env-var \
             name) or `forge.token_env`"
        ))
    }

    /// `true` when the forge block's token route can produce a value right
    /// now (inline always resolves; an env-var source resolves iff the env
    /// var is set). Used by config-load token-route validation.
    pub fn token_route_resolves(&self) -> bool {
        if let Some(src) = self.token.as_ref() {
            return matches!(src, SecretSource::Inline { .. })
                || matches!(src, SecretSource::EnvVar(name) if std::env::var(name).is_ok());
        }
        if let Some(env) = self.token_env.as_ref() {
            return std::env::var(env).is_ok();
        }
        false
    }
}

/// OSS-fork support (a26): per-repo spec-storage config. When set,
/// autocoder treats `<path>/openspec/` as the canonical-spec source
/// instead of `<workspace>/openspec/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecStorageConfig {
    /// Workspace-relative OR absolute path to a git working tree
    /// containing an `openspec/` subdirectory.
    pub path: String,
    /// a34: optional override for the git remote in the spec_storage
    /// working tree that spec-only iterations push to. When unset, the
    /// runtime uses `"origin"`. When set, config-load verifies the
    /// remote exists in the spec_storage repo's `git remote` output AND
    /// fails-fast if not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_remote: Option<String>,
    /// a34: optional override for the PR base branch in the spec_storage
    /// repo. When unset, the runtime queries
    /// `git -C <spec_storage.path> symbolic-ref refs/remotes/<push_remote>/HEAD`
    /// AND parses the branch name. On query failure, the documented
    /// fallback is `"main"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

/// OSS-fork support (a26): per-repo upstream-remote config. When set,
/// the polling iteration ensures a remote named `remote` exists
/// pointing at `url` AND opportunistically `git fetch <remote>` at
/// iteration start.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    #[serde(default = "default_upstream_remote")]
    pub remote: String,
    #[serde(default = "default_upstream_branch")]
    pub branch: String,
    pub url: String,
}

fn default_upstream_remote() -> String {
    "upstream".to_string()
}

fn default_upstream_branch() -> String {
    "main".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    pub kind: ExecutorKind,
    /// DEPRECATED: the daemon resolves each CLI by its canonical binary name
    /// (`claude` / `opencode` / `agy`) on the captured login PATH (a014). Put
    /// the binary on the daemon user's PATH, and symlink the canonical name to a
    /// fork/clone (or a flag-injecting wrapper) if needed. Still parsed AND
    /// honored — a set value points the `claude` implementer at that binary — for
    /// backward compatibility, but it is undocumented (omitted from
    /// config.example.yaml AND CONFIG.md per the deprecated-field carve-out).
    #[serde(default = "default_executor_command")]
    pub command: String,
    /// a70: the agentic CLI the implementer runs through. Unset → `claude`
    /// (the default; streaming live-log path, byte-identical to pre-a70). Set
    /// to `opencode` / `antigravity` to run the implementer capture-mode
    /// through that strategy (no live log; outcome + `final_answer` arrive via
    /// the MCP outcome relay). When this selects a non-`claude` CLI AND
    /// `command` is left at its default, the binary defaults to that CLI's own
    /// (`agy` for antigravity, `opencode` for opencode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementer_cli: Option<CliKind>,
    #[serde(default = "default_executor_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub sandbox: Option<ExecutorSandboxConfig>,
    /// a014: capture of the operator's activated login-shell environment +
    /// the credential-exclusion edits + the `doctor` expected-toolchain set.
    /// Unset → capture is ON with the default credential filter and the default
    /// expected-toolchain list.
    #[serde(default)]
    pub agent_env: Option<AgentEnvConfig>,
    /// Optional path to a custom implementer prompt template. When unset,
    /// the binary uses the template embedded at compile time from
    /// `prompts/implementer.md`. The file must contain the literal
    /// `{{change_body}}` placeholder which is replaced with the output of
    /// `openspec instructions apply` for each change.
    ///
    /// **Legacy flat-suffix field.** The modernized nested form is
    /// `executor.implementer.prompt_path` (see [`PromptOverrideBlock`]).
    /// Both forms remain accepted; the loader prefers the nested one
    /// when both are set.
    #[serde(default)]
    pub implementer_prompt_path: Option<PathBuf>,
    /// Optional path to a custom changelog-stylist prompt template. When
    /// unset, the binary uses the template embedded at compile time from
    /// `prompts/changelog-stylist.md`. An empty file at the override path
    /// is rejected at executor-construction time so the daemon does not
    /// feed an empty prompt to the wrapped CLI.
    ///
    /// **Legacy flat-suffix field.** Modernized form is
    /// `executor.changelog_stylist.prompt_path`.
    #[serde(default)]
    pub changelog_stylist_prompt_path: Option<PathBuf>,
    /// Nested override block for the implementer prompt (a24). When set
    /// AND its `prompt_path` file exists, takes precedence over the
    /// legacy flat field `implementer_prompt_path`. Workspace-relative
    /// paths resolve under the repository's local workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementer: Option<PromptOverrideBlock>,
    /// Nested override block for the changelog-stylist prompt (a24).
    /// Modernized form of `changelog_stylist_prompt_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changelog_stylist: Option<PromptOverrideBlock>,
    /// Nested override block for the implementer-revision prompt (a24).
    /// Previously had no operator override at all; now uniformly
    /// configurable via the loader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementer_revision: Option<PromptOverrideBlock>,
    /// Nested override block for the audit-triage prompt (a24, used by
    /// the polling-iteration `send it` flow). Previously had no
    /// operator override at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_triage: Option<PromptOverrideBlock>,
    /// Nested override block for the chat-request-triage prompt (a24,
    /// used by the polling-iteration `propose` flow). Previously had
    /// no operator override at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_request_triage: Option<PromptOverrideBlock>,
    /// Number of consecutive Failed outcomes for a single change before
    /// autocoder marks it perma-stuck (writes `.perma-stuck.json` in the
    /// change directory, posts a chatops alert, and excludes the change
    /// from `list_pending` until the marker is removed manually). When
    /// unset, defaults to 2. A configured value of 0 is a misconfiguration
    /// and is clamped to 1 with a WARN log at startup.
    #[serde(default)]
    pub perma_stuck_after_failures: Option<u32>,
    /// Global default for the per-iteration commit cap. Per-repository
    /// `RepositoryConfig::max_changes_per_pr` takes precedence. When both
    /// are unset, the global default of `3` applies. A configured value
    /// of `0` is clamped to `1` with a WARN log at startup.
    #[serde(default)]
    pub max_changes_per_pr: Option<u32>,
    /// Upper bound (in seconds) on the random sleep each polling task
    /// performs before its first iteration. Each task independently draws
    /// a value uniformly from `[0, startup_jitter_max_secs]` at spawn
    /// time. Staggers a fleet of concurrent `git fetch` operations so an
    /// IDS does not see a synchronized burst. `0` disables the startup
    /// jitter entirely. When unset, the effective default is `30`.
    #[serde(default)]
    pub startup_jitter_max_secs: Option<u64>,
    /// Percent (0..=100) of `poll_interval_sec` used as a uniform random
    /// offset on every inter-iteration sleep. Each task's sleep is drawn
    /// from `[interval - interval*pct/100, interval + interval*pct/100]`.
    /// Prevents long-term re-synchronization of multiple tasks. `0`
    /// produces exact intervals. When unset, the effective default is
    /// `10`. Values above 100 are clamped to 100 (the negative offset
    /// could otherwise exceed the interval and would saturate at zero).
    #[serde(default)]
    pub inter_iteration_jitter_pct: Option<u8>,
    /// Maximum number of AUTOMATIC (reviewer-marked, carrying the
    /// `<!-- reviewer-revision -->` marker) revision rounds applied to a
    /// single open PR before further automatic triggering comments are
    /// silently ignored. Human-initiated `@<bot> revise` comments are NOT
    /// counted against this cap and always process. Default `5`. A value
    /// of `0` disables the revision channel entirely (sites that want to
    /// opt out). Values above `20` are clamped to `20` with a WARN log at
    /// startup so a runaway reviewer-driven chain does not let one PR loop
    /// forever. The legacy key `max_revisions_per_pr` is accepted as a
    /// silent serde alias so existing config files load unchanged.
    #[serde(
        default = "default_max_auto_revisions_per_pr",
        alias = "max_revisions_per_pr"
    )]
    pub max_auto_revisions_per_pr: u32,
    /// a000: per-PR cap on HUMAN-initiated `@<bot> revise` triggers acted
    /// on. Distinct from `max_auto_revisions_per_pr` (which bounds
    /// reviewer-initiated automatic revisions) AND from
    /// `reviewer.max_code_reviews_per_pr` (re-reviews). Closes the
    /// previously-uncapped human-revise path: past this many authorized
    /// human revisions on one PR, further `@<bot> revise` triggers are
    /// declined without invoking the executor. The count is tracked in
    /// the per-PR state file; the cap is read live from config (so a
    /// reload applies to subsequent triggers). Default `10`.
    #[serde(default = "default_max_revise_triggers_per_pr")]
    pub max_revise_triggers_per_pr: u32,
    /// Seconds the `wipe_workspace` control-socket handler waits for the
    /// in-flight per-repo iteration to drain (release its busy marker)
    /// after firing the per-iteration cancel token. The wipe runs
    /// regardless of whether the drain completes within the window —
    /// the directory is going away one way or another; the drain is a
    /// politeness, not a hard precondition. Defaults to `30`. Values
    /// above `WIPE_DRAIN_TIMEOUT_CEILING_SECS` (300, i.e. 5 minutes) are
    /// clamped at startup with a WARN: anything longer is almost
    /// certainly operator misconfiguration and would hold the chatops
    /// listener busy for too long.
    #[serde(default = "default_wipe_drain_timeout_secs")]
    pub wipe_drain_timeout_secs: u64,
    /// Output format for the wrapped Claude CLI. `"json"` (the default)
    /// invokes the CLI with `--output-format stream-json`, runs the
    /// streaming-event parser, and writes the structured log shape
    /// (PROMPT / ACTIONS / FINAL ANSWER / STDERR). `"text"` opts out of
    /// the streaming path entirely and preserves today's at-exit
    /// capture (PROMPT / STDOUT / STDERR sections) — useful when a
    /// custom Claude CLI build lacks the streaming JSON format OR when
    /// debugging the executor itself.
    #[serde(default = "default_output_format")]
    pub output_format: ExecutorOutputFormat,
    /// Per-change run-log retention window (days). At daemon startup
    /// AND once every 24 hours during operation, logs older than
    /// `now - log_retention_days * 86400 seconds` whose corresponding
    /// change directory is no longer in the active path are deleted.
    /// Logs for active changes are preserved regardless of age.
    /// Defaults to `30`. Values above `LOG_RETENTION_DAYS_CEILING`
    /// (365) are clamped down with a WARN log at startup.
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u32,
    /// Stale-threshold (in seconds) for the live-PID busy-marker
    /// recovery branch. The marker classification logic treats any
    /// marker whose recorded PID is alive but older than this value as
    /// a stuck pass and SIGTERMs the process group. A value of `0` is
    /// permitted — every live-PID marker is then considered stale on
    /// inspection (useful for diagnostics). Dead-PID markers are
    /// recovered IMMEDIATELY regardless of this value; this field only
    /// gates the live-PID branch.
    ///
    /// Defaults to `600` (10 minutes). Decoupled from
    /// `executor.timeout_secs` so raising the executor timeout for one
    /// legitimately long-running change does not delay stale-marker
    /// recovery on unrelated iterations. Values above
    /// `BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS` (7200, i.e. 2 hours)
    /// are clamped down with a WARN log at startup.
    ///
    /// `None` means "operator did not set this field" — the daemon's
    /// startup-log code uses that signal to emit a migration-aware
    /// INFO line when the pre-spec implicit threshold
    /// (`timeout_secs + 600`) would have produced a longer value.
    #[serde(default)]
    pub busy_marker_stale_threshold_secs: Option<u64>,
    /// Opt-in gate for the change-internal contradiction pre-flight (a19).
    /// `Disabled` (the default) skips the LLM call entirely. `Enabled`
    /// runs the check AFTER `a17`'s archivability check AND BEFORE the
    /// executor; non-empty findings write `.needs-spec-revision.json`
    /// and halt the queue walk. Enabling without configuring
    /// `change_internal_contradiction_check_llm` is a fail-fast startup
    /// error.
    #[serde(default)]
    pub change_internal_contradiction_check: ContradictionCheckMode,
    /// Optional path to a custom contradiction-check prompt template.
    /// When unset, the binary uses the template embedded at compile time
    /// from `prompts/change-contradiction-check.md`. An empty override
    /// file is rejected at use time so the daemon does not feed an
    /// empty prompt to the LLM.
    #[serde(default)]
    pub change_internal_contradiction_check_prompt_path: Option<PathBuf>,
    /// LLM configuration for the contradiction check. Required when
    /// `change_internal_contradiction_check` is `Enabled`. Held as a
    /// distinct block from `reviewer:` so operators can pick a cheaper
    /// model for the contradiction check (the prompt is small AND the
    /// failure mode is fail-open).
    #[serde(default)]
    pub change_internal_contradiction_check_llm: Option<ContradictionCheckLlmConfig>,
    /// Opt-in gate for the change-vs-canonical contradiction pre-flight —
    /// the `[canon]` gate of the verifier framework (a62). `Disabled` (the
    /// default) skips the LLM call entirely. `Enabled` runs the check
    /// alongside the `[in]` gate, BEFORE the executor; non-empty findings
    /// write `.needs-spec-revision.json` and halt the queue walk. Enabling
    /// without configuring `change_canonical_contradiction_check_llm` is a
    /// fail-fast startup error.
    #[serde(default)]
    pub change_canonical_contradiction_check: ContradictionCheckMode,
    /// Optional path to a custom change-vs-canonical-check prompt template.
    /// When unset, the binary uses the template embedded at compile time
    /// from `prompts/change-vs-canonical-check.md`. An empty override file
    /// is rejected at use time so the daemon does not feed an empty prompt
    /// to the session.
    #[serde(default)]
    pub change_canonical_contradiction_check_prompt_path: Option<PathBuf>,
    /// LLM configuration for the change-vs-canonical check. Required when
    /// `change_canonical_contradiction_check` is `Enabled`. Parallel to the
    /// `[in]` gate's `change_internal_contradiction_check_llm` block so
    /// operators can pick a model independently.
    #[serde(default)]
    pub change_canonical_contradiction_check_llm: Option<ContradictionCheckLlmConfig>,
    /// Opt-in gate for the code-implements-spec verification — the `[out]`
    /// gate of the verifier framework (a63). `Disabled` (the default) skips
    /// the LLM call entirely AND spawns no post-executor session. `Enabled`
    /// runs the check AFTER the executor implements a change, in a read-only
    /// sandbox, AND renders an advisory `## Spec Verification` PR-body section.
    /// The gate NEVER opens a revision AND NEVER blocks PR creation. Enabling
    /// without configuring `code_implements_spec_check_llm` is a fail-fast
    /// startup error.
    #[serde(default)]
    pub code_implements_spec_check: ContradictionCheckMode,
    /// Optional path to a custom code-implements-spec-check prompt template.
    /// When unset, the binary uses the template embedded at compile time from
    /// `prompts/code-implements-spec-check.md`. An empty override file is
    /// rejected at use time so the daemon does not feed an empty prompt to the
    /// session.
    #[serde(default)]
    pub code_implements_spec_check_prompt_path: Option<PathBuf>,
    /// LLM configuration for the code-implements-spec check. Required when
    /// `code_implements_spec_check` is `Enabled`. Parallel to the pre-executor
    /// gates' `*_llm` blocks so operators can pick a model independently.
    #[serde(default)]
    pub code_implements_spec_check_llm: Option<ContradictionCheckLlmConfig>,
}

/// Opt-in gate for the change-internal contradiction pre-flight (a19).
/// Default `Disabled` preserves pre-spec behaviour for operators who do
/// not opt in.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContradictionCheckMode {
    #[default]
    Disabled,
    Enabled,
}

/// LLM configuration block for the contradiction-check pre-flight.
/// Parallel to `ReviewerConfig`'s API-key / provider / model surface but
/// kept as its own type so the contradiction check can evolve
/// independently (cheaper model, different failure mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContradictionCheckLlmConfig {
    /// LLM provider. OPTIONAL (a55): when omitted, `model` names a
    /// top-level `models:` nickname resolved at config-load; when present,
    /// the block is the legacy inline form. Always `Some` after
    /// [`Config::load_from`] resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ReviewerProvider>,
    pub model: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<SecretSource>,
    #[serde(default)]
    pub api_base_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorOutputFormat {
    /// Stream JSON events from the wrapped CLI's stdout, build the
    /// structured per-change log incrementally, and route the final
    /// `result` event's text to the PR comment. Default.
    Json,
    /// Legacy at-exit capture: no JSON streaming, log uses
    /// `=== STDOUT ===` / `=== STDERR ===` sections, PR comment reads
    /// raw stdout. Preserves today's "0-bytes STDOUT on timeout-kill"
    /// behavior.
    Text,
}

pub fn default_output_format() -> ExecutorOutputFormat {
    ExecutorOutputFormat::Json
}

pub fn default_log_retention_days() -> u32 {
    30
}

/// Upper bound on `executor.log_retention_days`. Anything above is
/// clamped down at startup with a WARN log so the operator notices.
pub const LOG_RETENTION_DAYS_CEILING: u32 = 365;

/// Default stale-threshold (seconds) for the live-PID busy-marker
/// recovery branch. 10 minutes is short enough that a live-but-truly-
/// stuck executor doesn't pin a repo for long, but long enough that
/// briefly slow normal work doesn't trip the kill path.
pub fn default_busy_marker_stale_threshold_secs() -> u64 {
    600
}

/// Upper bound on `executor.busy_marker_stale_threshold_secs`. Values
/// above are clamped down at startup with a WARN log so an operator
/// raising the threshold to "forever" notices the cap.
pub const BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS: u64 = 7200;

/// Clamp the configured busy-marker stale threshold. Values above
/// `BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS` are clamped down to the
/// ceiling AND a `tracing::warn!` is emitted naming both the
/// requested and clamped values. Returns `(clamped_value,
/// Option<warn_message>)` so callers (in particular
/// `Config::load_from` and the unit tests) can observe whether a
/// WARN was issued without scraping the tracing log. A value of `0`
/// is permitted and passes through unchanged — useful for diagnostics
/// where the operator wants every live-PID marker treated as stale on
/// inspection.
pub fn clamp_busy_marker_stale_threshold_secs(requested: u64) -> (u64, Option<String>) {
    if requested > BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS {
        let msg = format!(
            "executor.busy_marker_stale_threshold_secs ({requested}) is above the ceiling of \
             {BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS}; clamping to \
             {BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS}"
        );
        tracing::warn!("{msg}");
        (BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS, Some(msg))
    } else {
        (requested, None)
    }
}

/// Shape of the busy-marker stale-threshold startup INFO line.
/// Returned by [`busy_marker_threshold_startup_log`] so the daemon's
/// boot path can emit ONE log line per startup that names both the
/// resolved values AND, when applicable, the gap from the pre-spec
/// implicit threshold (`timeout_secs + 600`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusyMarkerThresholdStartupLog {
    /// Operator did NOT set `executor.busy_marker_stale_threshold_secs`
    /// AND the pre-spec implicit formula would have produced a longer
    /// threshold. Surfaces the gap so operators upgrading from the
    /// pre-spec build see the change without reading release notes.
    Migration {
        new_threshold_secs: u64,
        pre_spec_implicit_threshold_secs: u64,
        timeout_secs: u64,
    },
    /// Operator set the field explicitly OR the implicit threshold did
    /// not exceed the new resolved value (e.g. `timeout_secs = 0`).
    /// One INFO line naming both resolved values.
    Regular {
        timeout_secs: u64,
        busy_marker_stale_threshold_secs: u64,
    },
}

/// Decide which startup INFO line to emit for the busy-marker stale
/// threshold. Pure function — no side effects, no logging — so the
/// shape is unit-testable. Callers emit the actual `tracing::info!`
/// call.
///
/// `explicit_configured` is `Some(_)` iff the operator set
/// `executor.busy_marker_stale_threshold_secs` in YAML (even to the
/// default value). `resolved_threshold_secs` is what the daemon will
/// actually use (post-clamp); `timeout_secs` is the resolved
/// `executor.timeout_secs`.
pub fn busy_marker_threshold_startup_log(
    explicit_configured: Option<u64>,
    resolved_threshold_secs: u64,
    timeout_secs: u64,
) -> BusyMarkerThresholdStartupLog {
    let pre_spec_implicit = timeout_secs.saturating_add(600);
    if explicit_configured.is_none() && resolved_threshold_secs < pre_spec_implicit {
        BusyMarkerThresholdStartupLog::Migration {
            new_threshold_secs: resolved_threshold_secs,
            pre_spec_implicit_threshold_secs: pre_spec_implicit,
            timeout_secs,
        }
    } else {
        BusyMarkerThresholdStartupLog::Regular {
            timeout_secs,
            busy_marker_stale_threshold_secs: resolved_threshold_secs,
        }
    }
}

/// Decide the one-time startup log for the workspace-cache cap (a65).
/// Pure function — no side effects, no logging — so the decision is
/// unit-testable. The caller emits the actual `tracing` call.
///
/// `workspaces_max_gb` is the resolved `cache.workspaces_max_gb`. When
/// unset (`None`), the daemon returns a `Some(message)` nudging the
/// operator that the cache is unbounded AND naming the field that bounds
/// it. When set, returns `None` — a bounded cache needs no warning.
pub fn workspace_cache_unbounded_notice(workspaces_max_gb: Option<u64>) -> Option<String> {
    if workspaces_max_gb.is_none() {
        Some(
            "workspace cache is UNBOUNDED — per-repo workspaces under \
             <cache>/workspaces/ accumulate build artifacts with no size \
             cap and can fill the disk. Set `cache.workspaces_max_gb` to \
             bound the cache (least-recently-used idle workspaces are then \
             evicted to stay under the cap)."
                .to_string(),
        )
    } else {
        None
    }
}

/// Clamp the configured log-retention window. Values above
/// `LOG_RETENTION_DAYS_CEILING` are clamped down to the ceiling AND
/// a `tracing::warn!` is emitted naming both the requested and
/// clamped values. Returns `(clamped_value, Option<warn_message>)` so
/// callers can observe whether a WARN was issued without scraping
/// the tracing log.
pub fn clamp_log_retention_days(requested: u32) -> (u32, Option<String>) {
    if requested > LOG_RETENTION_DAYS_CEILING {
        let msg = format!(
            "executor.log_retention_days ({requested}) is above the ceiling of \
             {LOG_RETENTION_DAYS_CEILING}; clamping to {LOG_RETENTION_DAYS_CEILING}"
        );
        tracing::warn!("{msg}");
        (LOG_RETENTION_DAYS_CEILING, Some(msg))
    } else {
        (requested, None)
    }
}

/// Default seconds the wipe-workspace handler waits for the per-iteration
/// drain after firing the cancel token.
pub fn default_wipe_drain_timeout_secs() -> u64 {
    30
}

/// Upper bound on `executor.wipe_drain_timeout_secs`. Anything above is
/// clamped down at startup with a WARN log so the operator notices.
pub const WIPE_DRAIN_TIMEOUT_CEILING_SECS: u64 = 300;

/// Upper bound on `executor.max_auto_revisions_per_pr`. Anything above
/// this is clamped down at startup with a WARN log so the operator
/// notices.
pub const MAX_AUTO_REVISIONS_PER_PR_CEILING: u32 = 20;

fn default_max_auto_revisions_per_pr() -> u32 {
    5
}

/// Default per-PR cap on human-initiated `@<bot> revise` triggers (a000).
fn default_max_revise_triggers_per_pr() -> u32 {
    10
}

impl ExecutorConfig {
    /// Effective perma-stuck threshold. `None` → 2 (the default). Any
    /// configured value is clamped to `>=1` so the agent always gets at
    /// least one attempt. Callers that want the raw configured value
    /// (e.g. to warn about a zero) read `perma_stuck_after_failures`
    /// directly.
    pub fn perma_stuck_threshold(&self) -> u32 {
        self.perma_stuck_after_failures.unwrap_or(2).max(1)
    }

    /// Effective startup jitter ceiling (seconds). Unset → `30`.
    pub fn startup_jitter_max_secs(&self) -> u64 {
        self.startup_jitter_max_secs.unwrap_or(30)
    }

    /// Effective inter-iteration jitter percentage. Unset → `10`. Clamped
    /// to `100` so a negative offset cannot exceed the base interval (the
    /// arithmetic would otherwise saturate at zero and waste resolution).
    pub fn inter_iteration_jitter_pct(&self) -> u8 {
        self.inter_iteration_jitter_pct.unwrap_or(10).min(100)
    }

    /// Effective per-PR automatic-revision cap. Raw configured values
    /// above `MAX_AUTO_REVISIONS_PER_PR_CEILING` are clamped down to it;
    /// callers that want to detect-and-warn about the original value read
    /// `self.max_auto_revisions_per_pr` directly first.
    pub fn max_auto_revisions_per_pr_clamped(&self) -> u32 {
        self.max_auto_revisions_per_pr
            .min(MAX_AUTO_REVISIONS_PER_PR_CEILING)
    }

    /// Effective wipe-workspace drain timeout (seconds). Values above
    /// `WIPE_DRAIN_TIMEOUT_CEILING_SECS` are clamped down so a runaway
    /// config can't pin the chatops listener busy for longer than 5
    /// minutes on a single wipe. Operators wanting to detect the clamp
    /// at startup read `self.wipe_drain_timeout_secs` directly first.
    pub fn wipe_drain_timeout_secs_clamped(&self) -> u64 {
        self.wipe_drain_timeout_secs
            .min(WIPE_DRAIN_TIMEOUT_CEILING_SECS)
    }

    /// Effective busy-marker stale threshold (seconds). `None` →
    /// `default_busy_marker_stale_threshold_secs()` (600). Configured
    /// values above `BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS` are
    /// clamped down so a runaway operator config doesn't disable
    /// stuck-pass recovery entirely. The raw stored field is preserved
    /// (so the startup-log code can detect "operator did not set this
    /// field" via `Option::is_none`).
    pub fn busy_marker_stale_threshold_secs(&self) -> u64 {
        self.busy_marker_stale_threshold_secs
            .unwrap_or_else(default_busy_marker_stale_threshold_secs)
            .min(BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS)
    }
}

/// Clamp the configured wipe-workspace drain timeout. Values above
/// `WIPE_DRAIN_TIMEOUT_CEILING_SECS` are clamped down to the ceiling
/// AND a `tracing::warn!` is emitted naming both the requested and
/// clamped values. Returns `(clamped_value, Option<warn_message>)` so
/// callers (in particular `Config::load_from` and the unit tests) can
/// observe whether a WARN was issued without having to scrape the
/// tracing log.
pub fn clamp_wipe_drain_timeout_secs(requested: u64) -> (u64, Option<String>) {
    if requested > WIPE_DRAIN_TIMEOUT_CEILING_SECS {
        let msg = format!(
            "executor.wipe_drain_timeout_secs ({requested}) is above the ceiling of \
             {WIPE_DRAIN_TIMEOUT_CEILING_SECS}; clamping to {WIPE_DRAIN_TIMEOUT_CEILING_SECS}"
        );
        tracing::warn!("{msg}");
        (WIPE_DRAIN_TIMEOUT_CEILING_SECS, Some(msg))
    } else {
        (requested, None)
    }
}

/// a014: the operator's activated-toolchain environment capture, the
/// credential-exclusion edits, and the `doctor` runnability set. All fields are
/// optional; an absent block (or absent field) keeps the secure defaults
/// (capture ON, the default credential filter, the default expected-toolchain
/// list).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEnvConfig {
    /// Whether to capture the operator's login-shell environment at startup and
    /// inject it into every agentic subprocess. Defaults ON (`true`); set to
    /// `false` to run agentic subprocesses against the daemon's base
    /// environment only.
    #[serde(default)]
    pub capture: Option<bool>,
    /// Additional credential-pattern entries to EXCLUDE from the captured
    /// environment, on top of the defaults (`TOKEN`/`SECRET`/`KEY`/`PASSWORD`
    /// substrings, `AWS_`/`ANTHROPIC_` prefixes). An entry ending in `_` is a
    /// name PREFIX (e.g. `GCP_`); otherwise a case-insensitive substring.
    /// Mirrors `a013`'s `mask_add`.
    #[serde(default)]
    pub exclude_add: Option<Vec<String>>,
    /// Default credential-pattern entries to REMOVE (so a name matching only
    /// that pattern can propagate) — e.g. `KEY` to admit a `*_KEY` toolchain
    /// variable. An explicit relaxed posture. Mirrors `a013`'s `mask_remove`.
    #[serde(default)]
    pub exclude_remove: Option<Vec<String>>,
    /// The expected-toolchain set the `doctor` runnability check probes in the
    /// agent's actual environment (`<tool> --version`). Unset → the default
    /// common list (`python3`, `node`, `ruby`, `go`).
    #[serde(default)]
    pub expected_toolchains: Option<Vec<String>>,
}

impl AgentEnvConfig {
    /// Whether login-shell environment capture is enabled (defaults ON).
    pub fn capture_enabled(&self) -> bool {
        self.capture.unwrap_or(true)
    }

    /// The expected-toolchain set for the `doctor` runnability check: the
    /// operator's list when configured, else the default common list.
    pub fn expected_toolchains(&self) -> Vec<String> {
        self.expected_toolchains.clone().unwrap_or_else(|| {
            crate::agent_env::DEFAULT_EXPECTED_TOOLCHAINS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
    }
}

/// Per-iteration tool-use restrictions for the wrapped agent CLI. When
/// absent, restrictive safe defaults apply (see `default_allowed_tools`,
/// `default_disallowed_bash_patterns`, `default_disallowed_read_paths`).
/// Each field can be overridden independently; omitted fields keep their
/// safe defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorSandboxConfig {
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_bash_patterns: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_read_paths: Option<Vec<String>>,
    /// a006: hide every CLI strategy's config store EXCEPT the running role's
    /// own from the OS-level sandbox namespace (the filesystem allowlist).
    /// Defaults ON. A per-repository value overrides this global one. See
    /// [`crate::sandbox`].
    #[serde(default)]
    pub os_hide: Option<bool>,
    /// a006: extend the per-invocation tool-use denylist to deny the agent's
    /// `Read`/`Bash` tools on EVERY registered CLI store (the self-store
    /// included). Defaults ON. A per-repository value overrides this global
    /// one.
    #[serde(default)]
    pub engine_deny: Option<bool>,
    /// a006: when NO OS sandbox mechanism (`systemd-run` / `bwrap` /
    /// `sandbox-exec`) can apply the sandbox, agentic runs fail closed UNLESS
    /// this is `true` — the operator's explicit opt-in to running subprocesses
    /// unsandboxed (logged loudly at startup). Daemon-wide; not a
    /// per-repository toggle.
    #[serde(default)]
    pub allow_unsandboxed: bool,
    /// a013: additional filesystem paths to MASK for the executor under its
    /// exposed-home denylist policy, on top of the default mask-list. A leading
    /// `~/` or `$HOME/` expands to the home directory. Per-repository
    /// `mask_add` entries are appended to these.
    #[serde(default)]
    pub mask_add: Option<Vec<String>>,
    /// a013: default mask-list entries to REMOVE (expose) for the executor —
    /// e.g. `~/.ssh` to develop an SSH tool. Removing a default is an explicit
    /// relaxed posture, logged at startup. Per-repository `mask_remove` entries
    /// are appended to these.
    #[serde(default)]
    pub mask_remove: Option<Vec<String>>,
    /// a013: run the executor under the read-only-role allowlist (home masked;
    /// only the workspace read-write, the role's own store, the resolved CLI
    /// binary + toolchain, and the minimal runtime bound) for high-compliance
    /// hosts. Defaults OFF — the executor uses the exposed-home denylist.
    /// A per-repository value overrides this global one.
    #[serde(default)]
    pub strict_mode: Option<bool>,
}

/// Per-repository override of the a006 credential-protection toggles. Each
/// field, when set, overrides the global `executor.sandbox` value for that
/// repository; absent fields inherit global, then the secure default (ON).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoSandboxConfig {
    #[serde(default)]
    pub os_hide: Option<bool>,
    #[serde(default)]
    pub engine_deny: Option<bool>,
    /// a013: per-repository additions to the executor's filesystem mask-list,
    /// appended to the global `executor.sandbox.mask_add`.
    #[serde(default)]
    pub mask_add: Option<Vec<String>>,
    /// a013: per-repository default mask-list entries to remove (expose),
    /// appended to the global `executor.sandbox.mask_remove`. Removing a
    /// default is an explicit relaxed posture, logged at startup.
    #[serde(default)]
    pub mask_remove: Option<Vec<String>>,
    /// a013: per-repository override of the executor strict-mode flag.
    #[serde(default)]
    pub strict_mode: Option<bool>,
}

/// The fully-resolved per-repository sandbox posture (a006 credential
/// toggles + a013 mask-list edits + strict mode). The `os_hide`/`engine_deny`
/// toggles default ON (the secure default); `strict_mode` defaults OFF (the
/// executor uses the exposed-home denylist); the mask edit lists default empty.
///
/// Not `Copy` because of the `Vec` mask-edit fields — cloned where it was
/// previously copied (the runtime threading in [`crate::sandbox`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxToggles {
    pub os_hide: bool,
    pub engine_deny: bool,
    /// a013: run the executor under the allowlist (home masked). Read-only
    /// roles always use the allowlist regardless of this flag.
    pub strict_mode: bool,
    /// a013: operator additions to the executor's filesystem mask-list.
    pub mask_add: Vec<String>,
    /// a013: default mask-list entries the operator removed (exposed).
    pub mask_remove: Vec<String>,
}

impl Default for SandboxToggles {
    fn default() -> Self {
        Self {
            os_hide: true,
            engine_deny: true,
            strict_mode: false,
            mask_add: Vec::new(),
            mask_remove: Vec::new(),
        }
    }
}

impl SandboxToggles {
    /// Apply a per-repository override on top of these (global) toggles: each
    /// set boolean field of `repo` wins; unset booleans keep `self`'s value.
    /// The mask-edit lists are ADDITIVE — the repo's `mask_add`/`mask_remove`
    /// are appended to the global ones (a repo can mask more or expose more,
    /// never silently drop a global edit). Used at runtime to resolve the
    /// active repository's effective posture against the daemon-global default
    /// (equivalent to [`RepositoryConfig::resolved_sandbox_toggles`] when
    /// `self` is the global resolution).
    pub fn with_repo_override(&self, repo: Option<&RepoSandboxConfig>) -> SandboxToggles {
        let mut mask_add = self.mask_add.clone();
        let mut mask_remove = self.mask_remove.clone();
        if let Some(r) = repo {
            if let Some(a) = r.mask_add.as_ref() {
                mask_add.extend(a.iter().cloned());
            }
            if let Some(rm) = r.mask_remove.as_ref() {
                mask_remove.extend(rm.iter().cloned());
            }
        }
        SandboxToggles {
            os_hide: repo.and_then(|r| r.os_hide).unwrap_or(self.os_hide),
            engine_deny: repo.and_then(|r| r.engine_deny).unwrap_or(self.engine_deny),
            strict_mode: repo.and_then(|r| r.strict_mode).unwrap_or(self.strict_mode),
            mask_add,
            mask_remove,
        }
    }
}

/// The fully-resolved sandbox after per-field defaulting. Used by the
/// executor at spawn time.
#[derive(Debug, Clone)]
pub struct ResolvedSandbox {
    pub allowed_tools: Vec<String>,
    pub disallowed_bash_patterns: Vec<String>,
    pub disallowed_read_paths: Vec<String>,
}

impl ResolvedSandbox {
    /// Resolve a configured sandbox (or absence) into the values that will
    /// be passed to the wrapped CLI. Each field falls back to its safe
    /// default when unset in the operator's config.
    pub fn resolve(cfg: Option<&ExecutorSandboxConfig>) -> Self {
        let allowed_tools = cfg
            .and_then(|c| c.allowed_tools.clone())
            .unwrap_or_else(default_allowed_tools);
        let disallowed_bash_patterns = cfg
            .and_then(|c| c.disallowed_bash_patterns.clone())
            .unwrap_or_else(default_disallowed_bash_patterns);
        let disallowed_read_paths = cfg
            .and_then(|c| c.disallowed_read_paths.clone())
            .unwrap_or_else(default_disallowed_read_paths);
        Self {
            allowed_tools,
            disallowed_bash_patterns,
            disallowed_read_paths,
        }
    }
}

pub fn default_allowed_tools() -> Vec<String> {
    ["Read", "Write", "Edit", "Glob", "Grep", "Bash"]
        .into_iter()
        .map(String::from)
        .collect()
}

pub fn default_disallowed_bash_patterns() -> Vec<String> {
    [
        "curl:*",
        "wget:*",
        "nc:*",
        "ncat:*",
        "netcat:*",
        "ssh:*",
        "scp:*",
        "sftp:*",
        "rsync:*",
        "git push:*",
        "git remote *",
        "git fetch *://*",
        // Defense in depth against the "lazy archive" failure mode. The
        // structural check in polling_loop::detect_lazy_archive is the
        // real protection (catches bare `git mv` archive renames too).
        "openspec archive:*",
        "openspec unarchive:*",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub fn default_disallowed_read_paths() -> Vec<String> {
    [
        "/home/*/.ssh/**",
        "/home/*/.claude/**",
        "/etc/shadow",
        "/etc/ssl/private/**",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    ClaudeCli,
}

pub(crate) fn default_executor_command() -> String {
    "claude".to_string()
}

fn default_executor_timeout() -> u64 {
    1800
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    #[serde(default = "default_github_token_env")]
    pub token_env: String,
    #[serde(default)]
    pub token: Option<SecretSource>,
    #[serde(default)]
    pub owner_tokens: Option<HashMap<String, SecretSource>>,
    /// When set, autocoder operates in fork-PR mode: the agent branch is
    /// pushed to `git@github.com:<fork_owner>/<repo>.git` (a fork owned
    /// by this handle), and PRs are opened as cross-repository PRs with
    /// `head` formatted as `<fork_owner>:<agent_branch>`. The fork must
    /// be pre-created; autocoder verifies its existence at startup.
    #[serde(default)]
    pub fork_owner: Option<String>,
    /// When true and fork-PR mode is active, on every fresh workspace
    /// clone (workspace dir was absent) autocoder DELETES the existing
    /// fork on GitHub and re-forks upstream before initializing. This
    /// recovers cleanly from snafus where the fork has stale branches no
    /// one cares about, but is DESTRUCTIVE: any open PRs against
    /// branches on the deleted fork are closed by GitHub when the head
    /// ref disappears. Requires the operator's PAT to have the
    /// `delete_repo` scope. Defaults to `false`.
    #[serde(default)]
    pub recreate_fork_on_reinit: bool,
    /// a000: authorization gate for GitHub comment-sourced verbs
    /// (`@<bot> revise`, `@<bot> code-review`, and any future comment
    /// verb). When omitted, the default-deny block applies: only
    /// `OWNER` / `MEMBER` / `COLLABORATOR` associations are authorized.
    /// See [`CommandAuthorizationConfig`].
    #[serde(default)]
    pub command_authorization: CommandAuthorizationConfig,
}

fn default_github_token_env() -> String {
    "GITHUB_TOKEN".to_string()
}

/// The full set of GitHub `author_association` values surfaced by the
/// comments API. `command_authorization.allowed_associations` entries are
/// validated against this set at config load so a typo'd association
/// (`OWENR`, `Collaborator`) fails fast rather than silently never
/// matching. Mirrors the values documented for the GitHub REST API.
pub const KNOWN_AUTHOR_ASSOCIATIONS: &[&str] = &[
    "OWNER",
    "MEMBER",
    "COLLABORATOR",
    "CONTRIBUTOR",
    "FIRST_TIME_CONTRIBUTOR",
    "FIRST_TIMER",
    "NONE",
];

/// Default `allowed_associations`: exactly the associations carrying
/// write/triage permission on a repository. Used both when the operator
/// omits `command_authorization` entirely AND when they supply the block
/// but omit `allowed_associations`.
fn default_allowed_associations() -> Vec<String> {
    vec![
        "OWNER".to_string(),
        "MEMBER".to_string(),
        "COLLABORATOR".to_string(),
    ]
}

/// a000: who may trigger GitHub comment-sourced verbs. A commenter is
/// authorized when EITHER their `author_association` is in
/// `allowed_associations` OR their `login` is in `allowed_users`. An
/// absent or unrecognized association is treated as unauthorized
/// (default-deny). `decline_comment` controls whether a single polite
/// decline reply is posted when a trigger is dropped (default `false`, so
/// unauthorized triggers are silently ignored without comment spam).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandAuthorizationConfig {
    #[serde(default = "default_allowed_associations")]
    pub allowed_associations: Vec<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub decline_comment: bool,
}

impl Default for CommandAuthorizationConfig {
    fn default() -> Self {
        Self {
            allowed_associations: default_allowed_associations(),
            allowed_users: Vec::new(),
            decline_comment: false,
        }
    }
}

impl CommandAuthorizationConfig {
    /// Validate that every `allowed_associations` entry is a recognized
    /// GitHub `author_association` value AND that no `allowed_users` entry
    /// is empty or whitespace-only. Returns an `Err` naming the first
    /// offending entry AND (for associations) the accepted set so the
    /// operator can fix the typo. Called from [`Config::load_from`] so a
    /// bad config fails at startup rather than silently denying every
    /// commenter.
    pub fn validate(&self) -> Result<(), String> {
        for assoc in &self.allowed_associations {
            if !KNOWN_AUTHOR_ASSOCIATIONS.contains(&assoc.as_str()) {
                return Err(format!(
                    "github.command_authorization.allowed_associations contains unknown value `{assoc}`; \
                     valid values are: {}",
                    KNOWN_AUTHOR_ASSOCIATIONS.join(", ")
                ));
            }
        }
        // a000: reject empty / whitespace-only logins so an operator typo
        // (e.g. `allowed_users: [" "]` or a stray blank list entry) fails
        // fast at startup rather than sitting silently in the allowlist as
        // a login the runtime `!login.is_empty()` guard can never match.
        if let Some(blank) = self.allowed_users.iter().find(|u| u.trim().is_empty()) {
            return Err(format!(
                "github.command_authorization.allowed_users contains an empty or \
                 whitespace-only entry ({blank:?}); remove it or replace it with a \
                 valid GitHub login"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerConfig {
    #[serde(default)]
    pub enabled: bool,
    /// LLM provider. OPTIONAL (a55): when omitted, `model` names a
    /// top-level `models:` nickname resolved at config-load; when present,
    /// the block is the legacy inline form and the registry is not
    /// consulted. Always `Some` after [`Config::load_from`] resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ReviewerProvider>,
    pub model: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<SecretSource>,
    #[serde(default)]
    pub api_base_url: Option<String>,
    /// Legacy flat-suffix override for the reviewer's prompt template
    /// (`prompts/code-review-default.md`). Modernized form is
    /// `reviewer.code_review.prompt_path` (see [`PromptOverrideBlock`]).
    /// Both forms remain accepted; the loader prefers the nested one.
    #[serde(default)]
    pub prompt_template_path: Option<PathBuf>,
    /// Nested override block for the code-review prompt (a24).
    /// Modernized form of `prompt_template_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_review: Option<PromptOverrideBlock>,
    /// a005: tri-state reviewer auto-revision gate. Accepts `block`,
    /// `actionable`, or `off` and defaults to `block` (see [`AutoRevise`]).
    /// When it fires, the review's actionable concerns
    /// (`should_request_revision: true` with a non-empty `actionable_request`)
    /// are forwarded — AGGREGATED into a single revision run — to the
    /// PR-comment revision dispatcher, which picks them up on the next
    /// polling iteration. The legacy boolean is mapped for backward
    /// compatibility (`true` → `actionable`, `false` → `off`); the legacy key
    /// `auto_revise_on_block` is accepted as a silent alias so existing
    /// config files load unchanged.
    #[serde(
        default,
        alias = "auto_revise_on_block",
        deserialize_with = "deserialize_auto_revise"
    )]
    pub auto_revise: AutoRevise,
    /// Maximum size (in chars) of the rendered reviewer prompt body —
    /// change context + changed files + diff combined. Default
    /// `2_000_000` preserves the historical hard-coded value. No clamping:
    /// the operator is responsible for matching this to their LLM
    /// provider's actual context window. Hot-applicable via
    /// `autocoder reload`.
    #[serde(default = "default_prompt_budget_chars")]
    pub prompt_budget_chars: usize,
    /// Reviewer dispatch mode. `bundled` (default) keeps the existing
    /// one-reviewer-call-per-PR behavior. `per_change` dispatches one
    /// reviewer call per change in the pass and emits one
    /// `## Code Review: <slug>` section per change in the PR body.
    #[serde(default)]
    pub mode: ReviewerMode,
    /// Per-PR cap on operator-initiated re-reviews triggered via the
    /// `@<bot> code-review` PR-comment verb. `None` (the default) means
    /// UNLIMITED — every re-review is a deliberate operator action and
    /// there is no automatic re-review path, so there is no runaway to
    /// bound. When set to a positive integer it acts as an opt-in ceiling:
    /// values above `MAX_CODE_REVIEWS_PER_PR_CEILING` are clamped down at
    /// startup with a WARN. Independent of
    /// `executor.max_auto_revisions_per_pr`. The original automatic review
    /// at PR-open time does NOT count against this cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_code_reviews_per_pr: Option<u32>,
    /// Optional diff-overlap threshold for the daemon to suggest an
    /// operator-initiated re-review after a revision iteration. `None`
    /// disables the suggestion entirely (default). When `Some(threshold)`,
    /// the value MUST satisfy `0.0..=1.0`; out-of-range values fail
    /// config-load. After each operator-initiated revision iteration's
    /// Completed outcome, the daemon computes the cumulative-since-original-
    /// review diff overlap; when overlap >= threshold AND the suggestion
    /// has not already fired for the current `revisions_applied` count,
    /// a chatops notification recommending `@<bot> code-review` is posted.
    #[serde(default)]
    pub suggest_rereview_threshold: Option<f32>,
    /// a34: cost-optimization knob. When `true`, the polling iteration's
    /// reviewer-invocation step skips the reviewer call AND posts no
    /// `## Code Review` section for any PR whose ENTIRE diff lives
    /// under `openspec/` (i.e. spec-only PRs from brownfield, scout
    /// spec-it, OR archive-driven iterations). Default `false`
    /// (preserves canonical behavior: reviewer runs against every PR).
    #[serde(default)]
    pub skip_spec_only_prs: bool,
    /// a58: reviewer transport. `agentic` (the default since a64) runs the
    /// reviewer through the shared `agentic_run` primitive (a56) as a
    /// CLI-wrapped, read-only session that reads files on demand AND returns
    /// its verdict via the `submit_review` MCP tool. `oneshot` is the
    /// existing single-shot HTTP path that pre-dumps every touched file into
    /// one prompt and scrapes a `VERDICT:` line. The default flipped to
    /// `agentic` once the `opencode` strategy (a60) made the agentic path
    /// provider-agnostic. When the resolved reviewer CLI is unavailable at
    /// startup, an effective-`agentic` reviewer degrades to the `oneshot`
    /// HTTP path for that boot with one WARN (review is never disabled); set
    /// `kind: oneshot` explicitly to opt out of agentic and silence the
    /// warning. Hot-applicable via the existing `reviewer:` reload path.
    #[serde(default)]
    pub kind: ReviewerKind,
    /// a58: the CLI binary the agentic reviewer wraps. Default `"claude"`.
    /// A non-`claude` command resolves its strategy via the a55/a56
    /// `provider → CLI` rule (Anthropic → `claude`, other providers →
    /// `opencode` since a60). When the effective kind is `agentic` but this
    /// CLI is unavailable at startup (no registered strategy OR the binary
    /// is not on the daemon host's PATH) the reviewer falls back to
    /// `oneshot` for that boot. Ignored when `kind: oneshot`. Hot-applicable
    /// via the `reviewer:` reload path.
    #[serde(default = "default_reviewer_command")]
    pub command: String,
}

fn default_prompt_budget_chars() -> usize {
    2_000_000
}

fn default_reviewer_command() -> String {
    "claude".to_string()
}

/// Upper bound on `reviewer.max_code_reviews_per_pr`. Anything above this
/// is clamped down at startup with a WARN log so the operator notices.
pub const MAX_CODE_REVIEWS_PER_PR_CEILING: u32 = 20;

/// Clamp the configured per-PR code-review cap. Mirrors
/// `clamp_log_retention_days`'s shape so callers can observe whether a
/// WARN was issued without scraping the tracing log.
pub fn clamp_max_code_reviews_per_pr(requested: u32) -> (u32, Option<String>) {
    if requested > MAX_CODE_REVIEWS_PER_PR_CEILING {
        let msg = format!(
            "reviewer.max_code_reviews_per_pr ({requested}) is above the ceiling of \
             {MAX_CODE_REVIEWS_PER_PR_CEILING}; clamping to {MAX_CODE_REVIEWS_PER_PR_CEILING}"
        );
        tracing::warn!("{msg}");
        (MAX_CODE_REVIEWS_PER_PR_CEILING, Some(msg))
    } else {
        (requested, None)
    }
}

#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerMode {
    #[default]
    Bundled,
    PerChange,
}

/// a58: reviewer transport selector (`reviewer.kind`).
///
/// `Oneshot` is the existing HTTP single-shot path governed by the
/// `AI-driven code-quality review` requirement. `Agentic` (the default
/// since a64) runs the reviewer through the shared `agentic_run` primitive
/// (a56) — a read-only CLI-wrapped session that reads files on demand AND
/// returns its verdict via the `submit_review` MCP tool. The default is
/// `Agentic` now that the `opencode` strategy (a60) makes the agentic path
/// provider-agnostic, so it is the preferred default for every provider —
/// not only Anthropic-shaped ones. When the resolved reviewer CLI is
/// unavailable at startup the reviewer degrades to the `Oneshot` HTTP path
/// for that boot (see the `Agentic reviewer mode` requirement's startup
/// fallback); review is never disabled.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerKind {
    Oneshot,
    #[default]
    Agentic,
}

/// a005: tri-state reviewer auto-revision gate (`reviewer.auto_revise`).
///
/// Governs whether — AND under which verdict — a review's actionable
/// concerns are forwarded (aggregated into a SINGLE revision run per the
/// orchestrator-cli `Reviewer-initiated revisions from one review dispatch
/// as a single run` requirement) to the revision dispatcher.
///
/// - [`AutoRevise::Block`] (default): auto-revise fires only when the
///   review's effective verdict is `Block`. Combined with a004
///   (security-critical findings escalate the verdict to `Block`),
///   security-critical findings still auto-fix while non-`Block` `Concerns`
///   stay advisory — surfaced to the operator, not silently rewritten.
/// - [`AutoRevise::Actionable`]: fires on any actionable concern regardless
///   of verdict (the pre-a005 fire-regardless-of-verdict behavior).
/// - [`AutoRevise::Off`]: never auto-revise.
///
/// Deserializes from the canonical lowercase strings (`block`, `actionable`,
/// `off`) OR, for backward compatibility, from the legacy boolean: `true` →
/// [`AutoRevise::Actionable`], `false` → [`AutoRevise::Off`]. An absent field
/// defaults to [`AutoRevise::Block`] (the a005 default change, from the prior
/// `false`/off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AutoRevise {
    #[default]
    Block,
    Actionable,
    Off,
}

impl AutoRevise {
    /// Whether auto-revise should fire for a review whose effective verdict
    /// is (or is not) `Block`. `verdict_is_block` is computed from the final
    /// [`crate::code_reviewer::ReviewReport`] verdict, so it already includes
    /// the a004 security escalation.
    pub fn fires(self, verdict_is_block: bool) -> bool {
        match self {
            AutoRevise::Off => false,
            AutoRevise::Actionable => true,
            AutoRevise::Block => verdict_is_block,
        }
    }
}

/// Deserialize [`AutoRevise`] from either the canonical lowercase string
/// (`block`/`actionable`/`off`) or the legacy boolean (`true` → `actionable`,
/// `false` → `off`). Used by `ReviewerConfig::auto_revise`'s
/// `deserialize_with`, composing with `#[serde(default)]` (absent → `Block`)
/// and the `auto_revise_on_block` legacy-key alias.
fn deserialize_auto_revise<'de, D>(deserializer: D) -> Result<AutoRevise, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrStr {
        Bool(bool),
        Str(String),
    }
    match BoolOrStr::deserialize(deserializer)? {
        BoolOrStr::Bool(true) => Ok(AutoRevise::Actionable),
        BoolOrStr::Bool(false) => Ok(AutoRevise::Off),
        BoolOrStr::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "block" => Ok(AutoRevise::Block),
            "actionable" => Ok(AutoRevise::Actionable),
            "off" => Ok(AutoRevise::Off),
            other => Err(D::Error::custom(format!(
                "invalid auto_revise value {other:?}: expected one of `block`, `actionable`, `off` (or legacy `true`/`false`)"
            ))),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatOpsProvider {
    Slack,
    Discord,
    Teams,
    Mattermost,
    Matrix,
}

impl ChatOpsProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Teams => "teams",
            Self::Mattermost => "mattermost",
            Self::Matrix => "matrix",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatOpsConfig {
    pub provider: ChatOpsProvider,
    pub default_channel_id: String,
    #[serde(default)]
    pub notifications: Option<NotificationsConfig>,
    #[serde(default)]
    pub slack: Option<SlackProviderConfig>,
    #[serde(default)]
    pub discord: Option<DiscordProviderConfig>,
    #[serde(default)]
    pub teams: Option<TeamsProviderConfig>,
    #[serde(default)]
    pub mattermost: Option<MattermostProviderConfig>,
    #[serde(default)]
    pub matrix: Option<MatrixProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackProviderConfig {
    #[serde(default)]
    pub bot_token_env: Option<String>,
    #[serde(default)]
    pub bot_token: Option<SecretSource>,
    /// App-level token used by the Socket Mode inbound listener
    /// (`xapp-*` prefix). Optional — when absent, the inbound listener
    /// is not started. Resolved via the same inline-or-env-var pattern
    /// as `bot_token` / `bot_token_env`.
    #[serde(default)]
    pub app_token_env: Option<String>,
    #[serde(default)]
    pub app_token: Option<SecretSource>,
    /// Extra channel IDs the inbound listener will honor commands in,
    /// on top of the union of every `repositories[].chatops_channel_id`
    /// and `chatops.default_channel_id`. Messages from channels not in
    /// the resulting allowlist are silently dropped.
    #[serde(default)]
    pub listen_channels: Vec<String>,
    /// Maximum number of recently-processed `app_mention` events the
    /// inbound listener remembers for dedup. Slack's Socket Mode
    /// delivery is at-least-once; the dedup cache suppresses
    /// redeliveries of an event that has already been dispatched.
    /// Default `100`. Maximum `10000` (operator values above the cap
    /// are clamped to `10000` with a WARN). Value `0` disables dedup
    /// entirely (every event is dispatched).
    #[serde(default = "default_dedup_cache_capacity")]
    pub dedup_cache_capacity: usize,
    /// Per-entry TTL for the dedup cache, in seconds. Entries older
    /// than this are treated as not-present and replaced on the next
    /// lookup. Default `600` (10 minutes). Maximum `3600` (operator
    /// values above the cap are clamped with a WARN). `0` is not
    /// permitted — it's clamped to `1` with a WARN to keep the
    /// semantics clear (use `dedup_cache_capacity: 0` to disable
    /// dedup).
    #[serde(default = "default_dedup_cache_ttl_secs")]
    pub dedup_cache_ttl_secs: u64,
}

/// Default dedup-cache capacity for the Slack inbound listener.
pub fn default_dedup_cache_capacity() -> usize {
    100
}

/// Default dedup-cache TTL (seconds) for the Slack inbound listener.
pub fn default_dedup_cache_ttl_secs() -> u64 {
    600
}

/// Upper bound on `chatops.slack.dedup_cache_capacity`. Values above
/// are clamped down with a WARN.
pub const DEDUP_CACHE_CAPACITY_CEILING: usize = 10_000;

/// Upper bound on `chatops.slack.dedup_cache_ttl_secs`. Values above
/// are clamped down with a WARN.
pub const DEDUP_CACHE_TTL_SECS_CEILING: u64 = 3_600;

/// Clamp the configured dedup-cache capacity. Values above the ceiling
/// are clamped down AND a `tracing::warn!` is emitted naming both the
/// requested and clamped values. `0` is a valid configuration (dedup
/// disabled) and is passed through without warning.
pub fn clamp_dedup_cache_capacity(requested: usize) -> (usize, Option<String>) {
    if requested > DEDUP_CACHE_CAPACITY_CEILING {
        let msg = format!(
            "chatops.slack.dedup_cache_capacity ({requested}) is above the ceiling of \
             {DEDUP_CACHE_CAPACITY_CEILING}; clamping to {DEDUP_CACHE_CAPACITY_CEILING}"
        );
        tracing::warn!("{msg}");
        (DEDUP_CACHE_CAPACITY_CEILING, Some(msg))
    } else {
        (requested, None)
    }
}

/// Clamp the configured dedup-cache TTL. Values above the ceiling are
/// clamped down to the ceiling with a WARN. A configured value of `0`
/// is also clamped (to `1`) because the TTL has no "disabled" meaning
/// — use `dedup_cache_capacity: 0` to disable dedup.
pub fn clamp_dedup_cache_ttl_secs(requested: u64) -> (u64, Option<String>) {
    if requested == 0 {
        let msg =
            "chatops.slack.dedup_cache_ttl_secs (0) is not permitted; clamping to 1 \
             (use dedup_cache_capacity=0 to disable dedup)"
                .to_string();
        tracing::warn!("{msg}");
        return (1, Some(msg));
    }
    if requested > DEDUP_CACHE_TTL_SECS_CEILING {
        let msg = format!(
            "chatops.slack.dedup_cache_ttl_secs ({requested}) is above the ceiling of \
             {DEDUP_CACHE_TTL_SECS_CEILING}; clamping to {DEDUP_CACHE_TTL_SECS_CEILING}"
        );
        tracing::warn!("{msg}");
        (DEDUP_CACHE_TTL_SECS_CEILING, Some(msg))
    } else {
        (requested, None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordProviderConfig {
    pub bot_token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamsProviderConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret_env: String,
    pub team_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MattermostProviderConfig {
    pub server_url: String,
    pub access_token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixProviderConfig {
    pub homeserver_url: String,
    pub access_token_env: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    #[serde(default = "default_true")]
    pub start_work: bool,
    #[serde(default = "default_true")]
    pub failure_alerts: bool,
    #[serde(default = "default_true")]
    pub pr_opened: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            start_work: true,
            failure_alerts: true,
            pr_opened: true,
        }
    }
}

impl NotificationsConfig {
    /// Resolve the effective `start_work` flag given the (optional) ChatOps
    /// config: defaults to `true` when no `notifications:` block was set, and
    /// honors the explicit value otherwise.
    pub fn start_work_enabled(chatops: Option<&ChatOpsConfig>) -> bool {
        chatops
            .and_then(|s| s.notifications.as_ref())
            .map(|n| n.start_work)
            .unwrap_or(true)
    }

    /// Resolve the effective `failure_alerts` flag given the (optional) ChatOps
    /// config: defaults to `true` when no `notifications:` block was set, and
    /// honors the explicit value otherwise.
    pub fn failure_alerts_enabled(chatops: Option<&ChatOpsConfig>) -> bool {
        chatops
            .and_then(|s| s.notifications.as_ref())
            .map(|n| n.failure_alerts)
            .unwrap_or(true)
    }

    /// Resolve the effective `pr_opened` flag given the (optional) ChatOps
    /// config: defaults to `true` when no `notifications:` block was set,
    /// and honors the explicit value otherwise.
    pub fn pr_opened_enabled(chatops: Option<&ChatOpsConfig>) -> bool {
        chatops
            .and_then(|s| s.notifications.as_ref())
            .map(|n| n.pr_opened)
            .unwrap_or(true)
    }
}

/// Top-level periodic-audits config. Operators set this block to enable
/// any audits — without it every audit's effective cadence is `Disabled`
/// and no scheduler work happens. `defaults` maps audit type names to
/// their global cadence; `settings` carries per-audit knobs (prompt
/// override path, notify-on-clean flag, free-form `extra` for per-audit
/// thresholds like brightline's `file_lines_threshold`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditsConfig {
    #[serde(default)]
    pub defaults: HashMap<String, Cadence>,
    #[serde(default)]
    pub settings: HashMap<String, AuditSettings>,
    /// Number of retry attempts after a generated proposal fails
    /// `openspec validate --strict`. Each retry re-invokes the audit's
    /// LLM with the validation error appended to its prompt. `0` disables
    /// retries (first failure → `ValidationExhausted`). Values above
    /// [`MAX_VALIDATION_RETRIES_CEILING`] are clamped down at startup
    /// with a WARN.
    #[serde(default = "default_max_validation_retries")]
    pub max_validation_retries: u32,
    /// Per-iteration cap on how many audits run before the scheduler
    /// returns control to the iteration loop. Default `1` keeps audit
    /// work as low-priority background — even when many audits become
    /// eligible at once (e.g. after a HEAD change unblocks every
    /// `requires_head_change` audit), only one runs per iteration so
    /// pending changes still get attention each cycle. On-demand queued
    /// runs also count against the bound. Values above the number of
    /// registered audits clamp at the registry count with a WARN. Value
    /// `0` is permitted and disables audits behaviourally (every
    /// iteration skips the audit phase).
    #[serde(default = "default_max_audits_per_iteration")]
    pub max_audits_per_iteration: usize,
}

impl Default for AuditsConfig {
    fn default() -> Self {
        Self {
            defaults: HashMap::new(),
            settings: HashMap::new(),
            max_validation_retries: default_max_validation_retries(),
            max_audits_per_iteration: default_max_audits_per_iteration(),
        }
    }
}

/// Default retry budget when the operator does not configure
/// `audits.max_validation_retries`. One retry handles the common case
/// where the LLM made a single fixable error (wrong header name, missing
/// `SHALL`, etc.) and can self-correct when shown the error.
pub fn default_max_validation_retries() -> u32 {
    1
}

/// Upper bound on `audits.max_validation_retries`. Anything above this is
/// clamped down at startup with a WARN log. The ceiling is arbitrary but
/// reasonable — operators who think they need 6+ retries probably have a
/// different problem.
pub const MAX_VALIDATION_RETRIES_CEILING: u32 = 5;

/// If `audits.max_validation_retries` exceeds the ceiling, return the
/// clamped value AND the WARN message that should be emitted at startup.
/// Returns `(clamped_value, Option<warn_message>)`. The caller is
/// responsible for actually emitting the warn (the daemon does at config-
/// load; tests assert on the returned message).
pub fn clamp_max_validation_retries(requested: u32) -> (u32, Option<String>) {
    if requested > MAX_VALIDATION_RETRIES_CEILING {
        let msg = format!(
            "audits.max_validation_retries: requested {requested} exceeds ceiling \
             {MAX_VALIDATION_RETRIES_CEILING}; clamping to {MAX_VALIDATION_RETRIES_CEILING}"
        );
        tracing::warn!("{msg}");
        (MAX_VALIDATION_RETRIES_CEILING, Some(msg))
    } else {
        (requested, None)
    }
}

/// Default per-iteration cap when the operator does not configure
/// `audits.max_audits_per_iteration`. `1` matches the
/// audit-as-low-priority-background-task design intent — even when many
/// audits are eligible simultaneously, only one runs per iteration so
/// pending-change processing continues to share each iteration's
/// wall-clock.
pub fn default_max_audits_per_iteration() -> usize {
    1
}

/// If `audits.max_audits_per_iteration` exceeds `registry_count`, return
/// the clamped value AND the WARN message that should be emitted at
/// startup. Operators who request more than the number of registered
/// audits get clamped to `registry_count` — running more audits than
/// exist is impossible. Value `0` is permitted (every iteration skips
/// the audit phase) and never warns.
pub fn clamp_max_audits_per_iteration(
    requested: usize,
    registry_count: usize,
) -> (usize, Option<String>) {
    if requested > registry_count {
        let msg = format!(
            "audits.max_audits_per_iteration: requested {requested} exceeds the number of \
             registered audits ({registry_count}); clamping to {registry_count}"
        );
        tracing::warn!("{msg}");
        (registry_count, Some(msg))
    } else {
        (requested, None)
    }
}

/// The model an audit resolves to at config-load (audit-model-selection).
/// Populated from [`AuditSettings::model`] (a `models:` registry nickname)
/// during [`Config::load_from`]; `None` when the audit configured no model
/// and therefore keeps the default `claude` CLI strategy.
///
/// The credential is intentionally NOT retained: every periodic audit drives
/// a CLI strategy, which authenticates from the wrapped CLI's own login /
/// credential store (a003) and ignores any resolved key. Only the provider
/// (which selects the CLI strategy + OS-sandbox CLI kind), the concrete model
/// name, AND the optional API base URL (consumed by the `opencode` strategy's
/// `--model <provider>/<model>` flag + provider config) are needed downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAuditModel {
    pub provider: LlmProvider,
    pub model: String,
    pub api_base_url: Option<String>,
}

/// Per-audit settings keyed by audit type name. `prompt_path` overrides
/// the audit's embedded default LLM prompt template (no LLM audits ship
/// in the foundation change; the field is laid in for future audits).
/// `notify_on_clean` toggles a brief "no findings" chatops post for
/// `Reported(vec![])` outcomes (silence is success by default). `extra`
/// is a free-form YAML mapping each audit can read its own knobs out of.
/// `model` (audit-model-selection) is an optional `models:` registry
/// nickname routing this audit to a specific LLM + CLI strategy; it is
/// resolved into `resolved_model` at config-load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSettings {
    #[serde(default)]
    pub prompt_path: Option<PathBuf>,
    #[serde(default)]
    pub notify_on_clean: bool,
    /// Optional `models:` registry nickname (audit-model-selection). When
    /// set, the audit runner selects the CLI strategy for the resolved
    /// model's provider AND passes `--model <provider>/<model>`. Resolved
    /// against the registry at config-load; an unknown nickname fails fast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The `model` nickname resolved against the top-level `models:`
    /// registry at config-load. Never deserialized from / serialized to the
    /// config file — it is a derived runtime field. `None` when no `model`
    /// was configured (preserving the default `claude` CLI behavior).
    #[serde(skip)]
    pub resolved_model: Option<ResolvedAuditModel>,
    #[serde(default)]
    pub extra: HashMap<String, serde_yml::Value>,
}

/// Cadence at which a periodic audit fires. Deserializes from a YAML
/// string in one of the literal forms documented in the spec:
/// `disabled`, `daily`, `every-N-days` (N a positive integer),
/// `weekly`, `monthly`, `quarterly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    Disabled,
    Daily,
    EveryNDays(u32),
    Weekly,
    Monthly,
    Quarterly,
}

impl Cadence {
    /// Canonical lowercase string form. Mirrors what `Cadence::parse`
    /// accepts so a serialize → deserialize round trip is a fixed point.
    pub fn as_yaml_str(&self) -> String {
        match self {
            Self::Disabled => "disabled".to_string(),
            Self::Daily => "daily".to_string(),
            Self::Weekly => "weekly".to_string(),
            Self::Monthly => "monthly".to_string(),
            Self::Quarterly => "quarterly".to_string(),
            Self::EveryNDays(n) => format!("every-{n}-days"),
        }
    }
}

impl serde::Serialize for Cadence {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_yaml_str())
    }
}

impl Cadence {
    /// Effective inter-run interval. `Disabled` returns `None` so callers
    /// can short-circuit without computing a duration that would never
    /// trigger. All other variants return `Some(Duration)`.
    pub fn interval(self) -> Option<chrono::Duration> {
        match self {
            Self::Disabled => None,
            Self::Daily => Some(chrono::Duration::days(1)),
            Self::EveryNDays(n) => Some(chrono::Duration::days(i64::from(n))),
            Self::Weekly => Some(chrono::Duration::days(7)),
            Self::Monthly => Some(chrono::Duration::days(30)),
            Self::Quarterly => Some(chrono::Duration::days(90)),
        }
    }

    /// True for any variant other than `Disabled`. Equivalent to
    /// `self.interval().is_some()`.
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

impl<'de> Deserialize<'de> for Cadence {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let raw = String::deserialize(deserializer)?;
        Cadence::parse(&raw).map_err(D::Error::custom)
    }
}

impl Cadence {
    /// Parse a cadence string. Used by the custom `Deserialize` impl and
    /// directly by tests. Rejects `every-0-days`, negative N, and
    /// non-integer N with a descriptive error.
    pub fn parse(raw: &str) -> std::result::Result<Self, String> {
        let trimmed = raw.trim();
        match trimmed {
            "disabled" => Ok(Self::Disabled),
            "daily" => Ok(Self::Daily),
            "weekly" => Ok(Self::Weekly),
            "monthly" => Ok(Self::Monthly),
            "quarterly" => Ok(Self::Quarterly),
            other => {
                if let Some(rest) = other.strip_prefix("every-").and_then(|s| s.strip_suffix("-days")) {
                    // Reject leading `-` (negative) explicitly so the
                    // error message is precise; u32::from_str would also
                    // reject but with a generic "invalid digit" message.
                    if rest.starts_with('-') {
                        return Err(format!(
                            "cadence `{raw}`: N must be a positive integer, got negative value"
                        ));
                    }
                    let n: u32 = rest.parse().map_err(|_| {
                        format!(
                            "cadence `{raw}`: N must be a positive integer (parsed segment: `{rest}`)"
                        )
                    })?;
                    if n == 0 {
                        return Err(format!(
                            "cadence `{raw}`: N must be a positive integer, got 0"
                        ));
                    }
                    Ok(Self::EveryNDays(n))
                } else {
                    Err(format!(
                        "cadence `{raw}`: expected one of `disabled`, `daily`, `every-N-days`, `weekly`, `monthly`, `quarterly`"
                    ))
                }
            }
        }
    }
}

/// Resolve the effective cadence for `audit_type` against the given repo
/// and (optional) global audits config. Lookup order: per-repo override
/// → global default → `Disabled`. Used by the scheduler each iteration.
pub fn resolved_cadence(
    repo: &RepositoryConfig,
    audits_cfg: Option<&AuditsConfig>,
    audit_type: &str,
) -> Cadence {
    if let Some(overrides) = repo.audits.as_ref() {
        if let Some(c) = overrides.get(audit_type) {
            return *c;
        }
    }
    if let Some(global) = audits_cfg {
        if let Some(c) = global.defaults.get(audit_type) {
            return *c;
        }
    }
    Cadence::Disabled
}

/// Validate that every audit type name appearing in `audits.defaults` or
/// any `repositories[].audits` is in `known_audit_types`. Returns an
/// error listing each unknown name + the set of known names so the
/// operator can correct the config. Called from the daemon entry point
/// after the audit registry is built.
/// Emit WARN-level logs when the resolved Slack token values do not have
/// the expected provider-conventional prefix (`xoxb-` for bot tokens,
/// `xapp-` for app-level tokens). These are advisory only — Slack could
/// in principle change the prefix in the future — so a wrong prefix is
/// never a hard load-time failure. Returns the pair of warn messages
/// that were emitted (each as `Some(msg)`) so tests can assert without
/// re-running through `tracing-subscriber`.
pub fn warn_on_unexpected_slack_token_prefixes(
    bot_token: Option<&str>,
    app_token: Option<&str>,
) -> (Option<String>, Option<String>) {
    let bot_msg = bot_token
        .filter(|t| !t.starts_with("xoxb-"))
        .map(|_| {
            let m = "chatops.slack.bot_token does not start with `xoxb-`; \
                     Slack bot tokens conventionally use that prefix. \
                     This is a warning, not a hard failure."
                .to_string();
            tracing::warn!("{m}");
            m
        });
    let app_msg = app_token
        .filter(|t| !t.starts_with("xapp-"))
        .map(|_| {
            let m = "chatops.slack.app_token does not start with `xapp-`; \
                     Slack app-level tokens conventionally use that prefix. \
                     This is a warning, not a hard failure."
                .to_string();
            tracing::warn!("{m}");
            m
        });
    (bot_msg, app_msg)
}

pub fn validate_audit_type_names(
    cfg: &Config,
    known_audit_types: &[&str],
) -> Result<()> {
    let mut unknown: Vec<(String, String)> = Vec::new();
    if let Some(audits) = cfg.audits.as_ref() {
        for name in audits.defaults.keys() {
            if !known_audit_types.contains(&name.as_str()) {
                unknown.push((format!("audits.defaults.{name}"), name.clone()));
            }
        }
        for name in audits.settings.keys() {
            if !known_audit_types.contains(&name.as_str()) {
                unknown.push((format!("audits.settings.{name}"), name.clone()));
            }
        }
    }
    for (idx, repo) in cfg.repositories.iter().enumerate() {
        if let Some(overrides) = repo.audits.as_ref() {
            for name in overrides.keys() {
                if !known_audit_types.contains(&name.as_str()) {
                    unknown.push((
                        format!("repositories[{idx}].audits.{name}"),
                        name.clone(),
                    ));
                }
            }
        }
    }
    if unknown.is_empty() {
        return Ok(());
    }
    let known_list = if known_audit_types.is_empty() {
        "(none registered)".to_string()
    } else {
        known_audit_types.join(", ")
    };
    let mut msg = format!(
        "unknown audit type name(s) in config; known types: {known_list}\n"
    );
    for (path, name) in &unknown {
        msg.push_str(&format!("  - {path}: `{name}` is not a registered audit type\n"));
    }
    Err(anyhow!(msg.trim_end().to_string()))
}

impl RepositoryConfig {
    /// Resolve the ChatOps channel to use for this repo: explicit per-repo
    /// `chatops_channel_id` if set, otherwise the global default.
    pub fn chatops_channel<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.chatops_channel_id.as_deref().unwrap_or(fallback)
    }

    /// OSS-fork support (a26): resolve `spec_storage.path` against the
    /// per-repo workspace path. Returns `None` when `spec_storage` is
    /// unset (specs live at `<workspace>/openspec/`). Workspace-relative
    /// paths are anchored at `code_workspace`.
    ///
    /// Today consumed only by `SpecRoot::for_repo` (see
    /// `autocoder/src/spec_root.rs`); call-site migration of the
    /// existing `<workspace>/openspec/...` literals is task 2.3 in
    /// `openspec/changes/a26-oss-fork-support/tasks.md` AND is deferred
    /// to follow-up work.
    #[allow(dead_code)]
    pub fn resolved_spec_storage_dir(&self, code_workspace: &Path) -> Option<PathBuf> {
        let ss = self.spec_storage.as_ref()?;
        let raw = PathBuf::from(ss.path.trim());
        let resolved = if raw.is_absolute() {
            raw
        } else {
            code_workspace.join(raw)
        };
        Some(resolved)
    }

    /// Resolve the effective `max_changes_per_pr` for this repository.
    /// Lookup order: per-repo override → executor-level default → hardcoded
    /// `3`. Any configured value is clamped to `>= 1`. Callers that want
    /// to warn about a configured `0` read the raw fields directly.
    pub fn max_changes_per_pr(&self, executor: &ExecutorConfig) -> u32 {
        const DEFAULT: u32 = 3;
        let chosen = self
            .max_changes_per_pr
            .or(executor.max_changes_per_pr)
            .unwrap_or(DEFAULT);
        chosen.max(1)
    }

    /// a006: resolve the effective credential-protection toggles for this
    /// repository. Lookup order, per toggle: per-repo override → global
    /// `executor.sandbox` → the secure default (ON). There is no implicit
    /// downgrade — both are ON unless an operator explicitly set one off.
    pub fn resolved_sandbox_toggles(&self, global: Option<&ExecutorSandboxConfig>) -> SandboxToggles {
        let repo = self.sandbox.as_ref();
        let os_hide = repo
            .and_then(|r| r.os_hide)
            .or_else(|| global.and_then(|g| g.os_hide))
            .unwrap_or(true);
        let engine_deny = repo
            .and_then(|r| r.engine_deny)
            .or_else(|| global.and_then(|g| g.engine_deny))
            .unwrap_or(true);
        // a013: strict mode — per-repo → global → default OFF.
        let strict_mode = repo
            .and_then(|r| r.strict_mode)
            .or_else(|| global.and_then(|g| g.strict_mode))
            .unwrap_or(false);
        // a013: mask edits are additive — global first, then this repo's.
        let mut mask_add: Vec<String> = global
            .and_then(|g| g.mask_add.clone())
            .unwrap_or_default();
        if let Some(a) = repo.and_then(|r| r.mask_add.as_ref()) {
            mask_add.extend(a.iter().cloned());
        }
        let mut mask_remove: Vec<String> = global
            .and_then(|g| g.mask_remove.clone())
            .unwrap_or_default();
        if let Some(rm) = repo.and_then(|r| r.mask_remove.as_ref()) {
            mask_remove.extend(rm.iter().cloned());
        }
        SandboxToggles {
            os_hide,
            engine_deny,
            strict_mode,
            mask_add,
            mask_remove,
        }
    }

    /// a006: the per-repository startup WARN naming each credential-protection
    /// toggle that is OFF for this repository, or `None` when both are ON (the
    /// secure default — no WARN). Separated from the logging site so the
    /// disposition can be asserted without a daemon (task 8.6).
    pub fn relaxed_sandbox_warning(&self, global: Option<&ExecutorSandboxConfig>) -> Option<String> {
        let toggles = self.resolved_sandbox_toggles(global);
        let mut off: Vec<&str> = Vec::new();
        if !toggles.os_hide {
            off.push("os_hide");
        }
        if !toggles.engine_deny {
            off.push("engine_deny");
        }
        // a013: removing a default mask-list entry exposes a sensitive path —
        // an explicit relaxed posture that SHALL be logged, naming each entry.
        let exposed = crate::sandbox::removed_default_mask_entries(&toggles.mask_remove);
        if off.is_empty() && exposed.is_empty() {
            return None;
        }
        let mut clauses: Vec<String> = Vec::new();
        if !off.is_empty() {
            clauses.push(format!("{} OFF", off.join(" + ")));
        }
        if !exposed.is_empty() {
            clauses.push(format!("default mask entries exposed: {}", exposed.join(", ")));
        }
        Some(format!(
            "repository `{}` runs with relaxed sandbox credential protection: \
             {}. Sensitive paths may be reachable by the wrapped model, and \
             egress is unrestricted. This is an explicit, non-default posture.",
            self.url,
            clauses.join("; ")
        ))
    }
}

impl Config {
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let mut cfg: Config = serde_yml::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        if let Some(audits) = cfg.audits.as_mut() {
            let (clamped, _) = clamp_max_validation_retries(audits.max_validation_retries);
            audits.max_validation_retries = clamped;
        }
        let (drain_clamped, _) =
            clamp_wipe_drain_timeout_secs(cfg.executor.wipe_drain_timeout_secs);
        cfg.executor.wipe_drain_timeout_secs = drain_clamped;
        let (retention_clamped, _) =
            clamp_log_retention_days(cfg.executor.log_retention_days);
        cfg.executor.log_retention_days = retention_clamped;
        // Clamp the busy-marker stale threshold IN PLACE if the
        // operator set it explicitly. We preserve the `None` case so
        // the startup-log code can detect "operator did not set this"
        // — clamping `None` to `Some(default)` would erase that
        // signal.
        if let Some(raw) = cfg.executor.busy_marker_stale_threshold_secs {
            let (clamped, _) = clamp_busy_marker_stale_threshold_secs(raw);
            cfg.executor.busy_marker_stale_threshold_secs = Some(clamped);
        }
        if let Some(slack) = cfg
            .chatops
            .as_mut()
            .and_then(|c| c.slack.as_mut())
        {
            let (cap, _) = clamp_dedup_cache_capacity(slack.dedup_cache_capacity);
            slack.dedup_cache_capacity = cap;
            let (ttl, _) = clamp_dedup_cache_ttl_secs(slack.dedup_cache_ttl_secs);
            slack.dedup_cache_ttl_secs = ttl;
        }
        // a65: the workspace-cache cap is expressed by OMITTING the field
        // (unbounded) — a zero cap is a misconfiguration (it would demand
        // evicting every workspace) and is rejected up front.
        if let Some(max_gb) = cfg.cache.workspaces_max_gb
            && max_gb == 0
        {
            return Err(anyhow!(
                "cache.workspaces_max_gb must be greater than 0 when set; \
                 omit the field entirely for an unbounded workspace cache"
            ));
        }
        // a55: validate every `models:` registry entry's own per-provider
        // auth config up front, regardless of whether any block references
        // it (e.g. `ollama` with an `api_key` fails here). The registry is
        // cloned so the per-block resolution below can read it while each
        // block is borrowed mutably; config-load is one-time so the clone
        // is immaterial.
        let models = cfg.models.clone();
        if let Some(registry) = models.as_ref() {
            for (nick, entry) in registry {
                let key_present = entry.api_key.is_some() || entry.api_key_env.is_some();
                // a55 registry entries are CLI-capable (each resolves to a
                // claude/opencode/agy strategy), so `api_key` is optional here —
                // the CLI self-authenticates, and a supplied key is passed to it.
                // An in-process HTTP consumer that references a keyless entry
                // (RAG, oneshot reviewer) still enforces the key at ITS site.
                validate_llm_provider_config_cli(
                    entry.provider,
                    key_present,
                    entry.api_base_url.as_deref(),
                    &format!("models.{nick}"),
                )?;
            }
        }
        if let Some(rag) = cfg.canonical_rag.as_mut() {
            // a55: a block omitting inline `provider` resolves its `model`
            // nickname against `models:`; an inline block is untouched.
            if let Some(resolved) =
                resolve_model_reference(rag.provider, &rag.model, models.as_ref(), "canonical_rag")?
            {
                rag.provider = Some(resolved.provider);
                rag.model = resolved.model;
                if let Some(base) = resolved.api_base_url {
                    rag.api_base_url = base;
                }
                rag.api_key = resolved.api_key;
                rag.api_key_env = resolved.api_key_env;
            }
            let (top_k, _) = clamp_rag_top_k(rag.top_k);
            rag.top_k = top_k;
            // a37: per-subsystem AND per-provider validity. The check
            // fires regardless of `enabled` so an operator with a
            // partially-filled-out block sees the error at startup
            // rather than at the first `enabled: true` flip. After a55
            // resolution the provider is always `Some`.
            let provider = rag
                .provider
                .expect("canonical_rag.provider resolved at config-load");
            validate_provider_for_subsystem(provider, SubsystemKind::CanonicalRag)?;
            let key_present = rag.api_key.is_some() || rag.api_key_env.is_some();
            validate_llm_provider_config(
                provider,
                key_present,
                Some(&rag.api_base_url),
                "canonical_rag",
            )?;
        }
        if let Some(rev) = cfg.reviewer.as_mut() {
            // a55: resolve a nickname reference (no inline `provider`)
            // against the registry; an inline block is untouched.
            if let Some(resolved) =
                resolve_model_reference(rev.provider, &rev.model, models.as_ref(), "reviewer")?
            {
                rev.provider = Some(resolved.provider);
                rev.model = resolved.model;
                rev.api_base_url = resolved.api_base_url;
                rev.api_key = resolved.api_key;
                rev.api_key_env = resolved.api_key_env;
            }
            // The re-review cap is an opt-in ceiling: clamp only when the
            // operator set a value. `None` means unlimited and stays so.
            if let Some(cap) = rev.max_code_reviews_per_pr {
                let (clamped, _) = clamp_max_code_reviews_per_pr(cap);
                rev.max_code_reviews_per_pr = Some(clamped);
            }
            if let Some(t) = rev.suggest_rereview_threshold
                && !(0.0..=1.0).contains(&t)
            {
                return Err(anyhow!(
                    "reviewer.suggest_rereview_threshold ({t}) is out of range; valid range is 0.0..=1.0"
                ));
            }
            // a37: per-subsystem AND per-provider validity, enforced
            // regardless of `enabled` so an unused-but-misconfigured
            // block surfaces at startup. After a55 resolution the provider
            // is always `Some`.
            let provider = rev.provider.expect("reviewer.provider resolved at config-load");
            validate_provider_for_subsystem(provider, SubsystemKind::Reviewer)?;
            let key_present = rev.api_key.is_some() || rev.api_key_env.is_some();
            // The reviewer is agentic (CLI self-auth → api_key optional) OR
            // oneshot (in-process HTTP → api_key required per provider).
            if matches!(rev.kind, ReviewerKind::Agentic) {
                validate_llm_provider_config_cli(
                    provider,
                    key_present,
                    rev.api_base_url.as_deref(),
                    "reviewer",
                )?;
            } else {
                validate_llm_provider_config(
                    provider,
                    key_present,
                    rev.api_base_url.as_deref(),
                    "reviewer",
                )?;
            }
        }
        if let Some(llm) = cfg
            .executor
            .change_internal_contradiction_check_llm
            .as_mut()
        {
            // a55: resolve a nickname reference (no inline `provider`)
            // against the registry; an inline block is untouched.
            if let Some(resolved) = resolve_model_reference(
                llm.provider,
                &llm.model,
                models.as_ref(),
                "change_internal_contradiction_check_llm",
            )? {
                llm.provider = Some(resolved.provider);
                llm.model = resolved.model;
                llm.api_base_url = resolved.api_base_url;
                llm.api_key = resolved.api_key;
                llm.api_key_env = resolved.api_key_env;
            }
            let provider = llm
                .provider
                .expect("change_internal_contradiction_check_llm.provider resolved at config-load");
            validate_provider_for_subsystem(provider, SubsystemKind::ContradictionCheck)?;
            let key_present = llm.api_key.is_some() || llm.api_key_env.is_some();
            // The verifier gates are always CLI/agentic → api_key optional.
            validate_llm_provider_config_cli(
                provider,
                key_present,
                llm.api_base_url.as_deref(),
                "change_internal_contradiction_check_llm",
            )?;
        }
        if let Some(llm) = cfg
            .executor
            .change_canonical_contradiction_check_llm
            .as_mut()
        {
            // a55: resolve a nickname reference (no inline `provider`)
            // against the registry; an inline block is untouched.
            if let Some(resolved) = resolve_model_reference(
                llm.provider,
                &llm.model,
                models.as_ref(),
                "change_canonical_contradiction_check_llm",
            )? {
                llm.provider = Some(resolved.provider);
                llm.model = resolved.model;
                llm.api_base_url = resolved.api_base_url;
                llm.api_key = resolved.api_key;
                llm.api_key_env = resolved.api_key_env;
            }
            let provider = llm
                .provider
                .expect("change_canonical_contradiction_check_llm.provider resolved at config-load");
            validate_provider_for_subsystem(provider, SubsystemKind::CanonContradictionCheck)?;
            let key_present = llm.api_key.is_some() || llm.api_key_env.is_some();
            // The verifier gates are always CLI/agentic → api_key optional.
            validate_llm_provider_config_cli(
                provider,
                key_present,
                llm.api_base_url.as_deref(),
                "change_canonical_contradiction_check_llm",
            )?;
        }
        if let Some(llm) = cfg.executor.code_implements_spec_check_llm.as_mut() {
            // a55: resolve a nickname reference (no inline `provider`)
            // against the registry; an inline block is untouched.
            if let Some(resolved) = resolve_model_reference(
                llm.provider,
                &llm.model,
                models.as_ref(),
                "code_implements_spec_check_llm",
            )? {
                llm.provider = Some(resolved.provider);
                llm.model = resolved.model;
                llm.api_base_url = resolved.api_base_url;
                llm.api_key = resolved.api_key;
                llm.api_key_env = resolved.api_key_env;
            }
            let provider = llm
                .provider
                .expect("code_implements_spec_check_llm.provider resolved at config-load");
            validate_provider_for_subsystem(provider, SubsystemKind::CodeImplementsSpecCheck)?;
            let key_present = llm.api_key.is_some() || llm.api_key_env.is_some();
            // The verifier gates are always CLI/agentic → api_key optional.
            validate_llm_provider_config_cli(
                provider,
                key_present,
                llm.api_base_url.as_deref(),
                "code_implements_spec_check_llm",
            )?;
        }
        // audit-model-selection: resolve each audit's optional `model`
        // nickname against the `models:` registry, mirroring the reviewer
        // validation above. A nickname naming no registry entry fails
        // config-load fast (the error names both the nickname AND the
        // referencing `audits.settings.<audit_type>` block). The credential
        // is intentionally dropped — audits always drive a CLI strategy,
        // which authenticates from its own store (a003), so only provider +
        // model + base URL are retained for strategy + flag selection.
        if let Some(audits) = cfg.audits.as_mut() {
            for (audit_type, settings) in audits.settings.iter_mut() {
                let Some(nickname) = settings.model.clone() else {
                    continue;
                };
                let label = format!("audits.settings.{audit_type}");
                if let Some(resolved) =
                    resolve_model_reference(None, &nickname, models.as_ref(), &label)?
                {
                    settings.resolved_model = Some(ResolvedAuditModel {
                        provider: resolved.provider,
                        model: resolved.model,
                        api_base_url: resolved.api_base_url,
                    });
                }
            }
        }
        // a000: reject typo'd author_association entries up front so a
        // misconfigured allowlist fails at startup rather than silently
        // denying every commenter.
        cfg.github
            .command_authorization
            .validate()
            .map_err(|e| anyhow!("{e}"))?;
        // OSS-fork support (a26): validate spec_storage AND upstream
        // blocks. Fail-fast at config-load so the daemon never spins
        // up a polling task pointing at a missing/invalid spec store.
        for repo in &cfg.repositories {
            if let Some(ss) = repo.spec_storage.as_ref() {
                validate_spec_storage(repo, ss)
                    .with_context(|| format!("repository `{}`", repo.url))?;
            }
            if let Some(up) = repo.upstream.as_ref() {
                validate_upstream(repo, up)
                    .with_context(|| format!("repository `{}`", repo.url))?;
            }
        }
        Ok(cfg)
    }
}

/// Resolve `spec_storage.path` against the per-repo workspace and
/// verify it is a git working tree containing an `openspec/`
/// subdirectory. Workspace-relative paths resolve under the repo's
/// configured `local_path` (when set) or the path the daemon derives
/// from the URL.
fn validate_spec_storage(
    repo: &RepositoryConfig,
    ss: &SpecStorageConfig,
) -> Result<()> {
    let trimmed = ss.path.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("spec_storage.path is empty"));
    }
    let raw = PathBuf::from(trimmed);
    let resolved = if raw.is_absolute() {
        raw
    } else {
        // Workspace-relative: anchor at the configured local_path when
        // set; otherwise leave relative so the daemon resolves at runtime
        // against the derived workspace root. We still want to check
        // existence at config-load when possible, so probe against the
        // current working directory as a last resort.
        match repo.local_path.as_ref() {
            Some(lp) => lp.join(&raw),
            None => raw,
        }
    };
    if !resolved.exists() {
        return Err(anyhow!(
            "spec_storage.path `{}` does not exist",
            resolved.display()
        ));
    }
    if !resolved.is_dir() {
        return Err(anyhow!(
            "spec_storage.path `{}` is not a directory",
            resolved.display()
        ));
    }
    // Git working-tree check via `git -C <path> rev-parse
    // --is-inside-work-tree`. Tolerates both `.git/` directories AND
    // worktrees (where `.git` is a file naming the worktree).
    let probe = std::process::Command::new("git")
        .args(["-C"])
        .arg(&resolved)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    match probe {
        Ok(out) if out.status.success() => {
            let trimmed_out =
                String::from_utf8_lossy(&out.stdout).trim().to_string();
            if trimmed_out != "true" {
                return Err(anyhow!(
                    "spec_storage.path `{}` is not a git working tree \
                     (git -C ... rev-parse --is-inside-work-tree returned `{}`)",
                    resolved.display(),
                    trimmed_out
                ));
            }
        }
        Ok(out) => {
            let stderr =
                String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(anyhow!(
                "spec_storage.path `{}` is not a git working tree \
                 (git -C ... rev-parse --is-inside-work-tree failed: {})",
                resolved.display(),
                stderr
            ));
        }
        Err(e) => {
            return Err(anyhow!(
                "spec_storage.path `{}` could not be probed: \
                 `git` invocation failed: {e}",
                resolved.display()
            ));
        }
    }
    let openspec_dir = resolved.join("openspec");
    if !openspec_dir.is_dir() {
        return Err(anyhow!(
            "spec_storage.path `{}` is a git working tree but has no \
             `openspec/` subdirectory at `{}`",
            resolved.display(),
            openspec_dir.display()
        ));
    }
    // a34: when push_remote is set, verify the remote exists in the
    // spec_storage repo's `git remote` output. Fail-fast so the daemon
    // never spins up a polling task pointing at an invalid remote.
    if let Some(remote_name) = ss.push_remote.as_deref() {
        let remote_name = remote_name.trim();
        if remote_name.is_empty() {
            return Err(anyhow!(
                "spec_storage.push_remote is set but empty (expected a remote name)"
            ));
        }
        let list = std::process::Command::new("git")
            .args(["-C"])
            .arg(&resolved)
            .args(["remote"])
            .output();
        match list {
            Ok(out) if out.status.success() => {
                let raw = String::from_utf8_lossy(&out.stdout);
                let available: Vec<&str> =
                    raw.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                if !available.contains(&remote_name) {
                    return Err(anyhow!(
                        "spec_storage.push_remote `{remote_name}` does not exist in \
                         `git -C {} remote` output (available: [{}])",
                        resolved.display(),
                        available.join(", ")
                    ));
                }
            }
            Ok(out) => {
                let stderr =
                    String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err(anyhow!(
                    "spec_storage.push_remote could not be validated: \
                     `git -C {} remote` failed: {stderr}",
                    resolved.display()
                ));
            }
            Err(e) => {
                return Err(anyhow!(
                    "spec_storage.push_remote could not be validated: \
                     `git` invocation failed: {e}",
                ));
            }
        }
    }
    Ok(())
}

/// Validate `upstream` block. Only `url` non-emptiness is checked at
/// config-load; reachability is the polling iteration's concern.
fn validate_upstream(_repo: &RepositoryConfig, up: &UpstreamConfig) -> Result<()> {
    if up.url.trim().is_empty() {
        return Err(anyhow!("upstream.url is empty"));
    }
    if up.remote.trim().is_empty() {
        return Err(anyhow!("upstream.remote is empty"));
    }
    if up.branch.trim().is_empty() {
        return Err(anyhow!("upstream.branch is empty"));
    }
    Ok(())
}

// --------------------------------------------------------------------------
// Validation surface shared by `autocoder run` startup AND `autocoder
// check-config`. Side-effect-free: every check inspects the parsed
// `Config` (and the process environment for env-var existence) and pushes
// findings into the returned report. Callers decide how to react.
// --------------------------------------------------------------------------

/// Slug enum for every category the validator examines. The slug strings
/// here are the operator-visible labels (`OK: schema — ...`,
/// `ERROR: token-route: ...`) and the `category` field of `--json` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingCategory {
    Parse,
    Schema,
    TokenRoute,
    WorkspaceCollision,
    AuditSlug,
    PathCollision,
    SecretSource,
}

impl FindingCategory {
    /// Operator-visible slug used in stdout lines (`ERROR: <slug>: ...`)
    /// and the `category` JSON field. Stable string IDs — these are part
    /// of the CLI's documented contract.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Schema => "schema",
            Self::TokenRoute => "token-route",
            Self::WorkspaceCollision => "workspace-collision",
            Self::AuditSlug => "audit-slug",
            Self::PathCollision => "path-collision",
            Self::SecretSource => "secret-source",
        }
    }

    /// One-line summary printed for a passing category (`OK: <slug> — <summary>`).
    pub fn ok_summary(self) -> &'static str {
        match self {
            Self::Parse => "config parsed successfully",
            Self::Schema => "all required fields present and value ranges respected",
            Self::TokenRoute => "every repository has a resolvable GitHub token route",
            Self::WorkspaceCollision => "every repository resolves to a distinct workspace path",
            Self::AuditSlug => "every audit slug names a registered audit type",
            Self::PathCollision => "every paths.* role resolves to a distinct directory",
            Self::SecretSource => "every referenced env-var-sourced secret is set",
        }
    }
}

/// A single finding emitted by `validate_config`. `config_pointer` is a
/// JSON-Pointer-style locator into the YAML (e.g. `repositories/0/url`)
/// when the finding maps to a specific field; `None` for whole-config
/// findings (e.g. parse failures).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub category: FindingCategory,
    pub message: String,
    pub config_pointer: Option<String>,
}

/// Result of running every validation check. Errors are hard failures
/// (would block daemon startup or produce a non-zero `check-config`
/// exit); warnings are advisory (e.g. an env-var-sourced secret is
/// unset, which may resolve at systemd-unit-start time but is worth
/// surfacing now).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValidationReport {
    pub errors: Vec<Finding>,
    pub warnings: Vec<Finding>,
}

impl ValidationReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// True iff the report has zero errors AND zero warnings.
    /// Part of the documented `ValidationReport` API even if the daemon
    /// uses [`Self::has_errors`] for its own gating.
    #[allow(dead_code)]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty() && self.warnings.is_empty()
    }

    /// True iff at least one error is present.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    fn push_error(
        &mut self,
        category: FindingCategory,
        message: impl Into<String>,
        config_pointer: Option<String>,
    ) {
        self.errors.push(Finding {
            category,
            message: message.into(),
            config_pointer,
        });
    }

    fn push_warn(
        &mut self,
        category: FindingCategory,
        message: impl Into<String>,
        config_pointer: Option<String>,
    ) {
        self.warnings.push(Finding {
            category,
            message: message.into(),
            config_pointer,
        });
    }
}

/// Audit type slugs known to the daemon's audit registry. Used by the
/// validator's audit-slug check; kept in sync with `cli/run.rs` where
/// the actual `AuditRegistry` is built. A drift between the two would
/// either silently accept a typo (validator too lenient) or reject a
/// valid slug (validator too strict).
pub const KNOWN_AUDIT_TYPES: &[&str] = &[
    "architecture_brightline",
    "drift_audit",
    "missing_tests_audit",
    "security_bug_audit",
    "architecture_consultative",
    "documentation_audit",
    "canon_contradiction_audit",
    "canon_consolidation_audit",
];

/// Run every config validation check and return a structured report.
/// Side-effect-free apart from reading process env vars for the
/// `SecretSource` check. The caller decides how to surface the report
/// (block startup, render to stdout, emit JSON, log).
pub fn validate_config(config: &Config) -> ValidationReport {
    let mut report = ValidationReport::new();
    check_schema(config, &mut report);
    check_token_routes(config, &mut report);
    check_workspace_collisions(config, &mut report);
    check_audit_slugs(config, &mut report);
    check_path_collisions(config, &mut report);
    check_secret_sources(config, &mut report);
    report
}

/// Schema check: required fields are non-empty and value-range invariants
/// hold (positive `poll_interval_sec`, etc.). One error per violation.
fn check_schema(config: &Config, report: &mut ValidationReport) {
    if config.repositories.is_empty() {
        report.push_error(
            FindingCategory::Schema,
            "repositories list is empty; at least one repository must be configured",
            Some("repositories".into()),
        );
    }
    for (idx, repo) in config.repositories.iter().enumerate() {
        if repo.url.trim().is_empty() {
            report.push_error(
                FindingCategory::Schema,
                format!("repositories[{idx}].url must not be empty"),
                Some(format!("repositories/{idx}/url")),
            );
        }
        if repo.base_branch.trim().is_empty() {
            report.push_error(
                FindingCategory::Schema,
                format!("repositories[{idx}].base_branch must not be empty"),
                Some(format!("repositories/{idx}/base_branch")),
            );
        }
        if repo.agent_branch.trim().is_empty() {
            report.push_error(
                FindingCategory::Schema,
                format!("repositories[{idx}].agent_branch must not be empty"),
                Some(format!("repositories/{idx}/agent_branch")),
            );
        }
        if repo.poll_interval_sec == 0 {
            report.push_error(
                FindingCategory::Schema,
                format!(
                    "repositories[{idx}].poll_interval_sec must be > 0 (got 0)"
                ),
                Some(format!("repositories/{idx}/poll_interval_sec")),
            );
        }
    }
    if config.executor.command.trim().is_empty() {
        report.push_error(
            FindingCategory::Schema,
            "executor.command must not be empty",
            Some("executor/command".into()),
        );
    }
    if config.executor.timeout_secs == 0 {
        report.push_error(
            FindingCategory::Schema,
            "executor.timeout_secs must be > 0 (got 0)",
            Some("executor/timeout_secs".into()),
        );
    }
    // a19: opting into the contradiction check requires configuring the
    // LLM block. Fail fast at startup so operators get the misconfig
    // before the first polling iteration spends time on a pre-flight
    // that would have errored at use time anyway.
    if matches!(
        config.executor.change_internal_contradiction_check,
        ContradictionCheckMode::Enabled
    ) && config
        .executor
        .change_internal_contradiction_check_llm
        .is_none()
    {
        report.push_error(
            FindingCategory::Schema,
            "executor.change_internal_contradiction_check is enabled but executor.change_internal_contradiction_check_llm is not configured",
            Some("executor/change_internal_contradiction_check_llm".into()),
        );
    }
    // a62: opting into the change-vs-canonical check (the `[canon]` gate)
    // requires configuring its LLM block, exactly as the `[in]` gate does.
    // Fail fast at startup so the misconfig surfaces before the first
    // polling iteration.
    if matches!(
        config.executor.change_canonical_contradiction_check,
        ContradictionCheckMode::Enabled
    ) && config
        .executor
        .change_canonical_contradiction_check_llm
        .is_none()
    {
        report.push_error(
            FindingCategory::Schema,
            "executor.change_canonical_contradiction_check is enabled but executor.change_canonical_contradiction_check_llm is not configured",
            Some("executor/change_canonical_contradiction_check_llm".into()),
        );
    }
    // a63: opting into the code-implements-spec check (the `[out]` gate)
    // requires configuring its LLM block, exactly as the pre-executor gates
    // do. Fail fast at startup so the misconfig surfaces before the first
    // polling iteration.
    if matches!(
        config.executor.code_implements_spec_check,
        ContradictionCheckMode::Enabled
    ) && config.executor.code_implements_spec_check_llm.is_none()
    {
        report.push_error(
            FindingCategory::Schema,
            "executor.code_implements_spec_check is enabled but executor.code_implements_spec_check_llm is not configured",
            Some("executor/code_implements_spec_check_llm".into()),
        );
    }
    // a25: features.scout.max_items must be within 1..=50.
    if let Err(msg) = config.features.scout.validate() {
        report.push_error(
            FindingCategory::Schema,
            msg,
            Some("features/scout/max_items".into()),
        );
    }
    // a29: features.brownfield_survey.max_capabilities must be within 1..=50.
    if let Err(msg) = config.features.brownfield_survey.validate() {
        report.push_error(
            FindingCategory::Schema,
            msg,
            Some("features/brownfield_survey/max_capabilities".into()),
        );
    }
}

/// Token-route check: for each repo URL, derive owner and verify SOME
/// token source resolves. The check accepts EITHER an explicit
/// `owner_tokens` entry (whose env var is set or whose value is
/// inline), OR a global `github.token` (inline or env-var-set), OR a
/// `github.token_env` env var that is currently set. The repo is in
/// trouble only when none of those produces a usable secret.
fn check_token_routes(config: &Config, report: &mut ValidationReport) {
    for (idx, repo) in config.repositories.iter().enumerate() {
        // a008: a repo with an explicit `forge:` block sources its token from
        // that block's token route, NOT the global `github` config. The check
        // is independent of URL parsing (the block carries its own token), so
        // it runs first — a non-`github.com` host (GitLab / GHE) is the whole
        // point of the block AND must not be rejected as "unparsable github".
        if let Some(forge) = repo.forge.as_ref() {
            if forge.token_route_resolves() {
                continue;
            }
            report.push_error(
                FindingCategory::TokenRoute,
                format!(
                    "repositories[{idx}].url declares a `forge:` block whose token route does not \
                     resolve: set `forge.token` (inline or env-var name) or `forge.token_env`"
                ),
                Some(format!("repositories/{idx}/forge")),
            );
            continue;
        }
        // No `forge:` block → the GitHub/`github.com` default path.
        let owner = match crate::forge::parse_repo_with(None, &repo.url) {
            Ok((o, _r)) => o,
            Err(e) => {
                report.push_error(
                    FindingCategory::TokenRoute,
                    format!(
                        "repositories[{idx}].url could not be parsed: {e}"
                    ),
                    Some(format!("repositories/{idx}/url")),
                );
                continue;
            }
        };
        if token_route_resolves(&config.github, &owner) {
            continue;
        }
        report.push_error(
            FindingCategory::TokenRoute,
            format!(
                "repositories[{idx}].url (owner `{owner}`) has no matching `owner_tokens` entry AND `github.token` is unset AND `github.token_env` ({env}) is not set in the environment",
                env = config.github.token_env,
            ),
            Some(format!("repositories/{idx}/url")),
        );
    }
}

/// True if `owner` has a resolvable token route under `github`. Checks,
/// in order: an `owner_tokens` entry whose source resolves, the global
/// `github.token` whose source resolves, or `github.token_env`'s env
/// var being set. Side-effect: reads env vars (no writes).
fn token_route_resolves(github: &GithubConfig, owner: &str) -> bool {
    if let Some(map) = github.owner_tokens.as_ref()
        && let Some((_k, src)) = map.iter().find(|(k, _)| k.eq_ignore_ascii_case(owner))
        && secret_source_resolves(src)
    {
        return true;
    }
    if let Some(src) = github.token.as_ref()
        && secret_source_resolves(src)
    {
        return true;
    }
    std::env::var(&github.token_env).is_ok()
}

/// True if the secret source can produce a value right now. `Inline`
/// always resolves; `EnvVar` resolves iff `std::env::var(name)` succeeds.
fn secret_source_resolves(src: &SecretSource) -> bool {
    match src {
        SecretSource::Inline { .. } => true,
        SecretSource::EnvVar(name) => std::env::var(name).is_ok(),
    }
}

/// Workspace-collision check: two repos that resolve to the same
/// `local_path` would race each other. Emit ONE error per repo in the
/// colliding group so the operator sees both indices.
fn check_workspace_collisions(config: &Config, report: &mut ValidationReport) {
    use std::collections::HashMap;
    // Resolve paths from this same config so the workspace-derivation
    // here matches what the daemon would resolve at startup.
    let paths = match crate::paths::resolve_daemon_paths(config) {
        Ok(p) => p,
        Err(_) => return, // path resolution failures are surfaced by another check
    };
    let mut by_path: HashMap<std::path::PathBuf, Vec<usize>> = HashMap::new();
    for (idx, repo) in config.repositories.iter().enumerate() {
        let path = crate::workspace::resolve_path(&paths, repo);
        by_path.entry(path).or_default().push(idx);
    }
    for (path, indices) in by_path {
        if indices.len() < 2 {
            continue;
        }
        let others: Vec<String> = indices.iter().map(|i| i.to_string()).collect();
        for &idx in &indices {
            report.push_error(
                FindingCategory::WorkspaceCollision,
                format!(
                    "repositories[{idx}] resolves to workspace path `{}` which is shared with repositories[{others}]",
                    path.display(),
                    others = others.join(", "),
                ),
                Some(format!("repositories/{idx}")),
            );
        }
    }
}

/// Audit-slug check: every name under `audits.defaults`,
/// `audits.settings`, and each repo's per-repo `audits` map must match
/// a slug in `KNOWN_AUDIT_TYPES`. Unknown slugs silently never fire,
/// so we flag them at startup with one error per typo.
fn check_audit_slugs(config: &Config, report: &mut ValidationReport) {
    let known: std::collections::HashSet<&str> = KNOWN_AUDIT_TYPES.iter().copied().collect();
    if let Some(audits) = config.audits.as_ref() {
        for name in audits.defaults.keys() {
            if !known.contains(name.as_str()) {
                report.push_error(
                    FindingCategory::AuditSlug,
                    format!(
                        "audits.defaults.{name}: `{name}` is not a registered audit type (known: {})",
                        KNOWN_AUDIT_TYPES.join(", ")
                    ),
                    Some(format!("audits/defaults/{name}")),
                );
            }
        }
        for name in audits.settings.keys() {
            if !known.contains(name.as_str()) {
                report.push_error(
                    FindingCategory::AuditSlug,
                    format!(
                        "audits.settings.{name}: `{name}` is not a registered audit type (known: {})",
                        KNOWN_AUDIT_TYPES.join(", ")
                    ),
                    Some(format!("audits/settings/{name}")),
                );
            }
        }
    }
    for (idx, repo) in config.repositories.iter().enumerate() {
        if let Some(overrides) = repo.audits.as_ref() {
            for name in overrides.keys() {
                if !known.contains(name.as_str()) {
                    report.push_error(
                        FindingCategory::AuditSlug,
                        format!(
                            "repositories[{idx}].audits.{name}: `{name}` is not a registered audit type (known: {})",
                            KNOWN_AUDIT_TYPES.join(", ")
                        ),
                        Some(format!("repositories/{idx}/audits/{name}")),
                    );
                }
            }
        }
    }
}

/// Path-collision check: the four `paths.*` roles (state, cache, logs,
/// runtime) must resolve to distinct absolute paths. Reuses the same
/// resolution + collision detection that `paths::resolve_daemon_paths`
/// runs at startup, so a passing `check-config` matches startup
/// behaviour byte-for-byte.
fn check_path_collisions(config: &Config, report: &mut ValidationReport) {
    if let Err(e) = crate::paths::resolve_daemon_paths(config) {
        report.push_error(
            FindingCategory::PathCollision,
            format!("{e:#}"),
            Some("paths".into()),
        );
    }
}

/// Secret-source check (WARN-only): for each `*_env`-style reference
/// AND each `SecretSource::EnvVar(...)`, verify the named env var is
/// set in the calling environment. Misses are advisory because the
/// daemon may run under a systemd unit that injects secrets at unit
/// start via `EnvironmentFile=` not visible to the CLI. Inline-only
/// fields are never warned.
fn check_secret_sources(config: &Config, report: &mut ValidationReport) {
    let github_inline = config
        .github
        .token
        .as_ref()
        .map(|s| s.is_inline())
        .unwrap_or(false);
    if !github_inline && std::env::var(&config.github.token_env).is_err() {
        report.push_warn(
            FindingCategory::SecretSource,
            format!(
                "github.token_env references `{}` which is not set in the calling environment",
                config.github.token_env
            ),
            Some("github/token_env".into()),
        );
    }
    if let Some(SecretSource::EnvVar(name)) = config.github.token.as_ref()
        && std::env::var(name).is_err()
    {
        report.push_warn(
            FindingCategory::SecretSource,
            format!("github.token references env var `{name}` which is not set"),
            Some("github/token".into()),
        );
    }
    if let Some(map) = config.github.owner_tokens.as_ref() {
        for (owner, src) in map {
            if let SecretSource::EnvVar(name) = src
                && std::env::var(name).is_err()
            {
                report.push_warn(
                    FindingCategory::SecretSource,
                    format!(
                        "github.owner_tokens[{owner}] references env var `{name}` which is not set"
                    ),
                    Some(format!("github/owner_tokens/{owner}")),
                );
            }
        }
    }
    if let Some(reviewer) = config.reviewer.as_ref() {
        let has_inline = reviewer
            .api_key
            .as_ref()
            .map(|s| s.is_inline())
            .unwrap_or(false);
        if !has_inline
            && let Some(name) = reviewer.api_key_env.as_deref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "reviewer.api_key_env references `{name}` which is not set in the calling environment"
                ),
                Some("reviewer/api_key_env".into()),
            );
        }
        if let Some(SecretSource::EnvVar(name)) = reviewer.api_key.as_ref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!("reviewer.api_key references env var `{name}` which is not set"),
                Some("reviewer/api_key".into()),
            );
        }
    }
    if let Some(rag) = config.canonical_rag.as_ref() {
        let has_inline = rag
            .api_key
            .as_ref()
            .map(|s| s.is_inline())
            .unwrap_or(false);
        if !has_inline
            && let Some(name) = rag.api_key_env.as_deref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "canonical_rag.api_key_env references `{name}` which is not set in the calling environment"
                ),
                Some("canonical_rag/api_key_env".into()),
            );
        }
        if let Some(SecretSource::EnvVar(name)) = rag.api_key.as_ref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!("canonical_rag.api_key references env var `{name}` which is not set"),
                Some("canonical_rag/api_key".into()),
            );
        }
    }
    if let Some(cc_llm) = config
        .executor
        .change_internal_contradiction_check_llm
        .as_ref()
    {
        let has_inline = cc_llm
            .api_key
            .as_ref()
            .map(|s| s.is_inline())
            .unwrap_or(false);
        if !has_inline
            && let Some(name) = cc_llm.api_key_env.as_deref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "executor.change_internal_contradiction_check_llm.api_key_env references `{name}` which is not set in the calling environment"
                ),
                Some("executor/change_internal_contradiction_check_llm/api_key_env".into()),
            );
        }
        if let Some(SecretSource::EnvVar(name)) = cc_llm.api_key.as_ref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "executor.change_internal_contradiction_check_llm.api_key references env var `{name}` which is not set"
                ),
                Some("executor/change_internal_contradiction_check_llm/api_key".into()),
            );
        }
    }
    if let Some(canon_llm) = config
        .executor
        .change_canonical_contradiction_check_llm
        .as_ref()
    {
        let has_inline = canon_llm
            .api_key
            .as_ref()
            .map(|s| s.is_inline())
            .unwrap_or(false);
        if !has_inline
            && let Some(name) = canon_llm.api_key_env.as_deref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "executor.change_canonical_contradiction_check_llm.api_key_env references `{name}` which is not set in the calling environment"
                ),
                Some("executor/change_canonical_contradiction_check_llm/api_key_env".into()),
            );
        }
        if let Some(SecretSource::EnvVar(name)) = canon_llm.api_key.as_ref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "executor.change_canonical_contradiction_check_llm.api_key references env var `{name}` which is not set"
                ),
                Some("executor/change_canonical_contradiction_check_llm/api_key".into()),
            );
        }
    }
    if let Some(cis_llm) = config.executor.code_implements_spec_check_llm.as_ref() {
        let has_inline = cis_llm
            .api_key
            .as_ref()
            .map(|s| s.is_inline())
            .unwrap_or(false);
        if !has_inline
            && let Some(name) = cis_llm.api_key_env.as_deref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "executor.code_implements_spec_check_llm.api_key_env references `{name}` which is not set in the calling environment"
                ),
                Some("executor/code_implements_spec_check_llm/api_key_env".into()),
            );
        }
        if let Some(SecretSource::EnvVar(name)) = cis_llm.api_key.as_ref()
            && std::env::var(name).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "executor.code_implements_spec_check_llm.api_key references env var `{name}` which is not set"
                ),
                Some("executor/code_implements_spec_check_llm/api_key".into()),
            );
        }
    }
    if let Some(chatops) = config.chatops.as_ref() {
        if let Some(slack) = chatops.slack.as_ref() {
            let bot_inline = slack
                .bot_token
                .as_ref()
                .map(|s| s.is_inline())
                .unwrap_or(false);
            if !bot_inline
                && let Some(name) = slack.bot_token_env.as_deref()
                && std::env::var(name).is_err()
            {
                report.push_warn(
                    FindingCategory::SecretSource,
                    format!(
                        "chatops.slack.bot_token_env references `{name}` which is not set in the calling environment"
                    ),
                    Some("chatops/slack/bot_token_env".into()),
                );
            }
            if let Some(SecretSource::EnvVar(name)) = slack.bot_token.as_ref()
                && std::env::var(name).is_err()
            {
                report.push_warn(
                    FindingCategory::SecretSource,
                    format!(
                        "chatops.slack.bot_token references env var `{name}` which is not set"
                    ),
                    Some("chatops/slack/bot_token".into()),
                );
            }
            let app_inline = slack
                .app_token
                .as_ref()
                .map(|s| s.is_inline())
                .unwrap_or(false);
            if !app_inline
                && let Some(name) = slack.app_token_env.as_deref()
                && std::env::var(name).is_err()
            {
                report.push_warn(
                    FindingCategory::SecretSource,
                    format!(
                        "chatops.slack.app_token_env references `{name}` which is not set in the calling environment"
                    ),
                    Some("chatops/slack/app_token_env".into()),
                );
            }
            if let Some(SecretSource::EnvVar(name)) = slack.app_token.as_ref()
                && std::env::var(name).is_err()
            {
                report.push_warn(
                    FindingCategory::SecretSource,
                    format!(
                        "chatops.slack.app_token references env var `{name}` which is not set"
                    ),
                    Some("chatops/slack/app_token".into()),
                );
            }
        }
        if let Some(discord) = chatops.discord.as_ref()
            && std::env::var(&discord.bot_token_env).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "chatops.discord.bot_token_env references `{}` which is not set",
                    discord.bot_token_env
                ),
                Some("chatops/discord/bot_token_env".into()),
            );
        }
        if let Some(teams) = chatops.teams.as_ref()
            && std::env::var(&teams.client_secret_env).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "chatops.teams.client_secret_env references `{}` which is not set",
                    teams.client_secret_env
                ),
                Some("chatops/teams/client_secret_env".into()),
            );
        }
        if let Some(mm) = chatops.mattermost.as_ref()
            && std::env::var(&mm.access_token_env).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "chatops.mattermost.access_token_env references `{}` which is not set",
                    mm.access_token_env
                ),
                Some("chatops/mattermost/access_token_env".into()),
            );
        }
        if let Some(matrix) = chatops.matrix.as_ref()
            && std::env::var(&matrix.access_token_env).is_err()
        {
            report.push_warn(
                FindingCategory::SecretSource,
                format!(
                    "chatops.matrix.access_token_env references `{}` which is not set",
                    matrix.access_token_env
                ),
                Some("chatops/matrix/access_token_env".into()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(yaml: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        (dir, path)
    }

    const VALID_TWO_REPO_YAML: &str = r#"
repositories:
  - url: "git@github.com:owner/repo-a.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
  - url: "git@github.com:owner/repo-b.git"
    local_path: /tmp/workspaces/repo-b
    base_branch: dev
    agent_branch: agent-q
    poll_interval_sec: 1800
executor:
  kind: claude_cli
  command: claude
  timeout_secs: 1800
github:
  token_env: GITHUB_TOKEN
"#;

    /// Resolves the path to the shipped `config.example.yaml` (one level
    /// above this crate's manifest directory). Panics with a clear message
    /// if the file is missing — the example is part of the operator-facing
    /// contract and must always be present at this path.
    fn example_yaml_path() -> std::path::PathBuf {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("manifest dir has a parent")
            .join("config.example.yaml");
        assert!(
            path.exists(),
            "config.example.yaml not found at {}",
            path.display()
        );
        path
    }

    /// Coverage check: every YAML-deserializable field documented in the
    /// Configuration Reference SHALL appear as a substring in
    /// `config.example.yaml` (active key OR comment annotation). Catches
    /// new configurable fields that ship without corresponding example
    /// coverage at CI time, rather than at operator-onboarding time.
    ///
    /// When extending the schema with a new field, you MUST update BOTH
    /// `config.example.yaml` (add an active key or commented annotation)
    /// AND the `EXPECTED_FIELDS` list below. A failure here means one of
    /// the two artifacts was forgotten.
    #[test]
    fn example_yaml_mentions_every_top_level_field() {
        // Top-level keys on `Config` and nested-struct keys. Field names
        // only — values and comments are not asserted, only that each
        // identifier appears somewhere in the example file.
        const EXPECTED_FIELDS: &[&str] = &[
            // Top-level `Config` fields.
            "repositories",
            "executor",
            "github",
            "reviewer",
            "chatops",
            "audits",
            // `RepositoryConfig`.
            "local_path",
            "base_branch",
            "agent_branch",
            "poll_interval_sec",
            "chatops_channel_id",
            "max_changes_per_pr",
            // `ExecutorConfig` + `ExecutorSandboxConfig`.
            "command",
            "timeout_secs",
            "sandbox",
            "implementer_prompt_path",
            "changelog_stylist_prompt_path",
            // a24 nested override blocks under `executor.<area>`.
            "implementer",
            "changelog_stylist",
            "implementer_revision",
            "audit_triage",
            "chat_request_triage",
            // a24 nested override block under `reviewer.code_review`.
            "code_review",
            "perma_stuck_after_failures",
            "startup_jitter_max_secs",
            "inter_iteration_jitter_pct",
            "max_auto_revisions_per_pr",
            // a000: human-revise per-PR cap.
            "max_revise_triggers_per_pr",
            "wipe_drain_timeout_secs",
            "output_format",
            "log_retention_days",
            "busy_marker_stale_threshold_secs",
            "change_internal_contradiction_check",
            "change_internal_contradiction_check_prompt_path",
            "change_internal_contradiction_check_llm",
            "change_canonical_contradiction_check",
            "change_canonical_contradiction_check_prompt_path",
            "change_canonical_contradiction_check_llm",
            "code_implements_spec_check",
            "code_implements_spec_check_prompt_path",
            "code_implements_spec_check_llm",
            "allowed_tools",
            "disallowed_bash_patterns",
            "disallowed_read_paths",
            // a006 `ExecutorSandboxConfig` + per-repo `RepoSandboxConfig`.
            "os_hide",
            "engine_deny",
            "allow_unsandboxed",
            // a013 mask-list edits + strict mode.
            "mask_add",
            "mask_remove",
            "strict_mode",
            // a014 `AgentEnvConfig` (executor.agent_env).
            "agent_env",
            "capture",
            "exclude_add",
            "exclude_remove",
            "expected_toolchains",
            // `GithubConfig`.
            "token_env",
            "token",
            "owner_tokens",
            "fork_owner",
            "recreate_fork_on_reinit",
            // a000: `GithubConfig.command_authorization`.
            "command_authorization",
            "allowed_associations",
            "allowed_users",
            "decline_comment",
            // `ReviewerConfig`.
            "enabled",
            "provider",
            "model",
            "api_key_env",
            "api_key",
            "api_base_url",
            "auto_revise",
            "prompt_budget_chars",
            "mode",
            "max_code_reviews_per_pr",
            "suggest_rereview_threshold",
            "skip_spec_only_prs",
            // `ChatOpsConfig` + provider sub-blocks + `NotificationsConfig`.
            "bot_token_env",
            "bot_token",
            "app_token_env",
            "app_token",
            "listen_channels",
            "dedup_cache_capacity",
            "dedup_cache_ttl_secs",
            "default_channel_id",
            "notifications",
            "start_work",
            "failure_alerts",
            "pr_opened",
            // `AuditsConfig` + `AuditSettings`.
            "defaults",
            "settings",
            "prompt_path",
            "notify_on_clean",
            "extra",
            "max_validation_retries",
            "max_audits_per_iteration",
            // `CanonicalRagConfig` (a21).
            "canonical_rag",
            "api_base_url",
            "chunk_strategy",
            "reembed_on_archive",
            "top_k",
            // a26 OSS-fork support: per-repo blocks.
            "spec_storage",
            "upstream",
            "auto_submit_pr",
            // a34: spec_storage extensions + reviewer skip-spec-only-prs.
            "push_remote",
            "base_branch",
            // a65: workspace-cache size cap.
            "cache",
            "workspaces_max_gb",
        ];

        let path = example_yaml_path();
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("config.example.yaml not found at {}: {e}", path.display()));

        let mut missing: Vec<&str> = Vec::new();
        for field in EXPECTED_FIELDS {
            if !body.contains(field) {
                missing.push(field);
            }
        }
        assert!(
            missing.is_empty(),
            "config.example.yaml is missing documented field(s): {:?}\n\
             Update BOTH `config.example.yaml` (add an active key or commented \
             annotation) AND the EXPECTED_FIELDS list in \
             autocoder/src/config.rs::tests::example_yaml_mentions_every_top_level_field \
             so reviewers can confirm the example, the schema, and this \
             test stay in sync.",
            missing
        );
    }

    /// Parses the actual `config.example.yaml` file shipped at the repo
    /// root. This guards against the example drifting out of sync with the
    /// parser — operators who `cp config.example.yaml config.yaml` should
    /// always end up with a parseable file.
    #[test]
    fn config_example_yaml_parses() {
        let example_path = example_yaml_path();
        let cfg = Config::load_from(&example_path)
            .expect("config.example.yaml must be parseable as Config");
        // Single-repo by default per the design.
        assert_eq!(cfg.repositories.len(), 1);
        assert_eq!(cfg.repositories[0].base_branch, "main");
        assert_eq!(cfg.repositories[0].agent_branch, "agent-q");
        // Reviewer and ChatOps blocks are commented out by default.
        assert!(cfg.reviewer.is_none(), "reviewer must be off by default");
        assert!(cfg.chatops.is_none(), "chatops must be off by default");
    }

    #[test]
    fn loads_example() {
        let (_dir, path) = write_config(VALID_TWO_REPO_YAML);
        let cfg = Config::load_from(&path).expect("config should parse");
        assert_eq!(cfg.repositories.len(), 2);
        assert_eq!(cfg.repositories[0].url, "git@github.com:owner/repo-a.git");
        assert_eq!(cfg.repositories[0].poll_interval_sec, 300);
        assert!(cfg.repositories[0].local_path.is_none());
        assert_eq!(
            cfg.repositories[1].local_path.as_deref(),
            Some(Path::new("/tmp/workspaces/repo-b"))
        );
        assert_eq!(cfg.executor.kind, ExecutorKind::ClaudeCli);
        assert_eq!(cfg.executor.command, "claude");
        assert_eq!(cfg.executor.timeout_secs, 1800);
        assert_eq!(cfg.github.token_env, "GITHUB_TOKEN");
    }

    // ----------------------------------------------------------------
    // a65 workspace-cache config.
    // ----------------------------------------------------------------

    const MINIMAL_YAML: &str = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
executor:
  kind: claude_cli
  command: claude
  timeout_secs: 1800
github:
  token_env: GITHUB_TOKEN
"#;

    #[test]
    fn cache_absent_defaults_to_unbounded() {
        let (_dir, path) = write_config(MINIMAL_YAML);
        let cfg = Config::load_from(&path).expect("config without cache block parses");
        assert!(
            cfg.cache.workspaces_max_gb.is_none(),
            "absent cache block must mean unbounded (None)"
        );
        assert!(cfg.cache.is_empty());
    }

    #[test]
    fn cache_workspaces_max_gb_parses_when_set() {
        let yaml = format!("{MINIMAL_YAML}cache:\n  workspaces_max_gb: 50\n");
        let (_dir, path) = write_config(&yaml);
        let cfg = Config::load_from(&path).expect("config with cache cap parses");
        assert_eq!(cfg.cache.workspaces_max_gb, Some(50));
        assert!(!cfg.cache.is_empty());
    }

    #[test]
    fn cache_workspaces_max_gb_zero_is_rejected() {
        let yaml = format!("{MINIMAL_YAML}cache:\n  workspaces_max_gb: 0\n");
        let (_dir, path) = write_config(&yaml);
        let err = Config::load_from(&path)
            .expect_err("a zero workspace-cache cap must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("workspaces_max_gb"),
            "error must name the field: {msg}"
        );
        assert!(
            msg.contains("greater than 0") || msg.contains("unbounded"),
            "error must explain the constraint: {msg}"
        );
    }

    #[test]
    fn cache_unknown_field_is_rejected() {
        let yaml = format!("{MINIMAL_YAML}cache:\n  bogus_key: 1\n");
        let (_dir, path) = write_config(&yaml);
        Config::load_from(&path)
            .expect_err("an unknown key under `cache:` must be rejected (deny_unknown_fields)");
    }

    #[test]
    fn keyless_cli_roles_load_end_to_end() {
        // The boot fix (agentic-key-optional-and-used Part 1): a keyless `models:`
        // registry (openai_compatible + anthropic + ollama) referenced by the
        // three verifier gates loads — CLI/agentic roles self-authenticate, so
        // `api_key` is optional. (Previously every keyless registry entry failed
        // config-load with "requires api_key".)
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
models:
  reviewer_q:
    provider: openai_compatible
    model: qwen/qwen3.7-max
    api_base_url: https://openrouter.ai/api/v1
    cli: opencode
  claude_sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
    api_base_url: https://api.anthropic.com
  local_spec_check:
    provider: ollama
    model: q:latest
    api_base_url: http://10.42.11.10:11434
    cli: opencode
executor:
  kind: claude_cli
  command: claude
  timeout_secs: 1800
  change_internal_contradiction_check: enabled
  change_internal_contradiction_check_llm:
    model: local_spec_check
  change_canonical_contradiction_check: enabled
  change_canonical_contradiction_check_llm:
    model: claude_sonnet
  code_implements_spec_check: enabled
  code_implements_spec_check_llm:
    model: reviewer_q
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        Config::load_from(&path).expect(
            "keyless CLI/agentic roles (registry + verifier gates) must load — api_key optional",
        );
    }

    #[test]
    fn unbounded_notice_emitted_only_when_cap_unset() {
        // Unset → a notice naming the bounding field.
        let notice = workspace_cache_unbounded_notice(None)
            .expect("unset cap must produce a one-time unbounded notice");
        assert!(
            notice.contains("cache.workspaces_max_gb"),
            "notice must name the bounding field: {notice}"
        );
        assert!(
            notice.to_lowercase().contains("unbounded"),
            "notice must call out the unbounded failure mode: {notice}"
        );
        // Set → no notice.
        assert!(
            workspace_cache_unbounded_notice(Some(50)).is_none(),
            "a bounded cache needs no startup notice"
        );
    }

    /// a64 task 1.1: an unset `reviewer.kind` resolves to `Agentic` (the
    /// field stays optional in YAML; omitting it picks the default), while
    /// explicit `oneshot` / `agentic` values are honored verbatim.
    #[test]
    fn reviewer_kind_defaults_to_agentic_and_honors_explicit() {
        // Unset → the post-a64 default.
        let unset: ReviewerConfig =
            serde_yml::from_str("enabled: true\nmodel: x\n").expect("minimal reviewer parses");
        assert_eq!(
            unset.kind,
            ReviewerKind::Agentic,
            "unset reviewer.kind must default to agentic"
        );
        assert_eq!(
            ReviewerKind::default(),
            ReviewerKind::Agentic,
            "the ReviewerKind Default impl is agentic"
        );

        // Explicit values round-trip unchanged.
        let oneshot: ReviewerConfig =
            serde_yml::from_str("enabled: true\nmodel: x\nkind: oneshot\n")
                .expect("explicit oneshot parses");
        assert_eq!(oneshot.kind, ReviewerKind::Oneshot);
        let agentic: ReviewerConfig =
            serde_yml::from_str("enabled: true\nmodel: x\nkind: agentic\n")
                .expect("explicit agentic parses");
        assert_eq!(agentic.kind, ReviewerKind::Agentic);
    }

    #[test]
    fn applies_defaults_for_executor_and_github() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        assert_eq!(cfg.executor.command, "claude");
        assert_eq!(cfg.executor.timeout_secs, 1800);
        assert_eq!(cfg.github.token_env, "GITHUB_TOKEN");
    }

    #[test]
    fn rejects_unknown_field() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    typo_field: oops
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("should reject unknown field");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("typo_field") || msg.to_lowercase().contains("unknown"),
            "error should mention unknown field; got: {msg}"
        );
    }

    #[test]
    fn missing_config_path_errors_with_path_in_message() {
        // 13.1.2 attestation: orchestrator-cli baseline says missing config
        // "exits with a non-zero status code AND stderr contains a single
        // error line naming the offending file path". Config::load_from is
        // the only step in the dispatch chain that reads the file; if it
        // returns an Err whose message names the path, anyhow's `main`
        // formatting will print that to stderr and the process will exit
        // non-zero (a Result::Err from `main`).
        let path = Path::new("/nonexistent/orchestrator-test-config.yaml");
        let err = Config::load_from(path).expect_err("missing path must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("/nonexistent/orchestrator-test-config.yaml"),
            "error must name the offending path; got: {msg}"
        );
    }

    #[test]
    fn loads_with_reviewer() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  api_base_url: https://api.anthropic.com
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with reviewer should parse");
        let rv = cfg.reviewer.expect("reviewer block should be present");
        assert!(rv.enabled);
        assert_eq!(rv.provider, Some(ReviewerProvider::Anthropic));
        assert_eq!(rv.model, "claude-sonnet-4-6");
        assert_eq!(rv.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(rv.api_base_url.as_deref(), Some("https://api.anthropic.com"));
        assert!(rv.prompt_template_path.is_none());
        // a005: default (field omitted) → `block` (was `false`/off pre-a005).
        assert_eq!(rv.auto_revise, AutoRevise::Block);
    }

    #[test]
    fn reviewer_auto_revise_legacy_alias_explicit_true() {
        // The legacy key `auto_revise_on_block` is still accepted via the
        // serde alias; a005 maps the legacy boolean `true` → `actionable`.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  auto_revise_on_block: true
"#;
        let (_dir, path) = write_config(yaml);
        let cfg =
            Config::load_from(&path).expect("config with legacy auto_revise_on_block should parse");
        let rv = cfg.reviewer.expect("reviewer block should be present");
        assert_eq!(rv.auto_revise, AutoRevise::Actionable);
    }

    #[test]
    fn reviewer_auto_revise_explicit_true() {
        // The canonical key `auto_revise` with a legacy boolean deserializes
        // identically (`true` → `actionable`).
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  auto_revise: true
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with auto_revise should parse");
        let rv = cfg.reviewer.expect("reviewer block should be present");
        assert_eq!(rv.auto_revise, AutoRevise::Actionable);
    }

    /// a005: the canonical tri-state string values deserialize as expected.
    #[test]
    fn reviewer_auto_revise_tristate_strings() {
        for (value, expected) in [
            ("block", AutoRevise::Block),
            ("actionable", AutoRevise::Actionable),
            ("off", AutoRevise::Off),
        ] {
            let yaml = format!(
                r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {{}}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  auto_revise: {value}
"#
            );
            let (_dir, path) = write_config(&yaml);
            let cfg = Config::load_from(&path)
                .unwrap_or_else(|e| panic!("config with auto_revise: {value} should parse: {e}"));
            let rv = cfg.reviewer.expect("reviewer block should be present");
            assert_eq!(rv.auto_revise, expected, "auto_revise: {value}");
        }
    }

    /// a005: the legacy boolean `false` maps to `off`.
    #[test]
    fn reviewer_auto_revise_legacy_false_maps_off() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  auto_revise: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with auto_revise: false should parse");
        let rv = cfg.reviewer.expect("reviewer block should be present");
        assert_eq!(rv.auto_revise, AutoRevise::Off);
    }

    /// a005: an unrecognized string value is a hard config-load error.
    #[test]
    fn reviewer_auto_revise_invalid_string_errors() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  auto_revise: sometimes
"#;
        let (_dir, path) = write_config(yaml);
        assert!(
            Config::load_from(&path).is_err(),
            "an invalid auto_revise string must fail config-load"
        );
    }

    #[test]
    fn reviewer_default_prompt_budget_and_mode() {
        // Omitting `prompt_budget_chars` and `mode` resolves to
        // 2_000_000 chars and `ReviewerMode::Bundled` respectively —
        // the documented "no behavior change vs. before this change"
        // defaults.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("default reviewer parses");
        let rv = cfg.reviewer.expect("reviewer block should be present");
        assert_eq!(rv.prompt_budget_chars, 2_000_000);
        assert_eq!(rv.mode, ReviewerMode::Bundled);
    }

    #[test]
    fn reviewer_explicit_prompt_budget_and_mode() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  prompt_budget_chars: 4000000
  mode: per_change
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("explicit reviewer fields parse");
        let rv = cfg.reviewer.unwrap();
        assert_eq!(rv.prompt_budget_chars, 4_000_000);
        assert_eq!(rv.mode, ReviewerMode::PerChange);
    }

    #[test]
    fn reviewer_unknown_mode_value_errors() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  mode: chaotic
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("invalid mode must error");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("mode")
                || msg.to_lowercase().contains("chaotic")
                || msg.to_lowercase().contains("variant"),
            "error must mention the invalid mode; got: {msg}"
        );
    }

    #[test]
    fn reviewer_disabled_by_default() {
        // Absent block parses to None — opt-in semantics.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.reviewer.is_none());
    }

    #[test]
    fn contradiction_check_default_is_disabled() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("default config parses");
        assert_eq!(
            cfg.executor.change_internal_contradiction_check,
            ContradictionCheckMode::Disabled
        );
        assert!(cfg
            .executor
            .change_internal_contradiction_check_prompt_path
            .is_none());
        assert!(cfg
            .executor
            .change_internal_contradiction_check_llm
            .is_none());
    }

    #[test]
    fn contradiction_check_enabled_without_llm_config_fails_validation() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  change_internal_contradiction_check: enabled
github:
  token:
    value: ghp_test
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses even when llm is missing");
        let report = validate_config(&cfg);
        let msg = report
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            msg.contains(
                "executor.change_internal_contradiction_check is enabled but executor.change_internal_contradiction_check_llm is not configured"
            ),
            "expected fail-fast error message; got: {msg}"
        );
    }

    #[test]
    fn contradiction_check_enabled_with_llm_config_passes_validation() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  change_internal_contradiction_check: enabled
  change_internal_contradiction_check_llm:
    provider: anthropic
    model: claude-haiku-4-5-20251001
    api_key:
      value: sk-ant-inline
github:
  token:
    value: ghp_test
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let report = validate_config(&cfg);
        assert!(
            !report.errors.iter().any(|e| e
                .message
                .contains("change_internal_contradiction_check")),
            "expected no contradiction-check validation error; got: {:#?}",
            report.errors
        );
        let llm = cfg
            .executor
            .change_internal_contradiction_check_llm
            .as_ref()
            .expect("llm block present");
        assert_eq!(llm.provider, Some(ReviewerProvider::Anthropic));
        assert_eq!(llm.model, "claude-haiku-4-5-20251001");
    }

    // a63 (task 1.2): enabling the `[out]` gate without configuring its LLM
    // block is a fail-fast startup error, exactly as the pre-executor gates.
    #[test]
    fn code_implements_spec_check_enabled_without_llm_config_fails_validation() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  code_implements_spec_check: enabled
github:
  token:
    value: ghp_test
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses even when llm is missing");
        let report = validate_config(&cfg);
        let msg = report
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            msg.contains(
                "executor.code_implements_spec_check is enabled but executor.code_implements_spec_check_llm is not configured"
            ),
            "expected fail-fast error message; got: {msg}"
        );
    }

    // a63 (task 1.1/1.2): the `[out]` gate config parses (mode + llm +
    // prompt-path override) AND passes validation when the LLM block is set.
    #[test]
    fn code_implements_spec_check_enabled_with_llm_config_passes_validation() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  code_implements_spec_check: enabled
  code_implements_spec_check_prompt_path: /tmp/custom-out-gate.md
  code_implements_spec_check_llm:
    provider: anthropic
    model: claude-haiku-4-5-20251001
    api_key:
      value: sk-ant-inline
github:
  token:
    value: ghp_test
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let report = validate_config(&cfg);
        assert!(
            !report
                .errors
                .iter()
                .any(|e| e.message.contains("code_implements_spec_check")),
            "expected no code-implements-spec validation error; got: {:#?}",
            report.errors
        );
        assert_eq!(
            cfg.executor.code_implements_spec_check,
            ContradictionCheckMode::Enabled
        );
        assert_eq!(
            cfg.executor.code_implements_spec_check_prompt_path.as_deref(),
            Some(std::path::Path::new("/tmp/custom-out-gate.md"))
        );
        let llm = cfg
            .executor
            .code_implements_spec_check_llm
            .as_ref()
            .expect("llm block present");
        assert_eq!(llm.provider, Some(ReviewerProvider::Anthropic));
        assert_eq!(llm.model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn reviewer_openai_compatible_provider() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  provider: openai_compatible
  model: gpt-4o
  api_key_env: OPENAI_API_KEY
  api_base_url: https://api.openai.com/v1
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let rv = cfg.reviewer.unwrap();
        assert_eq!(rv.provider, Some(ReviewerProvider::OpenAiCompatible));
        assert!(!rv.enabled); // default false when omitted
    }

    #[test]
    fn loads_with_chatops_slack() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    chatops_channel_id: C01234OVERRIDE
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Slack);
        assert_eq!(co.default_channel_id, "C0DEFAULT");
        let slack = co.slack.expect("slack sub-block present");
        assert_eq!(slack.bot_token_env.as_deref(), Some("SLACK_BOT_TOKEN"));
        assert_eq!(
            cfg.repositories[0].chatops_channel_id.as_deref(),
            Some("C01234OVERRIDE")
        );
    }

    #[test]
    fn loads_with_chatops_discord() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: discord
  default_channel_id: "123456789012345678"
  discord:
    bot_token_env: DISCORD_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Discord);
        let d = co.discord.expect("discord sub-block");
        assert_eq!(d.bot_token_env, "DISCORD_BOT_TOKEN");
    }

    #[test]
    fn loads_with_chatops_teams() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: teams
  default_channel_id: "19:abc@thread.tacv2"
  teams:
    tenant_id: "11111111-2222-3333-4444-555555555555"
    client_id: "66666666-7777-8888-9999-aaaaaaaaaaaa"
    client_secret_env: TEAMS_CLIENT_SECRET
    team_id: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Teams);
        let t = co.teams.expect("teams sub-block");
        assert_eq!(t.tenant_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(t.client_id, "66666666-7777-8888-9999-aaaaaaaaaaaa");
        assert_eq!(t.client_secret_env, "TEAMS_CLIENT_SECRET");
        assert_eq!(t.team_id, "bbbbbbbb-cccc-dddd-eeee-ffffffffffff");
    }

    #[test]
    fn loads_with_chatops_mattermost() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: mattermost
  default_channel_id: c1abcd
  mattermost:
    server_url: "https://mattermost.example.com"
    access_token_env: MATTERMOST_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Mattermost);
        let m = co.mattermost.expect("mattermost sub-block");
        assert_eq!(m.server_url, "https://mattermost.example.com");
        assert_eq!(m.access_token_env, "MATTERMOST_TOKEN");
    }

    #[test]
    fn loads_with_chatops_matrix() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: matrix
  default_channel_id: "!abc:server.tld"
  matrix:
    homeserver_url: "https://matrix.example.com"
    access_token_env: MATRIX_ACCESS_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Matrix);
        let m = co.matrix.expect("matrix sub-block");
        assert_eq!(m.homeserver_url, "https://matrix.example.com");
        assert_eq!(m.access_token_env, "MATRIX_ACCESS_TOKEN");
    }

    #[test]
    fn rejects_unknown_chatops_provider() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: irc
  default_channel_id: general-channel
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("unknown chatops.provider must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("irc") || msg.to_lowercase().contains("variant"),
            "error should reject unknown variant; got: {msg}"
        );
    }

    #[test]
    fn repo_overrides_channel() {
        let repo_with_override = RepositoryConfig { forge: None,
            url: "x".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: Some("C_REPO_LEVEL".into()),
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
            sandbox: None,
        };
        assert_eq!(repo_with_override.chatops_channel("C_DEFAULT"), "C_REPO_LEVEL");

        let repo_default = RepositoryConfig { forge: None,
            url: "x".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
            sandbox: None,
        };
        assert_eq!(repo_default.chatops_channel("C_DEFAULT"), "C_DEFAULT");
    }

    #[test]
    fn chatops_block_absent_parses_to_none() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.chatops.is_none());
    }

    #[test]
    fn sandbox_absent_uses_defaults() {
        let resolved = ResolvedSandbox::resolve(None);
        assert_eq!(resolved.allowed_tools, default_allowed_tools());
        assert_eq!(
            resolved.disallowed_bash_patterns,
            default_disallowed_bash_patterns()
        );
        assert_eq!(
            resolved.disallowed_read_paths,
            default_disallowed_read_paths()
        );
        // Defense-in-depth: WebFetch and WebSearch are NOT in the defaults.
        assert!(!resolved.allowed_tools.iter().any(|t| t == "WebFetch"));
        assert!(!resolved.allowed_tools.iter().any(|t| t == "WebSearch"));
        // Spot-check that curl is denied.
        assert!(
            resolved
                .disallowed_bash_patterns
                .iter()
                .any(|p| p.starts_with("curl"))
        );
    }

    #[test]
    fn sandbox_default_blocks_openspec_archive() {
        let patterns = default_disallowed_bash_patterns();
        assert!(
            patterns.contains(&"openspec archive:*".to_string()),
            "default sandbox must deny `openspec archive`"
        );
        assert!(
            patterns.contains(&"openspec unarchive:*".to_string()),
            "default sandbox must deny `openspec unarchive`"
        );
    }

    #[test]
    fn sandbox_partial_override_uses_defaults_per_field() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  sandbox:
    allowed_tools: [Read, Write]
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("partial sandbox should parse");
        let resolved = ResolvedSandbox::resolve(cfg.executor.sandbox.as_ref());
        // Operator's allowed_tools wins.
        assert_eq!(
            resolved.allowed_tools,
            vec!["Read".to_string(), "Write".to_string()]
        );
        // Other fields fall back to safe defaults.
        assert_eq!(
            resolved.disallowed_bash_patterns,
            default_disallowed_bash_patterns()
        );
        assert_eq!(
            resolved.disallowed_read_paths,
            default_disallowed_read_paths()
        );
    }

    #[test]
    fn sandbox_full_override_uses_operator_values_only() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  sandbox:
    allowed_tools: [Read]
    disallowed_bash_patterns: ["custom-pat:*"]
    disallowed_read_paths: ["/custom/path/**"]
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("full sandbox should parse");
        let resolved = ResolvedSandbox::resolve(cfg.executor.sandbox.as_ref());
        assert_eq!(resolved.allowed_tools, vec!["Read".to_string()]);
        assert_eq!(
            resolved.disallowed_bash_patterns,
            vec!["custom-pat:*".to_string()]
        );
        assert_eq!(
            resolved.disallowed_read_paths,
            vec!["/custom/path/**".to_string()]
        );
    }

    // a006 / task 8.5: the secure default applies when neither global nor
    // per-repo sets a toggle — both ON, and no relaxed-posture WARN.
    #[test]
    fn sandbox_toggles_secure_default_when_unset() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let repo = &cfg.repositories[0];
        let toggles = repo.resolved_sandbox_toggles(cfg.executor.sandbox.as_ref());
        assert!(toggles.os_hide, "os_hide defaults ON");
        assert!(toggles.engine_deny, "engine_deny defaults ON");
        assert!(
            repo.relaxed_sandbox_warning(cfg.executor.sandbox.as_ref())
                .is_none(),
            "the secure default emits no relaxed-posture WARN"
        );
    }

    // a006 / task 8.5: per-repo overrides global; repos without a per-repo
    // value keep the global value.
    #[test]
    fn sandbox_toggles_per_repo_overrides_global() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/relaxed.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    sandbox:
      os_hide: false
  - url: "git@github.com:owner/strict.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  sandbox:
    os_hide: true
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let global = cfg.executor.sandbox.as_ref();
        // The repo that overrides → os_hide off; engine_deny still defaults on.
        let relaxed = cfg.repositories[0].resolved_sandbox_toggles(global);
        assert!(!relaxed.os_hide, "per-repo os_hide off wins over global on");
        assert!(relaxed.engine_deny, "unset engine_deny falls back to default ON");
        // The repo without an override → global os_hide on.
        let strict = cfg.repositories[1].resolved_sandbox_toggles(global);
        assert!(strict.os_hide, "repo without override keeps global os_hide on");
    }

    // a006 / task 8.6: a repo running with a toggle off emits a relaxed-posture
    // WARN naming that toggle (assert a WARN fires, not exact wording).
    #[test]
    fn sandbox_relaxed_posture_warns_naming_the_off_toggle() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    sandbox:
      os_hide: false
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let warn = cfg.repositories[0]
            .relaxed_sandbox_warning(cfg.executor.sandbox.as_ref())
            .expect("a repo with os_hide off must emit a relaxed-posture WARN");
        assert!(warn.contains("os_hide"), "the WARN names the off toggle: {warn}");
        // engine_deny still on → it is NOT named.
        assert!(!warn.contains("engine_deny"), "an ON toggle is not named: {warn}");
    }

    // a006: a global toggle off with no per-repo value resolves off (and warns).
    #[test]
    fn sandbox_global_off_flows_to_repo_without_override() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  sandbox:
    engine_deny: false
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let toggles = cfg.repositories[0].resolved_sandbox_toggles(cfg.executor.sandbox.as_ref());
        assert!(toggles.os_hide, "os_hide still defaults on");
        assert!(!toggles.engine_deny, "global engine_deny off flows to the repo");
        let warn = cfg.repositories[0]
            .relaxed_sandbox_warning(cfg.executor.sandbox.as_ref())
            .expect("engine_deny off must warn");
        assert!(warn.contains("engine_deny"), "{warn}");
    }

    // a006: the `allow_unsandboxed` opt-in parses and defaults false.
    #[test]
    fn sandbox_allow_unsandboxed_parses_and_defaults_false() {
        let default_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_d, p) = write_config(default_yaml);
        let cfg = Config::load_from(&p).expect("parses");
        assert!(
            !cfg.executor
                .sandbox
                .as_ref()
                .map(|s| s.allow_unsandboxed)
                .unwrap_or(false),
            "allow_unsandboxed defaults false"
        );

        let opt_in = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  sandbox:
    allow_unsandboxed: true
github: {}
"#;
        let (_d2, p2) = write_config(opt_in);
        let cfg2 = Config::load_from(&p2).expect("parses");
        assert!(cfg2.executor.sandbox.as_ref().unwrap().allow_unsandboxed);
    }

    // a013: strict_mode resolves per-repo → global → default OFF.
    #[test]
    fn sandbox_strict_mode_resolves_per_repo_over_global() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/strict.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    sandbox:
      strict_mode: true
  - url: "git@github.com:owner/normal.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  sandbox:
    strict_mode: false
github: {}
"#;
        let (_d, p) = write_config(yaml);
        let cfg = Config::load_from(&p).expect("parses");
        let global = cfg.executor.sandbox.as_ref();
        assert!(
            cfg.repositories[0].resolved_sandbox_toggles(global).strict_mode,
            "per-repo strict_mode on wins over global off"
        );
        assert!(
            !cfg.repositories[1].resolved_sandbox_toggles(global).strict_mode,
            "a repo without an override keeps the global strict_mode off"
        );
        // strict mode is MORE secure → no relaxed-posture WARN.
        assert!(
            cfg.repositories[0]
                .relaxed_sandbox_warning(global)
                .is_none(),
            "strict mode does not emit a relaxed-posture WARN"
        );
    }

    // a013: mask edits are additive — the global list, then the per-repo list.
    #[test]
    fn sandbox_mask_edits_are_additive_global_then_repo() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    sandbox:
      mask_add: ["~/repo-extra"]
      mask_remove: ["~/.netrc"]
executor:
  kind: claude_cli
  sandbox:
    mask_add: ["~/global-extra"]
    mask_remove: ["~/.aws"]
github: {}
"#;
        let (_d, p) = write_config(yaml);
        let cfg = Config::load_from(&p).expect("parses");
        let t = cfg.repositories[0].resolved_sandbox_toggles(cfg.executor.sandbox.as_ref());
        assert!(t.mask_add.contains(&"~/global-extra".to_string()));
        assert!(t.mask_add.contains(&"~/repo-extra".to_string()));
        assert!(t.mask_remove.contains(&"~/.aws".to_string()));
        assert!(t.mask_remove.contains(&"~/.netrc".to_string()));
    }

    // a013: removing a DEFAULT mask entry is a relaxed posture, logged and
    // named at startup; adding a path (or removing a non-default) is not.
    #[test]
    fn sandbox_removing_default_mask_entry_warns_naming_it() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/exposed.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    sandbox:
      mask_remove: ["~/.ssh"]
      mask_add: ["~/custom"]
  - url: "git@github.com:owner/quiet.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    sandbox:
      mask_add: ["~/custom"]
      mask_remove: ["~/not-a-default"]
executor:
  kind: claude_cli
github: {}
"#;
        let (_d, p) = write_config(yaml);
        let cfg = Config::load_from(&p).expect("parses");
        let global = cfg.executor.sandbox.as_ref();
        let warn = cfg.repositories[0]
            .relaxed_sandbox_warning(global)
            .expect("removing a default mask entry must warn");
        assert!(warn.contains(".ssh"), "the WARN names the exposed default: {warn}");
        // Adding a path / removing a non-default path is NOT a relaxed posture.
        assert!(
            cfg.repositories[1].relaxed_sandbox_warning(global).is_none(),
            "mask_add and removing a non-default entry emit no WARN"
        );
    }

    #[test]
    fn loads_fork_owner() {
        let yaml = r#"
repositories:
  - url: "git@github.com:upstream/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  fork_owner: machine-user-handle
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with fork_owner should parse");
        assert_eq!(cfg.github.fork_owner.as_deref(), Some("machine-user-handle"));
    }

    #[test]
    fn fork_owner_absent_defaults_to_none() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.github.fork_owner.is_none());
    }

    // ---------- a000: command_authorization + max_revise_triggers_per_pr ----------

    #[test]
    fn command_authorization_default_is_default_deny() {
        let auth = CommandAuthorizationConfig::default();
        assert_eq!(
            auth.allowed_associations,
            vec![
                "OWNER".to_string(),
                "MEMBER".to_string(),
                "COLLABORATOR".to_string()
            ]
        );
        assert!(auth.allowed_users.is_empty());
        assert!(!auth.decline_comment);
        assert!(auth.validate().is_ok());
    }

    #[test]
    fn command_authorization_validate_rejects_unknown_association() {
        let auth = CommandAuthorizationConfig {
            allowed_associations: vec!["OWNER".to_string(), "OWENR".to_string()],
            allowed_users: Vec::new(),
            decline_comment: false,
        };
        let err = auth.validate().expect_err("typo'd association must be rejected");
        assert!(err.contains("OWENR"), "error names the offending value: {err}");
    }

    #[test]
    fn command_authorization_validate_rejects_blank_allowed_user() {
        // A whitespace-only login (the classic `allowed_users: [" "]`
        // typo) must be rejected so it fails fast rather than sitting in
        // the allowlist as a login the runtime guard can never match.
        let auth = CommandAuthorizationConfig {
            allowed_associations: default_allowed_associations(),
            allowed_users: vec!["trusted-dev".to_string(), "  ".to_string()],
            decline_comment: false,
        };
        let err = auth
            .validate()
            .expect_err("whitespace-only allowed_users entry must be rejected");
        assert!(
            err.contains("allowed_users"),
            "error names the offending field: {err}"
        );
    }

    #[test]
    fn command_authorization_validate_rejects_empty_allowed_user() {
        let auth = CommandAuthorizationConfig {
            allowed_associations: default_allowed_associations(),
            allowed_users: vec![String::new()],
            decline_comment: false,
        };
        assert!(
            auth.validate().is_err(),
            "empty-string allowed_users entry must be rejected"
        );
    }

    #[test]
    fn command_authorization_validate_accepts_nonblank_allowed_users() {
        let auth = CommandAuthorizationConfig {
            allowed_associations: default_allowed_associations(),
            allowed_users: vec!["trusted-dev".to_string(), "another-dev".to_string()],
            decline_comment: false,
        };
        assert!(
            auth.validate().is_ok(),
            "non-blank logins must pass validation"
        );
    }

    #[test]
    fn load_from_rejects_blank_allowed_user() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  command_authorization:
    allowed_users: [" "]
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("blank allowed_users entry must fail load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("allowed_users"),
            "load error must name the offending field: {msg}"
        );
    }

    #[test]
    fn command_authorization_absent_uses_default_deny() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config without the block should parse");
        let auth = &cfg.github.command_authorization;
        assert_eq!(
            auth.allowed_associations,
            vec![
                "OWNER".to_string(),
                "MEMBER".to_string(),
                "COLLABORATOR".to_string()
            ]
        );
        assert!(auth.allowed_users.is_empty());
        assert!(!auth.decline_comment);
    }

    #[test]
    fn command_authorization_block_parses() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  command_authorization:
    allowed_associations: [OWNER, MEMBER]
    allowed_users: [trusted-dev]
    decline_comment: true
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("command_authorization block should parse");
        let auth = &cfg.github.command_authorization;
        assert_eq!(
            auth.allowed_associations,
            vec!["OWNER".to_string(), "MEMBER".to_string()]
        );
        assert_eq!(auth.allowed_users, vec!["trusted-dev".to_string()]);
        assert!(auth.decline_comment);
    }

    #[test]
    fn load_from_rejects_unknown_association() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  command_authorization:
    allowed_associations: [OWNER, BOGUS_VALUE]
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("unknown association must fail load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("BOGUS_VALUE"),
            "load error must name the offending association: {msg}"
        );
    }

    #[test]
    fn max_revise_triggers_per_pr_defaults_to_10() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        assert_eq!(cfg.executor.max_revise_triggers_per_pr, 10);
    }

    #[test]
    fn max_revise_triggers_per_pr_explicit_value_is_kept() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  max_revise_triggers_per_pr: 3
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        assert_eq!(cfg.executor.max_revise_triggers_per_pr, 3);
    }

    #[test]
    fn recreate_fork_on_reinit_defaults_to_false() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(!cfg.github.recreate_fork_on_reinit);
    }

    #[test]
    fn recreate_fork_on_reinit_parses_true() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  fork_owner: machine-user
  recreate_fork_on_reinit: true
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.github.recreate_fork_on_reinit);
        assert_eq!(cfg.github.fork_owner.as_deref(), Some("machine-user"));
    }

    #[test]
    fn recreate_fork_on_reinit_parses_false() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  recreate_fork_on_reinit: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(!cfg.github.recreate_fork_on_reinit);
    }

    #[test]
    fn loads_with_owner_tokens() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  owner_tokens:
    rabbeverly: PERSONAL_GH_TOKEN
    my-org-a: ORG_A_GH_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with owner_tokens should parse");
        let map = cfg
            .github
            .owner_tokens
            .expect("owner_tokens block should be present");
        match map.get("rabbeverly").unwrap() {
            SecretSource::EnvVar(name) => assert_eq!(name, "PERSONAL_GH_TOKEN"),
            _ => panic!("expected env-var source for rabbeverly"),
        }
        match map.get("my-org-a").unwrap() {
            SecretSource::EnvVar(name) => assert_eq!(name, "ORG_A_GH_TOKEN"),
            _ => panic!("expected env-var source for my-org-a"),
        }
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn owner_tokens_optional() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config without owner_tokens should parse");
        assert!(cfg.github.owner_tokens.is_none());
    }

    #[test]
    fn secret_source_parses_bare_string_as_env_var() {
        let s: SecretSource = serde_yml::from_str("MY_VAR").unwrap();
        match s {
            SecretSource::EnvVar(name) => assert_eq!(name, "MY_VAR"),
            _ => panic!("bare string must parse as EnvVar"),
        }
    }

    #[test]
    fn secret_source_parses_object_as_inline() {
        let s: SecretSource = serde_yml::from_str("value: \"abc123\"").unwrap();
        match s {
            SecretSource::Inline { value } => assert_eq!(value, "abc123"),
            _ => panic!("`{{value: ...}}` must parse as Inline"),
        }
    }

    #[test]
    fn secret_source_resolve_env_var_set() {
        // SAFETY: unique env var name per test, no parallel mutator.
        unsafe { std::env::set_var("AUTOCODER_TEST_SECRET_RESOLVE_SET", "x") };
        let s = SecretSource::EnvVar("AUTOCODER_TEST_SECRET_RESOLVE_SET".into());
        assert_eq!(s.resolve("test.field").unwrap(), "x");
        unsafe { std::env::remove_var("AUTOCODER_TEST_SECRET_RESOLVE_SET") };
    }

    #[test]
    fn secret_source_resolve_env_var_unset_names_field() {
        unsafe { std::env::remove_var("AUTOCODER_TEST_SECRET_RESOLVE_UNSET") };
        let s = SecretSource::EnvVar("AUTOCODER_TEST_SECRET_RESOLVE_UNSET".into());
        let err = s.resolve("my.field.label").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("AUTOCODER_TEST_SECRET_RESOLVE_UNSET"),
            "error must name env var; got: {msg}"
        );
        assert!(
            msg.contains("my.field.label"),
            "error must name field label; got: {msg}"
        );
    }

    #[test]
    fn secret_source_resolve_inline() {
        let s = SecretSource::Inline {
            value: "verbatim".into(),
        };
        assert_eq!(s.resolve("any.label").unwrap(), "verbatim");
    }

    #[test]
    fn secret_source_describe_redacts_inline_value() {
        let inline = SecretSource::Inline {
            value: "super-secret-token-xyz".into(),
        };
        let desc = inline.describe("github.token");
        assert!(
            !desc.contains("super-secret-token-xyz"),
            "describe must NEVER expose the inline value; got: {desc}"
        );
        assert_eq!(desc, "inline (github.token)");

        let env = SecretSource::EnvVar("MY_VAR".into());
        assert_eq!(env.describe("anything"), "env var MY_VAR");
    }

    #[test]
    fn loads_github_token_inline() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token:
    value: "ghp_inlinepat"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with inline github.token should parse");
        match cfg.github.token.unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "ghp_inlinepat"),
            _ => panic!("expected inline source"),
        }
        // token_env default still present:
        assert_eq!(cfg.github.token_env, "GITHUB_TOKEN");
    }

    #[test]
    fn loads_owner_tokens_mixed_env_and_inline() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  owner_tokens:
    org-with-env-var: ORG_ENV_VAR
    org-with-inline:
      value: "ghp_inlinevalue"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("mixed owner_tokens should parse");
        let map = cfg.github.owner_tokens.expect("present");
        match map.get("org-with-env-var").unwrap() {
            SecretSource::EnvVar(n) => assert_eq!(n, "ORG_ENV_VAR"),
            _ => panic!("env-var entry mis-parsed"),
        }
        match map.get("org-with-inline").unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "ghp_inlinevalue"),
            _ => panic!("inline entry mis-parsed"),
        }
    }

    #[test]
    fn loads_slack_inline_bot_token() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
    bot_token:
      value: "xoxb-inline"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("inline slack bot_token should parse");
        let co = cfg.chatops.unwrap();
        let slack = co.slack.unwrap();
        match slack.bot_token.unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "xoxb-inline"),
            _ => panic!("expected inline slack bot token"),
        }
        assert_eq!(slack.bot_token_env.as_deref(), Some("SLACK_BOT_TOKEN"));
    }

    #[test]
    fn loads_reviewer_inline_api_key_without_env_name() {
        // The point of the fix: with `api_key` inline set, `api_key_env`
        // is not required in YAML.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key:
    value: "sk-ant-inline-only"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path)
            .expect("reviewer with inline api_key and no api_key_env should parse");
        let rv = cfg.reviewer.unwrap();
        assert!(rv.api_key_env.is_none());
        assert!(rv.api_key.is_some());
    }

    #[test]
    fn loads_slack_app_token_via_env() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
    app_token_env: SLACK_APP_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("app_token_env should parse");
        let slack = cfg.chatops.unwrap().slack.unwrap();
        assert_eq!(slack.app_token_env.as_deref(), Some("SLACK_APP_TOKEN"));
        assert!(slack.app_token.is_none());
    }

    #[test]
    fn loads_slack_app_token_inline() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
    app_token:
      value: "xapp-1-inline"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("inline app_token should parse");
        let slack = cfg.chatops.unwrap().slack.unwrap();
        assert!(slack.app_token_env.is_none());
        match slack.app_token.unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "xapp-1-inline"),
            _ => panic!("expected inline app token"),
        }
    }

    #[test]
    fn slack_missing_app_token_env_var_errors_on_resolve() {
        // We don't fail at load time when the env var is missing — we
        // fail at resolve time, with a message naming the env var.
        // SAFETY: SAFE-RUST-001 — single-threaded test, no other thread
        // reads or writes this env var.
        unsafe { std::env::remove_var("APP_TOKEN_NEVER_SET_RACEY") };
        let source = SecretSource::EnvVar("APP_TOKEN_NEVER_SET_RACEY".to_string());
        let err = source
            .resolve("chatops.slack.app_token_env=APP_TOKEN_NEVER_SET_RACEY")
            .expect_err("missing env var must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("APP_TOKEN_NEVER_SET_RACEY"));
    }

    #[test]
    fn slack_unexpected_token_prefix_warns_not_errors() {
        // Both checks are advisory: load_from succeeds, and the warn
        // helper produces one or both messages depending on which
        // tokens look off. Mainly we assert no hard failure.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token:
      value: "not-xoxb-shaped"
    app_token:
      value: "not-xapp-shaped"
"#;
        let (_dir, path) = write_config(yaml);
        let _cfg = Config::load_from(&path).expect("non-conforming prefix must not block load");

        let (bot, app) = warn_on_unexpected_slack_token_prefixes(
            Some("not-xoxb-shaped"),
            Some("not-xapp-shaped"),
        );
        assert!(bot.is_some(), "bot token mismatch must warn");
        assert!(app.is_some(), "app token mismatch must warn");
        assert!(bot.as_deref().unwrap().contains("xoxb-"));
        assert!(app.as_deref().unwrap().contains("xapp-"));

        // Conforming prefixes do not warn.
        let (bot, app) = warn_on_unexpected_slack_token_prefixes(
            Some("xoxb-fine"),
            Some("xapp-fine"),
        );
        assert!(bot.is_none());
        assert!(app.is_none());
    }

    #[test]
    fn loads_slack_inline_bot_token_without_env_name() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token:
      value: "xoxb-inline-only"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path)
            .expect("slack with inline bot_token and no bot_token_env should parse");
        let co = cfg.chatops.unwrap();
        let slack = co.slack.unwrap();
        assert!(slack.bot_token_env.is_none());
        assert!(slack.bot_token.is_some());
    }

    #[test]
    fn loads_reviewer_inline_api_key() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  api_key:
    value: "sk-ant-inline"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("inline reviewer api_key should parse");
        let rv = cfg.reviewer.unwrap();
        match rv.api_key.unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "sk-ant-inline"),
            _ => panic!("expected inline reviewer key"),
        }
        // api_key_env still present:
        assert_eq!(rv.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn loads_notifications_block() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    start_work: false
    failure_alerts: true
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        let co = cfg.chatops.expect("chatops present");
        let n = co.notifications.clone().expect("notifications present");
        assert!(!n.start_work);
        assert!(n.failure_alerts);
        assert!(!NotificationsConfig::start_work_enabled(Some(&co)));
        assert!(NotificationsConfig::failure_alerts_enabled(Some(&co)));
    }

    #[test]
    fn notifications_partial_populated_defaults_other_to_true() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    start_work: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        let co = cfg.chatops.expect("chatops present");
        let n = co.notifications.expect("notifications present");
        assert!(!n.start_work);
        assert!(n.failure_alerts, "omitted field must default to true");
    }

    #[test]
    fn notifications_rejects_unknown_field() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    start_work: true
    typo_field: oops
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("unknown field in notifications must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("typo_field") || msg.to_lowercase().contains("unknown"),
            "error should mention unknown field; got: {msg}"
        );
    }

    #[test]
    fn pr_opened_default_is_true_when_block_absent() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops present");
        assert!(NotificationsConfig::pr_opened_enabled(Some(&co)));
        assert!(NotificationsConfig::pr_opened_enabled(None));
    }

    #[test]
    fn pr_opened_default_is_true_when_field_absent() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    start_work: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops present");
        let n = co.notifications.clone().expect("notifications present");
        assert!(n.pr_opened, "field defaults to true when omitted");
        assert!(NotificationsConfig::pr_opened_enabled(Some(&co)));
    }

    #[test]
    fn pr_opened_explicit_false_disables() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    pr_opened: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops present");
        let n = co.notifications.clone().expect("notifications present");
        assert!(!n.pr_opened);
        assert!(!NotificationsConfig::pr_opened_enabled(Some(&co)));
    }

    #[test]
    fn notifications_absent_block_defaults_both_true() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        let co = cfg.chatops.expect("chatops present");
        assert!(co.notifications.is_none(), "block must be absent");
        // Helpers must default to true when block omitted.
        assert!(NotificationsConfig::start_work_enabled(Some(&co)));
        assert!(NotificationsConfig::failure_alerts_enabled(Some(&co)));
        // Helpers must also default to true when chatops itself is None.
        assert!(NotificationsConfig::start_work_enabled(None));
        assert!(NotificationsConfig::failure_alerts_enabled(None));
    }

    #[test]
    fn executor_perma_stuck_default_is_two() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.executor.perma_stuck_after_failures.is_none());
        assert_eq!(cfg.executor.perma_stuck_threshold(), 2);
    }

    #[test]
    fn executor_perma_stuck_clamps_zero_to_one() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  perma_stuck_after_failures: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.perma_stuck_after_failures, Some(0));
        assert_eq!(
            cfg.executor.perma_stuck_threshold(),
            1,
            "zero must clamp to one"
        );
    }

    #[test]
    fn executor_perma_stuck_accepts_custom_value() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  perma_stuck_after_failures: 5
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.perma_stuck_after_failures, Some(5));
        assert_eq!(cfg.executor.perma_stuck_threshold(), 5);
    }

    #[test]
    fn max_changes_per_pr_global_default_is_3() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.repositories[0].max_changes_per_pr.is_none());
        assert!(cfg.executor.max_changes_per_pr.is_none());
        assert_eq!(cfg.repositories[0].max_changes_per_pr(&cfg.executor), 3);
    }

    #[test]
    fn max_changes_per_pr_executor_fallback_applies() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  max_changes_per_pr: 2
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.max_changes_per_pr, Some(2));
        assert_eq!(cfg.repositories[0].max_changes_per_pr(&cfg.executor), 2);
    }

    #[test]
    fn max_changes_per_pr_per_repo_override_takes_precedence() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    max_changes_per_pr: 5
executor:
  kind: claude_cli
  max_changes_per_pr: 2
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.repositories[0].max_changes_per_pr, Some(5));
        assert_eq!(cfg.executor.max_changes_per_pr, Some(2));
        assert_eq!(
            cfg.repositories[0].max_changes_per_pr(&cfg.executor),
            5,
            "per-repo override must win over executor-level"
        );
    }

    #[test]
    fn max_changes_per_pr_zero_clamps_to_1() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    max_changes_per_pr: 0
executor:
  kind: claude_cli
  max_changes_per_pr: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        // Raw configured values preserved so the WARN log can name them.
        assert_eq!(cfg.repositories[0].max_changes_per_pr, Some(0));
        assert_eq!(cfg.executor.max_changes_per_pr, Some(0));
        // Effective cap is clamped.
        assert_eq!(cfg.repositories[0].max_changes_per_pr(&cfg.executor), 1);
    }

    #[test]
    fn startup_jitter_default_is_30() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.executor.startup_jitter_max_secs.is_none());
        assert_eq!(cfg.executor.startup_jitter_max_secs(), 30);
    }

    #[test]
    fn startup_jitter_explicit_zero_is_zero() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  startup_jitter_max_secs: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.startup_jitter_max_secs, Some(0));
        assert_eq!(cfg.executor.startup_jitter_max_secs(), 0);
    }

    #[test]
    fn inter_iteration_jitter_default_is_10() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.executor.inter_iteration_jitter_pct.is_none());
        assert_eq!(cfg.executor.inter_iteration_jitter_pct(), 10);
    }

    #[test]
    fn max_auto_revisions_per_pr_default_is_5() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.max_auto_revisions_per_pr, 5);
        assert_eq!(cfg.executor.max_auto_revisions_per_pr_clamped(), 5);
    }

    /// Task 5.3: the legacy key `executor.max_revisions_per_pr` still loads
    /// via the serde alias into `max_auto_revisions_per_pr`, AND the new
    /// key loads identically.
    #[test]
    fn legacy_max_revisions_per_pr_key_loads_via_alias() {
        let legacy = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  max_revisions_per_pr: 8
github: {}
"#;
        let (_dir, path) = write_config(legacy);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.max_auto_revisions_per_pr, 8);

        let modern = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  max_auto_revisions_per_pr: 8
github: {}
"#;
        let (_dir2, path2) = write_config(modern);
        let cfg2 = Config::load_from(&path2).unwrap();
        assert_eq!(cfg2.executor.max_auto_revisions_per_pr, 8);
    }

    #[test]
    fn max_auto_revisions_per_pr_explicit_zero_disables_feature() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  max_auto_revisions_per_pr: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.max_auto_revisions_per_pr, 0);
        assert_eq!(cfg.executor.max_auto_revisions_per_pr_clamped(), 0);
    }

    #[test]
    fn max_auto_revisions_per_pr_at_ceiling_is_kept() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  max_auto_revisions_per_pr: 20
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.max_auto_revisions_per_pr, 20);
        assert_eq!(cfg.executor.max_auto_revisions_per_pr_clamped(), 20);
    }

    #[test]
    fn max_auto_revisions_per_pr_above_ceiling_is_clamped() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  max_auto_revisions_per_pr: 50
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.max_auto_revisions_per_pr, 50);
        assert_eq!(cfg.executor.max_auto_revisions_per_pr_clamped(), 20);
    }

    /// a47 Task 2.1: a reviewer block with no `max_code_reviews_per_pr` /
    /// `suggest_rereview_threshold` keys defaults the former to `None`
    /// (UNLIMITED) and the latter to `None`.
    #[test]
    fn reviewer_code_review_extension_fields_default_round_trip() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let r = cfg.reviewer.expect("reviewer block present");
        assert_eq!(r.max_code_reviews_per_pr, None);
        assert!(r.suggest_rereview_threshold.is_none());
    }

    /// Task 1.5: a `suggest_rereview_threshold` outside `0.0..=1.0`
    /// fails config-load with a message naming the field AND the valid
    /// range.
    #[test]
    fn reviewer_suggest_rereview_threshold_out_of_range_rejected() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  suggest_rereview_threshold: 1.5
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("out-of-range threshold must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reviewer.suggest_rereview_threshold")
                && msg.contains("0.0..=1.0"),
            "error must name field AND range; got: {msg}"
        );
    }

    /// Above-ceiling `reviewer.max_code_reviews_per_pr` clamps down at
    /// startup with the WARN message documented in
    /// `clamp_max_code_reviews_per_pr`. The raw stored value reflects the
    /// CLAMPED value (matches `executor.max_auto_revisions_per_pr`'s
    /// pattern where the field is rewritten in place by `Config::load_from`).
    #[test]
    fn reviewer_max_code_reviews_per_pr_above_ceiling_is_clamped() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  max_code_reviews_per_pr: 50
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let r = cfg.reviewer.expect("reviewer block present");
        assert_eq!(
            r.max_code_reviews_per_pr,
            Some(MAX_CODE_REVIEWS_PER_PR_CEILING)
        );
        let (clamped, warn) = clamp_max_code_reviews_per_pr(50);
        assert_eq!(clamped, MAX_CODE_REVIEWS_PER_PR_CEILING);
        assert!(warn.is_some());
    }

    /// a47 Task 5.4 (config layer): an explicit `max_code_reviews_per_pr`
    /// below the ceiling loads as `Some(n)`.
    #[test]
    fn reviewer_max_code_reviews_per_pr_explicit_loads_as_some() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  max_code_reviews_per_pr: 3
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let r = cfg.reviewer.expect("reviewer block present");
        assert_eq!(r.max_code_reviews_per_pr, Some(3));
    }

    #[test]
    fn inter_iteration_jitter_above_100_is_clamped() {
        // u8 fits up to 255; values above 100 must clamp to 100 so the
        // negative offset cannot exceed the base interval.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  inter_iteration_jitter_pct: 250
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.inter_iteration_jitter_pct, Some(250));
        assert_eq!(cfg.executor.inter_iteration_jitter_pct(), 100);
    }

    #[test]
    fn wipe_drain_timeout_defaults_to_thirty_when_absent() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.wipe_drain_timeout_secs, 30);
        assert_eq!(cfg.executor.wipe_drain_timeout_secs_clamped(), 30);
    }

    #[test]
    fn wipe_drain_timeout_zero_is_permitted() {
        // 0 skips the await; the wipe runs immediately whether the
        // iteration responded or not. Useful for sites that always want
        // the wipe NOW and don't care about a clean drain.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  wipe_drain_timeout_secs: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.wipe_drain_timeout_secs, 0);
        assert_eq!(cfg.executor.wipe_drain_timeout_secs_clamped(), 0);
        let (clamped, warn) = clamp_wipe_drain_timeout_secs(0);
        assert_eq!(clamped, 0);
        assert!(warn.is_none(), "no warn for 0");
    }

    #[test]
    fn wipe_drain_timeout_three_hundred_is_permitted_no_warn() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  wipe_drain_timeout_secs: 300
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.wipe_drain_timeout_secs, 300);
        assert_eq!(cfg.executor.wipe_drain_timeout_secs_clamped(), 300);
        let (clamped, warn) = clamp_wipe_drain_timeout_secs(300);
        assert_eq!(clamped, 300);
        assert!(warn.is_none(), "no warn at ceiling");
    }

    #[test]
    fn wipe_drain_timeout_above_ceiling_is_clamped_with_warn() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  wipe_drain_timeout_secs: 600
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        // load_from clamps the stored value in-place.
        assert_eq!(cfg.executor.wipe_drain_timeout_secs, WIPE_DRAIN_TIMEOUT_CEILING_SECS);
        assert_eq!(cfg.executor.wipe_drain_timeout_secs_clamped(), WIPE_DRAIN_TIMEOUT_CEILING_SECS);
        // And the warn-message inspection.
        let (clamped, warn) = clamp_wipe_drain_timeout_secs(600);
        assert_eq!(clamped, WIPE_DRAIN_TIMEOUT_CEILING_SECS);
        let msg = warn.expect("warn must be emitted when above ceiling");
        assert!(msg.contains("600"), "warn names requested value: {msg}");
        assert!(
            msg.contains(&WIPE_DRAIN_TIMEOUT_CEILING_SECS.to_string()),
            "warn names clamped value: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // executor.output_format and executor.log_retention_days
    // -----------------------------------------------------------------

    #[test]
    fn output_format_defaults_to_json() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.output_format, ExecutorOutputFormat::Json);
    }

    #[test]
    fn output_format_text_opt_out_round_trips() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  output_format: text
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.output_format, ExecutorOutputFormat::Text);
    }

    #[test]
    fn log_retention_days_defaults_to_30() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.log_retention_days, 30);
    }

    #[test]
    fn log_retention_days_above_ceiling_is_clamped_with_warn() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  log_retention_days: 1000
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.log_retention_days, LOG_RETENTION_DAYS_CEILING);
        let (clamped, warn) = clamp_log_retention_days(1000);
        assert_eq!(clamped, LOG_RETENTION_DAYS_CEILING);
        let msg = warn.expect("warn must be emitted when above ceiling");
        assert!(msg.contains("1000"));
        assert!(msg.contains(&LOG_RETENTION_DAYS_CEILING.to_string()));
    }

    #[test]
    fn log_retention_days_at_ceiling_no_warn() {
        let (clamped, warn) = clamp_log_retention_days(LOG_RETENTION_DAYS_CEILING);
        assert_eq!(clamped, LOG_RETENTION_DAYS_CEILING);
        assert!(warn.is_none(), "ceiling value is not clamped");
    }

    // -----------------------------------------------------------------
    // executor.busy_marker_stale_threshold_secs
    // -----------------------------------------------------------------

    #[test]
    fn busy_marker_stale_threshold_defaults_when_unset() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.executor.busy_marker_stale_threshold_secs.is_none());
        assert_eq!(cfg.executor.busy_marker_stale_threshold_secs(), 600);
    }

    #[test]
    fn busy_marker_stale_threshold_explicit_within_bounds_passes_through() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  busy_marker_stale_threshold_secs: 1800
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(
            cfg.executor.busy_marker_stale_threshold_secs,
            Some(1800)
        );
        assert_eq!(cfg.executor.busy_marker_stale_threshold_secs(), 1800);
    }

    #[test]
    fn busy_marker_stale_threshold_zero_is_permitted() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  busy_marker_stale_threshold_secs: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.busy_marker_stale_threshold_secs, Some(0));
        assert_eq!(cfg.executor.busy_marker_stale_threshold_secs(), 0);
    }

    #[test]
    fn busy_marker_stale_threshold_above_ceiling_is_clamped_with_warn() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  busy_marker_stale_threshold_secs: 10000
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(
            cfg.executor.busy_marker_stale_threshold_secs,
            Some(BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS)
        );
        assert_eq!(
            cfg.executor.busy_marker_stale_threshold_secs(),
            BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS
        );
        let (clamped, warn) = clamp_busy_marker_stale_threshold_secs(10000);
        assert_eq!(clamped, BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS);
        let msg = warn.expect("warn must be emitted when above ceiling");
        assert!(msg.contains("10000"), "warn names requested value: {msg}");
        assert!(
            msg.contains(&BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS.to_string()),
            "warn names clamped value: {msg}"
        );
    }

    #[test]
    fn busy_marker_stale_threshold_at_ceiling_no_warn() {
        let (clamped, warn) =
            clamp_busy_marker_stale_threshold_secs(BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS);
        assert_eq!(clamped, BUSY_MARKER_STALE_THRESHOLD_CEILING_SECS);
        assert!(warn.is_none(), "ceiling value is not clamped");
    }

    /// Operator bumped `timeout_secs` to 5400 (90 min) for one long
    /// change AND did NOT set the new field → pre-spec implicit was
    /// 6000s; new resolved is 600s. The Migration variant fires with
    /// both values so the operator sees the gap in the log.
    #[test]
    fn startup_log_migration_when_field_unset_and_implicit_was_longer() {
        let log = busy_marker_threshold_startup_log(None, 600, 5400);
        assert_eq!(
            log,
            BusyMarkerThresholdStartupLog::Migration {
                new_threshold_secs: 600,
                pre_spec_implicit_threshold_secs: 6000,
                timeout_secs: 5400,
            }
        );
    }

    /// Operator set the field explicitly → the regular line fires,
    /// even if the explicit value happens to equal the default. The
    /// "explicit" signal is what disables the migration branch.
    #[test]
    fn startup_log_regular_when_field_set_explicitly() {
        let log = busy_marker_threshold_startup_log(Some(600), 600, 5400);
        assert_eq!(
            log,
            BusyMarkerThresholdStartupLog::Regular {
                timeout_secs: 5400,
                busy_marker_stale_threshold_secs: 600,
            }
        );
    }

    /// Operator did NOT set the field AND the pre-spec implicit
    /// threshold (`timeout_secs + 600`) is NOT longer than the new
    /// default (i.e. `timeout_secs == 0`, or some pathological config
    /// where the operator left `timeout_secs` smaller than the
    /// 10-minute buffer would imply). The regular line fires — no
    /// "migration gap" exists to surface.
    #[test]
    fn startup_log_regular_when_implicit_not_longer() {
        let log = busy_marker_threshold_startup_log(None, 600, 0);
        assert_eq!(
            log,
            BusyMarkerThresholdStartupLog::Regular {
                timeout_secs: 0,
                busy_marker_stale_threshold_secs: 600,
            }
        );
    }

    /// Operator set the field higher than the default → the regular
    /// line still fires (their explicit value is what they want
    /// surfaced).
    #[test]
    fn startup_log_regular_when_field_set_to_high_value() {
        let log = busy_marker_threshold_startup_log(Some(7200), 7200, 1800);
        assert_eq!(
            log,
            BusyMarkerThresholdStartupLog::Regular {
                timeout_secs: 1800,
                busy_marker_stale_threshold_secs: 7200,
            }
        );
    }

    // -----------------------------------------------------------------
    // Periodic-audit framework tests (Section 1 of
    // a01-periodic-audits-foundation).
    // -----------------------------------------------------------------

    fn make_repo(url: &str, audits: Option<HashMap<String, Cadence>>) -> RepositoryConfig {
        RepositoryConfig { forge: None,
            url: url.into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
            sandbox: None,
        }
    }

    #[test]
    fn cadence_parses_each_string_form() {
        assert_eq!(Cadence::parse("disabled").unwrap(), Cadence::Disabled);
        assert_eq!(Cadence::parse("daily").unwrap(), Cadence::Daily);
        assert_eq!(Cadence::parse("weekly").unwrap(), Cadence::Weekly);
        assert_eq!(Cadence::parse("monthly").unwrap(), Cadence::Monthly);
        assert_eq!(Cadence::parse("quarterly").unwrap(), Cadence::Quarterly);
        assert_eq!(
            Cadence::parse("every-3-days").unwrap(),
            Cadence::EveryNDays(3)
        );
        assert_eq!(
            Cadence::parse("every-1-days").unwrap(),
            Cadence::EveryNDays(1)
        );
        // Also via serde
        let parsed: Cadence = serde_yml::from_str("\"every-7-days\"").unwrap();
        assert_eq!(parsed, Cadence::EveryNDays(7));
    }

    #[test]
    fn cadence_every_n_days_rejects_zero() {
        let err = Cadence::parse("every-0-days").expect_err("zero must be rejected");
        assert!(err.contains("0"), "error must mention zero: {err}");
        // And via serde:
        let res: std::result::Result<Cadence, _> = serde_yml::from_str("\"every-0-days\"");
        assert!(res.is_err(), "serde must reject every-0-days");
    }

    #[test]
    fn cadence_every_n_days_rejects_negative() {
        let err = Cadence::parse("every--3-days").expect_err("negative must be rejected");
        assert!(
            err.to_lowercase().contains("negative") || err.contains("positive"),
            "error must indicate negativity; got: {err}"
        );
    }

    #[test]
    fn cadence_rejects_unknown_form() {
        assert!(Cadence::parse("yearly").is_err());
        assert!(Cadence::parse("every-day").is_err());
        assert!(Cadence::parse("every-3-day").is_err()); // missing trailing s
    }

    #[test]
    fn max_validation_retries_defaults_to_one_when_field_absent() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let audits = cfg.audits.expect("audits block present");
        assert_eq!(audits.max_validation_retries, 1);
    }

    #[test]
    fn max_validation_retries_zero_is_permitted() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
  max_validation_retries: 0
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let audits = cfg.audits.expect("audits block present");
        assert_eq!(audits.max_validation_retries, 0);
        let (clamped, warn) = clamp_max_validation_retries(0);
        assert_eq!(clamped, 0);
        assert!(warn.is_none(), "no warn for 0");
    }

    #[test]
    fn max_validation_retries_five_is_permitted_no_warn() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
  max_validation_retries: 5
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let audits = cfg.audits.expect("audits block present");
        assert_eq!(audits.max_validation_retries, 5);
        let (clamped, warn) = clamp_max_validation_retries(5);
        assert_eq!(clamped, 5);
        assert!(warn.is_none(), "no warn at ceiling");
    }

    #[test]
    fn max_validation_retries_above_ceiling_is_clamped_with_warn() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
  max_validation_retries: 10
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let audits = cfg.audits.expect("audits block present");
        assert_eq!(audits.max_validation_retries, MAX_VALIDATION_RETRIES_CEILING);
        let (clamped, warn) = clamp_max_validation_retries(10);
        assert_eq!(clamped, MAX_VALIDATION_RETRIES_CEILING);
        let msg = warn.expect("warn must be emitted when above ceiling");
        assert!(msg.contains("10"), "warn names requested value: {msg}");
        assert!(
            msg.contains(&MAX_VALIDATION_RETRIES_CEILING.to_string()),
            "warn names clamped value: {msg}"
        );
    }

    #[test]
    fn max_audits_per_iteration_defaults_to_one_when_field_absent() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let audits = cfg.audits.expect("audits block present");
        assert_eq!(audits.max_audits_per_iteration, 1);
    }

    #[test]
    fn max_audits_per_iteration_explicit_value_passes_through() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
  max_audits_per_iteration: 3
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let audits = cfg.audits.expect("audits block present");
        assert_eq!(audits.max_audits_per_iteration, 3);
        // No clamp needed when within registry bound.
        let (clamped, warn) = clamp_max_audits_per_iteration(3, 5);
        assert_eq!(clamped, 3);
        assert!(warn.is_none());
    }

    #[test]
    fn max_audits_per_iteration_zero_is_permitted() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
  max_audits_per_iteration: 0
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config parses");
        let audits = cfg.audits.expect("audits block present");
        assert_eq!(audits.max_audits_per_iteration, 0);
        let (clamped, warn) = clamp_max_audits_per_iteration(0, 5);
        assert_eq!(clamped, 0);
        assert!(warn.is_none(), "no warn for 0");
    }

    #[test]
    fn max_audits_per_iteration_above_registry_count_clamps_with_warn() {
        // 50 requested, registry has 5 audits → clamps to 5 + WARN.
        let (clamped, warn) = clamp_max_audits_per_iteration(50, 5);
        assert_eq!(clamped, 5);
        let msg = warn.expect("warn must be emitted when above registry count");
        assert!(msg.contains("50"), "warn names requested value: {msg}");
        assert!(msg.contains('5'), "warn names clamped value: {msg}");
    }

    #[test]
    fn max_audits_per_iteration_at_registry_count_no_warn() {
        let (clamped, warn) = clamp_max_audits_per_iteration(5, 5);
        assert_eq!(clamped, 5);
        assert!(warn.is_none(), "no warn at registry count");
    }

    #[test]
    fn audits_block_parses() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
  settings:
    architecture_brightline:
      notify_on_clean: true
      extra:
        file_lines_threshold: 500
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with audits block should parse");
        let audits = cfg.audits.expect("audits block present");
        assert_eq!(
            audits.defaults.get("architecture_brightline").copied(),
            Some(Cadence::Weekly)
        );
        let settings = audits
            .settings
            .get("architecture_brightline")
            .expect("settings present");
        assert!(settings.notify_on_clean);
        assert!(
            settings.extra.get("file_lines_threshold").is_some(),
            "extra threshold should be parsed"
        );
    }

    #[test]
    fn audits_unknown_type_fails_at_load() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    nonexistent_audit_xyz: weekly
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("YAML must parse — validation is separate");
        let err = validate_audit_type_names(&cfg, &["architecture_brightline"])
            .expect_err("unknown audit name must be rejected by validate_audit_type_names");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nonexistent_audit_xyz"),
            "error must name the offending audit type; got: {msg}"
        );
        assert!(
            msg.contains("architecture_brightline"),
            "error must list known types; got: {msg}"
        );
    }

    #[test]
    fn audits_unknown_per_repo_type_fails_at_load() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    audits:
      typo_audit: daily
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("YAML must parse");
        let err = validate_audit_type_names(&cfg, &["architecture_brightline"])
            .expect_err("unknown per-repo audit name must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("typo_audit"),
            "error must name the offending audit type; got: {msg}"
        );
        assert!(
            msg.contains("repositories[0]"),
            "error must name the field path; got: {msg}"
        );
    }

    #[test]
    fn per_repo_audit_overrides_global_default() {
        let mut defaults = HashMap::new();
        defaults.insert("architecture_brightline".to_string(), Cadence::Weekly);
        let audits_cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
            ..AuditsConfig::default()
        };
        let mut overrides = HashMap::new();
        overrides.insert(
            "architecture_brightline".to_string(),
            Cadence::EveryNDays(3),
        );
        let repo = make_repo("git@github.com:o/r.git", Some(overrides));
        let effective = resolved_cadence(&repo, Some(&audits_cfg), "architecture_brightline");
        assert_eq!(effective, Cadence::EveryNDays(3));
    }

    #[test]
    fn audit_absent_from_both_resolves_to_disabled() {
        let repo = make_repo("git@github.com:o/r.git", None);
        let effective = resolved_cadence(&repo, None, "architecture_brightline");
        assert_eq!(effective, Cadence::Disabled);

        let audits_cfg = AuditsConfig::default();
        let effective = resolved_cadence(&repo, Some(&audits_cfg), "architecture_brightline");
        assert_eq!(effective, Cadence::Disabled);

        let mut defaults = HashMap::new();
        defaults.insert("other_audit".to_string(), Cadence::Daily);
        let audits_cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
            ..AuditsConfig::default()
        };
        let effective = resolved_cadence(&repo, Some(&audits_cfg), "architecture_brightline");
        assert_eq!(
            effective,
            Cadence::Disabled,
            "an audit not listed anywhere must resolve to Disabled"
        );
    }

    #[test]
    fn global_default_applies_when_no_per_repo_override() {
        let mut defaults = HashMap::new();
        defaults.insert("architecture_brightline".to_string(), Cadence::Monthly);
        let audits_cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
            ..AuditsConfig::default()
        };
        let repo = make_repo("git@github.com:o/r.git", None);
        let effective = resolved_cadence(&repo, Some(&audits_cfg), "architecture_brightline");
        assert_eq!(effective, Cadence::Monthly);
    }

    #[test]
    fn validate_audit_type_names_passes_when_all_known() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    audits:
      architecture_brightline: daily
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    architecture_brightline: weekly
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        validate_audit_type_names(&cfg, &["architecture_brightline"])
            .expect("registered audit must pass validation");
    }

    /// a75 (task 6.7): `validate_audit_type_names` accepts the new
    /// `canon_contradiction_audit` slug AND still rejects an unknown one,
    /// listing the registered slugs in the error.
    #[test]
    fn validate_audit_type_names_accepts_canon_contradiction_audit() {
        let known = &[
            "architecture_brightline",
            "architecture_consultative",
            "drift_audit",
            "missing_tests_audit",
            "security_bug_audit",
            "canon_contradiction_audit",
        ];

        // Accepts the new slug.
        let ok_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    canon_contradiction_audit: monthly
"#;
        let (_d1, p1) = write_config(ok_yaml);
        let cfg_ok = Config::load_from(&p1).unwrap();
        validate_audit_type_names(&cfg_ok, known)
            .expect("canon_contradiction_audit must be accepted");

        // Rejects an unknown slug, naming it AND listing the registered set.
        let bad_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    not_a_real_audit: monthly
"#;
        let (_d2, p2) = write_config(bad_yaml);
        let cfg_bad = Config::load_from(&p2).unwrap();
        let err = validate_audit_type_names(&cfg_bad, known)
            .expect_err("unknown slug must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("not_a_real_audit"), "names the unknown slug: {msg}");
        assert!(
            msg.contains("canon_contradiction_audit"),
            "lists the registered slugs including the new one: {msg}"
        );
    }

    /// a76 (task 5.7): `validate_audit_type_names` accepts the new
    /// `canon_consolidation_audit` slug AND still rejects an unknown one,
    /// listing the seven registered slugs in the error.
    #[test]
    fn validate_audit_type_names_accepts_canon_consolidation_audit() {
        // The seven slugs the canonical "Registered periodic audits"
        // enumeration carries after a76 (a75's six + canon_consolidation).
        let known = &[
            "architecture_brightline",
            "architecture_consultative",
            "drift_audit",
            "missing_tests_audit",
            "security_bug_audit",
            "canon_contradiction_audit",
            "canon_consolidation_audit",
        ];

        // Accepts the new slug.
        let ok_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    canon_consolidation_audit: monthly
"#;
        let (_d1, p1) = write_config(ok_yaml);
        let cfg_ok = Config::load_from(&p1).unwrap();
        validate_audit_type_names(&cfg_ok, known)
            .expect("canon_consolidation_audit must be accepted");

        // Rejects an unknown slug, naming it AND listing the registered set.
        let bad_yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
audits:
  defaults:
    not_a_real_audit: monthly
"#;
        let (_d2, p2) = write_config(bad_yaml);
        let cfg_bad = Config::load_from(&p2).unwrap();
        let err = validate_audit_type_names(&cfg_bad, known)
            .expect_err("unknown slug must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("not_a_real_audit"), "names the unknown slug: {msg}");
        assert!(
            msg.contains("canon_consolidation_audit"),
            "lists the registered slugs including the new one: {msg}"
        );
    }

    #[test]
    fn cadence_interval_matches_documented_durations() {
        assert!(Cadence::Disabled.interval().is_none());
        assert_eq!(Cadence::Daily.interval(), Some(chrono::Duration::days(1)));
        assert_eq!(Cadence::Weekly.interval(), Some(chrono::Duration::days(7)));
        assert_eq!(
            Cadence::EveryNDays(3).interval(),
            Some(chrono::Duration::days(3))
        );
        assert_eq!(Cadence::Monthly.interval(), Some(chrono::Duration::days(30)));
        assert_eq!(
            Cadence::Quarterly.interval(),
            Some(chrono::Duration::days(90))
        );
    }

    // -----------------------------------------------------------------
    // chatops.slack.dedup_cache_capacity / dedup_cache_ttl_secs
    // -----------------------------------------------------------------

    #[test]
    fn dedup_cache_defaults_when_omitted() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0FOO
  slack:
    bot_token_env: SLACK_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("default dedup config should parse");
        let slack = cfg.chatops.unwrap().slack.unwrap();
        assert_eq!(slack.dedup_cache_capacity, default_dedup_cache_capacity());
        assert_eq!(slack.dedup_cache_ttl_secs, default_dedup_cache_ttl_secs());
    }

    #[test]
    fn dedup_cache_explicit_values_within_bounds_pass_through() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0FOO
  slack:
    bot_token_env: SLACK_BOT_TOKEN
    dedup_cache_capacity: 500
    dedup_cache_ttl_secs: 120
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let slack = cfg.chatops.unwrap().slack.unwrap();
        assert_eq!(slack.dedup_cache_capacity, 500);
        assert_eq!(slack.dedup_cache_ttl_secs, 120);
    }

    #[test]
    fn dedup_cache_capacity_above_ceiling_is_clamped_with_warn() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0FOO
  slack:
    bot_token_env: SLACK_BOT_TOKEN
    dedup_cache_capacity: 50000
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let slack = cfg.chatops.unwrap().slack.unwrap();
        assert_eq!(slack.dedup_cache_capacity, DEDUP_CACHE_CAPACITY_CEILING);

        let (clamped, warn) = clamp_dedup_cache_capacity(50_000);
        assert_eq!(clamped, DEDUP_CACHE_CAPACITY_CEILING);
        let msg = warn.expect("warn must fire when above ceiling");
        assert!(msg.contains("50000"), "warn names requested value: {msg}");
        assert!(
            msg.contains(&DEDUP_CACHE_CAPACITY_CEILING.to_string()),
            "warn names clamped value: {msg}"
        );
    }

    #[test]
    fn dedup_cache_ttl_above_ceiling_is_clamped_with_warn() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0FOO
  slack:
    bot_token_env: SLACK_BOT_TOKEN
    dedup_cache_ttl_secs: 7200
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let slack = cfg.chatops.unwrap().slack.unwrap();
        assert_eq!(slack.dedup_cache_ttl_secs, DEDUP_CACHE_TTL_SECS_CEILING);

        let (clamped, warn) = clamp_dedup_cache_ttl_secs(7200);
        assert_eq!(clamped, DEDUP_CACHE_TTL_SECS_CEILING);
        let msg = warn.expect("warn must fire when above ceiling");
        assert!(msg.contains("7200"), "warn names requested value: {msg}");
        assert!(
            msg.contains(&DEDUP_CACHE_TTL_SECS_CEILING.to_string()),
            "warn names clamped value: {msg}"
        );
    }

    #[test]
    fn dedup_cache_ttl_zero_is_clamped_to_one_with_warn() {
        let (clamped, warn) = clamp_dedup_cache_ttl_secs(0);
        assert_eq!(clamped, 1, "0 must be clamped to 1");
        let msg = warn.expect("warn must fire for ttl=0");
        assert!(msg.contains('0'), "warn references the original 0 value: {msg}");
    }

    #[test]
    fn dedup_cache_capacity_zero_parses_without_warn_and_disables_dedup() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0FOO
  slack:
    bot_token_env: SLACK_BOT_TOKEN
    dedup_cache_capacity: 0
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("capacity 0 should parse");
        let slack = cfg.chatops.unwrap().slack.unwrap();
        assert_eq!(slack.dedup_cache_capacity, 0);

        // No WARN for capacity 0.
        let (clamped, warn) = clamp_dedup_cache_capacity(0);
        assert_eq!(clamped, 0);
        assert!(warn.is_none(), "no warn for capacity 0 (opt-out)");

        // Behavioural check: capacity 0 disables dedup at the cache layer.
        let cache = crate::chatops::event_dedup::EventDedupCache::new(
            slack.dedup_cache_capacity,
            std::time::Duration::from_secs(slack.dedup_cache_ttl_secs),
        );
        let key = crate::chatops::event_dedup::DedupKey {
            channel: "C".into(),
            ts: "1.0".into(),
            user: "U".into(),
        };
        for _ in 0..3 {
            assert!(matches!(
                cache.check_and_insert(key.clone()),
                crate::chatops::event_dedup::CheckResult::Fresh
            ));
        }
    }

    #[test]
    fn rejects_unknown_executor_kind() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: gpt_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("unknown executor kind should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("gpt_cli") || msg.to_lowercase().contains("variant"),
            "error should reject unknown variant; got: {msg}"
        );
    }

    // ----------------------------------------------------------------
    // validate_config — shared validation surface
    // ----------------------------------------------------------------

    /// Env-var mutation is process-global; tests that touch
    /// SecretSource env vars take this mutex.
    static VALIDATE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn valid_single_repo_yaml() -> &'static str {
        r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  command: claude
github:
  token: { value: "inline-pat" }
"#
    }

    #[test]
    fn validate_config_valid_returns_empty_report() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let (_dir, path) = write_config(valid_single_repo_yaml());
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        assert!(
            report.errors.is_empty(),
            "valid config should have zero errors; got: {:?}",
            report.errors
        );
        assert!(
            report.warnings.is_empty(),
            "valid config (inline token) should have zero warnings; got: {:?}",
            report.warnings
        );
        assert!(report.is_ok());
    }

    #[test]
    fn validate_config_schema_violation_emits_error_with_pointer() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 0
executor:
  kind: claude_cli
github:
  token: { value: "x" }
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        let schema_errs: Vec<&Finding> = report
            .errors
            .iter()
            .filter(|f| f.category == FindingCategory::Schema)
            .collect();
        assert!(
            !schema_errs.is_empty(),
            "expected at least one schema error; got: {:?}",
            report.errors
        );
        let f = schema_errs
            .iter()
            .find(|f| f.message.contains("poll_interval_sec"))
            .expect("must include the offending field name");
        assert_eq!(
            f.config_pointer.as_deref(),
            Some("repositories/0/poll_interval_sec")
        );
    }

    #[test]
    fn validate_config_empty_repositories_is_schema_error() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let yaml = r#"
repositories: []
executor:
  kind: claude_cli
github:
  token: { value: "x" }
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        assert!(
            report
                .errors
                .iter()
                .any(|f| f.category == FindingCategory::Schema
                    && f.message.contains("repositories list is empty")),
            "expected an empty-repos schema error; got: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_config_token_route_gap_emits_error_naming_owner() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let env_var = "AUTOCODER_TEST_VALIDATE_UNROUTED_FALLBACK";
        unsafe { std::env::remove_var(env_var) };
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:my-org-b/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: {env_var}
"#
        );
        let (_dir, path) = write_config(&yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        let route_errs: Vec<&Finding> = report
            .errors
            .iter()
            .filter(|f| f.category == FindingCategory::TokenRoute)
            .collect();
        assert!(
            !route_errs.is_empty(),
            "expected at least one token-route error; got: {:?}",
            report.errors
        );
        assert!(
            route_errs[0].message.contains("my-org-b"),
            "error must name the missing owner; got: {}",
            route_errs[0].message
        );
        assert_eq!(
            route_errs[0].config_pointer.as_deref(),
            Some("repositories/0/url")
        );
    }

    #[test]
    fn validate_config_gitlab_forge_block_with_inline_token_routes() {
        // a008: a `forge: { kind: gitlab }` block with an inline token parses
        // AND its token route resolves — a non-github.com host is NOT rejected
        // as "unparsable github", AND the global github token route is not
        // consulted for this repo.
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let unset = "AUTOCODER_TEST_VALIDATE_GITLAB_FALLBACK_UNSET";
        unsafe { std::env::remove_var(unset) };
        let yaml = format!(
            r#"
repositories:
  - url: "https://gitlab.example.com/group/subgroup/project.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    forge:
      kind: gitlab
      host: gitlab.example.com
      token: {{ value: "glpat-xxx" }}
executor:
  kind: claude_cli
github:
  token_env: {unset}
"#
        );
        let (_dir, path) = write_config(&yaml);
        let cfg = Config::load_from(&path).unwrap();
        // The block parsed into the typed config.
        let forge = cfg.repositories[0].forge.as_ref().expect("forge block");
        assert_eq!(forge.kind, ForgeKind::Gitlab);
        assert_eq!(forge.host.as_deref(), Some("gitlab.example.com"));
        assert!(forge.token_route_resolves());
        // No token-route error despite the (unset) global github fallback.
        let report = validate_config(&cfg);
        let route_errs: Vec<&Finding> = report
            .errors
            .iter()
            .filter(|f| f.category == FindingCategory::TokenRoute)
            .collect();
        assert!(
            route_errs.is_empty(),
            "gitlab forge block with inline token must route cleanly; got: {route_errs:?}"
        );
    }

    #[test]
    fn validate_config_gitlab_forge_block_without_token_route_errors() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let unset = "AUTOCODER_TEST_VALIDATE_GITLAB_TOKEN_ENV_UNSET";
        unsafe { std::env::remove_var(unset) };
        let yaml = format!(
            r#"
repositories:
  - url: "https://gitlab.example.com/group/project.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    forge:
      kind: gitlab
      host: gitlab.example.com
      token_env: {unset}
executor:
  kind: claude_cli
github:
  token: {{ value: "ignored-for-this-repo" }}
"#
        );
        let (_dir, path) = write_config(&yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        let route_errs: Vec<&Finding> = report
            .errors
            .iter()
            .filter(|f| f.category == FindingCategory::TokenRoute)
            .collect();
        assert_eq!(route_errs.len(), 1, "expected one forge token-route error");
        assert_eq!(
            route_errs[0].config_pointer.as_deref(),
            Some("repositories/0/forge")
        );
    }

    #[test]
    fn validate_config_workspace_collision_emits_one_error_per_repo() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    local_path: /tmp/shared-workspace
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
  - url: "git@github.com:other/repo.git"
    local_path: /tmp/shared-workspace
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token: { value: "x" }
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        let coll_errs: Vec<&Finding> = report
            .errors
            .iter()
            .filter(|f| f.category == FindingCategory::WorkspaceCollision)
            .collect();
        assert_eq!(
            coll_errs.len(),
            2,
            "expected one error per colliding repo; got: {:?}",
            coll_errs
        );
    }

    #[test]
    fn validate_config_audit_slug_typo_emits_error_naming_slug() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token: { value: "x" }
audits:
  defaults:
    typo_audit_name: weekly
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        let slug_errs: Vec<&Finding> = report
            .errors
            .iter()
            .filter(|f| f.category == FindingCategory::AuditSlug)
            .collect();
        assert!(
            !slug_errs.is_empty(),
            "expected at least one audit-slug error; got: {:?}",
            report.errors
        );
        assert!(
            slug_errs[0].message.contains("typo_audit_name"),
            "error must name the offending slug; got: {}",
            slug_errs[0].message
        );
    }

    /// a76 regression: `validate_config`'s audit-slug check (which reads
    /// `KNOWN_AUDIT_TYPES`) accepts the registered `canon_consolidation_audit`
    /// slug. The const must stay in lock-step with the `AuditRegistry` built in
    /// `cli/run.rs`; a drift would make `validate-config` reject a valid
    /// operator config that enables the audit.
    #[test]
    fn validate_config_accepts_canon_consolidation_audit_slug() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token: { value: "x" }
audits:
  defaults:
    canon_consolidation_audit: monthly
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        let slug_errs: Vec<&Finding> = report
            .errors
            .iter()
            .filter(|f| f.category == FindingCategory::AuditSlug)
            .collect();
        assert!(
            slug_errs.is_empty(),
            "canon_consolidation_audit is a registered slug and must not raise an audit-slug error; got: {slug_errs:?}"
        );
        // The const is the source of the validator's known set: it must list
        // the slug the registry registers.
        assert!(
            KNOWN_AUDIT_TYPES.contains(&"canon_consolidation_audit"),
            "KNOWN_AUDIT_TYPES must list canon_consolidation_audit to match the registry"
        );
    }

    #[test]
    fn validate_config_path_collision_emits_error() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token: { value: "x" }
paths:
  state_dir: /collide
  cache_dir: /collide
  logs_dir: /distinct-logs
  runtime_dir: /distinct-runtime
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        assert!(
            report
                .errors
                .iter()
                .any(|f| f.category == FindingCategory::PathCollision),
            "expected a path-collision error; got: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_config_missing_env_emits_warn_finding() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let env_var = "AUTOCODER_TEST_VALIDATE_MISSING_TOKEN_ENV";
        unsafe { std::env::remove_var(env_var) };
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: {env_var}
  owner_tokens:
    owner: {{ value: "inline-owner-pat" }}
"#
        );
        let (_dir, path) = write_config(&yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        // The repo has an owner_tokens inline route, so TokenRoute passes;
        // but `github.token_env` references an unset env var → WARN.
        assert!(
            report
                .errors
                .iter()
                .all(|f| f.category != FindingCategory::TokenRoute),
            "inline owner_tokens must satisfy token-route; got: {:?}",
            report.errors
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|f| f.category == FindingCategory::SecretSource
                    && f.message.contains(env_var)),
            "expected a secret-source WARN naming the unset env var; got: {:?}",
            report.warnings
        );
    }

    // -----------------------------------------------------------------
    // features.brownfield (a23)
    // -----------------------------------------------------------------

    #[test]
    fn features_brownfield_block_omitted_uses_defaults() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("absent features block parses");
        assert!(
            cfg.features.brownfield.enabled,
            "default enabled must be true"
        );
        assert!(
            cfg.features.brownfield.prompt_path.is_none(),
            "default prompt_path must be None"
        );
    }

    #[test]
    fn features_brownfield_disable_parses() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
features:
  brownfield:
    enabled: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("explicit disable parses");
        assert!(!cfg.features.brownfield.enabled);
        assert!(cfg.features.brownfield.prompt_path.is_none());
    }

    #[test]
    fn features_brownfield_explicit_prompt_path_parses() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
features:
  brownfield:
    prompt_path: "./prompts/brownfield-custom.md"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("prompt_path override parses");
        assert!(cfg.features.brownfield.enabled);
        assert_eq!(
            cfg.features.brownfield.prompt_path.as_deref(),
            Some(Path::new("./prompts/brownfield-custom.md"))
        );
    }

    #[test]
    fn executor_nested_prompt_overrides_round_trip(
    ) {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  implementer:
    prompt_path: "./prompts/impl-custom.md"
  changelog_stylist:
    prompt_path: "./prompts/stylist-custom.md"
  implementer_revision:
    prompt_path: "./prompts/revision-custom.md"
  audit_triage:
    prompt_path: "./prompts/triage-custom.md"
  chat_request_triage:
    prompt_path: "./prompts/chat-triage-custom.md"
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("nested overrides parse");
        assert_eq!(
            cfg.executor.implementer.as_ref().and_then(|b| b.prompt_path.as_deref()),
            Some(Path::new("./prompts/impl-custom.md"))
        );
        assert_eq!(
            cfg.executor
                .changelog_stylist
                .as_ref()
                .and_then(|b| b.prompt_path.as_deref()),
            Some(Path::new("./prompts/stylist-custom.md"))
        );
        assert_eq!(
            cfg.executor
                .implementer_revision
                .as_ref()
                .and_then(|b| b.prompt_path.as_deref()),
            Some(Path::new("./prompts/revision-custom.md"))
        );
        assert_eq!(
            cfg.executor
                .audit_triage
                .as_ref()
                .and_then(|b| b.prompt_path.as_deref()),
            Some(Path::new("./prompts/triage-custom.md"))
        );
        assert_eq!(
            cfg.executor
                .chat_request_triage
                .as_ref()
                .and_then(|b| b.prompt_path.as_deref()),
            Some(Path::new("./prompts/chat-triage-custom.md"))
        );
    }

    #[test]
    fn executor_legacy_and_nested_can_coexist() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  implementer_prompt_path: "/etc/autocoder/impl-legacy.md"
  implementer:
    prompt_path: "./prompts/impl-nested.md"
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("both forms coexist");
        assert_eq!(
            cfg.executor.implementer_prompt_path.as_deref(),
            Some(Path::new("/etc/autocoder/impl-legacy.md"))
        );
        assert_eq!(
            cfg.executor.implementer.as_ref().and_then(|b| b.prompt_path.as_deref()),
            Some(Path::new("./prompts/impl-nested.md"))
        );
    }

    #[test]
    fn reviewer_nested_code_review_block_round_trips() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-opus-4-7
  api_key_env: ANTHROPIC_API_KEY
  code_review:
    prompt_path: "./prompts/review-custom.md"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("reviewer nested form parses");
        let rv = cfg.reviewer.expect("reviewer parsed");
        assert_eq!(
            rv.code_review.as_ref().and_then(|b| b.prompt_path.as_deref()),
            Some(Path::new("./prompts/review-custom.md"))
        );
    }

    // -----------------------------------------------------------------
    // features.scout (a25)
    // -----------------------------------------------------------------

    #[test]
    fn features_scout_block_omitted_uses_defaults() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("absent features block parses");
        assert!(cfg.features.scout.enabled, "default enabled must be true");
        assert!(cfg.features.scout.prompt_path.is_none());
        assert_eq!(cfg.features.scout.max_items, 30);
        assert!(cfg.features.scout.include_issues);
        assert_eq!(cfg.features.scout.staleness_warn_days, 7);
    }

    #[test]
    fn features_scout_explicit_block_round_trips() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
features:
  scout:
    enabled: false
    prompt_path: "./prompts/scout-custom.md"
    max_items: 15
    include_issues: false
    staleness_warn_days: 14
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("explicit scout block parses");
        assert!(!cfg.features.scout.enabled);
        assert_eq!(
            cfg.features.scout.prompt_path.as_deref(),
            Some(Path::new("./prompts/scout-custom.md"))
        );
        assert_eq!(cfg.features.scout.max_items, 15);
        assert!(!cfg.features.scout.include_issues);
        assert_eq!(cfg.features.scout.staleness_warn_days, 14);
    }

    #[test]
    fn features_scout_max_items_zero_fails_validation() {
        let cfg = ScoutFeatureConfig {
            max_items: 0,
            ..ScoutFeatureConfig::default()
        };
        let err = cfg.validate().expect_err("max_items=0 invalid");
        assert!(err.contains("max_items"), "{err}");
        assert!(err.contains("1..=50"), "{err}");
    }

    #[test]
    fn features_scout_max_items_above_50_fails_validation() {
        let cfg = ScoutFeatureConfig {
            max_items: 51,
            ..ScoutFeatureConfig::default()
        };
        let err = cfg.validate().expect_err("max_items=51 invalid");
        assert!(err.contains("max_items"), "{err}");
        assert!(err.contains("1..=50"), "{err}");
    }

    #[test]
    fn features_scout_max_items_invalid_surfaces_in_validate_config() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  command: claude
  timeout_secs: 60
github:
  token_env: GITHUB_TOKEN
features:
  scout:
    max_items: 100
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("parses; range check is in validate");
        let report = validate_config(&cfg);
        assert!(
            report.errors.iter().any(|f| f.message.contains("max_items")
                && f.message.contains("1..=50")),
            "expected schema error naming max_items range; got: {:?}",
            report.errors
        );
    }

    // -----------------------------------------------------------------
    // features.brownfield_survey (a29)
    // -----------------------------------------------------------------

    #[test]
    fn features_brownfield_survey_block_omitted_uses_defaults() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("absent features block parses");
        assert!(
            cfg.features.brownfield_survey.enabled,
            "default enabled must be true"
        );
        assert!(cfg.features.brownfield_survey.prompt_path.is_none());
        assert_eq!(cfg.features.brownfield_survey.max_capabilities, 20);
    }

    #[test]
    fn features_brownfield_survey_explicit_block_round_trips() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
features:
  brownfield_survey:
    enabled: false
    prompt_path: "./prompts/survey-custom.md"
    max_capabilities: 35
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("explicit fields parse");
        assert!(!cfg.features.brownfield_survey.enabled);
        assert_eq!(
            cfg.features.brownfield_survey.prompt_path.as_deref(),
            Some(Path::new("./prompts/survey-custom.md"))
        );
        assert_eq!(cfg.features.brownfield_survey.max_capabilities, 35);
    }

    #[test]
    fn features_brownfield_survey_max_capabilities_zero_fails_validation() {
        let cfg = BrownfieldSurveyFeatureConfig {
            max_capabilities: 0,
            ..BrownfieldSurveyFeatureConfig::default()
        };
        let err = cfg.validate().expect_err("max_capabilities=0 invalid");
        assert!(err.contains("max_capabilities"), "{err}");
        assert!(err.contains("1..=50"), "{err}");
    }

    // -----------------------------------------------------------------
    // features.issues (a009)
    // -----------------------------------------------------------------

    #[test]
    fn features_issues_block_omitted_is_off_by_default() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("absent features block parses");
        // The issues lane is OFF by default — unlike the chatops-verb
        // features, an enabled lane changes per-iteration unit selection.
        assert!(
            !cfg.features.issues.enabled,
            "issues lane must default to OFF"
        );
        assert!(cfg.features.issues.prompt_path.is_none());
        assert_eq!(cfg.features.issues, IssuesFeatureConfig::default());
    }

    #[test]
    fn features_issues_explicit_block_round_trips() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
features:
  issues:
    enabled: true
    prompt_path: "./prompts/issue-custom.md"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("explicit fields parse");
        assert!(cfg.features.issues.enabled);
        assert_eq!(
            cfg.features.issues.prompt_path.as_deref(),
            Some(Path::new("./prompts/issue-custom.md"))
        );
    }

    #[test]
    fn features_brownfield_survey_max_capabilities_above_50_fails_validation() {
        let cfg = BrownfieldSurveyFeatureConfig {
            max_capabilities: 51,
            ..BrownfieldSurveyFeatureConfig::default()
        };
        let err = cfg.validate().expect_err("max_capabilities=51 invalid");
        assert!(err.contains("max_capabilities"), "{err}");
        assert!(err.contains("1..=50"), "{err}");
    }

    #[test]
    fn features_brownfield_survey_invalid_max_capabilities_surfaces_in_validate_config() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  command: claude
  timeout_secs: 60
github:
  token_env: GITHUB_TOKEN
features:
  brownfield_survey:
    max_capabilities: 100
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("parses; range check is in validate");
        let report = validate_config(&cfg);
        assert!(
            report
                .errors
                .iter()
                .any(|f| f.message.contains("max_capabilities")
                    && f.message.contains("1..=50")),
            "expected schema error naming max_capabilities range; got: {:?}",
            report.errors
        );
    }

    #[test]
    fn features_brownfield_non_bool_enabled_fails_load() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
features:
  brownfield:
    enabled: "yes"
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("non-bool enabled must fail config-load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("enabled") || msg.contains("bool"),
            "error must name the offending field / expected type; got: {msg}"
        );
    }

    #[test]
    fn canonical_rag_absent_block_parses_as_none() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.canonical_rag.is_none());
    }

    #[test]
    fn canonical_rag_full_block_parses() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
canonical_rag:
  enabled: true
  provider: ollama
  model: qwen3-embedding:4b
  api_base_url: http://gpu-host:11434
  top_k: 15
  chunk_strategy: per_requirement
  reembed_on_archive: true
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let rag = cfg.canonical_rag.expect("block should parse");
        assert!(rag.enabled);
        assert_eq!(rag.provider, Some(RagProvider::Ollama));
        assert_eq!(rag.model, "qwen3-embedding:4b");
        assert_eq!(rag.api_base_url, "http://gpu-host:11434");
        assert_eq!(rag.top_k, 15);
        assert_eq!(rag.chunk_strategy, ChunkStrategy::PerRequirement);
        assert!(rag.reembed_on_archive);
    }

    #[test]
    fn canonical_rag_missing_required_provider_errors() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
canonical_rag:
  enabled: true
  model: foo
  api_base_url: http://localhost:11434
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("missing provider must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("provider"),
            "error should mention `provider`; got: {msg}"
        );
    }

    #[test]
    fn canonical_rag_top_k_clamps_above_ceiling() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
canonical_rag:
  enabled: true
  provider: ollama
  model: nomic-embed-text
  api_base_url: http://localhost:11434
  top_k: 500
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.canonical_rag.unwrap().top_k, RAG_TOP_K_CEILING);
    }

    #[test]
    fn canonical_rag_inline_api_key_wins_over_env_with_warn() {
        let env_var = "AUTOCODER_TEST_CANONICAL_RAG_ENV";
        unsafe { std::env::remove_var(env_var) };
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
canonical_rag:
  enabled: true
  provider: openai_compatible
  model: voyage-2
  api_base_url: https://api.voyageai.com/v1
  api_key_env: {env_var}
  api_key:
    value: "inline-secret"
"#
        );
        let (_dir, path) = write_config(&yaml);
        let cfg = Config::load_from(&path).unwrap();
        let rag = cfg.canonical_rag.unwrap();
        let resolved = rag.resolve_api_key().unwrap();
        assert_eq!(resolved.as_deref(), Some("inline-secret"));
    }

    #[test]
    fn validate_config_inline_secret_does_not_warn() {
        let _g = VALIDATE_ENV_LOCK.lock().unwrap();
        let env_var = "AUTOCODER_TEST_VALIDATE_INLINE_NO_WARN";
        unsafe { std::env::remove_var(env_var) };
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: {env_var}
  token: {{ value: "inline-pat" }}
"#
        );
        let (_dir, path) = write_config(&yaml);
        let cfg = Config::load_from(&path).unwrap();
        let report = validate_config(&cfg);
        assert!(
            report
                .warnings
                .iter()
                .all(|f| f.category != FindingCategory::SecretSource),
            "inline github.token must suppress the token_env WARN; got: {:?}",
            report.warnings
        );
    }

    // --------------------------------------------------------------------
    // a26 OSS-fork support: per-repo `spec_storage`, `upstream`, AND
    // `auto_submit_pr` config fields.
    // --------------------------------------------------------------------

    fn init_git_repo(dir: &Path) {
        let st = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .status()
            .expect("git init");
        assert!(st.success(), "git init failed in {}", dir.display());
        // user.email/user.name needed for some test environments; not
        // strictly required for rev-parse so leave them off to keep the
        // setup minimal.
    }

    #[test]
    fn auto_submit_pr_defaults_to_true_when_absent() {
        let (_dir, path) = write_config(VALID_TWO_REPO_YAML);
        let cfg = Config::load_from(&path).unwrap();
        for repo in &cfg.repositories {
            assert!(repo.auto_submit_pr, "default auto_submit_pr is true");
        }
    }

    #[test]
    fn auto_submit_pr_round_trips_through_serde() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    auto_submit_pr: false
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(!cfg.repositories[0].auto_submit_pr);
    }

    #[test]
    fn upstream_config_round_trips_with_defaults() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    upstream:
      url: "https://github.com/upstream/repo.git"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let up = cfg.repositories[0].upstream.as_ref().expect("upstream set");
        assert_eq!(up.remote, "upstream");
        assert_eq!(up.branch, "main");
        assert_eq!(up.url, "https://github.com/upstream/repo.git");
    }

    #[test]
    fn upstream_config_explicit_remote_and_branch() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: dev
    agent_branch: agent-q
    poll_interval_sec: 60
    upstream:
      remote: parent
      branch: master
      url: "https://github.com/upstream/repo.git"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let up = cfg.repositories[0].upstream.as_ref().expect("upstream set");
        assert_eq!(up.remote, "parent");
        assert_eq!(up.branch, "master");
    }

    #[test]
    fn upstream_empty_url_fails_validation() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    upstream:
      url: ""
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("empty upstream.url must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("upstream.url"), "got: {msg}");
    }

    #[test]
    fn spec_storage_path_must_exist() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    spec_storage:
      path: "/definitely/does/not/exist"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("missing spec_storage.path must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not exist"),
            "expected missing-path error, got: {msg}"
        );
    }

    #[test]
    fn spec_storage_not_a_git_working_tree_fails() {
        let scratch = TempDir::new().unwrap();
        let plain_dir = scratch.path().join("plain");
        std::fs::create_dir_all(&plain_dir).unwrap();
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    spec_storage:
      path: "{}"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#,
            plain_dir.display()
        );
        let (_dir, cfg_path) = write_config(&yaml);
        let err = Config::load_from(&cfg_path)
            .expect_err("plain-dir spec_storage must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a git working tree"),
            "expected git-working-tree error, got: {msg}"
        );
    }

    #[test]
    fn spec_storage_missing_openspec_subdir_fails() {
        let scratch = TempDir::new().unwrap();
        let specs_repo = scratch.path().join("specs-repo");
        std::fs::create_dir_all(&specs_repo).unwrap();
        init_git_repo(&specs_repo);
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    spec_storage:
      path: "{}"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#,
            specs_repo.display()
        );
        let (_dir, cfg_path) = write_config(&yaml);
        let err = Config::load_from(&cfg_path)
            .expect_err("git tree without openspec/ must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("openspec"),
            "expected openspec-subdir error, got: {msg}"
        );
    }

    #[test]
    fn spec_storage_happy_path_parses() {
        let scratch = TempDir::new().unwrap();
        let specs_repo = scratch.path().join("specs-repo");
        std::fs::create_dir_all(specs_repo.join("openspec")).unwrap();
        init_git_repo(&specs_repo);
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    spec_storage:
      path: "{}"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#,
            specs_repo.display()
        );
        let (_dir, cfg_path) = write_config(&yaml);
        let cfg = Config::load_from(&cfg_path)
            .expect("valid spec_storage must parse");
        let ss = cfg.repositories[0].spec_storage.as_ref().unwrap();
        assert_eq!(ss.path, specs_repo.display().to_string());
    }

    #[test]
    fn resolved_spec_storage_dir_returns_none_when_unset() {
        let (_dir, path) = write_config(VALID_TWO_REPO_YAML);
        let cfg = Config::load_from(&path).unwrap();
        assert!(
            cfg.repositories[0]
                .resolved_spec_storage_dir(Path::new("/tmp/ws"))
                .is_none()
        );
    }

    /// a34: defaults round-trip — when `push_remote` and `base_branch`
    /// are unset in the YAML, the parsed `SpecStorageConfig` retains
    /// them as `None` so the runtime fallback (`origin` / remote-tracked
    /// HEAD) is engaged.
    #[test]
    fn spec_storage_push_remote_and_base_branch_default_to_none() {
        let scratch = TempDir::new().unwrap();
        let specs_repo = scratch.path().join("specs-repo");
        std::fs::create_dir_all(specs_repo.join("openspec")).unwrap();
        init_git_repo(&specs_repo);
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    spec_storage:
      path: "{}"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#,
            specs_repo.display()
        );
        let (_dir, cfg_path) = write_config(&yaml);
        let cfg = Config::load_from(&cfg_path).expect("valid spec_storage parses");
        let ss = cfg.repositories[0].spec_storage.as_ref().unwrap();
        assert!(
            ss.push_remote.is_none(),
            "push_remote must default to None: got {:?}",
            ss.push_remote
        );
        assert!(
            ss.base_branch.is_none(),
            "base_branch must default to None: got {:?}",
            ss.base_branch
        );
    }

    /// a34: `reviewer.skip_spec_only_prs` defaults to `false` when unset.
    #[test]
    fn reviewer_skip_spec_only_prs_defaults_false() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
"#;
        let (_dir, cfg_path) = write_config(yaml);
        let cfg = Config::load_from(&cfg_path).expect("valid reviewer parses");
        let rv = cfg.reviewer.as_ref().unwrap();
        assert!(
            !rv.skip_spec_only_prs,
            "skip_spec_only_prs must default to false: got {}",
            rv.skip_spec_only_prs
        );
    }

    /// a34: `spec_storage.push_remote` set to a name not present in the
    /// spec_storage repo's `git remote` output fails config-load with a
    /// clear message naming the missing remote.
    #[test]
    fn spec_storage_push_remote_must_exist() {
        let scratch = TempDir::new().unwrap();
        let specs_repo = scratch.path().join("specs-repo");
        std::fs::create_dir_all(specs_repo.join("openspec")).unwrap();
        init_git_repo(&specs_repo);
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    spec_storage:
      path: "{}"
      push_remote: "nonexistent-remote"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#,
            specs_repo.display()
        );
        let (_dir, cfg_path) = write_config(&yaml);
        let err = Config::load_from(&cfg_path)
            .expect_err("missing push_remote must fail config-load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nonexistent-remote"),
            "error must name the missing remote, got: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // a37: canonical `LlmProvider` enum + per-provider / per-subsystem
    // validation.
    // -----------------------------------------------------------------

    #[test]
    fn llm_provider_round_trips_through_serde_anthropic() {
        let p: LlmProvider = serde_yml::from_str("anthropic").unwrap();
        assert_eq!(p, LlmProvider::Anthropic);
        let s = serde_yml::to_string(&p).unwrap();
        assert_eq!(s.trim(), "anthropic");
    }

    #[test]
    fn llm_provider_round_trips_through_serde_openai_compatible() {
        let p: LlmProvider = serde_yml::from_str("openai_compatible").unwrap();
        assert_eq!(p, LlmProvider::OpenAiCompatible);
        let s = serde_yml::to_string(&p).unwrap();
        assert_eq!(s.trim(), "openai_compatible");
    }

    #[test]
    fn llm_provider_round_trips_through_serde_ollama() {
        let p: LlmProvider = serde_yml::from_str("ollama").unwrap();
        assert_eq!(p, LlmProvider::Ollama);
        let s = serde_yml::to_string(&p).unwrap();
        assert_eq!(s.trim(), "ollama");
    }

    #[test]
    fn rag_provider_alias_resolves_to_llm_provider() {
        // Type-alias check: pre-spec callers that imported the old
        // names continue to compile.
        let _: RagProvider = LlmProvider::Ollama;
        let _: ReviewerProvider = LlmProvider::Anthropic;
    }

    // ---- validate_llm_provider_config matrix ----

    #[test]
    fn validate_anthropic_requires_api_key() {
        let err = validate_llm_provider_config(
            LlmProvider::Anthropic,
            false,
            None,
            "reviewer",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("reviewer"), "{msg}");
        assert!(msg.contains("anthropic requires api_key"), "{msg}");
    }

    #[test]
    fn validate_anthropic_accepts_api_key_without_base_url() {
        validate_llm_provider_config(
            LlmProvider::Anthropic,
            true,
            None,
            "reviewer",
        )
        .expect("anthropic with api_key + no base_url must pass");
    }

    // ---- validate_llm_provider_config_cli (CLI/agentic: api_key OPTIONAL) ----

    #[test]
    fn cli_validator_allows_missing_key_for_anthropic_and_openai_compatible() {
        // A CLI/agentic role self-authenticates → no api_key required at load.
        validate_llm_provider_config_cli(LlmProvider::Anthropic, false, None, "models.claude_sonnet")
            .expect("anthropic CLI role needs no api_key");
        validate_llm_provider_config_cli(
            LlmProvider::OpenAiCompatible,
            false,
            Some("https://openrouter.ai/api/v1"),
            "models.reviewer_q",
        )
        .expect("openai_compatible CLI role needs no api_key");
    }

    #[test]
    fn cli_validator_still_requires_openai_compatible_base_url() {
        // base_url is not a credential → still required even for a CLI role.
        let err = validate_llm_provider_config_cli(
            LlmProvider::OpenAiCompatible,
            false,
            None,
            "models.reviewer_q",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("requires api_base_url"), "{err:#}");
    }

    #[test]
    fn cli_validator_tolerates_ollama_key_but_http_still_forbids() {
        // For a CLI/agentic ollama role a key is optional AND ignored (no forbid).
        validate_llm_provider_config_cli(
            LlmProvider::Ollama,
            true,
            Some("http://localhost:11434"),
            "models.local_spec_check",
        )
        .expect("ollama CLI role tolerates an (ignored) key");
        // The in-process HTTP validator still forbids it.
        assert!(
            validate_llm_provider_config(
                LlmProvider::Ollama,
                true,
                Some("http://localhost:11434"),
                "canonical_rag",
            )
            .is_err(),
            "HTTP ollama still forbids a key"
        );
    }

    #[test]
    fn validate_openai_compatible_requires_api_key() {
        let err = validate_llm_provider_config(
            LlmProvider::OpenAiCompatible,
            false,
            Some("https://api.openai.com/v1"),
            "reviewer",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("openai_compatible requires api_key"), "{msg}");
    }

    #[test]
    fn validate_openai_compatible_requires_api_base_url() {
        let err = validate_llm_provider_config(
            LlmProvider::OpenAiCompatible,
            true,
            None,
            "change_internal_contradiction_check_llm",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("change_internal_contradiction_check_llm"),
            "{msg}"
        );
        assert!(
            msg.contains("openai_compatible requires api_base_url"),
            "{msg}"
        );
    }

    #[test]
    fn validate_openai_compatible_passes_with_key_and_url() {
        validate_llm_provider_config(
            LlmProvider::OpenAiCompatible,
            true,
            Some("https://api.openai.com/v1"),
            "reviewer",
        )
        .expect("openai_compatible with key+url must pass");
    }

    #[test]
    fn validate_ollama_forbids_api_key() {
        let err = validate_llm_provider_config(
            LlmProvider::Ollama,
            true,
            Some("http://localhost:11434"),
            "reviewer",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("reviewer"), "{msg}");
        assert!(
            msg.contains("ollama does not authenticate"),
            "{msg}"
        );
        assert!(msg.contains("remove api_key field"), "{msg}");
    }

    #[test]
    fn validate_ollama_requires_api_base_url() {
        let err = validate_llm_provider_config(
            LlmProvider::Ollama,
            false,
            None,
            "canonical_rag",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ollama requires api_base_url"), "{msg}");
    }

    #[test]
    fn validate_ollama_passes_with_url_and_no_key() {
        validate_llm_provider_config(
            LlmProvider::Ollama,
            false,
            Some("http://localhost:11434"),
            "reviewer",
        )
        .expect("ollama without key + url must pass");
    }

    // ---- validate_provider_for_subsystem matrix ----

    #[test]
    fn reviewer_subsystem_accepts_all_three_providers() {
        for p in [
            LlmProvider::Anthropic,
            LlmProvider::OpenAiCompatible,
            LlmProvider::Ollama,
        ] {
            validate_provider_for_subsystem(p, SubsystemKind::Reviewer)
                .unwrap_or_else(|e| panic!("{p:?} must be valid for reviewer: {e:#}"));
        }
    }

    #[test]
    fn contradiction_check_subsystem_accepts_all_three_providers() {
        for p in [
            LlmProvider::Anthropic,
            LlmProvider::OpenAiCompatible,
            LlmProvider::Ollama,
        ] {
            validate_provider_for_subsystem(p, SubsystemKind::ContradictionCheck).unwrap();
        }
    }

    #[test]
    fn canonical_rag_subsystem_rejects_anthropic() {
        let err = validate_provider_for_subsystem(
            LlmProvider::Anthropic,
            SubsystemKind::CanonicalRag,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("canonical_rag does not support provider 'anthropic'"),
            "{msg}"
        );
        assert!(msg.contains("available providers"), "{msg}");
        assert!(msg.contains("ollama"), "{msg}");
        assert!(msg.contains("openai_compatible"), "{msg}");
    }

    #[test]
    fn canonical_rag_subsystem_accepts_ollama_and_openai_compatible() {
        validate_provider_for_subsystem(
            LlmProvider::Ollama,
            SubsystemKind::CanonicalRag,
        )
        .unwrap();
        validate_provider_for_subsystem(
            LlmProvider::OpenAiCompatible,
            SubsystemKind::CanonicalRag,
        )
        .unwrap();
    }

    // ---- Config::load_from integration: validators wired in ----

    #[test]
    fn config_load_rejects_canonical_rag_with_anthropic_provider() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
canonical_rag:
  enabled: true
  provider: anthropic
  model: claude-haiku-4-5
  api_base_url: https://api.anthropic.com
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("canonical_rag + anthropic must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("canonical_rag does not support provider 'anthropic'"),
            "{msg}"
        );
    }

    #[test]
    fn config_load_rejects_reviewer_ollama_with_api_key() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
reviewer:
  enabled: true
  kind: oneshot
  provider: ollama
  model: qwen2.5-coder:32b
  api_base_url: http://localhost:11434
  api_key:
    value: "anything"
"#;
        // `kind: oneshot` → in-process HTTP consumer → ollama still forbids a key.
        // (An agentic reviewer would tolerate-and-ignore it; see the agentic test.)
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("oneshot reviewer ollama + api_key must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reviewer: ollama does not authenticate"),
            "{msg}"
        );
    }

    #[test]
    fn config_load_accepts_reviewer_ollama_without_api_key() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
reviewer:
  enabled: true
  provider: ollama
  model: qwen2.5-coder:32b
  api_base_url: http://localhost:11434
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path)
            .expect("reviewer ollama with bare base + no api_key must load");
        let rv = cfg.reviewer.expect("reviewer block present");
        assert_eq!(rv.provider, Some(LlmProvider::Ollama));
        assert_eq!(rv.api_base_url.as_deref(), Some("http://localhost:11434"));
        assert!(rv.api_key.is_none());
        assert!(rv.api_key_env.is_none());
    }

    #[test]
    fn config_load_rejects_reviewer_openai_compatible_without_api_key() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
reviewer:
  enabled: true
  kind: oneshot
  provider: openai_compatible
  model: gpt-4o
  api_base_url: https://api.openai.com/v1
"#;
        // `kind: oneshot` → in-process HTTP consumer → api_key still required.
        // (An agentic reviewer needs no key; see the agentic test.)
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("oneshot openai_compatible without api_key must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("openai_compatible requires api_key"),
            "{msg}"
        );
    }

    #[test]
    fn agentic_reviewer_loads_without_api_key() {
        // The default (agentic) reviewer is CLI-driven → api_key optional; the
        // CLI self-authenticates. (The oneshot reviewer above still requires it.)
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
reviewer:
  enabled: true
  provider: openai_compatible
  model: gpt-4o
  api_base_url: https://api.openai.com/v1
"#;
        let (_dir, path) = write_config(yaml);
        Config::load_from(&path).expect(
            "an agentic reviewer (default kind) needs no api_key — the CLI self-authenticates",
        );
    }

    #[test]
    fn config_load_accepts_contradiction_check_ollama() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  change_internal_contradiction_check: enabled
  change_internal_contradiction_check_llm:
    provider: ollama
    model: qwen2.5:7b
    api_base_url: http://localhost:11434
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path)
            .expect("contradiction-check ollama must load");
        let llm = cfg
            .executor
            .change_internal_contradiction_check_llm
            .as_ref()
            .unwrap();
        assert_eq!(llm.provider, Some(LlmProvider::Ollama));
    }

    // ---- a55: top-level model registry + nickname references ----

    /// 4.1: a `reviewer` block omitting `provider` resolves its `model`
    /// nickname to the registry entry's FULL tuple — provider, model,
    /// api_base_url AND api_key_env.
    #[test]
    fn registry_reviewer_nickname_resolves_full_tuple() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  beefy_security:
    provider: openai_compatible
    model: moonshotai/kimi-k2
    api_base_url: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_KEY
reviewer:
  enabled: true
  model: beefy_security
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("nickname-referencing reviewer must load");
        let rv = cfg.reviewer.expect("reviewer block present");
        assert_eq!(rv.provider, Some(LlmProvider::OpenAiCompatible));
        assert_eq!(rv.model, "moonshotai/kimi-k2");
        assert_eq!(rv.api_base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(rv.api_key_env.as_deref(), Some("OPENROUTER_KEY"));
    }

    /// 2.3: a nickname entry carrying an INLINE `api_key` resolves into
    /// the block, reusing the existing `SecretSource` precedence logic.
    #[test]
    fn registry_nickname_resolves_inline_api_key() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  hosted_anthropic:
    provider: anthropic
    model: claude-opus-4-8
    api_key:
      value: sk-ant-from-registry
reviewer:
  enabled: true
  model: hosted_anthropic
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("inline-key nickname must load");
        let rv = cfg.reviewer.expect("reviewer block present");
        assert_eq!(rv.provider, Some(LlmProvider::Anthropic));
        assert_eq!(rv.model, "claude-opus-4-8");
        let key = rv
            .api_key
            .as_ref()
            .expect("inline api_key resolved from registry")
            .resolve("reviewer.api_key")
            .expect("inline value resolves");
        assert_eq!(key, "sk-ant-from-registry");
    }

    /// 4.2: an INLINE block (provider present) is unchanged by the
    /// registry — even when its `model` collides with a nickname, the
    /// inline provider wins AND the model string is NOT resolved.
    #[test]
    fn registry_not_consulted_for_inline_block() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  beefy_security:
    provider: openai_compatible
    model: moonshotai/kimi-k2
    api_base_url: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_KEY
reviewer:
  enabled: true
  provider: anthropic
  model: beefy_security
  api_key_env: ANTHROPIC_API_KEY
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("inline reviewer must load");
        let rv = cfg.reviewer.expect("reviewer block present");
        // Inline provider wins; the model is the literal string, NOT the
        // registry entry's `moonshotai/kimi-k2`.
        assert_eq!(rv.provider, Some(LlmProvider::Anthropic));
        assert_eq!(rv.model, "beefy_security");
        assert_eq!(rv.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(rv.api_base_url.is_none());
    }

    /// 4.3 / 2.1: a block omitting `provider` whose `model` names no
    /// registry entry fails config-load, naming BOTH the missing nickname
    /// AND the referencing block.
    #[test]
    fn registry_missing_nickname_fails_with_diagnostic() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  fast_local:
    provider: ollama
    model: qwen2.5-coder:32b
    api_base_url: http://localhost:11434
reviewer:
  enabled: true
  model: typo_nickname
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("unknown nickname must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("typo_nickname"), "must name the nickname: {msg}");
        assert!(msg.contains("reviewer"), "must name the block: {msg}");
    }

    // ---- audit-model-selection: per-audit `model` registry references ----

    /// An audit configured with a valid `models:` registry nickname resolves
    /// at config-load to the registry entry's full tuple (provider + model +
    /// base URL), surfaced on `AuditSettings::resolved_model`.
    #[test]
    fn audit_model_nickname_resolves_to_registry_entry() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  beefy_security:
    provider: openai_compatible
    model: moonshotai/kimi-k2
    api_base_url: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_KEY
audits:
  defaults:
    drift_audit: daily
  settings:
    drift_audit:
      model: beefy_security
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("audit nickname must resolve");
        let audits = cfg.audits.expect("audits block present");
        let settings = audits
            .settings
            .get("drift_audit")
            .expect("drift_audit settings present");
        // The unresolved nickname is preserved AND the resolved tuple is set.
        assert_eq!(settings.model.as_deref(), Some("beefy_security"));
        let resolved = settings
            .resolved_model
            .as_ref()
            .expect("model nickname resolved at config-load");
        assert_eq!(resolved.provider, LlmProvider::OpenAiCompatible);
        assert_eq!(resolved.model, "moonshotai/kimi-k2");
        assert_eq!(
            resolved.api_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    /// An audit whose `model` names no registry entry fails config-load,
    /// naming BOTH the missing nickname AND the referencing audit setting.
    #[test]
    fn audit_model_unknown_nickname_fails_with_diagnostic() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  fast_local:
    provider: ollama
    model: qwen2.5-coder:32b
    api_base_url: http://localhost:11434
audits:
  settings:
    security_bug_audit:
      model: nonexistent_model
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("unknown audit model nickname must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nonexistent_model"),
            "must name the missing nickname: {msg}"
        );
        assert!(
            msg.contains("audits.settings.security_bug_audit"),
            "must name the referencing audit setting: {msg}"
        );
    }

    /// An audit with settings but no `model` field resolves to `None`,
    /// preserving the default `claude` CLI behavior.
    #[test]
    fn audit_without_model_field_resolves_to_none() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
audits:
  defaults:
    drift_audit: daily
  settings:
    drift_audit:
      notify_on_clean: true
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("audit without a model must load");
        let audits = cfg.audits.expect("audits block present");
        let settings = audits
            .settings
            .get("drift_audit")
            .expect("drift_audit settings present");
        assert!(settings.model.is_none(), "no model nickname configured");
        assert!(
            settings.resolved_model.is_none(),
            "no model field must leave resolved_model None (default claude behavior)"
        );
    }

    /// 4.4 / 2.2: a `canonical_rag` block resolving (via the registry) to
    /// `anthropic` fails the subsystem-validity gate exactly as an inline
    /// `provider: anthropic` would.
    #[test]
    fn registry_rag_resolving_to_anthropic_fails_subsystem_gate() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  embed_anthropic:
    provider: anthropic
    model: claude-embed
    api_key_env: ANTHROPIC_API_KEY
canonical_rag:
  enabled: true
  model: embed_anthropic
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("RAG resolving to anthropic must fail the subsystem gate");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("canonical_rag does not support provider 'anthropic'"),
            "{msg}"
        );
    }

    /// 2.4: a `models:` entry that is itself invalid (ollama with an
    /// `api_key`) fails config-load via the per-provider auth validation,
    /// even when NO block references it.
    #[test]
    fn registry_unreferenced_invalid_entry_fails_load() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  bad_local:
    provider: openai_compatible
    model: gpt-4o
"#;
        // Registry entries are CLI-capable, so api_key is optional — but the
        // structural rules still apply: an openai_compatible entry MUST have an
        // api_base_url, even unreferenced. (A missing key no longer fails here.)
        let (_dir, path) = write_config(yaml);
        let err =
            Config::load_from(&path).expect_err("entry missing required api_base_url must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("models.bad_local") && msg.contains("requires api_base_url"),
            "must name the entry AND the rule: {msg}"
        );
    }

    /// 4.5: provider → default-CLI rule and the per-entry `cli` override.
    #[test]
    fn default_cli_for_provider_rule() {
        assert_eq!(default_cli_for(LlmProvider::Anthropic), CliKind::Claude);
        assert_eq!(default_cli_for(LlmProvider::Ollama), CliKind::Opencode);
        assert_eq!(
            default_cli_for(LlmProvider::OpenAiCompatible),
            CliKind::Opencode
        );
        // a69: the Google/Antigravity provider maps to the `agy` CLI.
        assert_eq!(default_cli_for(LlmProvider::Google), CliKind::Antigravity);
    }

    #[test]
    fn resolve_cli_command_uses_own_binary_for_non_claude() {
        // Claude role keeps the configured (possibly custom) command.
        assert_eq!(
            resolve_cli_command("/home/u/.local/bin/claude", CliKind::Claude),
            "/home/u/.local/bin/claude"
        );
        // Non-claude roles spawn their OWN binary, NOT the (custom) claude
        // command — regression for an ollama/opencode gate that ran the claude
        // binary and got "Not logged in" instead of reaching opencode.
        assert_eq!(
            resolve_cli_command("/home/u/.local/bin/claude", CliKind::Opencode),
            "opencode"
        );
        assert_eq!(resolve_cli_command("claude", CliKind::Opencode), "opencode");
        assert_eq!(
            resolve_cli_command("/home/u/.local/bin/claude", CliKind::Antigravity),
            "agy"
        );
    }

    /// a69 / task 3.1: the Antigravity CLI is configured as `cli: antigravity`
    /// but its binary on `PATH` is `agy`; the two accessors diverge only for
    /// this CLI (claude/opencode coincide).
    #[test]
    fn antigravity_cli_kind_string_and_binary() {
        assert_eq!(CliKind::Antigravity.as_str(), "antigravity");
        assert_eq!(CliKind::Antigravity.default_command(), "agy");
        assert_eq!(CliKind::Claude.default_command(), "claude");
        assert_eq!(CliKind::Opencode.default_command(), "opencode");
        // The registry's `cli: antigravity` parses to the variant, and an
        // explicit override wins over the provider default.
        let entry = ModelEntry {
            provider: LlmProvider::Anthropic,
            model: "x".into(),
            api_base_url: None,
            api_key: None,
            api_key_env: None,
            cli: Some(CliKind::Antigravity),
        };
        assert_eq!(entry.resolved_cli(), CliKind::Antigravity);
        // And a Google-provider entry defaults to Antigravity with no override.
        let google = ModelEntry {
            provider: LlmProvider::Google,
            model: "gemini-3-pro".into(),
            api_base_url: None,
            api_key: None,
            api_key_env: None,
            cli: None,
        };
        assert_eq!(google.resolved_cli(), CliKind::Antigravity);
    }

    /// 4.5: an entry's explicit `cli` override wins over the provider
    /// default; absent it, the provider default applies.
    #[test]
    fn model_entry_resolved_cli_honors_override() {
        let overridden = ModelEntry {
            provider: LlmProvider::OpenAiCompatible,
            model: "x".into(),
            api_base_url: Some("https://example/v1".into()),
            api_key: None,
            api_key_env: Some("KEY".into()),
            cli: Some(CliKind::Claude),
        };
        assert_eq!(overridden.resolved_cli(), CliKind::Claude);

        let defaulted = ModelEntry {
            cli: None,
            ..overridden.clone()
        };
        assert_eq!(defaulted.resolved_cli(), CliKind::Opencode);
    }

    /// 4.5: the `cli:` override parses from YAML and resolves through the
    /// registry entry.
    #[test]
    fn registry_cli_override_parses_and_resolves() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
models:
  hosted_via_claude:
    provider: openai_compatible
    model: some/model
    api_base_url: https://example/v1
    api_key_env: SOME_KEY
    cli: claude
  plain_local:
    provider: ollama
    model: qwen2.5-coder:32b
    api_base_url: http://localhost:11434
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("registry with cli override must load");
        let registry = cfg.models.expect("models registry present");
        assert_eq!(
            registry["hosted_via_claude"].resolved_cli(),
            CliKind::Claude
        );
        assert_eq!(registry["plain_local"].resolved_cli(), CliKind::Opencode);
    }

    #[test]
    fn resolved_spec_storage_dir_absolute_passes_through() {
        let scratch = TempDir::new().unwrap();
        let specs_repo = scratch.path().join("specs-repo");
        std::fs::create_dir_all(specs_repo.join("openspec")).unwrap();
        init_git_repo(&specs_repo);
        let yaml = format!(
            r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    spec_storage:
      path: "{}"
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#,
            specs_repo.display()
        );
        let (_dir, cfg_path) = write_config(&yaml);
        let cfg = Config::load_from(&cfg_path).unwrap();
        let resolved = cfg.repositories[0]
            .resolved_spec_storage_dir(Path::new("/some/workspace"))
            .unwrap();
        assert_eq!(resolved, specs_repo);
    }
}
