## Why

Slack apps carry two distinct identifiers. The `user_id` (U-prefix) is the bot's user-account identifier; the `bot_id` (B-prefix) is the bot/app identifier. Both refer to the same bot and either can resolve in a mention. The Slack desktop client emits mentions as `<@U...>` (user-style); the Slack mobile app emits the same mention as `<@B...>` (bot-style). Both render identically as `@<bot-name>` on screen, so an operator typing `@autocoder status` on their phone has no visual cue that the underlying message text is different from desktop.

The chatops inbound listener today (per `chatops-slack-inbound-listener`) caches only `user_id` via `auth.test` at startup AND requires the leading-mention check to match `<@{self.user_id}>`. Mobile-app messages whose mention is `<@B...>` never match, fail the leading-mention filter, are silently dropped (the `?` reaction doesn't fire either, because the filter rejects the message before it reaches the unknown-verb path). Operators on mobile see no response at all.

A real-world observation: operator's phone shows `@autocoder status` typed in the channel; the same message viewed on desktop shows `@B0B36FN15K9 status`. The chatops listener accepts the desktop form (`<@U0BOT_USER>`) but rejects the mobile form (`<@B0B36FN15K9>`). Same bot, same operator intent, two failure modes depending on which client they're using.

The fix is small: cache both IDs at startup (both come back in the existing `auth.test` response), accept either in the leading-mention check. No new API calls, no schema changes.

## What Changes

**Cache `bot_id` alongside `user_id` at SlackBackend construction.** `auth.test` already returns both fields (`user_id` is U-prefixed, `bot_id` is B-prefixed). The existing `AuthTestResponse` parser captures only `user_id`; extend it to also capture `bot_id`. Both are stored on the `SlackBackend` struct.

If `auth.test` returns `bot_id: null` (rare; some Slack token types don't have a bot_id), the daemon SHALL log WARN naming the missing field AND continue with `user_id`-only matching. Mobile-app mentions in that configuration won't work, but desktop continues; a clear log line tells the operator why mobile is broken.

**Leading-mention check accepts either form.** The current check matches `<@{user_id}>` as the first non-whitespace token. The new check matches `<@{user_id}>` OR `<@{bot_id}>` (when bot_id is present). Same trim-whitespace semantics; same position requirement (leading token); same case-sensitivity (Slack IDs are case-sensitive; the existing implementation already respects that).

**The dispatcher receives the canonical bot identifier.** Today the dispatcher gets `bot_mention: &str` formatted as `<@{user_id}>`. That parameter is used downstream to parse out the bot reference from the message body. To avoid duplicating mention-parsing across the listener and the dispatcher, the dispatcher continues to receive the user-id form regardless of which mention the message actually used. The listener normalizes: when it accepts an inbound message that mentions via `<@B...>`, it passes the canonical `<@U...>` to the dispatcher. The dispatcher's parser is unchanged.

**`parse_revision_trigger` accepts either mention form too.** Per `a01-pr-comment-revision-loop`, GitHub-comment-based revisions parse `@<bot-username> revise <text>`. That uses the GitHub username (text-based, not the Slack U/B-prefix), so the bot-id concern doesn't apply there. No changes needed to the revision-loop spec.

**No changes to the dispatcher, parser, or any other downstream consumer of the bot mention.** The fix is entirely in the SlackBackend + the listener's incoming-message filter.

## Impact

- **Affected specs:** `chatops-manager` — one ADDED requirement covering the bot_id caching and the dual-mention-form acceptance.
- **Affected code:**
  - `autocoder/src/chatops/slack.rs`'s `AuthTestResponse` struct gains `pub bot_id: Option<String>` (Option because some token types may not have one). The `SlackBackend` struct gains `pub bot_id: Option<String>`. Construction populates from the parsed response.
  - `autocoder/src/chatops/slack.rs`'s leading-mention check (in the inbound listener's `app_mention` handler) accepts `<@{user_id}>` OR `<@{bot_id}>` when `bot_id.is_some()`.
  - Normalization: when an inbound message uses the bot-id mention, the listener rewrites the message's leading token to the user-id form BEFORE passing to the dispatcher. The dispatcher continues to see only the user-id form regardless of inbound source.
  - WARN log at SlackBackend construction when `bot_id` is None in the response.
  - Tests:
    - `AuthTestResponse` parser: both fields populated → both fields on `SlackBackend`; `bot_id` missing → `Some(user_id), None bot_id` + WARN.
    - Leading-mention check accepts `<@U_BOT_USER>` (existing).
    - Leading-mention check accepts `<@B_BOT_ID>` when `bot_id` is cached.
    - Leading-mention check rejects `<@B_BOT_ID>` when `bot_id` is `None` (the cache is empty for this token; only the user-id form works).
    - Normalization: a message body `<@B_BOT_ID> status` is rewritten to `<@U_BOT_USER> status` before reaching the dispatcher.
    - End-to-end: stub a Slack inbound with `<@B...>` mention; assert the dispatcher's handle_message is invoked with the message body rewritten to `<@U...>` AND with the correct `bot_mention: <@U_BOT_USER>` parameter.

- **Operator-visible behavior:** mobile-app chatops commands work. Operators using `@autocoder status` from their phone get the same response they would on desktop. No new configuration required; the fix kicks in automatically when the SlackBackend is reconstructed (next daemon restart after upgrade).
- **Breaking:** no. Desktop messages continue to match via `<@U...>` as today. The bot_id form is additive.
- **Acceptance:** `cargo test` passes (new + existing). A Slack message whose body's leading token is `<@B...>` (mobile-app emission format) is recognized by the chatops listener AND dispatched to the operator-commands flow with the body's mention normalized to `<@U...>`. The dispatcher's reply text and behavior are identical to a desktop-emitted message.
