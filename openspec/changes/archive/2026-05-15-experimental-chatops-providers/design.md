## Context

The existing ChatOps implementation is a single concrete `ChatOps` struct in
`autocoder/src/chatops.rs` whose public surface (apart from the file-state
helpers) is:

- `pub async fn new(bot_token: String) -> Result<Self>` (Slack-specific
  construction; calls `auth.test` and caches `bot_user_id`)
- `pub fn bot_user_id(&self) -> &str`
- `pub async fn post_question(&self, channel: &str, change: &str, question: &str) -> Result<String>`
- `pub async fn poll_thread_for_human_reply(&self, channel: &str, thread_ts: &str) -> Result<Option<HumanReply>>`

`polling_loop.rs` uses exactly two of these methods on the hot path:
`post_question` (to escalate) and `poll_thread_for_human_reply` (to detect a
reply). `bot_user_id` is used only internally for the bot-self-filtering
heuristic in `poll_thread_for_human_reply`.

That gives us a small, real trait surface. Three other ChatOps platforms have
matching shapes:

- **Discord**: top-level message has an `id`; replies reference it via
  `message_reference.message_id`. Polling reads messages after the bot post.
- **Teams**: top-level message has a `messageId`; replies live under
  `/messages/{id}/replies`. Graph API auth is OAuth client-credentials.
- **Mattermost**: post has an `id`; replies share `root_id`. PAT auth.
- **Matrix**: event has an `event_id`; replies carry `m.relates_to.m.in_reply_to`.

IRC was considered and rejected: no persistent message id, requires a stateful
TCP connection rather than HTTP, and reply matching is heuristic. Users on
IRC are pointed at the Matrix bridge.

## Goals / Non-Goals

**Goals:**

- A small trait (`ChatOpsBackend`) that the polling loop consumes without
  caring which provider is in use.
- One Slack-officially-supported impl + four experimental impls, all loaded
  through a single startup factory.
- Loud per-startup warning naming the experimental provider so an operator
  who selected one cannot miss it.
- Config schema that's easy to read for a single-provider operator (no
  per-provider clutter for the unselected providers).
- Each experimental impl has unit-test coverage against a `mockito` fixture
  so the test suite stays self-contained (no live-service calls).

**Non-Goals:**

- **Bidirectional feature parity.** Threading, mentions, formatting, and
  attachment behavior will differ across providers. Each impl renders its
  one outgoing message as plain text plus a leading `❓ <change>:` prefix; no
  rich formatting is normalized across backends. If a provider lacks native
  threading (e.g. some Discord channel types), the impl falls back to
  polling channel history after the bot post.
- **Multi-provider concurrent use.** A single autocoder instance runs against
  one ChatOps backend at a time. Operators with multiple chat platforms run
  multiple instances or pick the most-used one.
- **OAuth flow management.** Teams' `client_credentials` token acquisition is
  in-scope; user-context OAuth flows are not. The operator obtains a bot
  token via the provider's developer portal and exports it; autocoder reads
  the token only.
- **Webhook / one-way sinks.** A pure-webhook "post-only, AskUser falls back
  to log-and-exit" mode was discussed but is deferred — it crosses the
  bidirectional/unidirectional line that the rest of the design doesn't
  currently model.
- **IRC.** Out of scope; recommend Matrix-IRC bridge.

## Decisions

### Trait shape

```rust
#[async_trait::async_trait]
pub trait ChatOpsBackend: Send + Sync {
    /// Stable name used in logs and the experimental-warning line.
    fn provider_name(&self) -> &'static str;

    /// Whether non-Slack providers SHOULD log the experimental warning.
    fn is_experimental(&self) -> bool;

    /// Post `question` to `channel` and return the opaque thread handle
    /// (provider-specific format) that subsequent reply-polls reference.
    async fn post_question(
        &self,
        channel: &str,
        change: &str,
        question: &str,
    ) -> Result<String>;

    /// Poll for the earliest reply in the thread identified by `handle`
    /// (the value previously returned from `post_question`). The reply
    /// MUST NOT be the bot's own message — providers that emit a bot-id
    /// or bot-user marker filter on that; providers without one filter
    /// by exact message-id equality against the handle.
    async fn poll_thread_for_human_reply(
        &self,
        channel: &str,
        handle: &str,
    ) -> Result<Option<HumanReply>>;
}
```

`HumanReply` is unchanged from today: `text`, `user_id`, `ts`. The `ts`
field is opaque (provider-specific format); the polling loop only stores it
in the state file.

Returning `String` (rather than a typed `MessageHandle`) keeps the
state-file format (`QuestionPayload.thread_ts`) unchanged and avoids a
serde-polymorphism layer. Each provider serializes its handle as a
provider-specific string (Slack: `1234567890.123456`; Discord:
`"1234567890123456789"`; Matrix: `$abc:server.tld`; etc.).

### Config schema

```yaml
chatops:
  provider: slack          # required; one of: slack | discord | teams | mattermost | matrix
  default_channel_id: "C01234ABCDE"  # provider-native channel id format

  slack:                   # required only when provider: slack
    bot_token_env: SLACK_BOT_TOKEN

  discord:                 # required only when provider: discord
    bot_token_env: DISCORD_BOT_TOKEN

  teams:                   # required only when provider: teams
    tenant_id: "11111111-2222-3333-4444-555555555555"
    client_id: "66666666-7777-8888-9999-aaaaaaaaaaaa"
    client_secret_env: TEAMS_CLIENT_SECRET
    team_id: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff"

  mattermost:              # required only when provider: mattermost
    server_url: "https://mattermost.example.com"
    access_token_env: MATTERMOST_TOKEN

  matrix:                  # required only when provider: matrix
    homeserver_url: "https://matrix.example.com"
    access_token_env: MATRIX_ACCESS_TOKEN
```

`repositories[].slack_channel_id` is renamed to `chatops_channel_id`. There
is no in-flight deployment outside the author's own to migrate; the rename
is a clean break.

The unselected sub-blocks are tolerated for ergonomic reasons (operator can
keep all of them filled in and switch `provider:` to test); only the
matching one is consulted. `#[serde(deny_unknown_fields)]` is preserved by
declaring all five sub-blocks as `Option<...>` on the parent struct rather
than using a flattened tagged enum.

### Startup factory

A new function `ChatOpsBackend::from_config(cfg: &ChatOpsConfig) -> Result<Arc<dyn ChatOpsBackend>>`
inspects `cfg.provider` and dispatches to the matching `<Provider>Backend::new(...)`.
Misconfiguration (e.g. `provider: teams` with no `chatops.teams:` block) is
caught here with an error that names both the selected provider and the
missing sub-block.

After construction, `cli::run::execute` emits exactly one of:

- `tracing::info!("ChatOps escalation enabled via {} ({})", provider_name, "officially supported")` — Slack
- `tracing::warn!("EXPERIMENTAL: ChatOps escalation enabled via {} — best-effort support, may break without notice, no API-stability guarantees", provider_name)` — Discord/Teams/Mattermost/Matrix

### Code organization

```
autocoder/src/chatops/
  mod.rs                   # ChatOpsBackend trait + factory + HumanReply + state-file helpers
  slack.rs                 # SlackBackend impl (existing Slack code, moved)
  discord.rs               # DiscordBackend impl
  teams.rs                 # TeamsBackend impl (incl. OAuth client-creds token cache)
  mattermost.rs            # MattermostBackend impl
  matrix.rs                # MatrixBackend impl
```

The state-file helpers (`write_question_file`, `read_question_file`,
`write_answer_file`, `read_answer_file`, `delete_question_file`,
`delete_answer_file`) stay in `mod.rs` — they're provider-agnostic.

### Each experimental impl's correctness floor

For each of Discord, Teams, Mattermost, Matrix:

- Compiles.
- Has at least 2 mockito-backed unit tests pinning the request shape:
  1. `post_question` — verifies the URL path, auth header, and JSON body.
  2. `poll_thread_for_human_reply` — verifies that a fixture response
     containing one bot message + one human reply yields exactly the human
     reply and that a fixture with only the bot's own post yields `None`.
- `provider_name()` returns the expected string.
- `is_experimental()` returns `true` (except Slack, which returns `false`).

That's the floor. Real-world correctness emerges as operators report bugs.

## Risks / Trade-offs

- **Risk:** Provider API drift breaks experimental backends silently — the
  test suite uses fixtures, not live calls.
  - **Mitigation:** the loud startup warning, the README's explicit
    "no-stability-guarantees" language, and the expectation that operators
    on experimental backends file bugs when they encounter live-service
    breakage. Promoting any backend to "officially supported" later is the
    moment we add monitoring or canary calls; not before.

- **Risk:** Schema churn — renaming `slack:` to `chatops:` and
  `slack_channel_id` to `chatops_channel_id` breaks any existing config.
  - **Mitigation:** the only deployment is the author's own; this is the
    explicit window to do the rename rather than carrying both names.
    Documented in the README's migration paragraph.

- **Risk:** Trait surface picks up provider-specific concerns (rate-limit
  hints, formatting hints, attachment support) as the experimental impls
  grow.
  - **Mitigation:** keep the trait deliberately small in this change;
    refuse method additions until two impls demand them. The two methods
    here are exactly what `polling_loop.rs` consumes today.

- **Risk:** Teams OAuth client-creds token acquisition has a TTL and
  requires refresh — an extra dimension the other backends don't have.
  - **Mitigation:** `TeamsBackend` holds an `RwLock<Option<TokenCache>>`
    and lazily re-acquires on 401 or expiry; this complexity is contained
    in `teams.rs` and not exposed in the trait.

- **Risk:** Matrix's `event_id` format (`$...:server`) contains characters
  that need URL-encoding in subsequent requests; easy to get wrong.
  - **Mitigation:** the `urlencode` helper already in `chatops.rs` for the
    Slack-channel param moves into `mod.rs` and gets reused; tested by the
    impl's own mockito tests.
