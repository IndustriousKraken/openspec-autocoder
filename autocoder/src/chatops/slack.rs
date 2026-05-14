//! Slack ChatOps backend — the officially-supported provider.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;

use super::{ChatOpsBackend, HumanReply, urlencode};

const DEFAULT_SLACK_BASE: &str = "https://slack.com/api";

pub struct SlackBackend {
    client: reqwest::Client,
    api_base: String,
    bot_token: String,
    bot_user_id: String,
}

impl SlackBackend {
    /// Construct against the real Slack API. Performs `auth.test` to cache
    /// the bot's own user_id (used to filter the bot's own messages out
    /// of thread polls).
    pub async fn new(bot_token: String) -> Result<Self> {
        Self::new_at(DEFAULT_SLACK_BASE.to_string(), bot_token).await
    }

    /// Test-only constructor allowing a non-default API base URL.
    #[doc(hidden)]
    pub async fn new_at(api_base: String, bot_token: String) -> Result<Self> {
        let client = reqwest::Client::new();
        let url = format!("{}/auth.test", api_base.trim_end_matches('/'));
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {bot_token}"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("")
            .send()
            .await
            .map_err(|e| anyhow!("slack auth.test request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("slack auth.test http {status}: {body}"));
        }
        let parsed: AuthTestResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack auth.test decode failed: {e}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack auth.test failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        let bot_user_id = parsed
            .user_id
            .ok_or_else(|| anyhow!("slack auth.test response missing user_id"))?;
        Ok(Self {
            client,
            api_base,
            bot_token,
            bot_user_id,
        })
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }
}

#[async_trait]
impl ChatOpsBackend for SlackBackend {
    fn provider_name(&self) -> &'static str {
        "slack"
    }

    fn is_experimental(&self) -> bool {
        false
    }

    async fn post_question(
        &self,
        channel: &str,
        change: &str,
        question: &str,
    ) -> Result<String> {
        let url = format!(
            "{}/chat.postMessage",
            self.api_base.trim_end_matches('/')
        );
        let text = format!("❓ `{change}`: {question}");
        let payload = serde_json::json!({
            "channel": channel,
            "text": text,
            "link_names": 1,
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("slack post request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("slack post http {status}"));
        }
        let parsed: PostMessageResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack post decode failed: {e}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack post failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        parsed
            .ts
            .ok_or_else(|| anyhow!("slack post response missing ts"))
    }

    async fn poll_thread_for_human_reply(
        &self,
        channel: &str,
        thread_ts: &str,
    ) -> Result<Option<HumanReply>> {
        let url = format!(
            "{}/conversations.replies?channel={}&ts={}",
            self.api_base.trim_end_matches('/'),
            urlencode(channel),
            urlencode(thread_ts),
        );
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .send()
            .await
            .map_err(|e| anyhow!("slack replies request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("slack replies http {status}"));
        }
        let parsed: ConversationsRepliesResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack replies decode failed: {e}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack replies failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        Ok(parsed
            .messages
            .into_iter()
            .find(|m| m.bot_id.is_none() && m.user.as_deref() != Some(&self.bot_user_id))
            .map(|m| HumanReply {
                text: m.text.unwrap_or_default(),
                user_id: m.user.unwrap_or_default(),
                ts: m.ts.unwrap_or_default(),
            }))
    }

    async fn post_notification(&self, channel: &str, text: &str) -> Result<()> {
        let url = format!(
            "{}/chat.postMessage",
            self.api_base.trim_end_matches('/')
        );
        let payload = serde_json::json!({
            "channel": channel,
            "text": text,
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("slack post_notification request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("slack post_notification http {status}"));
        }
        let parsed: PostMessageResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack post_notification decode failed: {e}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack post_notification failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct AuthTestResponse {
    ok: bool,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct PostMessageResponse {
    ok: bool,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ConversationsRepliesResponse {
    ok: bool,
    #[serde(default)]
    messages: Vec<RepliesMessage>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct RepliesMessage {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn must_err<T>(result: Result<T>, msg_hint: &str) -> anyhow::Error {
        match result {
            Ok(_) => panic!("expected Err: {msg_hint}"),
            Err(e) => e,
        }
    }

    #[tokio::test]
    async fn new_caches_bot_user_id_on_success() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/auth.test")
            .match_header("authorization", "Bearer xoxb-fixture")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
            .create_async()
            .await;

        let backend = SlackBackend::new_at(server.url(), "xoxb-fixture".to_string())
            .await
            .expect("auth ok");
        assert_eq!(backend.bot_user_id(), "U_BOT");
        assert_eq!(backend.provider_name(), "slack");
        assert!(!backend.is_experimental());
    }

    #[tokio::test]
    async fn new_returns_err_with_slack_error_field() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"invalid_auth"}"#)
            .create_async()
            .await;
        let err = must_err(
            SlackBackend::new_at(server.url(), "x".into()).await,
            "ok:false must error",
        );
        assert!(format!("{err:#}").contains("invalid_auth"), "got: {err:#}");
    }

    #[tokio::test]
    async fn new_returns_err_on_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/auth.test")
            .with_status(500)
            .with_body("server fail")
            .create_async()
            .await;
        let err = must_err(
            SlackBackend::new_at(server.url(), "x".into()).await,
            "500 must error",
        );
        assert!(format!("{err:#}").contains("500"), "got: {err:#}");
    }

    async fn fixture_backend(server: &mut mockito::Server) -> SlackBackend {
        let _auth_mock = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
            .create_async()
            .await;
        SlackBackend::new_at(server.url(), "xoxb-test".into()).await.unwrap()
    }

    #[tokio::test]
    async fn post_question_formats_text_and_returns_ts() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_header("authorization", "Bearer xoxb-test")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C0FOO","text":"❓ `make-thing`: What name?","link_names":1}"#
                    .to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1234567890.123456"}"#)
            .create_async()
            .await;

        let ts = backend
            .post_question("C0FOO", "make-thing", "What name?")
            .await
            .unwrap();
        assert_eq!(ts, "1234567890.123456");
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn post_question_returns_err_on_ok_false() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _ = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"channel_not_found"}"#)
            .create_async()
            .await;
        let err = must_err(
            backend.post_question("CBAD", "x", "q").await,
            "ok:false must error",
        );
        assert!(format!("{err:#}").contains("channel_not_found"));
    }

    #[tokio::test]
    async fn post_question_returns_err_on_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _ = server
            .mock("POST", "/chat.postMessage")
            .with_status(429)
            .with_body("rate limited")
            .create_async()
            .await;
        let err = must_err(
            backend.post_question("C", "x", "q").await,
            "429 must error",
        );
        assert!(format!("{err:#}").contains("429"));
    }

    #[tokio::test]
    async fn post_notification_posts_to_chat_postmessage() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_header("authorization", "Bearer xoxb-test")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C0FOO","text":"hello world"}"#.to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1234567890.000000"}"#)
            .create_async()
            .await;

        backend
            .post_notification("C0FOO", "hello world")
            .await
            .expect("notification posts successfully");
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn post_notification_returns_err_on_ok_false() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _ = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"channel_not_found"}"#)
            .create_async()
            .await;
        let err = must_err(
            backend.post_notification("CBAD", "hi").await,
            "ok:false must error",
        );
        assert!(
            format!("{err:#}").contains("channel_not_found"),
            "error must contain slack error field verbatim; got: {err:#}"
        );
    }

    #[tokio::test]
    async fn poll_returns_none_when_only_bot_messages() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _ = server
            .mock("GET", "/conversations.replies?channel=C&ts=1.0")
            .with_status(200)
            .with_body(
                r#"{"ok":true,"messages":[
                    {"user":"U_BOT","text":"❓ ...","ts":"1.0"},
                    {"bot_id":"B123","text":"bot edit","ts":"1.1"}
                ]}"#,
            )
            .create_async()
            .await;
        let reply = backend.poll_thread_for_human_reply("C", "1.0").await.unwrap();
        assert!(reply.is_none(), "bot-only thread must return None");
    }

    #[tokio::test]
    async fn poll_picks_first_non_bot_reply() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _ = server
            .mock("GET", "/conversations.replies?channel=C&ts=1.0")
            .with_status(200)
            .with_body(
                r#"{"ok":true,"messages":[
                    {"user":"U_BOT","text":"❓ ...","ts":"1.0"},
                    {"user":"U_HUMAN_A","text":"first answer","ts":"1.1"},
                    {"user":"U_HUMAN_B","text":"second answer","ts":"1.2"}
                ]}"#,
            )
            .create_async()
            .await;
        let reply = backend
            .poll_thread_for_human_reply("C", "1.0")
            .await
            .unwrap()
            .expect("human reply exists");
        assert_eq!(reply.user_id, "U_HUMAN_A");
        assert_eq!(reply.text, "first answer");
        assert_eq!(reply.ts, "1.1");
    }

    #[tokio::test]
    async fn poll_skips_bot_id_messages() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _ = server
            .mock("GET", "/conversations.replies?channel=C&ts=1.0")
            .with_status(200)
            .with_body(
                r#"{"ok":true,"messages":[
                    {"bot_id":"B_OTHER","user":"USOMEONE","text":"app message","ts":"1.0"},
                    {"user":"U_HUMAN","text":"real answer","ts":"1.1"}
                ]}"#,
            )
            .create_async()
            .await;
        let reply = backend.poll_thread_for_human_reply("C", "1.0").await.unwrap();
        assert_eq!(reply.unwrap().text, "real answer");
    }
}
