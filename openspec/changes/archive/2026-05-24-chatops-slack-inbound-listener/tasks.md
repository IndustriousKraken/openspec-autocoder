## 1. Config: app-level token

- [x] 1.1 Extend `ChatOpsSlackConfig` in `autocoder/src/config.rs` with an optional `app_token` / `app_token_env` pair, following the existing `bot_token` / `bot_token_env` pattern (inline value OR env-var indirection, resolved via the existing `SecretSource` machinery).
- [x] 1.2 Validation: if `app_token` is present, it MUST start with `xapp-` (Slack convention); if `bot_token` is present, it MUST start with `xoxb-`. Both checks are warnings at load time, not hard failures, since Slack could in principle change the prefix in the future.
- [x] 1.3 Tests: valid app_token-via-env, valid app_token inline, missing-env-var resolution error, prefix-warning case.

## 2. `Reply` enum, dispatcher return-type change, and minimum-privilege surface

- [x] 2.1 Introduce `pub enum Reply { Sync(String), Acked { ack_text: String, job_id: uuid::Uuid } }` in `autocoder/src/chatops/operator_commands.rs`.
- [x] 2.2 Introduce `pub struct RepoIdentity { pub url: String, pub workspace_path: PathBuf }` in the same module. The dispatcher receives `&[RepoIdentity]` instead of `&[RepositoryConfig]` — URL for substring matching, workspace_path for action submission. No tokens, channel IDs, or other config fields are visible to the dispatcher or to anything it calls.
- [x] 2.3 Change `OperatorCommandDispatcher::handle_message`'s return type from `String` to `Option<Reply>` and its repos parameter from `&[RepositoryConfig]` to `&[RepoIdentity]`:
  - `None` — message did not parse as a known verb (caller reacts with `?` emoji)
  - `Some(Reply::Sync(text))` — every v1 verb returns this
  - `Some(Reply::Acked { .. })` — reserved for future async verbs; v1 never constructs one
- [x] 2.4 Update every call site (44 existing dispatcher tests + the test fixture in `control_socket.rs`) to construct `RepoIdentity` values and match against `Option<Reply>`. Existing assertions on reply text become assertions on `Some(Reply::Sync(text))` with the same text.
- [x] 2.5 Add the new `help` verb to the parser. `@<bot> help` returns a `Sync` reply listing the verb set, syntax, and a pointer to the README for the destructive-confirmation flow.
- [x] 2.6 Argument sanitization at parser entry:
  - Change-slug args (`clear-perma-stuck`, `clear-revision`) must match `^[a-zA-Z0-9_-]{1,64}$`. Malformed → `Some(Reply::Sync("✗ invalid change name (must match ^[a-zA-Z0-9_-]+$, max 64 chars)".into()))`.
  - Repo-substring args (every verb) must match `^[a-zA-Z0-9._/-]{1,128}$`. Malformed → `Some(Reply::Sync("✗ invalid repo substring (must match ^[a-zA-Z0-9._/-]+$, max 128 chars)".into()))`.
  - The sanitization runs BEFORE any file path construction or control-socket dispatch.
- [x] 2.7 Tests:
  - `help` verb (matches `help`, `Help`, `HELP`; case-insensitive like other verbs).
  - `Option<Reply>` return contract (unrecognized → `None`; recognized → `Some(Sync)`).
  - Argument-sanitization rejects: `../etc/passwd` as change name; change name with shell metachars (`a; rm -rf /`); change name > 64 chars; repo substring with `..`; repo substring > 128 chars.
  - Argument-sanitization accepts the full real-world set: `a06-foo`, `auth-2fa`, kebab-and-digit slugs, repo substrings containing `/` and `.` and `_`.

## 3. `ChatOpsBackend` trait: inbound capability

- [x] 3.1 Extend `ChatOpsBackend` with `async fn start_inbound_listener(&self, dispatcher: Arc<OperatorCommandDispatcher>, repos: Arc<dyn RepoIdentityProvider>, allowed_channels: Arc<HashSet<String>>, cancel: CancellationToken) -> Result<JoinHandle<()>>`. Default impl returns `Err(anyhow!("backend `{}` does not support inbound messages", self.provider_name()))` so the existing experimental backends compile without changes.
- [x] 3.2 Define `pub trait RepoIdentityProvider: Send + Sync { fn snapshot(&self) -> Vec<RepoIdentity>; }`. Implemented by a thin newtype over the existing per-repo `ArcSwap` map. The newtype's `snapshot()` method projects each `RepositoryConfig` to a `RepoIdentity` (URL + workspace_path only) so the listener never receives the full config. The projection lives in the newtype, not in user code, so the minimum-privilege boundary is enforced by construction.
- [x] 3.3 Add `async fn post_threaded_reply(&self, channel: &str, thread_ts: &str, text: &str) -> Result<()>` and `async fn add_reaction(&self, channel: &str, message_ts: &str, name: &str) -> Result<()>` to the trait. Default impls return `Err("unsupported")`. Slack overrides both.
- [x] 3.4 Tests:
  - The default unsupported impls (verifies the error text contains the provider name).
  - `RepoIdentityProvider` projection drops everything except URL and workspace_path (assert the type returned is `RepoIdentity`, not `RepositoryConfig` — a compile-time guarantee).

## 4. Slack Socket Mode client

- [x] 4.1 Add `tokio-tungstenite = "<latest>"` and `futures-util = "<latest>"` to `Cargo.toml` after verifying current versions via crates.io (per the `check-current-versions-not-training` rule).
- [x] 4.2 Implement `slack::open_socket_mode_url(app_token) -> Result<String>`: POST `apps.connections.open` with `Authorization: Bearer <app_token>`, parse the `url` field from the response.
- [x] 4.3 Implement `slack::connect_socket_mode(url) -> Result<WebSocketStream>`: use `tokio-tungstenite::connect_async`. Return the connected stream.
- [x] 4.4 Define the Socket Mode envelope types:
  ```rust
  enum SocketMessage {
      Hello,                        // {"type":"hello", ...} — sent once after connect
      EventsApi(EventsApiPayload),  // {"type":"events_api","envelope_id":..., "payload":{"event":{...}}}
      Disconnect,                   // {"type":"disconnect","reason":"..."}
  }
  ```
  with `serde` derive for `Deserialize`.
- [x] 4.5 Implement the `app_mention` event handler with the layered drop-before-dispatch filters:
  1. **Channel allowlist**: if `channel` is not in the passed-in `allowed_channels` set, drop silently (no reaction, no log beyond DEBUG). This keeps the bot invisible in channels it's not authorized to command from.
  2. **Self-author filter**: if `user == self.bot_user_id`, drop silently. WARN-log once per such event since the bot mentioning itself is an unexpected state worth surfacing.
  3. **Bot-author filter**: if the envelope's `payload.event.bot_id` is `Some(_)` OR `payload.event.subtype == Some("bot_message")`, drop silently. WARN-log the event with the originating `bot_id` since this is the indirect-injection scenario worth alerting on.
  4. **Leading-mention check**: after trimming leading whitespace, the first token of `text` must be `<@{self.bot_user_id}>`. If not, drop silently.
  5. After all filters pass: pass `text` + `channel` + the cached `bot_user_id` (formatted as `<@U...>`) + `repos.snapshot()` into the dispatcher; route the returned `Option<Reply>` to either `post_threaded_reply` (`Sync` and `Acked.ack_text`) or `add_reaction` (`None` → `?`).
- [x] 4.6 Implement the ack protocol: after handling an event, send `{"envelope_id":"...", "no_ack":false}` over the WebSocket so Slack does not redeliver.
- [x] 4.7 Tests:
  - Envelope deserialization (hello, events_api with app_mention, disconnect).
  - `app_mention` → dispatcher → outbound mapping (using mockito for the Slack HTTP side and a fake WebSocket stream for the inbound side).
  - Ack envelope construction.
  - Each filter independently rejects: channel not in allowlist (silent drop, no `post_threaded_reply`, no `add_reaction` calls); message authored by self (silent drop + WARN); message with `bot_id: Some(...)` (silent drop + WARN); message with `subtype: "bot_message"` (silent drop + WARN); mention not at start (silent drop).
  - End-to-end injection test: synthesize an envelope whose `text` is a literal valid command (`@<bot> wipe-workspace evil`) but whose `bot_id` field is `Some("B999")`. Assert no dispatcher call, no submitter call, no `post_threaded_reply`, no `add_reaction`.

## 5. Reconnect + lifecycle

- [x] 5.1 The inbound listener task runs an outer `loop` that calls `open_socket_mode_url` + `connect_socket_mode`, runs the event loop until the stream errors or a `disconnect` envelope arrives, then sleeps for the current backoff and retries. Backoff: 1s, 2s, 4s, 8s, 16s, 30s (cap). On a successful event roundtrip, backoff resets to 1s.
- [x] 5.2 The event loop is itself a `tokio::select!` racing `cancel.cancelled()` against `stream.next()`. On cancel: close the WebSocket cleanly (send Close frame), break the outer loop, return.
- [x] 5.3 Each reconnect cycle logs INFO (`slack inbound: connecting`, `slack inbound: connected`, `slack inbound: disconnected — reason: ...`). Backoff waits log DEBUG.
- [x] 5.4 Tests:
  - Cancel during a connected event loop exits within 1s (event-driven, no sleep — fire the cancel after a Notify-signaled "connected" point, the same pattern as `cancellation_during_sleep_exits` in `polling_loop.rs`).
  - Disconnect envelope triggers reconnect (drive a fake stream that delivers disconnect, then a hello, assert both connection cycles ran).
  - Backoff cap (drive a stream that fails immediately N times, assert the wait never exceeds 30s).

## 6. Daemon wiring + channel allowlist construction

- [x] 6.1 In `cli/run.rs`, after constructing the chatops backend, check whether the resolved config has an `app_token`. If yes, build the `allowed_channels: HashSet<String>` as: every distinct `repositories[].chatops_channel_id` + the global `chatops.slack.default_channel_id` (if set) + every entry in the new optional `chatops.slack.listen_channels` list. Spawn the inbound listener task via the new trait method, pass the global cancel token, store the JoinHandle alongside the control-socket handle for graceful shutdown.
- [x] 6.2 If no `app_token` is configured, log a one-shot WARN: `"chatops inbound listener not started: chatops.slack.app_token not configured. Operator commands like '@<bot> status <repo>' will not receive replies. See <README section> for setup."`
- [x] 6.3 If `app_token` is configured but `allowed_channels` is empty (no chatops_channel_id on any repo, no default, no listen_channels), log a one-shot WARN: `"chatops inbound listener: no channels in allowlist. The bot will be connected but will silently drop every command. Configure at least one chatops_channel_id on a repository, or set chatops.slack.listen_channels."` and still spawn the listener — the operator may add config later via reload.
- [x] 6.4 On shutdown (cancel fires), await the inbound listener's JoinHandle alongside the polling tasks and the control-socket task; log error if it panicked.

## 7. README + config-reference updates

- [x] 7.1 Update the README's "ChatOps operator commands" section: add a "Setup" subsection covering the app-level token (Slack app config → Socket Mode → generate token), the required OAuth scopes (`app_mentions:read`, `chat:write`, `reactions:write`), and the config snippet (`chatops.slack.app_token_env: SLACK_APP_TOKEN`).
- [x] 7.2 Update the README's command-table to mention threaded replies and the `?`-reaction-on-unknown behavior.
- [x] 7.3 Document the new `help` verb in the command table.

## 8. Spec delta

- [x] 8.1 The ADDED requirements in `openspec/changes/chatops-slack-inbound-listener/specs/chatops-manager/spec.md` cover: the inbound-listener trait method, the `Reply` enum contract, the Slack Socket Mode connection lifecycle, the `app_mention`-only subscription scope, the threaded-reply rule, the `?`-reaction-on-unknown rule, the reconnect-with-backoff contract.

## 9. Verification

- [x] 9.1 `cargo test` passes (new + existing). All 44 existing operator-commands tests adapted to the `Option<Reply>` return type continue to pass.
- [x] 9.2 `openspec validate chatops-slack-inbound-listener --strict` passes.
- [x] 9.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
