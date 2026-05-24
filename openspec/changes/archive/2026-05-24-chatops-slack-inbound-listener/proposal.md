## Why

The `chatops-operator-commands` change shipped a parser, dispatcher, control-socket handlers, and 44 unit tests for verbs like `@autocoder status <repo>`, `@autocoder clear-perma-stuck <repo> <change>`, etc. It also shipped a README section documenting those verbs as a live operator interface.

It did not ship a Slack listener. The dispatcher exists; nothing in production feeds it a message. Operators who follow the README receive no reply and no log entry — the message never reaches the daemon because nothing is subscribed to channel-wide Slack events. The daemon's only inbound Slack path today is `conversations.replies` polling against specific question threads (the AskUser flow), which by design ignores everything else.

This is the missing piece. Without it, the entire operator-commands surface is documentation-only.

A secondary motivation: subsequent capabilities will want the same inbound channel. Two examples already in the user's mind:

- **Ad-hoc bug fix PRs** — an operator says `@autocoder fix the off-by-one in src/foo.rs:42`, the bot acks immediately, spawns an executor run against the named workspace, posts a follow-up message with the PR link when done.
- **Spec drafting** — `@autocoder draft a spec for adding X`, the bot acks, spawns an executor run that produces an OpenSpec change proposal + tasks, opens a PR with the new `openspec/changes/<name>/` tree.

Both are out-of-scope for this change. They are mentioned only to justify a design decision: the listener must support **both** synchronous reply ("here's your status") **and** async ack-then-callback ("on it; PR coming") response shapes from day 1. Designing only for synchronous replies forces a painful retrofit when the first async verb lands.

## What Changes

**Slack-only, Socket Mode.** The first inbound backend is Slack via Socket Mode (WebSocket-based, no public webhook URL needed). Other backends (Discord, Matrix, Mattermost, Teams) each get their own follow-up change. The chatops backend trait gains an inbound capability, but only `SlackBackend` implements it in v1; the experimental backends return `Unsupported` so existing configurations keep working unchanged.

**Subscription scope.** The Slack listener subscribes to `app_mention` events only. Messages not mentioning the bot are ignored at the Slack side (the Socket Mode subscription only delivers what the app subscribes to). This avoids any need to filter on the daemon side and keeps the operator's mental model simple: "the bot only sees what you @-mention it on."

**Threat model and defenses.** The trust boundary is "anyone with post access to the configured channels is trusted as an operator." Operators should not expose these channels to untrusted users. That said, the listener applies defense-in-depth so accidental misuse and indirect-injection scenarios stop early:

1. **Self-authored messages dropped.** Inbound messages whose `user == self.bot_user_id` are ignored before any parsing. The existing `poll_thread_for_human_reply` already does this for thread polls; the inbound listener applies the same filter.

2. **All bot-authored messages dropped.** Inbound messages with `bot_id.is_some()` OR `subtype == "bot_message"` are ignored before any parsing. This blocks the supply-chain scenario where a poisoned dependency in some repo causes the daemon (or any other bot in the channel) to post text matching `@autocoder <verb>`. Slack would deliver an `app_mention` event for such a post; the bot-author filter shuts that command-and-control path.

3. **Bot mention must be the leading token.** The bot mention `<@U...>` must be the first non-whitespace token of the message. A message whose body merely *contains* the mention (e.g. a quoted README line, a re-shared message) does not trigger.

4. **Channel allowlist, default-secure.** Commands are honored only in channels already used for outbound chatops — the union of every `repositories[].chatops_channel_id` plus the global default. Operators who want a separate listen-only channel add `chatops.slack.listen_channels: [<channel_id>, ...]` to extend the set. Messages in channels outside the allowlist are silently dropped (no `?` reaction either — silent drop keeps the bot's presence invisible in channels it is not authorized to command from).

5. **Argument sanitization.** Change slugs must match `^[a-zA-Z0-9_-]{1,64}$`; repo substrings must match `^[a-zA-Z0-9._/-]{1,128}$`. Malformed args reply with `✗ invalid <field>`. The listener never passes unsanitized strings to file path construction.

6. **Minimum-privilege dispatcher surface.** The dispatcher receives `Vec<RepoIdentity>` (URL + workspace path only), not `Vec<RepositoryConfig>`. The `RepositoryConfig` type contains scheduling, audits, and (in the future, potentially) other configuration that does not belong in the substring-matching codepath. This is a structural barrier so any future refactor that adds secrets-adjacent fields to `RepositoryConfig` does not accidentally widen what the dispatcher can see.

**Routing.** Inbound `app_mention` events are passed to `OperatorCommandDispatcher::handle_message` exactly as the existing tests already exercise (`message`, `channel_id`, `bot_mention`, `repos`, `submitter`). The dispatcher's return value drives the response.

**Response shape: typed `Reply` enum.** The dispatcher's return type changes from `String` (current) to a `Reply` enum:

```rust
pub enum Reply {
    /// Post `text` immediately as a threaded reply to the original message.
    Sync(String),
    /// Post `ack_text` immediately; the listener will receive a separate
    /// completion event for `job_id` and post the follow-up at that time.
    /// v1 has no async verbs — variant exists for forward compatibility so
    /// future ad-hoc-task verbs do not require listener retrofit.
    Acked { ack_text: String, job_id: uuid::Uuid },
}
```

All v1 verbs return `Sync`. The `Acked` variant is wired through the listener (it knows how to post both the ack and the eventual follow-up given a `job_id` and a completion channel) but no production code path constructs one yet. This is **not** a stub per `no_stubs_in_changes` — the variant is a pure-data type, fully serializable and pattern-matchable; what's deferred is the v1 set of *commands* that would emit it, not the variant's handling.

**Unrecognized messages: react, don't reply.** When the dispatcher returns `None` (message doesn't parse as a known verb), the listener posts a `?` reaction emoji on the original message. No text reply, no thread spam. Operators who type `@autocoder help` (a real verb introduced in this change) get the verb list as a `Sync` reply. The `?` reaction is purely a "this didn't parse" signal — discoverable, low-noise, ignorable.

**Replies are threaded.** All `Sync` replies post as a thread on the original message (`thread_ts: <original message ts>`). Single-line replies and the multi-line `status` output both go in the thread. The channel stays clean; the conversation stays grouped near the request.

**Listener lifecycle.** The Slack listener runs as a sibling task to the polling tasks and control-socket listener, owned by the same root `CancellationToken`. On Socket Mode disconnect, the listener reconnects with exponential backoff (capped at 30s) — the same pattern Slack's own SDK recommends. On cancel, the listener closes the WebSocket and exits.

**Config.** Socket Mode requires an *app-level token* (`xapp-*` prefix) in addition to the bot token already in config. Add a new optional field `chatops.slack.app_token` (or `app_token_env`) following the existing token-resolution pattern. If absent, the listener is not started and a one-shot WARN logs at daemon startup explaining what's missing — outbound chatops continues to work, only the inbound listener is disabled.

## Impact

- **Affected specs:** `chatops-manager` — ADDED requirement for the inbound listener capability and the `Reply` enum contract. No other capability changes.
- **Affected code:**
  - `autocoder/src/chatops/mod.rs` — extend `ChatOpsBackend` trait with `start_inbound_listener(dispatcher: Arc<OperatorCommandDispatcher>, ...) -> Result<JoinHandle<()>>`. Default impl returns `Err("unsupported")` so experimental backends compile without change.
  - `autocoder/src/chatops/slack.rs` — Socket Mode client (WebSocket via `tokio-tungstenite`), `apps.connections.open` to retrieve the wss URL, `app_mention` event handling (with the self / bot-author / leading-mention / channel-allowlist filters), threaded `chat.postMessage` for replies, `reactions.add` for the `?` reaction, reconnect-with-backoff.
  - `autocoder/src/chatops/operator_commands.rs` — change `handle_message` return type from `String` to `Option<Reply>` (None = unrecognized → caller reacts; Some(Sync) = post text; Some(Acked) = unused in v1 but wired). Add `help` verb. Replace the `Vec<RepositoryConfig>` parameter with `Vec<RepoIdentity>` (URL + workspace_path only). Add argument-sanitization checks at parser entry. Update existing tests.
  - `autocoder/src/cli/run.rs` — spawn the Slack inbound listener task at daemon start if `chatops.slack.app_token` is configured; otherwise WARN-and-skip.
  - `autocoder/src/config.rs` — `ChatOpsSlackConfig` gains optional `app_token` / `app_token_env` fields with the existing secret-source resolution.
  - `Cargo.toml` — add `tokio-tungstenite` (WebSocket client) + `futures-util` for the WebSocket sink/stream split.
  - Tests:
    - Unit tests for the Socket Mode envelope parsing (the JSON shape Slack sends over the WebSocket — `type: "events_api"` wrapping `app_mention`).
    - Unit tests for threaded-reply construction (correct `thread_ts`, correct text).
    - Unit tests for the `Reply` enum dispatch and the `?` reaction path.
    - Mockito-driven test of the full inbound→dispatcher→outbound cycle using a fake Socket Mode JSON stream.
- **Operator-visible behavior:** the README's "ChatOps operator commands" section starts working as documented. No verbs change shape. `@autocoder help` is new.
- **Breaking:** no. Existing chatops outbound behavior unchanged. Backends without an inbound impl continue to work. Sites without `app_token` configured continue to work (with one new WARN at startup).
- **Acceptance:** `cargo test` passes (new + existing). An operator pointing the daemon at a Slack workspace, mentioning `@autocoder status <repo>`, receives a threaded reply matching the README's documented shape within ~1 second. An unrecognized `@autocoder asdf` produces a `?` reaction on the operator's message and no text reply.
