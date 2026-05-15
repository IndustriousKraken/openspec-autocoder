# chatops-manager Specification

## Purpose
TBD - created by archiving change chatops-escalation. Update Purpose after archive.
## Requirements
### Requirement: Post escalation question to Slack
The chatops-manager SHALL expose a `post_question(channel, change, question)`
method on the `ChatOpsBackend` trait. Each concrete backend SHALL post a
human-readable question to its provider, prefixed with `❓ <change>:`, and
SHALL return an opaque string handle that subsequent reply-polls reference.

#### Scenario: Slack impl posts to chat.postMessage
- **WHEN** the active backend is `SlackBackend` and the polling loop calls
  `post_question(channel, change, question)`
- **THEN** the backend issues an HTTP POST to
  `https://slack.com/api/chat.postMessage` with header
  `Authorization: Bearer <token>` (token from
  `chatops.slack.bot_token_env`) and a JSON body containing `channel`,
  `text` beginning with `❓ \`<change>\`:` followed by the question, and
  `link_names: 1`
- **AND** on a 2xx response with `ok: true`, the backend returns the
  response's `ts` field as the handle string
- **AND** on `ok: false`, the backend returns an error whose text contains
  the Slack `error` field verbatim
- **AND** on a non-2xx response, the backend returns an error whose text
  contains the HTTP status code

#### Scenario: SlackBackend identifies as official
- **WHEN** `is_experimental()` is called on `SlackBackend`
- **THEN** it returns `false`
- **AND** `provider_name()` returns `"slack"`

### Requirement: Identify the bot's own Slack user id
Each backend SHALL learn its own provider user id at construction time so
subsequent reply detection can distinguish bot messages from human replies.

#### Scenario: SlackBackend learns its user id from auth.test
- **WHEN** `SlackBackend::new(bot_token)` is invoked
- **THEN** it issues an HTTP POST to `https://slack.com/api/auth.test`
- **AND** on a 2xx response with `ok: true`, caches `user_id` internally
- **AND** on any other response, returns an error whose text contains the
  Slack `error` field (or HTTP status if non-2xx)

#### Scenario: Each experimental backend learns its identity via its own provider
- **WHEN** any experimental backend is constructed
- **THEN** it issues the provider's identity call (Discord:
  `GET /users/@me`; Teams: derives identity from the OAuth client_id;
  Mattermost: `GET /api/v4/users/me`; Matrix:
  `GET /_matrix/client/v3/account/whoami`) and caches the result
- **AND** on identity-call failure the backend's constructor returns an
  error whose text names the provider and the failing call

### Requirement: Poll Slack thread for first non-bot reply
The `poll_thread_for_human_reply(channel, handle)` method SHALL return the
earliest message in the thread/reference identified by `handle` whose
author is not the bot itself, or `None` if no such message is present.

#### Scenario: Slack thread contains only the bot's posting
- **WHEN** the active backend is `SlackBackend` AND the only message in
  the thread is the bot's own posting
- **THEN** the backend returns `None`

#### Scenario: Slack thread contains a human reply
- **WHEN** the active backend is `SlackBackend` AND the thread contains
  at least one message whose `bot_id` field is absent AND whose `user`
  field differs from the cached bot user id
- **THEN** the backend returns `Some(HumanReply { text, user_id, ts })` for
  the EARLIEST such message
- **AND** the original posting message is never returned even if it
  appears first in the array

#### Scenario: Bot-self-filter is per-backend
- **WHEN** an experimental backend's poll method runs against a fixture
  thread containing one bot message + one human reply
- **THEN** the backend returns the human reply
- **AND** the bot's own message is never returned, using whatever
  bot-self-marker the provider exposes (Discord: `author.bot`; Teams:
  `from.user.id` equality against bot; Mattermost: `user_id` equality
  against bot; Matrix: `sender` equality against access-token owner)

### Requirement: Atomic and idempotent state-file management
The chatops-manager SHALL provide read, write, and delete helpers for the `.question.json` and `.answer.json` files inside change directories. Writes MUST be atomic; deletes MUST be idempotent.

#### Scenario: Writing a question file
- **WHEN** autocoder calls `write_question_file(workspace, change, payload)`
- **THEN** the manager writes a JSON document containing at least `thread_ts`, `channel`, `resume_handle`, and `asked_at` to `<workspace>/openspec/changes/<change>/.question.json`
- **AND** the write is performed via tempfile-then-rename in the same directory so a partially-written file is never observable

#### Scenario: Writing an answer file
- **WHEN** autocoder calls `write_answer_file(workspace, change, payload)`
- **THEN** the manager writes a JSON document containing at least `answer`, `answered_at`, and `answerer_user_id` to `<workspace>/openspec/changes/<change>/.answer.json`
- **AND** the write is atomic by the same mechanism

#### Scenario: Deleting state files is idempotent
- **WHEN** `delete_question_file(workspace, change)` or `delete_answer_file(workspace, change)` is called
- **THEN** the file is removed if it exists
- **AND** no error is returned if the file is already absent

### Requirement: ChatOpsBackend trait abstracts provider-specific code
The chatops-manager SHALL expose a `ChatOpsBackend` trait that the polling
loop consumes. Concrete provider implementations (Slack, Discord, Teams,
Mattermost, Matrix) SHALL be loaded behind this trait through a single
startup factory. The trait SHALL declare exactly the methods the polling
loop calls today: `provider_name`, `is_experimental`, `post_question`,
`poll_thread_for_human_reply`.

#### Scenario: Polling loop holds a trait object, not a concrete type
- **WHEN** the polling loop's `ChatOpsContext` is constructed
- **THEN** its `chatops` field is typed as `Arc<dyn ChatOpsBackend>` rather
  than `Arc<SlackBackend>` or any other concrete provider type
- **AND** the polling loop calls only `post_question` and
  `poll_thread_for_human_reply` on it; no provider-specific method is
  reachable from the loop

#### Scenario: Factory dispatches on `chatops.provider`
- **WHEN** `cli::run::execute` loads a config whose `chatops.provider` is set
- **THEN** the manager's factory returns the matching concrete backend
  wrapped in `Arc<dyn ChatOpsBackend>`
- **AND** if the matching `chatops.<provider>:` sub-block is absent, the
  factory returns an error whose text names both the selected provider and
  the missing sub-block

### Requirement: Discord backend conformance
The chatops-manager SHALL provide a `DiscordBackend` implementing
`ChatOpsBackend`. Authentication uses an `Authorization: Bot <token>`
header where the token comes from the env var named in
`chatops.discord.bot_token_env`. Reply threading uses the
`message_reference.message_id` field of subsequent messages in the channel.

#### Scenario: Posting via Discord
- **WHEN** `post_question(channel, change, question)` is called
- **THEN** the backend issues `POST /channels/{channel}/messages` against
  the Discord API base with a JSON body containing `content` (formatted as
  `❓ \`<change>\`: <question>`)
- **AND** on a 2xx response, the backend returns the message's `id` field
  as the opaque handle string
- **AND** on any non-2xx response, the backend returns an error containing
  the HTTP status

#### Scenario: Polling for a Discord reply
- **WHEN** `poll_thread_for_human_reply(channel, handle)` is called and the
  channel contains a subsequent message whose `message_reference.message_id`
  equals `handle` AND whose `author.bot` field is `false`
- **THEN** the backend returns `Some(HumanReply)` with `text` from
  `content`, `user_id` from `author.id`, and `ts` from the message `id`
- **AND** if no such reply exists, the backend returns `None`

#### Scenario: Discord identifies as experimental
- **WHEN** `is_experimental()` is called on `DiscordBackend`
- **THEN** it returns `true`
- **AND** `provider_name()` returns `"discord"`

### Requirement: Teams backend conformance
The chatops-manager SHALL provide a `TeamsBackend` implementing
`ChatOpsBackend` against the Microsoft Graph API. Authentication uses
OAuth `client_credentials` against
`https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token` with
`client_id`, `client_secret` (env-sourced), and scope
`https://graph.microsoft.com/.default`. The acquired access token is
cached in-process and re-acquired on 401 or expiry.

#### Scenario: Posting via Teams
- **WHEN** `post_question(channel, change, question)` is called
- **THEN** the backend issues `POST /teams/{team_id}/channels/{channel}/messages`
  against `https://graph.microsoft.com/v1.0` with a JSON body containing
  `body.content` (formatted as `❓ <code>change</code>: question`) and
  `body.contentType: html`
- **AND** on a 2xx response, the backend returns the message's `id` field
  as the opaque handle string

#### Scenario: Polling for a Teams reply
- **WHEN** `poll_thread_for_human_reply(channel, handle)` is called
- **THEN** the backend issues
  `GET /teams/{team_id}/channels/{channel}/messages/{handle}/replies`
- **AND** returns `Some(HumanReply)` for the earliest reply whose `from.user`
  is present and differs from the bot's identity (resolved from the OAuth
  client_id at construction)

#### Scenario: Teams identifies as experimental
- **WHEN** `is_experimental()` is called on `TeamsBackend`
- **THEN** it returns `true`
- **AND** `provider_name()` returns `"teams"`

### Requirement: Mattermost backend conformance
The chatops-manager SHALL provide a `MattermostBackend` implementing
`ChatOpsBackend`. Authentication uses an `Authorization: Bearer <token>`
header where the token comes from the env var named in
`chatops.mattermost.access_token_env`. Reply threading uses the `root_id`
field.

#### Scenario: Posting via Mattermost
- **WHEN** `post_question(channel, change, question)` is called
- **THEN** the backend issues `POST {server_url}/api/v4/posts` with a JSON
  body containing `channel_id` and `message` (formatted as
  `❓ \`<change>\`: <question>`)
- **AND** on a 2xx response, the backend returns the post's `id` field as
  the opaque handle string

#### Scenario: Polling for a Mattermost reply
- **WHEN** `poll_thread_for_human_reply(channel, handle)` is called
- **THEN** the backend issues
  `GET {server_url}/api/v4/posts/{handle}/thread`
- **AND** returns `Some(HumanReply)` for the earliest message in the
  `posts` array whose `root_id == handle` AND whose `user_id` differs from
  the bot user resolved at construction

#### Scenario: Mattermost identifies as experimental
- **WHEN** `is_experimental()` is called on `MattermostBackend`
- **THEN** it returns `true`
- **AND** `provider_name()` returns `"mattermost"`

### Requirement: Matrix backend conformance
The chatops-manager SHALL provide a `MatrixBackend` implementing
`ChatOpsBackend` against the Matrix Client-Server API. Authentication uses
the access token from the env var named in
`chatops.matrix.access_token_env` passed as an `Authorization: Bearer`
header. Reply threading uses `m.relates_to.m.in_reply_to.event_id` per the
Matrix spec.

#### Scenario: Posting via Matrix
- **WHEN** `post_question(channel, change, question)` is called (channel
  is the Matrix room id, e.g. `!abc:server.tld`)
- **THEN** the backend issues
  `PUT {homeserver_url}/_matrix/client/v3/rooms/{room}/send/m.room.message/{txn_id}`
  where `{txn_id}` is a UUIDv4 unique per call, with a JSON body containing
  `msgtype: "m.text"` and `body` (formatted as `❓ <change>: <question>`)
- **AND** on a 2xx response, the backend returns the response's `event_id`
  field as the opaque handle string

#### Scenario: Polling for a Matrix reply
- **WHEN** `poll_thread_for_human_reply(channel, handle)` is called
- **THEN** the backend issues
  `GET {homeserver_url}/_matrix/client/v3/rooms/{room}/messages?from=...&dir=f`
  starting from a `from` token obtained at construction
- **AND** returns `Some(HumanReply)` for the earliest event whose
  `content.m.relates_to.m.in_reply_to.event_id` equals `handle` AND whose
  `sender` differs from the access-token owner's user id (resolved via
  `GET /_matrix/client/v3/account/whoami` at construction)

#### Scenario: Matrix identifies as experimental
- **WHEN** `is_experimental()` is called on `MatrixBackend`
- **THEN** it returns `true`
- **AND** `provider_name()` returns `"matrix"`

