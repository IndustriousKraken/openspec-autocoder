## 1. Config schema

- [x] 1.1 Replace `SlackConfig` in `src/config.rs` with a new `ChatOpsConfig`
    containing: `provider: ChatOpsProvider` (enum: `Slack`, `Discord`,
    `Teams`, `Mattermost`, `Matrix`; `#[serde(rename_all = "snake_case")]`),
    `default_channel_id: String`, and five `Option<<Provider>Config>`
    sub-blocks (`slack`, `discord`, `teams`, `mattermost`, `matrix`).
- [x] 1.2 Define each sub-config struct per `design.md`'s schema:
    `SlackProviderConfig { bot_token_env }`,
    `DiscordProviderConfig { bot_token_env }`,
    `TeamsProviderConfig { tenant_id, client_id, client_secret_env, team_id }`,
    `MattermostProviderConfig { server_url, access_token_env }`,
    `MatrixProviderConfig { homeserver_url, access_token_env }`.
- [x] 1.3 Rename `Config.slack: Option<SlackConfig>` to
    `chatops: Option<ChatOpsConfig>`. Update every callsite.
- [x] 1.4 Rename `RepositoryConfig.slack_channel_id` to
    `chatops_channel_id`. Update the `slack_channel(...)` helper to
    `chatops_channel(...)`. Update every callsite.
- [x] 1.5 Update `config.example.yaml`: replace the commented-out `slack:`
    block with a commented-out `chatops:` block showing `provider: slack`
    and the `chatops.slack:` sub-block; add commented examples of the four
    experimental sub-blocks underneath with a header comment naming each
    as EXPERIMENTAL.
- [x] 1.6 **Verify:** `cargo test config::tests::loads_with_chatops_slack`
    and analogous tests for each of the four experimental providers parse
    the schema; `config::tests::rejects_unknown_chatops_provider` confirms
    an invalid `provider:` value is rejected.

## 2. ChatOpsBackend trait + module restructure

- [x] 2.1 Convert `src/chatops.rs` into `src/chatops/mod.rs`. Move the
    state-file helpers (`write_question_file`, `read_question_file`,
    `write_answer_file`, `read_answer_file`, `delete_question_file`,
    `delete_answer_file`) and the `QuestionPayload`/`AnswerPayload`/
    `HumanReply` types into `mod.rs`. Move the `urlencode` helper into
    `mod.rs` (it'll be reused by Matrix).
- [x] 2.2 Define the `ChatOpsBackend` trait in `mod.rs` per design.md:
    `#[async_trait::async_trait] pub trait ChatOpsBackend: Send + Sync`
    with `provider_name`, `is_experimental`, `post_question`,
    `poll_thread_for_human_reply`. Add `async-trait` to `Cargo.toml`.
- [x] 2.3 Add a startup factory `pub async fn from_config(cfg: &ChatOpsConfig)
    -> Result<Arc<dyn ChatOpsBackend>>` in `mod.rs` that matches
    `cfg.provider` and dispatches to the matching `<Provider>Backend::new`.
    Errors when the matching sub-block is absent name both the provider
    and the missing sub-block.
- [x] 2.4 Update `ChatOpsContext` in `polling_loop.rs` from
    `chatops: Arc<ChatOps>` to `chatops: Arc<dyn ChatOpsBackend>`. Update
    the polling loop's two callsites (`post_question`,
    `poll_thread_for_human_reply`) to call through the trait. No other
    polling-loop changes.
- [x] 2.5 Update `cli/run.rs` to call `chatops::from_config(...)` instead
    of `ChatOps::new(...)`. Construction now returns
    `Arc<dyn ChatOpsBackend>` directly.
- [x] 2.6 **Verify:** existing polling-loop tests
    (`askuser_on_pending_escalates_to_chatops` and the resume tests)
    continue to pass with the trait substituted in. No new test required
    here — the next sections add per-backend tests.

## 3. SlackBackend (relocate existing impl)

- [x] 3.1 Move the existing Slack `ChatOps` impl into a new
    `src/chatops/slack.rs` as `pub struct SlackBackend`. The existing
    HTTP code (`chat.postMessage`, `conversations.replies`, `auth.test`)
    moves verbatim; only the constructor signature changes to take a
    `&SlackProviderConfig` reference.
- [x] 3.2 Implement `ChatOpsBackend for SlackBackend`:
    `provider_name() -> "slack"`, `is_experimental() -> false`,
    `post_question` and `poll_thread_for_human_reply` delegate to the
    existing impl bodies (now inherent methods).
- [x] 3.3 **Verify:** the two existing mockito tests
    (`post_question_*` and `poll_thread_*`) still pass after the move.
    Add one new assertion that `SlackBackend::is_experimental()` is
    `false` and `provider_name()` is `"slack"`.

## 4. DiscordBackend

- [x] 4.1 Create `src/chatops/discord.rs` with
    `pub struct DiscordBackend { client, api_base, bot_token, bot_user_id }`.
    Constructor performs `GET /users/@me` against
    `https://discord.com/api/v10`, with header
    `Authorization: Bot <token>`, to cache `bot_user_id`.
- [x] 4.2 Implement `post_question`: `POST /channels/{c}/messages` with
    JSON `{"content": "❓ `<change>`: <question>"}`. Return the response's
    `id` field (a snowflake string) as the handle.
- [x] 4.3 Implement `poll_thread_for_human_reply`:
    `GET /channels/{c}/messages?after={handle}&limit=50`. Return
    `Some(HumanReply)` for the earliest result whose
    `message_reference.message_id == handle` AND `author.bot == false`.
- [x] 4.4 Implement `ChatOpsBackend for DiscordBackend`:
    `provider_name() -> "discord"`, `is_experimental() -> true`, plus the
    two methods above.
- [x] 4.5 **Verify:** two mockito tests in `discord.rs`:
    `posts_to_messages_endpoint_with_bot_auth` (asserts URL, header,
    body) and `polls_replies_filtered_by_message_reference` (fixture
    response with one bot post + one human reply yields the human reply;
    fixture with only the bot post yields `None`).

## 5. TeamsBackend

- [x] 5.1 Create `src/chatops/teams.rs` with
    `pub struct TeamsBackend { client, api_base, login_base, tenant_id, client_id, client_secret, team_id, token_cache: RwLock<Option<TokenCache>> }`.
    `TokenCache { access_token: String, expires_at: Instant }`.
- [x] 5.2 Implement private `acquire_token()`:
    `POST {login_base}/{tenant_id}/oauth2/v2.0/token` with form body
    `grant_type=client_credentials&client_id=...&client_secret=...&scope=https%3A%2F%2Fgraph.microsoft.com%2F.default`.
    Parse `access_token` + `expires_in`; populate the cache. Used at
    construction and on 401 from any subsequent call.
- [x] 5.3 Constructor calls `acquire_token()` to validate credentials at
    startup. Caches `bot_identity = client_id` (Teams app identity).
- [x] 5.4 Implement `post_question`:
    `POST /teams/{team_id}/channels/{c}/messages` with
    `{"body": {"content": "❓ <code>change</code>: question", "contentType": "html"}}`.
    Return the response's `id` field as the handle.
- [x] 5.5 Implement `poll_thread_for_human_reply`:
    `GET /teams/{team_id}/channels/{c}/messages/{handle}/replies`. Return
    `Some(HumanReply)` for the earliest reply where `from.user` is
    present AND `from.user.id != bot_identity`.
- [x] 5.6 On any 401 response from `post_question` or
    `poll_thread_for_human_reply`, the backend calls `acquire_token()`
    once and retries the original call.
- [x] 5.7 Implement `ChatOpsBackend for TeamsBackend`:
    `provider_name() -> "teams"`, `is_experimental() -> true`.
- [x] 5.8 **Verify:** three mockito tests in `teams.rs`:
    `acquires_token_at_construction`,
    `posts_to_messages_endpoint_with_bearer_token`,
    `polls_replies_filters_bot_self`. The 401-retry path is covered by a
    fourth test `re_acquires_token_on_401`.

## 6. MattermostBackend

- [x] 6.1 Create `src/chatops/mattermost.rs` with
    `pub struct MattermostBackend { client, server_url, access_token, bot_user_id }`.
    Constructor performs `GET {server_url}/api/v4/users/me` with
    `Authorization: Bearer <token>` to cache `bot_user_id`.
- [x] 6.2 Implement `post_question`: `POST /api/v4/posts` with
    `{"channel_id": "<c>", "message": "❓ `<change>`: <question>"}`.
    Return the post's `id` field as the handle.
- [x] 6.3 Implement `poll_thread_for_human_reply`:
    `GET /api/v4/posts/{handle}/thread`. Return `Some(HumanReply)` for
    the earliest post in the `posts` map whose `root_id == handle` AND
    `user_id != bot_user_id`.
- [x] 6.4 Implement `ChatOpsBackend for MattermostBackend`:
    `provider_name() -> "mattermost"`, `is_experimental() -> true`.
- [x] 6.5 **Verify:** two mockito tests in `mattermost.rs`:
    `posts_to_v4_posts_endpoint` and `polls_thread_filters_bot_self`.

## 7. MatrixBackend

- [x] 7.1 Create `src/chatops/matrix.rs` with
    `pub struct MatrixBackend { client, homeserver_url, access_token, user_id, sync_from: RwLock<Option<String>> }`.
    Constructor performs
    `GET /_matrix/client/v3/account/whoami` with the access token to
    cache `user_id`, plus an initial `GET /_matrix/client/v3/sync?timeout=0`
    to obtain an initial `next_batch` token stored in `sync_from`.
- [x] 7.2 Implement `post_question`:
    `PUT /_matrix/client/v3/rooms/{room}/send/m.room.message/{txn_id}`
    where `{room}` and `{txn_id}` are URL-encoded via the shared
    `urlencode` helper, and `{txn_id}` is a fresh UUIDv4 per call. JSON
    body: `{"msgtype": "m.text", "body": "❓ <change>: <question>"}`.
    Return the response's `event_id` field as the handle.
- [x] 7.3 Implement `poll_thread_for_human_reply`:
    `GET /_matrix/client/v3/rooms/{room}/messages?from={sync_from}&dir=f`.
    Return `Some(HumanReply)` for the earliest event whose
    `content.m\\.relates_to.m\\.in_reply_to.event_id == handle` AND
    `sender != user_id`. Update `sync_from` to the response's `end`
    token after each call.
- [x] 7.4 Implement `ChatOpsBackend for MatrixBackend`:
    `provider_name() -> "matrix"`, `is_experimental() -> true`.
- [x] 7.5 **Verify:** two mockito tests in `matrix.rs`:
    `posts_room_message_event` (asserts URL, header, body, txn_id
    presence) and `polls_messages_filters_by_in_reply_to`.

## 8. Startup wiring + experimental warning

- [x] 8.1 In `cli/run.rs`, after `validate_github_token_routes`, replace
    the existing `cfg.slack.as_ref()` match with a single call to
    `chatops::from_config(chatops_cfg)`. The result is an
    `Option<Arc<dyn ChatOpsBackend>>` (None when no `chatops:` block).
- [x] 8.2 Emit the startup log line per requirement: if backend is
    `Some(b)` and `b.is_experimental()`, log a `warn!` line containing
    `"EXPERIMENTAL"`, `"best-effort"`, and `b.provider_name()`. Otherwise
    log an `info!` line containing `"ChatOps escalation enabled via "`
    and `b.provider_name()`. If `None`, log
    `"ChatOps escalation disabled (no chatops: config block)"`.
- [x] 8.3 Update `ChatOpsContext` construction in `cli/run.rs` to use
    `repo.chatops_channel(&chatops_cfg.default_channel_id)` for the
    per-repo channel resolution.
- [x] 8.4 **Verify:** add `cli::run::tests::startup_logs_experimental_warning_for_discord`
    and `cli::run::tests::startup_logs_info_for_slack`. Use
    `tracing-test::traced_test` (add to dev-deps) to assert the log
    line content. Construction can fail (no real env vars set); the
    test only needs to exercise the log-emission helper, which is fine
    to split into a `fn emit_chatops_startup_log(provider, experimental)`.

## 9. Documentation

- [x] 9.1 README: rename the "ChatOps Escalation" section's anchor and
    contents to describe the `chatops:` block with `provider: slack` as
    the officially-supported path. Update all internal links.
- [x] 9.2 Add a new "Experimental ChatOps Backends" section immediately
    after, with a one-paragraph disclaimer ("no API-stability
    guarantees, may break against live API changes, please file bugs")
    and a walkthrough of one Discord setup end-to-end as the
    representative example. Smaller subsections for Teams, Mattermost,
    Matrix listing only the config keys + the bot-token acquisition
    pointer for each.
- [x] 9.3 Configuration Reference: replace the `slack:` row in the table
    with a `chatops:` row. Add a `chatops.provider` row listing valid
    values and which are experimental.
- [x] 9.4 Quick Start prerequisites mention only Slack (the official
    path); the experimental section is for operators who deliberately
    seek it out.
- [x] 9.5 Deployment section's `EnvironmentFile=/etc/autocoder.env`
    example: add commented-in alternatives for each experimental
    provider's required env vars.

## 10. Verification

- [x] 10.1 `cargo test` passes with no regressions. Test count grows by
    at least: 5 config tests, 2 Slack tests (existing relocated), 2
    Discord, 4 Teams, 2 Mattermost, 2 Matrix, 2 startup-log tests = ~19
    new or relocated tests.
- [x] 10.2 `cargo build --release` produces a binary that, given a
    config with each of the five providers set in turn (and matching
    env vars), starts up emitting the expected log line and the
    factory returns the correct backend type. (Live-service verification
    is per-operator and not in this change.)
- [x] 10.3 `openspec validate experimental-chatops-providers --strict`
    passes.
