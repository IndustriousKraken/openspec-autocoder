//! Discord ChatOps backend (EXPERIMENTAL).
//!
//! Best-effort support; no API-stability guarantees. The implementation
//! posts via the Discord bot REST API and polls for replies whose
//! `message_reference.message_id` equals the bot post's id.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;

use super::{ChatOpsBackend, HumanReply};

const DEFAULT_DISCORD_BASE: &str = "https://discord.com/api/v10";

pub struct DiscordBackend {
    client: reqwest::Client,
    api_base: String,
    bot_token: String,
    bot_user_id: String,
}

impl DiscordBackend {
    pub async fn new(bot_token: String) -> Result<Self> {
        Self::new_at(DEFAULT_DISCORD_BASE.to_string(), bot_token).await
    }

    #[doc(hidden)]
    pub async fn new_at(api_base: String, bot_token: String) -> Result<Self> {
        let client = reqwest::Client::new();
        let url = format!("{}/users/@me", api_base.trim_end_matches('/'));
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bot {bot_token}"))
            .send()
            .await
            .map_err(|e| anyhow!("discord users/@me request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("discord users/@me http {status}: {body}"));
        }
        let parsed: WhoAmI = resp
            .json()
            .await
            .map_err(|e| anyhow!("discord users/@me decode failed: {e}"))?;
        Ok(Self {
            client,
            api_base,
            bot_token,
            bot_user_id: parsed.id,
        })
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }
}

#[async_trait]
impl ChatOpsBackend for DiscordBackend {
    fn provider_name(&self) -> &'static str {
        "discord"
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
            "{}/channels/{channel}/messages",
            self.api_base.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "content": format!("❓ `{change}`: {question}"),
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("discord post request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("discord post http {status}"));
        }
        let parsed: Message = resp
            .json()
            .await
            .map_err(|e| anyhow!("discord post decode failed: {e}"))?;
        Ok(parsed.id)
    }

    async fn poll_thread_for_human_reply(
        &self,
        channel: &str,
        handle: &str,
    ) -> Result<Option<HumanReply>> {
        let url = format!(
            "{}/channels/{channel}/messages?after={handle}&limit=50",
            self.api_base.trim_end_matches('/')
        );
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .send()
            .await
            .map_err(|e| anyhow!("discord messages request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("discord messages http {status}"));
        }
        let mut parsed: Vec<Message> = resp
            .json()
            .await
            .map_err(|e| anyhow!("discord messages decode failed: {e}"))?;
        // Discord returns newest-first when paging with `?after=`. Process
        // oldest-first so we pick the earliest matching reply.
        parsed.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(parsed
            .into_iter()
            .find(|m| {
                let refs_handle = m
                    .message_reference
                    .as_ref()
                    .and_then(|r| r.message_id.as_deref())
                    == Some(handle);
                let is_human = m
                    .author
                    .as_ref()
                    .map(|a| !a.bot.unwrap_or(false))
                    .unwrap_or(false);
                refs_handle && is_human
            })
            .map(|m| HumanReply {
                text: m.content.unwrap_or_default(),
                user_id: m.author.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
                ts: m.id,
            }))
    }

    async fn post_notification(&self, channel: &str, text: &str) -> Result<()> {
        let url = format!(
            "{}/channels/{channel}/messages",
            self.api_base.trim_end_matches('/')
        );
        let body = serde_json::json!({ "content": text });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("discord notification request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("discord notification http {status}"));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct WhoAmI {
    id: String,
}

#[derive(Deserialize)]
struct Message {
    id: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    author: Option<Author>,
    #[serde(default)]
    message_reference: Option<MessageReference>,
}

#[derive(Deserialize)]
struct Author {
    id: String,
    #[serde(default)]
    bot: Option<bool>,
}

#[derive(Deserialize)]
struct MessageReference {
    #[serde(default)]
    message_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture_backend(server: &mut mockito::Server) -> DiscordBackend {
        let _whoami = server
            .mock("GET", "/users/@me")
            .with_status(200)
            .with_body(r#"{"id":"BOT_USER_42"}"#)
            .create_async()
            .await;
        DiscordBackend::new_at(server.url(), "DISCORD_TOKEN_FIXTURE".into())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn provider_name_and_experimental_flag() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        assert_eq!(backend.provider_name(), "discord");
        assert!(backend.is_experimental());
    }

    #[tokio::test]
    async fn posts_to_messages_endpoint_with_bot_auth() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        let mock = server
            .mock("POST", "/channels/CHAN1/messages")
            .match_header("authorization", "Bot DISCORD_TOKEN_FIXTURE")
            .match_body(mockito::Matcher::JsonString(
                r#"{"content":"❓ `make-thing`: What name?"}"#.to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"id":"9999999999999999"}"#)
            .create_async()
            .await;

        let handle = backend
            .post_question("CHAN1", "make-thing", "What name?")
            .await
            .unwrap();
        assert_eq!(handle, "9999999999999999");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn polls_replies_filtered_by_message_reference() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        // One bot post + one human reply referencing the bot post.
        let _mock = server
            .mock("GET", "/channels/CHAN1/messages?after=1000&limit=50")
            .with_status(200)
            .with_body(
                r#"[
                    {"id":"1010","content":"thanks","author":{"id":"USER_HUMAN","bot":false},"message_reference":{"message_id":"1000"}},
                    {"id":"1005","content":"my own edit","author":{"id":"BOT_USER_42","bot":true},"message_reference":{"message_id":"1000"}}
                ]"#,
            )
            .create_async()
            .await;
        let reply = backend
            .poll_thread_for_human_reply("CHAN1", "1000")
            .await
            .unwrap()
            .expect("human reply");
        assert_eq!(reply.user_id, "USER_HUMAN");
        assert_eq!(reply.text, "thanks");
        assert_eq!(reply.ts, "1010");
    }

    #[tokio::test]
    async fn polls_returns_none_when_only_bot_posted() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _mock = server
            .mock("GET", "/channels/CHAN1/messages?after=1000&limit=50")
            .with_status(200)
            .with_body(
                r#"[{"id":"1005","content":"❓ ...","author":{"id":"BOT_USER_42","bot":true},"message_reference":{"message_id":"1000"}}]"#,
            )
            .create_async()
            .await;
        let reply = backend
            .poll_thread_for_human_reply("CHAN1", "1000")
            .await
            .unwrap();
        assert!(reply.is_none());
    }
}
