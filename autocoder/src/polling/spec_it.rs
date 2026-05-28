//! Spec-it polling handler (a25). Drains ONE pending spec-it request
//! per iteration. For each request: load the referenced `ScoutRunState`,
//! resolve the named item, compute staleness signals, construct the
//! propose-request text, AND hand off to the canonical propose flow.
//!
//! Staleness is a WARN, not a block — the operator gets a thread reply
//! naming what changed, AND the propose-request still submits.

use crate::config::{RepositoryConfig, ScoutFeatureConfig};
use crate::control_socket::SpecItRequest;
use crate::polling_loop::ChatOpsContext;
use crate::proposal_requests::{
    self, ProposalRequestState, ProposalRequestStatus,
};
use crate::state::scout_run::{self, ScoutItem};
use crate::git;
use anyhow::Result;
use std::path::Path;

/// Process one drained spec-it request. Returns `Ok(())` on every path
/// including handled-error paths; only IO-level errors propagate as
/// `Err` so the caller can log without aborting the iteration.
pub async fn process_pending_spec_it(
    workspace: &Path,
    repo: &RepositoryConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    request: &SpecItRequest,
    scout_cfg: &ScoutFeatureConfig,
) -> Result<()> {
    // 1. Load the referenced scout state.
    let state = match scout_run::read_state(workspace, &request.scout_request_id)? {
        Some(s) => s,
        None => {
            let body = format!(
                "✗ spec-it: scout state for request {} not found (was it cleared?). Re-run scout to refresh the list.",
                request.scout_request_id
            );
            reply(chatops_ctx, request, &body).await;
            return Ok(());
        }
    };

    // 2. Resolve the item.
    let item = match state.items.iter().find(|i| i.id == request.item_id) {
        Some(i) => i.clone(),
        None => {
            let body = format!(
                "✗ spec-it: item #{} not present in scout state. The list may have changed; run @<bot> scout <repo> for a fresh list.",
                request.item_id
            );
            reply(chatops_ctx, request, &body).await;
            return Ok(());
        }
    };

    // 3. Staleness signals.
    let now = chrono::Utc::now();
    let age = now - state.completed_at;
    let age_days = age.num_days();
    let scout_too_old = age_days >= scout_cfg.staleness_warn_days as i64;
    let current_head_sha = git::rev_parse(workspace, "HEAD").ok();
    let head_drifted = match current_head_sha.as_deref() {
        Some(cur) => cur != state.head_sha_at_run,
        None => false,
    };
    if scout_too_old || head_drifted {
        let head_clause = if !head_drifted {
            "unchanged".to_string()
        } else {
            let commit_count =
                head_drift_commit_count(workspace, &state.head_sha_at_run).unwrap_or(0);
            format!("moved {commit_count} commits")
        };
        let age_clause = human_age_days(age_days);
        let warn = format!(
            "⚠️ Scout from {age_clause} ago; HEAD has {head_clause}. Proceeding with the scouted item; consider re-running scout for fresh results."
        );
        reply(chatops_ctx, request, &warn).await;
    }

    // 4. Construct propose-request text per the documented shape.
    let request_text = build_propose_text(&item, request.guidance.as_deref());

    // 5. Submit a ProposalRequest using the canonical propose machinery.
    //    Reuses the existing on-disk state file + control-socket action
    //    plumbing so the resulting propose lifecycle posts status updates
    //    into the scout's thread.
    let request_id = uuid::Uuid::new_v4().to_string();
    let proposal_state = ProposalRequestState {
        request_id: request_id.clone(),
        repo_url: request.repo_url.clone(),
        channel: request.channel.clone(),
        // Chain the propose lifecycle back into the scout's thread so
        // every spec-it update lands in one visible conversation.
        thread_ts: request.thread_ts.clone(),
        ack_message_ts: request.thread_ts.clone(),
        operator_user: String::new(),
        request_text,
        submitted_at: now,
        status: ProposalRequestStatus::Pending,
        reason: None,
    };

    let state_root = proposal_requests::default_state_root();
    if let Err(e) = proposal_requests::write_state(&state_root, &proposal_state) {
        let body = format!(
            "✗ spec-it: could not persist proposal-request state file: {e}"
        );
        reply(chatops_ctx, request, &body).await;
        return Ok(());
    }

    // Enqueue via the per-repo proposal-request queue. The polling loop
    // drains this queue on its next iteration AND runs the
    // chat-request-triage flow on each entry.
    if let Err(e) = enqueue_propose_request(
        repo,
        chatops_ctx,
        request,
        &request_id,
    )
    .await
    {
        let body = format!("✗ spec-it: could not enqueue propose-request: {e:#}");
        reply(chatops_ctx, request, &body).await;
        return Ok(());
    }

    let confirm = format!(
        "✓ spec-it #{}: queued propose-request for item `{}`. Follow along in this thread.",
        request.item_id, item.title,
    );
    reply(chatops_ctx, request, &confirm).await;

    Ok(())
}

/// Reach into the daemon's control-socket facility to push a
/// `ProposalRequest` onto the matched repo's `pending_proposal_requests`
/// queue via the standard control-socket action.
async fn enqueue_propose_request(
    repo: &RepositoryConfig,
    _chatops_ctx: Option<&ChatOpsContext>,
    request: &SpecItRequest,
    request_id: &str,
) -> Result<()> {
    // The polling layer doesn't have a direct handle to ControlState;
    // dispatch over the unix socket via the same submitter the chatops
    // dispatcher uses.
    use crate::chatops::operator_commands::{ActionSubmitter, ControlSocketSubmitter};
    let socket_path = crate::control_socket::socket_path();
    let submitter = ControlSocketSubmitter::new(socket_path);
    let resp = submitter
        .submit(serde_json::json!({
            "action": "queue_proposal_request",
            "url": repo.url,
            "request_id": request_id,
        }))
        .await;
    if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("(no error message)");
        anyhow::bail!("queue_proposal_request failed: {err}");
    }
    let _ = request;
    Ok(())
}

/// Build the documented propose-request text shape.
fn build_propose_text(item: &ScoutItem, guidance: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(&format!("[scout-item #{}] {}\n\n", item.id, item.title));
    out.push_str(&item.body);
    out.push_str("\n\n");
    out.push_str(&format!("Source: {}\n", item.source));
    out.push_str(&format!("Category: {}\n", item.category));
    out.push_str(&format!("Tractability: {}\n", item.tractability));
    if let Some(g) = guidance {
        let g = g.trim();
        if !g.is_empty() {
            out.push('\n');
            out.push_str(g);
        }
    }
    out
}

fn human_age_days(days: i64) -> String {
    if days <= 0 {
        "less than a day".to_string()
    } else if days == 1 {
        "1 day".to_string()
    } else {
        format!("{days} days")
    }
}

fn head_drift_commit_count(workspace: &Path, prior_sha: &str) -> Option<usize> {
    let range = format!("{prior_sha}..HEAD");
    git::rev_list_count(workspace, &range).ok()
}

async fn reply(chatops_ctx: Option<&ChatOpsContext>, request: &SpecItRequest, body: &str) {
    if let Some(ctx) = chatops_ctx {
        if let Err(e) = ctx
            .chatops
            .post_threaded_reply(&request.channel, &request.thread_ts, body)
            .await
        {
            tracing::warn!(
                scout_request_id = %request.scout_request_id,
                "spec-it: thread reply failed: {e:#}"
            );
        }
    } else {
        tracing::info!(
            scout_request_id = %request.scout_request_id,
            "spec-it: (no chatops) reply suppressed: {body}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::scout_run::ScoutItem;

    fn item(id: usize) -> ScoutItem {
        ScoutItem {
            id,
            category: "bug".into(),
            title: "Unauthenticated debug endpoint".into(),
            body: "The /debug endpoint serves diagnostic data without authentication."
                .into(),
            source: "src/debug.rs:42".into(),
            tractability: "small".into(),
        }
    }

    #[test]
    fn propose_text_shape_matches_spec_with_no_guidance() {
        let it = item(3);
        let text = build_propose_text(&it, None);
        assert!(text.starts_with("[scout-item #3] Unauthenticated debug endpoint\n\n"));
        assert!(text.contains("Source: src/debug.rs:42"));
        assert!(text.contains("Category: bug"));
        assert!(text.contains("Tractability: small"));
    }

    #[test]
    fn propose_text_appends_operator_guidance() {
        let it = item(5);
        let text = build_propose_text(
            &it,
            Some("stick to the OAuth scope, ignore the rate-limit angle"),
        );
        assert!(
            text.ends_with("\n\nstick to the OAuth scope, ignore the rate-limit angle"),
            "text: {text:?}"
        );
    }

    #[test]
    fn propose_text_skips_empty_guidance() {
        let it = item(7);
        let text = build_propose_text(&it, Some("   \n  "));
        assert!(text.ends_with("Tractability: small\n"), "text: {text:?}");
    }

    #[test]
    fn human_age_days_renders_grammatically() {
        assert_eq!(human_age_days(-1), "less than a day");
        assert_eq!(human_age_days(0), "less than a day");
        assert_eq!(human_age_days(1), "1 day");
        assert_eq!(human_age_days(10), "10 days");
    }
}
