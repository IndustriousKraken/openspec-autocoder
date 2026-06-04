//! LLM client abstraction. The code-reviewer module is the only caller; this
//! file isolates HTTP details from review semantics and supports multiple
//! providers behind one trait so users can pick Claude, GPT, Grok, Ollama,
//! or any OpenAI-compatible endpoint.

use crate::config::{
    ContradictionCheckLlmConfig, LlmProvider, ReviewerConfig,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

const DEFAULT_ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, prompt: &str) -> Result<String>;
}

pub struct AnthropicClient {
    api_base: String,
    api_key: String,
    model: String,
}

impl AnthropicClient {
    pub fn new(api_base: String, api_key: String, model: String) -> Self {
        Self { api_base, api_key, model }
    }
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/v1/messages", self.api_base.trim_end_matches('/'));
        let payload = json!({
            "model": self.model,
            "max_tokens": DEFAULT_MAX_TOKENS,
            "messages": [{
                "role": "user",
                "content": prompt,
            }],
        });
        let resp = reqwest::Client::new()
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("anthropic request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(500).collect();
            return Err(anyhow!("anthropic API error {status}: {snippet}"));
        }
        let parsed: AnthropicResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("anthropic response decode failed: {e}"))?;
        for block in parsed.content {
            if block.block_type == "text"
                && let Some(text) = block.text
            {
                return Ok(text);
            }
        }
        Err(anyhow!("anthropic response contained no text block"))
    }
}

pub struct OpenAiCompatibleClient {
    api_base: String,
    api_key: String,
    model: String,
}

impl OpenAiCompatibleClient {
    pub fn new(api_base: String, api_key: String, model: String) -> Self {
        Self { api_base, api_key, model }
    }
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}

#[async_trait]
impl LlmClient for OpenAiCompatibleClient {
    async fn complete(&self, prompt: &str) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.api_base.trim_end_matches('/')
        );
        let payload = json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": prompt,
            }],
        });
        let resp = reqwest::Client::new()
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("openai-compatible request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(500).collect();
            return Err(anyhow!("openai-compatible API error {status}: {snippet}"));
        }
        let parsed: OpenAiResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("openai-compatible response decode failed: {e}"))?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow!("openai-compatible response contained no choices"))
    }
}

/// Ollama native chat client (a37). POSTs to `<api_base>/api/chat` using
/// Ollama's native chat API (NOT the OpenAI-compat shim at
/// `/v1/chat/completions`). No `Authorization` header — Ollama does not
/// authenticate; the per-provider auth-semantics check at config-load
/// rejects `api_key` for `provider: ollama`, so no key is ever in scope.
pub struct OllamaChatClient {
    api_base: String,
    model: String,
}

impl OllamaChatClient {
    pub fn new(api_base: String, model: String) -> Self {
        Self { api_base, model }
    }
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaChatMessage,
}

#[derive(Deserialize)]
struct OllamaChatMessage {
    content: String,
}

#[async_trait]
impl LlmClient for OllamaChatClient {
    async fn complete(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/api/chat", self.api_base.trim_end_matches('/'));
        let payload = json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": prompt,
            }],
            "stream": false,
        });
        let resp = reqwest::Client::new()
            .post(&url)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("ollama request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(500).collect();
            return Err(anyhow!("ollama API error {status}: {snippet}"));
        }
        let parsed: OllamaChatResponse = resp.json().await.map_err(|e| {
            anyhow!("OllamaChatClient response decode failed: {e}")
        })?;
        Ok(parsed.message.content)
    }
}

/// Construct the right `LlmClient` for the configured provider. Reads the
/// API key from the environment variable named by `cfg.api_key_env` for
/// providers that authenticate. The `Ollama` arm skips the api_key
/// resolution entirely — config-load validation rejects `api_key` when
/// `provider: ollama`, so no key is ever in scope here.
pub fn build_from_config(cfg: &ReviewerConfig) -> Result<Box<dyn LlmClient>> {
    let provider = cfg
        .provider
        .expect("reviewer.provider resolved at config-load");
    let model = cfg.model.clone();
    let base = cfg.api_base_url.clone();

    match provider {
        LlmProvider::Ollama => {
            let base = base.ok_or_else(|| {
                anyhow!("reviewer.api_base_url is required when provider=ollama")
            })?;
            Ok(Box::new(OllamaChatClient::new(base, model)))
        }
        LlmProvider::Anthropic | LlmProvider::OpenAiCompatible => {
            let api_key = resolve_reviewer_api_key(cfg)?;
            Ok(match provider {
                LlmProvider::Anthropic => Box::new(AnthropicClient::new(
                    base.unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE.to_string()),
                    api_key,
                    model,
                )),
                LlmProvider::OpenAiCompatible => {
                    let base = base.ok_or_else(|| {
                        anyhow!(
                            "reviewer.api_base_url is required when provider=openai_compatible"
                        )
                    })?;
                    Box::new(OpenAiCompatibleClient::new(base, api_key, model))
                }
                LlmProvider::Ollama => unreachable!("handled above"),
            })
        }
    }
}

fn resolve_reviewer_api_key(cfg: &ReviewerConfig) -> Result<String> {
    match (cfg.api_key.as_ref(), cfg.api_key_env.as_ref()) {
        (Some(inline), env_name_opt) => {
            let key = inline.resolve("reviewer.api_key")?;
            if inline.is_inline()
                && let Some(env_name) = env_name_opt
                && std::env::var(env_name).is_ok()
            {
                tracing::warn!(
                    "reviewer.api_key (inline) takes precedence; env var `{env_name}` is being ignored for the reviewer key"
                );
            }
            Ok(key)
        }
        (None, Some(env_name)) => crate::config::SecretSource::EnvVar(env_name.clone())
            .resolve(&format!("reviewer.api_key_env={env_name}")),
        (None, None) => Err(anyhow!(
            "reviewer config has neither `api_key` (inline) nor `api_key_env` (env var name) set"
        )),
    }
}

/// Resolve the change-internal contradiction pre-flight's LLM config (a19)
/// into a [`crate::agentic_run::ResolvedModel`] (a56) for the agentic
/// transport (a59). The `claude` CLI strategy reads the resulting tuple to
/// set `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL`;
/// its `provider` selects which CLI strategy runs.
///
/// A non-Anthropic provider still resolves a tuple here (Anthropic is the
/// only registered strategy until a60), but its CLI has no registered
/// strategy yet, so the contradiction-check session fails open at
/// strategy-resolution time — never spawning a process. The api_key is
/// resolved only for the key-bearing providers; Ollama (no auth) gets an
/// empty key it never uses.
pub fn resolve_contradiction_check_model(
    cfg: &ContradictionCheckLlmConfig,
) -> Result<crate::agentic_run::ResolvedModel> {
    let provider = cfg
        .provider
        .expect("change_internal_contradiction_check_llm.provider resolved at config-load");
    let model = cfg.model.clone();
    let api_base_url = match provider {
        LlmProvider::Anthropic => cfg
            .api_base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE.to_string()),
        // Defense-in-depth: config-load validation already requires
        // `api_base_url` for these providers, but resolve it through an
        // explicit error rather than silently defaulting to `""` — so a
        // bypassed OR buggy validator surfaces a clear message here instead
        // of an opaque CLI spawn failure downstream. Mirrors the reviewer's
        // `build_from_config` AND the pre-a59 `build_from_contradiction_check_config`.
        LlmProvider::OpenAiCompatible => cfg.api_base_url.clone().ok_or_else(|| {
            anyhow!(
                "executor.change_internal_contradiction_check_llm.api_base_url is required when provider=openai_compatible"
            )
        })?,
        LlmProvider::Ollama => cfg.api_base_url.clone().ok_or_else(|| {
            anyhow!(
                "executor.change_internal_contradiction_check_llm.api_base_url is required when provider=ollama"
            )
        })?,
    };
    let api_key = match provider {
        LlmProvider::Ollama => String::new(),
        LlmProvider::Anthropic | LlmProvider::OpenAiCompatible => {
            resolve_contradiction_check_api_key(cfg)?
        }
    };
    Ok(crate::agentic_run::ResolvedModel {
        provider,
        model,
        api_base_url,
        api_key,
    })
}

fn resolve_contradiction_check_api_key(
    cfg: &ContradictionCheckLlmConfig,
) -> Result<String> {
    match (cfg.api_key.as_ref(), cfg.api_key_env.as_ref()) {
        (Some(inline), env_name_opt) => {
            let key = inline.resolve("executor.change_internal_contradiction_check_llm.api_key")?;
            if inline.is_inline()
                && let Some(env_name) = env_name_opt
                && std::env::var(env_name).is_ok()
            {
                tracing::warn!(
                    "executor.change_internal_contradiction_check_llm.api_key (inline) takes precedence; env var `{env_name}` is being ignored"
                );
            }
            Ok(key)
        }
        (None, Some(env_name)) => crate::config::SecretSource::EnvVar(env_name.clone())
            .resolve(&format!(
                "executor.change_internal_contradiction_check_llm.api_key_env={env_name}"
            )),
        (None, None) => Err(anyhow!(
            "executor.change_internal_contradiction_check_llm has neither `api_key` (inline) nor `api_key_env` (env var name) set"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_from_config_errors_when_no_key_source_set() {
        use crate::config::{ReviewerConfig, ReviewerProvider};
        let cfg = ReviewerConfig {
            enabled: true,
            provider: Some(ReviewerProvider::Anthropic),
            model: "claude-sonnet-4-6".into(),
            api_key_env: None,
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
        };
        let err = match build_from_config(&cfg) {
            Ok(_) => panic!("no key source must error"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("api_key") && msg.contains("api_key_env"),
            "error must name both fields; got: {msg}"
        );
    }

    #[tokio::test]
    async fn build_from_config_succeeds_with_inline_only() {
        use crate::config::{ReviewerConfig, ReviewerProvider, SecretSource};
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "sk-inline-only")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"content":[{"type":"text","text":"ok"}]}"#)
            .create_async()
            .await;
        let cfg = ReviewerConfig {
            enabled: true,
            provider: Some(ReviewerProvider::Anthropic),
            model: "claude-sonnet-4-6".into(),
            api_key_env: None,
            api_key: Some(SecretSource::Inline {
                value: "sk-inline-only".into(),
            }),
            api_base_url: Some(server.url()),
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
        };
        let client = build_from_config(&cfg)
            .expect("inline api_key with no api_key_env should succeed");
        let _ = client.complete("hi").await.expect("complete succeeds");
        mock.assert_async().await;
    }

    /// `build_from_config` MUST use `reviewer.api_key` (inline) verbatim and
    /// SHOULD NOT touch `reviewer.api_key_env`'s env var even if it happens
    /// to be set. Asserted by checking the bearer/api-key header on the
    /// outgoing request matches the inline value.
    #[tokio::test]
    async fn inline_api_key_takes_precedence_over_env_var() {
        use crate::config::{ReviewerConfig, ReviewerProvider, SecretSource};

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "inline-key-wins")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"content":[{"type":"text","text":"ok"}]}"#)
            .create_async()
            .await;

        // Set the env-var pointed to by api_key_env so we can confirm it's
        // ignored — if precedence were wrong, the request would carry the
        // env value and mockito would 501 the request shape.
        unsafe {
            std::env::set_var("AUTOCODER_TEST_INLINE_PREC_KEY", "env-value-must-not-be-sent")
        };
        let cfg = ReviewerConfig {
            enabled: true,
            provider: Some(ReviewerProvider::Anthropic),
            model: "claude-sonnet-4-6".into(),
            api_key_env: Some("AUTOCODER_TEST_INLINE_PREC_KEY".into()),
            api_key: Some(SecretSource::Inline {
                value: "inline-key-wins".into(),
            }),
            api_base_url: Some(server.url()),
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
        };
        let client = build_from_config(&cfg).expect("inline build should succeed");
        let _ = client.complete("hi").await.expect("complete succeeds");
        mock.assert_async().await;
        unsafe { std::env::remove_var("AUTOCODER_TEST_INLINE_PREC_KEY") };
    }

    #[tokio::test]
    async fn anthropic_serializes_request_and_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "testkey")
            .match_header("anthropic-version", ANTHROPIC_VERSION)
            .match_body(mockito::Matcher::JsonString(
                r#"{"model":"claude-sonnet-4-6","max_tokens":4096,"messages":[{"role":"user","content":"hi"}]}"#
                    .to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"content":[{"type":"text","text":"hello back"}]}"#)
            .create_async()
            .await;

        let client = AnthropicClient::new(
            server.url(),
            "testkey".to_string(),
            "claude-sonnet-4-6".to_string(),
        );
        let out = client.complete("hi").await.unwrap();
        assert_eq!(out, "hello back");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn anthropic_surfaces_non_2xx_with_status_and_snippet() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/v1/messages")
            .with_status(429)
            .with_body(r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#)
            .create_async()
            .await;

        let client = AnthropicClient::new(
            server.url(),
            "testkey".to_string(),
            "claude-sonnet-4-6".to_string(),
        );
        let err = client.complete("hi").await.expect_err("429 must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("429"), "must include status: {msg}");
        assert!(msg.contains("rate_limit_error"), "must include body snippet: {msg}");
    }

    #[tokio::test]
    async fn openai_compatible_serializes_request_and_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .match_header("authorization", "Bearer testkey")
            .match_body(mockito::Matcher::JsonString(
                r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#.to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"choices":[{"message":{"role":"assistant","content":"hello back"}}]}"#,
            )
            .create_async()
            .await;

        let client = OpenAiCompatibleClient::new(
            server.url(),
            "testkey".to_string(),
            "gpt-4o".to_string(),
        );
        let out = client.complete("hi").await.unwrap();
        assert_eq!(out, "hello back");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn openai_compatible_surfaces_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/chat/completions")
            .with_status(401)
            .with_body(r#"{"error":{"message":"invalid api key"}}"#)
            .create_async()
            .await;

        let client = OpenAiCompatibleClient::new(
            server.url(),
            "testkey".to_string(),
            "gpt-4o".to_string(),
        );
        let err = client.complete("hi").await.expect_err("401 must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("invalid api key"), "{msg}");
    }

    #[tokio::test]
    async fn anthropic_errors_when_response_contains_no_text_block() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":[{"type":"image","source":{"type":"base64","data":"x"}}]}"#,
            )
            .create_async()
            .await;

        let client = AnthropicClient::new(
            server.url(),
            "testkey".to_string(),
            "claude-sonnet-4-6".to_string(),
        );
        let err = client
            .complete("hi")
            .await
            .expect_err("no text block must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no text block"),
            "must name missing-text-block condition: {msg}"
        );
        assert!(
            !msg.contains("request failed") && !msg.contains("API error"),
            "must not claim the HTTP call failed: {msg}"
        );
    }

    #[tokio::test]
    async fn anthropic_errors_when_response_body_is_unparseable_json() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not-json")
            .create_async()
            .await;

        let client = AnthropicClient::new(
            server.url(),
            "testkey".to_string(),
            "claude-sonnet-4-6".to_string(),
        );
        let err = client
            .complete("hi")
            .await
            .expect_err("unparseable JSON must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("decode failed"),
            "must name decode failure: {msg}"
        );
    }

    #[tokio::test]
    async fn openai_compatible_errors_when_choices_array_is_empty() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"choices":[]}"#)
            .create_async()
            .await;

        let client = OpenAiCompatibleClient::new(
            server.url(),
            "testkey".to_string(),
            "gpt-4o".to_string(),
        );
        let err = client
            .complete("hi")
            .await
            .expect_err("empty choices must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no choices"),
            "must name empty-choices condition: {msg}"
        );
    }

    #[tokio::test]
    async fn openai_compatible_errors_when_response_body_is_unparseable_json() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not-json")
            .create_async()
            .await;

        let client = OpenAiCompatibleClient::new(
            server.url(),
            "testkey".to_string(),
            "gpt-4o".to_string(),
        );
        let err = client
            .complete("hi")
            .await
            .expect_err("unparseable JSON must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("decode failed"),
            "must name decode failure: {msg}"
        );
    }

    // -------------------------------------------------------------
    // a37: OllamaChatClient — native /api/chat against a mock server
    // -------------------------------------------------------------

    #[tokio::test]
    async fn ollama_chat_serializes_request_and_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/chat")
            .match_body(mockito::Matcher::JsonString(
                r#"{"model":"qwen2.5-coder:32b","messages":[{"role":"user","content":"review this diff: ..."}],"stream":false}"#
                    .to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"message":{"role":"assistant","content":"VERDICT: Pass\n\nLooks good."},"done":true}"#,
            )
            .create_async()
            .await;

        let client = OllamaChatClient::new(server.url(), "qwen2.5-coder:32b".to_string());
        let out = client.complete("review this diff: ...").await.unwrap();
        assert_eq!(out, "VERDICT: Pass\n\nLooks good.");
        mock.assert_async().await;
    }

    /// Ollama does not authenticate — the client MUST NOT send an
    /// `Authorization` header. We assert this negatively by configuring
    /// the mock to ONLY match when `Authorization` is absent; if the
    /// client adds the header, mockito returns 501 and `complete` errors.
    #[tokio::test]
    async fn ollama_chat_sends_no_authorization_header() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/chat")
            .match_header("authorization", mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#,
            )
            .create_async()
            .await;
        let client = OllamaChatClient::new(server.url(), "qwen2.5".to_string());
        let _ = client.complete("hi").await.expect("complete succeeds");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_chat_surfaces_non_2xx_with_status_and_snippet() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/api/chat")
            .with_status(404)
            .with_body(r#"{"error":"model 'nonexistent' not found"}"#)
            .create_async()
            .await;

        let client = OllamaChatClient::new(server.url(), "nonexistent".to_string());
        let err = client.complete("hi").await.expect_err("404 must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("404"), "must include status: {msg}");
        assert!(msg.contains("model 'nonexistent' not found"), "{msg}");
    }

    #[tokio::test]
    async fn ollama_chat_errors_when_response_body_is_unparseable_json() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/api/chat")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not-json")
            .create_async()
            .await;

        let client = OllamaChatClient::new(server.url(), "qwen2.5".to_string());
        let err = client
            .complete("hi")
            .await
            .expect_err("unparseable JSON must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("OllamaChatClient"),
            "decode error must name the client: {msg}"
        );
        assert!(
            msg.contains("decode failed"),
            "decode error must name the failure mode: {msg}"
        );
    }

    #[tokio::test]
    async fn ollama_chat_errors_when_response_missing_message_content() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/api/chat")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"unexpected_shape": true}"#)
            .create_async()
            .await;

        let client = OllamaChatClient::new(server.url(), "qwen2.5".to_string());
        let err = client
            .complete("hi")
            .await
            .expect_err("missing message.content must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("OllamaChatClient") && msg.contains("decode failed"),
            "must name client + decode failure: {msg}"
        );
    }

    #[tokio::test]
    async fn build_from_config_constructs_ollama_chat_client() {
        use crate::config::{ReviewerConfig, ReviewerProvider};
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/chat")
            .match_header("authorization", mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#,
            )
            .create_async()
            .await;
        let cfg = ReviewerConfig {
            enabled: true,
            provider: Some(ReviewerProvider::Ollama),
            model: "qwen2.5-coder:32b".into(),
            api_key_env: None,
            api_key: None,
            api_base_url: Some(server.url()),
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
        };
        let client = build_from_config(&cfg)
            .expect("ollama reviewer must build without api_key");
        let _ = client.complete("hi").await.expect("complete succeeds");
        mock.assert_async().await;
    }
}
