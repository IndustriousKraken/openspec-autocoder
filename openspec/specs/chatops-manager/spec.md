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
- **WHEN** the orchestrator calls `write_question_file(workspace, change, payload)`
- **THEN** the manager writes a JSON document containing at least `thread_ts`, `channel`, `resume_handle`, and `asked_at` to `<workspace>/openspec/changes/<change>/.question.json`
- **AND** the write is performed via tempfile-then-rename in the same directory so a partially-written file is never observable

#### Scenario: Writing an answer file
- **WHEN** the orchestrator calls `write_answer_file(workspace, change, payload)`
- **THEN** the manager writes a JSON document containing at least `answer`, `answered_at`, and `answerer_user_id` to `<workspace>/openspec/changes/<change>/.answer.json`
- **AND** the write is atomic by the same mechanism

#### Scenario: Deleting state files is idempotent
- **WHEN** `delete_question_file(workspace, change)` or `delete_answer_file(workspace, change)` is called
- **THEN** the file is removed if it exists
- **AND** no error is returned if the file is already absent

### Requirement: Post a one-way notification
The chatops-manager SHALL expose a `post_notification(channel, text)`
method distinct from `post_question`. The method SHALL post the given
text to the channel without returning a thread/reply handle. The
method's contract is one-way: there is no expectation that callers
will track or read replies to a notification.

#### Scenario: Notification posts to Slack with no return handle
- **WHEN** `post_notification(channel, text)` is called against
  `SlackBackend`
- **THEN** the backend issues an HTTP POST to
  `https://slack.com/api/chat.postMessage` with the text in the
  `text` JSON field
- **AND** the method returns `Ok(())` on success (no thread handle
  is exposed to the caller)
- **AND** on a 2xx response with `ok: false`, the method returns an
  error whose text contains the Slack `error` field verbatim
- **AND** on a non-2xx response, the method returns an error whose
  text contains the HTTP status

#### Scenario: Notification posting is independent of question state
- **WHEN** the manager has an in-flight `post_question` thread for a
  given channel AND `post_notification(channel, text)` is called
- **THEN** the notification posts as a new top-level message in the
  same channel, NOT as a threaded reply to the in-flight question
- **AND** the notification's emission does NOT affect any
  `poll_thread_for_human_reply` poll in progress

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

### Requirement: Inbound listener trait method
The `ChatOpsBackend` trait SHALL expose an `async fn start_inbound_listener(&self, dispatcher: Arc<OperatorCommandDispatcher>, repos: Arc<dyn RepoSnapshotProvider>, cancel: CancellationToken) -> Result<JoinHandle<()>>` method. The default implementation SHALL return an error whose text identifies the backend by name and states that inbound listening is unsupported, so backends that do not implement an inbound listener compile and run unchanged. Only `SlackBackend` overrides the default in this change.

#### Scenario: Default implementation errors with backend name
- **WHEN** a backend that has not overridden `start_inbound_listener` is asked to start one
- **THEN** the call returns `Err` whose text contains the value of `provider_name()` and the word `unsupported`
- **AND** no listener task is spawned

#### Scenario: SlackBackend overrides the default
- **WHEN** `SlackBackend::start_inbound_listener` is called with a valid Socket Mode `app_token` configured
- **THEN** the call returns `Ok(JoinHandle)` for the spawned listener task
- **AND** the listener begins the connect / event-loop cycle described in the Slack Socket Mode lifecycle requirement

### Requirement: Reply enum contract
The dispatcher SHALL return `Option<Reply>` from `handle_message`, where `Reply` is an enum with two variants: `Sync(String)` for immediate textual responses, and `Acked { ack_text: String, job_id: uuid::Uuid }` for future async commands that ack immediately and post their completion later. `None` SHALL mean the message did not parse as a known verb. The listener SHALL act on each return value as: `None` → react with `?` emoji on the original message; `Sync(text)` → post `text` as a threaded reply on the original message; `Acked { ack_text, job_id }` → post `ack_text` as a threaded reply on the original message and register `job_id` with the listener's completion channel for a later follow-up post.

#### Scenario: None triggers question-mark reaction, no text reply
- **WHEN** the dispatcher returns `None` for an inbound message at `(channel, message_ts)`
- **THEN** the listener calls `add_reaction(channel, message_ts, "question")`
- **AND** the listener does NOT call `post_threaded_reply`

#### Scenario: Sync triggers threaded reply
- **WHEN** the dispatcher returns `Some(Reply::Sync(text))` for an inbound message at `(channel, message_ts)`
- **THEN** the listener calls `post_threaded_reply(channel, message_ts, text)`
- **AND** the listener does NOT call `add_reaction`

#### Scenario: Acked triggers ack reply and registers completion
- **WHEN** the dispatcher returns `Some(Reply::Acked { ack_text, job_id })` for an inbound message at `(channel, message_ts)`
- **THEN** the listener calls `post_threaded_reply(channel, message_ts, ack_text)`
- **AND** the listener registers `(job_id, channel, message_ts)` so that a later completion event for `job_id` posts a follow-up threaded reply at the same `(channel, message_ts)`

### Requirement: Slack Socket Mode connection lifecycle
The Slack inbound listener SHALL obtain a WebSocket URL via `POST https://slack.com/api/apps.connections.open` using the configured app-level token, connect via WebSocket, and remain connected until either the daemon's `cancel` fires or the stream errors. On stream error or Slack `disconnect` envelope, the listener SHALL reconnect with exponential backoff starting at 1 second, doubling, capped at 30 seconds. A successful event roundtrip SHALL reset the backoff to 1 second. On cancel, the listener SHALL close the WebSocket cleanly and return.

#### Scenario: apps.connections.open is called with the app-level token
- **WHEN** the listener starts
- **THEN** it issues `POST https://slack.com/api/apps.connections.open` with `Authorization: Bearer <app_token>`
- **AND** on `ok: true` parses the response's `url` field as the WebSocket URL
- **AND** on `ok: false` returns an error whose text contains the Slack `error` field verbatim

#### Scenario: Disconnect envelope triggers reconnect with backoff
- **WHEN** the listener receives a `{"type":"disconnect", ...}` envelope
- **THEN** the listener closes the current stream
- **AND** waits `backoff_secs` (starting at 1, doubling on each successive failure)
- **AND** issues a new `apps.connections.open` + connect cycle

#### Scenario: Successful event resets the backoff
- **WHEN** the listener has reconnected after one or more failures and successfully processes at least one event
- **THEN** the next reconnect after a future disconnect waits 1 second, not the doubled previous backoff

#### Scenario: Backoff caps at 30 seconds
- **WHEN** the listener has experienced enough consecutive failures that `1 * 2^N` would exceed 30
- **THEN** the wait is capped at 30 seconds

#### Scenario: Cancel exits within 1 second
- **WHEN** the daemon's root cancel token fires while the listener is connected to Slack
- **THEN** the listener closes the WebSocket within 1 second and its `JoinHandle` resolves

### Requirement: app_mention-only subscription
The Slack inbound listener SHALL handle only Slack `app_mention` events from the Socket Mode stream. The Slack app's Events API subscription SHALL be configured for `app_mention` only (operators configure this in the Slack app dashboard; the daemon does not configure it). Other event types received over the WebSocket SHALL be acknowledged (so Slack does not redeliver them) but otherwise ignored.

#### Scenario: app_mention with all filters passing is dispatched
- **WHEN** an `events_api` envelope arrives with `payload.event.type == "app_mention"` AND the message passes every drop-before-dispatch filter (channel allowlist, self-author, bot-author, leading-mention)
- **THEN** the listener extracts `text`, `channel`, `ts`
- **AND** calls `dispatcher.handle_message(text, channel, bot_mention, repos.snapshot(), submitter)`
- **AND** routes the returned `Option<Reply>` per the Reply enum contract

#### Scenario: Other event types are acked but otherwise ignored
- **WHEN** an `events_api` envelope arrives with `payload.event.type != "app_mention"`
- **THEN** the listener sends the Socket Mode ack envelope `{"envelope_id": "...", "no_ack": false}`
- **AND** does NOT call the dispatcher

### Requirement: Drop-before-dispatch inbound filters
The Slack inbound listener SHALL apply four drop-before-dispatch filters to every `app_mention` event in this fixed order. Any filter that rejects an event SHALL cause the listener to ack the envelope (so Slack does not redeliver) and stop processing — it SHALL NOT call the dispatcher, post a reply, or react. Filters that drop a message because of an unexpected condition (self-author, bot-author) SHALL emit a WARN-level log so the operator can investigate; the channel-allowlist and leading-mention drops are routine and log at DEBUG.

#### Scenario: Channel-allowlist filter drops messages from non-allowlisted channels
- **WHEN** an `app_mention` arrives with `channel` not in the listener's `allowed_channels` set
- **THEN** the envelope is acked
- **AND** the dispatcher is NOT called
- **AND** no reply or reaction is posted
- **AND** a DEBUG log records the drop with the rejected channel

#### Scenario: Self-author filter drops messages authored by the bot itself
- **WHEN** an `app_mention` arrives with `user == self.bot_user_id`
- **THEN** the envelope is acked
- **AND** the dispatcher is NOT called
- **AND** no reply or reaction is posted
- **AND** a WARN log records the drop (the bot mentioning itself is an unexpected state)

#### Scenario: Bot-author filter drops messages authored by any bot
- **WHEN** an `app_mention` arrives with `bot_id == Some(_)` OR `subtype == Some("bot_message")`
- **THEN** the envelope is acked
- **AND** the dispatcher is NOT called
- **AND** no reply or reaction is posted
- **AND** a WARN log records the drop with the originating `bot_id` (this is the indirect-injection scenario worth surfacing — e.g. a supply-chain attack causing another bot in the channel to post a command-shaped message)

#### Scenario: Leading-mention filter drops messages where the bot mention is not the first token
- **WHEN** an `app_mention` arrives whose `text`, after trimming leading whitespace, does NOT begin with `<@{self.bot_user_id}>`
- **THEN** the envelope is acked
- **AND** the dispatcher is NOT called
- **AND** no reply or reaction is posted
- **AND** a DEBUG log records the drop

#### Scenario: Indirect-injection end-to-end is blocked
- **WHEN** an `app_mention` arrives whose `text` is a literal valid command (e.g. `"<@UBOT> wipe-workspace evil"`) but whose `bot_id` field is `Some("B999")`
- **THEN** the bot-author filter drops the event
- **AND** the dispatcher is never called
- **AND** no submitter call is ever made
- **AND** no `post_threaded_reply` or `add_reaction` call is made

### Requirement: Minimum-privilege dispatcher surface
The dispatcher SHALL receive `&[RepoIdentity]` rather than `&[RepositoryConfig]`. `RepoIdentity` SHALL contain exactly two fields: `url: String` and `workspace_path: PathBuf`. The `RepoIdentityProvider` trait SHALL be the sole construction path for these values; the trait's implementation SHALL project from `RepositoryConfig` so the dispatcher never holds — and cannot accidentally observe — tokens, channel IDs, audit settings, scheduling fields, or any other config not strictly required for substring matching and action submission.

#### Scenario: RepoIdentity contains only url and workspace_path
- **WHEN** `RepoIdentityProvider::snapshot()` is called
- **THEN** every returned `RepoIdentity` has exactly the `url` and `workspace_path` fields populated
- **AND** the type itself (compile-time) carries no other field — adding a new field to `RepositoryConfig` does NOT automatically widen what the dispatcher can see

#### Scenario: Dispatcher signature is RepoIdentity, not RepositoryConfig
- **WHEN** the dispatcher's `handle_message` signature is inspected
- **THEN** the repos parameter type is `&[RepoIdentity]`
- **AND** the dispatcher module does NOT import `RepositoryConfig`

### Requirement: Argument sanitization at parser entry
The parser SHALL sanitize every operator-supplied argument before passing it to file-path construction or control-socket dispatch. Change-slug arguments SHALL match `^[a-zA-Z0-9_-]{1,64}$`; repo-substring arguments SHALL match `^[a-zA-Z0-9._/-]{1,128}$`. Malformed arguments SHALL produce `Some(Reply::Sync("✗ invalid <field>: ..."))` and SHALL NOT result in any file-system or control-socket call.

#### Scenario: Path-traversal in change name is rejected
- **WHEN** `handle_message("<@UBOT> clear-perma-stuck myrepo ../../etc/passwd", ...)` is called
- **THEN** the return value is `Some(Reply::Sync(text))` where `text` begins with `✗ invalid change name`
- **AND** no control-socket submission is performed
- **AND** no `std::fs::*` call is made

#### Scenario: Shell metacharacter in change name is rejected
- **WHEN** `handle_message("<@UBOT> clear-perma-stuck myrepo a; rm -rf /", ...)` is called
- **THEN** the return value is `Some(Reply::Sync(text))` where `text` begins with `✗ invalid change name`
- **AND** no control-socket submission is performed

#### Scenario: Oversized argument is rejected
- **WHEN** a change name with more than 64 characters is supplied
- **THEN** the return value is `Some(Reply::Sync(text))` where `text` begins with `✗ invalid change name`

#### Scenario: Valid arguments pass through
- **WHEN** valid arguments such as change name `a06-foo` and repo substring `your-org/your-repo` are supplied
- **THEN** the parser returns the recognized `OperatorCommand` variant
- **AND** the dispatcher proceeds normally

### Requirement: Help verb returns the verb list
The dispatcher SHALL recognize `@<bot> help` (case-insensitive) as a verb and return `Some(Reply::Sync(text))` where `text` enumerates every currently-supported verb, its syntax, and a one-line description, plus a one-line pointer to the README's confirmation-flow section for the destructive verbs.

#### Scenario: help returns a multi-line synopsis
- **WHEN** `handle_message("@<bot> help", ...)` is called
- **THEN** the return value is `Some(Reply::Sync(text))`
- **AND** `text` contains the strings `status`, `clear-perma-stuck`, `clear-revision`, `wipe-workspace`, `rebuild-specs`, and `help` (the current verb set)

#### Scenario: help is case-insensitive
- **WHEN** `handle_message("@<bot> HELP", ...)` is called
- **THEN** the return value is `Some(Reply::Sync(text))` matching the lowercase form

### Requirement: Status reply always shows live workspace snapshot
The `status` verb's reply SHALL always include five sections regardless of whether the repo has any markers, throttled alerts, or queued changes: (1) `branches: base=<base>, agent=<agent>`; (2) one `last commit on <branch>` line per branch (base and agent), each rendering as `<short_sha> "<subject>" (<age> ago)` when a commit exists or `(none)` when the branch does not exist or has no commits; (3) `latest PR: ...` with a URL on the following line when a PR exists from the agent branch, or `latest PR: (none)` otherwise; (4) `currently: idle` OR `currently: working on <change> (started <age> ago)` based on the per-repo busy marker; (5) the existing `next iteration: in <age> ...` line. These sections SHALL precede the existing marker / throttled-alert / queue sections.

#### Scenario: All sections present for a healthy repo
- **WHEN** an operator issues `status <repo>` against a repo with commits on both branches, an open PR from the agent branch, an idle daemon, and an empty queue
- **THEN** the reply contains all five always-present sections in the documented order
- **AND** the `currently:` line reads `idle`
- **AND** the queue section either reads `queue: 0 pending, 0 waiting, 0 excluded` (one-liner form) or is omitted entirely per the queue-one-liner requirement

#### Scenario: Absent data renders `(none)`, not blank or missing
- **WHEN** the agent branch does not exist yet (fresh clone)
- **THEN** `last commit on <agent_branch>:` reads `(none)`
- **AND** the line is still present (the section is always shown)

#### Scenario: GitHub failure does not break the reply
- **WHEN** the GitHub API call for `latest PR` returns an error (network failure, 4xx, 5xx, rate-limit)
- **THEN** the daemon logs a WARN with the underlying error
- **AND** the reply's `latest PR:` line reads `(none)`
- **AND** every other section is rendered normally
- **AND** the status reply succeeds — the operator gets the local-state half even when GitHub is unreachable

#### Scenario: Local git failure does not break the reply
- **WHEN** `git log -1` returns an error (workspace not yet cloned, .git directory corrupt)
- **THEN** the daemon logs a WARN with the underlying error
- **AND** the affected `last commit on <branch>:` line reads `(none)`
- **AND** every other section is rendered normally

#### Scenario: Currently-busy line reflects the live busy marker
- **WHEN** the daemon is mid-iteration on change `a05-foo` started 2 minutes ago
- **THEN** the `currently:` line reads `working on a05-foo (started 2m ago)`
- **AND** the busy-marker file is read but NOT taken, held, or released by the status path

### Requirement: Queue one-liner for small queues
When `pending_changes`, `waiting_changes`, and the marker-excluded set each contain 5 or fewer entries, the status reply SHALL render the queue as a single line: `queue: N pending (<list>), M waiting (<list>), K excluded`. When any of those lists exceeds 5 entries, the reply SHALL fall back to the existing per-line format (one line per change). Empty lists in the one-liner form SHALL render as `N pending` (no parenthetical) rather than `0 pending ()`.

#### Scenario: All three lists are small → one-liner
- **WHEN** the queue has 2 pending, 1 waiting, 0 excluded changes
- **THEN** the queue section is rendered as one line: `queue: 2 pending (a06-foo, a07-bar), 1 waiting (a10-secrets), 0 excluded`

#### Scenario: A list exceeds 5 entries → per-line fallback
- **WHEN** `pending_changes` has 6 entries
- **THEN** the queue section is rendered in the existing per-line format (one line per change, grouped by status)

#### Scenario: Empty list renders count only
- **WHEN** the queue has 0 pending and the threshold path applies
- **THEN** the one-liner contains `0 pending` (no empty parens)

### Requirement: Slack-escape user-controlled fields
The status formatter SHALL escape Slack-special characters (`<`, `>`, `&`) in every user-controlled string field before including it in the reply text. The escape substitutions SHALL be applied in the order `&` → `&amp;`, then `<` → `&lt;`, then `>` → `&gt;` so the substitution does not double-escape its own output. User-controlled fields in the status reply are: every commit subject, the PR title, and every change name. Operator-controlled or daemon-controlled fields (branch names from config, repo URLs, marker timestamps) are not escaped because they are not author-supplied.

#### Scenario: Commit subject with channel-mention is escaped
- **WHEN** a commit subject is the literal string `<!channel> ping everyone`
- **THEN** the reply contains the escaped form `&lt;!channel&gt; ping everyone`
- **AND** the reply does not contain the literal sequence `<!channel>` that would ping the channel when posted

#### Scenario: PR title with user-mention is escaped
- **WHEN** a PR title is the literal string `<@U123> please review`
- **THEN** the reply contains `&lt;@U123&gt; please review`
- **AND** Slack does NOT render this as a mention because the angle brackets are escaped

#### Scenario: Escape order avoids double-escape
- **WHEN** the input string is `&<`
- **THEN** the escaped output is `&amp;&lt;`
- **AND** the output is NOT `&amp;lt;` (which would be the result of escaping `<` first then `&`)

#### Scenario: Plain ampersand is escaped
- **WHEN** a commit subject contains `foo & bar`
- **THEN** the reply contains `foo &amp; bar`

### Requirement: ChatOpsBackend exposes a threaded-notification method with graceful degradation
The `ChatOpsBackend` trait SHALL expose `async fn post_notification_with_thread(&self, channel: &str, top_line: &str, thread_body: &str) -> Result<()>`. The trait's default implementation SHALL concatenate `top_line` + a blank-line separator + `thread_body` AND post the result via `post_notification` (no native threading). Backends with native threading support SHALL override the method with platform-appropriate threading. Backends without native threading continue working unchanged via the default impl.

#### Scenario: Default implementation concatenates for non-threading backends
- **WHEN** a backend that has not overridden `post_notification_with_thread` is asked to post one
- **THEN** the call results in exactly one `post_notification` invocation whose body contains `top_line`, then a blank line, then `thread_body`
- **AND** no platform-specific threading metadata is involved

#### Scenario: Slack override uses chat.postMessage + thread_ts
- **WHEN** `SlackBackend::post_notification_with_thread` is called with non-empty `top_line` and `thread_body`
- **THEN** the backend issues two HTTP POSTs to `chat.postMessage`
- **AND** the first POST's body is `{"channel": <channel>, "text": <top_line>}` and the response is parsed for the `ts` field
- **AND** the second POST's body is `{"channel": <channel>, "text": <thread_body>, "thread_ts": <captured_ts>}`
- **AND** the call returns Ok when both POSTs succeed

#### Scenario: Slack top-line failure aborts before threading
- **WHEN** the first `chat.postMessage` (the top-line) fails
- **THEN** the second POST is NOT attempted
- **AND** the function returns Err with the Slack error from the first call

#### Scenario: Slack thread-reply failure does not bubble up
- **WHEN** the first POST succeeds AND the second POST (the thread reply) fails
- **THEN** the function returns Ok (the top-line is the user-visible signal)
- **AND** a WARN log is emitted naming the missed thread reply

### Requirement: Audit findings post via the threaded-notification path when long enough to benefit
The audit scheduler SHALL route findings notifications through `post_notification_with_thread` when the body would benefit from threading: body line count > 3 OR body character count > 300. Below the threshold, findings inline into a single-message `post_notification` call. Empty findings posted under `notify_on_clean=true` use the inline path (`✅ <audit> on <repo>: no findings`); empty findings under `notify_on_clean=false` produce no notification at all (existing behaviour).

#### Scenario: Long findings post to a thread
- **WHEN** an audit produces findings whose body exceeds 3 lines OR 300 characters
- **THEN** the scheduler calls `post_notification_with_thread` with the audit-type's top-line summary AND the full findings body
- **AND** no separate `post_notification` call is made

#### Scenario: Short findings inline into the top-line
- **WHEN** an audit produces findings whose body is ≤3 lines AND ≤300 characters
- **THEN** the scheduler calls `post_notification` with the combined top-line + inline-body text
- **AND** no thread is created

#### Scenario: Empty findings with notify_on_clean=true posts the `✅` form inline
- **WHEN** an audit produces zero findings AND its `notify_on_clean` setting is `true`
- **THEN** the scheduler calls `post_notification` with the `✅ <audit_type> on <repo>: no findings` text
- **AND** no threaded reply is created (the body is empty; nothing to thread)

#### Scenario: Empty findings with notify_on_clean=false posts nothing
- **WHEN** an audit produces zero findings AND its `notify_on_clean` setting is `false`
- **THEN** no chatops call is made (existing behaviour preserved)

### Requirement: Audit top-line uses per-type emoji and audit-specific summary
The top-line of each audit notification SHALL be formatted per audit type so operators can scan the channel and immediately recognize the audit producing each message:

- `architecture_brightline`: `📐 architecture_brightline on <repo>: <N> file(s) over line threshold; <M> duplicate signature(s)`
- `drift_audit`: `🧭 drift_audit on <repo>: <N> spec/code divergence(s) detected`
- The proposal-creating audits (`missing_tests_audit`, `security_bug_audit`, `architecture_consultative`) use the `🔍 created proposal` form from `a02-audit-proposal-created-notification` (unchanged by this requirement; their notifications are already concise and do not need threading).

When an audit has zero findings AND `notify_on_clean=true`, the top-line is `✅ <audit_type> on <repo>: no findings` (uniform across audit types).

#### Scenario: Brightline summary names both counts
- **WHEN** an `architecture_brightline` notification fires with 7 files over threshold AND 3 duplicate signatures
- **THEN** the top-line is `📐 architecture_brightline on <repo>: 7 file(s) over line threshold; 3 duplicate signature(s)`

#### Scenario: Drift summary names the divergence count
- **WHEN** a `drift_audit` notification fires with 2 divergences detected
- **THEN** the top-line is `🧭 drift_audit on <repo>: 2 spec/code divergence(s) detected`

#### Scenario: No-findings top-line uses the `✅` form uniformly
- **WHEN** any audit fires with zero findings AND `notify_on_clean=true`
- **THEN** the top-line is `✅ <audit_type> on <repo>: no findings` regardless of audit type

### Requirement: Thread body truncates at 35,000 characters with a pointer to the daemon log
When the thread body would exceed 35,000 characters, it SHALL be truncated to 35,000 characters AND end with a marker pointing at the daemon log so operators can grep the full content. The 35,000 cap leaves a 5,000-character safety margin under Slack's per-message limit of 40,000.

#### Scenario: Body over 35k is truncated with the documented pointer
- **WHEN** the thread body would be 50,000 characters
- **THEN** the actual thread body posted is exactly 35,000 characters (or close to it; the truncation point is text-aware where reasonable) AND ends with `\n\n… [truncated; full findings at journalctl -u autocoder | grep audit_id=<audit_id>]`
- **AND** the `<audit_id>` is a deterministic identifier of the form `<repo-sanitized>:<audit-type>:<utc-timestamp>` that the audit-runner has stamped into its daemon-log entries for the same run

#### Scenario: Body under 35k is posted in full
- **WHEN** the thread body is 1,000 characters
- **THEN** the thread body is posted as-is with no truncation pointer

### Requirement: ValidationExhausted notifications use threading when the error is long
The `❌ <audit_type> produced an invalid proposal` notification from `a01-audit-proposal-self-validation` SHALL use the threaded-notification path when the validation error excerpt exceeds the threading threshold (>3 lines or >300 characters). The top-line names the audit, the repo, and the retry count; the thread body contains the full validation error. Short errors continue to inline into a single message.

#### Scenario: ValidationExhausted with multi-line error uses threading
- **WHEN** an audit returns `ValidationExhausted` with a `final_error` body exceeding the threading threshold
- **THEN** the scheduler routes the notification through `post_notification_with_thread`
- **AND** the top-line is `❌ <repo>: <audit_type> produced an invalid proposal that failed openspec validation after <retries_attempted> retries.`
- **AND** the thread body contains the full validation error excerpt

#### Scenario: ValidationExhausted with short error inlines
- **WHEN** an audit returns `ValidationExhausted` with a `final_error` body within the threading threshold
- **THEN** the scheduler routes the notification through `post_notification` (the existing inline path)

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

### Requirement: Bare `status` returns the per-repo menu
The chatops dispatcher SHALL recognise `@<bot> status` with no arguments as the `StatusMenu` command and SHALL return a `Sync` reply containing a one-line announcement plus one two-line section per configured repository. The existing `@<bot> status <repo-substring>` SHALL continue to behave as the per-repo deep-dive. Argument count after the verb token is the disambiguator: zero args → `StatusMenu`; one arg → `Status { repo_substring }`; two or more args → the existing "invalid" error.

#### Scenario: Bare status produces the menu reply
- **WHEN** an operator posts `@<bot> status` (no further arguments) in an allowlisted channel
- **THEN** the dispatcher returns `Some(Reply::Sync(text))` whose first line is `📊 Watching <N> repositories. Reply \`@<bot> status <repo-substring>\` for details.`
- **AND** the reply contains one section per configured repository

#### Scenario: Status with a substring still works
- **WHEN** an operator posts `@<bot> status myrepo`
- **THEN** the dispatcher returns the existing per-repo `Sync` reply
- **AND** the dispatcher does NOT return the menu reply

#### Scenario: Trailing whitespace and casing tolerated
- **WHEN** an operator posts `@<bot> Status   ` (trailing whitespace; verb in mixed case)
- **THEN** the message parses as `StatusMenu` and the menu reply is returned

#### Scenario: Empty configured-repos slice
- **WHEN** the daemon has zero configured repositories AND an operator posts `@<bot> status`
- **THEN** the reply is `📊 No repositories configured.`

### Requirement: Menu reply renders queue, busy, and last-iteration clauses per repo
Each section of the menu reply SHALL render the repo URL on its own line and a summary line containing three clauses joined by ` · `: a queue clause, a busy clause, and a last-iteration clause. Empty / zero values render as documented placeholders rather than blank fields. User-controlled fields (change names) pass through the Slack-escape helper before assembly.

#### Scenario: Idle empty-queue repo renders the empty-queue collapse
- **WHEN** a repo has zero pending, zero waiting, zero excluded, no busy marker, and a last iteration 5m ago
- **THEN** the summary line reads `empty queue · idle · last iteration 5m ago`

#### Scenario: Busy repo with pending entries
- **WHEN** a repo has 2 pending (`a06-foo`, `a07-bar`), 0 waiting, 0 excluded, busy marker on `a05-foo` started 2m ago, last iteration just now
- **THEN** the summary line reads `2 pending (a06-foo, a07-bar), 0 waiting, 0 excluded · working on a05-foo (started 2m ago) · last iteration just now`

#### Scenario: Pending-list truncates after 5 entries
- **WHEN** a repo has 7 pending entries (`a01`, `a02`, `a03`, `a04`, `a05`, `a06`, `a07`)
- **THEN** the queue clause renders `7 pending (a01, a02, a03, a04, a05 …+2 more)`

#### Scenario: Fresh daemon with no iteration history
- **WHEN** a repo's `last_iteration` is `None` (daemon just started)
- **THEN** the last-iteration clause reads `no iteration yet`

#### Scenario: User-controlled change name is Slack-escaped
- **WHEN** a change name passed in by the parser somehow contains `<` (despite the parser's allowlist — belt-and-braces)
- **THEN** the change name renders with the angle bracket escaped to `&lt;` in the menu reply

### Requirement: Partial-degradation: one repo's failure does not block the menu
When the dispatcher cannot assemble a complete `RepoStatusResponse` for a specific repository (control-socket call errored, repo-not-found, etc.), the menu SHALL still render every other repository's section normally AND SHALL render the failing repository's section with `(unavailable: <error excerpt>)` in place of the summary line. A WARN log is emitted for each unavailable repository.

#### Scenario: One repo unavailable, two healthy
- **WHEN** a three-repo daemon returns Ok for two of the three and Err for one
- **THEN** the menu reply contains three sections in total
- **AND** the two Ok sections render normally
- **AND** the one Err section renders `(unavailable: <error excerpt>)` in place of the summary line
- **AND** the URL line for the unavailable section is still present

#### Scenario: All repos unavailable
- **WHEN** every per-repo lookup errors
- **THEN** the menu reply contains one section per repo, each with `(unavailable: ...)`
- **AND** the leading announcement line still names the count

### Requirement: Help verb mentions the bare-status menu
The `help` verb's reply SHALL include a line documenting that `@<bot> status` with no repo argument returns the per-repo menu. The line distinguishes the two `status` forms so operators discovering the help text learn both modes.

#### Scenario: Help mentions bare status
- **WHEN** an operator posts `@<bot> help`
- **THEN** the reply text contains a phrase describing that bare `@<bot> status` returns the per-repo menu
- **AND** the reply text also mentions the per-repo form `@<bot> status <repo-substring>` for the detailed view

### Requirement: Wipe-workspace confirmation shows live repository context
The first-step warning message for `@<bot> wipe-workspace <repo>` SHALL include a context preview drawn from the same live data the per-repo `status` command surfaces. The preview names the workspace path being deleted, the currently-busy state (`idle` or `working on <change> (started <age> ago) — will be cancelled`), a one-line queue summary, and any active git-tracked operator markers that would persist across the wipe. Sections collapse when their underlying data is empty (no marker section when no markers exist; queue clause collapses to `empty queue` when all categories are zero). The trailing `Reply 'confirm' within 60 seconds to proceed.` line is unchanged.

#### Scenario: Confirmation message names the in-flight change when busy
- **WHEN** an operator posts `@<bot> wipe-workspace myrepo` AND the daemon is currently working on change `audit-proposal-self-validation` (busy marker present, started 5 minutes ago)
- **THEN** the first-step warning text contains `Currently: working on \`audit-proposal-self-validation\` (started 5m ago) — will be cancelled`
- **AND** the warning text contains the workspace path being deleted
- **AND** the warning text contains the queue clause

#### Scenario: Confirmation message reads `idle` when no iteration is in flight
- **WHEN** an operator posts `@<bot> wipe-workspace myrepo` AND no busy marker exists for the repo
- **THEN** the warning text contains `Currently: idle`
- **AND** the warning text does NOT contain a `— will be cancelled` clause

#### Scenario: Active markers section appears only when markers exist
- **WHEN** the repo has at least one `.perma-stuck.json` OR `.needs-spec-revision.json` marker file under any active or excluded change
- **THEN** the warning text contains an `Active markers (git-tracked; preserved across the wipe):` section listing each marker as `• <change> (<marker-file>)`
- **WHEN** the repo has no such markers
- **THEN** the warning text does NOT contain the active-markers section at all (no empty section, no `(none)` placeholder)

#### Scenario: Queue clause collapses to `empty queue` when all categories are zero
- **WHEN** the repo's pending, waiting, and excluded queue categories are all empty
- **THEN** the warning text's queue line reads `Queue (continues after wipe): empty queue`

#### Scenario: User-controlled fields are Slack-escaped
- **WHEN** a change name appearing in the queue clause OR the markers section contains a `<` character (despite the parser's allowlist; belt-and-braces)
- **THEN** the rendered warning text contains `&lt;` in place of the literal `<`

### Requirement: Wipe-workspace drains the in-flight iteration before deleting
On `confirm`, the daemon SHALL signal the per-repo polling task's per-iteration cancel token, await the per-repo `iteration_drained` Notify with a timeout of `executor.wipe_drain_timeout_secs` seconds (default 30, clamped at 300 with WARN), then perform the directory deletion. The deletion runs regardless of whether the drain completed within the timeout — the directory is going to be gone either way; the drain is a politeness, not a hard precondition. The reply text names which of four drain outcomes occurred so operators see at a glance whether the iteration drained cleanly or whether it was stuck enough to require force.

#### Scenario: Iteration drains cleanly within the timeout
- **WHEN** a wipe is confirmed AND the per-repo polling task has an in-flight iteration AND the iteration exits within `executor.wipe_drain_timeout_secs` of receiving the cancel signal
- **THEN** the success reply text contains `(drained cleanly in <Xs>)` where X is the elapsed seconds (one-decimal precision)
- **AND** the workspace directory is deleted after the drain
- **AND** no SIGTERM-shaped failure log entry (exit status 143) appears in `journalctl` for the cancelled iteration

#### Scenario: Drain timeout fires; wipe proceeds anyway
- **WHEN** a wipe is confirmed AND the in-flight iteration does NOT exit within the configured timeout
- **THEN** the success reply text contains `(drain timeout — iteration may have been stuck)`
- **AND** the workspace directory is deleted regardless of the drain not completing
- **AND** the daemon logs a WARN naming the stuck iteration's change for operator follow-up

#### Scenario: No iteration in flight short-circuits the drain
- **WHEN** a wipe is confirmed AND the per-repo polling task has no in-flight iteration (between iterations, in the inter-iteration sleep) AND the per-iteration cancel handle is `None`
- **THEN** the success reply text contains `(no iteration in flight)`
- **AND** no Notify is awaited; the wipe proceeds immediately to the directory deletion

#### Scenario: Workspace already absent renders the existing outcome
- **WHEN** a wipe is confirmed AND the workspace directory does not exist on disk AND no iteration is in flight
- **THEN** the success reply text contains `(already absent)` (the existing pre-this-change outcome wording is preserved for the idempotent no-op case)

#### Scenario: Per-iteration cancel does NOT propagate to the global cancel
- **WHEN** a wipe is confirmed AND the per-iteration cancel fires
- **THEN** only the in-flight iteration exits
- **AND** the per-repo polling task itself remains alive
- **AND** the global daemon-shutdown cancel token is not affected
- **AND** the next polling tick fires normally, observes the missing workspace, and re-clones via the existing `workspace::ensure_initialized` path

### Requirement: Slack inbound listener deduplicates redelivered events
The Slack inbound listener SHALL maintain an in-memory cache of recently-processed `app_mention` events keyed by `(channel, ts, user)` — the tuple that uniquely identifies a Slack message regardless of how many times Slack delivers it across envelopes or reconnects. Before dispatching an event (after the drop-before-dispatch filters return Pass), the listener SHALL look up the event's key in the cache. A cache hit SHALL skip the dispatch entirely; the listener still sends the envelope ack (so Slack stops redelivering) but does NOT post a reply, submit a control-socket action, or otherwise execute the operator's intent a second time.

#### Scenario: First delivery dispatches normally; subsequent redeliveries are suppressed
- **WHEN** the listener receives an `app_mention` event with `(channel=C, ts=T, user=U)` for the first time AND the event passes all drop-before-dispatch filters
- **THEN** the dedup cache returns `Fresh`
- **AND** the dispatcher is invoked
- **AND** the cache records the key
- **WHEN** the listener receives the same event again (Slack redelivery)
- **THEN** the dedup cache returns `Duplicate { suppressed_count: 1 }`
- **AND** the dispatcher is NOT invoked
- **AND** no chatops reply is posted
- **AND** an INFO log records the suppression

#### Scenario: Multiple redeliveries increment the suppressed count
- **WHEN** the same event is redelivered three times after the initial delivery
- **THEN** the cache returns `Duplicate { suppressed_count: 1 }`, then `2`, then `3` for each subsequent call
- **AND** the dispatcher is invoked exactly once (for the initial delivery)
- **AND** three INFO logs are emitted (one per suppression) with monotonically increasing suppressed_count values

#### Scenario: Different events do not collide
- **WHEN** the listener receives events with different `(channel, ts, user)` tuples
- **THEN** each event's first delivery returns `Fresh`
- **AND** the dispatcher is invoked for each
- **AND** no suppression occurs

#### Scenario: Cache persists across listener reconnect cycles
- **WHEN** the listener processes an event AND then the WebSocket disconnects AND the listener reconnects
- **AND** Slack redelivers the same event on the new connection
- **THEN** the dedup cache returns `Duplicate { suppressed_count: 1 }` (the cache persists across reconnect; the dedup decision is preserved)
- **AND** the dispatcher is NOT invoked

### Requirement: Dedup cache has bounded capacity and TTL with operator-configurable knobs
The dedup cache SHALL enforce both a maximum capacity (LRU eviction past the cap) AND a per-entry TTL (entries older than TTL are treated as Fresh on next lookup). Default capacity is `100`; default TTL is `600` seconds (10 minutes). Configurable via `chatops.slack.dedup_cache_capacity` (max `10000` with WARN-and-clamp) AND `chatops.slack.dedup_cache_ttl_secs` (max `3600` with WARN-and-clamp). Capacity `0` is permitted AND disables dedup behaviorally (every event is treated as Fresh; legacy pre-this-spec behavior).

#### Scenario: Capacity bound enforced via LRU eviction
- **WHEN** the cache has capacity `2` AND three distinct keys are inserted in succession
- **THEN** the first-inserted key is evicted to make room for the third
- **AND** a subsequent lookup of the first key returns `Fresh` (it's no longer in the cache)

#### Scenario: TTL bound treats stale entries as fresh
- **WHEN** a key is inserted into the cache AND `ttl_secs + 1` seconds pass AND the same key is looked up again
- **THEN** the cache returns `Fresh` (the stale entry is treated as not-present)
- **AND** the entry is replaced with a new insertion

#### Scenario: Capacity 0 disables dedup
- **WHEN** `chatops.slack.dedup_cache_capacity` is set to `0`
- **THEN** every `check_and_insert` call returns `Fresh`
- **AND** the dispatcher is invoked for every event regardless of past deliveries (today's pre-spec behavior is preserved verbatim)

#### Scenario: Out-of-bounds config values are clamped with WARN
- **WHEN** `chatops.slack.dedup_cache_capacity` is set to `50000`
- **THEN** the resolved capacity is `10000`
- **AND** a WARN log fires at startup naming both the requested and clamped values
- **WHEN** `chatops.slack.dedup_cache_ttl_secs` is set to `7200`
- **THEN** the resolved TTL is `3600`
- **AND** a WARN log fires at startup

### Requirement: Dedup suppression is logged at INFO with the key and suppressed count
Each cache hit SHALL emit a single INFO log line naming the dedup key fields AND the running suppressed-count for that key. Operators investigating "the bot didn't reply to my message" can grep journalctl for `deduplicated event` lines AND confirm whether their message was received-and-suppressed (vs not received at all OR dropped by a different filter).

#### Scenario: Suppression log format includes the dedup key and count
- **WHEN** a duplicate event is suppressed by the dedup cache
- **THEN** an INFO log fires with text containing `deduplicated event`, the channel ID, the ts, the user ID, AND the `suppressed_count` value
- **AND** the log uses the field naming convention `channel=<channel> ts=<ts> user=<user> suppressed_count=<n>` consistent with other slack-inbound listener log lines

