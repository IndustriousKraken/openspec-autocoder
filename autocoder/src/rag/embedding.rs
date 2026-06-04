//! Embedding provider trait + builder for the canonical-spec RAG
//! pipeline (a21). Two adapters today:
//! - [`ollama::OllamaEmbedClient`] POSTs to `<base_url>/api/embed`.
//! - [`openai_compatible::OpenAiCompatEmbedClient`] POSTs to
//!   `<base_url>/embeddings` with a Bearer token.
//!
//! Both implement [`EmbedClient`]. The provider trait is `async_trait`
//! so adapters can use plain reqwest without locking the call site
//! into any specific HTTP runtime detail.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;

use crate::config::{CanonicalRagConfig, LlmProvider, RagProvider};

pub mod ollama;
pub mod openai_compatible;

#[async_trait]
pub trait EmbedClient: Send + Sync {
    /// Embed a batch of texts. Implementations SHOULD respect their
    /// provider's batch limit; the canonical batch ceiling is 32 per
    /// the orchestrator spec, but adapters may choose smaller batches
    /// internally so long as `texts.len() == returned.len()` and the
    /// ordering is preserved.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Embed a single text. Default implementation wraps `embed_batch`.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(&[text.to_string()]).await?;
        out.pop()
            .ok_or_else(|| anyhow!("embed_one: provider returned empty vec"))
    }
}

/// Construct an embed client from the canonical-RAG config. Resolves
/// API keys per the documented `inline > env_var` precedence with WARN
/// when both are set (the `resolve_api_key` impl logs).
pub fn build_client(config: &CanonicalRagConfig) -> Result<Arc<dyn EmbedClient>> {
    match config
        .provider
        .expect("canonical_rag.provider resolved at config-load")
    {
        RagProvider::Ollama => {
            let api_key = config.resolve_api_key().ok().flatten();
            Ok(Arc::new(ollama::OllamaEmbedClient::new(
                config.api_base_url.clone(),
                config.model.clone(),
                api_key,
            )))
        }
        RagProvider::OpenAiCompatible => {
            let api_key = config
                .resolve_api_key()?
                .ok_or_else(|| anyhow!(
                    "canonical_rag.provider=openai_compatible requires api_key OR api_key_env"
                ))?;
            Ok(Arc::new(openai_compatible::OpenAiCompatEmbedClient::new(
                config.api_base_url.clone(),
                config.model.clone(),
                api_key,
            )))
        }
        // a37: defensive backstop. Config-load validation rejects
        // `canonical_rag.provider: anthropic` (Anthropic exposes no
        // embeddings API), so this arm is unreachable in normal
        // operation. We keep it instead of `unreachable!()` so a future
        // code change that bypasses the validation surfaces as a clean
        // operator-actionable error rather than a panic.
        LlmProvider::Anthropic => Err(anyhow!(
            "anthropic does not support embeddings; configure canonical_rag.provider as ollama or openai_compatible"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChunkStrategy;

    fn rag_with_provider(provider: LlmProvider) -> CanonicalRagConfig {
        CanonicalRagConfig {
            enabled: true,
            provider: Some(provider),
            model: "any-model".into(),
            api_base_url: "http://localhost:11434".into(),
            api_key_env: None,
            api_key: None,
            top_k: 10,
            chunk_strategy: ChunkStrategy::PerRequirement,
            reembed_on_archive: true,
        }
    }

    #[test]
    fn build_client_rejects_anthropic_with_clear_message() {
        let cfg = rag_with_provider(LlmProvider::Anthropic);
        let err = match build_client(&cfg) {
            Ok(_) => panic!("anthropic must error in RAG dispatch"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("anthropic does not support embeddings"),
            "must name the rejection reason: {msg}"
        );
        assert!(
            msg.contains("ollama") && msg.contains("openai_compatible"),
            "must name the valid alternatives: {msg}"
        );
    }
}
