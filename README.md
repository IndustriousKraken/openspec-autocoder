# autocoder

**autocoder** is an autonomous daemon that reads OpenSpec implementation proposals from one or more configured repositories, drives an AI coding agent (the Claude CLI by default) through each change in serial order, and opens monolithic Pull Requests for human review. It's "OpenSpec change at the top, working code in a PR at the bottom" wired into a single long-running process.

---

## Quick Start

Get from `git clone` to a running daemon in about ten minutes. Each step is self-contained; do them in order.

### 1. Prerequisites

On the machine where the daemon will run:

- **Rust toolchain.** Install via [rustup](https://rustup.rs/) — autocoder builds against stable Rust on edition 2024.
- **Claude Code authenticated.** Install [Claude Code](https://www.anthropic.com/claude-code) and run `claude auth login` as the same OS user that will run the daemon. The credentials are persisted in `~/.claude/` and survive restarts.
- **A GitHub fine-grained Personal Access Token**, scoped to the repositories autocoder will manage. Required permissions:
  - **`Pull requests: read & write`** — needed for PR creation.
  - **`Contents: read & write`** — needed ONLY if your `config.yaml` uses HTTPS URLs (`https://github.com/...`); when you use SSH URLs (`git@github.com:...`), git authenticates via your SSH key and `Contents` is not required.
  - **`Issues: read & write`** — needed ONLY in the rare case that your host rejects draft PRs and triggers the `do-not-merge` label fallback. GitHub.com supports drafts on every repo type, so this is essentially never needed there; only relevant for some private GHE configurations.

  Export the token as `GITHUB_TOKEN` in the environment that will launch the daemon. Note: fine-grained PATs are scoped to a single account or organization. If your `config.yaml` has repos across multiple owners (personal + one or more orgs), see the [Multiple GitHub Tokens](#multiple-github-tokens) section below — one autocoder instance can route a different PAT per owner via the optional `github.owner_tokens:` config map.
- **`git` configured.** Either a registered SSH key for the configured repository URLs (recommended), or HTTPS credentials in a credential helper.

### 2. Clone and configure

```bash
git clone https://github.com/rabbeverly/cicd-impl-agents.git
cd cicd-impl-agents
cp config.example.yaml config.yaml
```

Edit `config.yaml` and set the single `url:` value to your repository. The shipped example uses `git@github.com:your-org/your-repo.git` as a placeholder. Everything else (executor, github token env var, polling interval) works out of the box for a Claude-Code-on-this-host setup; no API keys go in this file.

### 3. Build the daemon

```bash
cargo build --release
```

This produces `target/release/autocoder` (~10 MB). The binary is self-contained — no other files need to be present at run time, except your `config.yaml` and the optional `prompts/` directory if you customize the code-reviewer prompt later.

### 4. Run it

```bash
export GITHUB_TOKEN=ghp_yourfinegrained_token_here
RUST_LOG=info ./target/release/autocoder run --config config.yaml
```

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
| `slack_channel_id`   | no       | falls back to `slack.default_channel_id` | See [ChatOps Escalation](#chatops-escalation). |

### `executor:` (required)

| Field           | Required | Default     | Description |
|-----------------|----------|-------------|-------------|
| `kind`          | yes      | —           | Currently only `claude_cli` is supported. |
| `command`       | no       | `claude`    | Path to the wrapped CLI. Set only if `claude` isn't on `$PATH`. |
| `timeout_secs`  | no       | `1800`      | Wall-clock budget per change. Killed-and-Failed on overrun. |

### `github:` (required)

| Field          | Required | Default          | Description |
|----------------|----------|------------------|-------------|
| `token_env`    | no       | `GITHUB_TOKEN`   | Name of the env var holding the fallback PAT. |
| `owner_tokens` | no       | _absent_         | Optional map of GitHub owner → env var name, for multi-owner setups. See [Multiple GitHub Tokens](#multiple-github-tokens). |

### `reviewer:` (optional)

See [Code Review](#code-review). Absent block disables the reviewer step.

### `slack:` (optional)

See [ChatOps Escalation](#chatops-escalation). Absent block disables Slack escalation; an executor `AskUser` outcome falls back to "log and exit the iteration" behavior.

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
    rabbeverly:  PERSONAL_GH_TOKEN     # owner → env var name (not the token value)
    my-org-a:    ORG_A_GH_TOKEN
    my-org-b:    ORG_B_GH_TOKEN

repositories:
  - url: "git@github.com:rabbeverly/personal-repo.git"
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

Before spawning any polling task, autocoder iterates every configured repository and resolves a token route for each. If any repo's owner has no matching `owner_tokens` entry AND its fallback (`token_env`'s named env var) is unset, the daemon exits non-zero immediately, naming the unmappable repo. This catches typos and missing env vars at boot, not five minutes later on the first PR attempt.

On success, autocoder emits one log line per repo naming the env var (never the token value):

```
INFO repository git@github.com:rabbeverly/personal-repo.git will use GitHub token from env var PERSONAL_GH_TOKEN
INFO repository git@github.com:my-org-a/work-repo.git will use GitHub token from env var ORG_A_GH_TOKEN
INFO repository git@github.com:my-org-b/another-repo.git will use GitHub token from env var ORG_B_GH_TOKEN
```

### Matching rules

- Map keys are matched against URL owners **case-insensitively** (`My-Org` matches `my-org` and vice versa). GitHub owner names are case-insensitive at the platform level.
- The first matching entry wins. If you have duplicate keys differing only in case, fix the YAML — there is no defined priority.
- An owner with no `owner_tokens` entry falls back to `token_env`. A repository with neither route is a startup error.

### Backward compatibility

A config with only `token_env: GITHUB_TOKEN` and no `owner_tokens` works exactly as before. The field is purely additive.

### git operations are separate

This routing affects only HTTP calls to GitHub's REST API (PR creation, optional label fallback). Git operations (`clone`, `fetch`, `push`) go through whichever authentication `git` itself uses — your SSH key, an HTTPS credential helper, etc.

**Recommendation for multi-owner setups:** use SSH URLs (`git@github.com:owner/repo.git`) in `config.yaml`. A single SSH key registered against each account/org covers the git side without per-owner credential-helper trickery, while autocoder's `owner_tokens` covers the API side. HTTPS URLs work but require a git credential helper that can map URLs to different PATs, which autocoder does not configure for you.

### Non-goal: per-repository overrides

If two repos under the same owner need different tokens (rare), this design does not support it. Open an issue if you hit that case; the natural extension is a `github_token_env` field on each `repositories[]` entry, but it's not in this implementation.

---

## Architecture

autocoder is a single tokio-based daemon with one polling task per configured repository. Each iteration follows a fixed workflow: fetch + branch init → process waiting (escalated) changes → process pending changes → push + PR if any commits were produced. The serial-per-repo invariant guarantees that change B does not run while change A is mid-flight or waiting on human input.

Built capabilities (each is a baseline spec under `openspec/specs/`):

1. **orchestrator-cli** — the `run` daemon entry point and the `rewind` recovery subcommand. Multi-repo dispatch with a shared cancellation token; per-repo polling tasks; SIGINT/SIGTERM drain.
2. **workspace-manager** — deterministic per-repo workspace paths under `/tmp/workspaces/`, idempotent clone-or-fetch, startup-time cross-repo collision detection, and a startup dirty-workspace check that permanently skips contaminated repos for the process lifetime.
3. **openspec-queue-engine** — enumerate (pending + waiting), lock/unlock via `.in-progress` markers, stale-lock cleanup at startup, archive on completion with `YYYY-MM-DD-<change>` date prefix, unarchive on rewind.
4. **executor** — backend-agnostic `Executor` trait with `Completed` / `AskUser` / `Failed` outcomes plus a `resume()` entry point. First concrete backend is `ClaudeCliExecutor`, which wraps the `claude` CLI as a subprocess with a configurable timeout, two-layer `AskUser` detection (an MCP-tool marker file plus a stdout-regex backstop), and a real `resume()` implementation.
5. **git-workflow-manager** — branch init (`fetch → checkout base → pull --ff-only → checkout -B agent`), per-change commits with `<change>: <first line of ## Why>` subject truncated to 72 chars, monolithic PR creation via the GitHub REST API with `--force-with-lease` push.
6. **chatops-manager** — Slack escalation. On `AskUser`, the daemon posts a question to a configured channel and persists `.question.json` to disk. On the next iteration it polls the Slack thread; when the first non-bot reply arrives it writes `.answer.json` and resumes the executor. Same-repo serial-queue invariant is preserved: any waiting change in a repository blocks all pending-change processing for that repo until resolved.
7. **code-reviewer** — opt-in AI code-quality review of the diff between base and agent branches. Configurable LLM provider (Anthropic or any OpenAI-compatible endpoint, including Grok, OpenRouter, local Ollama). A `Block` verdict creates the PR as a draft (with a `do-not-merge` label fallback on hosts that reject drafts).

The default executor backend wraps `claude` as a subprocess. The daemon writes a per-workspace `.mcp.json` pointing at itself as an MCP server exposing the `ask_user` tool; when the agent calls it, a marker file is written and the daemon picks it up after the child exits. The MCP server is hosted as a hidden subcommand of the autocoder binary, so deployment is a single-binary install.

---

## ChatOps Escalation

When the optional `slack:` config block is present, autocoder routes ambiguous agent outcomes (executor returning `AskUser`) to a human via Slack thread replies, persists the conversation state to disk, and resumes implementation on the next iteration when an answer arrives.

### Configuring Slack

```yaml
slack:
  bot_token_env: SLACK_BOT_TOKEN        # env var containing your xoxb-... bot token
  default_channel_id: C0123456789       # fallback channel id (use the Slack channel ID, not the name)
```

Per-repo override:

```yaml
repositories:
  - url: "git@github.com:my-org/auth-service.git"
    # ...
    slack_channel_id: C0AUTH_CHANNEL    # this repo posts to a different channel
```

### Required Slack bot scopes

The Slack app's bot token must have at least these OAuth scopes:

- `chat:write` — post the escalation message into the channel.
- `channels:history` — read thread replies on public channels.
- `channels:read` — list channels (for token validation).

autocoder does NOT need `users:read` or any user-level scopes; reply attribution is by Slack user id only.

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

### `.question.json` and `.answer.json` as workspace artifacts

These files are written by autocoder into the workspace alongside the change's `proposal.md`. They are safe to inspect (plain JSON) but unsafe to modify by hand — atomic writes via temp-file-then-rename mean they're consistent on disk, but the daemon's state machine assumes it owns their lifecycle. When a change is archived, the directory move takes the marker files with it; they're not deleted separately.

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
  api_key_env: ANTHROPIC_API_KEY    # env var containing the API token
  api_base_url: https://api.anthropic.com   # optional; provider default if omitted
  prompt_template_path: ./prompts/code-review-default.md  # optional; built-in default if omitted
```

The `openai_compatible` provider works with any endpoint that speaks the OpenAI `/chat/completions` API — Grok, OpenRouter, local Ollama, etc. Point `api_base_url` at the endpoint and provide a matching token via `api_key_env`.

### Verdict semantics

| Verdict     | PR state  | Meaning                                                                   |
|-------------|-----------|---------------------------------------------------------------------------|
| `Pass`      | non-draft | No concerns above style nits.                                              |
| `Concerns`  | non-draft | Issues warrant discussion but the diff is mergeable.                       |
| `Block`     | **draft** | At least one issue would cause real harm if merged.                        |

If the LLM's response cannot be parsed for a verdict, the daemon defaults to `Concerns` and prepends a parse-failure note to the report. If the API call itself errors (network, auth, rate limit), the daemon logs the error and ships the PR anyway with `(reviewer failed: <reason>)` in the `## Code Review` section. **A failed reviewer never blocks PR creation.**

### Block-verdict enforcement (recommended)

autocoder does its part by creating the PR as a draft. To make `Block` actually gate merge, configure a branch-protection rule on the PR target branch that **requires PRs not be draft**. Without that rule, anyone with write access can flip the draft state and merge.

On hosts that don't support drafts (some private GHE configurations, certain repo types), autocoder falls back automatically: it retries the PR creation with `draft: false` and applies a `do-not-merge` label via the issues-labels endpoint. Configure your branch protection to require the absence of that label as the fallback gate.

### Custom prompt templates

If the default template doesn't match your project's style, override it via `reviewer.prompt_template_path`. Custom templates are **user-owned** — the project does not enforce scope on overrides, so if you want to expand the reviewer to additional dimensions (spec compliance, style guide, etc.), you can. The template must include the two substitution variables `{{diff}}` and `{{change_summary}}` and must instruct the model to begin its response with a line of the form `VERDICT: Pass`, `VERDICT: Concerns`, or `VERDICT: Block`.

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

`repositories:` accepts any number of entries. autocoder spawns one polling task per entry, each on its own `poll_interval_sec`. Per-repo state is fully independent: an iteration failure on repo A does not affect repo B; a Slack escalation on repo A blocks A's pending queue but does not touch B.

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

### Recovering from a bad run

The `rewind` subcommand throws away the in-flight agent branch and re-queues one or more archived changes. See [CLI Reference → rewind](#rewind) below.

---

## Deployment

For production, run autocoder as a systemd service on a dedicated Linux host. The daemon handles its own polling, so do NOT wrap it in a cron job.

### 1. Install the binary

```bash
cargo build --release
sudo cp target/release/autocoder /usr/local/bin/autocoder
```

### 2. Create a deploy user and authenticate Claude Code

```bash
sudo useradd -m -s /bin/bash autocoder
sudo -u autocoder -i      # become the deploy user
claude auth login          # interactive Anthropic OAuth
exit                       # back to your admin shell
```

The Claude credentials now live at `/home/autocoder/.claude/`. They survive restarts as long as the systemd unit runs as the same user.

### 3. Stage the working directory

```bash
sudo mkdir -p /opt/autocoder
sudo cp config.example.yaml /opt/autocoder/config.yaml
sudo chown -R autocoder:autocoder /opt/autocoder
sudo -u autocoder $EDITOR /opt/autocoder/config.yaml   # edit the repo URLs
```

### 4. Set up the systemd service

Create `/etc/systemd/system/autocoder.service`:

```ini
[Unit]
Description=autocoder — autonomous OpenSpec implementation daemon
After=network.target

[Service]
Type=simple
User=autocoder
WorkingDirectory=/opt/autocoder

# GitHub PAT (required). Other env vars (e.g. ANTHROPIC_API_KEY for the
# reviewer, SLACK_BOT_TOKEN for ChatOps) are only needed if the matching
# config block is enabled.
EnvironmentFile=/etc/autocoder.env

ExecStart=/usr/local/bin/autocoder run --config /opt/autocoder/config.yaml
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

Create `/etc/autocoder.env` with the secrets (mode `0600`, owned by root):

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

# Optional:
# ANTHROPIC_API_KEY=...
# SLACK_BOT_TOKEN=xoxb-...
```

### 5. Start it

```bash
sudo systemctl daemon-reload
sudo systemctl enable autocoder
sudo systemctl start autocoder
sudo journalctl -u autocoder -f      # tail logs
```

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

Never give autocoder a Personal Access Token (PAT) or SSH key with admin access to your organization. Provide it with **scoped access** strictly limited to the repositories defined in `config.yaml`. A fine-grained PAT scoped to two specific repos is dramatically safer than an org-wide classic token.

### 2. Branch protection

Protect your `main` and `dev` branches. autocoder must **never** be allowed to push directly to protected branches. It pushes only to the designated `agent_branch` and opens PRs for human review. Configure GitHub branch protection to require PR approval and (optionally) require PRs not be draft, so the reviewer's `Block` verdict actually gates merge.

### 3. The "self-modifying AI" risk

If you point autocoder at its own repository (e.g. `cicd-impl-agents`), there is a risk of the agent modifying its own source code in unexpected ways. A "lazy" LLM under pressure might try to satisfy failing tests by deleting them, modify the OpenSpec schema to avoid spec checks, or alter its own system prompts.

**Mitigation:** require human + reviewer-agent approval for any PR merged into autocoder's own repository. Never auto-merge autocoder's PRs into itself without a human in the loop.

### 4. Workspace isolation

autocoder clones repositories into `/tmp/workspaces/`. Ensure this partition has sufficient disk space and gets cleared of orphaned files on system restart (most distros mount `/tmp` as tmpfs by default, which handles this). Do not run autocoder with root privileges. The deploy user only needs:

- Write access to `/tmp/workspaces/`
- Write access to its own `~/.claude/` (for Claude Code credentials)
- Read access to `/opt/autocoder/config.yaml`

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

### `rewind`

Throw away the in-flight agent branch and re-queue one or more archived changes. Use this when an agent produced bad work or a PR was rejected and you want the daemon to try again.

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

With **one** configured repository, `--repo` is optional and defaults to that repo. With **two or more** configured repositories, `--repo` is required. autocoder matches the selector against each repository's full URL (exact equality) AND against the URL's short-name (basename with any trailing `.git` stripped). Zero matches or multiple matches exit non-zero with a clear error listing the available selectors.

**Unarchiving multiple changes:**

If you pass multiple change names and one of them fails to unarchive (typo, no matching archive entry, destination collision), the remaining names are still attempted. The process exits non-zero at the end with a summary naming both the succeeded and failed changes.

**"I rewound the wrong change":**

Archived directories are **not** deleted by archive — they are renamed under `openspec/changes/archive/<YYYY-MM-DD>-<name>/`. If you accidentally rewind a change and want to put it back, move the directory back into the archive yourself (the canonical date-prefix format is preserved by autocoder's `archive` step, so a manual `mv` restores the queue's view of state).

---

## Status & Roadmap

The seven capabilities listed under [Architecture](#architecture) are all **implemented and tested**. autocoder runs end-to-end against real GitHub repositories with the Claude CLI as executor and (optionally) Slack as the escalation channel.

The following capabilities are **explicitly aspirational** — referenced in design documents but not built:

- **Verifier** *(planned; not in any active change)*: a spec-audit step that runs alongside the code reviewer and asks "did the diff actually implement the spec?" The reviewer agent currently focuses on code quality and explicitly does not assess spec compliance. Until the verifier ships, spec correctness is a human-review concern.
- **Drift audit** *(planned; not in any active change)*: a periodic whole-repo verification that catches gradual divergence between the baseline `openspec/specs/` and the code. Until this ships, the per-change architecture-baseline cross-reference section (e.g. section 13 of an archived change like `orchestrator-foundation`) is the closest equivalent — it runs once at change-archive time, not continuously.

Other items deferred without a current owner:

- **Multi-instance distributed deployment.** autocoder assumes single-instance ownership of each configured workspace; running two daemons against the same `local_path` would race. Out of scope for the current architecture.
- **Per-repo executor configuration overrides.** The `executor:` block is global; mixing Claude on one repo and a different backend on another in the same config is not supported.
- **Streaming or incremental code review.** The reviewer sends the full diff in one LLM call; truncation at 100k chars is documented in `prompts/code-review-default.md`.

If you build something that depends on an aspirational item, file an issue or open an OpenSpec change proposal in this repository — autocoder can dogfood its own development once a sandbox is wired up (with appropriate self-modification guardrails; see [AI Security & Guardrails](#ai-security--guardrails)).

---

*Documentation maintained per the `project-documentation` OpenSpec rule. Any new capabilities or operational shifts must be updated here in the same change that introduces them.*
