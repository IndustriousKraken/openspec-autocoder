//! Test-only chatops backend used by the audit framework's integration
//! tests. Records every `post_notification` call in order so tests can
//! assert that the documented notification text fires (and that the
//! ordering between the audit and the scheduler matches the spec).
//!
//! Only compiled under `#[cfg(test)]`; never linked into the daemon.

use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use crate::chatops::{ChatOpsBackend, HumanReply};

/// A captured `post_notification` call, ordered as it was made.
#[derive(Debug, Clone)]
pub struct RecordedNotification {
    pub channel: String,
    pub text: String,
    /// `git rev-parse HEAD` evaluated at post time, when the backend
    /// was constructed with a workspace path. `None` otherwise.
    pub head_at_post: Option<String>,
}

/// In-memory chatops backend. Records calls in the order they were
/// made. When `workspace` is set, also snapshots `git rev-parse HEAD`
/// at post time so order-of-operations tests can assert the notification
/// fired BEFORE the audit's commit.
pub struct RecordingBackend {
    notifications: Mutex<Vec<RecordedNotification>>,
    fail_with: Option<&'static str>,
    workspace: Option<std::path::PathBuf>,
}

impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            notifications: Mutex::new(Vec::new()),
            fail_with: None,
            workspace: None,
        }
    }

    /// Construct a backend whose `post_notification` returns `Err(msg)`
    /// every time. Used to exercise the "notification failed → audit
    /// success unaffected" path.
    pub fn failing(msg: &'static str) -> Self {
        Self {
            notifications: Mutex::new(Vec::new()),
            fail_with: Some(msg),
            workspace: None,
        }
    }

    /// Snapshot `git rev-parse HEAD` in `workspace` on every
    /// `post_notification` call. Use this for order-of-operations
    /// tests where we need to prove the notification fired before
    /// the audit committed.
    pub fn with_workspace(mut self, workspace: std::path::PathBuf) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn calls(&self) -> Vec<RecordedNotification> {
        self.notifications.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatOpsBackend for RecordingBackend {
    fn provider_name(&self) -> &'static str {
        "recording"
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
        unreachable!("recording backend is post_notification-only")
    }
    async fn poll_thread_for_human_reply(
        &self,
        _channel: &str,
        _handle: &str,
    ) -> Result<Option<HumanReply>> {
        unreachable!("recording backend is post_notification-only")
    }
    async fn post_notification(&self, channel: &str, text: &str) -> Result<()> {
        if let Some(msg) = self.fail_with {
            return Err(anyhow::anyhow!("{msg}"));
        }
        let head = self.workspace.as_ref().and_then(|ws| {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(ws)
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                    } else {
                        None
                    }
                })
        });
        self.notifications.lock().unwrap().push(RecordedNotification {
            channel: channel.to_string(),
            text: text.to_string(),
            head_at_post: head,
        });
        Ok(())
    }
}

/// Build a `ChatOpsContext` whose backend is a freshly-constructed
/// `RecordingBackend`. Returns the context and a handle the test can
/// query for the captured calls.
pub fn make_recording_ctx(
    backend: std::sync::Arc<RecordingBackend>,
) -> crate::polling_loop::ChatOpsContext {
    crate::polling_loop::ChatOpsContext {
        chatops: backend,
        channel: "C_AUDIT_TEST".to_string(),
        start_work_enabled: true,
        failure_alerts_enabled: true,
        pr_opened_enabled: true,
    }
}
