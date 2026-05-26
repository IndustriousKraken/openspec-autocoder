## ADDED Requirements

### Requirement: SlackBackend caches both user_id and bot_id at construction
`SlackBackend::new_at` SHALL parse BOTH `user_id` AND `bot_id` from the `auth.test` response AND store them on the struct. The `user_id` (U-prefix) is the bot's user-account identifier as today; the `bot_id` (B-prefix) is the bot/app identifier that some Slack clients (notably the mobile app) emit when the operator mentions the bot. Both fields are returned by `auth.test`; the existing parser captures only `user_id` and discards `bot_id`. When `auth.test` does not include `bot_id` (rare; some token types lack one), the backend SHALL log WARN naming the gap AND store `bot_id: None`; mobile-app mentions will not be recognized in that configuration but desktop mentions continue to work.

#### Scenario: Both fields are cached when auth.test returns both
- **WHEN** `SlackBackend::new_at` is invoked AND the `auth.test` response contains both `user_id: "U_BOT"` and `bot_id: "B_BOT"`
- **THEN** the constructed `SlackBackend` has `user_id == "U_BOT"` AND `bot_id == Some("B_BOT")`
- **AND** no WARN is logged

#### Scenario: Missing bot_id logs WARN and stores None
- **WHEN** the `auth.test` response contains `user_id: "U_BOT"` but no `bot_id` field
- **THEN** the constructed `SlackBackend` has `user_id == "U_BOT"` AND `bot_id == None`
- **AND** a WARN log fires naming the missing field AND that mobile-app mentions will not be recognized

### Requirement: Inbound listener accepts either mention form as the leading bot mention
The inbound listener's leading-mention check SHALL accept `<@{user_id}>` OR (when `bot_id` is cached) `<@{bot_id}>` as the message's leading non-whitespace token. The two forms refer to the same bot; clients vary in which they emit. After acceptance, the listener SHALL rewrite the message body's leading token to the user-id form before passing to the dispatcher; downstream code sees a normalized `<@{user_id}>` mention regardless of inbound source.

#### Scenario: Desktop mention form is accepted as today
- **WHEN** an inbound message's text is `<@U_BOT> status myrepo` AND the cached `user_id` is `U_BOT`
- **THEN** the leading-mention check accepts the message
- **AND** the dispatcher receives the message text unchanged AND `bot_mention: "<@U_BOT>"`

#### Scenario: Mobile mention form is accepted and normalized
- **WHEN** an inbound message's text is `<@B_BOT> status myrepo` AND the cached `bot_id` is `Some("B_BOT")`
- **THEN** the leading-mention check accepts the message
- **AND** the dispatcher receives the message text rewritten to `<@U_BOT> status myrepo`
- **AND** the dispatcher receives `bot_mention: "<@U_BOT>"`

#### Scenario: Mobile mention form is rejected when bot_id is uncached
- **WHEN** an inbound message's text is `<@B_BOT> status myrepo` AND the cached `bot_id` is `None`
- **THEN** the leading-mention check rejects the message
- **AND** the dispatcher is NOT invoked
- **AND** the inbound listener applies its existing dispatcher-rejection handling (silent drop per the `chatops-slack-inbound-listener` filter contract)

#### Scenario: Mention referring to a different user is rejected
- **WHEN** an inbound message's text is `<@U_OTHER> status myrepo` (a different user's mention)
- **THEN** the leading-mention check rejects the message
- **AND** the dispatcher is NOT invoked
