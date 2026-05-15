## Why

Today autocoder's ChatOps escalation path is wired exclusively to Slack via the
`slack:` config block and a Slack-shaped `ChatOps` struct. Operators who work in
Discord, Microsoft Teams, Mattermost, or Matrix have no way to receive AskUser
escalations short of running a parallel Slack workspace, which is friction
disproportionate to the feature's value.

This change introduces a backend abstraction (`ChatOpsBackend`) and adds four
experimental, best-effort implementations alongside the existing Slack one.
Discord, Teams, Mattermost, and Matrix are explicitly marked as experimental:
they emit a loud startup warning, are documented under a separate README
section, and carry no API-stability or behavior guarantees. Slack remains the
only officially-supported backend.

The motivating use case: a solo operator who lives in Discord and uses
autocoder against their personal repos should be able to set
`chatops.provider: discord`, export a bot token, and have AskUser escalations
post to their Discord channel — accepting up front that the implementation may
break against API changes and that the operator is the one filing bugs when it
does.

## What Changes

- Introduce `ChatOpsBackend` trait abstracting the existing Slack interface:
  `post_question` and `poll_thread_for_human_reply` (the two methods the
  polling loop actually calls). The current Slack `ChatOps` struct is refit as
  one impl of this trait.
- Rename the `slack:` config block to `chatops:` and add a required
  `provider:` field. Supported values: `slack` (official), `discord`, `teams`,
  `mattermost`, `matrix` (all experimental).
- Add per-provider config sub-blocks (`chatops.slack:`, `chatops.discord:`,
  etc.). Only the sub-block matching the selected `provider:` is consulted at
  startup; others are tolerated for testing convenience but ignored.
- Add four new ChatOps backend implementations behind the trait:
  - **Discord** — Bot tokens via `Authorization: Bot <token>`; messages with
    `message_reference` for reply threading; polling via
    `/channels/{c}/messages?after=...` filtered by `message_reference`.
  - **Teams** — Microsoft Graph (`/teams/{t}/channels/{c}/messages` +
    `/replies`); OAuth client-credentials auth with tenant + client id +
    client secret.
  - **Mattermost** — Slack-shaped REST (`/api/v4/posts`); `root_id` for reply
    threading; PAT auth.
  - **Matrix** — Client-Server API (`PUT /rooms/{r}/send/m.room.message/...`,
    `GET /rooms/{r}/messages`); `m.relates_to.m.in_reply_to` for threading;
    access-token auth.
- Loud experimental warning: at startup, when `chatops.provider` is anything
  other than `slack`, emit one `tracing::warn!` line per repo that explicitly
  names the provider and states "EXPERIMENTAL: best-effort support; may break
  without notice; no API-stability guarantees."
- README: rename the "ChatOps Escalation" section to keep Slack as the
  documented official path; add a new "Experimental ChatOps Backends" section
  immediately after, walking through one Discord setup end-to-end as the
  representative example and pointing at the per-provider config keys for the
  other three.
- `RepositoryConfig.slack_channel_id` (the per-repo override) is renamed to
  `chatops_channel_id` to match the new generality. The old key is removed —
  there is no in-flight deployment outside the author's own to migrate.

## Capabilities

### Modified Capabilities

- `chatops-manager`: gains the `ChatOpsBackend` trait and four experimental
  backends; the existing Slack requirements are rewritten as the Slack-impl
  conformance contract under the trait rather than free-standing Slack
  requirements.
- `orchestrator-cli`: startup wiring switches from a Slack-specific construct
  to a provider-selecting factory that returns a `Box<dyn ChatOpsBackend>`;
  the experimental-provider warning happens here.

## Impact

Operators on Slack see no behavior change beyond the config-block rename
(`slack:` → `chatops:` with `provider: slack`). Operators on the other four
platforms gain a best-effort path that lets them use autocoder without
maintaining a parallel Slack workspace, in exchange for accepting that the
implementations are unreviewed against live services and may drift as
provider APIs change.

The experimental backends are written by autocoder itself implementing this
spec; each one's correctness floor is "compiles, passes its unit tests
against recorded fixture responses, and emits the experimental warning."
Live-service validation is the operator's responsibility per project
convention.
