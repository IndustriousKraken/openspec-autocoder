## 1. Config schema

- [ ] 1.1 Add `pub notifications: Option<NotificationsConfig>` field to `SlackConfig` in `src/config.rs` with `#[serde(default)]`.
- [ ] 1.2 Define `pub struct NotificationsConfig { #[serde(default = "default_true")] pub start_work: bool, #[serde(default = "default_true")] pub failure_alerts: bool }` with `#[serde(deny_unknown_fields)]`. Add a `fn default_true() -> bool { true }` helper.
- [ ] 1.3 Add helper `impl NotificationsConfig { pub fn start_work_enabled(slack: Option<&SlackConfig>) -> bool; pub fn failure_alerts_enabled(slack: Option<&SlackConfig>) -> bool; }` returning `true` when the field defaults apply AND `false` when explicitly disabled. (Or expose getters with `Option` chaining at the call site — pick whichever is cleaner.)
- [ ] 1.4 **Verify:** add tests `config::tests::loads_notifications_block`, `config::tests::notifications_partial_populated_defaults_other_to_true`, `config::tests::notifications_rejects_unknown_field`, `config::tests::notifications_absent_block_defaults_both_true`.

## 2. ChatOps `post_notification` method

- [ ] 2.1 Add `pub async fn post_notification(&self, channel: &str, text: &str) -> Result<()>` to `ChatOps` in `src/chatops.rs`. Implementation mirrors `post_question`'s HTTP shape but posts the raw `text` field (no `❓` prefix, no `change` formatting) and returns `Ok(())` without parsing `ts`.
- [ ] 2.2 **Verify:** add a mockito test `chatops::tests::post_notification_posts_to_chat_postmessage` asserting URL, auth header, JSON body shape (`{ "channel": "...", "text": "..." }` — no `link_names`), and that on `ok: true` the method returns `Ok(())`.
- [ ] 2.3 **Verify:** add `chatops::tests::post_notification_returns_err_on_ok_false` asserting the error text contains the Slack `error` field.

## 3. Alert state file

- [ ] 3.1 Add a new `src/alert_state.rs` module:
    ```rust
    pub enum AlertCategory { WorkspaceInitFailure, BranchPushFailure, PrCreationFailure }
    pub struct AlertEntry { pub last_alerted_at: DateTime<Utc>, pub last_error_excerpt: String }
    pub struct AlertState { pub alerts: HashMap<AlertCategory, AlertEntry> }
    impl AlertState {
        pub fn load_or_default(workspace: &Path) -> Self;       // missing file → empty
        pub fn save(&self, workspace: &Path) -> Result<()>;     // atomic tempfile-then-rename
        pub fn clear(workspace: &Path) -> Result<()>;           // idempotent file removal
    }
    ```
    Serialize `AlertCategory` via `#[serde(rename_all = "snake_case")]` so the JSON keys match the spec's labels.
- [ ] 3.2 Reuse the existing atomic-write pattern from `chatops::write_question_file` (tempfile-in-same-dir, then persist). Path: `<workspace>/.alert-state.json`.
- [ ] 3.3 **Verify:** add unit tests `alert_state::tests::load_missing_returns_empty`, `save_and_reload_roundtrip`, `clear_is_idempotent`, `clear_does_not_error_on_missing`.

## 4. Failure-alert handling helper

- [ ] 4.1 In `src/polling_loop.rs` (or a new `src/alerts.rs`, your call), add:
    ```rust
    async fn handle_predictable_failure(
        workspace: &Path,
        repo_url: &str,
        chatops_ctx: Option<&ChatOpsContext>,
        notifications_enabled: bool,
        category: AlertCategory,
        err: &anyhow::Error,
    );
    ```
    Logic per design.md: skip if disabled or no chatops; load state; check 24h window; format text; call `post_notification`; on success update timestamp; on post failure log and do NOT update timestamp.
- [ ] 4.2 Format the alert text:
    ```
    ⚠️ `<repo-url>`: <category-label> for the past 24h. Latest: <error-excerpt>
    ```
    where `<category-label>` is one of `workspace init keeps failing`, `branch push keeps failing`, `PR creation keeps failing`; `<error-excerpt>` is `format!("{err:#}")` truncated to 200 chars with an ellipsis suffix when truncated.
- [ ] 4.3 **Verify:** unit tests for the helper covering: (a) first failure posts and saves state, (b) repeat within 24h is silent, (c) >24h re-alerts, (d) post failure does NOT update state, (e) disabled notifications skip even reading the file.

## 5. Wire start-of-work notifications

- [ ] 5.1 In `polling_loop::walk_queue`, after the change is dequeued AND `.in-progress` is locked AND BEFORE invoking the executor, call:
    ```rust
    if start_work_enabled && let Some(ctx) = chatops_ctx {
        let summary = first_line_of_section(&proposal_text, "## Why").unwrap_or("");
        let text = format!("🚀 `{}`: starting work on `{}` — {}", repo.url, change, summary);
        if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
            tracing::warn!(?e, change, "start-of-work notification failed; continuing");
        }
    }
    ```
    Read `proposal_text` only if the notification is enabled (avoid the disk read otherwise).
- [ ] 5.2 **Verify:** add `polling_loop::tests::start_of_work_notification_posted_on_dequeue` — fixture pass with one pending change, mockito server, assertion that the start-of-work mock matched.
- [ ] 5.3 **Verify:** add `polling_loop::tests::start_of_work_suppressed_when_disabled` — same fixture but with `notifications.start_work: false` (or programmatic equivalent); assertion that the mock was NOT called.

## 6. Wire failure alerts at three sites

- [ ] 6.1 `workspace_init_failure` site: in `polling_loop::run_pass_through_commits`, wrap the `workspace::ensure_initialized(workspace, &repo.url)?` call so that on Err, `handle_predictable_failure` is called with `WorkspaceInitFailure` before returning. (Cannot use `?` directly; refactor to `if let Err(e) = ... { handle...(...).await; return Err(e); }`.)
- [ ] 6.2 `branch_push_failure` site: in `execute_one_pass`, wrap the `git::push_force_with_lease(workspace, &repo.agent_branch)?` call similarly.
- [ ] 6.3 `pr_creation_failure` site: in `open_pull_request`, wrap the `github::create_pull_request(...)` call similarly.
- [ ] 6.4 Clear-on-success: at the END of `execute_one_pass`, on the Ok path (after push and PR creation have both succeeded, OR when the pass completed cleanly without producing commits), call `AlertState::clear(workspace)`. This clears every category at once — once an iteration has run end-to-end without hitting any of the three failure sites, the throttle resets so the next failure (whenever it happens) re-alerts immediately.
    - **Why end-of-pass, not after-init:** if clear-on-success ran at the top of `run_pass_through_commits`, a transient `workspace_init_failure` followed by a successful re-init on the next pass would wipe the throttle — and then a push failure on that same pass would re-alert inside the 24h window, breaking scenario 6.5. Clearing only after the WHOLE pass succeeds ties the throttle to actual recovery.
    - **Granularity (out of scope for this change):** a more precise design would clear each category at its own success point (init-failure throttle clears after init succeeds, push-failure throttle after push succeeds, etc.). That's a future refinement; the simpler "clear all on end-of-pass" is sufficient for the scenarios below.
- [ ] 6.5 **Verify:** integration test `polling_loop::tests::failure_alert_posted_then_suppressed_within_24h` — fixture with a forced push failure (e.g. unreachable git remote), run two iterations, assert the chatops mock received exactly one alert.
- [ ] 6.6 **Verify:** `polling_loop::tests::failure_alert_cleared_on_subsequent_success` — fixture where iteration 1 fails (post fires), iteration 2 succeeds (state cleared), iteration 3 fails again (post fires AGAIN, with no 24h delay because state was cleared).

## 7. Documentation

- [ ] 7.1 README's ChatOps Escalation section: add a new "Progress notifications" subsection between "Configuring Slack" and "Required Slack bot scopes" describing the start-of-work and failure-alert flows, the config keys, the throttle window, and the per-workspace `.alert-state.json` artifact.
- [ ] 7.2 README's existing "`.question.json` and `.answer.json` as workspace artifacts" note: extend to mention `.alert-state.json` as a third per-workspace artifact (safe to inspect, safe to delete — deleting resets the alert window for that repo).
- [ ] 7.3 `config.example.yaml`: add a commented `notifications:` sub-block under the existing `slack:` block.

## 8. Verification

- [ ] 8.1 `cargo test` passes; test count grows by at least: 4 config + 2 chatops + 4 alert_state + ~5 helper + 4 polling-loop integration = ~19 new tests.
- [ ] 8.2 `cargo build --release` produces a binary that, configured with `notifications.start_work: true` and a working chatops backend, posts a start-of-work message on each pending-change dequeue.
- [ ] 8.3 `openspec validate chatops-progress-notifications --strict` passes.
