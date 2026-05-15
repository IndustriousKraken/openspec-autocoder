## Why

Today autocoder opens PRs and the only signal an operator gets is the systemd log line `opened PR pr=<url>`. To know that work has shipped, the operator has to be tailing journalctl or watching GitHub. The existing start-of-work and failure-alert notifications already tell ChatOps about what's *starting* and what's *broken*; the natural complement is a notification when work *succeeds* — i.e. when the PR is opened and waiting for human review.

Adding a one-line PR-opened ChatOps post (with a clickable link to the PR) closes the loop: an operator who watches their Slack channel sees the daemon start work, then sees the resulting PR appear without having to switch to journalctl or GitHub. The notification reuses the existing `notifications.*` flag pattern, so it's easy to enable/disable and consistent with the other notification types.

## What Changes

- **ADDED capability:** `orchestrator-cli` gains a "PR-opened ChatOps notification" requirement covering when the notification fires, what it contains, and how it interacts with the existing notification flags.
- **Config:** new optional `chatops.notifications.pr_opened: bool` flag (default `true`, matching `start_work` / `failure_alerts`). When unset or `true`, autocoder posts the notification. When `false`, the post is suppressed (the existing INFO log line is unchanged).
- **Code:**
  - `NotificationsConfig` gains `pr_opened: bool` (`default = true`).
  - `ChatOpsContext` gains `pr_opened_enabled: bool` resolved from the optional `NotificationsConfig` at startup (same pattern as `start_work_enabled` and `failure_alerts_enabled`).
  - `polling_loop::open_pull_request` calls a new `maybe_post_pr_opened` helper *after* `github::create_pull_request` returns `Ok(pr)`, *before* the existing post-PR-comment step. The helper posts via `ctx.chatops.post_notification(channel, text)` and logs WARN on failure (never propagates).
- **Message format:** `🎉 \`<repo_url>\`: opened PR <pr.html_url> with <N> change(s)`. The URL is included as a bare token so every backend renders it as a clickable link without needing provider-specific markup.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/config.rs` (one new field on `NotificationsConfig` + tests), `autocoder/src/polling_loop.rs` (`ChatOpsContext` gains one field; `open_pull_request` gains one helper call), `autocoder/src/cli/run.rs` (resolve the new flag when building `ChatOpsContext`).
- Behavior change: when ChatOps is configured and `pr_opened` is true (the default), each successful PR creation produces one short message in the configured channel. Default-on so operators get the notification immediately on upgrade without config changes; explicit `false` opt-out for noise-sensitive channels.
- README: under `chatops.notifications` table, add the new `pr_opened` row.
- Breaking: no. Existing configs without `notifications` (or with `notifications.start_work` / `failure_alerts` only) get the default `true` for `pr_opened`.
