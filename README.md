# autocoder

**autocoder** is an autonomous daemon that reads OpenSpec implementation proposals from one or more configured repositories, drives an AI coding agent (the Claude CLI by default) through each change in serial order, and opens monolithic Pull Requests for human review. It's "OpenSpec change at the top, working code in a PR at the bottom" wired into a single long-running process.

---

## Quick Start

Get from `git clone` to a running daemon in about ten minutes. Each step is self-contained; do them in order.

### 1. Prerequisites

On the machine where the daemon will run:

- **Rust toolchain.** Install via [rustup](https://rustup.rs/) — autocoder builds against stable Rust on edition 2024.
- **Claude Code authenticated.** Install [Claude Code](https://www.anthropic.com/claude-code) and run `claude auth login` as the same OS user that will run the daemon. The credentials are persisted in `~/.claude/` and survive restarts.
- **OpenSpec CLI installed and on `$PATH`.** Install with `npm install -g @fission-ai/openspec` (Node.js required) and verify with `openspec --version`. autocoder shells out to `openspec instructions apply` to build richer per-change prompts for the agent; without it the executor falls back to raw markdown concatenation, which gives the agent noticeably less guidance and is a known cause of "lazy archive" failures.
- **A GitHub Personal Access Token**, scoped to the repositories autocoder will manage. Either form works; pick based on your account setup:

  - **Fine-grained PAT** (recommended for personal-account-owned repos). Required permissions:
    - **`Pull requests: read & write`** — needed for PR creation.
    - **`Administration: write`** — needed ONLY if you use `github.fork_owner` (fork-PR mode) AND not all forks already exist; autocoder calls `POST /forks` for missing ones.
    - **`Contents: read & write`** — needed ONLY if your `config.yaml` uses HTTPS URLs (`https://github.com/...`); when you use SSH URLs (`git@github.com:...`), git authenticates via your SSH key and `Contents` is not required.
    - **`Issues: read & write`** — needed ONLY in the rare case that your host rejects draft PRs and triggers the `do-not-merge` label fallback. GitHub.com supports drafts on every repo type, so this is not needed there; only relevant for some private GHE configurations.

    Fine-grained PATs are scoped to a single account or organization; multi-owner setups use [Multiple GitHub Tokens](#multiple-github-tokens) instead.

  - **Classic PAT** (simpler when the minting account itself has scoped repo access — e.g. a machine user added as Read collaborator on specific repos). Required scope: **`repo`** (covers PR creation, label apply, and HTTPS git ops). The PAT's effective access intersects with the minting user's actual repo permissions, so a classic PAT minted by a scoped-access machine user is effectively scoped to those repos. Tradeoff: future repo additions (new team membership, new collaborator invite) automatically extend the PAT's reach; fine-grained requires re-minting. Also: some orgs require fine-grained PATs at the org-policy level (Settings → Personal access tokens → "Restrict access via fine-grained personal access tokens"); check before committing to classic.

  Export the token as `GITHUB_TOKEN` in the environment that will launch the daemon, or use the inline form in `config.yaml` (see [Secrets in `config.yaml`](#5-secrets-in-configyaml-inline-vs-env-var)).
- **`git` configured.** Either a registered SSH key for the configured repository URLs (recommended), or HTTPS credentials in a credential helper.

### 2. Clone and configure

```bash
git clone https://github.com/IndustriousKraken/openspec-autocoder.git
cd openspec-autocoder
cp config.example.yaml config.yaml
```

Edit `config.yaml` and set the `url:` value to your repository. The shipped example uses `git@github.com:your-org/your-repo.git` as a placeholder.

### 3. Build the daemon

```bash
cd autocoder
cargo build --release
sudo cp target/release/autocoder /usr/local/bin/autocoder
cd ..
mkdir -p ~/autocoder
cp config.yaml ~/autocoder/config.yaml
chmod 600 ~/autocoder/config.yaml
```

The build produces a `~10 MB` self-contained binary. The implementer prompt template (`prompts/implementer.md`) and the code-reviewer template (`prompts/code-review-default.md`) are both embedded at compile time, so the runtime needs only `config.yaml`. To override either template, set `executor.implementer_prompt_path` or `reviewer.prompt_template_path` in `config.yaml` to a path on disk. The `--config` flag accepts any absolute path.

### 4. Run it

```bash
export GITHUB_TOKEN=ghp_yourfinegrained_token_here
RUST_LOG=info autocoder run --config ~/autocoder/config.yaml
```

> **Multiple GitHub accounts/orgs?** Skip the `GITHUB_TOKEN` export and use the [Multiple GitHub Tokens](#multiple-github-tokens) section to configure `github.owner_tokens:` in `config.yaml` instead.

You should see (within a few seconds):

```
INFO autocoder: configured repository url=... workspace=/tmp/workspaces/... poll_interval_sec=300
INFO autocoder: starting polling loop ...
INFO autocoder: polling pass produced no changes
```

If your repository's `openspec/changes/` directory contains a ready change, the daemon picks it up on the next iteration, runs the Claude CLI against it, commits the diff, pushes the agent branch, and opens a PR.

To stop the daemon: `Ctrl-C` (SIGINT). It drains the current iteration and exits within ~30 seconds.

### 5. (Optional) Verify against a sandbox

[`docs/foundation-smoke-test.md`](docs/foundation-smoke-test.md) walks through scaffolding two throwaway GitHub repos with trivial OpenSpec changes and confirming the full clone → execute → commit → push → PR cycle works against them. Recommended for first-time deploys.

---

## Configuration Reference

Full schema of `config.yaml`. The minimal viable file is in [config.example.yaml](config.example.yaml); everything below is for tuning or enabling optional capabilities.

### `repositories:` (required)

A list of one or more repositories to manage. Each entry:

| Field                | Required | Default | Description |
|----------------------|----------|---------|-------------|
| `url`                | yes      | —       | Git URL (SSH or HTTPS). |
| `base_branch`        | yes      | —       | The branch agent work is based off of (typically `main` or `dev`). |
| `agent_branch`       | yes      | —       | The branch the daemon pushes work to (typically `agent-q`). |
| `poll_interval_sec`  | yes      | —       | Seconds between iterations on this repo. |
| `local_path`         | no       | derived | See [Workspace path derivation](#workspace-path-derivation). |
| `chatops_channel_id` | no       | falls back to `chatops.default_channel_id` | See [ChatOps Escalation](#chatops-escalation). |
| `max_changes_per_pr` | no       | falls back to `executor.max_changes_per_pr`, then `3` | Upper bound on archived changes committed in one iteration's PR. Remaining pending changes wait for the next iteration. A value of `0` is clamped to `1` with a WARN log at startup. |

### `executor:` (required)

| Field                       | Required | Default       | Description |
|-----------------------------|----------|---------------|-------------|
| `kind`                      | yes      | —             | Currently only `claude_cli` is supported. |
| `command`                   | no       | `claude`      | Path to the wrapped CLI. Set only if `claude` isn't on `$PATH`. |
| `timeout_secs`              | no       | `1800`        | Wall-clock budget per change. Killed-and-Failed on overrun. |
| `sandbox`                   | no       | safe defaults | Tool-use restrictions applied to every executor invocation. See [Executor tool sandbox](#8-executor-tool-sandbox). |
| `implementer_prompt_path`   | no       | _embedded_    | Path to a file overriding the built-in implementer prompt template. The template must contain the literal `{{change_body}}` placeholder, which is replaced with `openspec instructions apply` output at each invocation. Unset means use the template compiled into the binary. |
| `perma_stuck_after_failures`| no       | `2`           | Consecutive Failed iterations after which a change is marked perma-stuck. See [Perma-stuck change detection](#perma-stuck-change-detection). A value of `0` is clamped to `1` with a WARN log at startup. |
| `max_changes_per_pr`        | no       | `3`           | Default cap on archived changes committed in one iteration's PR; per-repo `max_changes_per_pr` overrides. Operators with long queues see them ship across multiple iterations instead of one large PR. A value of `0` is clamped to `1` with a WARN log at startup. |
| `startup_jitter_max_secs`   | no       | `30`          | Each polling task waits a uniformly random `[0, startup_jitter_max_secs]` seconds before its first iteration. Staggers a fleet of concurrent `git fetch` operations so an IDS does not see a synchronized burst. Set to `0` to disable. See [Polling cadence and your firewall](#polling-cadence-and-your-firewall). |
| `inter_iteration_jitter_pct`| no       | `10`          | Each inter-iteration sleep is `poll_interval_sec` adjusted by ±this percent (uniform random offset). Prevents long-term re-synchronization of multiple tasks. Set to `0` for exact intervals. Values above `100` are clamped to `100`. |

### `github:` (required)

| Field          | Required | Default          | Description |
|----------------|----------|------------------|-------------|
| `token_env`    | no       | `GITHUB_TOKEN`   | Name of the env var holding the fallback PAT. |
| `token`        | no       | _absent_         | Inline alternative to `token_env`: `{ value: "ghp_..." }`. When set, `token_env` is ignored. See [Secrets in `config.yaml`](#5-secrets-in-configyaml-inline-vs-env-var). |
| `owner_tokens` | no       | _absent_         | Optional map of GitHub owner → env var name **or** inline `{ value: "..." }`. See [Multiple GitHub Tokens](#multiple-github-tokens). |
| `fork_owner`   | no       | _absent_         | Enables fork-and-PR mode. Names the GitHub handle that owns the forks. See [Fork-and-PR workflow](#7-fork-and-pr-workflow-recommended-for-org-repos). |
| `recreate_fork_on_reinit` | no | `false` | When `true` AND fork-PR mode is active AND the workspace directory was absent at iteration start (fresh clone), autocoder deletes the existing fork on GitHub and re-forks upstream before initializing the workspace. Recovers cleanly when the fork has accumulated stale branches no one cares about. **Destructive**: any open PRs whose head branch lives on the deleted fork are closed by GitHub when the head ref disappears. Requires the operator's PAT to include the `delete_repo` scope (without it, the DELETE returns 403, autocoder logs ERROR, and falls back to the conservative non-recreating init path). See [Operating notes — fork recreation on workspace reinitialization](#fork-recreation-on-workspace-reinitialization). |

### `reviewer:` (optional)

See [Code Review](#code-review). Absent block disables the reviewer step.

### `chatops:` (optional)

See [ChatOps Escalation](#chatops-escalation). The block carries a required `provider:` field (`slack` officially supported; `discord`, `teams`, `mattermost`, `matrix` are [EXPERIMENTAL](#experimental-chatops-backends)) plus a `default_channel_id:` and a per-provider sub-block. Absent block disables ChatOps escalation; an executor `AskUser` outcome falls back to "log and exit the iteration" behavior.

### `audits:` (optional)

Top-level periodic-audit framework configuration. Absent block → every audit's effective cadence is `disabled` and the daemon behaves identically to a build without the framework. See [Periodic audits](#periodic-audits) for the full operational model.

| Field | Type | Description |
|---|---|---|
| `defaults` | `map<audit-slug, Cadence>` | Global default cadence per audit type. Audit slugs must match a registered type (currently `architecture_brightline`); typos fail at config load with a list of known slugs. |
| `settings` | `map<audit-slug, AuditSettings>` | Per-audit knobs. See below. |

Per-repo override: each entry under `repositories[]` accepts an `audits:` field that maps audit slugs to cadences. Per-repo entries take precedence over `audits.defaults`; an absent entry in both locations resolves to `disabled`.

**`Cadence` syntax (string):** `disabled` | `daily` | `every-N-days` (N must be a positive integer; `every-0-days` and negative N are rejected at load time) | `weekly` | `monthly` | `quarterly`.

**`AuditSettings` fields:**

| Field | Type | Description |
|---|---|---|
| `prompt_path` | `path` (optional) | Override the audit's embedded LLM prompt template. No LLM audits ship in the foundation change; the mechanism is in place for future audits. |
| `notify_on_clean` | `bool` (default `false`) | When `true`, an empty-findings `Reported` outcome posts `✅ <repo>: <audit_type> — no findings` to chatops. When `false`, silence is success. |
| `extra` | `map<string, yaml>` | Free-form per-audit knobs. `architecture_brightline` reads `file_lines_threshold` (default `800`) from here. |

---

## Multiple GitHub Tokens

GitHub fine-grained PATs are scoped to a single account or organization — only the owner of a resource can mint one for it. A contributor who runs autocoder against, say, a personal repo plus repos in two work orgs cannot cover all three with a single fine-grained PAT.

autocoder resolves this by routing PATs per **repository owner** (the segment before the repo name in the URL: `<owner>/<repo>`). Configure the `github.owner_tokens:` map and export one env var per owner; autocoder parses each repo's URL at startup, picks the matching env var case-insensitively, and uses it for that repo's PR-creation HTTP calls.

### Example: personal + two orgs

`config.yaml`:

```yaml
github:
  token_env: GITHUB_TOKEN              # fallback for any owner not in the map below
  owner_tokens:
    my-personal-gh:  PERSONAL_GH_TOKEN     # owner → env var name (not the token value)
    my-org-a:    ORG_A_GH_TOKEN
    my-org-b:    ORG_B_GH_TOKEN

repositories:
  - url: "git@github.com:rbeverly/personal-repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
  - url: "git@github.com:my-org-a/work-repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
  - url: "git@github.com:my-org-b/another-repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
```

Environment when launching the daemon:

```bash
export PERSONAL_GH_TOKEN=github_pat_xxx_personal
export ORG_A_GH_TOKEN=github_pat_xxx_org_a
export ORG_B_GH_TOKEN=github_pat_xxx_org_b
# GITHUB_TOKEN need not be set, because every configured owner has a route.
RUST_LOG=info ./target/release/autocoder run --config config.yaml
```

### Startup behavior

Before spawning any polling task, autocoder iterates every configured repository and resolves a token route for each. If any repo's owner has no matching `owner_tokens` entry AND its fallback (`token_env`'s named env var) is unset, the daemon exits non-zero immediately, naming the unmappable repo.

On success, autocoder emits one log line per repo naming the env var (never the token value):

```
INFO repository git@github.com:rbeverly/personal-repo.git will use GitHub token from env var PERSONAL_GH_TOKEN
INFO repository git@github.com:my-org-a/work-repo.git will use GitHub token from env var ORG_A_GH_TOKEN
INFO repository git@github.com:my-org-b/another-repo.git will use GitHub token from env var ORG_B_GH_TOKEN
```

### Matching rules

- Map keys are matched against URL owners **case-insensitively** (`My-Org` matches `my-org` and vice versa). GitHub owner names are case-insensitive at the platform level.
- The first matching entry wins. If you have duplicate keys differing only in case, fix the YAML — there is no defined priority.
- An owner with no `owner_tokens` entry falls back to `github.token` (inline) if set, otherwise `github.token_env`. A repository with neither route is a startup error.

### Inline owner-token values

Each map value can be either an env var name (bare string) or an inline value (`{ value: "..." }`); the two forms can be mixed in one map:

```yaml
github:
  owner_tokens:
    my-org-a: ORG_A_GH_TOKEN              # env var name
    my-org-b:                             # inline value
      value: "github_pat_xxx_for_org_b"
```

See [Secrets in `config.yaml`](#5-secrets-in-configyaml-inline-vs-env-var) for the security tradeoff.

### git operations are separate

This routing affects only HTTP calls to GitHub's REST API (PR creation, optional label fallback). Git operations (`clone`, `fetch`, `push`) go through whichever authentication `git` itself uses — your SSH key, an HTTPS credential helper, etc.

**Recommendation for multi-owner setups:** use SSH URLs (`git@github.com:owner/repo.git`) in `config.yaml`. A single SSH key registered against each account/org covers the git side without per-owner credential-helper configuration, while autocoder's `owner_tokens` covers the API side. HTTPS URLs work but require a git credential helper that can map URLs to different PATs, which autocoder does not configure for you.

### Non-goal: per-repository overrides

Two repositories under the same owner cannot use different tokens. Token routing is per-owner only.

---

## Architecture

autocoder is a single tokio-based daemon with one polling task per configured repository. Each iteration follows a fixed workflow: fetch + branch init → process waiting (escalated) changes → process pending changes → push + PR if any commits were produced. The serial-per-repo invariant guarantees that change B does not run while change A is mid-flight or waiting on human input.

Built capabilities (each is a baseline spec under `openspec/specs/`):

1. **orchestrator-cli** — the `run` daemon entry point and the `rewind` recovery subcommand. Multi-repo dispatch with a shared cancellation token; per-repo polling tasks; SIGINT/SIGTERM drain.
2. **workspace-manager** — deterministic per-repo workspace paths under `/tmp/workspaces/`, idempotent clone-or-fetch, startup-time cross-repo collision detection, and a startup dirty-workspace check that skips a dirty repo for the process lifetime.
3. **openspec-queue-engine** — enumerate (pending + waiting), lock/unlock via `.in-progress` markers, stale-lock cleanup at startup, archive on completion with `YYYY-MM-DD-<change>` date prefix, unarchive on rewind.
4. **executor** — backend-agnostic `Executor` trait with `Completed` / `AskUser` / `Failed` outcomes plus a `resume()` entry point. First concrete backend is `ClaudeCliExecutor`, which wraps the `claude` CLI as a subprocess with a configurable timeout and two-layer `AskUser` detection (an MCP-tool marker file plus a stdout-regex backstop).
5. **git-workflow-manager** — branch init (`fetch → checkout base → pull --ff-only → checkout -B agent`), per-change commits with `<change>: <first line of ## Why>` subject truncated to 72 chars, monolithic PR creation via the GitHub REST API with `--force-with-lease` push.
6. **chatops-manager** — chat-platform escalation behind a `ChatOpsBackend` trait. Slack is the officially-supported provider; Discord, Teams, Mattermost, and Matrix are [experimental backends](#experimental-chatops-backends) with no API-stability guarantees. On `AskUser`, the daemon posts a question to a configured channel and persists `.question.json` to disk. On the next iteration it polls the thread; when the first non-bot reply arrives it writes `.answer.json` and resumes the executor. Same-repo serial-queue invariant is preserved: any waiting change in a repository blocks all pending-change processing for that repo until resolved.
7. **code-reviewer** — opt-in AI code-quality review of the diff between base and agent branches. Configurable LLM provider (Anthropic or any OpenAI-compatible endpoint, including Grok, OpenRouter, local Ollama). A `Block` verdict creates the PR as a draft (with a `do-not-merge` label fallback on hosts that reject drafts).

The default executor backend wraps `claude` as a subprocess. The daemon writes a per-workspace `.mcp.json` pointing at itself as an MCP server exposing the `ask_user` tool; when the agent calls it, a marker file is written and the daemon picks it up after the child exits. The MCP server is hosted as a hidden subcommand of the autocoder binary, so deployment is a single-binary install.

---

## ChatOps Escalation

When the optional `chatops:` config block is present, autocoder routes ambiguous agent outcomes (executor returning `AskUser`) to a human via chat-thread replies, persists the conversation state to disk, and resumes implementation on the next iteration when an answer arrives. **Slack is the officially-supported provider**; Discord, Teams, Mattermost, and Matrix are available as [experimental backends](#experimental-chatops-backends) with no API-stability guarantees.

### Configuring Slack (officially supported)

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

The inline form follows the same dual-source pattern as `github.token` and `reviewer.api_key`; see [Secrets in `config.yaml`](#5-secrets-in-configyaml-inline-vs-env-var) for the security tradeoff.

Per-repo override:

```yaml
repositories:
  - url: "git@github.com:my-org/auth-service.git"
    # ...
    chatops_channel_id: C0AUTH_CHANNEL  # this repo posts to a different channel
```

### Progress notifications

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

### Required Slack bot scopes

A **private channel** is the recommended deployment — it keeps non-operators from prompting the agent. The Slack app's bot token must have:

- `chat:write` — post the escalation message into the channel.
- `groups:history` — read thread replies in private channels (use `channels:history` instead if you deploy against a public channel).

`auth.test` is scope-less, so the bot's identity check at startup needs nothing further. `users:read` is not required — reply attribution is by Slack user id only.

After installing the app, invite the bot to the channel (`/invite @YourAppName`); otherwise `chat.postMessage` returns `not_in_channel`.

### What gets posted

When an executor returns `AskUser { question, resume_handle }`, the daemon posts to the resolved channel:

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

### `.question.json`, `.answer.json`, and `.alert-state.json` as workspace artifacts

These files are written by autocoder into the workspace as bookkeeping. `.question.json` and `.answer.json` live alongside the change's `proposal.md`; `.alert-state.json` lives at the workspace root and tracks the per-(repo, category) 24h-alert throttle for [progress notifications](#progress-notifications).

All three are safe to inspect (plain JSON) but unsafe to modify by hand — atomic writes via temp-file-then-rename mean they're consistent on disk, but the daemon's state machine assumes it owns their lifecycle. When a change is archived, the directory move takes the change-scoped marker files with it; `.alert-state.json` is cleared whenever the polling pass completes without hitting any of the three predictable-failure sites.

Deleting `.alert-state.json` by hand is harmless: it just resets the alert throttle window for that repository, so the next predictable failure will alert immediately rather than wait out the 24h window.

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

---

## Code Review

When the optional `reviewer:` config block is present and `enabled: true`, every PR opened by autocoder includes a structured AI-generated code-quality review under a `## Code Review` heading in the PR body. A `Block` verdict additionally causes the PR to be created as a draft.

### Scope

The reviewer's job is **code quality only**: security (injection, auth, secrets), error handling, naming/style/idioms, dead code, obvious bugs. It explicitly does **not** assess whether the diff implements the spec — that is a separate concern handled by the (future) verifier. The default prompt template (`prompts/code-review-default.md`) enforces this scope statement at the top.

### Configuring the reviewer

```yaml
reviewer:
  enabled: true
  provider: anthropic               # or `openai_compatible`
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY    # env var holding the API token
  # OR — inline alternative; when `api_key` is set, `api_key_env` is ignored.
  # api_key:
  #   value: "sk-ant-..."
  api_base_url: https://api.anthropic.com   # optional; provider default if omitted
  prompt_template_path: ./prompts/code-review-default.md  # optional; built-in default if omitted
```

The `openai_compatible` provider works with any endpoint that speaks the OpenAI `/chat/completions` API — Grok, OpenRouter, local Ollama, etc. Point `api_base_url` at the endpoint and provide a matching token via `api_key_env` (or `api_key` inline, see [Secrets in `config.yaml`](#5-secrets-in-configyaml-inline-vs-env-var)).

### Verdict semantics

| Verdict     | PR state  | Meaning                                                                   |
|-------------|-----------|---------------------------------------------------------------------------|
| `Pass`      | non-draft | No concerns above style nits.                                              |
| `Concerns`  | non-draft | Issues warrant discussion but the diff is mergeable.                       |
| `Block`     | **draft** | At least one issue would cause real harm if merged.                        |

If the LLM's response cannot be parsed for a verdict, the daemon defaults to `Concerns` and prepends a parse-failure note to the report. If the API call itself errors (network, auth, rate limit), the daemon logs the error and still opens the PR with `(reviewer failed: <reason>)` in the `## Code Review` section. **A failed reviewer never blocks PR creation.**

### Block-verdict enforcement (recommended)

autocoder marks Block-verdict PRs as draft. To make this gate merge, configure a branch-protection rule on the PR target branch that **requires PRs not be draft**. Without that rule, anyone with write access can change the draft state and merge.

On hosts that don't support drafts (some private GHE configurations, certain repo types), autocoder falls back automatically: it retries the PR creation with `draft: false` and applies a `do-not-merge` label via the issues-labels endpoint. Configure your branch protection to require the absence of that label as the fallback gate.

### Review context

The reviewer receives a structured bundle, not just a diff. In priority order:

1. **Change context** — the proposal, optional design, and tasks of every OpenSpec change archived in this pass, so the reviewer understands the *intent* of the work.
2. **Changed files (full contents)** — every file touched by the pass, read at the agent-branch state. Whole-file context lets the reviewer evaluate trust boundaries, call sites, and helper definitions — work that a unified diff alone cannot support.
3. **Unified diff** — included last, if the prompt budget allows.

The combined prompt is capped at **2,000,000 characters** (sized for current 1M-token-class models). Files are never partially truncated: if the next file would push the running total over budget, it is skipped in full and named in a `## Skipped (budget exhausted): ...` footer. When files are skipped, the diff is also dropped and replaced by an explanatory message. The default template instructs the model to acknowledge missing context in its first bullet under "Possible bugs" and bias toward `Concerns` over `Pass`.

This is a stopgap until the reviewer is upgraded to an MCP-tool-using model that can `Read`/`Grep` the codebase directly — for now, "send the whole touched surface" gives the reviewer enough information to do a real security review.

### Custom prompt templates

Override the default with `reviewer.prompt_template_path`. Custom templates are **user-owned** — autocoder does not enforce scope on overrides, so you can expand the reviewer to additional dimensions (spec compliance, style guide, etc.) by editing the template.

The template must include the three substitution variables `{{change_context}}`, `{{changed_files}}`, and `{{diff}}`, and must instruct the model to begin its response with `VERDICT: Pass`, `VERDICT: Concerns`, or `VERDICT: Block`. A template still using the retired `{{change_summary}}` placeholder (pre-`reviewer-full-file-context`) will not substitute — the literal text appears in the rendered prompt. Migrate by replacing `{{change_summary}}` with `{{change_context}}`.

### PR composition

Every PR autocoder opens carries the change list in its body, plus the optional `## Code Review` section described above. Immediately after creation, autocoder posts a single follow-up issue comment titled `## Agent implementation notes` with one `### <change-name>` subsection per change in the pass. Each subsection contains the implementer agent's captured stdout from that change's run — the agent's own narrative of what it did, deviations from the spec, and any meta-observations.

The comment is best-effort: if the POST fails, the PR still ships and the failure is logged at ERROR. Source for each section is `<system-temp>/autocoder/logs/<workspace-basename>/<change>.log` (the same per-change log file written by the executor); a missing or unreadable log is logged at WARN and that change's section is omitted. If every change's log is missing, no comment is posted.

The total comment body is capped at 60,000 characters (under GitHub's 65,535 limit, with headroom for wrapper text). When truncated, the tail is replaced with a marker pointing back at `/tmp/autocoder/logs/<basename>/<change>.log` so reviewers can fetch the full output server-side.

---

## Operating Notes

### Workspace path derivation

If a repository entry omits `local_path`, the workspace path is derived deterministically from the URL:

1. Strip the protocol prefix (`git@`, `ssh://`, `https://`, `http://`).
2. Strip a trailing `.git`.
3. Replace any character that is not ASCII alphanumeric, `_`, or `-` with `_`.
4. Prepend `/tmp/workspaces/`.

`git@github.com:owner/repo.git` and `https://github.com/owner/repo.git` both map to `/tmp/workspaces/github_com_owner_repo`. At startup, autocoder runs a collision check: if two configured repositories resolve to the same workspace path (whether by derivation or by explicit `local_path`), the process exits non-zero before spawning any polling tasks. Set `local_path` explicitly to disambiguate.

### Multi-repo setup

`repositories:` accepts any number of entries. autocoder spawns one polling task per entry, each on its own `poll_interval_sec`. Per-repo state is fully independent: an iteration failure on repo A does not affect repo B; a ChatOps escalation on repo A blocks A's pending queue but does not touch B.

```yaml
repositories:
  - url: "git@github.com:my-org/auth-service.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300

  - url: "git@github.com:my-org/web-dashboard.git"
    base_branch: dev
    agent_branch: agent-q
    poll_interval_sec: 3600
```

### Polling cadence and your firewall

When autocoder spawns ≥5 polling tasks at process start, the simultaneous `git fetch` operations from a single source IP can look like a port scan or scraper to network IDS — one operator reported their IDS killing SSH connections the moment the daemon tried to poll 8–9 repos at once. Even without an IDS, tasks that all share the same `poll_interval_sec` (e.g. the default `300`) drift only marginally across iterations because `git fetch` dominates each iteration's wall-clock, so they tend to re-cluster over time.

Two defaults defuse this:

- `executor.startup_jitter_max_secs` (default `30`) — each task waits a uniformly-random `[0, 30]` seconds before its first iteration, smearing the first round of fetches across a 30 s window.
- `executor.inter_iteration_jitter_pct` (default `10`) — each inter-iteration sleep is `poll_interval_sec ± 10%`, so tasks that briefly synchronize drift apart again on the next cycle.

Both jitters cost almost nothing in wall-clock and respect SIGTERM/SIGINT (cancellation is observed within 200 ms during either sleep). Operators on isolated networks who prefer deterministic timing can set both to `0`. Operators who want a wider window — say, after seeing IDS alerts even with the defaults — can raise `startup_jitter_max_secs` to something like `120` or `300`.

### Queue order

Pending changes are processed in ascending entry-name order (UTF-8 byte order, which is alphabetical for ASCII names). Operators with stacked dependencies — i.e. change N+1 depends on change N — encode order explicitly by prefixing change names with a letter+number tag: `a01-rename-foo`, `a02-extract-bar`, `a03-wire-baz`. The prefix is the operator's contract for "this change depends on the previous in sequence." For a second unrelated stack, use a different letter group (`b01-`, `b02-`). For unrelated single changes, no prefix is needed; alphabetical order is arbitrary but deterministic.

Note: OpenSpec rejects change names that start with a digit. Plain `01-`/`02-` prefixes will fail at the prompt-building step (`openspec instructions apply --change <name>` returns "Invalid change name"). Always start with a letter.

Each iteration commits at most `max_changes_per_pr` archived changes (default `3`); any remaining pending changes wait for the next iteration. The cap is configurable per repository, or globally via `executor.max_changes_per_pr`. A long queue therefore ships as several reviewable PRs over time rather than one large PR.

A change that fails (or escalates to chatops) halts the queue walk for that iteration; remaining pending changes wait for the next iteration. This preserves the stacked-dependency assumption behind authoring-order processing: change N+1 may depend on change N having succeeded, so the bot does not attempt N+1 while N is unfixed. A persistently-failing change accumulates failure-counter increments and hits perma-stuck (default after 2 consecutive failures), at which point it drops out of `list_pending` and the queue resumes at N+1.

### Startup preflight

At startup, `autocoder run` invokes `openspec --version` once. If the binary is not on the daemon's PATH or exits non-zero, the daemon exits non-zero before any polling task is spawned. The stderr message names the failure (binary not found, non-zero exit code, etc.). This means a misconfigured deployment surfaces at startup rather than producing empty iterations.

If you see `openspec preflight failed: binary not found on PATH`, add the install directory to the systemd unit's `Environment="PATH=..."` line (see [Deployment](#deployment)).

### Busy marker

At the start of each polling iteration, autocoder writes a per-repo JSON marker at `/tmp/autocoder/busy/<workspace-basename>.json` and holds it through every stage of the pass (executor → review → push → PR). The marker is removed when the pass returns normally. A daemon crash that bypasses normal cleanup (SIGKILL, segfault, host power loss) intentionally leaves the marker for the next pass to discover.

Marker contents: `repo_url`, `pid`, `pgid` (Linux process group for `killpg` recovery), `comm` (process name from `/proc/<pid>/comm` at acquire time), `started_at`, and `stage` (one of `executor`, `commit`, `review`, `push`, `pr`).

On the next iteration's startup, autocoder classifies any pre-existing marker:

| Marker state | Action |
|---|---|
| File absent | Acquire, run iteration |
| Age < `executor.timeout_secs` + 10 min | Skip iteration with INFO log — another pass is working |
| Age over threshold, PID dead | Auto-recover: clear marker, WARN log, proceed |
| Age over threshold, PID alive + `comm` matches | Stuck: `SIGTERM` the process group, wait 5s, `SIGKILL` if still alive, clear marker, post chatops alert, proceed |
| Age over threshold, PID alive + `comm` differs | Ambiguous (PID reuse suspected) — ERROR log, post chatops alert, SKIP iteration, leave marker for human inspection |
| Malformed JSON | Treat as stale: WARN log, clear marker, proceed |

Operators inspecting the file:
```bash
sudo -u autocoder cat /tmp/autocoder/busy/<basename>.json
```

To force a recovery from a stuck state, stop the systemd unit, delete the marker file, and start the unit again:
```bash
sudo systemctl stop autocoder
sudo -u autocoder rm /tmp/autocoder/busy/<basename>.json
sudo systemctl start autocoder
```

The per-change run logs (`/tmp/autocoder/logs/<basename>/<change>.log`) and the busy markers share the same `/tmp/autocoder/` root.

### Workspace directory deleted

If a workspace directory under `/tmp/workspaces/` is removed while autocoder is running (or while stopped), the daemon's next iteration treats this as a fresh-clone case: it clones upstream into the path again. In fork-PR mode it also fetches ONLY the configured agent branch from the `fork` remote at that time (via `git fetch fork +refs/heads/<agent_branch>:refs/remotes/fork/<agent_branch>`) so the local `refs/remotes/fork/<agent_branch>` tracking ref reflects the fork's actual state. Without that fetch the next `git push --force-with-lease fork <agent_branch>` would compare an empty local tracking value against the fork's existing commits and reject with `! [rejected] <agent_branch> -> <agent_branch> (stale info)`, leaving the daemon stuck. The fetch deliberately restricts itself to one branch: a wholesale `git fetch fork` would populate `refs/remotes/fork/<every-branch>`, and if any fork branch shadows an upstream name (e.g. both `origin/dev` and `fork/dev` exist), the next `git checkout <base_branch>` would fail with `fatal: 'dev' matched multiple (2) remote tracking branches`. The post-clone fork fetch is best-effort: if it fails (network blip, fork doesn't yet exist, agent branch doesn't yet exist on the fork), the daemon proceeds and the next push will surface any real divergence via the existing branch-push-failure alert.

### Fork recreation on workspace reinitialization

The default workspace-deleted recovery above preserves whatever state lives on the fork. That is the right behavior when you have open PRs from that fork — losing their head refs would close the PRs. But the same preservation is a liability when the fork has accumulated stale branches no one cares about, or when the fork's state is genuinely worthless and you'd rather start from a pristine mirror of upstream.

Set `github.recreate_fork_on_reinit: true` to opt in to the destructive recovery path. When that flag is enabled AND fork-PR mode is active AND the workspace directory is absent at iteration start, autocoder:

1. Calls `DELETE /repos/<fork_owner>/<repo>` against the GitHub API to delete the fork.
2. Waits 2 seconds for the deletion to propagate.
3. Calls `POST /repos/<upstream_owner>/<repo>/forks` to re-fork from upstream.
4. Polls the new fork's URL via `git ls-remote` for up to 30 seconds until reachable.
5. Proceeds with the normal clone + fork-remote registration.

After a successful re-fork, autocoder posts a one-line chatops notification:

> :warning: \`<repo>\`: re-forked at workspace reinitialization (previous fork deleted; any open PRs from this fork are now closed)

The notification is gated by the same `chatops.notifications.failure_alerts` toggle as the other operator-visible failure alerts.

Requirements:

- The operator's PAT must include the `delete_repo` scope. Without it, the DELETE returns 403, autocoder logs an ERROR naming the missing scope, and falls back to the conservative non-recreating init path (clone + fetch fork). The iteration still makes progress; the fork is unchanged.
- The flag is global on the `github:` block, not per-repository — all configured repos in a single autocoder process share the same fork owner, and the fork-recreation policy is uniform across them.

Defaults to `false`. With the default, the workspace-deleted recovery preserves fork state (see [Workspace directory deleted](#workspace-directory-deleted) above).

### Perma-stuck change detection

When an agent fails the same change two iterations in a row, autocoder marks it perma-stuck: writes a `.perma-stuck.json` marker inside the change directory, posts a chatops alert, and excludes the change from `list_pending` on every subsequent pass until the marker is removed manually. The threshold is `executor.perma_stuck_after_failures` (default `2`, minimum `1`).

What counts as a failure:

- The executor returns `Failed`.
- The executor returns `Completed` but did not modify the workspace (no-op completion).
- The executor returns `Completed` but only renamed the change directory into `archive/` (lazy archive).

What does NOT count (transient infrastructure problems):

- Workspace init / clone / fetch failure.
- `openspec` preflight failure.
- GitHub API transport errors.
- A busy-marker stuck-state that skipped the iteration entirely.

Per-repo counter state lives at `<workspace>/.failure-state.json` (registered in `.git/info/exclude` at workspace init so it never trips the pre-pass dirty check). Successfully archiving a change clears its counter entry; the next failure starts fresh from `1`.

The marker file at `<workspace>/openspec/changes/<change>/.perma-stuck.json` has the schema:

```json
{
  "change": "<change-name>",
  "consecutive_failures": 2,
  "last_reason": "...",
  "marked_stuck_at": "RFC 3339 UTC timestamp",
  "operator_action": "Delete this file to retry the change."
}
```

The chatops alert names the repo, change, count, and a truncated `last_reason`, plus the marker file path. It is subject to the same 24-hour throttle as the predictable-failure alerts: repeat fix-test-fail cycles do not spam the channel. When no chatops backend is configured, the marker is still written and the change is still excluded — an ERROR log is the operator's only signal.

To clear the marker: delete the file. The change re-enters `list_pending` on the next poll. If the underlying problem is not fixed, the change will fail twice more and be marked perma-stuck again (with the 24-hour alert throttle suppressing duplicate notifications inside the window).

### Self-heal for already-implemented changes

When a rebase or merge lands the work for a change on the base branch without moving the change directory into `archive/`, the agent sees the implementation already done and returns `Completed` without modifying the workspace. Normally that's classified as Failed (no-op completion) and retried on every poll, burning tokens to re-confirm the same answer. autocoder self-heals this case instead:

When the executor returns `Completed`, `git status --porcelain` is empty, `openspec validate <change> --strict` exits 0, AND every checkbox in `openspec/changes/<change>/tasks.md` is `[x]`, autocoder runs the archive move itself, commits it with subject `archive: <change>: implementation already in base`, and ships a PR through the normal push + PR flow.

If any of the four preconditions fails — including `openspec validate` erroring or any task still `[ ]` — autocoder falls through to the existing Failed path, so non-self-heal cases retain their prior behavior.

The PR body for a pass that self-healed one or more changes is prefixed with:

> _This PR archives one or more changes whose implementation was already present on the base branch. No code diff is included; only the openspec archive move._

The disclaimer identifies these passes for reviewers regardless of whether the pass also includes normally-implemented changes.

### Skipping iterations while a PR is open

Before each polling iteration begins its work, autocoder queries GitHub for open PRs whose `head` matches the configured agent branch (`<fork_owner>:<agent_branch>` in fork-PR mode, `<repo_owner>:<agent_branch>` in direct mode, base = the configured base branch). If an open PR is found, the iteration is skipped: no executor invocation, no commits, no push, no PR creation attempt. The skip persists until the open PR is closed or merged. This prevents the daemon from re-implementing the same changes on every poll while a PR sits awaiting review, which would otherwise force-push new commits over the PR's branch and burn agent tokens redundantly.

To re-implement after rejecting a PR: close it (don't merge). The next poll proceeds. To accept the implementation: merge it; the archive moves land on the base branch and the changes drop out of `list_pending`.

If the GitHub query itself fails (transport error, non-2xx), the iteration proceeds as if no PR existed — better to incur a redundant Claude run than to halt the repo on a flaky API. The failure is logged at WARN.

### Periodic audits

Beyond the OpenSpec change queue, autocoder runs a periodic-audit framework: a set of registered audits that fire on per-audit cadences, write per-invocation logs, and (depending on the audit) post chatops findings or write new OpenSpec changes that feed back into the queue.

The framework is **default-off**. With no `audits:` block in the config, every registered audit's effective cadence resolves to `disabled` and the daemon behaves exactly as it did before the framework existed. Operators opt in explicitly per audit.

**Registered audit type names:**

| Slug | What it does | LLM | Default cadence | WritePolicy |
|---|---|---|---|---|
| `architecture_brightline` | Pure-code metrics — file size, duplicate signatures across files. Surfaces oversize files and accidental copies. | No | `disabled` (opt-in via `audits.defaults` or per-repo) | `None` (read-only) |

Each audit declares a `WritePolicy`:

- **`None`** — sandbox blocks `Write`/`Edit`; after `run()` returns the framework runs `git status --porcelain -uall` and asserts the workspace is clean. Any unexpected diff is treated as failure: the state file is NOT updated (so the cadence retriggers on the next iteration), the diff is reverted via `git reset --hard HEAD` + `git clean -fd`, and a throttled chatops alert under the `audit_write_policy_violation` category is posted.
- **`OpenSpecOnly`** — sandbox allows `Write`/`Edit`; after `run()` returns every modified or new path must begin with `openspec/changes/`. A diff outside that prefix triggers the same failure handling.
- **`Approved`** — full write access. Reserved for future audits with broader scope; not used by any audit shipped today.

**Cadence configuration:**

```yaml
audits:
  defaults:
    architecture_brightline: weekly      # disabled | daily | every-N-days | weekly | monthly | quarterly
  settings:
    architecture_brightline:
      notify_on_clean: false             # silence is success; set true for an explicit ✅ post each clean run
      extra:
        file_lines_threshold: 800        # override the brightline default (800)

repositories:
  - url: "git@github.com:my-org/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
    audits:
      architecture_brightline: every-3-days   # per-repo override of the global default
```

Per-repo entries under `repositories[].audits` override the corresponding `audits.defaults` entry for that repository only. An audit name that does not match a registered slug fails config validation at startup with a list of the known names; this prevents typos silently disabling an audit.

**When audits fire:** Each polling iteration, after `recreate_branch` and BEFORE `list_pending`. This ordering means a spec-writing audit (`AuditOutcome::SpecsWritten(...)`) creates `openspec/changes/<name>/` and the same iteration's queue walk picks the new change up — implementer commit and audit's creation commit ship in one PR.

**`requires_head_change` semantics:** Audits that compute over the codebase (like `architecture_brightline`) declare `requires_head_change = true`; the scheduler skips them when the base-branch HEAD SHA matches the recorded `last_run_sha`, regardless of cadence. Audits whose inputs are external (package registries, GitHub PR lists) return `false` and run on cadence alone.

**Audit-run logs:** Every invocation (success, failure, violation) writes a timestamped log file at:

```
/tmp/autocoder/logs/<workspace-basename>/audits/<audit_type>-<UTC-RFC3339-with-Z>.log
```

The log contains: the audit type, workspace path, start/end timestamps, the resolved cadence, the last-run record (if any), the prompt (for LLM audits), the raw audit output, every finding's full body, and the final `AuditOutcome` variant. The directory is created on first use. Cleanup is operator-driven (same model as the per-change run logs).

**State file:** Per-workspace audit run state lives at `<workspace>/.audit-state.json`. The file is autocoder bookkeeping and is registered in `.git/info/exclude` at workspace init so it does not trip the pre-pass dirty check. Missing/unparseable file → "no audits have ever run" (every audit is eligible on its next due iteration). Lost state safely re-runs all audits on schedule.

**Outcome dispatch:**

- `AuditOutcome::Reported(findings)` → chatops post with header `📋 <repo>: <audit_type> — N finding(s)` and a bullet list of severity-glyphed subjects (low: `•`, medium: `⚠`, high: `🔴`). Default per-finding excerpt is 200 chars; full bodies live in the audit-run log. Empty findings vector is silent unless `notify_on_clean: true`.
- `AuditOutcome::SpecsWritten(names)` → no chatops post (the existing start-of-work + PR-opened notifications cover the subsequent flow). The framework logs an info line naming each created change.
- `AuditOutcome::NoFindings` → silent.

**Failure modes:**

- An audit returning `Err` is logged at ERROR; the state file is NOT updated for that audit; the iteration continues to the remaining audits and then to `list_pending`. Other audits in the registry still run.
- A WritePolicy violation is treated the same way (state untouched), additionally reverts the workspace and posts the throttled `audit_write_policy_violation` chatops alert.

### Recovering from a bad run

The `rewind` subcommand discards the in-flight agent branch and re-queues one or more archived changes. See [CLI Reference → rewind](#rewind) below.

### Dirty workspace auto-recovery

If a workspace under `/tmp/workspaces/` is left dirty between polls (uncommitted edits, untracked files, or a checked-out branch other than the base), autocoder recovers automatically at the next startup or poll cycle: it checks out the configured `base_branch`, runs `git reset --hard origin/<base_branch>`, and runs `git clean -fd`. The repo then re-enters its normal polling loop. If recovery itself fails (e.g. the remote is unreachable), the repo is skipped for the daemon's lifetime and an error is logged — restart the daemon once the underlying problem is fixed.

Operators who want to inspect a dirty workspace before any daemon action should stop the systemd unit first:

```bash
sudo systemctl stop autocoder
# inspect /tmp/workspaces/<repo>/ at your leisure
sudo systemctl start autocoder
```

### Runtime control: live config reload

A running daemon exposes a Unix-domain control socket at `<system-temp>/autocoder/control/control.sock` (typically `/tmp/autocoder/control/control.sock` on Linux). The file is created on startup with mode `0600` and owned by the user running the daemon — only that user can connect. The socket file is removed at shutdown.

The `autocoder reload` subcommand connects to the socket, sends `{"action":"reload"}`, and prints the daemon's response. The daemon re-reads the YAML config from the same path it was launched with, validates it (parse + workspace-collision + token-route checks), and either rejects the request or hot-applies the safe subset of changes.

What gets hot-applied:

- `github` — per-owner tokens, default `token_env`, `fork_owner`. Applied at the next iteration boundary for each repository.
- `reviewer` — provider, model, API key, prompt template. In-flight reviews finish with the previous reviewer; subsequent reviews use the new one.
- `chatops` — backend selection, default channel, notification flags. In-flight notifications finish with the previous backend; subsequent ones use the new one.
- `repositories` — adding, removing, or modifying repositories in the list. New entries are spawned as fresh polling tasks (workspace setup, dirty-check, busy-marker — same as daemon startup). Removed entries get their per-repo cancellation token fired; the running task finishes its in-flight iteration normally (including push + PR) and exits at the next inter-poll sleep boundary. Modified entries hot-swap an `Arc<ArcSwap<RepositoryConfig>>` holder so the next iteration of that task reads the new `base_branch`, `agent_branch`, `poll_interval_sec`, `chatops_channel_id`, `local_path`, or `max_changes_per_pr`. The reload handler diffs the new list against the current task set by `url` — that field is the identity key. Changing the `url` of an existing entry is treated as `remove old_url + add new_url`. Reordering the list has no effect.

What requires a full restart:

- `executor` — only one executor instance exists, shared across tasks. Changes to `executor:` fields are reported under `requires_restart`.

Response shape on success:

```json
{
  "ok": true,
  "applied": ["github", "reviewer", "repositories"],
  "requires_restart": ["executor"],
  "unchanged": ["chatops"],
  "repositories_delta": {
    "added": ["git@github.com:owner/repo-c.git"],
    "removed": ["git@github.com:owner/repo-a.git"],
    "changed": ["git@github.com:owner/repo-b.git"]
  }
}
```

`repositories_delta` is always present (the three arrays can each be empty) so client tooling has a consistent shape to parse. An entry only appears under one of `added` / `removed` / `changed` per reload.

Validation rejection is non-disruptive: if the new YAML fails to parse or fails semantic validation, the daemon continues running with the previous in-memory config. The response is `{"ok": false, "error": "<message>"}` naming the failure, and the CLI exits non-zero. If the daemon is not running (or is running under a different user), the CLI prints an error naming the expected socket path and hinting at the cause.

#### Adding a repository at runtime

To add a repository without restarting the daemon:

1. Edit `config.yaml` (the path the daemon was launched with) and append the new entry under `repositories:`. Set its `url`, `base_branch`, `agent_branch`, and `poll_interval_sec` as usual.
2. Run `sudo -u autocoder autocoder reload` from the same host. The CLI prints the daemon's response.
3. Verify the response includes the new URL under `repositories_delta.added` and `"repositories"` appears in `applied`. The polling task is now running; it does workspace initialization on its first pass.

The reverse (remove a repository) works the same way: delete the entry, reload, and the new URL appears under `repositories_delta.removed`. The cancelled task finishes its current iteration before exiting, so a removal during an active push or PR step completes cleanly.

#### In-flight iteration safety

A repo cancelled mid-iteration finishes its in-flight pass normally. The cancellation check sits in the inter-poll `tokio::select!`, so the next poll never starts after the cancel — but the current one runs to completion. A modify-in-place is observed at the *next* iteration; the current iteration uses the old snapshot. Both rules eliminate mid-iteration tearing of `RepositoryConfig` fields.

If you remove a repo and re-add it (or change a setting) before the previous task has fully exited (e.g. it is mid-push when the reload lands), the response logs a WARN and reports the URL as unchanged for that reload. Run `autocoder reload` again after a brief wait; the second reload sees the URL as absent and re-adds it cleanly.

---

## Deployment

For production, run autocoder as a systemd service on a dedicated Linux host. The daemon polls on its own — do not wrap it in a cron job.

### 1. Install the binary

```bash
cargo build --release
sudo cp target/release/autocoder /usr/local/bin/autocoder
```

### 2. Create a deploy user and authenticate Claude Code

```bash
sudo useradd -m -s /bin/bash autocoder
sudo -u autocoder -i                            # become the deploy user
claude auth login                                # interactive Anthropic OAuth
git config --global user.email "autocoder@$(hostname)"
git config --global user.name "autocoder"
exit                                             # back to your admin shell

# Install openspec so the executor can generate richer prompts via
# `openspec instructions apply`. Without it the daemon falls back to
# raw markdown concatenation which gives the agent less guidance.
sudo -u autocoder npm install -g @fission-ai/openspec
sudo -u autocoder openspec --version             # verify
```

The Claude credentials now live at `/home/autocoder/.claude/`. The git config writes to `/home/autocoder/.gitconfig` and is required — autocoder's commit step fails without an author identity. Both survive restarts as long as the systemd unit runs as the same user.

(If `npm` isn't on the autocoder user's `$PATH`, install Node.js first via your distro's package manager or `nvm`. The exact openspec install command may vary; check the openspec project for the current recommendation.)

### 3. Set up SSH for the autocoder user

Required for `config.yaml` repositories using SSH URLs (`git@github.com:...`), which is the recommended form for multi-owner setups. The autocoder user needs an SSH key tied to a GitHub identity with access to exactly the configured repositories — no more.

Generate the keypair and pre-accept github.com's host key:

```bash
# Generate a passphrase-less key for the autocoder user. The outer single
# quotes are required so `-N ""` survives sudo's argument handling.
sudo -u autocoder bash -c 'mkdir -p ~/.ssh && ssh-keygen -t ed25519 -C "autocoder@$(hostname)" -f ~/.ssh/id_ed25519 -N ""'

# Pre-accept github.com's host key so the daemon never hits an interactive prompt.
sudo -u autocoder bash -c 'ssh-keyscan github.com >> ~/.ssh/known_hosts && chmod 600 ~/.ssh/known_hosts'

# Print the public key to register with GitHub.
sudo -u autocoder cat /home/autocoder/.ssh/id_ed25519.pub
```

Then register the public key against a GitHub identity. **Pick one of the three options below** based on your security posture:

#### Option A — Machine user (recommended for orgs with real users)

Create a dedicated GitHub account (e.g. `<your-handle>-autocoder`) that exists only to be autocoder. Add it as a member of a team in each org with access to only the repositories in `config.yaml`, then register the SSH key on the machine user's account (*Settings → SSH and GPG keys → New SSH key*).

Required team-grant permission level:

- **Read** if you use [Fork-and-PR mode](#7-fork-and-pr-workflow-recommended-for-org-repos) (recommended). The machine user only reads upstream and pushes to its own fork.
- **Write** if you use direct-push mode (no `github.fork_owner` set). The machine user pushes the agent branch directly to upstream.

Mint the PATs you set in `config.yaml`'s `github.owner_tokens` from the machine user too — same scoping principle: the credential's authority matches autocoder's job. A full compromise of the autocoder host then gives the attacker exactly the access you granted that user and nothing more.

GitHub's terms of service permit machine users for automation. The account is free.

#### Option B — Per-repo deploy keys (works without a separate identity)

Add the same public key as a deploy key on each repo: *Repo settings → Deploy keys → Add deploy key*, with **"Allow write access"** checked so autocoder can push the agent branch.

Caveat: GitHub enforces that any given public key can be registered as a deploy key on **exactly one repo** across the platform. If autocoder manages N repos, you need N keypairs in `~autocoder/.ssh/` plus a `~/.ssh/config` with per-host routing — e.g.:

```
Host github.com-org-a-repo-1
  HostName github.com
  IdentityFile ~/.ssh/id_ed25519_org_a_repo_1
  IdentitiesOnly yes
```

Then the `config.yaml` URL becomes `git@github.com-org-a-repo-1:org-a/repo-1.git`. Manageable up to a handful of repos; tedious past that.

#### Option C — Personal-account key (small personal-repo setups only)

Register the key under your own `Settings → SSH and GPG keys → New SSH key`. The autocoder daemon will then act as you for all git operations, with whatever permissions you have. **Do not use this for organization repos with real users** — a compromised autocoder host can `git push` anywhere you can. Acceptable only for solo developers managing their own personal repos.

#### Verify

```bash
sudo -u autocoder ssh -T git@github.com
# Expected: "Hi <user>! You've successfully authenticated, but GitHub does not provide shell access."
```

`<user>` will be whichever identity you registered the key under (the machine user, your own account, or — for deploy keys — empty since deploy keys don't have a user identity).

### 4. Stage the working directory

```bash
sudo mkdir -p /home/autocoder/autocoder
sudo cp config.example.yaml /home/autocoder/autocoder/config.yaml
sudo chown -R autocoder:autocoder /home/autocoder/autocoder
sudo -u autocoder $EDITOR /home/autocoder/autocoder/config.yaml   # edit repo URLs, and inline secrets if you chose that path
sudo chmod 600 /home/autocoder/autocoder/config.yaml              # restrictive perms regardless of secret path
```

### 5. Set up the systemd service

Pick one of the two secret-delivery paths below depending on what you put in your `config.yaml` (see [Secrets in `config.yaml`](#5-secrets-in-configyaml-inline-vs-env-var)).

#### Path A — inline secrets (recommended for single-host deployments)

With secrets inline in `config.yaml` (`github.token`, `reviewer.api_key`, `chatops.slack.bot_token`), the unit needs no env vars. Create `/etc/systemd/system/autocoder.service`:

```ini
[Unit]
Description=autocoder — autonomous OpenSpec implementation daemon
After=network.target

[Service]
Type=simple
User=autocoder
WorkingDirectory=/home/autocoder/autocoder

# PATH must include the directories containing `claude` and `openspec` — both
# are invoked by name. systemd does not inherit the operator's interactive
# PATH. `which openspec claude` as the deploy user is the authoritative check.
Environment="PATH=/usr/local/bin:/usr/bin:/bin"

ExecStart=/usr/local/bin/autocoder run --config /home/autocoder/autocoder/config.yaml
Restart=on-failure
RestartSec=60

[Install]
WantedBy=multi-user.target
```

`openspec` must be on autocoder's PATH. The daemon runs `openspec --version` at startup and exits non-zero with a clear stderr message if the binary is missing. Confirm with `sudo -u autocoder which openspec`. The per-change run log at `/tmp/autocoder/logs/<workspace>/<change>.log` records the prompt sent to Claude under a `=== PROMPT (n bytes) ===` header for inspection.

#### Path B — env-var secrets (multi-user hosts, classical production pattern)

With `*_env` fields in `config.yaml` (no inline secrets), add an `EnvironmentFile=` directive pointing at a separate, root-owned env file:

```ini
[Unit]
Description=autocoder — autonomous OpenSpec implementation daemon
After=network.target

[Service]
Type=simple
User=autocoder
WorkingDirectory=/home/autocoder/autocoder

# PATH must include the directories containing `claude` and `openspec`.
# See Path A above for the rationale.
Environment="PATH=/usr/local/bin:/usr/bin:/bin"

# Required only if your config.yaml uses *_env fields (env-var secret path).
EnvironmentFile=/etc/autocoder.env

ExecStart=/usr/local/bin/autocoder run --config /home/autocoder/autocoder/config.yaml
Restart=on-failure
RestartSec=60

[Install]
WantedBy=multi-user.target
```

Create `/etc/autocoder.env` (mode `0600`, owned by root):

```
# Single-owner setups: a single PAT named by `github.token_env` in config.yaml.
GITHUB_TOKEN=ghp_yourtokenhere

# Multi-owner setups (see "Multiple GitHub Tokens" above): one PAT per owner.
# Uncomment and adjust to match the env var names referenced from
# `github.owner_tokens:` in config.yaml. GITHUB_TOKEN can be omitted if
# every configured repository's owner has an explicit route.
# PERSONAL_GH_TOKEN=github_pat_xxx_personal
# ORG_A_GH_TOKEN=github_pat_xxx_org_a
# ORG_B_GH_TOKEN=github_pat_xxx_org_b

# Optional, only if the matching config block is enabled and uses *_env:
# ANTHROPIC_API_KEY=...
# SLACK_BOT_TOKEN=xoxb-...        # chatops.provider: slack
# DISCORD_BOT_TOKEN=...           # chatops.provider: discord (EXPERIMENTAL)
# TEAMS_CLIENT_SECRET=...         # chatops.provider: teams (EXPERIMENTAL)
# MATTERMOST_TOKEN=...            # chatops.provider: mattermost (EXPERIMENTAL)
# MATRIX_ACCESS_TOKEN=...         # chatops.provider: matrix (EXPERIMENTAL)
```

The two paths can be mixed per-secret — e.g. inline `github.token` alongside `reviewer.api_key_env: ANTHROPIC_API_KEY` — in which case the unit needs `EnvironmentFile=` and the env file carries only the env-var-sourced secrets.

### 6. Start it

```bash
sudo systemctl daemon-reload
sudo systemctl enable autocoder
sudo systemctl start autocoder
sudo journalctl -u autocoder -f      # tail logs
```

### Applying config changes without a restart

Edit `config.yaml`, then run:

```bash
sudo -u autocoder autocoder reload
```

The `autocoder reload` subcommand connects to the daemon's control socket at `/tmp/autocoder/control/control.sock`. That socket is created on startup with mode `0600` and is owned by the user the daemon runs as (the `autocoder` user in this guide), so any reload command must run as the same user — `sudo -u autocoder` is the standard invocation. The daemon re-reads `config.yaml` from the path it was launched with, validates it, and hot-applies the `github`, `reviewer`, `chatops`, and `repositories` sections at the next iteration boundary for each repo. Only changes to `executor:` are not hot-applied; the response names that under `requires_restart` so you know it still needs `systemctl restart autocoder`. See [Runtime control: live config reload](#runtime-control-live-config-reload) above for the full response shape and validation-rejection semantics.

### Upgrading

Build the new release, copy the binary, restart the unit:

```bash
cd /path/to/cicd-impl-agents
git pull
cargo build --release
sudo cp target/release/autocoder /usr/local/bin/autocoder
sudo systemctl restart autocoder
```

If you were on an older version that installed under `/usr/local/bin/openspec-orchestrator` or used a service unit named `openspec-orchestrator.service`, remove those before installing the rename:

```bash
sudo systemctl stop openspec-orchestrator 2>/dev/null
sudo systemctl disable openspec-orchestrator 2>/dev/null
sudo rm -f /etc/systemd/system/openspec-orchestrator.service /usr/local/bin/openspec-orchestrator
sudo systemctl daemon-reload
```

---

## AI Security & Guardrails

Running an autonomous coding agent with push access to your repositories introduces unique risks. Adhere to the following practices.

### 1. Credential scoping

Never give autocoder a Personal Access Token (PAT) or SSH key with admin access to your organization. Provide it with **scoped access** strictly limited to the repositories defined in `config.yaml`. There are two paths:

- **Fine-grained PAT minted by your own account**, with the PAT's repository allowlist restricted to the autocoder-managed repos. The PAT enforces the scope.
- **Classic PAT minted by a machine user** whose account-level access is itself scoped (Read collaborator on specific repos via team membership). The minting user's permissions enforce the scope, not the PAT.

Either path is acceptable; what matters is that the credential cannot push, merge, or change settings outside the configured repos. An org-wide classic PAT minted by your own admin account is the configuration to avoid.

### 2. Branch protection

Protect your `main` and `dev` branches. autocoder must **never** be allowed to push directly to protected branches. It pushes only to the designated `agent_branch` and opens PRs for human review. Configure GitHub branch protection to require PR approval and (optionally) require PRs not be draft, so the reviewer's `Block` verdict actually gates merge.

### 3. The "self-modifying AI" risk

If you point autocoder at its own repository (e.g. `cicd-impl-agents`), there is a risk of the agent modifying its own source code in unexpected ways. A "lazy" LLM under pressure might try to satisfy failing tests by deleting them, modify the OpenSpec schema to avoid spec checks, or alter its own system prompts.

**Mitigation:** require human + reviewer-agent approval for any PR merged into autocoder's own repository. Never auto-merge autocoder's PRs into itself without a human in the loop.

### 4. Workspace isolation

autocoder clones repositories into `/tmp/workspaces/`. Ensure this partition has sufficient disk space and gets cleared of orphaned files on system restart (most distros mount `/tmp` as tmpfs by default, which handles this). Do not run autocoder with root privileges. The deploy user only needs:

- Write access to `/tmp/workspaces/`
- Write access to its own `~/.claude/` (for Claude Code credentials)
- Read access to `/home/autocoder/autocoder/config.yaml`

### 5. Secrets in `config.yaml` (inline vs env-var)

Every secret-bearing field (`github.token` / `github.owner_tokens[*]` / `reviewer.api_key`) accepts EITHER an env-var name (the original pattern) OR an inline value via the `{ value: "..." }` shape. Examples:

```yaml
github:
  token_env: GITHUB_TOKEN                   # env-var path
  # OR
  token:
    value: "github_pat_xxx"                 # inline
  owner_tokens:
    my-personal-handle: PERSONAL_GH_TOKEN   # env-var name
    my-org-a:                               # inline
      value: "github_pat_for_org_a"

reviewer:
  api_key_env: ANTHROPIC_API_KEY            # env-var path
  # OR
  api_key:
    value: "sk-ant-..."                     # inline
```

When both forms are set on the same logical field, the inline value wins and autocoder logs a `warn`-level line at startup naming the env var being ignored. Startup logs name the source (`inline (github.token)` or `env var GITHUB_TOKEN`) so an audit can confirm which secrets live in YAML.

**Env-var form:** secrets stay out of `config.yaml`. Suits multi-user hosts and systemd deployments with `EnvironmentFile=/etc/autocoder.env`.

**Inline form:** secrets live in `config.yaml`. Suits single-host, single-user deployments where one file is easier to manage than two. Requirements:

- `chmod 600` on the config file, owned by the autocoder user.
- Never commit it. The project root's `.gitignore` already excludes `config.yaml` by name.

### 6. Dedicated, non-SSH user (recommended)

Run autocoder as a dedicated user (`autocoder`) with no SSH login. Authenticate Claude Code as that user (`sudo -iu autocoder claude auth login`) and keep `config.yaml`, `~/.claude/`, and the daemon's process under that uid. A compromised login user must then clear an additional uid boundary to reach autocoder's secrets — meaningful when the login user is not a passwordless sudoer. The Deployment section's systemd setup follows this pattern.

### 7. Fork-and-PR workflow (recommended for org repos)

By default, autocoder pushes the agent branch directly to upstream and opens a same-repo PR. This requires the autocoder identity to hold push access on every managed repo. Branch protection on `main`/`dev` limits the damage of a compromise but leaves all other branches reachable.

Fork-and-PR mode collapses the blast radius to "what an external open-source contributor could already do." Set `github.fork_owner` to the handle that owns the forks (typically the machine user from section 6):

```yaml
github:
  fork_owner: my-machine-user-handle
  owner_tokens:
    UpstreamOrg:
      value: "github_pat_..."
```

In this mode autocoder:

- Pushes the agent branch to `git@github.com:my-machine-user-handle/<repo>.git` (the fork)
- Opens cross-repository PRs with `head: "my-machine-user-handle:agent-q"` against the upstream
- Never writes to upstream branches; the machine user only needs **read** access on upstream

**One-time setup per repo:**

1. The machine user must have **Read** access to the upstream repo (collaborator invitation, team membership, or — for public repos — no setup required). Read is enough on github.com because the only API calls the bot makes against upstream are `POST /pulls` (Read can do this), `POST /forks` (creates a fork to the bot's own account; the bot's PAT owns the destination), and — only if the host rejects drafts — `POST /labels` (this needs **Triage**, but github.com supports drafts everywhere so the label fallback never fires there). Grant Triage only if you deploy against a GitHub Enterprise host that rejects draft PRs.
2. **Forks are created automatically.** On first startup, autocoder probes each configured repo's fork URL and, when missing, calls `POST /repos/<upstream>/<repo>/forks` to create it under `fork_owner`, then polls (up to 60s) for the fork to become reachable before proceeding. Adding a new repo to `config.yaml` and restarting the daemon is a complete workflow — no manual fork step.
3. PATs in `github.owner_tokens` (or `github.token`) should be minted by the machine user. Fine-grained: scope to "Pull requests: read & write" + "Administration: write" (the latter is required to fork) on the upstream repo — no `Contents: write` needed. Classic: `repo` scope covers PR creation AND fork creation. The machine user's account-level access provides the actual repo scoping.

**Startup check:** autocoder probes each fork with `git ls-remote` before spawning any polling task. Missing forks are created automatically via the GitHub API and then polled (up to 60s) for reachability. A creation failure (e.g. PAT lacks fork permission, or upstream is inaccessible) produces an aggregated startup error naming every repo that failed.

**Rewind in fork mode:** `autocoder rewind --hard` deletes the agent branch from the fork (not upstream), since that's where it lived.

**Limitations:** `fork_owner` is global; one machine user owns the forks for every repo in the config. Per-repo overrides are not supported. Two upstream repos with the same name (across different orgs) would map to the same fork URL — set explicit `local_path` and/or rename one fork to disambiguate.

### 8. Executor tool sandbox

The agent CLI (Claude Code by default) runs inside the workspace with whatever tool access its defaults allow. autocoder constrains this via per-iteration Claude Code settings files that block exfiltration channels by default.

**Default deny rules** (active when `executor.sandbox` is absent from `config.yaml`):

- **Bash commands:** `curl`, `wget`, `nc`/`ncat`/`netcat`, `ssh`/`scp`/`sftp`/`rsync`, `git push`, `git remote *`, `git fetch <url>`, `openspec archive`, `openspec unarchive`. Build/test commands (`cargo`, `npm`, `pytest`, `go test`, etc.) are not on the list.
- **File reads:** `/home/*/.ssh/**`, `/home/*/.claude/**`, `/etc/shadow`, `/etc/ssl/private/**`.
- **Tools:** `Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash` allowed. `WebFetch`, `WebSearch`, and any other tools NOT allowed.

**Customizing:** set `executor.sandbox` in `config.yaml`. Each field overrides its safe default independently; omitted fields keep their defaults.

```yaml
executor:
  kind: claude_cli
  sandbox:
    # If your project's build needs HTTPS (pip install, brew install, etc.),
    # restate disallowed_bash_patterns with `curl:*` omitted:
    disallowed_bash_patterns:
      - "nc:*"
      - "ncat:*"
      - "ssh:*"
      - "git push:*"
      - "git remote *"
```

**What the agent sees on a denial:** the wrapped CLI tells the model the tool call was blocked. The model typically narrates the failure in its output, which surfaces in the iteration's captured stdout. If a legitimate workflow gets blocked, the iteration logs make it obvious which command was denied.

**Threat model caveat:** this is a tool-routing-layer sandbox, not OS-level isolation. A determined model can in principle exec arbitrary code via the allowed `Bash` tool with command patterns that don't match the denylist. For hard isolation, run autocoder under OS-level sandboxing (firejail, bubblewrap, or systemd `ProtectHome=`/`ProtectSystem=`). The autocoder sandbox is a useful first layer.

**Lazy-archive structural detection.** Beyond the sandbox, autocoder inspects the working-tree diff after every executor invocation. If the only changes are renames into `openspec/changes/archive/<date>-<name>/`, the daemon treats the iteration as Failed (not Completed), reverts the staged moves via `git reset --hard`, and leaves the change pending for retry. This catches the "agent renamed the change directory and called itself done" failure mode regardless of which command produced the moves — the openspec-CLI denials above are belt-and-suspenders for the obvious path, but the structural check is what does the real work.

**Reviewer LLM is a separate data flow.** The code reviewer (if enabled) sends the diff to its configured LLM provider as a direct HTTP call. That data flow is governed by your `reviewer:` config (provider, api_key, api_base_url), NOT by `executor.sandbox`. Operators opted in to that flow by enabling the reviewer; sandbox restrictions do not apply.

---

## CLI Reference

```
autocoder <COMMAND>
```

### `run`

Start the polling daemon.

```bash
autocoder run --config <path-to-config.yaml>
```

The daemon polls every configured repository on its interval, processes ready OpenSpec changes, and opens monolithic PRs. Terminates only on SIGINT, SIGTERM, or a fatal initialization error. Logs go to stderr; control verbosity with `RUST_LOG=info` (default), `RUST_LOG=debug`, etc.

### `reload`

Ask a running daemon to re-read its YAML config and hot-apply the `github`, `reviewer`, and `chatops` sections.

```bash
sudo -u autocoder autocoder reload
```

The CLI connects to the daemon's Unix-domain control socket at `/tmp/autocoder/control/control.sock`, sends `{"action":"reload"}`, and prints the daemon's pretty-printed JSON response to stdout (exit 0) or stderr (exit non-zero). The socket file is mode `0600` and owned by the user the daemon runs as, so the CLI must run as the same user — hence `sudo -u autocoder`. If the daemon is not running, the CLI prints an error naming the expected socket path and exits non-zero. See [Runtime control: live config reload](#runtime-control-live-config-reload) for the full behavior.

### `rewind`

Discard the in-flight agent branch and re-queue one or more archived changes. Use this when an agent produced unusable work or a PR was rejected and you want the daemon to try again.

```bash
# Soft rewind (single-repo config): prompt for confirmation, then delete
# the local agent branch and unarchive one change.
autocoder rewind my-broken-change --config config.yaml

# Hard rewind: skip the prompt, delete local AND remote agent branch,
# then unarchive two changes.
autocoder rewind change-A change-B --config config.yaml --hard

# Multi-repo config: --repo is REQUIRED. The selector matches either the
# full URL or the short-name (basename minus .git).
autocoder rewind my-change --config config.yaml --repo my-repo
```

**Soft vs hard semantics:**

| Mode     | Confirmation prompt | Local agent branch | Remote agent branch                       |
|----------|---------------------|--------------------|-------------------------------------------|
| soft     | y/N, defaults no    | deleted            | left intact                                |
| `--hard` | skipped             | deleted            | deleted (failures logged but non-blocking) |

The confirmation prompt for soft rewind looks like:

```
This will delete branch 'agent-q' (local) and unarchive 1 change(s) (my-broken-change). Proceed? [y/N]
```

Bare Enter, `n`, or any input other than `y`/`Y` declines and exits without modifying any state.

**`--repo` selector:**

With **one** configured repository, `--repo` is optional and defaults to that repo. With **two or more** configured repositories, `--repo` is required. autocoder matches the selector against each repository's full URL (exact equality) AND against the URL's short-name (basename with any trailing `.git` stripped). Zero matches or multiple matches exit non-zero with an error listing the available selectors.

**Unarchiving multiple changes:**

If you pass multiple change names and one of them fails to unarchive (typo, no matching archive entry, destination collision), the remaining names are still attempted. The process exits non-zero at the end with a summary naming both the succeeded and failed changes.

**Recovering from an accidental rewind:**

Archived directories are **not** deleted by archive — they are renamed under `openspec/changes/archive/<YYYY-MM-DD>-<name>/`. To reverse an accidental rewind, move the directory back into the archive manually. The canonical date-prefix format is preserved by autocoder's `archive` step, so a manual `mv` restores the queue's view of state.

---

## Status & Roadmap

The seven capabilities listed under [Architecture](#architecture) are all **implemented and tested**. autocoder runs end-to-end against real GitHub repositories with the Claude CLI as executor and (optionally) Slack as the officially-supported escalation channel. The four experimental ChatOps backends (Discord, Teams, Mattermost, Matrix) compile and have unit-test coverage against recorded fixtures but no live-service validation; operators who deliberately select one are the ones surfacing bugs.

The following capabilities are **explicitly aspirational** — referenced in design documents but not built:

- **Verifier** *(planned; not in any active change)*: a spec-audit step that runs alongside the code reviewer and asks "did the diff actually implement the spec?" The reviewer agent currently focuses on code quality and explicitly does not assess spec compliance. Until the verifier ships, spec correctness is a human-review concern.
- **Drift audit** *(planned; not in any active change)*: a periodic whole-repo verification that catches gradual divergence between the baseline `openspec/specs/` and the code. Until this ships, the per-change architecture cross-reference (run once at change-archive time) is the closest equivalent.

Other items deferred without a current owner:

- **Multi-instance distributed deployment.** autocoder assumes single-instance ownership of each configured workspace; running two daemons against the same `local_path` would race. Out of scope for the current architecture.
- **Per-repo executor configuration overrides.** The `executor:` block is global; mixing Claude on one repo and a different backend on another in the same config is not supported.
- **Streaming or incremental code review.** The reviewer sends the full diff in one LLM call; truncation at 100k chars is documented in `prompts/code-review-default.md`.

To request an aspirational item, file an issue or open an OpenSpec change proposal in this repository. Self-modification guardrails apply when autocoder works on its own codebase; see [AI Security & Guardrails](#ai-security--guardrails).

---

*Documentation maintained per the `project-documentation` OpenSpec rule. Any new capabilities or operational shifts must be updated here in the same change that introduces them.*
