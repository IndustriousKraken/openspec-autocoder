//! Matrix ChatOps backend (EXPERIMENTAL).
//!
//! Best-effort support; no API-stability guarantees. Uses the Matrix
//! Client-Server API with bearer-token auth. Reply threading uses
//! `m.relates_to.m.in_reply_to` per the Matrix spec.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::RwLock;

use super::{ChatOpsBackend, HumanReply, urlencode};

pub struct MatrixBackend {
    client: reqwest::Client,
    homeserver_url: String,
    access_token: String,
    user_id: String,
    sync_from: RwLock<Option<String>>,
}

impl MatrixBackend {
    pub async fn new(homeserver_url: String, access_token: String) -> Result<Self> {
        let client = reqwest::Client::new();
        let base = homeserver_url.trim_end_matches('/').to_string();
        // Identity: whoami → user_id.
        let url = format!("{base}/_matrix/client/v3/account/whoami");
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| anyhow!("matrix whoami request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("matrix whoami http {status}: {body}"));
        }
        let who: WhoAmI = resp
            .json()
            .await
            .map_err(|e| anyhow!("matrix whoami decode failed: {e}"))?;

        // Initial /sync to obtain a next_batch token for subsequent paged reads.
        let sync_url = format!("{base}/_matrix/client/v3/sync?timeout=0");
        let resp = client
            .get(&sync_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| anyhow!("matrix initial sync request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("matrix initial sync http {status}: {body}"));
        }
        let sync: SyncResp = resp
            .json()
            .await
            .map_err(|e| anyhow!("matrix initial sync decode failed: {e}"))?;

        Ok(Self {
            client,
            homeserver_url: base,
            access_token,
            user_id: who.user_id,
            sync_from: RwLock::new(sync.next_batch),
        })
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }
}

#[async_trait]
impl ChatOpsBackend for MatrixBackend {
    fn provider_name(&self) -> &'static str {
        "matrix"
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
        let txn_id = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver_url,
            urlencode(channel),
            urlencode(&txn_id),
        );
        let body = serde_json::json!({
            "msgtype": "m.text",
            "body": format!("❓ {change}: {question}"),
        });
        let resp = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("matrix put request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("matrix put http {status}"));
        }
        let parsed: SendResp = resp
            .json()
            .await
            .map_err(|e| anyhow!("matrix put decode failed: {e}"))?;
        Ok(parsed.event_id)
    }

    async fn poll_thread_for_human_reply(
        &self,
        channel: &str,
        handle: &str,
    ) -> Result<Option<HumanReply>> {
        let from_token = self.sync_from.read().await.clone();
        let mut url = format!(
            "{}/_matrix/client/v3/rooms/{}/messages?dir=f",
            self.homeserver_url,
            urlencode(channel),
        );
        if let Some(t) = from_token.as_deref() {
            url.push_str("&from=");
            url.push_str(&urlencode(t));
        }
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
            .map_err(|e| anyhow!("matrix messages request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("matrix messages http {status}"));
        }
        let parsed: MessagesResp = resp
            .json()
            .await
            .map_err(|e| anyhow!("matrix messages decode failed: {e}"))?;

        // Persist the new pagination token so the next poll starts where this
        // one ended.
        if let Some(end) = parsed.end.clone() {
            let mut guard = self.sync_from.write().await;
            *guard = Some(end);
        }

        let user_id = self.user_id.clone();
        let found = parsed.chunk.into_iter().find(|ev| {
            let refs_handle = ev
                .content
                .as_ref()
                .and_then(|c| c.relates_to.as_ref())
                .and_then(|r| r.in_reply_to.as_ref())
                .map(|r| r.event_id.as_str())
                == Some(handle);
            let from_other = ev.sender.as_deref() != Some(&user_id);
            refs_handle && from_other
        });
        Ok(found.map(|ev| HumanReply {
            text: ev
                .content
                .as_ref()
                .and_then(|c| c.body.clone())
                .unwrap_or_default(),
            user_id: ev.sender.clone().unwrap_or_default(),
            ts: ev.event_id.clone().unwrap_or_default(),
        }))
    }

    async fn post_notification(&self, channel: &str, text: &str) -> Result<()> {
        let txn_id = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver_url,
            urlencode(channel),
            urlencode(&txn_id),
        );
        let body = serde_json::json!({
            "msgtype": "m.text",
            "body": text,
        });
        let resp = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("matrix notification request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("matrix notification http {status}"));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct WhoAmI {
    user_id: String,
}

#[derive(Deserialize)]
struct SyncResp {
    #[serde(default)]
    next_batch: Option<String>,
}

#[derive(Deserialize)]
struct SendResp {
    event_id: String,
}

#[derive(Deserialize)]
struct MessagesResp {
    #[serde(default)]
    chunk: Vec<Event>,
    #[serde(default)]
    end: Option<String>,
}

#[derive(Deserialize)]
struct Event {
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    content: Option<EventContent>,
}

#[derive(Deserialize)]
struct EventContent {
    #[serde(default)]
    body: Option<String>,
    #[serde(default, rename = "m.relates_to")]
    relates_to: Option<RelatesTo>,
}

#[derive(Deserialize)]
struct RelatesTo {
    #[serde(default, rename = "m.in_reply_to")]
    in_reply_to: Option<InReplyTo>,
}

#[derive(Deserialize)]
struct InReplyTo {
    event_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture_backend(server: &mut mockito::Server) -> MatrixBackend {
        let _whoami = server
            .mock("GET", "/_matrix/client/v3/account/whoami")
            .with_status(200)
            .with_body(r#"{"user_id":"@bot:server.tld"}"#)
            .create_async()
            .await;
        let _sync = server
            .mock("GET", "/_matrix/client/v3/sync?timeout=0")
            .with_status(200)
            .with_body(r#"{"next_batch":"s_initial"}"#)
            .create_async()
            .await;
        MatrixBackend::new(server.url(), "MATRIX_TOKEN_FIXTURE".into())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn provider_name_and_experimental_flag() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        assert_eq!(backend.provider_name(), "matrix");
        assert!(backend.is_experimental());
        assert_eq!(backend.user_id(), "@bot:server.tld");
    }

    #[tokio::test]
    async fn posts_room_message_event() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        // The URL has both room and txn_id URL-encoded.
        // Room `!abc:server.tld` → `%21abc%3Aserver.tld`.
        let mock = server
            .mock(
                "PUT",
                mockito::Matcher::Regex(
                    r"^/_matrix/client/v3/rooms/%21abc%3Aserver\.tld/send/m\.room\.message/[A-Za-z0-9%\-]+$"
                        .to_string(),
                ),
            )
            .match_header("authorization", "Bearer MATRIX_TOKEN_FIXTURE")
            .match_body(mockito::Matcher::JsonString(
                r#"{"msgtype":"m.text","body":"❓ make-thing: What name?"}"#.to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"event_id":"$abc:server.tld"}"#)
            .create_async()
            .await;
        let handle = backend
            .post_question("!abc:server.tld", "make-thing", "What name?")
            .await
            .unwrap();
        assert_eq!(handle, "$abc:server.tld");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn polls_messages_filters_by_in_reply_to() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _mock = server
            .mock(
                "GET",
                "/_matrix/client/v3/rooms/%21abc%3Aserver.tld/messages?dir=f&from=s_initial",
            )
            .with_status(200)
            .with_body(
                r#"{"chunk":[
                    {"event_id":"$bot:server.tld","sender":"@bot:server.tld","content":{"body":"❓ ...","m.relates_to":null}},
                    {"event_id":"$human:server.tld","sender":"@user:server.tld","content":{"body":"hello","m.relates_to":{"m.in_reply_to":{"event_id":"$question:server.tld"}}}}
                ],"end":"s_after"}"#,
            )
            .create_async()
            .await;
        let reply = backend
            .poll_thread_for_human_reply("!abc:server.tld", "$question:server.tld")
            .await
            .unwrap()
            .expect("human reply");
        assert_eq!(reply.user_id, "@user:server.tld");
        assert_eq!(reply.text, "hello");
        assert_eq!(reply.ts, "$human:server.tld");
    }
}
