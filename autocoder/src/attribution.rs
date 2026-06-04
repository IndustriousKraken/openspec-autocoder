//! Redaction-safe model attribution for operator-facing LLM-driven output
//! (a49).
//!
//! Operator-facing output the daemon composes from an LLM-driven surface —
//! the code reviewer, the change-internal contradiction check, and audits
//! configured with a daemon-known model — carries a one-line
//! `*<Role>: <provider>/<model>*` attribution so operators can associate a
//! comment's quality with the model that produced it.
//!
//! The accessor reads ONLY a positive allowlist of non-secret fields: the
//! provider KIND and the model identifier. It can never return, embed, or
//! derive its output from `api_key`, an `api_key_env`-resolved value,
//! `api_base_url`, or any other secret- or endpoint-bearing field. Because
//! the allowlist is positive, a future field added to a surface's config
//! block is excluded from the attribution unless it is explicitly named
//! here.
//!
//! The displayed `<provider>` is the configured provider KIND
//! (`anthropic` / `openai_compatible` / `ollama`), NOT the upstream brand —
//! a model served via an OpenAI-compatible gateway renders as
//! `openai_compatible/<model>`, not the gateway's name.

use crate::config::{ContradictionCheckLlmConfig, LlmProvider, ReviewerConfig};

/// An LLM-driven surface whose `(provider, model)` the daemon knows and may
/// attribute. Implementors expose ONLY the two allowlisted, non-secret
/// fields; the default [`attribution`](Self::attribution) accessor formats
/// them as `<provider>/<model>`.
pub trait AttributionSurface {
    /// The configured provider KIND (not the upstream brand).
    fn attribution_provider(&self) -> LlmProvider;
    /// The configured model identifier, verbatim.
    fn attribution_model(&self) -> &str;
    /// Redaction-safe `<provider>/<model>`. Reads only the provider KIND
    /// and the model identifier; it is structurally impossible for this to
    /// return an `api_key`, a resolved env-var value, or an `api_base_url`
    /// because the trait exposes no accessor for those fields.
    fn attribution(&self) -> String {
        format!(
            "{}/{}",
            self.attribution_provider().as_str(),
            self.attribution_model()
        )
    }
}

impl AttributionSurface for ReviewerConfig {
    fn attribution_provider(&self) -> LlmProvider {
        self.provider
            .expect("reviewer.provider resolved at config-load")
    }
    fn attribution_model(&self) -> &str {
        &self.model
    }
}

impl AttributionSurface for ContradictionCheckLlmConfig {
    fn attribution_provider(&self) -> LlmProvider {
        self.provider
            .expect("change_internal_contradiction_check_llm.provider resolved at config-load")
    }
    fn attribution_model(&self) -> &str {
        &self.model
    }
}

/// Compose a one-line, italicized attribution: `*<role>: <attribution>*`,
/// where `attribution` is the `<provider>/<model>` string from a
/// redaction-safe accessor (see [`AttributionSurface::attribution`]).
pub fn attribution_line(role: &str, attribution: &str) -> String {
    format!("*{role}: {attribution}*")
}

/// Audit-flavored attribution line: `*Auditor (<audit_type>): <attribution>*`.
/// The role names the specific audit type so operators can tell which
/// audit's model produced a finding.
pub fn audit_attribution_line(audit_type: &str, attribution: &str) -> String {
    attribution_line(&format!("Auditor ({audit_type})"), attribution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ContradictionCheckLlmConfig, ReviewerConfig, ReviewerProvider, SecretSource,
    };

    /// Build a reviewer config that carries BOTH secret-bearing fields
    /// (inline `api_key`) AND an endpoint-bearing field (`api_base_url`)
    /// alongside the allowlisted provider + model.
    fn reviewer_with_secrets() -> ReviewerConfig {
        ReviewerConfig {
            enabled: true,
            provider: Some(ReviewerProvider::OpenAiCompatible),
            model: "moonshotai/kimi-latest".to_string(),
            api_key_env: None,
            api_key: Some(SecretSource::Inline {
                value: "sk-super-secret-do-not-leak".to_string(),
            }),
            api_base_url: Some("https://gateway.internal.example.com/v1".to_string()),
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: None,
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
        }
    }

    #[test]
    fn accessor_returns_provider_slash_model() {
        let cfg = reviewer_with_secrets();
        assert_eq!(cfg.attribution(), "openai_compatible/moonshotai/kimi-latest");
    }

    #[test]
    fn accessor_output_contains_no_secret_or_endpoint() {
        let cfg = reviewer_with_secrets();
        let out = cfg.attribution();
        assert!(
            !out.contains("sk-super-secret-do-not-leak"),
            "attribution must not leak the api_key value: {out}"
        );
        assert!(
            !out.contains("gateway.internal.example.com"),
            "attribution must not leak the api_base_url: {out}"
        );
        assert!(
            !out.contains("https://"),
            "attribution must not contain any endpoint URL: {out}"
        );
    }

    #[test]
    fn provider_is_the_kind_not_the_brand() {
        // A kimi model served via an OpenAI-compatible gateway renders the
        // configured KIND, not "moonshot" or the gateway name.
        let cfg = reviewer_with_secrets();
        assert!(cfg.attribution().starts_with("openai_compatible/"));
    }

    #[test]
    fn contradiction_check_surface_attributes() {
        let cfg = ContradictionCheckLlmConfig {
            provider: Some(ReviewerProvider::Anthropic),
            model: "claude-opus-4-8".to_string(),
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            api_key: None,
            api_base_url: None,
        };
        assert_eq!(cfg.attribution(), "anthropic/claude-opus-4-8");
    }

    #[test]
    fn attribution_line_is_role_prefixed_and_italic() {
        assert_eq!(
            attribution_line("Reviewer", "anthropic/claude-opus-4-8"),
            "*Reviewer: anthropic/claude-opus-4-8*"
        );
    }

    #[test]
    fn audit_attribution_line_names_the_type() {
        assert_eq!(
            audit_attribution_line("security_bug_audit", "anthropic/claude-opus-4-8"),
            "*Auditor (security_bug_audit): anthropic/claude-opus-4-8*"
        );
    }
}
