//! Mattermost ChatOps backend (EXPERIMENTAL).
//!
//! Best-effort support; no API-stability guarantees. Uses Mattermost's
//! Slack-shaped REST API with PAT auth.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;

use super::{ChatOpsBackend, HumanReply};

pub struct MattermostBackend {
    client: reqwest::Client,
    server_url: String,
    access_token: String,
    bot_user_id: String,
}

impl MattermostBackend {
    pub async fn new(server_url: String, access_token: String) -> Result<Self> {
        let client = reqwest::Client::new();
        let url = format!("{}/api/v4/users/me", server_url.trim_end_matches('/'));
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| anyhow!("mattermost users/me request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("mattermost users/me http {status}: {body}"));
        }
        let parsed: User = resp
            .json()
            .await
            .map_err(|e| anyhow!("mattermost users/me decode failed: {e}"))?;
        Ok(Self {
            client,
            server_url,
            access_token,
            bot_user_id: parsed.id,
        })
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }
}

#[async_trait]
impl ChatOpsBackend for MattermostBackend {
    fn provider_name(&self) -> &'static str {
        "mattermost"
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
        let url = format!("{}/api/v4/posts", self.server_url.trim_end_matches('/'));
        let payload = serde_json::json!({
            "channel_id": channel,
            "message": format!("❓ `{change}`: {question}"),
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("mattermost post request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("mattermost post http {status}"));
        }
        let parsed: Post = resp
            .json()
            .await
            .map_err(|e| anyhow!("mattermost post decode failed: {e}"))?;
        Ok(parsed.id)
    }

    async fn poll_thread_for_human_reply(
        &self,
        _channel: &str,
        handle: &str,
    ) -> Result<Option<HumanReply>> {
        let url = format!(
            "{}/api/v4/posts/{handle}/thread",
            self.server_url.trim_end_matches('/')
        );
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
            .map_err(|e| anyhow!("mattermost thread request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("mattermost thread http {status}"));
        }
        let parsed: PostThread = resp
            .json()
            .await
            .map_err(|e| anyhow!("mattermost thread decode failed: {e}"))?;
        let bot_user_id = &self.bot_user_id;
        // Select the earliest reply matching root_id == handle and not by us.
        let mut replies: Vec<Post> = parsed
            .posts
            .into_values()
            .filter(|p| p.root_id.as_deref() == Some(handle) && p.user_id.as_deref() != Some(bot_user_id))
            .collect();
        replies.sort_by_key(|p| p.create_at.unwrap_or(0));
        Ok(replies.into_iter().next().map(|p| HumanReply {
            text: p.message.unwrap_or_default(),
            user_id: p.user_id.unwrap_or_default(),
            ts: p.id,
        }))
    }

    async fn post_notification(&self, channel: &str, text: &str) -> Result<()> {
        let url = format!("{}/api/v4/posts", self.server_url.trim_end_matches('/'));
        let payload = serde_json::json!({
            "channel_id": channel,
            "message": text,
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("mattermost notification request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("mattermost notification http {status}"));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct User {
    id: String,
}

#[derive(Deserialize)]
struct Post {
    id: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    root_id: Option<String>,
    #[serde(default)]
    create_at: Option<i64>,
}

#[derive(Deserialize)]
struct PostThread {
    #[serde(default)]
    posts: HashMap<String, Post>,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture_backend(server: &mut mockito::Server) -> MattermostBackend {
        let _whoami = server
            .mock("GET", "/api/v4/users/me")
            .with_status(200)
            .with_body(r#"{"id":"BOT_USER_X"}"#)
            .create_async()
            .await;
        MattermostBackend::new(server.url(), "PAT_TOKEN".into())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn provider_name_and_experimental_flag() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        assert_eq!(backend.provider_name(), "mattermost");
        assert!(backend.is_experimental());
    }

    #[tokio::test]
    async fn posts_to_v4_posts_endpoint() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let mock = server
            .mock("POST", "/api/v4/posts")
            .match_header("authorization", "Bearer PAT_TOKEN")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel_id":"CHAN1","message":"❓ `make-thing`: What name?"}"#.to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":"POST_42"}"#)
            .create_async()
            .await;
        let handle = backend
            .post_question("CHAN1", "make-thing", "What name?")
            .await
            .unwrap();
        assert_eq!(handle, "POST_42");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn polls_thread_filters_bot_self() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _mock = server
            .mock("GET", "/api/v4/posts/POST_42/thread")
            .with_status(200)
            .with_body(
                r#"{"order":["POST_42","P1","P2"],"posts":{
                    "POST_42":{"id":"POST_42","message":"❓ ...","user_id":"BOT_USER_X","create_at":100},
                    "P1":{"id":"P1","message":"my own follow","user_id":"BOT_USER_X","root_id":"POST_42","create_at":101},
                    "P2":{"id":"P2","message":"hello","user_id":"USER_HUMAN","root_id":"POST_42","create_at":102}
                }}"#,
            )
            .create_async()
            .await;
        let reply = backend
            .poll_thread_for_human_reply("CHAN1", "POST_42")
            .await
            .unwrap()
            .expect("human reply");
        assert_eq!(reply.user_id, "USER_HUMAN");
        assert_eq!(reply.text, "hello");
        assert_eq!(reply.ts, "P2");
    }
}
