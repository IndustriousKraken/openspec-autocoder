## ADDED Requirements

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
