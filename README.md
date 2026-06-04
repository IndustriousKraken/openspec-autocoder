# autocoder

**autocoder** is an autonomous daemon that reads OpenSpec implementation proposals from one or more configured repositories, drives an AI coding agent (the Claude CLI by default) through each change in serial order, and opens monolithic Pull Requests for human review. It's "OpenSpec change at the top, working code in a PR at the bottom" wired into a single long-running process.

---

## Quick install

```bash
curl -fsSL https://raw.githubusercontent.com/IndustriousKraken/openspec-autocoder/master/install.sh | bash
```

The one-liner downloads a pre-built binary, verifies its SHA-256, places it at `/usr/local/bin/autocoder` (or `~/.local/bin/autocoder` if `sudo` is unavailable or `--user` is passed), then execs `autocoder install`. **The bootstrap script is intentionally tiny (~75 lines, no operator prompts).** Everything else — the configuration wizard, `useradd`/`systemctl`/`apt-get`, optional Claude CLI bootstrap — lives in the `autocoder install` Rust subcommand which ships with `cargo test` coverage.

By default `autocoder install` picks **server mode** on Linux when systemd is detected (`/run/systemd/system` present): it creates an `autocoder` system user, writes `/etc/autocoder/config.yaml` + `/etc/autocoder/secrets.env`, renders `/etc/systemd/system/autocoder.service`, and offers to start it. Otherwise it picks **dev mode** and writes to `~/.config/autocoder/` instead, with no system-user / systemd work. Either mode can be forced with `--mode server` or `--mode dev`.

For automation (Ansible, Terraform, cloud-init), pass `--non-interactive` along with `--repo-url`, `--token-env-var`, `--chatops-backend`, and `--reviewer-provider`. Anything after `--` on the `install.sh` command line is forwarded to the subcommand:

```bash
curl -fsSL .../install.sh | bash -s -- --non-interactive \
  --repo-url git@github.com:acme/widgets.git \
  --token-env-var GITHUB_TOKEN \
  --chatops-backend none \
  --reviewer-provider none
```

Prefer to build from source instead? See [docs/INSTALL.md](docs/INSTALL.md).

### Periodic audits during install

The wizard asks about periodic audits before writing `config.yaml`. The five LLM-driven audits — `architecture_brightline`, `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit` — are gated behind a single `[y/N]` question so operators who want to defer can answer "n" and move on. Operators who accept the gate get a fast-path prompt that enables all five at recommended cadences, falling back to an individual cadence walk-through if they decline the fast path.

For non-interactive installs, the same configuration is available via `--audits-spec-sync <disabled|daily|weekly|monthly>` (defaults to `daily`), `--audits-llm-driven <none|recommended|all-disabled>` (defaults to `none`), and per-audit `--audit-<slug> <cadence>` overrides. A `--non-interactive` invocation that passes none of the `--audits-*` flags inherits the conservative default (spec-sync daily; everything else disabled), so IaC scripts that pre-date this wizard step keep working without surprise behavior changes. See [docs/CONFIG.md#audits-optional](docs/CONFIG.md#audits-optional) for cadence syntax and the `extra` knobs each audit reads.

### Reinstalling / upgrading

Re-running `install.sh` downloads the latest binary (or a specific tag — pass `--version vX.Y.Z` to the script or set `AUTOCODER_VERSION=vX.Y.Z` in the environment), verifies the checksum, and replaces `/usr/local/bin/autocoder`. The subsequent `autocoder install` detects the existing `config.yaml` and exits 0 without re-prompting: the operator's choices made on first run are preserved across binary upgrades. To force the wizard back on (e.g. to relocate the config), pass `--upgrade` after `--`.

**Unattended upgrades via cron.** For single-host SBC, indie VPS, and homelab deployments where set-and-forget is the goal, `update.sh` ships at the repo root as a cron-friendly companion to `install.sh`. It resolves the installed version, fetches the latest non-prerelease tag, downloads + checksum-verifies the binary, runs `autocoder check-config` as a preflight, atomically swaps the binary aside to `/usr/local/bin/autocoder.previous`, restarts the systemd unit, and rolls back automatically if the daemon does not reach `active` within 30 seconds. See [docs/DEPLOYMENT.md → Unattended updates via cron](docs/DEPLOYMENT.md#unattended-updates-via-cron) for the recommended crontab entry, version-pinning, and `--dry-run` for first-time setup. **Not recommended** for enterprise change-control environments where Ansible / container registries / k8s pipelines already own update orchestration.

**Re-prompting one section of an existing install.** `autocoder install --reconfigure <audits|reviewer|chatops>` re-runs the wizard for just that section and patches the existing `config.yaml`. Useful for changing the periodic-audit cadences after first install, switching the reviewer provider, or rewiring chatops to a different backend, without touching anything else. The flag rejects values outside the three-section allowlist; `repositories`, `paths`, and `executor` are intentionally excluded (use `autocoder reload` for repos; restart-required for the others). See [docs/CLI.md → install](docs/CLI.md#install).

---

## What you can do with it

Once the daemon is running, this is the day-to-day surface.

**Implement specs autonomously.** Drop an OpenSpec change into `openspec/changes/<slug>/`, push to the base branch. The next polling iteration drives the wrapped agent through it, opens a PR with the diff, and archives the change on merge. Failures surface via chatops; per-change `.perma-stuck.json` and `.needs-spec-revision.json` markers signal when human action is needed.

**Talk to autocoder in chat** (Slack officially supported; Discord, Teams, Mattermost, Matrix [experimental](docs/CHATOPS.md#experimental-chatops-backends)). With Slack Socket Mode wired in:

| Verb | What it does |
| --- | --- |
| `@<bot> propose <repo> <text>` | Free-form request. The agent classifies it as a **directive** (produces a single spec-only PR; the implementer writes the code on the next iteration after merge), a **question** (replies in-thread, no PR), or **ambiguous** (asks for clarification via the standard `ask_user` escalation). See [docs/CHATOPS.md → Chat-driven proposals](docs/CHATOPS.md#chat-driven-proposals-propose). |
| `@<bot> send it` *(inside an audit thread)* | Act on the audit's findings. Produces a single spec-only PR (same shape as `propose`); code fixes flow through the implementer after merge. See [docs/CHATOPS.md → Acting on audit findings](docs/CHATOPS.md#acting-on-audit-findings-send-it). |
| `@<bot> audit <type> <repo>` | Run any registered audit on demand, bypassing its cadence. CLI variant: `autocoder audit run`. See [docs/CLI.md → audit run](docs/CLI.md#audit-run). |
| `@<bot> changelog <repo> [args]` | Queue an LLM-styled `CHANGELOG.md` update against the repo. Opens a PR with a polished draft that iterates via the revision loop. Distinct from the deterministic `autocoder changelog` CLI subcommand which prints to stdout. See [docs/CHATOPS.md → Generating a changelog](docs/CHATOPS.md#generating-a-changelog-changelog). |
| `@<bot> revise <text>` *(on a PR comment)* | Re-run the agent against the original change plus your text and force-push the updated diff. Human requests are **uncapped** (always process); the per-PR cap (default 5, configurable, max 20 via `executor.max_auto_revisions_per_pr`) bounds only automatic reviewer-initiated revisions. See [docs/OPERATIONS.md → Revising an open PR via comment](docs/OPERATIONS.md#revising-an-open-pr-via-comment). |
| `@<bot> status [<repo>]` | Live workspace snapshot — branches, last commits, latest PR, busy state, queue. Bare `status` returns the per-repo menu. |
| Recovery verbs | `@<bot> clear-perma-stuck`, `clear-revision`, `wipe-workspace` (two-step confirm), `rebuild-specs`, `help`. See [docs/CHATOPS.md → Operator recovery commands](docs/CHATOPS.md#operator-recovery-commands). |

**Periodic audits.** Five audits run on configurable cadences. All `disabled` by default; opt in globally via `audits.defaults.<slug>` or per-repo. The install wizard offers a single fast-path to enable the recommended cadences.

| Audit | What it does | LLM | Output |
| --- | --- | --- | --- |
| `architecture_brightline` | Pure-code metrics: oversize files, duplicate signatures across files. | No | Reported findings (chatops `📐`) |
| `architecture_consultative` | Anchored architectural questions tied to specific `file:line` ranges. Read-only sandbox. | Yes | Reported findings (chatops `📋`) |
| `drift_audit` | Each canonical-spec requirement vs. observable code behavior, plus spec-vs-spec contradiction detection. Read-only sandbox. | Yes | Reported findings (chatops `🧭`) |
| `missing_tests_audit` | Surveys for uncovered error paths; writes `openspec/changes/tests-*` proposals. | Yes | New OpenSpec changes (queued automatically; chatops `🔍`) |
| `security_bug_audit` | Surveys for security issues and bugs; writes `openspec/changes/fix-*` / `secure-*` proposals. | Yes | New OpenSpec changes (queued automatically; chatops `🔍`) |

Spec-writing audits enqueue their new proposals into the same iteration's queue walk — implementer commit and audit's creation commit ship in one PR. Generated proposals run through `openspec validate --strict` before commit; invalid proposals are discarded and a `❌` chatops notification fires. See [docs/OPERATIONS.md → Periodic audits](docs/OPERATIONS.md#periodic-audits).

**Optional code-quality reviewer.** Each PR gains a `## Code Review` section produced by a separately-configured LLM (Anthropic, or any OpenAI-compatible endpoint — Grok, OpenRouter, local Ollama). A `Block` verdict marks the PR as draft. With `auto_revise: true` (legacy alias: `auto_revise_on_block`), per-concern actionable fixes feed back into the same revision dispatcher operator `@<bot> revise` uses — fired on actionable concerns regardless of verdict. See [docs/CODE-REVIEW.md](docs/CODE-REVIEW.md).

**Beyond the daily surface.** Fork-and-PR mode ([SECURITY.md](docs/SECURITY.md#7-fork-and-pr-workflow-recommended-for-org-repos)) collapses the blast radius — the bot account only needs Read on upstream. Per-owner PAT routing ([CONFIG.md](docs/CONFIG.md#multiple-github-tokens)) handles multi-org setups. `autocoder reload` hot-applies `github`, `reviewer`, `chatops`, and `repositories` config changes without a restart, including adding/removing repos at runtime ([OPERATIONS.md](docs/OPERATIONS.md#runtime-control-live-config-reload)). `autocoder sync-specs --rebuild` (or `@<bot> rebuild-specs`) reconciles canonical specs from archive history when the openspec `sync` workflow was missing upstream ([OPERATIONS.md](docs/OPERATIONS.md#rebuilding-canonical-specs-from-archive-history)). Per-change logs use JSON-stream parsing by default and split into PROMPT / ACTIONS / FINAL ANSWER / STDERR sections so a timeout-killed run still shows what the agent was doing ([OPERATIONS.md](docs/OPERATIONS.md#per-change-run-log-shape)).

---

## How it works

autocoder is a single tokio-based daemon with one polling task per configured repository. Each iteration follows a fixed workflow: fetch + branch init → run any due audits → process waiting (escalated) changes → process pending changes → push + PR if any commits were produced. The serial-per-repo invariant guarantees that change B does not run while change A is mid-flight or waiting on human input.

Built capabilities (each is a baseline spec under `openspec/specs/`):

1. **orchestrator-cli** — `run` daemon entry point, the `rewind` recovery subcommand, the `reload` runtime-control subcommand, and the `audit run` on-demand audit subcommand. Multi-repo dispatch with a shared cancellation token; per-repo polling tasks; SIGINT/SIGTERM drain; the periodic-audit framework, PR-comment revision dispatcher, `send it` and `propose` triage flows, canonical-spec rebuild, and standard data-directory layout all live here.
2. **workspace-manager** — deterministic per-repo workspace paths under `<cache_dir>/workspaces/`, idempotent clone-or-fetch, startup-time cross-repo collision detection, startup dirty-workspace check, partial-clone self-heal, and dirty-workspace auto-recovery between iterations.
3. **openspec-queue-engine** — enumerate (pending + waiting), lock/unlock via `.in-progress` markers, stale-lock cleanup at startup, archive on completion via `openspec archive`, unarchive on rewind.
4. **executor** — backend-agnostic `Executor` trait with `Completed` / `AskUser` / `Failed` outcomes plus a `resume()` entry point. First concrete backend is `ClaudeCliExecutor`, which wraps the `claude` CLI as a subprocess (default `--output-format stream-json`), captures the tool-call action stream and the final result event separately, applies a per-iteration sandbox, and detects `AskUser` via an MCP-tool marker file plus a stdout-regex backstop.
5. **git-workflow-manager** — branch init (`fetch → checkout base → pull --ff-only → checkout -B agent`), per-change commits with `<change>: <first line of ## Why>` subject truncated to 72 chars, monolithic PR creation via the GitHub REST API with `--force-with-lease` push.
6. **chatops-manager** — chat-platform integration behind a `ChatOpsBackend` trait. Slack is the officially-supported provider; Discord, Teams, Mattermost, and Matrix are [experimental backends](docs/CHATOPS.md#experimental-chatops-backends) with no API-stability guarantees. Outbound surface covers `AskUser` escalation, progress notifications, threaded audit-finding notifications, and the proposal-created / validation-exhausted / revision-cap notifications. The Slack Socket Mode inbound listener parses operator verbs (`propose`, `send it`, `audit`, `status`, `revise`, the recovery verbs) and submits actions over the daemon's Unix-domain control socket.
7. **code-reviewer** — opt-in AI code-quality review of the diff between base and agent branches. Configurable LLM provider (Anthropic or any OpenAI-compatible endpoint). A `Block` verdict creates the PR as a draft (with a `do-not-merge` label fallback on hosts that reject drafts). When `auto_revise: true`, per-concern actionable fixes are posted as `<!-- reviewer-revision -->`-marked PR comments that the same revision dispatcher picks up — fired on actionable concerns regardless of verdict (the legacy key `auto_revise_on_block` still works as an alias).

The default executor backend wraps `claude` as a subprocess. The daemon writes a per-workspace `.mcp.json` pointing at itself as an MCP server exposing the `ask_user` tool; when the agent calls it, a marker file is written and the daemon picks it up after the child exits. The MCP server is hosted as a hidden subcommand of the autocoder binary, so deployment is a single-binary install.

---

## Documentation

Everything beyond the quick install lives under [`docs/`](docs/README.md) — grouped by getting started, feature surfaces, operating the daemon, reference, and internals. Highlights:

- [Configuration reference](docs/CONFIG.md) — full `config.yaml` schema, multi-token routing, `paths:` block. The [Prompt overrides table](docs/CONFIG.md#prompt-overrides) is the canonical reference for customizing the daemon's embedded LLM prompts.
- [ChatOps](docs/CHATOPS.md) — chat-driven workflows (`propose`, `send it`, `audit`, `revise`), operator recovery verbs, Slack Socket Mode setup, experimental backends.
- [Operating notes](docs/OPERATIONS.md) — periodic audits, on-demand triggers, perma-stuck/needs-revision recovery, PR-comment revisions, live config reload, canonical spec rebuild.
- [Deployment](docs/DEPLOYMENT.md) — systemd unit, SSH keys, upgrades.
- [Security & guardrails](docs/SECURITY.md) — credential scoping, fork-and-PR mode, executor sandbox.
- [CLI reference](docs/CLI.md) — `run`, `reload`, `rewind`, `audit run`, `sync-specs`.
- [Troubleshooting](docs/TROUBLESHOOTING.md) — rebuild failures, revision misfires, audit timeouts, post-reboot audit storms.

---

## Status & Roadmap

The seven capabilities listed under [How it works](#how-it-works) are all **implemented and tested**. autocoder runs end-to-end against real GitHub repositories with the Claude CLI as executor and (optionally) Slack as the officially-supported escalation channel. The four experimental ChatOps backends (Discord, Teams, Mattermost, Matrix) compile and have unit-test coverage against recorded fixtures but no live-service validation; operators who deliberately select one are the ones surfacing bugs.

The following capability is **explicitly aspirational**:

- **Verifier** *(planned; not in any active change)*: a spec-audit step that runs alongside the code reviewer and asks "did the diff actually implement the spec?" The reviewer agent currently focuses on code quality and explicitly does not assess spec compliance. Until the verifier ships, spec correctness is a human-review concern. The shipped `drift_audit` is a related but distinct signal — periodic spec-vs-code divergence detection across the whole repo, separate from per-change verification.

Other items deferred without a current owner:

- **Multi-instance distributed deployment.** autocoder assumes single-instance ownership of each configured workspace; running two daemons against the same `local_path` would race. Out of scope for the current architecture.
- **Per-repo executor configuration overrides.** The `executor:` block is global; mixing Claude on one repo and a different backend on another in the same config is not supported.
- **Streaming or incremental code review.** The reviewer sends the diff plus full file contents in one LLM call; truncation at the prompt-budget ceiling is documented in `prompts/code-review-default.md`. The executor's own output IS streamed (JSON event stream parsed live into the per-change log); the review LLM is the remaining single-shot call.

To request an aspirational item, file an issue or open an OpenSpec change proposal in this repository. Self-modification guardrails apply when autocoder works on its own codebase; see [docs/SECURITY.md](docs/SECURITY.md).

---

## License

Licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

---

*Documentation maintained per the `project-documentation` OpenSpec rule. Any new capabilities or operational shifts must be updated here in the same change that introduces them.*
