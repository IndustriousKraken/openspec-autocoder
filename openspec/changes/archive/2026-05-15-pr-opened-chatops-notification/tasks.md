## 1. Config schema

- [x] 1.1 In `autocoder/src/config.rs::NotificationsConfig`, add `pub pr_opened: bool` with `#[serde(default = "default_true")]`. Update the `Default` impl to set `pr_opened: true`.
- [x] 1.2 Add `pub fn pr_opened_enabled(chatops: Option<&ChatOpsConfig>) -> bool` mirroring `start_work_enabled` and `failure_alerts_enabled`: defaults to `true` when the block or field is absent.
- [x] 1.3 Tests in `config::tests`:
  - `pr_opened_default_is_true_when_block_absent`
  - `pr_opened_default_is_true_when_field_absent`
  - `pr_opened_explicit_false_disables`

## 2. ChatOpsContext + helper

- [x] 2.1 In `autocoder/src/polling_loop.rs::ChatOpsContext`, add `pub pr_opened_enabled: bool` next to the existing flags. Also added the field on `ChatOpsSlot` in `control_socket.rs` (the hot-reload slot that feeds `ChatOpsContext` at iteration start).
- [x] 2.2 Add a helper function (near `maybe_post_start_of_work`):
  ```rust
  async fn maybe_post_pr_opened(
      repo: &RepositoryConfig,
      chatops_ctx: Option<&ChatOpsContext>,
      pr_url: &str,
      change_count: usize,
  ) {
      let Some(ctx) = chatops_ctx else { return };
      if !ctx.pr_opened_enabled {
          return;
      }
      let text = format!(
          "🎉 `{}`: opened PR {pr_url} with {change_count} change(s)",
          repo.url
      );
      if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
          tracing::warn!(
              url = %repo.url,
              pr = %pr_url,
              "pr-opened notification failed; continuing: {e:#}"
          );
      }
  }
  ```

## 3. Wire into open_pull_request

- [x] 3.1 In `polling_loop::open_pull_request`, after the `tracing::info!("opened PR ...")` log line and BEFORE the `post_implementer_summary_comment` call, invoke `maybe_post_pr_opened(repo, chatops_ctx, &pr.html_url, changes.len()).await;`.

## 4. Daemon-level plumbing

- [x] 4.1 In `autocoder/src/cli/run.rs` and `autocoder/src/control_socket.rs`'s `build_chatops_slot`, populate `pr_opened_enabled` from `NotificationsConfig::pr_opened_enabled(Some(co))`. `build_chatops_ctx` in `polling_loop.rs` propagates the value into the per-iteration `ChatOpsContext`.

## 5. Tests

- [x] 5.1 `polling_loop::tests::pr_opened_notification_fires_when_enabled` — directly tests `maybe_post_pr_opened` with a mockito Slack server expecting exactly one `chat.postMessage` whose body matches the repo URL, PR URL, and change count.
- [x] 5.2 `polling_loop::tests::pr_opened_notification_suppressed_when_disabled` — same fixture but `pr_opened_enabled = false`; mockito asserts zero posts.
- [x] 5.3 `polling_loop::tests::pr_opened_notification_failure_does_not_propagate` — Slack returns `ok:false`; the helper still returns without panic, never propagates.
- [x] 5.4 **Verify:** every existing `ChatOpsContext` and `ChatOpsSlot` construction site updated to include `pr_opened_enabled`. Confirmed via `cargo check --tests` (compiles cleanly).

Additional test added:
- [x] 5.5 `polling_loop::tests::pr_opened_notification_noop_without_chatops` — covers `chatops_ctx = None`.

## 6. Documentation

- [x] 6.1 README "Progress notifications" — added `pr_opened: true` line to the example YAML and updated the "absent block" sentence from "both true" to "all true".

## 7. Verification

- [x] 7.1 `cargo test` passes (377/378; 1 ignored is unrelated).
- [x] 7.2 `openspec validate pr-opened-chatops-notification --strict` passes.
