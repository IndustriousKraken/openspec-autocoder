//! ChatOps backend abstraction. Slack is the officially-supported backend;
//! Discord, Teams, Mattermost, and Matrix are experimental, best-effort
//! implementations selected via `chatops.provider:` in the config.
//!
//! Provider-specific HTTP code lives in the per-backend submodules; this
//! module owns the trait, the startup factory, and the provider-agnostic
//! state-file helpers (`.question.json` / `.answer.json` lifecycle).

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::chatops::operator_commands::{
    OperatorCommandDispatcher, RepoIdentity, RepoIdentityProvider,
};
use crate::config::{ChatOpsConfig, ChatOpsProvider, RepositoryConfig, SecretSource};

pub mod discord;
pub mod matrix;
pub mod mattermost;
pub mod operator_commands;
pub mod slack;
pub mod teams;

pub use discord::DiscordBackend;
pub use matrix::MatrixBackend;
pub use mattermost::MattermostBackend;
pub use slack::SlackBackend;
pub use teams::TeamsBackend;

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

/// Backend abstraction the polling loop holds. Each implementation talks
/// to a different chat provider (Slack, Discord, Teams, Mattermost, Matrix).
#[async_trait]
pub trait ChatOpsBackend: Send + Sync {
    /// Stable name used in logs and the experimental-warning line.
    fn provider_name(&self) -> &'static str;

    /// Whether this backend is experimental. Only Slack returns `false`.
    fn is_experimental(&self) -> bool;

    /// Post `question` to `channel` and return the opaque thread handle
    /// (provider-specific format) that subsequent reply-polls reference.
    async fn post_question(
        &self,
        channel: &str,
        change: &str,
        question: &str,
    ) -> Result<String>;

    /// Poll for the earliest reply in the thread identified by `handle`
    /// (the value previously returned from `post_question`). The reply
    /// MUST NOT be the bot's own message.
    async fn poll_thread_for_human_reply(
        &self,
        channel: &str,
        handle: &str,
    ) -> Result<Option<HumanReply>>;

    /// Post a plain-text notification (no question prefix, no reply
    /// threading). Used for start-of-work and throttled failure alerts.
    async fn post_notification(&self, channel: &str, text: &str) -> Result<()>;

    /// Post `text` as a threaded reply to the message at `(channel,
    /// thread_ts)`. Used by the inbound operator-commands listener so
    /// `Sync` replies stay grouped under the original `@bot <verb>`
    /// message rather than spilling into channel-level chatter.
    ///
    /// Default impl returns an error whose text names the provider —
    /// only backends that override `start_inbound_listener` need to
    /// implement this.
    #[allow(dead_code)] // trait method called only via inbound-listener override
    async fn post_threaded_reply(
        &self,
        _channel: &str,
        _thread_ts: &str,
        _text: &str,
    ) -> Result<()> {
        Err(anyhow!(
            "backend `{}` does not support threaded replies",
            self.provider_name()
        ))
    }

    /// Add the reaction emoji `name` (without surrounding colons) to
    /// the message at `(channel, message_ts)`. Used by the inbound
    /// operator-commands listener to react with `?` on unrecognized
    /// messages instead of posting a text reply.
    ///
    /// Default impl returns an error whose text names the provider.
    #[allow(dead_code)] // trait method called only via inbound-listener override
    async fn add_reaction(
        &self,
        _channel: &str,
        _message_ts: &str,
        _name: &str,
    ) -> Result<()> {
        Err(anyhow!(
            "backend `{}` does not support reactions",
            self.provider_name()
        ))
    }

    /// Spawn the inbound listener task for this backend. The default
    /// implementation returns an error naming the provider so backends
    /// that have not opted in to inbound listening continue to compile
    /// and run unchanged.
    ///
    /// The listener:
    ///   - subscribes to provider-native mention events
    ///   - applies the drop-before-dispatch filters (channel allowlist,
    ///     self-author, bot-author, leading-mention)
    ///   - calls `dispatcher.handle_message` on every surviving event
    ///   - routes the returned `Option<Reply>` through
    ///     `post_threaded_reply` (text replies) or `add_reaction`
    ///     (unrecognized → `?`)
    ///   - exits when `cancel` fires (or earlier on unrecoverable error)
    async fn start_inbound_listener(
        &self,
        _dispatcher: Arc<OperatorCommandDispatcher>,
        _repos: Arc<dyn RepoIdentityProvider>,
        _allowed_channels: Arc<HashSet<String>>,
        _cancel: CancellationToken,
    ) -> Result<JoinHandle<()>> {
        Err(anyhow!(
            "backend `{}` does not support inbound messages",
            self.provider_name()
        ))
    }
}

/// `RepoIdentityProvider` newtype over the daemon's per-repo
/// `Arc<ArcSwap<RepositoryConfig>>` map. The projection from
/// `RepositoryConfig` to `RepoIdentity` (URL + workspace_path only)
/// lives inside `snapshot()` so the inbound listener — and any code
/// the listener calls — can never observe tokens, channel IDs, or
/// any other config field. Any future field added to
/// `RepositoryConfig` is not automatically widened into the listener's
/// view.
pub struct TaskMapRepoIdentities {
    inner: Arc<dyn Fn() -> Vec<RepositoryConfig> + Send + Sync>,
}

impl TaskMapRepoIdentities {
    /// Construct from a closure that returns the current list of
    /// `RepositoryConfig` values. The closure is invoked once per
    /// `snapshot()` call (i.e. once per inbound command), so it should
    /// be cheap — typically an `ArcSwap` load.
    pub fn new<F>(snapshotter: F) -> Self
    where
        F: Fn() -> Vec<RepositoryConfig> + Send + Sync + 'static,
    {
        Self {
            inner: Arc::new(snapshotter),
        }
    }
}

impl RepoIdentityProvider for TaskMapRepoIdentities {
    fn snapshot(&self) -> Vec<RepoIdentity> {
        (self.inner)()
            .iter()
            .map(|r| RepoIdentity {
                url: r.url.clone(),
                workspace_path: crate::workspace::resolve_path(r),
            })
            .collect()
    }
}

/// Resolve a secret from an inline value or an env var name, honoring the
/// "inline takes precedence over env var" rule and warning when both are
/// set. Returns the secret string.
fn resolve_inline_or_env(
    inline: Option<&SecretSource>,
    env_name: Option<&str>,
    field_label: &str,
) -> Result<String> {
    match (inline, env_name) {
        (Some(src), env_opt) => {
            let resolved = src.resolve(field_label)?;
            if src.is_inline() {
                if let Some(name) = env_opt {
                    if std::env::var(name).is_ok() {
                        tracing::warn!(
                            "{field_label} (inline) takes precedence; env var `{name}` is being ignored"
                        );
                    }
                }
            }
            Ok(resolved)
        }
        (None, Some(name)) => SecretSource::EnvVar(name.to_string())
            .resolve(&format!("{field_label}_env={name}")),
        (None, None) => Err(anyhow!(
            "{field_label}: neither inline value nor env var name is set"
        )),
    }
}

/// Build a `ChatOpsBackend` from config. Dispatches on `cfg.provider`
/// and returns an error whose text names both the selected provider and
/// the missing sub-block when the matching one is absent.
pub async fn from_config(cfg: &ChatOpsConfig) -> Result<Arc<dyn ChatOpsBackend>> {
    match cfg.provider {
        ChatOpsProvider::Slack => {
            let sub = cfg
                .slack
                .as_ref()
                .ok_or_else(|| anyhow!("chatops.provider is `slack` but `chatops.slack:` sub-block is missing"))?;
            let token = resolve_inline_or_env(
                sub.bot_token.as_ref(),
                sub.bot_token_env.as_deref(),
                "chatops.slack.bot_token",
            )?;
            let app_token = if sub.app_token.is_some() || sub.app_token_env.is_some() {
                Some(resolve_inline_or_env(
                    sub.app_token.as_ref(),
                    sub.app_token_env.as_deref(),
                    "chatops.slack.app_token",
                )?)
            } else {
                None
            };
            crate::config::warn_on_unexpected_slack_token_prefixes(
                Some(token.as_str()),
                app_token.as_deref(),
            );
            let mut backend = SlackBackend::new(token)
                .await
                .context("initializing Slack backend from config")?;
            if let Some(at) = app_token {
                backend = backend.with_app_token(at);
            }
            Ok(Arc::new(backend))
        }
        ChatOpsProvider::Discord => {
            let sub = cfg
                .discord
                .as_ref()
                .ok_or_else(|| anyhow!("chatops.provider is `discord` but `chatops.discord:` sub-block is missing"))?;
            let token = SecretSource::EnvVar(sub.bot_token_env.clone())
                .resolve(&format!(
                    "chatops.discord.bot_token_env={}",
                    sub.bot_token_env
                ))?;
            let backend = DiscordBackend::new(token)
                .await
                .context("initializing Discord backend from config")?;
            Ok(Arc::new(backend))
        }
        ChatOpsProvider::Teams => {
            let sub = cfg
                .teams
                .as_ref()
                .ok_or_else(|| anyhow!("chatops.provider is `teams` but `chatops.teams:` sub-block is missing"))?;
            let secret = SecretSource::EnvVar(sub.client_secret_env.clone())
                .resolve(&format!(
                    "chatops.teams.client_secret_env={}",
                    sub.client_secret_env
                ))?;
            let backend = TeamsBackend::new(
                sub.tenant_id.clone(),
                sub.client_id.clone(),
                secret,
                sub.team_id.clone(),
            )
            .await
            .context("initializing Teams backend from config")?;
            Ok(Arc::new(backend))
        }
        ChatOpsProvider::Mattermost => {
            let sub = cfg
                .mattermost
                .as_ref()
                .ok_or_else(|| anyhow!("chatops.provider is `mattermost` but `chatops.mattermost:` sub-block is missing"))?;
            let token = SecretSource::EnvVar(sub.access_token_env.clone())
                .resolve(&format!(
                    "chatops.mattermost.access_token_env={}",
                    sub.access_token_env
                ))?;
            let backend = MattermostBackend::new(sub.server_url.clone(), token)
                .await
                .context("initializing Mattermost backend from config")?;
            Ok(Arc::new(backend))
        }
        ChatOpsProvider::Matrix => {
            let sub = cfg
                .matrix
                .as_ref()
                .ok_or_else(|| anyhow!("chatops.provider is `matrix` but `chatops.matrix:` sub-block is missing"))?;
            let token = SecretSource::EnvVar(sub.access_token_env.clone())
                .resolve(&format!(
                    "chatops.matrix.access_token_env={}",
                    sub.access_token_env
                ))?;
            let backend = MatrixBackend::new(sub.homeserver_url.clone(), token)
                .await
                .context("initializing Matrix backend from config")?;
            Ok(Arc::new(backend))
        }
    }
}

/// Minimal URL-encoder for query params and path segments. Encodes
/// everything outside the unreserved set per RFC 3986. Shared across
/// Slack (channel/ts params), Matrix (event_id with `$`/`:` characters),
/// and any other backend that needs URL-encoded values.
pub(crate) fn urlencode(s: &str) -> String {
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

        let entries: Vec<_> = std::fs::read_dir(
            ws.join("openspec/changes/feature-x"),
        )
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
        assert!(
            !entries.iter().any(|n| n.contains(".tmp")),
            "no `.tmp` files should leak: {entries:?}"
        );
    }

    #[test]
    fn deletes_are_idempotent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "feature-y");

        delete_question_file(ws, "feature-y").unwrap();
        delete_answer_file(ws, "feature-y").unwrap();

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
        delete_question_file(ws, "feature-y").unwrap();
    }

    #[test]
    fn urlencode_preserves_unreserved_and_encodes_specials() {
        assert_eq!(urlencode("abc_DEF-1.2~"), "abc_DEF-1.2~");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("$abc:server.tld"), "%24abc%3Aserver.tld");
    }

    // ====================================================================
    // ChatOpsBackend default trait method tests
    // ====================================================================

    use std::collections::HashSet;
    use std::sync::Arc as StdArc;

    /// A minimal `ChatOpsBackend` impl that does not override any of
    /// the inbound-listener / threaded-reply / reaction methods. Used
    /// to assert the default impls return an `unsupported` error whose
    /// text contains the provider name.
    struct StubBackend(&'static str);

    #[async_trait]
    impl ChatOpsBackend for StubBackend {
        fn provider_name(&self) -> &'static str {
            self.0
        }
        fn is_experimental(&self) -> bool {
            true
        }
        async fn post_question(
            &self,
            _channel: &str,
            _change: &str,
            _question: &str,
        ) -> Result<String> {
            unreachable!()
        }
        async fn poll_thread_for_human_reply(
            &self,
            _channel: &str,
            _handle: &str,
        ) -> Result<Option<HumanReply>> {
            unreachable!()
        }
        async fn post_notification(&self, _channel: &str, _text: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn default_inbound_listener_errors_with_provider_name() {
        let backend = StubBackend("stubbo");
        let dispatcher = StdArc::new(OperatorCommandDispatcher::new());
        let repos: StdArc<dyn RepoIdentityProvider> =
            StdArc::new(TaskMapRepoIdentities::new(Vec::new));
        let channels = StdArc::new(HashSet::<String>::new());
        let err = backend
            .start_inbound_listener(
                dispatcher,
                repos,
                channels,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect_err("stub backend has no inbound listener");
        let msg = format!("{err:#}");
        assert!(msg.contains("stubbo"), "error names backend: {msg}");
        assert!(msg.to_lowercase().contains("unsupported") || msg.contains("not support"), "{msg}");
    }

    #[tokio::test]
    async fn default_post_threaded_reply_errors_with_provider_name() {
        let backend = StubBackend("stubbo");
        let err = backend
            .post_threaded_reply("C", "1.0", "x")
            .await
            .expect_err("default impl must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("stubbo"), "{msg}");
    }

    #[tokio::test]
    async fn default_add_reaction_errors_with_provider_name() {
        let backend = StubBackend("stubbo");
        let err = backend
            .add_reaction("C", "1.0", "question")
            .await
            .expect_err("default impl must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("stubbo"), "{msg}");
    }

    // ====================================================================
    // RepoIdentityProvider projection tests
    // ====================================================================

    fn fixture_repo_config(url: &str) -> RepositoryConfig {
        RepositoryConfig {
            url: url.to_string(),
            local_path: Some(PathBuf::from("/tmp/ws/fixture")),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: Some("C_SECRET_NEVER_VISIBLE".into()),
            max_changes_per_pr: None,
            audits: None,
        }
    }

    #[test]
    fn repo_identity_provider_only_exposes_url_and_workspace_path() {
        let provider = TaskMapRepoIdentities::new(|| {
            vec![
                fixture_repo_config("git@github.com:acme/a.git"),
                fixture_repo_config("git@github.com:acme/b.git"),
            ]
        });
        let snapshot: Vec<RepoIdentity> = provider.snapshot();
        assert_eq!(snapshot.len(), 2);

        // Compile-time guarantee: the type is RepoIdentity, NOT
        // RepositoryConfig. We can only read `url` and
        // `workspace_path`. The `chatops_channel_id`, `poll_interval_sec`,
        // and `audits` fields from RepositoryConfig do not exist on
        // RepoIdentity. The two-field shape is enforced by the type
        // system, so this assertion is partly redundant — but it also
        // catches a future regression where someone widens RepoIdentity
        // and writes the chatops_channel_id into it.
        assert_eq!(snapshot[0].url, "git@github.com:acme/a.git");
        assert_eq!(snapshot[0].workspace_path, PathBuf::from("/tmp/ws/fixture"));
        assert_eq!(snapshot[1].url, "git@github.com:acme/b.git");
    }

    #[test]
    fn repo_identity_provider_reflects_underlying_state_changes() {
        // The closure is invoked once per snapshot, so updating shared
        // state changes the result. This models the production case
        // where the daemon's `ArcSwap` holds the latest config.
        use std::sync::Mutex as StdMutex;
        let state: StdArc<StdMutex<Vec<RepositoryConfig>>> =
            StdArc::new(StdMutex::new(vec![fixture_repo_config(
                "git@github.com:acme/a.git",
            )]));
        let state_for_closure = state.clone();
        let provider = TaskMapRepoIdentities::new(move || {
            state_for_closure.lock().unwrap().clone()
        });
        assert_eq!(provider.snapshot().len(), 1);

        state.lock().unwrap().push(fixture_repo_config("git@github.com:acme/b.git"));
        assert_eq!(provider.snapshot().len(), 2);
    }
}
