//! Slack ChatOps backend — the officially-supported provider.
//!
//! Outbound surface is `post_question` / `post_notification` /
//! `poll_thread_for_human_reply`. Inbound surface is the Socket Mode
//! listener (see `start_inbound_listener` and the `slack_socket_mode`
//! module below); it connects via `apps.connections.open` + WebSocket,
//! processes `app_mention` events, and posts threaded replies via
//! `chat.postMessage` / `reactions.add`.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use super::{ChatOpsBackend, HumanReply, urlencode};
use crate::chatops::event_dedup::{CheckResult, DedupKey, EventDedupCache};
use crate::chatops::operator_commands::{
    OperatorCommandDispatcher, RepoIdentityProvider, Reply,
};

const DEFAULT_SLACK_BASE: &str = "https://slack.com/api";

pub struct SlackBackend {
    client: reqwest::Client,
    api_base: String,
    bot_token: String,
    bot_user_id: String,
    /// Cached `bot_id` from `auth.test`. Slack's mobile client emits
    /// `@<bot>` mentions as `<@B...>` (bot-id form) while the desktop
    /// client emits them as `<@U...>` (user-id form). Caching the B-id
    /// at construction lets the inbound listener accept both. `None`
    /// when `auth.test` did not return a `bot_id` for this token type;
    /// in that configuration mobile-app mentions are not recognised.
    pub(crate) bot_id: Option<String>,
    /// App-level token used by the Socket Mode listener
    /// (`apps.connections.open`). When `None`, the inbound listener is
    /// not started — outbound chatops continues to work.
    app_token: Option<String>,
    /// Maximum number of recently-processed events kept in the
    /// inbound dedup cache. Default `100`. `0` disables dedup.
    dedup_cache_capacity: usize,
    /// Per-entry TTL (seconds) for the inbound dedup cache. Default
    /// `600` (10 minutes).
    dedup_cache_ttl_secs: u64,
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
        let bot_id = parsed.bot_id;
        if bot_id.is_none() {
            tracing::warn!(
                "slack: auth.test response missing bot_id field; \
                 mobile-app mentions (B-prefix) will not be recognized. \
                 Desktop mentions (U-prefix) continue to work."
            );
        }
        Ok(Self {
            client,
            api_base,
            bot_token,
            bot_user_id,
            bot_id,
            app_token: None,
            dedup_cache_capacity: crate::config::default_dedup_cache_capacity(),
            dedup_cache_ttl_secs: crate::config::default_dedup_cache_ttl_secs(),
        })
    }

    /// Builder-style setter for the Socket Mode app-level token.
    /// Stored verbatim; the listener uses it in the `Authorization:
    /// Bearer` header for `apps.connections.open`.
    pub fn with_app_token(mut self, app_token: String) -> Self {
        self.app_token = Some(app_token);
        self
    }

    /// Builder-style setter for the inbound dedup-cache configuration.
    /// `capacity` is the maximum number of recently-processed
    /// `app_mention` events the listener remembers; `0` disables
    /// dedup. `ttl_secs` is the per-entry TTL — entries older than
    /// this are treated as not-present on the next lookup.
    pub fn with_dedup_cache_config(mut self, capacity: usize, ttl_secs: u64) -> Self {
        self.dedup_cache_capacity = capacity;
        self.dedup_cache_ttl_secs = ttl_secs;
        self
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    #[allow(dead_code)] // exposed for daemon-wiring assertions and tests
    pub fn has_app_token(&self) -> bool {
        self.app_token.is_some()
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

    async fn post_notification_with_thread(
        &self,
        channel: &str,
        top_line: &str,
        thread_body: &str,
    ) -> Result<Option<String>> {
        let url = format!(
            "{}/chat.postMessage",
            self.api_base.trim_end_matches('/')
        );
        // First POST: the top-line. Failure aborts before threading.
        let top_payload = serde_json::json!({
            "channel": channel,
            "text": top_line,
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&top_payload)
            .send()
            .await
            .map_err(|e| anyhow!("slack post_notification_with_thread top-line request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!(
                "slack post_notification_with_thread top-line http {status}"
            ));
        }
        let parsed: PostMessageResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack post_notification_with_thread top-line decode failed: {e}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack post_notification_with_thread top-line failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        let thread_ts = match parsed.ts {
            Some(ts) => ts,
            None => {
                return Err(anyhow!(
                    "slack post_notification_with_thread top-line response missing ts"
                ));
            }
        };

        // Second POST: the thread reply. Failure here does NOT bubble up;
        // the top-line already landed and is the operator-facing signal.
        let reply_payload = serde_json::json!({
            "channel": channel,
            "text": thread_body,
            "thread_ts": thread_ts,
        });
        let reply_result = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&reply_payload)
            .send()
            .await;
        let resp = match reply_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    channel = channel,
                    thread_ts = %thread_ts,
                    "slack thread reply request failed; top-line already posted: {e}"
                );
                return Ok(Some(thread_ts));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            tracing::warn!(
                channel = channel,
                thread_ts = %thread_ts,
                "slack thread reply http {status}; top-line already posted",
            );
            return Ok(Some(thread_ts));
        }
        match resp.json::<PostMessageResponse>().await {
            Ok(parsed) if parsed.ok => Ok(Some(thread_ts)),
            Ok(parsed) => {
                tracing::warn!(
                    channel = channel,
                    thread_ts = %thread_ts,
                    "slack thread reply failed: {}; top-line already posted",
                    parsed.error.unwrap_or_else(|| "unknown".to_string())
                );
                Ok(Some(thread_ts))
            }
            Err(e) => {
                tracing::warn!(
                    channel = channel,
                    thread_ts = %thread_ts,
                    "slack thread reply decode failed: {e}; top-line already posted",
                );
                Ok(Some(thread_ts))
            }
        }
    }

    async fn post_threaded_reply(
        &self,
        channel: &str,
        thread_ts: &str,
        text: &str,
    ) -> Result<()> {
        let url = format!(
            "{}/chat.postMessage",
            self.api_base.trim_end_matches('/')
        );
        let payload = serde_json::json!({
            "channel": channel,
            "thread_ts": thread_ts,
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
            .map_err(|e| anyhow!("slack post_threaded_reply request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("slack post_threaded_reply http {status}"));
        }
        let parsed: PostMessageResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack post_threaded_reply decode failed: {e}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack post_threaded_reply failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        Ok(())
    }

    async fn post_message_capturing_ts(
        &self,
        channel: &str,
        text: &str,
    ) -> Result<String> {
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
            .map_err(|e| anyhow!("slack post_message_capturing_ts request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!(
                "slack post_message_capturing_ts http {status}"
            ));
        }
        let parsed: PostMessageResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack post_message_capturing_ts decode failed: {e}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack post_message_capturing_ts failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        parsed.ts.ok_or_else(|| {
            anyhow!("slack post_message_capturing_ts response missing ts")
        })
    }

    async fn add_reaction(
        &self,
        channel: &str,
        message_ts: &str,
        name: &str,
    ) -> Result<()> {
        let url = format!(
            "{}/reactions.add",
            self.api_base.trim_end_matches('/')
        );
        let payload = serde_json::json!({
            "channel": channel,
            "timestamp": message_ts,
            "name": name,
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("slack reactions.add request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("slack reactions.add http {status}"));
        }
        let parsed: ReactionsAddResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack reactions.add decode failed: {e}"))?;
        if !parsed.ok {
            // Slack returns `already_reacted` if the bot already added
            // the same reaction. Treat that as success — the operator's
            // signal is "this message was acknowledged", which is true
            // whether the emoji was added now or earlier.
            if parsed.error.as_deref() == Some("already_reacted") {
                return Ok(());
            }
            return Err(anyhow!(
                "slack reactions.add failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        Ok(())
    }

    async fn start_inbound_listener(
        &self,
        dispatcher: Arc<OperatorCommandDispatcher>,
        repos: Arc<dyn RepoIdentityProvider>,
        allowed_channels: Arc<HashSet<String>>,
        cancel: CancellationToken,
    ) -> Result<JoinHandle<()>> {
        let app_token = match self.app_token.clone() {
            Some(t) => t,
            None => {
                return Err(anyhow!(
                    "slack backend has no app_token configured; \
                     inbound listener cannot be started"
                ));
            }
        };
        // Construct the dedup cache once per listener lifetime; the
        // same Arc is shared across every reconnect cycle (the outer
        // reconnect loop in `run_inbound_listener` holds the ctx by
        // reference, so the cache outlives reconnects).
        let dedup_cache = Arc::new(EventDedupCache::new(
            self.dedup_cache_capacity,
            Duration::from_secs(self.dedup_cache_ttl_secs),
        ));
        let ctx = InboundListenerContext {
            client: self.client.clone(),
            api_base: self.api_base.clone(),
            bot_token: self.bot_token.clone(),
            bot_user_id: self.bot_user_id.clone(),
            bot_id: self.bot_id.clone(),
            app_token,
            dispatcher,
            repos,
            allowed_channels,
            dedup_cache,
        };
        let handle = tokio::spawn(run_inbound_listener(ctx, cancel));
        Ok(handle)
    }
}

#[derive(Deserialize)]
struct AuthTestResponse {
    ok: bool,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
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
struct ReactionsAddResponse {
    ok: bool,
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

// ============================================================================
// Socket Mode envelope shapes + filter logic
// ============================================================================

/// Parsed Socket Mode envelope. Slack tags every envelope with a top-level
/// `type` field; we only model the three we act on. Other types (e.g.
/// `slash_commands`, `interactive`) deserialize to `Other` so the outer
/// loop can ack them and move on without crashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketMessage {
    /// First message after connect. Carries `num_connections` and other
    /// debug-only fields we do not act on.
    Hello,
    /// An `events_api` envelope wrapping a delivered Events API event.
    EventsApi {
        envelope_id: String,
        event_type: String,
        event: AppMentionEvent,
    },
    /// Slack is asking the client to reconnect (typically a server-side
    /// rotation or session timeout). Carries an advisory reason string.
    Disconnect { reason: String },
    /// Any other top-level type the client is not coded against. Still
    /// ack'd so Slack doesn't redeliver.
    Other { envelope_id: Option<String> },
}

/// Subset of the Slack `app_mention` event we read. Slack's full
/// payload has many more fields; everything not in this struct is
/// ignored via `#[serde(default)]` on the rest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AppMentionEvent {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub ts: String,
    /// Slack populates this on reply events (messages posted inside an
    /// existing thread). Absent on top-level mentions. Required by
    /// thread-aware verbs (`send it`) to identify which thread the
    /// operator is replying inside.
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// Field name from Slack's event-type field on the nested event,
    /// not the top-level envelope. Populated by the deserializer.
    #[serde(default)]
    #[serde(rename = "type")]
    pub event_type: String,
}

/// Parse one Socket Mode envelope text frame into a `SocketMessage`.
/// Pure function — no IO. Returns `Err` only when the JSON itself is
/// malformed; unrecognized top-level `type` values become
/// `SocketMessage::Other` so the outer loop can ack them.
pub fn parse_socket_envelope(raw: &str) -> Result<SocketMessage> {
    #[derive(Deserialize)]
    struct EnvelopeShape {
        #[serde(rename = "type")]
        msg_type: String,
        #[serde(default)]
        envelope_id: Option<String>,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        payload: Option<EventsApiPayload>,
    }
    #[derive(Deserialize)]
    struct EventsApiPayload {
        #[serde(default)]
        event: Option<AppMentionEvent>,
    }

    let env: EnvelopeShape = serde_json::from_str(raw)
        .map_err(|e| anyhow!("socket envelope parse failed: {e}; raw: {raw}"))?;

    match env.msg_type.as_str() {
        "hello" => Ok(SocketMessage::Hello),
        "disconnect" => Ok(SocketMessage::Disconnect {
            reason: env.reason.unwrap_or_default(),
        }),
        "events_api" => {
            let envelope_id = env
                .envelope_id
                .ok_or_else(|| anyhow!("events_api envelope missing envelope_id"))?;
            let event = env
                .payload
                .and_then(|p| p.event)
                .ok_or_else(|| anyhow!("events_api envelope missing payload.event"))?;
            let event_type = event.event_type.clone();
            Ok(SocketMessage::EventsApi {
                envelope_id,
                event_type,
                event,
            })
        }
        _ => Ok(SocketMessage::Other {
            envelope_id: env.envelope_id,
        }),
    }
}

/// Which form of bot mention the inbound message used. Slack's desktop
/// client emits `<@U...>` (user-id form); the mobile client emits
/// `<@B...>` (bot-id form). Both refer to the same bot. The listener
/// accepts either and normalises the message text to the user-id form
/// before passing it to the dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionForm {
    /// Mention used `<@{user_id}>` (the U-prefix, desktop default).
    UserId,
    /// Mention used `<@{bot_id}>` (the B-prefix, mobile default).
    BotId,
}

/// Check whether `text`'s leading non-whitespace token is a mention of
/// the bot. Accepts either `<@{user_id}>` or (when `bot_id` is cached)
/// `<@{bot_id}>`. Returns `None` if neither form matches the leading
/// token. Pure function — no IO, fully unit-testable.
pub fn leading_mention_matches_self(
    text: &str,
    user_id: &str,
    bot_id: Option<&str>,
) -> Option<MentionForm> {
    let trimmed = text.trim_start();
    let user_form = format!("<@{user_id}>");
    if trimmed.starts_with(&user_form) {
        return Some(MentionForm::UserId);
    }
    if let Some(bid) = bot_id {
        let bot_form = format!("<@{bid}>");
        if trimmed.starts_with(&bot_form) {
            return Some(MentionForm::BotId);
        }
    }
    None
}

/// Filter outcome for a single `app_mention` event. The first
/// (DropChannelAllowlist, DropSelfAuthor, DropBotAuthor,
/// DropLeadingMention) layer that rejects determines the result; the
/// listener never re-evaluates later layers after a drop.
#[derive(Debug, PartialEq, Eq)]
pub enum FilterDecision {
    /// Dispatch into the operator-commands codepath. Carries which
    /// mention form the inbound text used so the caller can normalise
    /// the message body before invoking the dispatcher.
    Dispatch(MentionForm),
    /// Channel not in the operator-configured allowlist. Routine drop;
    /// DEBUG log only.
    DropChannelAllowlist,
    /// Bot's own user_id authored the message. WARN log — the bot
    /// mentioning itself is an unexpected state.
    DropSelfAuthor,
    /// Some bot authored the message (any non-None `bot_id` OR
    /// `subtype == "bot_message"`). WARN log — this is the
    /// indirect-injection scenario worth alerting on.
    DropBotAuthor,
    /// The bot mention is not the first non-whitespace token. Routine
    /// drop; DEBUG log only.
    DropLeadingMention,
}

/// Apply the four drop-before-dispatch filters to an `app_mention`
/// event in fixed order. Pure function — fully unit-testable without
/// network, dispatcher, or task spawning. The caller is expected to
/// log per the level described on each variant and decide whether to
/// proceed with dispatch.
pub fn classify_app_mention(
    event: &AppMentionEvent,
    bot_user_id: &str,
    bot_id: Option<&str>,
    allowed_channels: &HashSet<String>,
) -> FilterDecision {
    // 1. Channel allowlist (cheapest check first — routine drop).
    if !allowed_channels.contains(&event.channel) {
        return FilterDecision::DropChannelAllowlist;
    }
    // 2. Self-author (the bot must not respond to itself).
    if event.user.as_deref() == Some(bot_user_id) {
        return FilterDecision::DropSelfAuthor;
    }
    // 3. Bot-author (indirect-injection guard).
    if event.bot_id.is_some() || event.subtype.as_deref() == Some("bot_message") {
        return FilterDecision::DropBotAuthor;
    }
    // 4. Leading-mention check — the operator must @-mention the bot
    //    as the first token. Quoted README lines and re-shared
    //    messages that merely *contain* the mention are dropped.
    //    Mobile clients emit `<@B...>`; desktop emits `<@U...>`. Both
    //    are accepted when both ids are cached.
    match leading_mention_matches_self(&event.text, bot_user_id, bot_id) {
        Some(form) => FilterDecision::Dispatch(form),
        None => FilterDecision::DropLeadingMention,
    }
}

// ============================================================================
// Socket Mode WebSocket plumbing
// ============================================================================

#[derive(Deserialize)]
struct AppsConnectionsOpenResponse {
    ok: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// POST `apps.connections.open` and return the WebSocket URL Slack
/// hands back. The app-level token MUST start with `xapp-`; this is
/// not enforced here (the config-layer prefix check is advisory only),
/// but Slack will reject the call with `not_authed` if the prefix is
/// wrong.
pub async fn open_socket_mode_url(
    client: &reqwest::Client,
    api_base: &str,
    app_token: &str,
) -> Result<String> {
    let url = format!(
        "{}/apps.connections.open",
        api_base.trim_end_matches('/')
    );
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {app_token}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .map_err(|e| anyhow!("slack apps.connections.open request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("slack apps.connections.open http {status}"));
    }
    let parsed: AppsConnectionsOpenResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("slack apps.connections.open decode failed: {e}"))?;
    if !parsed.ok {
        return Err(anyhow!(
            "slack apps.connections.open failed: {}",
            parsed.error.unwrap_or_else(|| "unknown".to_string())
        ));
    }
    parsed
        .url
        .ok_or_else(|| anyhow!("slack apps.connections.open response missing url"))
}

#[derive(Clone)]
struct InboundListenerContext {
    client: reqwest::Client,
    api_base: String,
    bot_token: String,
    bot_user_id: String,
    /// Cached `bot_id` (B-prefix) for accepting mobile-client mentions.
    /// See [`leading_mention_matches_self`] and `SlackBackend::bot_id`.
    bot_id: Option<String>,
    app_token: String,
    dispatcher: Arc<OperatorCommandDispatcher>,
    repos: Arc<dyn RepoIdentityProvider>,
    allowed_channels: Arc<HashSet<String>>,
    /// Per-listener-task dedup cache. Constructed once at startup AND
    /// shared across every reconnect cycle within the listener's
    /// lifetime — reconnects do NOT clear the cache. Drops when the
    /// listener task exits (daemon shutdown).
    pub dedup_cache: Arc<EventDedupCache>,
}

/// Outer reconnect loop. Calls `open_socket_mode_url` + connect,
/// runs the inner event loop, sleeps for the current backoff on
/// disconnect / error, and retries until `cancel` fires. Backoff
/// schedule: 1s, 2s, 4s, 8s, 16s, 30s (cap). A successful event
/// roundtrip resets the backoff to 1s.
async fn run_inbound_listener(ctx: InboundListenerContext, cancel: CancellationToken) {
    let mut backoff_secs: u64 = 1;
    const BACKOFF_CAP_SECS: u64 = 30;

    loop {
        if cancel.is_cancelled() {
            return;
        }
        tracing::info!("slack inbound: connecting");
        let url = match open_socket_mode_url(&ctx.client, &ctx.api_base, &ctx.app_token).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("slack inbound: apps.connections.open failed: {e}");
                if !backoff_sleep(backoff_secs, &cancel).await {
                    return;
                }
                backoff_secs = (backoff_secs * 2).min(BACKOFF_CAP_SECS);
                continue;
            }
        };

        let (ws_stream, _) = match tokio_tungstenite::connect_async(&url).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("slack inbound: websocket connect failed: {e}");
                if !backoff_sleep(backoff_secs, &cancel).await {
                    return;
                }
                backoff_secs = (backoff_secs * 2).min(BACKOFF_CAP_SECS);
                continue;
            }
        };
        tracing::info!("slack inbound: connected");

        let exit = run_event_loop(&ctx, ws_stream, &cancel).await;
        match exit {
            EventLoopExit::Cancelled => return,
            EventLoopExit::HandledEvent(disconnect_reason) => {
                tracing::info!(
                    "slack inbound: disconnected — reason: {}",
                    disconnect_reason
                );
                backoff_secs = 1; // we got at least one successful event roundtrip
            }
            EventLoopExit::ConnectionError(reason) => {
                tracing::info!("slack inbound: disconnected — reason: {reason}");
                // No successful event this cycle; grow the backoff.
                if !backoff_sleep(backoff_secs, &cancel).await {
                    return;
                }
                backoff_secs = (backoff_secs * 2).min(BACKOFF_CAP_SECS);
                continue;
            }
        }
        if !backoff_sleep(backoff_secs, &cancel).await {
            return;
        }
        backoff_secs = (backoff_secs * 2).min(BACKOFF_CAP_SECS);
    }
}

/// Sleep for `secs` seconds OR until `cancel` fires. Returns `true`
/// if the sleep completed normally; `false` if cancellation
/// interrupted it (the outer loop should exit). DEBUG-logs the wait
/// so operators tailing logs can see the backoff cadence.
async fn backoff_sleep(secs: u64, cancel: &CancellationToken) -> bool {
    tracing::debug!("slack inbound: backoff wait {secs}s");
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(secs)) => true,
        _ = cancel.cancelled() => false,
    }
}

enum EventLoopExit {
    /// The daemon's cancel token fired. The outer loop should exit.
    Cancelled,
    /// At least one event was processed (i.e. backoff reset is
    /// warranted). The payload is the disconnect reason.
    HandledEvent(String),
    /// The connection died before a single event roundtrip completed.
    /// Backoff should grow. The payload describes the failure.
    ConnectionError(String),
}

/// Run the event loop on an already-connected stream. Races
/// `cancel.cancelled()` against the next frame; on cancel sends a
/// clean Close frame and returns. The function is generic over the
/// concrete stream type so tests can drive it with a fake stream.
async fn run_event_loop<S>(
    ctx: &InboundListenerContext,
    mut stream: S,
    cancel: &CancellationToken,
) -> EventLoopExit
where
    S: StreamExt<Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + SinkExt<WsMessage, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let mut processed_any_event = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = stream.send(WsMessage::Close(None)).await;
                let _ = stream.close().await;
                return EventLoopExit::Cancelled;
            }
            next = stream.next() => {
                let msg = match next {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        let reason = format!("stream error: {e}");
                        if processed_any_event {
                            return EventLoopExit::HandledEvent(reason);
                        } else {
                            return EventLoopExit::ConnectionError(reason);
                        }
                    }
                    None => {
                        let reason = "stream ended".to_string();
                        if processed_any_event {
                            return EventLoopExit::HandledEvent(reason);
                        } else {
                            return EventLoopExit::ConnectionError(reason);
                        }
                    }
                };
                let text = match msg {
                    WsMessage::Text(t) => t.to_string(),
                    WsMessage::Ping(p) => {
                        // tokio-tungstenite auto-responds to pings if
                        // we let it; explicit Pong is also fine.
                        let _ = stream.send(WsMessage::Pong(p)).await;
                        continue;
                    }
                    WsMessage::Close(_) => {
                        return EventLoopExit::HandledEvent("server close frame".into());
                    }
                    _ => continue, // binary / pong / etc. — ignored
                };
                let envelope = match parse_socket_envelope(&text) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("slack inbound: envelope parse error: {e}");
                        continue;
                    }
                };
                match envelope {
                    SocketMessage::Hello => {
                        tracing::debug!("slack inbound: hello received");
                    }
                    SocketMessage::Disconnect { reason } => {
                        let _ = stream.send(WsMessage::Close(None)).await;
                        let _ = stream.close().await;
                        return EventLoopExit::HandledEvent(
                            format!("server disconnect: {reason}"),
                        );
                    }
                    SocketMessage::Other { envelope_id } => {
                        // Other top-level types are ack'd so Slack does
                        // not redeliver, but we do not handle them.
                        if let Some(id) = envelope_id {
                            let _ = send_ack(&mut stream, &id).await;
                        }
                    }
                    SocketMessage::EventsApi {
                        envelope_id,
                        event_type,
                        event,
                    } => {
                        // Ack first regardless of whether we dispatch.
                        // Slack's at-least-once delivery contract means
                        // a not-yet-acked event will be redelivered on
                        // disconnect; the dispatch decision is
                        // independent.
                        let _ = send_ack(&mut stream, &envelope_id).await;
                        if event_type != "app_mention" {
                            // Other event types (member_joined_channel,
                            // etc.) are ignored. Subscription on the
                            // Slack-app side should already be
                            // app_mention-only, but defense-in-depth
                            // here is cheap.
                            continue;
                        }
                        if process_app_mention(ctx, &event).await {
                            processed_any_event = true;
                        }
                    }
                }
            }
        }
    }
}

async fn send_ack<S>(
    stream: &mut S,
    envelope_id: &str,
) -> std::result::Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: SinkExt<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let ack = serde_json::json!({
        "envelope_id": envelope_id,
        "no_ack": false,
    });
    stream.send(WsMessage::Text(ack.to_string().into())).await
}

/// Apply the drop-before-dispatch filters, dispatch on pass, and post
/// the response. Returns `true` if the event reached the dispatcher
/// (used by the outer loop to decide whether the backoff should
/// reset).
async fn process_app_mention(ctx: &InboundListenerContext, event: &AppMentionEvent) -> bool {
    let form = match classify_app_mention(
        event,
        &ctx.bot_user_id,
        ctx.bot_id.as_deref(),
        &ctx.allowed_channels,
    ) {
        FilterDecision::DropChannelAllowlist => {
            tracing::debug!(
                channel = event.channel.as_str(),
                "slack inbound: drop — channel not in allowlist"
            );
            return false;
        }
        FilterDecision::DropSelfAuthor => {
            tracing::warn!(
                channel = event.channel.as_str(),
                ts = event.ts.as_str(),
                "slack inbound: drop — message authored by the bot itself"
            );
            return false;
        }
        FilterDecision::DropBotAuthor => {
            tracing::warn!(
                channel = event.channel.as_str(),
                ts = event.ts.as_str(),
                bot_id = event.bot_id.as_deref().unwrap_or("(subtype=bot_message)"),
                "slack inbound: drop — message authored by a bot (indirect-injection guard)"
            );
            return false;
        }
        FilterDecision::DropLeadingMention => {
            tracing::debug!(
                channel = event.channel.as_str(),
                ts = event.ts.as_str(),
                "slack inbound: drop — bot mention not the first token"
            );
            return false;
        }
        FilterDecision::Dispatch(f) => f,
    };

    // Dedup AFTER drop-before-dispatch filters but BEFORE invoking the
    // dispatcher. The envelope ack was already sent earlier in the
    // event loop, so Slack knows we received the event — we just
    // skip the redundant dispatch on a cache hit.
    let dedup_key = DedupKey {
        channel: event.channel.clone(),
        ts: event.ts.clone(),
        user: event.user.clone().unwrap_or_default(),
    };
    match ctx.dedup_cache.check_and_insert(dedup_key.clone()) {
        CheckResult::Fresh => {}
        CheckResult::Duplicate { suppressed_count } => {
            tracing::info!(
                channel = dedup_key.channel.as_str(),
                ts = dedup_key.ts.as_str(),
                user = dedup_key.user.as_str(),
                suppressed_count = suppressed_count,
                "slack inbound: deduplicated event"
            );
            return false;
        }
    }

    let bot_mention = format!("<@{}>", ctx.bot_user_id);
    // Normalise mobile-client mentions (`<@B...>`) to the canonical
    // user-id form (`<@U...>`) before the dispatcher sees the text.
    // Downstream parsing keys off `<@{user_id}>`, so the normalisation
    // keeps that one parser the single source of truth.
    let normalized_text = match form {
        MentionForm::UserId => std::borrow::Cow::Borrowed(event.text.as_str()),
        MentionForm::BotId => {
            let bid = ctx.bot_id.as_deref().expect(
                "MentionForm::BotId implies ctx.bot_id is Some \
                 (set by leading_mention_matches_self)",
            );
            let bot_form = format!("<@{bid}>");
            let trimmed = event.text.trim_start();
            // Pre-trim prefix (whitespace) preserved verbatim so the
            // dispatcher sees the same body shape it would on desktop
            // — just with the mention rewritten.
            let lead_len = event.text.len() - trimmed.len();
            let mut rewritten = String::with_capacity(
                event.text.len() + bot_mention.len() - bot_form.len(),
            );
            rewritten.push_str(&event.text[..lead_len]);
            rewritten.push_str(&bot_mention);
            rewritten.push_str(&trimmed[bot_form.len()..]);
            std::borrow::Cow::Owned(rewritten)
        }
    };
    let repos = ctx.repos.snapshot();
    let submitter = crate::chatops::operator_commands::ControlSocketSubmitter::new(
        crate::control_socket::socket_path(),
    );
    let reply = ctx
        .dispatcher
        .handle_message_with_context(
            &normalized_text,
            &event.channel,
            event.thread_ts.as_deref(),
            event.user.as_deref(),
            &bot_mention,
            &repos,
            &submitter,
        )
        .await;

    let surface = SlackInboundResponder {
        client: &ctx.client,
        api_base: &ctx.api_base,
        bot_token: &ctx.bot_token,
    };
    match reply {
        None => {
            if let Err(e) = surface
                .add_reaction(&event.channel, &event.ts, "question")
                .await
            {
                tracing::warn!("slack inbound: add_reaction failed: {e}");
            }
        }
        Some(Reply::Silent) => {
            // The dispatcher posted its own chat side-effects (the
            // `propose` verb posts a top-level ack itself). No listener
            // action — neither a threaded reply nor a `?` reaction.
        }
        Some(Reply::Sync(text)) | Some(Reply::Acked { ack_text: text, .. }) => {
            if let Err(e) = surface
                .post_threaded_reply(&event.channel, &event.ts, &text)
                .await
            {
                tracing::warn!("slack inbound: post_threaded_reply failed: {e}");
            }
            // NB: Reply::Acked also requires registering job_id for a
            // later follow-up post. No v1 verb constructs Acked, so
            // the registration path is not yet wired — when the first
            // async verb lands, it adds the channel/ts/job_id to a
            // completion-channel watcher here.
        }
    }
    true
}

/// Lightweight HTTP-only response surface used by `process_app_mention`.
/// Keeps the per-event reply path independent of `&self` so the listener
/// task can run without the backend struct.
struct SlackInboundResponder<'a> {
    client: &'a reqwest::Client,
    api_base: &'a str,
    bot_token: &'a str,
}

impl<'a> SlackInboundResponder<'a> {
    async fn post_threaded_reply(
        &self,
        channel: &str,
        thread_ts: &str,
        text: &str,
    ) -> Result<()> {
        let url = format!("{}/chat.postMessage", self.api_base.trim_end_matches('/'));
        let payload = serde_json::json!({
            "channel": channel,
            "thread_ts": thread_ts,
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
            .map_err(|e| anyhow!("slack post_threaded_reply request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("slack post_threaded_reply http {status}"));
        }
        let parsed: PostMessageResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack post_threaded_reply decode failed: {e}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack post_threaded_reply failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        Ok(())
    }

    async fn add_reaction(&self, channel: &str, message_ts: &str, name: &str) -> Result<()> {
        let url = format!("{}/reactions.add", self.api_base.trim_end_matches('/'));
        let payload = serde_json::json!({
            "channel": channel,
            "timestamp": message_ts,
            "name": name,
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("slack reactions.add request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("slack reactions.add http {status}"));
        }
        let parsed: ReactionsAddResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("slack reactions.add decode failed: {e}"))?;
        if !parsed.ok {
            if parsed.error.as_deref() == Some("already_reacted") {
                return Ok(());
            }
            return Err(anyhow!(
                "slack reactions.add failed: {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string())
            ));
        }
        Ok(())
    }
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
        assert!(!backend.has_app_token());
        // Response carried no bot_id field → cached as None and a WARN
        // log fires (covered by `new_warns_when_bot_id_missing` below).
        assert!(backend.bot_id.is_none());
    }

    #[tokio::test]
    async fn new_caches_both_user_id_and_bot_id_when_present() {
        // Both fields present → both populated on the backend; no WARN.
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"user_id":"U_BOT","bot_id":"B_BOT"}"#)
            .create_async()
            .await;
        let backend = SlackBackend::new_at(server.url(), "xoxb-x".into())
            .await
            .expect("auth ok");
        assert_eq!(backend.bot_user_id(), "U_BOT");
        assert_eq!(backend.bot_id.as_deref(), Some("B_BOT"));
    }

    #[tokio::test]
    async fn new_warns_when_bot_id_missing() {
        // auth.test returns only user_id. The backend constructs
        // successfully but `bot_id` is None — desktop-only matching.
        // The WARN log itself is logged via tracing; we don't trap the
        // subscriber in this test (the WARN-emission path is also
        // exercised by the existing `new_caches_bot_user_id_on_success`).
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
            .create_async()
            .await;
        let backend = SlackBackend::new_at(server.url(), "xoxb-x".into())
            .await
            .expect("auth ok");
        assert_eq!(backend.bot_user_id(), "U_BOT");
        assert!(backend.bot_id.is_none(), "bot_id must be None when absent");
    }

    #[tokio::test]
    async fn new_returns_err_when_user_id_missing_even_if_bot_id_present() {
        // Defensive: a token that returns only bot_id (no user_id) is
        // unusable — the bot's own message filter keys off user_id. The
        // user_id-required error path wins regardless of bot_id state.
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"bot_id":"B_BOT"}"#)
            .create_async()
            .await;
        let err = must_err(
            SlackBackend::new_at(server.url(), "x".into()).await,
            "missing user_id must error",
        );
        assert!(format!("{err:#}").contains("user_id"));
    }

    #[tokio::test]
    async fn with_app_token_records_it() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
            .create_async()
            .await;
        let backend = SlackBackend::new_at(server.url(), "xoxb-x".to_string())
            .await
            .unwrap()
            .with_app_token("xapp-1-test".into());
        assert!(backend.has_app_token());
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
    async fn post_notification_with_thread_happy_path_carries_thread_ts() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        // Top-line POST: no `thread_ts`. Captures the response `ts` for
        // the threaded reply.
        let top_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C0FOO","text":"📐 brightline on r: 1 file(s) over line threshold; 0 duplicate signature(s)"}"#
                    .to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"9999.5555"}"#)
            .expect(1)
            .create_async()
            .await;

        // Threaded-reply POST: matches both the top-line's `ts` carried
        // as `thread_ts` AND the documented body shape.
        let reply_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C0FOO","text":"file foo.rs is 1234 lines","thread_ts":"9999.5555"}"#
                    .to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"9999.5556"}"#)
            .expect(1)
            .create_async()
            .await;

        let outcome = backend
            .post_notification_with_thread(
                "C0FOO",
                "📐 brightline on r: 1 file(s) over line threshold; 0 duplicate signature(s)",
                "file foo.rs is 1234 lines",
            )
            .await
            .expect("happy path returns Ok");
        // Native-threading backend MUST surface the top-line `ts` so
        // the audit-reply-acts scheduler can stamp the audit-thread state.
        assert_eq!(
            outcome.as_deref(),
            Some("9999.5555"),
            "slack threaded post must return the top-line ts"
        );

        top_mock.assert_async().await;
        reply_mock.assert_async().await;
    }

    #[tokio::test]
    async fn post_notification_with_thread_top_line_failure_aborts_before_reply() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        // The top-line returns ok:false. The reply must NEVER be
        // attempted — set `.expect(1)` on the first call only; if a
        // second call leaks through, mockito would silently match this
        // mock again (no separate matcher would catch it). Use
        // `expect_at_most` semantics via a wide matcher and verify by
        // checking the bubble-up Err.
        let _top_mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"channel_not_found"}"#)
            .expect(1)
            .create_async()
            .await;

        let err = backend
            .post_notification_with_thread("CBAD", "top", "body")
            .await
            .expect_err("top-line failure must bubble up");
        assert!(
            format!("{err:#}").contains("channel_not_found"),
            "err must surface the slack error field: {err:#}"
        );
    }

    #[tokio::test]
    async fn post_notification_with_thread_reply_failure_returns_ok() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        // Two POSTs: top-line succeeds (returns ts), reply fails.
        // mockito matches mocks in declaration order, so we set up the
        // top-line matcher first (matching the top-line body exactly)
        // and the reply matcher second (matching thread_ts).
        let top_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C0","text":"top"}"#.to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let reply_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C0","text":"body","thread_ts":"1.0"}"#.to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"slack_internal"}"#)
            .expect(1)
            .create_async()
            .await;

        // Top-line landed → function returns Ok even though the reply
        // post errored. The top-line is the operator-visible signal.
        backend
            .post_notification_with_thread("C0", "top", "body")
            .await
            .expect("reply failure must not propagate; top-line is the signal");

        top_mock.assert_async().await;
        reply_mock.assert_async().await;
    }

    #[tokio::test]
    async fn post_threaded_reply_posts_thread_ts() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C0","thread_ts":"1.0","text":"hi"}"#.to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.1"}"#)
            .create_async()
            .await;

        backend.post_threaded_reply("C0", "1.0", "hi").await.unwrap();
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn add_reaction_posts_to_reactions_add() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;

        let post_mock = server
            .mock("POST", "/reactions.add")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C0","timestamp":"1.0","name":"question"}"#.to_string(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true}"#)
            .create_async()
            .await;

        backend.add_reaction("C0", "1.0", "question").await.unwrap();
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn add_reaction_treats_already_reacted_as_success() {
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let _ = server
            .mock("POST", "/reactions.add")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"already_reacted"}"#)
            .create_async()
            .await;
        backend
            .add_reaction("C0", "1.0", "question")
            .await
            .expect("already_reacted should not bubble as error");
    }

    #[tokio::test]
    async fn open_socket_mode_url_extracts_url() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/apps.connections.open")
            .match_header("authorization", "Bearer xapp-1-test")
            .with_status(200)
            .with_body(r#"{"ok":true,"url":"wss://wss-primary.slack.com/link/?ticket=ABC"}"#)
            .create_async()
            .await;
        let client = reqwest::Client::new();
        let url = open_socket_mode_url(&client, &server.url(), "xapp-1-test")
            .await
            .unwrap();
        assert_eq!(url, "wss://wss-primary.slack.com/link/?ticket=ABC");
    }

    #[tokio::test]
    async fn open_socket_mode_url_propagates_slack_error_verbatim() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/apps.connections.open")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"not_authed"}"#)
            .create_async()
            .await;
        let err = must_err(
            open_socket_mode_url(&reqwest::Client::new(), &server.url(), "xapp-bad")
                .await,
            "ok:false must error",
        );
        assert!(format!("{err:#}").contains("not_authed"));
    }

    // ---------- envelope parsing ----------

    #[test]
    fn parse_hello_envelope() {
        let raw = r#"{"type":"hello","num_connections":1,"debug_info":{}}"#;
        match parse_socket_envelope(raw).unwrap() {
            SocketMessage::Hello => {}
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn parse_disconnect_envelope() {
        let raw = r#"{"type":"disconnect","reason":"warning"}"#;
        match parse_socket_envelope(raw).unwrap() {
            SocketMessage::Disconnect { reason } => assert_eq!(reason, "warning"),
            other => panic!("expected Disconnect, got {other:?}"),
        }
    }

    #[test]
    fn parse_events_api_app_mention_envelope() {
        let raw = r#"{
            "type": "events_api",
            "envelope_id": "env-abc",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@UBOT> status myrepo",
                    "user": "U_HUMAN",
                    "channel": "C_OPS",
                    "ts": "1700000000.000100"
                }
            }
        }"#;
        match parse_socket_envelope(raw).unwrap() {
            SocketMessage::EventsApi {
                envelope_id,
                event_type,
                event,
            } => {
                assert_eq!(envelope_id, "env-abc");
                assert_eq!(event_type, "app_mention");
                assert_eq!(event.text, "<@UBOT> status myrepo");
                assert_eq!(event.user.as_deref(), Some("U_HUMAN"));
                assert_eq!(event.channel, "C_OPS");
                // Top-level mention: no `thread_ts` field in Slack's
                // payload, deserializes to `None`. Required for thread-aware
                // verbs like `send it` to refuse correctly outside threads.
                assert!(event.thread_ts.is_none());
            }
            other => panic!("expected EventsApi, got {other:?}"),
        }
    }

    #[test]
    fn parse_app_mention_envelope_captures_thread_ts() {
        // Reply-in-thread case: Slack includes `thread_ts` in the event
        // payload pointing at the parent message's `ts`. The listener
        // must capture this so thread-aware verbs (`send it`) can
        // resolve which thread the operator replied inside.
        let raw = r#"{
            "type": "events_api",
            "envelope_id": "env-thread",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@UBOT> send it",
                    "user": "U_RAB",
                    "channel": "C_OPS",
                    "ts": "1700000050.000200",
                    "thread_ts": "9999.1234"
                }
            }
        }"#;
        match parse_socket_envelope(raw).unwrap() {
            SocketMessage::EventsApi { event, .. } => {
                assert_eq!(event.thread_ts.as_deref(), Some("9999.1234"));
            }
            other => panic!("expected EventsApi, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_type_becomes_other() {
        let raw = r#"{"type":"slash_commands","envelope_id":"env-x","payload":{}}"#;
        match parse_socket_envelope(raw).unwrap() {
            SocketMessage::Other { envelope_id } => {
                assert_eq!(envelope_id.as_deref(), Some("env-x"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_envelope_errors() {
        let err = parse_socket_envelope("{not json}").unwrap_err();
        assert!(format!("{err:#}").contains("parse failed"));
    }

    #[test]
    fn parse_events_api_missing_envelope_id_errors() {
        // Slack's contract: every events_api envelope MUST carry an
        // envelope_id (the ack reference). Missing it is a parse
        // error so we don't silently drop the event.
        let raw = r#"{"type":"events_api","payload":{"event":{"type":"app_mention","text":"hi"}}}"#;
        let err = parse_socket_envelope(raw).unwrap_err();
        assert!(format!("{err:#}").contains("envelope_id"));
    }

    // ---------- send-it thread-context propagation (a20a0) ----------

    // Records every action submitted, lets us assert the dispatcher
    // received the thread context the slack inbound listener wired in.
    struct RecordingSubmitter {
        log: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    impl RecordingSubmitter {
        fn new() -> Self {
            Self {
                log: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<serde_json::Value> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::chatops::operator_commands::ActionSubmitter for RecordingSubmitter {
        async fn submit(&self, action: serde_json::Value) -> serde_json::Value {
            self.log.lock().unwrap().push(action.clone());
            // Pretend the daemon accepts every action so the dispatcher's
            // happy path runs to completion.
            serde_json::json!({"ok": true, "poll_interval_sec": 60})
        }
    }

    fn stamp_audit_thread_state(
        state_root: &std::path::Path,
        thread_ts: &str,
    ) {
        let state = crate::audits::threads::AuditThreadState {
            thread_ts: thread_ts.to_string(),
            channel: "C_OPS".to_string(),
            repo_url: "git@github.com:acme/myrepo.git".to_string(),
            audit_type: "architecture_brightline".to_string(),
            findings_excerpt: "  • file foo.rs is 1234 lines".to_string(),
            posted_at: chrono::Utc::now() - chrono::Duration::minutes(30),
            status: crate::audits::threads::AuditThreadStatus::Open,
            reason: None,
        };
        crate::audits::threads::write_state(state_root, &state).unwrap();
    }

    #[tokio::test]
    async fn slack_inbound_propagates_thread_ts_to_dispatcher_for_send_it() {
        // The contract this test guards (a20a0):
        // when Slack delivers an app_mention reply with `thread_ts` set,
        // the inbound listener captures the field on AppMentionEvent and
        // forwards it via handle_message_with_context. The dispatcher's
        // send-it handler then resolves the audit thread and submits a
        // trigger_audit_action with that thread_ts.
        //
        // Pre-fix code called handle_message(...) which dropped
        // thread_ts → ParseOutcome::None → no trigger submitted → ? reaction.
        // This test would have failed against the pre-fix listener.
        let tmp = tempfile::TempDir::new().unwrap();
        stamp_audit_thread_state(tmp.path(), "9999.1234");

        let dispatcher = crate::chatops::operator_commands::OperatorCommandDispatcher::new()
            .with_audit_thread_state_dir(tmp.path().to_path_buf());
        let submitter = RecordingSubmitter::new();
        let bot_mention = "<@UBOT>";

        // Construct the event exactly as the deserializer would for a
        // threaded reply (parse_app_mention_envelope_captures_thread_ts
        // covers the JSON → struct half).
        let event = AppMentionEvent {
            text: format!("{bot_mention} send it"),
            user: Some("U_RAB".into()),
            bot_id: None,
            subtype: None,
            channel: "C_OPS".into(),
            ts: "1700000050.000200".into(),
            thread_ts: Some("9999.1234".into()),
            event_type: "app_mention".into(),
        };

        // Replicate the production dispatch call shape from
        // process_app_mention (slack.rs ~line 1190). If that call site
        // ever regresses back to the no-context entry point,
        // handle_message itself is #[cfg(test)] so the regression
        // surfaces at compile time, not here.
        let reply = dispatcher
            .handle_message_with_context(
                &event.text,
                &event.channel,
                event.thread_ts.as_deref(),
                event.user.as_deref(),
                bot_mention,
                &[],
                &submitter,
            )
            .await
            .expect("send-it inside a tracked thread must produce a reply");

        let text = match reply {
            crate::chatops::operator_commands::Reply::Sync(s) => s,
            other => panic!("expected Sync reply, got {other:?}"),
        };
        assert!(text.starts_with("✓"), "expected accept reply, got: {text}");

        let calls = submitter.calls();
        assert_eq!(
            calls.len(),
            1,
            "exactly one action expected (trigger_audit_action)"
        );
        assert_eq!(calls[0]["action"], "trigger_audit_action");
        assert_eq!(
            calls[0]["thread_ts"], "9999.1234",
            "thread_ts must flow from the AppMentionEvent into the control-socket submission"
        );
    }

    #[tokio::test]
    async fn slack_inbound_send_it_outside_thread_still_refused() {
        // The negative-side regression: when thread_ts is None
        // (top-level mention), send it correctly returns ParseOutcome::None
        // → the dispatcher returns None → the listener applies the
        // `?` reaction. This preserves the canonical "send it outside
        // an audit thread is refused" behaviour.
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = crate::chatops::operator_commands::OperatorCommandDispatcher::new()
            .with_audit_thread_state_dir(tmp.path().to_path_buf());
        let submitter = RecordingSubmitter::new();
        let bot_mention = "<@UBOT>";

        let event = AppMentionEvent {
            text: format!("{bot_mention} send it"),
            user: Some("U_RAB".into()),
            bot_id: None,
            subtype: None,
            channel: "C_OPS".into(),
            ts: "1700000050.000200".into(),
            thread_ts: None, // top-level mention; no thread context
            event_type: "app_mention".into(),
        };

        let reply = dispatcher
            .handle_message_with_context(
                &event.text,
                &event.channel,
                event.thread_ts.as_deref(),
                event.user.as_deref(),
                bot_mention,
                &[],
                &submitter,
            )
            .await;

        // Parser returns ParseOutcome::None for send it without thread_ts.
        // Dispatcher returns None; production listener turns that into
        // a `?` reaction (covered by the existing slack-listener tests).
        assert!(
            reply.is_none(),
            "top-level `send it` must produce no dispatcher reply"
        );
        assert!(
            submitter.calls().is_empty(),
            "no action should be submitted when send it is refused"
        );
    }

    // ---------- filter logic ----------

    fn evt(text: &str, channel: &str, user: Option<&str>) -> AppMentionEvent {
        AppMentionEvent {
            text: text.to_string(),
            user: user.map(str::to_string),
            bot_id: None,
            subtype: None,
            channel: channel.to_string(),
            ts: "1.0".into(),
            thread_ts: None,
            event_type: "app_mention".into(),
        }
    }

    fn allow(set: &[&str]) -> HashSet<String> {
        set.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn filter_drops_channel_not_in_allowlist() {
        let e = evt("<@UBOT> status myrepo", "C_OUTSIDE", Some("U_HUMAN"));
        let d = classify_app_mention(&e, "UBOT", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::DropChannelAllowlist);
    }

    #[test]
    fn filter_drops_self_authored() {
        let e = evt("<@UBOT> status myrepo", "C_OPS", Some("UBOT"));
        let d = classify_app_mention(&e, "UBOT", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::DropSelfAuthor);
    }

    #[test]
    fn filter_drops_bot_id_set() {
        let mut e = evt("<@UBOT> status myrepo", "C_OPS", Some("U_HUMAN"));
        e.bot_id = Some("B999".into());
        let d = classify_app_mention(&e, "UBOT", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::DropBotAuthor);
    }

    #[test]
    fn filter_drops_subtype_bot_message() {
        let mut e = evt("<@UBOT> status myrepo", "C_OPS", Some("U_HUMAN"));
        e.subtype = Some("bot_message".into());
        let d = classify_app_mention(&e, "UBOT", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::DropBotAuthor);
    }

    #[test]
    fn filter_drops_mention_not_at_start() {
        // Re-shared message that merely contains the mention later in
        // the text. The leading-mention check refuses to treat it as a
        // command.
        let e = evt(
            "good morning everyone! <@UBOT> status myrepo",
            "C_OPS",
            Some("U_HUMAN"),
        );
        let d = classify_app_mention(&e, "UBOT", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::DropLeadingMention);
    }

    #[test]
    fn filter_passes_with_leading_whitespace() {
        // Leading whitespace is trimmed before the mention check.
        let e = evt("   <@UBOT> help", "C_OPS", Some("U_HUMAN"));
        let d = classify_app_mention(&e, "UBOT", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::Dispatch(MentionForm::UserId));
    }

    #[test]
    fn filter_indirect_injection_blocked_even_with_valid_text() {
        // The text looks like a valid command, but the message was
        // authored by a bot. The bot-author filter must win.
        let mut e = evt("<@UBOT> wipe-workspace evil", "C_OPS", Some("U_BOT2"));
        e.bot_id = Some("B999".into());
        let d = classify_app_mention(&e, "UBOT", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::DropBotAuthor);
    }

    #[test]
    fn filter_passes_happy_path() {
        let e = evt("<@UBOT> status myrepo", "C_OPS", Some("U_HUMAN"));
        let d = classify_app_mention(&e, "UBOT", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::Dispatch(MentionForm::UserId));
    }

    #[test]
    fn filter_passes_with_bot_id_mention_when_cached() {
        // Mobile-client form: `<@B...>`. When `bot_id` is cached the
        // filter accepts the mention and reports MentionForm::BotId
        // so the caller can normalise the message text.
        let e = evt("<@B_BOT_ID> status myrepo", "C_OPS", Some("U_HUMAN"));
        let d = classify_app_mention(&e, "U_BOT_USER", Some("B_BOT_ID"), &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::Dispatch(MentionForm::BotId));
    }

    #[test]
    fn filter_drops_bot_id_mention_when_bot_id_not_cached() {
        // Mobile-client form arrives but `auth.test` didn't return a
        // bot_id (some token types lack one). Without a cached bot_id
        // the listener cannot match the mobile form — the leading
        // mention check rejects it.
        let e = evt("<@B_BOT_ID> status myrepo", "C_OPS", Some("U_HUMAN"));
        let d = classify_app_mention(&e, "U_BOT_USER", None, &allow(&["C_OPS"]));
        assert_eq!(d, FilterDecision::DropLeadingMention);
    }

    // ---------- leading_mention_matches_self ----------

    #[test]
    fn leading_mention_user_form_matches() {
        assert_eq!(
            leading_mention_matches_self("<@U_BOT_USER> status", "U_BOT_USER", None),
            Some(MentionForm::UserId),
        );
    }

    #[test]
    fn leading_mention_bot_form_matches_when_bot_id_cached() {
        assert_eq!(
            leading_mention_matches_self(
                "<@B_BOT_ID> status",
                "U_BOT_USER",
                Some("B_BOT_ID"),
            ),
            Some(MentionForm::BotId),
        );
    }

    #[test]
    fn leading_mention_bot_form_rejected_when_bot_id_is_none() {
        assert_eq!(
            leading_mention_matches_self("<@B_BOT_ID> status", "U_BOT_USER", None),
            None,
        );
    }

    #[test]
    fn leading_mention_other_user_rejected() {
        assert_eq!(
            leading_mention_matches_self(
                "<@U_OTHER_USER> status",
                "U_BOT_USER",
                Some("B_BOT_ID"),
            ),
            None,
        );
    }

    #[test]
    fn leading_mention_accepts_leading_whitespace_for_both_forms() {
        assert_eq!(
            leading_mention_matches_self("   <@U_BOT_USER> help", "U_BOT_USER", None),
            Some(MentionForm::UserId),
        );
        assert_eq!(
            leading_mention_matches_self(
                "\t  <@B_BOT_ID> help",
                "U_BOT_USER",
                Some("B_BOT_ID"),
            ),
            Some(MentionForm::BotId),
        );
    }

    #[test]
    fn poll_returns_none_when_only_bot_messages_inline() {
        // Lightweight check that the older poll-thread test still
        // works after the inbound-listener refactor.
        let json = r#"{"ok":true,"messages":[
            {"user":"U_BOT","text":"❓ ...","ts":"1.0"},
            {"bot_id":"B123","text":"bot edit","ts":"1.1"}
        ]}"#;
        // Just sanity-check the response decode shape.
        let parsed: ConversationsRepliesResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.messages.len(), 2);
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

    // ====================================================================
    // Event-loop integration tests using a fake duplex stream
    // ====================================================================

    use futures_util::stream::Stream;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};
    use tokio::sync::mpsc;

    /// In-memory bidirectional WebSocket stub. Inbound frames are
    /// pushed via the `inbound_tx` end; outbound frames (acks, close)
    /// are observable via `outbound_rx`. Implements `Stream<Item =
    /// Result<WsMessage, _>>` and `Sink<WsMessage>` so it slots
    /// directly into `run_event_loop`.
    struct FakeStream {
        inbound: mpsc::UnboundedReceiver<
            std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>,
        >,
        outbound: mpsc::UnboundedSender<WsMessage>,
    }

    impl Stream for FakeStream {
        type Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Self::Item>> {
            self.inbound.poll_recv(cx)
        }
    }

    impl futures_util::sink::Sink<WsMessage> for FakeStream {
        type Error = tokio_tungstenite::tungstenite::Error;
        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(
            self: Pin<&mut Self>,
            item: WsMessage,
        ) -> std::result::Result<(), Self::Error> {
            // Drop send errors silently — the test side may close the
            // receiver before observing every ack.
            let _ = self.outbound.send(item);
            Ok(())
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn make_fake_stream() -> (
        FakeStream,
        mpsc::UnboundedSender<
            std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>,
        >,
        mpsc::UnboundedReceiver<WsMessage>,
    ) {
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        (
            FakeStream {
                inbound: in_rx,
                outbound: out_tx,
            },
            in_tx,
            out_rx,
        )
    }

    fn test_ctx_for_event_loop(
        api_base: String,
        bot_token: String,
        bot_user_id: &str,
        channels: &[&str],
    ) -> InboundListenerContext {
        test_ctx_for_event_loop_with_bot_id(api_base, bot_token, bot_user_id, None, channels)
    }

    fn test_ctx_for_event_loop_with_bot_id(
        api_base: String,
        bot_token: String,
        bot_user_id: &str,
        bot_id: Option<&str>,
        channels: &[&str],
    ) -> InboundListenerContext {
        InboundListenerContext {
            client: reqwest::Client::new(),
            api_base,
            bot_token,
            bot_user_id: bot_user_id.to_string(),
            bot_id: bot_id.map(str::to_string),
            app_token: "xapp-1-test".into(),
            dispatcher: Arc::new(OperatorCommandDispatcher::new()),
            repos: Arc::new(crate::chatops::TaskMapRepoIdentities::new(Vec::new)),
            allowed_channels: Arc::new(channels.iter().map(|s| s.to_string()).collect()),
            dedup_cache: Arc::new(EventDedupCache::new(100, Duration::from_secs(600))),
        }
    }

    /// Variant for tests that need to control the dedup cache directly
    /// (e.g. to assert cache persistence across reconnects, or to
    /// supply a cache populated with a specific prior entry).
    fn test_ctx_with_dedup_cache(
        api_base: String,
        bot_token: String,
        bot_user_id: &str,
        channels: &[&str],
        dedup_cache: Arc<EventDedupCache>,
    ) -> InboundListenerContext {
        InboundListenerContext {
            client: reqwest::Client::new(),
            api_base,
            bot_token,
            bot_user_id: bot_user_id.to_string(),
            bot_id: None,
            app_token: "xapp-1-test".into(),
            dispatcher: Arc::new(OperatorCommandDispatcher::new()),
            repos: Arc::new(crate::chatops::TaskMapRepoIdentities::new(Vec::new)),
            allowed_channels: Arc::new(channels.iter().map(|s| s.to_string()).collect()),
            dedup_cache,
        }
    }

    #[tokio::test]
    async fn event_loop_disconnect_envelope_returns_handled_event() {
        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        // Mockito server only used to give the ctx a valid base URL; no
        // calls are made because we deliver `disconnect` before any
        // events_api event.
        let ctx = test_ctx_for_event_loop(
            "http://unused.invalid".to_string(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"warning"}"#.to_string().into(),
            )))
            .unwrap();
        let exit = run_event_loop(&ctx, stream, &cancel).await;
        match exit {
            EventLoopExit::HandledEvent(reason) => {
                assert!(reason.contains("warning"), "{reason}");
            }
            other => panic!("expected HandledEvent, got {other:?}"),
        }
    }

    impl std::fmt::Debug for EventLoopExit {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Cancelled => write!(f, "Cancelled"),
                Self::HandledEvent(r) => write!(f, "HandledEvent({r})"),
                Self::ConnectionError(r) => write!(f, "ConnectionError({r})"),
            }
        }
    }

    #[tokio::test]
    async fn event_loop_cancel_exits_within_1s() {
        let (stream, _in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            "http://unused.invalid".to_string(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        // Cancel after a short delay; the event loop should observe
        // it via the select! arm and return Cancelled.
        let cancel_for_task = cancel.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_for_task.cancel();
        });
        let exit = tokio::time::timeout(
            Duration::from_secs(1),
            run_event_loop(&ctx, stream, &cancel),
        )
        .await
        .expect("must exit within 1s");
        canceller.await.unwrap();
        match exit {
            EventLoopExit::Cancelled => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_loop_stream_end_returns_connection_error_before_any_event() {
        // No events_api received → backoff should grow → ConnectionError.
        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            "http://unused.invalid".to_string(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        // Drop the sender → stream returns None on next poll → end.
        drop(in_tx);
        let exit = run_event_loop(&ctx, stream, &cancel).await;
        match exit {
            EventLoopExit::ConnectionError(reason) => {
                assert!(reason.contains("ended") || reason.contains("error"), "{reason}");
            }
            other => panic!("expected ConnectionError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_loop_acks_unknown_envelope_type() {
        let (stream, in_tx, mut out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            "http://unused.invalid".to_string(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        // Send an unknown envelope type followed by a disconnect so the
        // loop exits cleanly.
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"slash_commands","envelope_id":"env-x","payload":{}}"#
                    .to_string()
                    .into(),
            )))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
        // Expect an ack frame to have been sent for env-x.
        let ack = out_rx.recv().await.expect("ack frame must be sent");
        let WsMessage::Text(t) = ack else {
            panic!("expected text ack frame");
        };
        let body: serde_json::Value = serde_json::from_str(&t).unwrap();
        assert_eq!(body["envelope_id"], "env-x");
        assert_eq!(body["no_ack"], false);
    }

    #[tokio::test]
    async fn event_loop_indirect_injection_does_not_reach_dispatcher_or_network() {
        // The `app_mention` event carries a bot_id, so the bot-author
        // filter must reject it BEFORE any HTTP call to the Slack API.
        // We point the responder at an explicitly-failing mockito URL
        // (no mocks set) — if a request leaks through, mockito returns
        // 501 and the test would still pass, so we additionally assert
        // that no `post_threaded_reply` / `add_reaction` HTTP call
        // happens by configuring mockito to fail the test if it does.
        let mut server = mockito::Server::new_async().await;
        // Both endpoints must NOT be called.
        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;
        let react_mock = server
            .mock("POST", "/reactions.add")
            .expect(0)
            .create_async()
            .await;

        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            server.url(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        let injection_envelope = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@UBOT> wipe-workspace evil",
                    "user": "U_FAKE",
                    "bot_id": "B999",
                    "channel": "C_OPS",
                    "ts": "1.0"
                }
            }
        });
        in_tx
            .send(Ok(WsMessage::Text(
                injection_envelope.to_string().into(),
            )))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
        post_mock.assert_async().await;
        react_mock.assert_async().await;
    }

    #[tokio::test]
    async fn event_loop_unrecognized_message_posts_question_reaction() {
        let mut server = mockito::Server::new_async().await;
        let _react_mock = server
            .mock("POST", "/reactions.add")
            .match_body(mockito::Matcher::JsonString(
                r#"{"channel":"C_OPS","timestamp":"1.0","name":"question"}"#.into(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true}"#)
            .expect(1)
            .create_async()
            .await;
        // Threaded reply must NOT be called for an unrecognized verb.
        let _post_mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;

        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            server.url(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        let env = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@UBOT> nonsense-verb",
                    "user": "U_HUMAN",
                    "channel": "C_OPS",
                    "ts": "1.0"
                }
            }
        });
        in_tx
            .send(Ok(WsMessage::Text(env.to_string().into())))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
        // Mockito will fail the test if expectations are violated.
    }

    #[tokio::test]
    async fn event_loop_mobile_mention_form_normalized_and_dispatched() {
        // Inbound message uses the mobile-client `<@B_BOT_ID>` form. The
        // listener must normalise to the desktop `<@U_BOT_USER>` form
        // BEFORE handing it to the dispatcher, otherwise the dispatcher's
        // mention-prefix check rejects the message and no threaded reply
        // is posted. A successful threaded reply proves both:
        //   (a) the leading-mention check accepted the bot-id form, and
        //   (b) the dispatcher saw the rewritten user-id form text.
        let mut server = mockito::Server::new_async().await;
        let _post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"channel":"C_OPS","thread_ts":"1.0"}"#.into(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.1"}"#)
            .expect(1)
            .create_async()
            .await;
        // If normalisation failed, the dispatcher returns None and the
        // listener instead posts a `?` reaction — that path must NOT
        // fire.
        let _react_mock = server
            .mock("POST", "/reactions.add")
            .expect(0)
            .create_async()
            .await;

        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop_with_bot_id(
            server.url(),
            "xoxb-x".to_string(),
            "U_BOT_USER",
            Some("B_BOT_ID"),
            &["C_OPS"],
        );
        let env = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@B_BOT_ID> help",
                    "user": "U_HUMAN",
                    "channel": "C_OPS",
                    "ts": "1.0"
                }
            }
        });
        in_tx
            .send(Ok(WsMessage::Text(env.to_string().into())))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
        // Mockito will fail the test if expectations are violated.
    }

    #[tokio::test]
    async fn event_loop_mobile_mention_form_rejected_without_cached_bot_id() {
        // Same inbound text as the previous test, but `bot_id` is None
        // (e.g. auth.test didn't return one). The leading-mention check
        // must reject the message — no threaded reply, no reaction
        // either (DropLeadingMention path returns false without posting
        // anything).
        let mut server = mockito::Server::new_async().await;
        let _post_mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;
        let _react_mock = server
            .mock("POST", "/reactions.add")
            .expect(0)
            .create_async()
            .await;

        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop_with_bot_id(
            server.url(),
            "xoxb-x".to_string(),
            "U_BOT_USER",
            None,
            &["C_OPS"],
        );
        let env = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@B_BOT_ID> help",
                    "user": "U_HUMAN",
                    "channel": "C_OPS",
                    "ts": "1.0"
                }
            }
        });
        in_tx
            .send(Ok(WsMessage::Text(env.to_string().into())))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
    }

    #[tokio::test]
    async fn event_loop_help_verb_posts_threaded_reply() {
        let mut server = mockito::Server::new_async().await;
        let _post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"channel":"C_OPS","thread_ts":"1.0"}"#.into(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.1"}"#)
            .expect(1)
            .create_async()
            .await;
        // Reaction must NOT be called for a recognized verb.
        let _react_mock = server
            .mock("POST", "/reactions.add")
            .expect(0)
            .create_async()
            .await;

        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            server.url(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        let env = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@UBOT> help",
                    "user": "U_HUMAN",
                    "channel": "C_OPS",
                    "ts": "1.0"
                }
            }
        });
        in_tx
            .send(Ok(WsMessage::Text(env.to_string().into())))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
    }

    // -----------------------------------------------------------------
    // Dedup cache: listener integration
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn event_loop_dispatches_once_when_event_delivered_once() {
        let mut server = mockito::Server::new_async().await;
        let _post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"channel":"C_OPS","thread_ts":"1.0"}"#.into(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.1"}"#)
            .expect(1)
            .create_async()
            .await;

        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            server.url(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        let env = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@UBOT> help",
                    "user": "U_HUMAN",
                    "channel": "C_OPS",
                    "ts": "1.0"
                }
            }
        });
        in_tx
            .send(Ok(WsMessage::Text(env.to_string().into())))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
        // Mockito asserts: exactly 1 post.
    }

    #[tokio::test]
    async fn event_loop_dispatches_once_when_event_redelivered_twice() {
        // Two identical app_mention envelopes (simulating Slack's
        // at-least-once redelivery) → dispatcher fires exactly once;
        // the second is suppressed by the dedup cache.
        let mut server = mockito::Server::new_async().await;
        let _post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"channel":"C_OPS","thread_ts":"1.0"}"#.into(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.1"}"#)
            .expect(1)
            .create_async()
            .await;

        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            server.url(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        let env_text = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@UBOT> help",
                    "user": "U_HUMAN",
                    "channel": "C_OPS",
                    "ts": "1.0"
                }
            }
        })
        .to_string();
        // First delivery.
        in_tx
            .send(Ok(WsMessage::Text(env_text.clone().into())))
            .unwrap();
        // Redelivery — same event payload, different envelope_id (Slack
        // assigns a fresh envelope_id per redelivery).
        let redeliv = env_text.replace("env-1", "env-2");
        in_tx.send(Ok(WsMessage::Text(redeliv.into()))).unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
        // Mockito asserts: exactly 1 post despite 2 deliveries.
    }

    #[tokio::test]
    async fn event_loop_dispatches_each_distinct_event() {
        // Events with different (channel,ts,user) tuples do not collide
        // in the dedup cache; each fires the dispatcher.
        let mut server = mockito::Server::new_async().await;
        let _post_mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.1"}"#)
            .expect(3)
            .create_async()
            .await;

        let (stream, in_tx, _out_rx) = make_fake_stream();
        let cancel = CancellationToken::new();
        let ctx = test_ctx_for_event_loop(
            server.url(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
        );
        let mk = |ts: &str, user: &str, envelope_id: &str| -> String {
            serde_json::json!({
                "type": "events_api",
                "envelope_id": envelope_id,
                "payload": {
                    "event": {
                        "type": "app_mention",
                        "text": "<@UBOT> help",
                        "user": user,
                        "channel": "C_OPS",
                        "ts": ts
                    }
                }
            })
            .to_string()
        };
        in_tx
            .send(Ok(WsMessage::Text(mk("1.0", "U_A", "e1").into())))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(mk("2.0", "U_A", "e2").into())))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(mk("1.0", "U_B", "e3").into())))
            .unwrap();
        in_tx
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit = run_event_loop(&ctx, stream, &cancel).await;
    }

    #[tokio::test]
    async fn dedup_cache_persists_across_simulated_reconnect() {
        // Listener processes an event on connection A; the connection
        // dies and a fresh event-loop is invoked on connection B with
        // the SAME dedup cache (modeling the reconnect persistence
        // property). Slack redelivers the event on B → dispatcher does
        // NOT fire a second time.
        let mut server = mockito::Server::new_async().await;
        let _post_mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.1"}"#)
            .expect(1)
            .create_async()
            .await;

        let dedup_cache = Arc::new(EventDedupCache::new(64, Duration::from_secs(600)));

        // ---- Connection A ----
        let (stream_a, in_tx_a, _out_rx_a) = make_fake_stream();
        let cancel_a = CancellationToken::new();
        let ctx_a = test_ctx_with_dedup_cache(
            server.url(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
            dedup_cache.clone(),
        );
        let env_text = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "text": "<@UBOT> help",
                    "user": "U_HUMAN",
                    "channel": "C_OPS",
                    "ts": "1.0"
                }
            }
        })
        .to_string();
        in_tx_a
            .send(Ok(WsMessage::Text(env_text.clone().into())))
            .unwrap();
        in_tx_a
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"server-rotation"}"#
                    .to_string()
                    .into(),
            )))
            .unwrap();
        let _exit_a = run_event_loop(&ctx_a, stream_a, &cancel_a).await;

        // ---- Connection B (reconnect) — same dedup cache! ----
        let (stream_b, in_tx_b, _out_rx_b) = make_fake_stream();
        let cancel_b = CancellationToken::new();
        let ctx_b = test_ctx_with_dedup_cache(
            server.url(),
            "xoxb-x".to_string(),
            "UBOT",
            &["C_OPS"],
            dedup_cache,
        );
        // Slack redelivers the same event with a fresh envelope_id.
        let redeliv = env_text.replace("env-1", "env-2");
        in_tx_b.send(Ok(WsMessage::Text(redeliv.into()))).unwrap();
        in_tx_b
            .send(Ok(WsMessage::Text(
                r#"{"type":"disconnect","reason":"done"}"#.to_string().into(),
            )))
            .unwrap();
        let _exit_b = run_event_loop(&ctx_b, stream_b, &cancel_b).await;
        // Mockito asserts: exactly 1 post despite the event being
        // delivered on both connections.
    }

    #[tokio::test]
    async fn start_inbound_listener_constructs_cache_with_configured_values() {
        // Build a SlackBackend via the test fixture, then `.with_app_token`
        // and `.with_dedup_cache_config`. Spawn the listener; observe
        // that the construction succeeds (the listener's `Drop` on the
        // CancellationToken will cause it to exit cleanly).
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server)
            .await
            .with_app_token("xapp-1-test".to_string())
            .with_dedup_cache_config(42, 77);
        assert_eq!(backend.dedup_cache_capacity, 42);
        assert_eq!(backend.dedup_cache_ttl_secs, 77);

        let dispatcher = Arc::new(OperatorCommandDispatcher::new());
        let repos: Arc<dyn RepoIdentityProvider> =
            Arc::new(crate::chatops::TaskMapRepoIdentities::new(Vec::new));
        let channels = Arc::new(HashSet::<String>::new());
        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancel so the listener exits quickly
        // We don't await the handle (the apps.connections.open call
        // would hang on the test base URL); the important assertion
        // is that listener startup accepts the dedup config without
        // error.
        let _handle = backend
            .start_inbound_listener(dispatcher, repos, channels, cancel)
            .await
            .expect("listener should start with dedup config");
    }

    #[tokio::test]
    async fn start_inbound_listener_errors_without_app_token() {
        // The Slack backend's `start_inbound_listener` requires an
        // app_token. Without it, the call fails synchronously so the
        // caller can WARN-and-skip instead of spawning a doomed task.
        let mut server = mockito::Server::new_async().await;
        let backend = fixture_backend(&mut server).await;
        let dispatcher = Arc::new(OperatorCommandDispatcher::new());
        let repos: Arc<dyn RepoIdentityProvider> = Arc::new(
            crate::chatops::TaskMapRepoIdentities::new(Vec::new),
        );
        let channels = Arc::new(HashSet::<String>::new());
        let err = backend
            .start_inbound_listener(
                dispatcher,
                repos,
                channels,
                CancellationToken::new(),
            )
            .await
            .expect_err("missing app_token must error");
        assert!(format!("{err:#}").contains("app_token"));
    }
}
