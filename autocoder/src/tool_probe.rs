//! Startup tool-capability probe for agentic model endpoints.
//!
//! The verifier gates ([in]/[canon]/[out]) and the agentic reviewer drive their
//! model through a tool-using CLI session: the model must call the `Read` tool to
//! open the change, then call a `submit_*` MCP tool to return its verdict. A model
//! whose endpoint cannot emit tool calls — a missing or abliterated tool template,
//! an older model family that never supported function calling — never reads the
//! change and never submits, so the fail-closed gate holds every change with an
//! inscrutable cause ("models can't use tools", stray prose).
//!
//! This probe sends ONE tool-calling request to each agentic registry model's
//! endpoint at startup and WARNs when the model does not return a tool call — so
//! the operator learns the model is unusable for the gates BEFORE a change is
//! held, instead of from a cryptic mid-run failure. Best-effort and time-bounded:
//! it never blocks startup. Scoped to `models:` registry entries (always agentic)
//! with an HTTP-reachable, tool-using provider (`ollama` / `openai_compatible`);
//! `anthropic`/`google` models drive the `claude`/`agy` CLIs, which self-
//! authenticate AND are known tool-capable (and we hold no key to probe them).

use crate::config::{Config, LlmProvider, ModelEntry};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

const PROBE_TIMEOUT_SECS: u64 = 12;

#[derive(Debug, PartialEq, Eq)]
pub enum ToolProbeOutcome {
    /// The endpoint returned at least one tool call — usable for agentic gates.
    Supported,
    /// The endpoint answered but emitted no tool call (or rejected the tools
    /// request): the model cannot drive an agentic gate.
    NoToolSupport { detail: String },
    /// The probe could not complete (connection error, timeout, 5xx, undecodable
    /// body): tool support is unverified.
    Unreachable { cause: String },
}

#[derive(Deserialize)]
struct ProbeResponse {
    #[serde(default)]
    choices: Vec<ProbeChoice>,
}

#[derive(Deserialize)]
struct ProbeChoice {
    message: ProbeMessage,
}

#[derive(Deserialize)]
struct ProbeMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<serde_json::Value>>,
}

fn excerpt(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

/// Classify an HTTP probe response (status + body) into an outcome. Pure, so the
/// tool-call-vs-prose decision is unit-tested without a live endpoint.
fn classify_probe_response(status: u16, body: &str) -> ToolProbeOutcome {
    if (200..300).contains(&status) {
        match serde_json::from_str::<ProbeResponse>(body) {
            Ok(r) => {
                let has_tool_call = r
                    .choices
                    .iter()
                    .any(|c| c.message.tool_calls.as_ref().is_some_and(|t| !t.is_empty()));
                if has_tool_call {
                    ToolProbeOutcome::Supported
                } else {
                    let content = r
                        .choices
                        .first()
                        .and_then(|c| c.message.content.clone())
                        .unwrap_or_default();
                    ToolProbeOutcome::NoToolSupport {
                        detail: format!(
                            "the endpoint accepted the request but returned no tool call (excerpt: {})",
                            excerpt(&content, 160)
                        ),
                    }
                }
            }
            Err(e) => ToolProbeOutcome::Unreachable {
                cause: format!(
                    "could not parse the probe response: {e} (excerpt: {})",
                    excerpt(body, 160)
                ),
            },
        }
    } else if (400..500).contains(&status) {
        ToolProbeOutcome::NoToolSupport {
            detail: format!(
                "the endpoint rejected the tool-calling request (HTTP {status}; excerpt: {})",
                excerpt(body, 160)
            ),
        }
    } else {
        ToolProbeOutcome::Unreachable {
            cause: format!(
                "the endpoint returned HTTP {status} (excerpt: {})",
                excerpt(body, 160)
            ),
        }
    }
}

/// Send one OpenAI-compatible tool-calling request to `<base>/chat/completions`
/// — the exact path the agentic CLI (opencode) uses — and classify the response.
async fn probe_endpoint(base: &str, model: &str, api_key: Option<&str>) -> ToolProbeOutcome {
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Capability probe: call the `probe` function with ok=true."}],
        "tools": [{"type": "function", "function": {
            "name": "probe",
            "description": "Capability probe — call this with ok=true.",
            "parameters": {"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]}
        }}],
        "tool_choice": "auto",
        "max_tokens": 64,
        "stream": false,
    });
    let mut req = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .json(&payload);
    if let Some(k) = api_key {
        req = req.header("Authorization", format!("Bearer {k}"));
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            classify_probe_response(status, &body)
        }
        Err(e) => ToolProbeOutcome::Unreachable {
            cause: format!("request failed: {e}"),
        },
    }
}

/// Resolve a registry entry's key from config (inline secret OR env var) for the
/// probe's `Authorization` header. `None` when neither is set/resolvable.
fn resolve_entry_key(entry: &ModelEntry) -> Option<String> {
    if let Some(s) = &entry.api_key {
        if let Ok(v) = s.resolve("models.api_key") {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    if let Some(env_name) = &entry.api_key_env {
        if let Ok(v) = std::env::var(env_name) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Probe every registry model that drives an agentic CLI over an HTTP-reachable,
/// tool-using endpoint, and log the result. Best-effort: never blocks startup.
pub async fn run_tool_capability_preflight(cfg: &Config) {
    let Some(models) = &cfg.models else {
        return;
    };
    for (name, entry) in models {
        if !matches!(
            entry.provider,
            LlmProvider::OpenAiCompatible | LlmProvider::Ollama
        ) {
            continue;
        }
        let Some(base) = entry
            .api_base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue; // validate_config already errors on a missing base
        };
        let key = resolve_entry_key(entry);
        if matches!(entry.provider, LlmProvider::OpenAiCompatible) && key.is_none() {
            tracing::info!(
                model = name.as_str(),
                "tool-capability probe skipped: openai_compatible model has no config key to probe with (the CLI self-authenticates at run time)"
            );
            continue;
        }
        match probe_endpoint(base, &entry.model, key.as_deref()).await {
            ToolProbeOutcome::Supported => tracing::info!(
                model = name.as_str(),
                "tool-capability probe: endpoint emits tool calls (usable for agentic gates)"
            ),
            ToolProbeOutcome::NoToolSupport { detail } => tracing::warn!(
                model = name.as_str(),
                "tool-capability probe: {detail}. The agentic verifier gates and reviewer require tool calling; this model will fail-closed and HOLD changes. Use a model whose template supports tools — `ollama show <model>` should list `tools`."
            ),
            ToolProbeOutcome::Unreachable { cause } => tracing::warn!(
                model = name.as_str(),
                "tool-capability probe could not run: {cause}. Tool support is unverified; if the endpoint is simply not up yet, the agentic gates will surface any issue at run time."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_when_response_carries_a_tool_call() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"probe","arguments":"{\"ok\":true}"}}]}}]}"#;
        assert_eq!(classify_probe_response(200, body), ToolProbeOutcome::Supported);
    }

    #[test]
    fn no_tool_support_when_response_is_prose_only() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"Sure! There is no cookie."}}]}"#;
        assert!(matches!(
            classify_probe_response(200, body),
            ToolProbeOutcome::NoToolSupport { .. }
        ));
    }

    #[test]
    fn no_tool_support_when_endpoint_rejects_tools_4xx() {
        assert!(matches!(
            classify_probe_response(400, "this model does not support tools"),
            ToolProbeOutcome::NoToolSupport { .. }
        ));
    }

    #[test]
    fn unreachable_on_5xx_and_undecodable_2xx() {
        assert!(matches!(
            classify_probe_response(503, "upstream down"),
            ToolProbeOutcome::Unreachable { .. }
        ));
        assert!(matches!(
            classify_probe_response(200, "not json at all"),
            ToolProbeOutcome::Unreachable { .. }
        ));
    }

    #[test]
    fn empty_tool_calls_array_is_not_support() {
        let body = r#"{"choices":[{"message":{"content":"hi","tool_calls":[]}}]}"#;
        assert!(matches!(
            classify_probe_response(200, body),
            ToolProbeOutcome::NoToolSupport { .. }
        ));
    }
}
