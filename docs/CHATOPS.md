# ChatOps

ChatOps is autocoder's chat-facing surface. It serves two purposes:

- **Operator-driven workflows.** Verbs like `propose`, `send it`, `audit`, and `revise` (on a PR comment) drive end-to-end work — they kick off triage runs, on-demand audits, and PR revisions. These are the primary day-to-day operator interface.
- **Daemon-driven signal.** Progress notifications, threaded audit-finding posts, and the `AskUser` escalation mechanic keep operators in the loop without requiring SSH.

**Slack is the officially-supported provider.** Discord, Teams, Mattermost, and Matrix are available as [experimental backends](#experimental-chatops-backends) with no API-stability guarantees.

## Configuring Slack (outbound — required for any chat surface)

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

Per-repo channel override:

```yaml
repositories:
  - url: "git@github.com:my-org/auth-service.git"
    # ...
    chatops_channel_id: C0AUTH_CHANNEL  # this repo posts to a different channel
```

### Required outbound bot scopes

A **private channel** is the recommended deployment — it keeps non-operators from prompting the agent. The Slack app's bot token must have:

- `chat:write` — post the escalation message into the channel.
- `groups:history` — read thread replies in private channels (use `channels:history` instead if you deploy against a public channel).

`auth.test` is scope-less, so the bot's identity check at startup needs nothing further. `users:read` is not required — reply attribution is by Slack user id only.

After installing the app, invite the bot to the channel (`/invite @YourAppName`); otherwise `chat.postMessage` returns `not_in_channel`.

## Configuring the inbound listener (Socket Mode — required for operator verbs)

The outbound chatops surface (notifications, AskUser questions) needs only the bot token above. The inbound listener that receives `@<bot>` commands additionally requires a Slack **app-level token** with Socket Mode enabled. Without it, the daemon logs one WARN line at startup and every operator verb does nothing — typed commands receive no reply and no reaction.

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
       app_token_env: SLACK_APP_TOKEN  # Socket Mode app-level token
   ```

   Inline values also work via the `{ value: "..." }` form, matching the existing `bot_token` pattern.
6. Restart the daemon. You should see the log line `slack inbound: connected` shortly after startup.

By default the inbound listener honours commands in any channel already used by the outbound side — the union of every `repositories[].chatops_channel_id` plus `chatops.default_channel_id`. Operators who want a separate listen-only channel add it to the optional `chatops.slack.listen_channels` list. Messages from channels outside this allowlist are silently dropped (no `?` reaction either — silent drop keeps the bot's presence invisible in channels it is not authorized to command from).

### Duplicate-delivery suppression

Slack's Socket Mode contract is explicitly *at-least-once*: if the WebSocket ack for an event doesn't reach Slack — typically because the connection dropped before Slack confirmed receipt, or across a reconnect cycle — Slack redelivers the same event on the next connection. Without protection, each redelivery would flow through the full listener pipeline a second time and produce a duplicate bot reply.

The inbound listener defends against this with an in-memory dedup cache, keyed by `(channel, ts, user)` — the tuple that uniquely identifies a Slack message regardless of how many times it's delivered. The first delivery of an event dispatches normally and records the key; subsequent redeliveries of the same key are skipped (the envelope ack is still sent so Slack stops redelivering, but no reply is posted and no control-socket action is submitted). Each suppression logs at INFO with the dedup key and the running suppressed-count:

```
INFO slack inbound: deduplicated event channel=C_OPS ts=1700000000.000100 user=U_OPER suppressed_count=1
```

The cache persists across the listener's reconnect cycles (otherwise we'd lose the signal exactly when we need it most) and is dropped on daemon shutdown.

Two knobs live under `chatops.slack:`:
- `dedup_cache_capacity` (default `100`, max `10000`, set `0` to disable): the maximum number of recently-processed events the listener remembers. Raise for high-traffic channels.
- `dedup_cache_ttl_secs` (default `600` = 10 minutes, max `3600` = 1 hour): per-entry TTL. Slack's redelivery window is typically minutes; the default is generous.

Most operators will not need to touch these. If you're seeing duplicate replies under heavy traffic and `journalctl -u autocoder | grep "deduplicated event"` shows hits being missed (e.g. the key for the duplicate isn't logged), the cache is probably evicting under LRU pressure — raise `dedup_cache_capacity`. If the duplicates are arriving long after the original (rare; only happens during prolonged Slack-side outages), raise `dedup_cache_ttl_secs`.

---

## Chat-driven workflows

These verbs drive entire work flows — chat is the entry point and the bot delivers PRs or threaded replies as the output. All three triage-style verbs (`propose`, `send it`, the implicit triage initiated by certain audits) share the same downstream plumbing: explore the codebase, classify each finding/request as a quick-fix vs spec-worthy, apply both kinds of output, and split the resulting diff into a fixes PR and/or a spec PR.

### Chat-driven proposals: `propose`

The `propose` verb is the chat entry point for "I want autocoder to look at this and either fix it or talk about it":

```
@<bot> propose <repo-substring> <free-form text>
```

Examples:

- `@<bot> propose myrepo add a /healthz endpoint that returns 200 OK with the daemon's version and uptime` — directive; triage produces a fixes PR (and maybe a spec PR).
- `@<bot> propose myrepo what would it take to extract the auth logic into a separate module?` — question; triage replies in the thread, no PR.
- `@<bot> propose myrepo something something handler logic` — ambiguous; triage emits AskUser, the standard chatops escalation fires, the operator clarifies, the executor resumes.

**Ack and lifecycle thread.** The bot's response to `@<bot> propose ...` is a top-level message in the channel:

```
✓ Queued proposal request for <repo_url>. The next polling iteration will run it. Follow along in this thread.
```

The ack message's `ts` becomes the proposal-request's lifecycle thread. Subsequent status updates, the LLM's discussion reply (when the input is a question), and any AskUser escalations all post into that thread.

**Three-way classification.** The chat-triage prompt instructs the LLM to classify the operator's text into one of three buckets BEFORE acting:

- **DIRECTIVE** — a specific action a reasonable engineer would know how to build. The LLM proceeds to explore the codebase, classify each work item as quick-fix vs spec-worthy, apply the fixes, and create new `openspec/changes/chat-request-<short-hash>/` proposals for the spec-worthy items. The diff splits into a fixes PR and a spec PR exactly like `send it`.
- **QUESTION** — the operator is asking for analysis or opinion, not asking for code changes. The LLM writes its reply to `<workspace>/.chat-reply.md` and stops. The polling iteration then reads the file, posts the contents (truncated to 35,000 chars with a daemon-log pointer when over) as a threaded reply in the lifecycle thread, deletes the file, and sets the proposal-request's status to `Discussed`. No PRs are created.
- **AMBIGUOUS** — the request might be a directive but the LLM can't pin down what to build. The LLM calls the `ask_user` MCP tool. The existing chatops escalation posts the clarifying question into the lifecycle thread and resumes the executor once the operator replies.

**Two output paths.** Same shape as the `send it` two-PR mechanic: a fixes PR carrying any code changes and a spec PR carrying any new `openspec/changes/chat-request-<short-hash>/` directory. Both PRs are normal autocoder-opened PRs and participate in [PR-comment revisions](OPERATIONS.md#revising-an-open-pr-via-comment), so `@<bot> revise <text>` on either gets revisions through the standard channel.

**7-day staleness rule.** Proposal-request state files are pruned after 7 days regardless of terminal status (`Acted`, `Discussed`, `TriageFailed`). The directory stays bounded the same way audit-thread state does.

**Polite-refusal cases.** A request whose repo substring resolves to multiple repos gets the standard "be more specific" reply with the candidate URLs. A request with no text after the substring gets `✗ propose: missing request text.`. A request whose text exceeds the 10,000-character cap gets `✗ propose: request text exceeds 10000 characters.` — put longer descriptions in an issue or doc and reference it in a shorter request.

### Acting on audit findings: `send it`

When an audit posts findings to chatops via the threaded-notification path (a `📋`/`📐`/`🧭` top-line with the full findings body as a thread reply), the daemon stamps an audit-thread state file on disk so operators can act on those findings by replying inside the same thread.

```
@<bot> send it       (posted as a reply inside the audit thread)
```

Outside an audit thread, `@<bot> send it` parses as an unknown verb and gets the standard `?` reaction. Inside a tracked, fresh, open audit thread it spawns the executor in **triage mode**: the agent reads the findings, explores the codebase, classifies each finding as a **quick fix** (apply directly to source) or **spec-worthy** (write a new `openspec/changes/<slug>/` proposal), then applies both kinds of output. The polling iteration that drains the triage queue runs immediately after the chatops scheduling, so the operator usually sees the produced PRs within one polling cycle.

**Two-PR output shape.** autocoder splits the executor's diff by path: anything under the new `openspec/changes/<slug>/` directory becomes a separate **spec PR**; everything else becomes a **fixes PR**. Each PR is created on its own branch off `base_branch` and its body cross-links the companion PR (when both are created). If the triage diff has only code, only the fixes PR is created. If it has only a new spec, only the spec PR is created. If it's empty (the LLM decided nothing was actionable), no PR is created and the bot posts the agent's reasoning back into the audit thread.

**7-day staleness rule.** Audit-thread state files are pruned after 7 days regardless of status. A `send it` against an audit older than 7 days gets a polite refusal:

```
✗ This audit's findings are too old to act on (>7d). Re-run the audit via @<bot> audit <type> <repo>.
```

This is intentional: stale audit findings probably no longer reflect the current code, and acting on them blindly burns tokens producing a useless diff.

**Already-acted threads.** Once a triage has run on an audit thread, subsequent `send it` replies get a polite refusal naming the current status (`triage-pending`, `acted`). The exception is `triage-failed`: a failed triage resets back to `triage-pending` on retry, so the operator can `send it` again after fixing whatever went wrong.

**Revising the produced PRs.** Both the fixes PR and the spec PR are normal autocoder-opened PRs that participate in [PR-comment revisions](OPERATIONS.md#revising-an-open-pr-via-comment). If the agent over-promoted findings to specs, ask it to inline the fix via a revision comment on the spec PR; if it under-fixed, point that out via a revision comment on the fixes PR.

### On-demand audit: `audit`

Cadence-based scheduling fires audits on `daily`/`weekly`/`monthly` intervals, which suits steady-state operation but not the production-readiness workflow ("run an architecture audit now, fix what it surfaces, run a security audit now, iterate"). The `audit` verb queues an audit run for the next polling iteration:

```
@<bot> audit <audit-substring> <repo-substring>
```

Audit-substring is matched case-insensitively against the registered audit-type names (same rule as `repo-substring`). Unique match in both → ack with the canonical names and an ETA derived from the repo's `poll_interval_sec`. Ambiguous audit substring → the bot lists the matching candidates. No match → the bot lists every registered audit type.

Example:

```
@<bot> audit sec myrepo
```

becomes:

```
✓ Queued security_bug_audit for git@github.com:acme/myrepo.git. Will run on the next polling iteration (~5m).
```

The ETA is `~Nm` where `N` is `poll_interval_sec` rounded to minutes, or `imminently` when the next iteration is <30 seconds away. Queuing the same audit twice before the iteration fires collapses to a single run.

**Cadence interaction.** A queued audit's `last_run_at` is updated on success, so the next cadence-scheduled fire moves forward by the cadence interval from the on-demand timestamp — an on-demand run "consumes" one cycle of the cadence. Audits configured `cadence: disabled` can still be triggered on-demand; the audit's `last_run_at` is still updated, but with no cadence interval the "next scheduled fire" remains in the past, so the audit stays effectively disabled for cadence-driven scheduling.

**CLI variant.** `autocoder audit run --workspace <path> --audit <name>` does the same job from the command line (no substring matching — the audit-type slug must match exactly). See [CLI.md → audit run](CLI.md#audit-run).

### Generating a changelog: `changelog`

The `changelog` verb queues an LLM-styled `CHANGELOG.md` update against a managed repo. Unlike the deterministic `autocoder changelog` CLI subcommand (which prints to stdout), the chatops verb opens a PR with a polished draft that operators iterate on via the existing revision loop.

```
@<bot> changelog <repo-substring> [<args>]
```

**Accepted flags** (mirror the [`autocoder changelog`](CLI.md#changelog) CLI surface):

- `--since <tag>` — lower bound (exclusive). Default: the most recent tag on `HEAD`'s ancestry. `--since ever` explicitly opts into "from the beginning of archive history".
- `--to <tag>` — upper bound (inclusive). Default: `HEAD`.

The `--workspace <path>` flag is intentionally NOT accepted via chatops: letting any channel member point the stylist at an arbitrary directory is a security gap. The daemon refuses such requests with an inline error AND a WARN log line. Operators with daemon-host access can use `autocoder changelog --workspace <path>` directly instead.

**Ack and lifecycle thread.** Same shape as `propose`:

```
✓ Queued changelog request for <repo-url>. The next polling iteration will run it. Follow along in this thread.
```

The ack's `ts` becomes the changelog-request's lifecycle thread. Status updates and the final PR-URL reply all post into that thread.

**Polling-iteration flow.** On the next polling iteration after the verb is queued, the daemon:

1. Runs the deterministic `a05` extractor against the workspace's archive (calls the data-producing helpers directly — no subprocess).
2. Invokes the wrapped agent CLI with the embedded `prompts/changelog-stylist.md` system prompt + the JSON data as input. The stylist reads any existing `CHANGELOG.md` in the workspace root (matching its style if present, creating a fresh Keep a Changelog v1.1.0 file if absent).
3. Validates the resulting diff's path scope. Only `CHANGELOG.md` AND `openspec/changes/archive/<slug>/proposal.md` (frontmatter edits) are accepted; anything else is refused with `✗ changelog: LLM produced out-of-scope diff; refusing to commit.`
4. Commits the diff to a `changelog-<short-hash>` branch, pushes, AND opens a single PR.
5. Posts a threaded reply in the lifecycle thread: `✓ Changelog draft ready at <PR-URL>. Review on GitHub; revise via @<bot> revise <text>.`

**Single-PR shape.** Unlike `propose`'s two-PR mechanic, the changelog flow produces a single PR. The reason: `CHANGELOG.md` is the only output artifact. When the stylist proposes `changelog: skip` frontmatter edits to source proposals, those land in the same PR — they're part of "what this release's changelog work decided," not a separable concern.

**Frontmatter propagation.** When an operator's revision implies a durable classification (`@<bot> revise leave out the refactors`), the stylist MAY include `changelog: skip` frontmatter edits to the relevant `openspec/changes/archive/<slug>/proposal.md` files in the same PR. Future invocations of the deterministic extractor honor the frontmatter — the classification persists across releases. Reviewers see both the `CHANGELOG.md` edit AND the proposal.md frontmatter edits in a single diff.

**Revision loop.** The PR's `changelog-<short-hash>` branch participates in the [PR-comment revision dispatcher](OPERATIONS.md#revising-an-open-pr-via-comment). An `@<bot> revise <text>` comment on the PR re-runs the stylist with the operator's revision text injected, validates the new diff's path scope, AND force-pushes the updated commit to the same branch (no PR close/re-open).

**7-day staleness rule.** Changelog-request state files are pruned after 7 days regardless of terminal status. Same shape as `propose` / audit-thread state.

**Polite-refusal cases.**

- `✗ changelog: missing repo-substring.` — no first arg.
- `✗ no repo matched '<sub>'; configured: <list>` — substring doesn't resolve to any configured repo.
- `✗ `<sub>` matched multiple repos: ...` — ambiguous substring; lists candidates.
- `✗ changelog: chatops backend not configured.` — the verb needs the backend to ack.
- `✗ changelog: could not post ack to chat: <reason>` — ack post failed; no state file is written (the verb is idempotent on retry).
- `✗ changelog: bad arg: <text>` — `parse_changelog_args` rejected an unrecognized flag or missing value.

### Revising an open PR: `@<bot> revise <text>` (cross-link)

When the bot opens a PR (from a normal queue iteration, from a `send it` triage, or from a `propose` directive), an operator comment of the form `@<bot> revise <free-form text>` on that PR triggers an in-place revision: the next polling iteration re-runs the executor with the original change material, the current PR diff, and the operator's text, then force-pushes the updated diff and posts a `✅ Revision applied:` or `✗ Revision attempt failed:` reply comment.

Per-PR cap (default 5; configurable up to 20 via `executor.max_revisions_per_pr`). Reviewer-initiated revisions (when `reviewer.auto_revise_on_block: true`) share the same cap. Full spec in [OPERATIONS.md → Revising an open PR via comment](OPERATIONS.md#revising-an-open-pr-via-comment).

---

## Operator recovery commands

A small set of admin verbs handles the SSH-and-edit recovery actions from chat instead of switching to a terminal. Every reply is posted as a **threaded reply** to your original `@<bot> <verb>` message — the channel stays clean and the conversation stays grouped near the request. Messages that don't parse as a known verb get a `?`-emoji reaction on the original message rather than a text reply, so typos and drive-by mentions do not spam the channel.

| Verb | Syntax | What it does |
| --- | --- | --- |
| `status` | `@<bot> status <repo-substring>` | Posts a multi-line threaded reply with five always-present sections — branches, last commit on each branch, latest PR from the agent branch, currently-busy state (one of `idle`, `working on <change>`, `running audit <type>`, `<stage> in progress`, `stale marker from pid <pid>`, or the unclassified-fallback `busy (stage=<stage>)` — see [`currently:` line variants](#currently-line-variants) below), and the next-iteration estimate — followed by any active markers, currently-engaged 24h alert throttles, and the queue snapshot (compact one-liner when small, per-line when any list exceeds five entries). When called without `<repo-substring>`, returns a per-repo menu listing every watched repository. |
| `clear-perma-stuck` | `@<bot> clear-perma-stuck <repo-substring> <change-slug>` | Deletes `openspec/changes/<change>/.perma-stuck.json`. The next iteration will retry the change. |
| `clear-revision` | `@<bot> clear-revision <repo-substring> <change-slug>` | Deletes `openspec/changes/<change>/.needs-spec-revision.json`. Use after you've edited `tasks.md` to remove or revise the unimplementable tasks. |
| `wipe-workspace` | `@<bot> wipe-workspace <repo-substring>` | Destructive: removes the entire `<cache_dir>/workspaces/<sanitized-url>/` directory so the next iteration re-clones. Requires two-step confirmation (see below). |
| `rebuild-specs` | `@<bot> rebuild-specs <repo-substring>` | Schedules a full canonical-spec rebuild from archive history. The rebuild runs on the next polling iteration; the resulting commits land via the usual push + PR flow. See [Rebuilding canonical specs from archive history](OPERATIONS.md#rebuilding-canonical-specs-from-archive-history). |
| `help` | `@<bot> help` | Posts a threaded synopsis of every recognised verb with its syntax and a one-line description. |

The verbs `pause`, `resume`, and `clear-alert-throttle` are intentionally not in this initial set. If your operator workflow needs them, file a follow-up issue describing the usage pattern.

### Bare `status` — the per-repo menu

When you don't remember the exact substring of a configured repo, type `@<bot> status` with no arguments. The bot returns a one-line announcement followed by one two-line section per watched repository (URL on top, summary on the next line). The summary has three clauses joined by ` · `: a queue clause (`empty queue` when all three counts are zero, otherwise `<N> pending (<list>), <M> waiting (<list>), <K> excluded` with each list truncating after 5 entries), a busy clause matching the per-repo `currently:` line variants (`idle`, `working on <change> (started <age> ago)`, `running audit <type> (started <age> ago)`, `<stage> in progress (started <age> ago)`, `stale marker from pid <pid> (...)`, or the unclassified-fallback `busy (stage=<stage>, ...)` — see [`currently:` line variants](#currently-line-variants)), and a last-iteration clause (`last iteration <age> ago` or `no iteration yet`). Example:

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

### Two-step confirmation for `wipe-workspace`

`wipe-workspace` is destructive, so the first reply is a warning rather than the action. The warning includes a context preview drawn from the same live data the per-repo `status` command surfaces, so you can make an informed go/no-go call before committing to the wipe:

```
⚠️ Wipe-workspace requested for git@github.com:acme/myrepo.git
This will delete <cache_dir>/workspaces/github_com_acme_myrepo (forces a re-clone on the next iteration).

Currently: working on `audit-proposal-self-validation` (started 5m ago) — will be cancelled
Queue (continues after wipe): 2 pending (pr-body-tweak, queue-archive), 0 waiting, 0 excluded
Active markers (git-tracked; preserved across the wipe):
  • audit-proposal-created-notification (.needs-spec-revision.json)

Reply 'confirm' within 60 seconds to proceed.
```

What each section means:

- **`Currently:`** — `idle` when no busy marker exists; `working on <change> (started <age> ago) — will be cancelled` when the daemon is mid-iteration on a named change. When the daemon is busy without a named change (audit run, post-executor stage, recovery operation, or a stale marker), the line mirrors the per-repo `currently:` variants (`running audit <type> ... — will be cancelled`, `stale marker from pid <pid> ... — will be cancelled`, etc.). Always present so you see what state the wipe is acting on.
- **`Queue (continues after wipe):`** — one-line summary in the same compact form as `status`'s queue clause. Collapses to `Queue (continues after wipe): empty queue` when pending, waiting, and excluded categories are all zero. The queue is preserved across the wipe: only the workspace directory is deleted; the daemon's per-repo state continues.
- **`Active markers (git-tracked; preserved across the wipe):`** — only present when at least one `.perma-stuck.json` or `.needs-spec-revision.json` marker file exists. The "git-tracked; preserved" note reassures you the wipe does not lose marker state — markers are part of the repository tree and return from origin on the next re-clone.

To proceed, reply `confirm` (case-insensitive, no mention needed) within 60 seconds in the same channel. The confirmation is channel-scoped: a `confirm` in a different channel does NOT trigger a pending wipe somewhere else. If you wait longer than 60 seconds, the pending entry expires and you must re-issue the original `wipe-workspace` command.

On `confirm`, the daemon signals the in-flight iteration's per-iteration cancel token, awaits a brief drain (default 30 seconds, configurable via `executor.wipe_drain_timeout_secs`), then deletes the directory. The reply names the drain outcome:

- `✓ Wiped <path> (drained cleanly in <Xs>)` — the iteration exited within the timeout. The cleanest outcome.
- `✓ Wiped <path> (drain timeout — iteration may have been stuck)` — the iteration did not exit within the timeout; the wipe ran anyway. Yellow flag: see [TROUBLESHOOTING.md](TROUBLESHOOTING.md) for follow-up.
- `✓ Wiped <path> (no iteration in flight)` — the daemon was between iterations at confirm time. No drain was needed.
- `✓ Wiped <path> (already absent)` — the workspace directory was already missing. Idempotent no-op.

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

#### `currently:` line variants

The `currently:` line surfaces the daemon's live busy-marker contents. It distinguishes between "truly idle," "working on a named change," "running an audit," "in a post-executor lifecycle phase," and "stale marker awaiting recovery" so an operator wondering why a pending change isn't being picked up can read the line and tell exactly what the daemon is doing:

```
currently: idle
currently: working on a36-expense-tracking (started 3m ago)
currently: running audit architecture_consultative (started 14m ago)
currently: commit in progress (started 12s ago)
currently: push in progress (started 8s ago)
currently: stale marker from pid 490170 (age 9m, recovery in 1m)
currently: stale marker from pid 490170 (age 11m40s, threshold passed, recovery eligible next iteration)
currently: stale marker from pid 490170 (age 53m, recovery eligible now)
currently: busy (stage=executor, started 30s ago)
```

The variants are computed by branching on the marker's contents in this priority order:

1. **No marker present** → `idle`.
2. **Marker present and stale** (dead pid OR age ≥ `executor.busy_marker_stale_threshold_secs`) → `stale marker from pid <pid> (age <age>, recovery <eligible-or-remaining>)`. Three sub-shapes: `recovery eligible now` when the recorded PID is no longer in `/proc` (recovery fires immediately on the next iteration); `threshold passed, recovery eligible next iteration` when the PID is still alive but past the threshold (SIGTERM fires on the next iteration per the busy-marker recovery flow); `recovery in <duration>` when the marker is past 80% of the threshold but not yet at it (recovery is upcoming, so operators see "stuck-feeling" markers as visibly transitioning rather than permanent).
3. **Marker present and `change` non-empty** → `working on <change> (started <age> ago)`. The change branch wins over the stage-based variants because the operator wants to know the change slug before the lifecycle phase.
4. **Marker present, `stage=executor`, `change` empty, and an audit log matches the marker's `started_at`** → `running audit <audit_type> (started <age> ago)`. The audit_type is parsed from the matching audit-log filename under `<logs_dir>/runs/<workspace>/audits/`.
5. **Marker present and `stage` ∈ `{commit, review, push, pr}`** → `<stage> in progress (started <age> ago)`. Names the lifecycle phase so the operator sees which post-executor step is in flight.
6. **Marker present but unclassifiable** (e.g. `stage=executor` with no matching audit log) → `busy (stage=<stage>, started <age> ago)` fallback.

Why this matters: pre-spec, the line collapsed every non-`change` busy state into a misleading `currently: idle`, so an operator hitting "status myrepo" during an audit run would see `currently: idle` plus a non-empty queue and have no idea why the pending change wasn't being picked up. With the surfaced variants, the operator can distinguish "audit in flight, just wait" from "stale marker, need recovery to fire (or manual `rm`)" from "truly idle, something else is wrong." The busy-marker classification logic the stale-marker branches mirror is documented in [OPERATIONS.md](OPERATIONS.md)'s busy-marker section; the immediate-fix-by-hand path for a stale marker is in [TROUBLESHOOTING.md](TROUBLESHOOTING.md)'s stale-marker section.

The age formatting matches the busy-marker convention: `Xs` under 1 minute, `Xm` under 1 hour, `XhYm` past 1 hour. Older "stuck-feeling" markers like `2h17m ago` retain their minute resolution so the operator can see meaningful progress.

Branches and the busy-marker line are always present. `(none)` fills any always-present field whose underlying data is absent (fresh clone, no PR ever opened, etc.). If the GitHub API call fails or local `git log` errors, the affected line falls back to `(none)` and a WARN is logged — the reply still ships every other section so an operator can read the local-state half during a GitHub incident. The queue line uses the compact one-liner form when each of `pending` / `waiting` / `excluded` has ≤5 entries; larger lists fall back to the multi-line `queue snapshot:` format. Commit subjects and PR titles pass through a Slack-escape pass so author-supplied text like `<!channel>` cannot trigger channel-wide mentions when echoed into the reply.

### Repo substring matching

You type the short name; the bot resolves it. The match is case-insensitive substring search against the full configured `repositories[].url`. `myrepo` matches `git@github.com:acme/myrepo.git`; `MYREPO` does too. If two repos with the same trailing name exist under different owners, the bot replies with the candidate list and asks for a more specific substring. If nothing matches, the bot replies with the full list of configured URLs so you can copy one back.

### Unrecognised verbs get a `?` reaction, no text reply

Random chat that happens to mention the bot but doesn't match a known verb (typos, drive-by mentions, AskUser-thread replies, etc.) gets a single `?`-emoji reaction on the original message — no text reply, no thread spam. The reaction is a quiet "this didn't parse" signal: discoverable for the operator who typed the command, ignorable for everyone else. Type `@<bot> help` for the current verb list.

### Mobile vs desktop mention forms

Slack's mobile client and desktop client render `@<bot-name>` identically on screen but emit two different mention strings in the underlying message text. Desktop emits the bot's **user id** (`<@U...>`); mobile emits the bot's **bot/app id** (`<@B...>`). Both refer to the same bot. autocoder caches both ids at startup (via `auth.test`) and the inbound chatops listener accepts either form as the leading bot mention — operators don't need to do anything specific.

If mobile mentions stop working after a token rotation, check the daemon log for the `auth.test response missing bot_id` WARN. Some Slack token types don't return a `bot_id` field; when that field is missing, the daemon falls back to user-id-only matching and mobile-app mentions stop being recognised while desktop continues to work. The WARN line names the gap explicitly so operators know where to look.

---

## AskUser escalation

The original chatops mechanic: when an executor returns `AskUser { question, resume_handle }`, the daemon posts the question to the resolved channel, the change moves from "in flight" to "waiting on human," and the next iteration polls the Slack thread for the first non-bot reply. When the reply arrives, the executor resumes against the operator's answer.

### What gets posted

```
❓ `<change-name>`: <question text>
```

The resulting Slack message's thread timestamp + the executor's opaque resume handle are persisted to `<workspace>/openspec/changes/<change-name>/.question.json`. The agent's `.in-progress` lock is removed, so the change moves from "in flight" to "waiting on human."

### How reply detection works

On every polling iteration, BEFORE considering pending changes for that repository, the daemon:

1. Calls `queue::list_waiting(workspace)` to find all `.question.json`-bearing changes.
2. For each, GETs `conversations.replies` on the tracked thread.
3. The **first message** that has no `bot_id` field AND whose `user` differs from autocoder's own bot user id is treated as the human's answer.
4. The daemon writes `.answer.json`, deletes `.question.json`, calls `executor.resume(handle, answer)`, and handles the new outcome like a fresh run (commit + archive on `Completed`, escalate again on a second `AskUser`, log + revert to pending on `Failed`).

### Same-repo queue blocking

A change waiting on a human answer in repository X blocks ALL pending-change processing for repository X. This preserves the architecture's serial-queue invariant: when change A asks a question, change B (which may depend on A's restructuring) is NOT processed until A is resolved. Cross-repo polling tasks are independent — repository Y continues to be serviced.

### Operator escape hatches for a stuck waiting change

If a Slack reply never arrives, autocoder does not time out — it waits indefinitely. Three operator-controlled ways to unblock:

1. **Reply in Slack** — the original thread is still tracked. Send any non-bot message in that thread; the next polling iteration resumes the change.
2. **Manually delete `.question.json`** — reverts the change to pending state. The next iteration re-runs it from scratch (without the answer). Useful when the question was a false positive or the change should restart.
3. **`autocoder rewind <change>`** — full reset: deletes the agent branch, unarchives if needed, clears all `.question.json` / `.answer.json` markers via the rewind path.

---

## Progress notifications

In addition to escalation, autocoder posts a low-volume activity feed to the same chatops channel — operators watching the channel can tell at a glance whether the daemon is alive and what it is doing.

### Configuration

```yaml
chatops:
  # existing fields...
  notifications:
    start_work: true       # default true; one message per change pickup
    failure_alerts: true   # default true; throttled per (repo, category)
    pr_opened: true        # default true; one message per opened PR (with link)
```

All three keys are optional. An absent `notifications:` block parses to "all true" — first-time deployments see useful chatops traffic without further configuration. Set a key to `false` to suppress that stream without affecting the others. If `post_notification` itself fails (network blip, channel renamed, scope revoked), the failure is logged to stderr but is NEVER re-routed back through chatops — there is no recursive alert cascade.

### Start-of-work (`🚀`)

One line per change pickup, fired immediately after the change's `.in-progress` lock is created and BEFORE the executor is invoked:

```
🚀 `<repo-url>`: starting work on `<change-name>` — <first line of ## Why>
```

### Throttled failure alerts (`⚠️`)

Emitted at most once every 24 hours per (repository, failure category) for three categories of *predictable* infrastructure failure: workspace init / clone failure, branch push rejection, and PR creation 4xx from GitHub.

```
⚠️ `<repo-url>`: <category-label> for the past 24h. Latest: <error excerpt>
```

The 24h throttle state lives in a per-workspace `.alert-state.json` file. On the next successful iteration the file is removed, so a transient outage followed by recovery does not leave the next failure (whenever it occurs) silenced. Other failure surfaces — executor returning `Failed`, reviewer LLM call errors, the chatops post itself failing — are deliberately out of scope and never produce a categorized alert.

### PR-opened (`✅`)

One message per opened PR with the URL. Gated by `notifications.pr_opened`.

### Proposal-created audit notifications (`🔍`)

LLM-driven audits that generate OpenSpec change proposals (`missing_tests_audit`, `security_bug_audit`) post a `🔍` notification immediately after the proposal passes `openspec validate --strict` AND before the audit's `git commit` ships it to the agent branch:

```
🔍 <repo-url>: <audit-type> created proposal `<change-slug>` — <first line of ## Why>
```

When the proposal validated only after one or more retries, the text gains the same parenthetical the success log line uses:

```
🔍 <repo-url>: <audit-type> created proposal `<change-slug>` — <summary> (validated on retry 1 of 2)
```

This **always fires** when an LLM-driven audit produces a valid proposal; it is **not** gated by `notify_on_clean`. The two switches operate on opposite signal classes: `notify_on_clean` suppresses "nothing to do" messages, whereas `🔍` is the "audit found something worth doing" signal — suppressing it would defeat the purpose. The operator's next chatops message about that change is the existing `🚀 starting work on …` line; the `🔍` provides the provenance for it.

The pure-data `architecture_brightline` audit does NOT fire this notification (it does not generate an LLM proposal). The advisory `architecture_consultative` and `drift_audit` audits also do not fire it — they emit findings via the existing `📋` chatops dispatch and never write `openspec/changes/<slug>/`.

If the chatops backend is unconfigured OR `post_notification` errors when this notification is posted, the failure is logged at WARN and the audit's success outcome (proposal commit, queue insertion) is unaffected.

### Audit-finding threaded notifications (`📐` / `🧭` / `📋` / `✅`)

Audit results from the advisory audits (`architecture_brightline`, `drift_audit`, `architecture_consultative`) are posted as a **one-line top-level message** in the channel with the full findings carried in a **Slack thread reply** to that message. Channel watchers see a clean feed of summary lines; clicking into a thread surfaces the per-finding detail. Per-audit-type emoji conventions:

- `📐 architecture_brightline on <repo-url>: <N> file(s) over line threshold; <M> duplicate signature(s)`
- `🧭 drift_audit on <repo-url>: <N> spec/code divergence(s) detected`
- `📋 <audit-type> on <repo-url>: <N> finding(s)` — generic fallback for any other `Reported`-outcome audit.
- `✅ <audit-type> on <repo-url>: no findings` — uniform shape for clean runs under `notify_on_clean=true`.

The thread is only used when the findings body would actually benefit from one: more than 3 lines OR more than 300 characters. Shorter findings inline into a single message — a thread for a one-line bullet is more friction than value. Empty findings under `notify_on_clean=true` post the `✅` form inline (the body is empty; nothing to thread); under `notify_on_clean=false` no message is posted at all (existing behaviour).

Slack's per-message limit is 40,000 characters. When the thread body would exceed 35,000 characters, it is truncated to 35,000 and ends with a pointer at the daemon log so operators can recover the full text:

```
… [truncated; full findings at journalctl -u autocoder | grep audit_id=<repo-sanitized>:<audit-type>:<utc-timestamp>]
```

The audit-runner stamps the same `audit_id` into its daemon-log entries for the same run.

### Validation-exhausted audit notifications (`❌`)

LLM-driven audits that generate OpenSpec change proposals run each proposal through `openspec validate --strict` before committing. When validation fails and the configured retry budget (`audits.max_validation_retries`, default `1`, see [CONFIG.md](CONFIG.md)) is exhausted, the audit discards the proposal and posts a one-line chatops notification:

```
❌ <repo-url>: <audit-type> produced an invalid proposal that failed openspec validation after <N> retries.
Final validation error:
<truncated stderr, capped at 800 chars>
No commit was made. The audit will retry on its next scheduled cadence.
```

When the validation error is multi-line OR exceeds 300 characters, the notification routes through the same threaded path used for audit findings: the `❌` top-line lands in the channel and the `Final validation error: …` body lands in the thread reply. Single-line short errors continue to inline into a single message as shown above.

This fires **regardless of `notify_on_clean`** — an audit producing invalid proposals is operator-actionable feedback that the audit's prompt template or LLM output is degrading; suppressing the signal would hide the failure mode. The audit's own cadence determines when it retries (no special re-trigger).

Operator action when this fires repeatedly for the same audit type: review the audit's prompt template (`audits.settings.<slug>.prompt_path` or the embedded default). Repeated validation failures usually mean the prompt does not bind the LLM tightly enough to the OpenSpec delta format. See [TROUBLESHOOTING.md](TROUBLESHOOTING.md#audit-produces-invalid-proposal--what-to-do).

When a `notify_on_clean=true` Reported outcome comes back with `retries_used > 0` (the audit succeeded after one or more retries), the existing success notification gains a trailing clause:

```
✅ <repo-url>: <audit-type> — no findings (validated on retry 1 of 1)
```

The clause is informational. Operators tracking audit reliability over time can use it as a leading indicator that a prompt template might benefit from tightening before it starts failing outright.

### Revision cap notifications (`🛑`)

The PR-comment revision channel (see [OPERATIONS.md → Revising an open PR via comment](OPERATIONS.md#revising-an-open-pr-via-comment)) emits a one-time chatops notification when an open PR hits its revision cap:

```
🛑 <repo-url>: PR #<num> hit the revision cap of <N>. Further revision requests ignored.
```

This fires alongside the one-time `🛑 Revision cap reached` PR comment. Subsequent triggering comments on the same PR are silently ignored — the one chatops line is the operator's only out-of-band signal that the PR has stopped accepting revisions. The notification is not gated by the `failure_alerts` switch (it is a one-shot per PR, not a throttled infrastructure alert).

---

## Reference

### Workspace state files

| File | Location | Role |
| --- | --- | --- |
| `.question.json` | `<workspace>/openspec/changes/<change>/` | AskUser thread + resume handle while a change is waiting on a human answer. |
| `.answer.json` | `<workspace>/openspec/changes/<change>/` | The operator's reply text, captured on the iteration that resumed the executor. Removed after resume completes. |
| `.alert-state.json` | `<workspace>/` | 24h-alert throttle window per (repo, failure category) for progress notifications. |

All three are written atomically (temp-file-then-rename) so they're consistent on disk, but the daemon's state machine assumes it owns their lifecycle — safe to inspect (plain JSON), unsafe to modify by hand. When a change is archived, the directory move takes the change-scoped marker files with it; `.alert-state.json` is cleared whenever the polling pass completes without hitting any of the three predictable-failure sites.

Deleting `.alert-state.json` by hand is harmless: it just resets the alert throttle window for that repository, so the next predictable failure will alert immediately rather than wait out the 24h window.

### Trust boundary

Whoever has write access to the configured chatops channel is treated as an operator — the same trust boundary as the existing `AskUser` reply detection. Sites that need finer-grained control configure separate channels per concern via the existing per-repo `chatops_channel_id` override.

Under the hood, the chatops listener parses the command, resolves the repository, and submits a JSON action over the daemon's existing Unix-domain control socket (the same socket used by `autocoder reload`). The same actions are reachable from any future CLI subcommand without duplicating logic; the control socket's existing Unix-perms / daemon-user-only authentication applies identically.

---

## Experimental ChatOps Backends

> **No API-stability guarantees.** Discord, Microsoft Teams, Mattermost, and Matrix are implemented behind the same `ChatOpsBackend` trait as Slack but are explicitly marked experimental: their unit tests pin only the request shape against recorded fixture responses (not live services), so an upstream API change can break them silently. Each emits a loud `warn`-level startup log line stating "EXPERIMENTAL — best-effort support, may break without notice." If you select one and it stops working, **please file a bug**; that is how the experimental backends move toward official support.
>
> Slack remains the only officially-supported provider. Single-process autocoder runs against exactly one chat backend at a time; if you live on multiple platforms, pick the most-used one.
>
> **Threaded audit notifications fall back to a single message.** The audit-finding threading pattern is native to Slack only. Experimental backends inherit the trait's default `post_notification_with_thread` implementation, which concatenates the top-line summary and the findings body into one `post_notification` call separated by a blank line. The operator-visible effect is the pre-threading behaviour: walls of text in the channel. Per-backend native-threading overrides may be added in future changes; today's experimental backends are unchanged by this trait addition.
>
> **Inbound listener is Slack-only.** The operator verbs in [Chat-driven workflows](#chat-driven-workflows) and [Operator recovery commands](#operator-recovery-commands) require the Slack Socket Mode inbound listener. Experimental backends do not implement an inbound surface — they are outbound-only (`AskUser` escalation, notifications). Operators on a non-Slack backend interact with the daemon via SSH, `autocoder` CLI subcommands, and manual marker-file edits.

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
