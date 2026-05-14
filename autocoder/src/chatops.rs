//! Slack-side communication for ChatOps escalation. Owns:
//! - Slack HTTP calls (`chat.postMessage`, `conversations.replies`, `auth.test`).
//! - The `.question.json` / `.answer.json` file lifecycle for each change,
//!   with atomic writes and idempotent deletes.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const DEFAULT_SLACK_BASE: &str = "https://slack.com/api";
const QUESTION_FILE: &str = ".question.json";
const ANSWER_FILE: &str = ".answer.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionPayload {
    pub thread_ts: String,
    pub channel: String,
    /// Opaque executor handle, serialized as-is.
    pub resume_handle: serde_json::Value,
    pub asked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerPayload {
    pub answer: String,
    pub answered_at: DateTime<Utc>,
    pub answerer_user_id: String,
}

#[derive(Debug, Clone)]
pub struct HumanReply {
    pub text: String,
    pub user_id: String,
    pub ts: String,
}

pub struct ChatOps {
    client: reqwest::Client,
    api_base: String,
    bot_token: String,
    bot_user_id: String,
}

impl ChatOps {
    /// Construct against the real Slack API. Performs `auth.test` to cache
    /// the bot's own user_id (used to filter the bot's own messages out of
    /// thread polls).
    pub async fn new(bot_token: String) -> Result<Self> {
        Self::new_at(DEFAULT_SLACK_BASE.to_string(), bot_token).await
    }

    /// Test-only constructor allowing a non-default API base URL. The
    /// production caller uses `new()` which delegates here with the real
    /// Slack base.
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

    /// Post a question to Slack. Returns the resulting thread timestamp so
    /// future polling iterations can find the human's reply.
    pub async fn post_question(
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

    /// Post a non-question notification to a Slack channel. Used for
    /// fire-and-forget operational alerts (e.g. "repo recovered from
    /// stuck state"). Unlike `post_question` this does not format the
    /// text and does not return the thread timestamp — the caller does
    /// not poll for replies.
    pub async fn post_notification(&self, channel: &str, text: &str) -> Result<()> {
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

    /// Poll the tracked thread for the earliest human reply. Returns
    /// `Some(reply)` for the first message that lacks a `bot_id` field AND
    /// whose `user` field differs from the cached bot user id. Otherwise
    /// `None`.
    pub async fn poll_thread_for_human_reply(
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
}

/// Minimal URL-encoder for Slack's GET query params. Encodes everything
/// outside the unreserved set per RFC 3986.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
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

// =====================================================================
// File lifecycle helpers
// =====================================================================

fn change_dir(workspace: &Path, change: &str) -> PathBuf {
    workspace.join("openspec/changes").join(change)
}

fn question_path(workspace: &Path, change: &str) -> PathBuf {
    change_dir(workspace, change).join(QUESTION_FILE)
}

fn answer_path(workspace: &Path, change: &str) -> PathBuf {
    change_dir(workspace, change).join(ANSWER_FILE)
}

/// Write the question file via tempfile-then-rename so a torn write is
/// never observable to a concurrent reader.
pub fn write_question_file(
    workspace: &Path,
    change: &str,
    payload: &QuestionPayload,
) -> Result<()> {
    let path = question_path(workspace, change);
    atomic_write_json(&path, payload)
}

pub fn read_question_file(workspace: &Path, change: &str) -> Result<QuestionPayload> {
    let path = question_path(workspace, change);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str::<QuestionPayload>(&raw)
        .with_context(|| format!("parsing {}", path.display()))
}

pub fn write_answer_file(
    workspace: &Path,
    change: &str,
    payload: &AnswerPayload,
) -> Result<()> {
    let path = answer_path(workspace, change);
    atomic_write_json(&path, payload)
}

pub fn read_answer_file(workspace: &Path, change: &str) -> Result<AnswerPayload> {
    let path = answer_path(workspace, change);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str::<AnswerPayload>(&raw)
        .with_context(|| format!("parsing {}", path.display()))
}

pub fn delete_question_file(workspace: &Path, change: &str) -> Result<()> {
    idempotent_remove(&question_path(workspace, change))
}

pub fn delete_answer_file(workspace: &Path, change: &str) -> Result<()> {
    idempotent_remove(&answer_path(workspace, change))
}

fn atomic_write_json<T: Serialize>(path: &Path, payload: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("destination path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating parent dir {}", parent.display()))?;
    let tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    serde_json::to_writer_pretty(&tmp, payload)
        .with_context(|| format!("serializing JSON for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
    Ok(())
}

fn idempotent_remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_change_dir(workspace: &Path, change: &str) {
        let dir = workspace.join("openspec/changes").join(change);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proposal.md"), "## Why\nfixture\n").unwrap();
    }

    // ----------------------------------------------------------------
    // ChatOps::new (auth.test)
    // ----------------------------------------------------------------

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

        let chatops = ChatOps::new_at(server.url(), "xoxb-fixture".to_string())
            .await
            .expect("auth ok");
        assert_eq!(chatops.bot_user_id(), "U_BOT");
    }

    /// Helper: assert the call errors AND return the error so the caller
    /// can inspect its message. `expect_err` requires `Debug` on the Ok
    /// type; `ChatOps` doesn't derive Debug, so we go through a match.
    fn must_err<T>(result: Result<T>, msg_hint: &str) -> anyhow::Error {
        match result {
            Ok(_) => panic!("expected Err: {msg_hint}"),
            Err(e) => e,
        }
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
            ChatOps::new_at(server.url(), "x".into()).await,
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
            ChatOps::new_at(server.url(), "x".into()).await,
            "500 must error",
        );
        assert!(format!("{err:#}").contains("500"), "got: {err:#}");
    }

    // ----------------------------------------------------------------
    // post_question
    // ----------------------------------------------------------------

    async fn fixture_chatops(server: &mut mockito::Server) -> ChatOps {
        // Auth-handshake mock so we can construct a ChatOps against a
        // mockito server. The mock is kept registered for the lifetime of
        // the server.
        let _auth_mock = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
            .create_async()
            .await;
        ChatOps::new_at(server.url(), "xoxb-test".into()).await.unwrap()
    }

    #[tokio::test]
    async fn post_question_formats_text_and_returns_ts() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;

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

        let ts = chatops
            .post_question("C0FOO", "make-thing", "What name?")
            .await
            .unwrap();
        assert_eq!(ts, "1234567890.123456");
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn post_question_returns_err_on_ok_false() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let _ = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"channel_not_found"}"#)
            .create_async()
            .await;
        let err = must_err(
            chatops.post_question("CBAD", "x", "q").await,
            "ok:false must error",
        );
        assert!(format!("{err:#}").contains("channel_not_found"));
    }

    #[tokio::test]
    async fn post_question_returns_err_on_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let _ = server
            .mock("POST", "/chat.postMessage")
            .with_status(429)
            .with_body("rate limited")
            .create_async()
            .await;
        let err = must_err(
            chatops.post_question("C", "x", "q").await,
            "429 must error",
        );
        assert!(format!("{err:#}").contains("429"));
    }

    // ----------------------------------------------------------------
    // poll_thread_for_human_reply
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn poll_returns_none_when_only_bot_messages() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
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
        let reply = chatops.poll_thread_for_human_reply("C", "1.0").await.unwrap();
        assert!(reply.is_none(), "bot-only thread must return None");
    }

    #[tokio::test]
    async fn poll_picks_first_non_bot_reply() {
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
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
        let reply = chatops
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
        let chatops = fixture_chatops(&mut server).await;
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
        let reply = chatops.poll_thread_for_human_reply("C", "1.0").await.unwrap();
        assert_eq!(reply.unwrap().text, "real answer");
    }

    // ----------------------------------------------------------------
    // File helpers
    // ----------------------------------------------------------------

    #[test]
    fn file_helpers_atomic_write_and_roundtrip() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "feature-x");

        let q = QuestionPayload {
            thread_ts: "1234.5678".into(),
            channel: "C0FOO".into(),
            resume_handle: serde_json::json!({"change":"feature-x","session_id":"s-1"}),
            asked_at: chrono::Utc::now(),
        };
        write_question_file(ws, "feature-x", &q).unwrap();
        let q2 = read_question_file(ws, "feature-x").unwrap();
        assert_eq!(q2.thread_ts, "1234.5678");
        assert_eq!(q2.channel, "C0FOO");
        assert_eq!(q2.resume_handle["change"], "feature-x");

        let a = AnswerPayload {
            answer: "use the name SAMPLE".into(),
            answered_at: chrono::Utc::now(),
            answerer_user_id: "U_HUMAN".into(),
        };
        write_answer_file(ws, "feature-x", &a).unwrap();
        let a2 = read_answer_file(ws, "feature-x").unwrap();
        assert_eq!(a2.answer, "use the name SAMPLE");

        // Verify NO tempfile leftover next to the target.
        let entries: Vec<_> = std::fs::read_dir(
            ws.join("openspec/changes/feature-x"),
        )
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
        let leftover_tmps: Vec<&String> = entries
            .iter()
            .filter(|n| n.starts_with(".tmp") || (n.starts_with('.') && n.len() > 6 && !n.starts_with(".question") && !n.starts_with(".answer")))
            .filter(|n| !["proposal.md"].contains(&n.as_str()))
            .collect();
        // We don't enforce exact tempfile naming, but we do enforce no
        // partial-write leftovers via the `.tmp` prefix.
        assert!(
            !entries.iter().any(|n| n.contains(".tmp")),
            "no `.tmp` files should leak: {entries:?}, leftover_tmps={leftover_tmps:?}"
        );
    }

    #[test]
    fn deletes_are_idempotent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "feature-y");

        // No files yet: deletes succeed.
        delete_question_file(ws, "feature-y").unwrap();
        delete_answer_file(ws, "feature-y").unwrap();

        // Create + delete once.
        let q = QuestionPayload {
            thread_ts: "x".into(),
            channel: "C".into(),
            resume_handle: serde_json::Value::Null,
            asked_at: chrono::Utc::now(),
        };
        write_question_file(ws, "feature-y", &q).unwrap();
        assert!(ws.join("openspec/changes/feature-y/.question.json").exists());
        delete_question_file(ws, "feature-y").unwrap();
        assert!(!ws.join("openspec/changes/feature-y/.question.json").exists());
        // Re-delete: still Ok.
        delete_question_file(ws, "feature-y").unwrap();
    }
}
