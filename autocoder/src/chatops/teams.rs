//! Microsoft Teams ChatOps backend (EXPERIMENTAL).
//!
//! Best-effort support; no API-stability guarantees. Uses Microsoft Graph
//! with an OAuth `client_credentials` flow; the access token is cached
//! in-process and re-acquired on 401 or expiry.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use super::{ChatOpsBackend, HumanReply};

const DEFAULT_GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const DEFAULT_LOGIN_BASE: &str = "https://login.microsoftonline.com";
const GRAPH_SCOPE: &str = "https://graph.microsoft.com/.default";

struct TokenCache {
    access_token: String,
    expires_at: Instant,
}

pub struct TeamsBackend {
    client: reqwest::Client,
    api_base: String,
    login_base: String,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    team_id: String,
    token_cache: RwLock<Option<TokenCache>>,
}

impl TeamsBackend {
    pub async fn new(
        tenant_id: String,
        client_id: String,
        client_secret: String,
        team_id: String,
    ) -> Result<Self> {
        Self::new_at(
            DEFAULT_GRAPH_BASE.to_string(),
            DEFAULT_LOGIN_BASE.to_string(),
            tenant_id,
            client_id,
            client_secret,
            team_id,
        )
        .await
    }

    #[doc(hidden)]
    pub async fn new_at(
        api_base: String,
        login_base: String,
        tenant_id: String,
        client_id: String,
        client_secret: String,
        team_id: String,
    ) -> Result<Self> {
        let me = Self {
            client: reqwest::Client::new(),
            api_base,
            login_base,
            tenant_id,
            client_id,
            client_secret,
            team_id,
            token_cache: RwLock::new(None),
        };
        // Validate credentials at startup. Subsequent calls reuse the cache
        // (with 401-triggered re-acquire) so we never pre-emptively
        // round-trip.
        me.acquire_token()
            .await
            .map_err(|e| anyhow!("teams: failed to acquire initial OAuth token: {e}"))?;
        Ok(me)
    }

    pub fn bot_identity(&self) -> &str {
        &self.client_id
    }

    async fn acquire_token(&self) -> Result<String> {
        let url = format!(
            "{}/{}/oauth2/v2.0/token",
            self.login_base.trim_end_matches('/'),
            self.tenant_id
        );
        let scope_enc = super::urlencode(GRAPH_SCOPE);
        let body = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}&scope={}",
            super::urlencode(&self.client_id),
            super::urlencode(&self.client_secret),
            scope_enc
        );
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow!("teams token request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("teams token http {status}: {text}"));
        }
        let parsed: TokenResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("teams token decode failed: {e}"))?;
        let expires_at = Instant::now()
            + Duration::from_secs(parsed.expires_in.saturating_sub(30).max(0) as u64);
        let mut guard = self.token_cache.write().await;
        *guard = Some(TokenCache {
            access_token: parsed.access_token.clone(),
            expires_at,
        });
        Ok(parsed.access_token)
    }

    async fn current_token(&self) -> Result<String> {
        {
            let guard = self.token_cache.read().await;
            if let Some(cache) = guard.as_ref() {
                if cache.expires_at > Instant::now() {
                    return Ok(cache.access_token.clone());
                }
            }
        }
        self.acquire_token().await
    }
}

#[async_trait]
impl ChatOpsBackend for TeamsBackend {
    fn provider_name(&self) -> &'static str {
        "teams"
    }

    fn is_experimental(&self) -> bool {
        true
    }

    async fn post_question(
        &self,
        channel: &str,
        change: &str,
        question: &str,
    ) -> Result<String> {
        let url = format!(
            "{}/teams/{}/channels/{channel}/messages",
            self.api_base.trim_end_matches('/'),
            self.team_id
        );
        let payload = serde_json::json!({
            "body": {
                "content": format!("❓ <code>{change}</code>: {question}"),
                "contentType": "html",
            }
        });

        let token = self.current_token().await?;
        let mut resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("teams post request failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let new_token = self.acquire_token().await?;
            resp = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {new_token}"))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
                .map_err(|e| anyhow!("teams post retry failed: {e}"))?;
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("teams post http {status}"));
        }
        let parsed: TeamsMessage = resp
            .json()
            .await
            .map_err(|e| anyhow!("teams post decode failed: {e}"))?;
        Ok(parsed.id)
    }

    async fn poll_thread_for_human_reply(
        &self,
        channel: &str,
        handle: &str,
    ) -> Result<Option<HumanReply>> {
        let url = format!(
            "{}/teams/{}/channels/{channel}/messages/{handle}/replies",
            self.api_base.trim_end_matches('/'),
            self.team_id
        );
        let token = self.current_token().await?;
        let mut resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| anyhow!("teams replies request failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let new_token = self.acquire_token().await?;
            resp = self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {new_token}"))
                .send()
                .await
                .map_err(|e| anyhow!("teams replies retry failed: {e}"))?;
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("teams replies http {status}"));
        }
        let parsed: RepliesPage = resp
            .json()
            .await
            .map_err(|e| anyhow!("teams replies decode failed: {e}"))?;
        let bot_id = self.client_id.clone();
        // The Graph API returns newest-first by default; sort by createdDateTime
        // ascending so we pick the earliest matching reply.
        let mut messages = parsed.value;
        messages.sort_by(|a, b| a.created_date_time.cmp(&b.created_date_time));
        Ok(messages
            .into_iter()
            .find(|m| {
                m.from
                    .as_ref()
                    .and_then(|f| f.user.as_ref())
                    .map(|u| u.id != bot_id)
                    .unwrap_or(false)
            })
            .map(|m| HumanReply {
                text: m
                    .body
                    .as_ref()
                    .and_then(|b| b.content.clone())
                    .unwrap_or_default(),
                user_id: m
                    .from
                    .and_then(|f| f.user)
                    .map(|u| u.id)
                    .unwrap_or_default(),
                ts: m.id,
            }))
    }

    async fn post_notification(&self, channel: &str, text: &str) -> Result<()> {
        let url = format!(
            "{}/teams/{}/channels/{channel}/messages",
            self.api_base.trim_end_matches('/'),
            self.team_id
        );
        let payload = serde_json::json!({
            "body": {
                "content": text,
                "contentType": "text",
            }
        });
        let token = self.current_token().await?;
        let mut resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("teams notification request failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let new_token = self.acquire_token().await?;
            resp = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {new_token}"))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
                .map_err(|e| anyhow!("teams notification retry failed: {e}"))?;
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("teams notification http {status}"));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: i64,
}

#[derive(Deserialize)]
struct TeamsMessage {
    id: String,
    #[serde(default, rename = "createdDateTime")]
    created_date_time: Option<String>,
    #[serde(default)]
    from: Option<From>,
    #[serde(default)]
    body: Option<Body>,
}

#[derive(Deserialize)]
struct RepliesPage {
    #[serde(default)]
    value: Vec<Reply>,
}

#[derive(Deserialize)]
struct Reply {
    id: String,
    #[serde(default, rename = "createdDateTime")]
    created_date_time: Option<String>,
    #[serde(default)]
    from: Option<From>,
    #[serde(default)]
    body: Option<Body>,
}

#[derive(Deserialize)]
struct From {
    #[serde(default)]
    user: Option<User>,
}

#[derive(Deserialize)]
struct User {
    id: String,
}

#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture_backend(server: &mut mockito::Server) -> TeamsBackend {
        let _token = server
            .mock("POST", "/TENANT/oauth2/v2.0/token")
            .with_status(200)
            .with_body(r#"{"access_token":"ACCESS_TOKEN_1","expires_in":3600}"#)
            .create_async()
            .await;
        TeamsBackend::new_at(
            server.url(),
            server.url(),
            "TENANT".into(),
            "CLIENT_APP_ID".into(),
            "CLIENT_SECRET".into(),
            "TEAM_ID".into(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn acquires_token_at_construction() {
        let mut server = mockito::Server::new_async().await;
        let token_mock = server
            .mock("POST", "/TENANT/oauth2/v2.0/token")
            .with_status(200)
            .with_body(r#"{"access_token":"ACCESS_TOKEN_AT_BOOT","expires_in":3600}"#)
            .expect(1)
            .create_async()
            .await;
        let backend = TeamsBackend::new_at(
            server.url(),
            server.url(),
            "TENANT".into(),
            "CLIENT_APP_ID".into(),
            "CLIENT_SECRET".into(),
            "TEAM_ID".into(),
        )
        .await
        .unwrap();
        token_mock.assert_async().await;
        assert_eq!(backend.bot_identity(), "CLIENT_APP_ID");
        assert_eq!(backend.provider_name(), "teams");
        assert!(backend.is_experimental());
    }

    #[tokio::test]
    async fn posts_to_messages_endpoint_with_bearer_token() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        let mock = server
            .mock("POST", "/teams/TEAM_ID/channels/CHAN1/messages")
            .match_header("authorization", "Bearer ACCESS_TOKEN_1")
            .match_body(mockito::Matcher::JsonString(
                r#"{"body":{"content":"❓ <code>make-thing</code>: What name?","contentType":"html"}}"#.to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":"MSG_42"}"#)
            .create_async()
            .await;
        let handle = backend
            .post_question("CHAN1", "make-thing", "What name?")
            .await
            .unwrap();
        assert_eq!(handle, "MSG_42");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn polls_replies_filters_bot_self() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _mock = server
            .mock(
                "GET",
                "/teams/TEAM_ID/channels/CHAN1/messages/MSG_42/replies",
            )
            .with_status(200)
            .with_body(
                r#"{"value":[
                    {"id":"R1","createdDateTime":"2026-05-14T00:00:01Z","from":{"user":{"id":"CLIENT_APP_ID"}},"body":{"content":"bot self"}},
                    {"id":"R2","createdDateTime":"2026-05-14T00:00:02Z","from":{"user":{"id":"USER_HUMAN"}},"body":{"content":"hello"}}
                ]}"#,
            )
            .create_async()
            .await;
        let reply = backend
            .poll_thread_for_human_reply("CHAN1", "MSG_42")
            .await
            .unwrap()
            .expect("human reply");
        assert_eq!(reply.user_id, "USER_HUMAN");
        assert_eq!(reply.text, "hello");
        assert_eq!(reply.ts, "R2");
    }

    #[tokio::test]
    async fn re_acquires_token_on_401() {
        let mut server = mockito::Server::new_async().await;
        // Initial construction acquires once.
        let _initial = server
            .mock("POST", "/TENANT/oauth2/v2.0/token")
            .with_status(200)
            .with_body(r#"{"access_token":"OLD_TOKEN","expires_in":3600}"#)
            .expect(1)
            .create_async()
            .await;
        let backend = TeamsBackend::new_at(
            server.url(),
            server.url(),
            "TENANT".into(),
            "CLIENT_APP_ID".into(),
            "CLIENT_SECRET".into(),
            "TEAM_ID".into(),
        )
        .await
        .unwrap();

        // First post: 401. The retry uses a fresh token.
        let _first = server
            .mock("POST", "/teams/TEAM_ID/channels/CHAN1/messages")
            .match_header("authorization", "Bearer OLD_TOKEN")
            .with_status(401)
            .with_body("unauthorized")
            .expect(1)
            .create_async()
            .await;
        let _refresh = server
            .mock("POST", "/TENANT/oauth2/v2.0/token")
            .with_status(200)
            .with_body(r#"{"access_token":"NEW_TOKEN","expires_in":3600}"#)
            .expect(1)
            .create_async()
            .await;
        let _retry = server
            .mock("POST", "/teams/TEAM_ID/channels/CHAN1/messages")
            .match_header("authorization", "Bearer NEW_TOKEN")
            .with_status(201)
            .with_body(r#"{"id":"AFTER_RETRY"}"#)
            .expect(1)
            .create_async()
            .await;

        let handle = backend
            .post_question("CHAN1", "make-thing", "q?")
            .await
            .unwrap();
        assert_eq!(handle, "AFTER_RETRY");
    }
}
