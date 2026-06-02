# Deployment

For production, run autocoder as a systemd service on a dedicated Linux host. The daemon polls on its own — do not wrap it in a cron job.

## Recommended: install from a binary release

For most operators, the [Quick install](../README.md#quick-install) one-liner is the right path. It downloads a pre-built binary from the [GitHub Releases](https://github.com/IndustriousKraken/openspec-autocoder/releases) page (per tag, for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `aarch64-apple-darwin`), verifies its SHA-256, and then runs `autocoder install` to set up the systemd service and configuration. Releases are versioned with SemVer tags (`vX.Y.Z`); dash-suffixed tags such as `vX.Y.Z-rc1` are pre-releases that the installer skips by default. The rest of this section covers the manual / source-build path for operators who specifically want to avoid downloaded binaries.

## Switching from source-build to binary updates

If your existing autocoder deployment was built from source — typically the layout in this guide, with `config.yaml` under `/home/autocoder/autocoder/` and a hand-written `/etc/systemd/system/autocoder.service` — and you want to switch onto the released-binary upgrade path, you have two options.

**Before `autocoder install` learned to probe systemd:** re-running `install.sh` against such a host triggered the full wizard, overwrote the systemd unit, and produced a daemon pointed at a fresh wizard-generated config instead of your existing one. Any custom `Environment="PATH=..."` entries you'd added — a common case is the openspec CLI living under `~/.nvm/versions/node/<v>/bin/` — were lost, along with `WorkingDirectory` and the unit's `--config` path. The install wizard's systemd probe now prevents that outcome, but only when the operator passes `--config-dir <existing-config-dir>` OR the existing unit can be detected via `systemctl show autocoder.service`.

### Option 1 — Re-run `install.sh` against your existing config

Run the standard one-liner, passing `--config-dir` pointing at where your existing `config.yaml` lives:

```bash
curl -fsSL https://raw.githubusercontent.com/IndustriousKraken/openspec-autocoder/main/install.sh \
  | bash -s -- --config-dir /home/autocoder/autocoder
```

The installer downloads the binary, verifies its SHA-256, copies it to `/usr/local/bin/autocoder`, and execs `autocoder install`. The new systemd probe finds your loaded `autocoder.service`, reads its `ExecStart=` line, sees the `--config /home/autocoder/autocoder/config.yaml` flag, and exits without running the wizard. Your existing config, secrets, and systemd unit are untouched. Then restart the daemon to pick up the new binary:

```bash
sudo systemctl restart autocoder
```

### Option 2 — Manual binary swap (no `install.sh` involvement)

If you'd rather skip the bash wrapper entirely, replicate what `install.sh` does internally. Release assets are named `autocoder-<tag>-<triple>` (e.g. `autocoder-v0.5.0-x86_64-unknown-linux-gnu`) and each has a companion `.sha256` sidecar:

```bash
TAG=v0.5.0
TRIPLE=x86_64-unknown-linux-gnu
BASE=https://github.com/IndustriousKraken/openspec-autocoder/releases/download/${TAG}
curl -fsSL -o autocoder "${BASE}/autocoder-${TAG}-${TRIPLE}"
curl -fsSL -o autocoder.sha256 "${BASE}/autocoder-${TAG}-${TRIPLE}.sha256"
sha256sum -c autocoder.sha256
sudo install -m 755 autocoder /usr/local/bin/autocoder
sudo systemctl restart autocoder
```

This path does not touch your config, secrets, or systemd unit at all — only the binary at `/usr/local/bin/autocoder` is swapped.

### Editing one section without re-doing the whole wizard

`autocoder install --reconfigure <section>` re-prompts ONE block of an existing install and patches the existing `config.yaml`. Use it when you want to change cadences, swap reviewer providers, or move chatops to a different channel without walking through the full first-run questionnaire again. Accepted values: `audits`, `reviewer`, `chatops`.

The most common use is `autocoder install --reconfigure audits` after deciding that the conservative first-install defaults are too quiet (or too loud): the wizard re-prompts every audit cadence with the current value as the default, writes the new `audits.defaults.*` in place, then prints the `sudo -u autocoder autocoder reload` command to hot-apply. The `reviewer` and `chatops` variants show a unified diff before applying so you can catch hand-edited overrides (custom `api_base_url`, notifications block) that would otherwise be lost on round-trip.

Sections NOT covered: `repositories` (use `autocoder reload`, which hot-applies add/remove without a restart — `--reconfigure repos` is intentionally absent), `paths.*` (destructive, restart-required), and `executor.*` (restart-required). See [docs/CLI.md](CLI.md) for the full flag reference.

For ongoing unattended updates (cron-driven binary swaps that watch the releases feed), see [Unattended updates via cron](DEPLOYMENT.md#unattended-updates-via-cron).

## 1. Install the binary

```bash
cargo build --release
sudo cp target/release/autocoder /usr/local/bin/autocoder
```

## 2. Create a deploy user and authenticate Claude Code

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

If Claude Code is installed system-wide rather than under the autocoder user, the daemon user can't auto-update it. Add a root cron entry to update it periodically:

```
0 4 * * 0 root claude update
```

(If `npm` isn't on the autocoder user's `$PATH`, install Node.js first via your distro's package manager or `nvm`. The exact openspec install command may vary; check the openspec project for the current recommendation.)

After installing the openspec CLI, run `openspec config profile` once on this host and enable the `Sync specs` workflow:

```bash
sudo -u autocoder openspec config profile
```

This launches an interactive picker. Choose **Delivery: Both (skills + commands)** and at minimum tick **Sync specs** in the workflow list. Then in each project the daemon operates on, refresh the project's openspec install so the new workflows take effect:

```bash
sudo -u autocoder bash -c 'cd /var/cache/autocoder/workspaces/<sanitized-url> && openspec update'
```

autocoder's archive step shells out to `openspec archive`, which performs both the file move AND the merge of change deltas into canonical capability specs — but the merge step is only available when `sync` is enabled in the openspec profile. Without it, `openspec archive` will move the change directory but won't update canonical specs; autocoder iterations succeed but drift accumulates in `openspec/specs/`. To reconcile drift after the fact (e.g. for repos with pre-existing drift, or after onboarding a repo from a host that didn't have `sync` enabled), see the companion `rebuild-canonical-specs-from-archive` change.

## 3. Set up SSH for the autocoder user

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

### Option A — Machine user (recommended for orgs with real users)

Create a dedicated GitHub account (e.g. `<your-handle>-autocoder`) that exists only to be autocoder. Add it as a member of a team in each org with access to only the repositories in `config.yaml`, then register the SSH key on the machine user's account (*Settings → SSH and GPG keys → New SSH key*).

Required team-grant permission level:

- **Read** if you use [Fork-and-PR mode](SECURITY.md#7-fork-and-pr-workflow-recommended-for-org-repos) (recommended). The machine user only reads upstream and pushes to its own fork.
- **Write** if you use direct-push mode (no `github.fork_owner` set). The machine user pushes the agent branch directly to upstream.

Mint the PATs you set in `config.yaml`'s `github.owner_tokens` from the machine user too — same scoping principle: the credential's authority matches autocoder's job. A full compromise of the autocoder host then gives the attacker exactly the access you granted that user and nothing more.

GitHub's terms of service permit machine users for automation. The account is free.

### Option B — Per-repo deploy keys (works without a separate identity)

Add the same public key as a deploy key on each repo: *Repo settings → Deploy keys → Add deploy key*, with **"Allow write access"** checked so autocoder can push the agent branch.

Caveat: GitHub enforces that any given public key can be registered as a deploy key on **exactly one repo** across the platform. If autocoder manages N repos, you need N keypairs in `~autocoder/.ssh/` plus a `~/.ssh/config` with per-host routing — e.g.:

```
Host github.com-org-a-repo-1
  HostName github.com
  IdentityFile ~/.ssh/id_ed25519_org_a_repo_1
  IdentitiesOnly yes
```

Then the `config.yaml` URL becomes `git@github.com-org-a-repo-1:org-a/repo-1.git`. Manageable up to a handful of repos; tedious past that.

### Option C — Personal-account key (small personal-repo setups only)

Register the key under your own `Settings → SSH and GPG keys → New SSH key`. The autocoder daemon will then act as you for all git operations, with whatever permissions you have. **Do not use this for organization repos with real users** — a compromised autocoder host can `git push` anywhere you can. Acceptable only for solo developers managing their own personal repos.

### Verify

```bash
sudo -u autocoder ssh -T git@github.com
# Expected: "Hi <user>! You've successfully authenticated, but GitHub does not provide shell access."
```

`<user>` will be whichever identity you registered the key under (the machine user, your own account, or — for deploy keys — empty since deploy keys don't have a user identity).

## 4. Stage the working directory

```bash
sudo mkdir -p /home/autocoder/autocoder
sudo cp config.example.yaml /home/autocoder/autocoder/config.yaml
sudo chown -R autocoder:autocoder /home/autocoder/autocoder
sudo -u autocoder $EDITOR /home/autocoder/autocoder/config.yaml   # edit repo URLs, and inline secrets if you chose that path
sudo chmod 600 /home/autocoder/autocoder/config.yaml              # restrictive perms regardless of secret path
```

## 5. Set up the systemd service

Pick one of the two secret-delivery paths below depending on what you put in your `config.yaml` (see [Secrets in `config.yaml`](SECURITY.md#5-secrets-in-configyaml-inline-vs-env-var)).

### Path A — inline secrets (recommended for single-host deployments)

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

`openspec` must be on autocoder's PATH. The daemon runs `openspec --version` at startup and exits non-zero with a clear stderr message if the binary is missing. Confirm with `sudo -u autocoder which openspec`. The per-change run log at `<logs_dir>/runs/<repo>/<change>.log` (typically `/var/log/autocoder/runs/<repo>/<change>.log` under systemd) records the prompt sent to Claude under a `=== PROMPT (n bytes) ===` header for inspection.

### Path B — env-var secrets (multi-user hosts, classical production pattern)

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

## 6. Start it

```bash
sudo systemctl daemon-reload
sudo systemctl enable autocoder
sudo systemctl start autocoder
sudo journalctl -u autocoder -f      # tail logs
```

## Applying config changes without a restart

Edit `config.yaml`, then run:

```bash
sudo -u autocoder autocoder reload
```

The `autocoder reload` subcommand connects to the daemon's control socket at `<runtime_dir>/control.sock` (typically `/run/autocoder/control.sock` under systemd, or `${XDG_RUNTIME_DIR}/autocoder/control.sock` in dev mode). That socket is created on startup with mode `0600` and is owned by the user the daemon runs as (the `autocoder` user in this guide), so any reload command must run as the same user — `sudo -u autocoder` is the standard invocation. The daemon re-reads `config.yaml` from the path it was launched with, validates it, and hot-applies the `github`, `reviewer`, `chatops`, and `repositories` sections at the next iteration boundary for each repo. Only changes to `executor:` are not hot-applied; the response names that under `requires_restart` so you know it still needs `systemctl restart autocoder`. See [Runtime control: live config reload](OPERATIONS.md#runtime-control-live-config-reload) above for the full response shape and validation-rejection semantics.

## Upgrading

Build the new release, copy the binary, restart the unit:

```bash
cd /path/to/cicd-impl-agents
git pull
cargo build --release
sudo cp target/release/autocoder /usr/local/bin/autocoder
sudo systemctl restart autocoder
```

**Previewing release notes before tagging.** Operators about to push a new release tag can preview the changelog locally to confirm the harvested `## Why` paragraphs read sensibly before the GitHub release publishes:

```bash
# From inside an autocoder checkout — emits the markdown that the release
# workflow will publish as the GitHub Release body.
autocoder changelog --since v0.4.0 --to HEAD
```

See [docs/CLI.md `changelog`](CLI.md#changelog) for the full flag surface, frontmatter overrides (`changelog: skip`, `changelog.summary: "..."`), and the cross-project usage path (`--workspace <path>` from the daemon host).

If you were on an older version that installed under `/usr/local/bin/openspec-orchestrator` or used a service unit named `openspec-orchestrator.service`, remove those before installing the rename:

```bash
sudo systemctl stop openspec-orchestrator 2>/dev/null
sudo systemctl disable openspec-orchestrator 2>/dev/null
sudo rm -f /etc/systemd/system/openspec-orchestrator.service /usr/local/bin/openspec-orchestrator
sudo systemctl daemon-reload
```

## Version-string format

The version string surfaced by `autocoder --version` and the `🆙` startup notification is resolved **at build time** by `build.rs` running `git describe --tags --always --dirty` against the source checkout. Operators see different forms depending on how the binary was built:

| Build context | Example output | When you see it |
| --- | --- | --- |
| Clean tag commit | `v1.1.1` | The build sits exactly on a `vX.Y.Z` tag. Binary-release installs (via `update.sh`) always land here, because the GitHub Actions release workflow builds at the tagged commit. |
| Dev commits past tag | `v1.1.1-23-g4abc123` | The build is N commits past the most-recent tag. The trailing `-g<short-sha>` names the working commit. Typical of source-built deployments running master. |
| Dirty working tree | `v1.1.1-23-g4abc123-dirty` | The build includes uncommitted modifications to tracked files. The `-dirty` suffix surfaces that the running binary was built from an in-progress local state. |
| Source tarball without `.git/` | Cargo.toml's `version =` field verbatim | `cargo install autocoder` from crates.io, an unpacked source tarball, or any host without the `git` binary on PATH. `build.rs` falls back to `env!("CARGO_PKG_VERSION")` and the build still succeeds. |

`Cargo.toml`'s `version =` field is the **base version operators manually bump at semver-meaningful releases** (major / minor / patch). It is NOT bumped per commit — `git describe` provides the delta-past-tag info automatically, so per-commit version bumps would just be churn. The Cargo.toml version only surfaces directly in the tarball-fallback case above; in every other case it is the prefix that `git describe` extends with `-N-gSHA[-dirty]`.

Binary-release operators (using `update.sh`) always see clean `vX.Y.Z` strings in their `🆙` notifications and `--version` output because the release workflow builds at tagged commits. Source-build operators see the `-N-gSHA` form whenever their checkout sits past a tag — which is the common case on master.

## Unattended updates via cron

For single-host SBC, indie VPS, and homelab deployments where set-and-forget is the explicit goal, `update.sh` ships at the repo root as a cron-friendly companion to `install.sh`. The script resolves the installed version, fetches the latest non-prerelease tag, downloads + checksum-verifies the binary, runs `autocoder check-config` against the downloaded binary as a preflight, atomically swaps the binary aside to `/usr/local/bin/autocoder.previous`, restarts the systemd unit, and rolls back automatically if the daemon does not reach `active` within 30 seconds.

**Audience caveat.** This workflow is for homelab / indie / SBC deployments where the operator wants binary updates without intervention. **Do not use it** in enterprise change-control environments where Ansible, apt repositories, container registries, or k8s pipelines already own update orchestration — those tools have richer rollback, staging, and audit semantics than `update.sh` provides. If you are upgrading an existing source-built deployment onto the binary-update workflow, follow [Switching from source-build to binary updates](#switching-from-source-build-to-binary-updates) first; once you are on a binary install, this section's cron entry takes over.

**Stage the script.** Place `update.sh` somewhere the autocoder user can run it. The conventional location is `/home/autocoder/update.sh`:

```bash
sudo curl -fsSL https://raw.githubusercontent.com/IndustriousKraken/openspec-autocoder/main/update.sh \
  -o /home/autocoder/update.sh
sudo chown autocoder:autocoder /home/autocoder/update.sh
sudo chmod 0755 /home/autocoder/update.sh
```

**Cron entry.** Add the following to root's crontab (`sudo crontab -e`) — the script uses `sudo` internally for `mv`, `install`, and `systemctl restart`, but running it as root sidesteps the sudo prompt entirely:

```cron
0 3 * * * /home/autocoder/update.sh >> /var/log/autocoder-update.log 2>&1
```

The `0 3 * * *` runs once a day at 03:00 local time — low-traffic on most homelab hosts. The redirect captures both stdout and stderr so any failure leaves a paper trail; if you'd rather see only failures, `MAILTO=` at the top of the crontab plus a non-redirected entry mails the output of any non-zero exit.

When running across a small fleet, jitter the minute field so hosts do not all hit the GitHub Releases API at the same second:

```cron
# Per-host: pick a stable minute 0..59 (e.g. `awk 'BEGIN { srand(); print int(rand()*60) }'`).
17 3 * * * /home/autocoder/update.sh >> /var/log/autocoder-update.log 2>&1
```

**`--version <tag>` for explicit pinning.** Operators who freeze on a known-good release between manual upgrade reviews can pin the cron entry:

```cron
0 3 * * * /home/autocoder/update.sh --version v0.7.2 >> /var/log/autocoder-update.log 2>&1
```

Pre-release tags (e.g. `v2.0.0-rc1`) are accepted only via `--version`; the default flow uses the latest non-prerelease tag via `GET /repos/<owner>/<repo>/releases/latest`, which by GitHub's contract excludes pre-releases.

**`--dry-run` for the first scheduled run.** When you first set up the cron job, run `update.sh --dry-run` interactively from the host so you can inspect the preflight output without committing to a swap:

```bash
sudo -i /home/autocoder/update.sh --dry-run
```

The script reports the resolved current + target versions, downloads + verifies the binary, runs `check-config --json`, prints `[dry-run] Would swap to <tag>`, and exits 0. Nothing on disk or in systemd changes.

**Operator-visibility loop.** When chatops is configured, the daemon posts a `🆙 autocoder vX.Y.Z started — N repository(ies) configured` notification on every successful startup — so after a cron-driven update lands, the chat channel records the version transition automatically. See [CLI.md `run`](CLI.md#run) for the notification's format.

---

## Self-hosted Ollama for RAG

When `canonical_rag.provider: ollama` is configured (see
[CONFIG.md → `canonical_rag:`](CONFIG.md#canonical_rag-optional)), the
daemon connects to the operator's Ollama instance for every embed
call. Three common topologies:

### 1. Docker quick-start (recommended for trial deployments)

`autocoder install`'s wizard option (1) copies
`install/ollama-docker-compose.yml` to the operator's config
directory. The wizard does NOT auto-run docker; the operator opts in
explicitly:

```bash
docker compose -f <config_dir>/ollama-docker-compose.yml up -d
```

The compose file ships pulling `nomic-embed-text` as the default
startup model — small enough for CPU-only hosts AND reasonable
quality for the corpus size of typical OpenSpec projects. Once the
container is running, the daemon's workspace-init step connects on
the first iteration AND embeds the canonical corpus.

To upgrade to a larger embedding model (e.g. on hardware with a GPU
attached), edit the entrypoint in the copied compose file to pull
`qwen3-embedding:4b` or another larger model AND `docker compose up
-d` to recreate the container with the new pull.

### 2. Remote Ollama on a GPU machine

For shared-team deployments where the daemon runs on a small VM AND a
dedicated GPU host serves embeddings, point the daemon at the remote
instance:

```yaml
canonical_rag:
  enabled: true
  provider: ollama
  model: qwen3-embedding:4b
  api_base_url: http://gpu-host:11434
```

The Ollama process on `gpu-host` SHOULD bind on `0.0.0.0:11434` (or
behind a private network proxy). No autocoder-side TLS is required at
this layer — operators wanting TLS terminate it at the proxy. The
daemon does not authenticate against vanilla Ollama; expose only on
trusted networks.

### 3. OpenAI-compatible providers

For operators preferring a managed embedding provider (Voyage,
OpenRouter, OpenAI, llama.cpp's server, etc.), use the
`openai_compatible` provider — see
[CONFIG.md → `canonical_rag:`](CONFIG.md#canonical_rag-optional) for
the `api_base_url` AND `api_key_env`/`api_key` fields.

### Hardware suggestions

- CPU works for the corpus size of typical OpenSpec projects (~50
  capabilities, ~500 chunks). Expect ~30s for the cold-start embed.
- GPU is faster (sub-second cold start) but NOT required. Operators
  adding a GPU later edit the compose file's entrypoint OR
  `api_base_url` AND restart the daemon.
- Memory: the in-memory store is bounded by the canonical corpus
  size. For typical projects this is a few MB; the embedding model
  itself dominates RAM use (~1.5 GB for `nomic-embed-text`).
