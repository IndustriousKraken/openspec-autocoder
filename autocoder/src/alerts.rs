//! Predictable-failure alert routing. Owns the 24h-per-(repo, category)
//! throttle plus the message formatting. See
//! `openspec/changes/chatops-progress-notifications/design.md` for the
//! algorithm.

use crate::alert_state::{AlertCategory, AlertEntry, AlertState};
use crate::polling_loop::ChatOpsContext;
use crate::recovery_classification::RecoveryFailureClass;
use chrono::{Duration as ChronoDuration, Utc};
use std::path::Path;

const ERROR_EXCERPT_MAX_CHARS: usize = 200;
const ALERT_THROTTLE_HOURS: i64 = 24;

/// Truncate `format!("{err:#}")` to at most `ERROR_EXCERPT_MAX_CHARS`
/// characters, appending an ellipsis when shortened.
pub(crate) fn excerpt(err: &anyhow::Error) -> String {
    excerpt_str(&format!("{err:#}"))
}

fn excerpt_str(s: &str) -> String {
    let count = s.chars().count();
    if count <= ERROR_EXCERPT_MAX_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(ERROR_EXCERPT_MAX_CHARS).collect();
        out.push('…');
        out
    }
}

/// Format the alert text as `⚠️ <repo>: <label>[<class-suffix>]. Latest:
/// <excerpt>`. The 24h throttle that governs how often this category
/// re-posts is an implementation detail of `handle_predictable_failure` —
/// earlier wordings claimed "for the past 24h" inside the alert body,
/// which operators read as a duration measurement rather than a throttle
/// window. The duration claim is intentionally absent here. When `class`
/// is `Some(_)`, the suffix from [`RecoveryFailureClass::alert_suffix`]
/// is appended after the category label (pinned strings — operator-
/// visible AND referenced in `docs/CHATOPS.md`).
pub(crate) fn format_alert_text_with_class(
    repo_url: &str,
    category: AlertCategory,
    err: &anyhow::Error,
    class: Option<RecoveryFailureClass>,
) -> String {
    let suffix = class.map(|c| c.alert_suffix()).unwrap_or("");
    format!(
        "⚠️ `{repo_url}`: {label}{suffix}. Latest: {excerpt}",
        label = category.label(),
        excerpt = excerpt(err),
    )
}

/// Handle a predictable-failure site: load state, decide whether to alert,
/// post if the 24h window has elapsed (or this is the first occurrence), and
/// persist on post success only. A failed post deliberately does NOT update
/// the state so the next iteration retries.
///
/// Silent when notifications are disabled or no chatops backend is wired —
/// in both cases the function returns without reading or writing the state
/// file.
pub async fn handle_predictable_failure(
    workspace: &Path,
    repo_url: &str,
    chatops_ctx: Option<&ChatOpsContext>,
    notifications_enabled: bool,
    category: AlertCategory,
    err: &anyhow::Error,
) {
    handle_predictable_failure_inner(
        workspace,
        repo_url,
        chatops_ctx,
        notifications_enabled,
        category,
        err,
        None,
    )
    .await
}

/// Variant of [`handle_predictable_failure`] that records a mid-iteration
/// recovery classification (transient vs. permanent). The class is
/// rendered as a suffix on the alert text per
/// [`RecoveryFailureClass::alert_suffix`]. The 24h throttle, persistence
/// path, and silent-on-disabled behavior are identical to the un-classed
/// helper.
pub async fn handle_classified_recovery_failure(
    workspace: &Path,
    repo_url: &str,
    chatops_ctx: Option<&ChatOpsContext>,
    notifications_enabled: bool,
    category: AlertCategory,
    err: &anyhow::Error,
    class: RecoveryFailureClass,
) {
    handle_predictable_failure_inner(
        workspace,
        repo_url,
        chatops_ctx,
        notifications_enabled,
        category,
        err,
        Some(class),
    )
    .await
}

async fn handle_predictable_failure_inner(
    workspace: &Path,
    repo_url: &str,
    chatops_ctx: Option<&ChatOpsContext>,
    notifications_enabled: bool,
    category: AlertCategory,
    err: &anyhow::Error,
    class: Option<RecoveryFailureClass>,
) {
    if !notifications_enabled {
        return;
    }
    let Some(ctx) = chatops_ctx else { return };

    let mut state = AlertState::load_or_default(workspace);
    let now = Utc::now();
    let should_alert = state
        .alerts
        .get(&category)
        .map(|entry| now - entry.last_alerted_at >= ChronoDuration::hours(ALERT_THROTTLE_HOURS))
        .unwrap_or(true);
    if !should_alert {
        return;
    }

    let text = format_alert_text_with_class(repo_url, category, err, class);
    if let Err(post_err) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        // Per design.md: chatops failures are never re-routed through
        // chatops, and the timestamp is NOT updated so the next iteration
        // can re-attempt the alert immediately.
        tracing::error!(
            url = %repo_url,
            category = ?category,
            alert_text = %text,
            "chatops alert post failed; not retrying through chatops: {post_err:#}"
        );
        return;
    }

    state.alerts.insert(
        category,
        AlertEntry {
            last_alerted_at: now,
            last_error_excerpt: excerpt(err),
        },
    );
    if let Err(save_err) = state.save(workspace) {
        // Best-effort: if we can't persist the timestamp the worst case is
        // the next iteration re-alerts. Log so the operator notices.
        tracing::warn!(
            url = %repo_url,
            category = ?category,
            "failed to persist alert state after posting alert: {save_err:#}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatops::{ChatOpsBackend, SlackBackend};
    use anyhow::anyhow;
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn fixture_chatops(server: &mut mockito::Server) -> Arc<dyn ChatOpsBackend> {
        let _auth = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
            .create_async()
            .await;
        Arc::new(
            SlackBackend::new_at(server.url(), "xoxb-fixture".into())
                .await
                .unwrap(),
        )
    }

    fn make_ctx(chatops: Arc<dyn ChatOpsBackend>) -> ChatOpsContext {
        ChatOpsContext {
            chatops,
            channel: "C_FIXTURE".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        }
    }

    #[test]
    fn excerpt_truncates_long_strings_with_ellipsis() {
        let long: String = "x".repeat(500);
        let trimmed = excerpt_str(&long);
        let count = trimmed.chars().count();
        assert_eq!(count, ERROR_EXCERPT_MAX_CHARS + 1, "max chars + ellipsis");
        assert!(trimmed.ends_with('…'));
    }

    #[test]
    fn excerpt_passes_short_strings_through() {
        let s = "short error";
        assert_eq!(excerpt_str(s), s);
    }

    #[test]
    fn format_alert_text_contains_repo_label_and_excerpt() {
        let err = anyhow!("server hangup");
        let text = format_alert_text_with_class(
            "git@github.com:owner/repo.git",
            AlertCategory::BranchPushFailure,
            &err,
            None,
        );
        assert!(text.contains("git@github.com:owner/repo.git"));
        assert!(text.contains("branch push keeps failing"));
        assert!(text.contains("server hangup"));
        assert!(text.starts_with("⚠️"));
    }

    /// Regression: an earlier version of the format included "for the
    /// past 24h" as a hardcoded duration claim, which operators read as
    /// a measurement rather than as the throttle window. The phrase is
    /// intentionally absent now — pin it so no future change
    /// reintroduces the misleading wording.
    #[test]
    fn format_alert_text_does_not_claim_a_duration() {
        let err = anyhow!("anything");
        let text = format_alert_text_with_class(
            "git@github.com:owner/repo.git",
            AlertCategory::WorkspaceDirtyMidIteration,
            &err,
            None,
        );
        assert!(
            !text.contains("for the past 24h"),
            "alert text must not claim a duration measurement; got: {text}"
        );
        assert!(
            !text.contains("past 24h"),
            "alert text must not claim a duration measurement; got: {text}"
        );
    }

    #[test]
    fn format_alert_text_with_transient_class_includes_retrying_suffix() {
        let err = anyhow!("Could not resolve host github.com");
        let text = format_alert_text_with_class(
            "git@github.com:owner/repo.git",
            AlertCategory::WorkspaceInitFailure,
            &err,
            Some(RecoveryFailureClass::Transient),
        );
        assert!(
            text.contains("workspace init keeps failing (transient; retrying). Latest:"),
            "alert must carry the transient-retrying suffix; got: {text}"
        );
        assert!(text.contains("Could not resolve host"));
    }

    #[test]
    fn format_alert_text_with_permanent_class_includes_operator_action_suffix() {
        let err = anyhow!("workspace still dirty after recovery");
        let text = format_alert_text_with_class(
            "git@github.com:owner/repo.git",
            AlertCategory::WorkspaceDirtyMidIteration,
            &err,
            Some(RecoveryFailureClass::Permanent),
        );
        assert!(
            text.contains(
                "workspace dirty mid-iteration (permanent; skipped until daemon restart) — operator inspection required. Latest:"
            ),
            "alert must carry the permanent-operator-action suffix; got: {text}"
        );
    }

    #[test]
    fn format_alert_text_workspace_dirty_mid_iteration() {
        let err = anyhow!("workspace /tmp/x is dirty before pass: D foo/bar");
        let text = format_alert_text_with_class(
            "git@github.com:owner/repo.git",
            AlertCategory::WorkspaceDirtyMidIteration,
            &err,
            None,
        );
        assert!(text.contains("git@github.com:owner/repo.git"));
        assert!(text.contains("workspace dirty mid-iteration"));
        assert!(text.contains("D foo/bar"));
    }

    #[tokio::test]
    async fn first_failure_posts_and_saves_state() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);

        handle_predictable_failure(
            ws,
            "git@github.com:owner/repo.git",
            Some(&ctx),
            true,
            AlertCategory::WorkspaceInitFailure,
            &anyhow!("clone failed: 403 Forbidden"),
        )
        .await;

        mock.assert_async().await;
        let state = AlertState::load_or_default(ws);
        assert!(
            state.alerts.contains_key(&AlertCategory::WorkspaceInitFailure),
            "state must persist the alerted-at timestamp after a successful post"
        );
        assert!(
            state.alerts[&AlertCategory::WorkspaceInitFailure]
                .last_error_excerpt
                .contains("403 Forbidden")
        );
    }

    #[tokio::test]
    async fn repeat_within_24h_is_silent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Pre-populate state as if we alerted 1 hour ago.
        let mut state = AlertState::default();
        state.alerts.insert(
            AlertCategory::BranchPushFailure,
            AlertEntry {
                last_alerted_at: Utc::now() - ChronoDuration::hours(1),
                last_error_excerpt: "prior".into(),
            },
        );
        state.save(ws).unwrap();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0) // MUST NOT POST
            .create_async()
            .await;
        let ctx = make_ctx(chatops);

        handle_predictable_failure(
            ws,
            "git@github.com:owner/repo.git",
            Some(&ctx),
            true,
            AlertCategory::BranchPushFailure,
            &anyhow!("push rejected"),
        )
        .await;

        mock.assert_async().await;
        // State must be unchanged.
        let state_after = AlertState::load_or_default(ws);
        assert_eq!(
            state_after.alerts[&AlertCategory::BranchPushFailure].last_error_excerpt,
            "prior"
        );
    }

    #[tokio::test]
    async fn beyond_24h_re_alerts_and_updates_state() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let old_time = Utc::now() - ChronoDuration::hours(25);
        let mut state = AlertState::default();
        state.alerts.insert(
            AlertCategory::PrCreationFailure,
            AlertEntry {
                last_alerted_at: old_time,
                last_error_excerpt: "stale-error".into(),
            },
        );
        state.save(ws).unwrap();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);

        handle_predictable_failure(
            ws,
            "git@github.com:owner/repo.git",
            Some(&ctx),
            true,
            AlertCategory::PrCreationFailure,
            &anyhow!("fresh failure 422"),
        )
        .await;

        mock.assert_async().await;
        let state_after = AlertState::load_or_default(ws);
        let entry = &state_after.alerts[&AlertCategory::PrCreationFailure];
        assert_ne!(entry.last_alerted_at, old_time, "timestamp must update");
        assert!(entry.last_error_excerpt.contains("fresh failure 422"));
    }

    #[tokio::test]
    async fn post_failure_does_not_update_state() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        // Slack returns ok:false → post errors.
        let _mock = server
            .mock("POST", "/chat.postMessage")
            .with_status(200)
            .with_body(r#"{"ok":false,"error":"channel_not_found"}"#)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);

        handle_predictable_failure(
            ws,
            "git@github.com:owner/repo.git",
            Some(&ctx),
            true,
            AlertCategory::WorkspaceInitFailure,
            &anyhow!("clone failure"),
        )
        .await;

        // State file must NOT be written when the post fails.
        assert!(
            !ws.join(".alert-state.json").exists(),
            "state file must not be written when chatops post fails"
        );
    }

    #[tokio::test]
    async fn disabled_skips_even_reading_state() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();

        // Make the workspace read-only-looking: place a state file we'd
        // expect to remain untouched (no read means no surprise behavior).
        let mut state = AlertState::default();
        state.alerts.insert(
            AlertCategory::WorkspaceInitFailure,
            AlertEntry {
                last_alerted_at: Utc::now() - ChronoDuration::hours(48),
                last_error_excerpt: "should-not-be-read".into(),
            },
        );
        state.save(ws).unwrap();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0) // disabled → no post
            .create_async()
            .await;
        let ctx = make_ctx(chatops);

        handle_predictable_failure(
            ws,
            "git@github.com:owner/repo.git",
            Some(&ctx),
            false, // disabled
            AlertCategory::WorkspaceInitFailure,
            &anyhow!("anything"),
        )
        .await;

        mock.assert_async().await;
        // State file untouched.
        let state_after = AlertState::load_or_default(ws);
        assert_eq!(
            state_after.alerts[&AlertCategory::WorkspaceInitFailure].last_error_excerpt,
            "should-not-be-read"
        );
    }
}
