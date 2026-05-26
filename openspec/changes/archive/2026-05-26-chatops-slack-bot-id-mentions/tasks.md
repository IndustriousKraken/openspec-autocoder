## 1. Cache bot_id at SlackBackend construction

- [x] 1.1 Extend the `AuthTestResponse` struct in `autocoder/src/chatops/slack.rs`:
  ```rust
  #[derive(Deserialize)]
  struct AuthTestResponse {
      ok: bool,
      #[serde(default)]
      user_id: Option<String>,
      #[serde(default)]
      bot_id: Option<String>,    // NEW
      #[serde(default)]
      error: Option<String>,
  }
  ```
- [x] 1.2 Extend the `SlackBackend` struct with `pub bot_id: Option<String>`. Construction populates from the parsed response.
- [x] 1.3 WARN log at construction when `bot_id` is None:
  ```
  WARN slack: auth.test response missing bot_id field; mobile-app mentions (B-prefix) will not be recognized. Desktop mentions (U-prefix) continue to work.
  ```
- [x] 1.4 Tests:
  - `auth.test` mock returning both fields → `SlackBackend` has both populated; no WARN.
  - `auth.test` mock returning only `user_id` → `bot_id: None`; WARN logged.
  - `auth.test` mock returning only `bot_id` (defensive; rare) → existing user_id error path fires (bot is unusable without user_id).

## 2. Leading-mention check accepts either form

- [x] 2.1 Locate the leading-mention check in the chatops inbound listener (in `slack.rs`, the `app_mention` event handler from `chatops-slack-inbound-listener`). The current check looks for `<@{user_id}>` as the first non-whitespace token.
- [x] 2.2 Replace with a check that accepts either:
  ```rust
  fn leading_mention_matches_self(text: &str, user_id: &str, bot_id: Option<&str>) -> Option<MentionForm> {
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
  pub enum MentionForm { UserId, BotId }
  ```
- [x] 2.3 Tests:
  - Text starting with `<@U_BOT_USER> status` returns `Some(UserId)`.
  - Text starting with `<@B_BOT_ID> status` returns `Some(BotId)` when bot_id is cached.
  - Text starting with `<@B_BOT_ID> status` returns `None` when bot_id is `None`.
  - Text starting with `<@U_OTHER_USER> status` returns `None`.
  - Text with leading whitespace before either mention form still matches (the existing trim-whitespace behavior).

## 3. Normalize bot-id mention to user-id mention before dispatch

- [x] 3.1 When the leading-mention check returns `Some(BotId)`, rewrite the message text's leading token from `<@{bot_id}>` to `<@{user_id}>` before passing to the dispatcher. The rest of the message body is unchanged.
- [x] 3.2 The dispatcher continues to receive `bot_mention: "<@{user_id}>"` regardless of inbound mention form. Downstream code is unchanged.
- [x] 3.3 Tests:
  - Inbound message `<@B_BOT_ID> status myrepo` → dispatcher receives message `<@U_BOT_USER> status myrepo` AND `bot_mention: "<@U_BOT_USER>"`.
  - Inbound message `<@U_BOT_USER> status myrepo` → dispatcher receives unchanged message AND `bot_mention: "<@U_BOT_USER>"`.

## 4. README + docs updates

- [x] 4.1 In `docs/CHATOPS.md`'s troubleshooting subsection (or a new "Mobile vs desktop mention forms" paragraph), add a brief note explaining that Slack's mobile app emits mentions with the bot/app ID (B-prefix) while desktop emits the user ID (U-prefix). autocoder accepts both; operators don't need to do anything specific. If mobile mentions stop working after a token rotation, check the daemon log for the `auth.test response missing bot_id` WARN.

## 5. Spec delta

- [x] 5.1 The ADDED requirement in `openspec/changes/chatops-slack-bot-id-mentions/specs/chatops-manager/spec.md` codifies: the bot_id caching at construction, the dual-form leading-mention acceptance, the normalization-to-user-id-form before dispatch, and the WARN-when-missing behavior.

## 6. Verification

- [x] 6.1 `cargo test` passes (new + existing).
- [x] 6.2 `openspec validate chatops-slack-bot-id-mentions --strict` passes.
- [x] 6.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
