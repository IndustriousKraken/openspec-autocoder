# ChatOps Escalation

When the optional `chatops:` config block is present, autocoder routes ambiguous agent outcomes (executor returning `AskUser`) to a human via chat-thread replies, persists the conversation state to disk, and resumes implementation on the next iteration when an answer arrives. **Slack is the officially-supported provider**; Discord, Teams, Mattermost, and Matrix are available as [experimental backends](CHATOPS.md#experimental-chatops-backends) with no API-stability guarantees.

## Configuring Slack (officially supported)

```yaml
chatops:
  provider: slack
  default_channel_id: C0123456789       # fallback channel id (use the Slack channel ID, not the name)
  slack:
    bot_token_env: SLACK_BOT_TOKEN      # env var containing your xoxb-... bot token
    # OR — inline alternative; when `bot_token` is set, `bot_token_env` is ignored.
    # bot_token:
    #   value: "xoxb-yourtokenhere"
```

The inline form follows the same dual-source pattern as `github.token` and `reviewer.api_key`; see [Secrets in `config.yaml`](SECURITY.md#5-secrets-in-configyaml-inline-vs-env-var) for the security tradeoff.

Per-repo override:

```yaml
repositories:
  - url: "git@github.com:my-org/auth-service.git"
    # ...
    chatops_channel_id: C0AUTH_CHANNEL  # this repo posts to a different channel
```

## Progress notifications

In addition to escalation, autocoder posts two **operator-facing** notification streams to the same chatops channel — a low-volume activity feed so a channel-watching operator can tell at a glance whether the daemon is alive and what it is doing.

**Start-of-work** — one line per change pickup:

```
🚀 `<repo-url>`: starting work on `<change-name>` — <first line of ## Why>
```

Fires immediately after the change's `.in-progress` lock is created and BEFORE the executor is invoked.

**Throttled failure alerts** — emitted at most once every 24 hours per (repository, failure category) for three categories of *predictable* infrastructure failure: workspace init / clone failure, branch push rejection, and PR creation 4xx from GitHub. Format:

```
⚠️ `<repo-url>`: <category-label> for the past 24h. Latest: <error excerpt>
```

The 24h throttle state lives in a per-workspace `.alert-state.json` file. On the next successful iteration the file is removed, so a transient outage followed by recovery does not leave the next failure (whenever it occurs) silenced.

Other failure surfaces — executor returning `Failed`, reviewer LLM call errors, the chatops post itself failing — are deliberately out of scope and never produce a categorized alert.

Configure independently under `chatops.notifications`:

```yaml
chatops:
  # existing fields...
  notifications:
    start_work: true       # default true; one message per change pickup
    failure_alerts: true   # default true; throttled per (repo, category)
    pr_opened: true        # default true; one message per opened PR (with link)
```

All three keys are optional. An absent `notifications:` block parses to "all true" — first-time deployments see useful chatops traffic without further configuration. Set a key to `false` to suppress that stream without affecting the others.

If `post_notification` itself fails (network blip, channel renamed, scope revoked), the failure is logged to stderr but is NEVER re-routed back through chatops — there is no recursive alert cascade.

**Revision cap notifications.** The PR-comment revision channel (see
[OPERATIONS.md](OPERATIONS.md#revising-an-open-pr-via-comment)) emits a
one-time chatops notification when an open PR hits its revision cap:

```
🛑 <repo-url>: PR #<num> hit the revision cap of <N>. Further revision requests ignored.
```

This fires alongside the one-time `🛑 Revision cap reached` PR comment.
Subsequent triggering comments on the same PR are silently ignored — the
one chatops line is the operator's only out-of-band signal that the PR
has stopped accepting revisions. The notification is not gated by the
`failure_alerts` switch (it is a one-shot per PR, not a throttled
infrastructure alert).

## Required Slack bot scopes

A **private channel** is the recommended deployment — it keeps non-operators from prompting the agent. The Slack app's bot token must have:

- `chat:write` — post the escalation message into the channel.
- `groups:history` — read thread replies in private channels (use `channels:history` instead if you deploy against a public channel).

`auth.test` is scope-less, so the bot's identity check at startup needs nothing further. `users:read` is not required — reply attribution is by Slack user id only.

After installing the app, invite the bot to the channel (`/invite @YourAppName`); otherwise `chat.postMessage` returns `not_in_channel`.

## What gets posted

When an executor returns `AskUser { question, resume_handle }`, the daemon posts to the resolved channel:

```
❓ `<change-name>`: <question text>
```

The resulting Slack message's thread timestamp + the executor's opaque resume handle are persisted to `<workspace>/openspec/changes/<change-name>/.question.json`. The agent's `.in-progress` lock is removed, so the change moves from "in flight" to "waiting on human."

## How reply detection works

On every polling iteration, BEFORE considering pending changes for that repository, the daemon:

1. Calls `queue::list_waiting(workspace)` to find all `.question.json`-bearing changes.
2. For each, GETs `conversations.replies` on the tracked thread.
3. The **first message** that has no `bot_id` field AND whose `user` differs from autocoder's own bot user id is treated as the human's answer.
4. The daemon writes `.answer.json`, deletes `.question.json`, calls `executor.resume(handle, answer)`, and handles the new outcome like a fresh run (commit + archive on `Completed`, escalate again on a second `AskUser`, log + revert to pending on `Failed`).

## Same-repo queue blocking

A change waiting on a human answer in repository X blocks ALL pending-change processing for repository X. This preserves the architecture's serial-queue invariant: when change A asks a question, change B (which may depend on A's restructuring) is NOT processed until A is resolved. Cross-repo polling tasks are independent — repository Y continues to be serviced.

## Operator escape hatches for a stuck waiting change

If a Slack reply never arrives, autocoder does not time out — it waits indefinitely. Three operator-controlled ways to unblock:

1. **Reply in Slack** — the original thread is still tracked. Send any non-bot message in that thread; the next polling iteration resumes the change.
2. **Manually delete `.question.json`** — reverts the change to pending state. The next iteration re-runs it from scratch (without the answer). Useful when the question was a false positive or the change should restart.
3. **`autocoder rewind <change>`** — full reset: deletes the agent branch, unarchives if needed, clears all `.question.json` / `.answer.json` markers via the rewind path.

## `.question.json`, `.answer.json`, and `.alert-state.json` as workspace artifacts

These files are written by autocoder into the workspace as bookkeeping. `.question.json` and `.answer.json` live alongside the change's `proposal.md`; `.alert-state.json` lives at the workspace root and tracks the per-(repo, category) 24h-alert throttle for [progress notifications](CHATOPS.md#progress-notifications).

All three are safe to inspect (plain JSON) but unsafe to modify by hand — atomic writes via temp-file-then-rename mean they're consistent on disk, but the daemon's state machine assumes it owns their lifecycle. When a change is archived, the directory move takes the change-scoped marker files with it; `.alert-state.json` is cleared whenever the polling pass completes without hitting any of the three predictable-failure sites.

Deleting `.alert-state.json` by hand is harmless: it just resets the alert throttle window for that repository, so the next predictable failure will alert immediately rather than wait out the 24h window.

## ChatOps operator commands

A small set of operator-issued commands lets you handle the common SSH-and-edit recovery actions from chat instead of switching to a terminal. Every reply is posted as a **threaded reply** to your original `@<bot> <verb>` message — the channel stays clean and the conversation stays grouped near the request. Messages that don't parse as a known verb get a `?`-emoji reaction on the original message rather than a text reply, so typos and drive-by mentions do not spam the channel.

The bot recognises:

| Verb | Syntax | What it does |
| --- | --- | --- |
| `status` | `@<bot> status <repo-substring>` | Posts a multi-line threaded reply with five always-present sections — branches, last commit on each branch, latest PR from the agent branch, currently-busy state (`idle` or `working on <change>`), and the next-iteration estimate — followed by any active markers, currently-engaged 24h alert throttles, and the queue snapshot (compact one-liner when small, per-line when any list exceeds five entries). When called without `<repo-substring>`, returns a per-repo menu listing every watched repository. |
| `clear-perma-stuck` | `@<bot> clear-perma-stuck <repo-substring> <change-slug>` | Deletes `openspec/changes/<change>/.perma-stuck.json`. The next iteration will retry the change. |
| `clear-revision` | `@<bot> clear-revision <repo-substring> <change-slug>` | Deletes `openspec/changes/<change>/.needs-spec-revision.json`. Use after you've edited `tasks.md` to remove or revise the unimplementable tasks. |
| `wipe-workspace` | `@<bot> wipe-workspace <repo-substring>` | Destructive: removes the entire `/tmp/workspaces/<sanitized-url>/` directory so the next iteration re-clones. Requires two-step confirmation (see below). |
| `rebuild-specs` | `@<bot> rebuild-specs <repo-substring>` | Schedules a full canonical-spec rebuild from archive history. The rebuild runs on the next polling iteration; the resulting commits land via the usual push + PR flow. See [Rebuilding canonical specs from archive history](OPERATIONS.md#rebuilding-canonical-specs-from-archive-history). |
| `help` | `@<bot> help` | Posts a threaded synopsis of every recognised verb with its syntax and a one-line description. |

The `clear-perma-stuck` and `clear-revision` verbs are the in-chat equivalent of the SSH-and-rm-the-file workflow described above — the same marker files that [perma-stuck](CHATOPS.md#operator-escape-hatches-for-a-stuck-waiting-change) and [needs-spec-revision](CHATOPS.md#what-gets-posted) recovery uses, deleted via a chat reply instead.

**Bare `status` — the per-repo menu.** When you don't remember the exact substring of a configured repo, type `@<bot> status` with no arguments. The bot returns a one-line announcement followed by one two-line section per watched repository (URL on top, summary on the next line). The summary has three clauses joined by ` · `: a queue clause (`empty queue` when all three counts are zero, otherwise `<N> pending (<list>), <M> waiting (<list>), <K> excluded` with each list truncating after 5 entries), a busy clause (`idle` or `working on <change> (started <age> ago)`), and a last-iteration clause (`last iteration <age> ago` or `no iteration yet`). Example:

```
📊 Watching 3 repositories. Reply `@<bot> status <repo-substring>` for details.

  • git@github.com:acme/widgets.git
    2 pending (a06-foo, a07-bar), 0 waiting, 0 excluded · idle · last iteration 3m ago

  • git@github.com:org-b/another.git
    empty queue · idle · last iteration 5m ago

  • git@github.com:personal/foo.git
    5 pending (a01, a02, a03, a04, a05 …+2 more), 1 waiting (a07-bar), 0 excluded · working on a05-foo (started 2m ago) · no iteration yet
```

If any individual repo's state cannot be assembled (workspace mid-failure, control-socket per-repo error), that repository's section renders `(unavailable: <error excerpt>)` in place of the summary line. The menu still ships every other repository's section so a single broken workspace doesn't blank the whole list. From the menu, pick a repo and re-issue `@<bot> status <substring>` for the full per-repo detail.

### Setup (Slack)

The outbound chatops surface (notifications, AskUser questions) needs only the bot token configured in [Configuring Slack](CHATOPS.md#configuring-slack-officially-supported). The inbound listener that receives `@<bot>` commands additionally requires a Slack **app-level token** with Socket Mode enabled. Without it, the daemon logs one WARN line at startup and the verbs in the table above do nothing — operator commands typed in chat will receive no reply and no reaction.

To enable the inbound listener:

1. In your browser, go to **https://api.slack.com/apps** and click your app. (Not `slack.com/apps` — that page is the user-facing install / marketplace view and has no configuration buttons.) Open **Settings → Socket Mode** and toggle it on. Slack will prompt you to generate an app-level token; give it the `connections:write` scope and copy the resulting `xapp-*` value.
2. In **Features → OAuth & Permissions → Bot Token Scopes**, ensure the bot has:
   - `app_mentions:read` — receive `app_mention` events over Socket Mode (the only event subscription you need).
   - `chat:write` — post the threaded reply.
   - `reactions:write` — add the `?` reaction on unrecognised messages.
   - the channel-history scope your channel deployment requires (`groups:history` for private channels, `channels:history` for public).
3. In **Features → Event Subscriptions**, enable events and subscribe the bot to `app_mention` only.
4. Reinstall the app to your workspace so the updated scopes apply.
5. Export the app-level token alongside the bot token and reference it from your config:

   ```yaml
   chatops:
     provider: slack
     default_channel_id: C0123456789
     slack:
       bot_token_env: SLACK_BOT_TOKEN
       app_token_env: SLACK_APP_TOKEN  # NEW — Socket Mode app-level token
   ```

   Inline values also work via the `{ value: "..." }` form, matching the existing `bot_token` pattern.
6. Restart the daemon. You should see the log line `slack inbound: connected` shortly after startup.

By default the inbound listener honours commands in any channel already used by the outbound side — the union of every `repositories[].chatops_channel_id` plus `chatops.default_channel_id`. Operators who want a separate listen-only channel add it to the optional `chatops.slack.listen_channels` list. Messages from channels outside this allowlist are silently dropped (no `?` reaction either — silent drop keeps the bot's presence invisible in channels it is not authorized to command from).

### Repo substring matching

You type the short name; the bot resolves it. The match is case-insensitive substring search against the full configured `repositories[].url`. `myrepo` matches `git@github.com:acme/myrepo.git`; `MYREPO` does too. If two repos with the same trailing name exist under different owners, the bot replies with the candidate list and asks for a more specific substring. If nothing matches, the bot replies with the full list of configured URLs so you can copy one back.

### Two-step confirmation for `wipe-workspace`

`wipe-workspace` is destructive, so the first reply is a warning rather than the action:

```
⚠️ This will delete /tmp/workspaces/github_com_acme_myrepo (forces a re-clone on the next iteration). Reply 'confirm' within 60 seconds.
```

To proceed, reply `confirm` (case-insensitive, no mention needed) within 60 seconds in the same channel. The confirmation is channel-scoped: a `confirm` in a different channel does NOT trigger a pending wipe somewhere else. If you wait longer than 60 seconds, the pending entry expires and you must re-issue the original `wipe-workspace` command.

### Reply shape

Success replies are one line beginning with `✓`. Error replies are one line beginning with `✗`. The `status` command is the only multi-line reply. Examples:

```
✓ cleared .perma-stuck.json for a06-foo on myrepo
✗ no perma-stuck marker for change a99-nonexistent on myrepo
✗ no repo matched 'gibberish'; configured: myrepo, widgets
```

The `status` reply for a healthy repo looks like:

```
📊 git@github.com:acme/myrepo.git

branches: base=main, agent=agent-q
last commit on main: 9f2c1aa "Merge pull request #41" (3h ago)
last commit on agent-q: 4d77b82 "implement a08-foo" (12m ago)

latest PR: #42 "a08-foo: add deployment hook"  open · head=agent-q · 11m ago
           https://github.com/acme/myrepo/pull/42

currently: working on a09-bar (started 2m ago)
queue: 1 pending (a10-baz), 0 waiting, 0 excluded
```

Branches and the busy-marker line are always present. `(none)` fills any always-present field whose underlying data is absent (fresh clone, no PR ever opened, etc.). If the GitHub API call fails or local `git log` errors, the affected line falls back to `(none)` and a WARN is logged — the reply still ships every other section so an operator can read the local-state half during a GitHub incident. The queue line uses the compact one-liner form when each of `pending` / `waiting` / `excluded` has ≤5 entries; larger lists fall back to the multi-line `queue snapshot:` format. Commit subjects and PR titles pass through a Slack-escape pass so author-supplied text like `<!channel>` cannot trigger channel-wide mentions when echoed into the reply.

### Unrecognised verbs get a `?` reaction, no text reply

Random chat that happens to mention the bot but doesn't match a known verb (typos, drive-by mentions, AskUser-thread replies, etc.) gets a single `?`-emoji reaction on the original message — no text reply, no thread spam. The reaction is a quiet "this didn't parse" signal: discoverable for the operator who typed the command, ignorable for everyone else. Type `@<bot> help` for the current verb list.

The verbs `pause`, `resume`, and `clear-alert-throttle` are intentionally not in this initial set. If your operator workflow needs them, file a follow-up issue describing the usage pattern.

### Trust boundary

Whoever has write access to the configured chatops channel is treated as an operator — the same trust boundary as the existing `AskUser` reply detection. Sites that need finer-grained control configure separate channels per concern via the existing per-repo `chatops_channel_id` override.

Under the hood, the chatops listener parses the command, resolves the repository, and submits a JSON action over the daemon's existing Unix-domain control socket (the same socket used by `autocoder reload`). The same actions are reachable from any future CLI subcommand without duplicating logic; the control socket's existing Unix-perms / daemon-user-only authentication applies identically.

---

## Experimental ChatOps Backends

> **No API-stability guarantees.** Discord, Microsoft Teams, Mattermost, and Matrix are implemented behind the same `ChatOpsBackend` trait as Slack but are explicitly marked experimental: their unit tests pin only the request shape against recorded fixture responses (not live services), so an upstream API change can break them silently. Each emits a loud `warn`-level startup log line stating "EXPERIMENTAL — best-effort support, may break without notice." If you select one and it stops working, **please file a bug**; that is how the experimental backends move toward official support.
>
> Slack remains the only officially-supported provider. Single-process autocoder runs against exactly one chat backend at a time; if you live on multiple platforms, pick the most-used one.

### Discord (representative walkthrough)

1. Create a Discord application at https://discord.com/developers/applications. Open the **Bot** tab and reveal the bot token (this is the value you'll export as an env var).
2. Under **OAuth2 → URL Generator**, check `bot` and the per-channel scopes (`Send Messages`, `Read Message History`). Use the generated URL to invite the bot to your server.
3. Get the **channel id** for the channel that should receive escalations (Discord → Settings → Advanced → enable Developer Mode → right-click the channel → Copy Channel ID).
4. Configure autocoder:

   ```yaml
   chatops:
     provider: discord
     default_channel_id: "123456789012345678"  # Discord channel snowflake
     discord:
       bot_token_env: DISCORD_BOT_TOKEN
   ```

5. Export the bot token at launch:

   ```bash
   export DISCORD_BOT_TOKEN="..."
   ./target/release/autocoder run --config config.yaml
   ```

   At startup you'll see:

   ```
   WARN EXPERIMENTAL: ChatOps escalation enabled via discord — best-effort support, may break without notice, no API-stability guarantees
   ```

When the executor returns `AskUser`, the bot posts `❓ \`<change>\`: <question>` to the channel. Replies are detected via Discord's `message_reference.message_id` field: any subsequent message in the channel that references the bot's original post and is authored by a non-bot user is treated as the human answer.

### Teams

Microsoft Graph + OAuth `client_credentials`. Register an app in Azure AD; grant it the `ChannelMessage.Send` and `ChannelMessage.ReadAll` application permissions; mint a client secret. Get the tenant id, application (client) id, and team id from Azure / Teams admin.

```yaml
chatops:
  provider: teams
  default_channel_id: "19:abc@thread.tacv2"   # Teams channel id (URL-encoded `:` and `@`)
  teams:
    tenant_id: "11111111-2222-3333-4444-555555555555"
    client_id: "66666666-7777-8888-9999-aaaaaaaaaaaa"
    client_secret_env: TEAMS_CLIENT_SECRET
    team_id: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff"
```

Reply threading uses `/messages/{id}/replies`. The OAuth token is cached in-process and re-acquired on 401 / expiry.

### Mattermost

Personal Access Token auth against the Mattermost v4 REST API. In Mattermost: System Console → Integrations → enable Personal Access Tokens; in your account, generate a PAT. Channel id is the alphanumeric segment in the URL.

```yaml
chatops:
  provider: mattermost
  default_channel_id: c1abcd...
  mattermost:
    server_url: "https://mattermost.example.com"
    access_token_env: MATTERMOST_TOKEN
```

Reply threading uses `root_id` on the post objects.

### Matrix

Bearer-token auth against the Matrix Client-Server API. In Element (or any Matrix client) get an access token via Settings → Help & About → Access Token, or log in via the API. Room id is the `!abc:server.tld`-style identifier (use the "Settings → Advanced" panel for an invited room).

```yaml
chatops:
  provider: matrix
  default_channel_id: "!abc:server.tld"
  matrix:
    homeserver_url: "https://matrix.example.com"
    access_token_env: MATRIX_ACCESS_TOKEN
```

Reply threading uses `m.relates_to.m.in_reply_to.event_id`. Initial sync establishes a `next_batch` token at startup so subsequent message reads only return newly-arrived events.

### IRC?

Out of scope. IRC has no stable, persistent message id (a `PRIVMSG` is fire-and-forget; reply correlation is heuristic), and the protocol assumes a long-lived TCP connection rather than HTTP request/response. Operators on IRC are pointed at the Matrix-IRC bridge run by most networks.

