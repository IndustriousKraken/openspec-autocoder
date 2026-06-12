# AI Security & Guardrails

Running an autonomous coding agent with push access to your repositories introduces unique risks. Adhere to the following practices.

## 1. Credential scoping

Never give autocoder a Personal Access Token (PAT) or SSH key with admin access to your organization. Provide it with **scoped access** strictly limited to the repositories defined in `config.yaml`. There are two paths:

- **Fine-grained PAT minted by your own account**, with the PAT's repository allowlist restricted to the autocoder-managed repos. The PAT enforces the scope.
- **Classic PAT minted by a machine user** whose account-level access is itself scoped (Read collaborator on specific repos via team membership). The minting user's permissions enforce the scope, not the PAT.

Either path is acceptable; what matters is that the credential cannot push, merge, or change settings outside the configured repos. An org-wide classic PAT minted by your own admin account is the configuration to avoid.

## 2. Branch protection

Protect your `main` and `dev` branches. autocoder must **never** be allowed to push directly to protected branches. It pushes only to the designated `agent_branch` and opens PRs for human review. Configure GitHub branch protection to require PR approval and (optionally) require PRs not be draft, so the reviewer's `Block` verdict actually gates merge.

## 3. The "self-modifying AI" risk

If you point autocoder at its own repository (e.g. `cicd-impl-agents`), there is a risk of the agent modifying its own source code in unexpected ways. A "lazy" LLM under pressure might try to satisfy failing tests by deleting them, modify the OpenSpec schema to avoid spec checks, or alter its own system prompts.

**Mitigation:** require human + reviewer-agent approval for any PR merged into autocoder's own repository. Never auto-merge autocoder's PRs into itself without a human in the loop.

## 4. Workspace isolation

autocoder clones repositories into `/tmp/workspaces/`. Ensure this partition has sufficient disk space and gets cleared of orphaned files on system restart (most distros mount `/tmp` as tmpfs by default, which handles this). Do not run autocoder with root privileges. The deploy user only needs:

- Write access to `/tmp/workspaces/`
- Write access to its own `~/.claude/` (for Claude Code credentials)
- Read access to `/home/autocoder/autocoder/config.yaml`

## 5. Secrets in `config.yaml` (inline vs env-var)

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

**Env-var form:** secrets stay out of `config.yaml`. Suits multi-user hosts and systemd deployments with `EnvironmentFile=/etc/autocoder.env`. When the install wizard writes `secrets.env`, it creates the file with mode `0600` atomically (mode applied in the same syscall that creates the file); no world-readable window exists during install.

**Inline form:** secrets live in `config.yaml`. Suits single-host, single-user deployments where one file is easier to manage than two. Requirements:

- `chmod 600` on the config file, owned by the autocoder user.
- Never commit it. The project root's `.gitignore` already excludes `config.yaml` by name.

## 6. Dedicated, non-SSH user (recommended)

Run autocoder as a dedicated user (`autocoder`) with no SSH login. Authenticate Claude Code as that user (`sudo -iu autocoder claude auth login`) and keep `config.yaml`, `~/.claude/`, and the daemon's process under that uid. A compromised login user must then clear an additional uid boundary to reach autocoder's secrets — meaningful when the login user is not a passwordless sudoer. The Deployment section's systemd setup follows this pattern.

## 7. Fork-and-PR workflow (recommended for org repos)

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

**Startup check:** autocoder probes each fork with `git ls-remote` before spawning any polling task. Missing forks are created automatically via the GitHub API and then polled (up to 60s) for reachability.

**Fork-setup failure degrades gracefully (it does not crash the daemon):** a per-repo fork-setup failure — the creation POST returns non-2xx (e.g. the PAT lacks fork permission, or upstream is inaccessible), or the fork is not reachable within the 60s timeout (e.g. an upstream rename left the fork under a different name) — does **not** abort startup. autocoder records the failure, **skips that repository for the process lifetime** (no polling task is spawned for it), and emits a **chatops alert** naming the repository with a brief remedy hint. The daemon and every other repository — and chatops — keep running; even if *every* configured repository fails fork setup, the daemon stays up rather than exiting non-zero and crash-looping under systemd. **Recovery:** fix the fork (ensure it exists and is reachable at the derived URL), then restart the daemon (or run `autocoder reload`) to pick the repository back up.

**Rewind in fork mode:** `autocoder rewind --hard` deletes the agent branch from the fork (not upstream), since that's where it lived.

**Limitations:** `fork_owner` is global; one machine user owns the forks for every repo in the config. Per-repo overrides are not supported. Two upstream repos with the same name (across different orgs) would map to the same fork URL — set explicit `local_path` and/or rename one fork to disambiguate.

## 8. Executor tool sandbox

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

**Threat model caveat:** this is a tool-routing-layer sandbox, not OS-level isolation. A determined model can in principle exec arbitrary code via the allowed `Bash` tool with command patterns that don't match the denylist. autocoder now wraps every agentic subprocess in a kernel-enforced OS-level sandbox *in addition* to this tool-routing layer — see [§9 OS-level agentic sandbox](#9-os-level-agentic-sandbox-a006). The tool-routing sandbox remains the first layer; the OS layer is the hard boundary.

**Lazy-archive structural detection.** Beyond the sandbox, autocoder inspects the working-tree diff after every executor invocation. If the only changes are renames into `openspec/changes/archive/<date>-<name>/`, the daemon treats the iteration as Failed (not Completed), reverts the staged moves via `git reset --hard`, and leaves the change pending for retry. This catches the "agent renamed the change directory and called itself done" failure mode regardless of which command produced the moves — the openspec-CLI denials above are belt-and-suspenders for the obvious path, but the structural check is what does the real work.

**Reviewer LLM is a separate data flow.** The code reviewer (if enabled) sends the diff to its configured LLM provider as a direct HTTP call. That data flow is governed by your `reviewer:` config (provider, api_key, api_base_url), NOT by `executor.sandbox`. Operators opted in to that flow by enabling the reviewer; sandbox restrictions do not apply.

---

## 9. OS-level agentic sandbox (a006)

Section 8 stops a model from *exfiltrating* over the tool layer. This section stops a model from *reaching* a credential that exists on the host in the first place — another CLI's config store, `~/.ssh`, autocoder's own config — by wrapping **every** agentic subprocess in a kernel-enforced sandbox. Enforcement is external to the wrapped CLI: the kernel applies it regardless of the CLI's own settings. Because the wrap lives on the single `agentic_run` spawn seam, no role (executor, audits, agentic reviewer, contradiction checks) can opt out.

**Mechanism (auto-detected at startup).** The subprocess is launched under `systemd-run` in transient **service** mode (PID 1 applies the namespace; stdout captured with `--pipe --wait --collect`), with a **`bwrap`** (bubblewrap) fallback for unprivileged / non-systemd / in-container hosts. What it enforces:

- **Filesystem — exposed-home denylist (default).** The home directory is **present and writable** so the wrapped CLI and its toolchains work (node/pyenv/rbenv/cargo, and the CLI's own install + session + caches all live under `$HOME`) — EXCEPT a default-deny **mask-list** of sensitive paths (`~/.ssh`, `~/.aws`, `~/.gnupg`, every *other* CLI's config store, package-manager credential files like `~/.cargo/credentials.toml`, and shell-init/persistence files) which is **masked** (empty/inaccessible) even inside an otherwise-exposed tool tree — unreadable even via a `Bash` `cat`. The **workspace** is read-write for the executor and **read-only** for read-only roles (audits / the agentic reviewer / the verifier gates) — except a writable, ephemeral project-scratch dir the CLI needs (e.g. opencode's `<workspace>/.opencode/`), overlaid on a tmpfs so the CLI's project state works while the repo's tracked files stay read-only and the scratch is discarded after the run. That read-only-workspace posture is the only filesystem difference between roles — their "read-only" is the repo, not the home (a read-only role may read the home and write its own caches/session, but cannot modify the repo). System paths outside `$HOME` are read-only. **Strict mode** (opt-in, high-compliance) swaps this for a **masked-home allowlist**: the home is replaced with an empty tmpfs and only the workspace, the role's own CLI store, the resolved CLI binary + its dependency closure, and the minimal runtime are bound back — accepting that a toolchain-heavy CLI (e.g. a Node app whose runtime sprawls under `$HOME`) may be unable to start under the mask.
- **Capability drops.** `CAP_NET_RAW` (no raw-socket sniffing), `CAP_NET_ADMIN` (no route/iptables hijack), `CAP_SYS_PTRACE` (no reading another process's memory); `NoNewPrivileges`; address families restricted to exclude `AF_PACKET`.
- **Process-table restriction.** `/proc` is mounted so the subprocess cannot read another process's `environ` or `mem`.

**Network egress is deliberately NOT restricted** by this sandbox. Egress control belongs to the host firewall, and there is no maintainable in-app allowlist for CDN'd API/forge hosts. The sandbox does filesystem and host isolation, not a network allowlist.

### Two credential-store toggles

CLI config stores get two protection layers, each an independent toggle under `executor.sandbox` (and per-repository), **both ON by default**:

| Toggle | Layer | Fail mode | What it does |
|--------|-------|-----------|--------------|
| `os_hide` | filesystem mask-list | **fail-closed** | Every CLI store *other than the running role's own* is on the sandbox mask-list, so it is masked (absent) from the namespace regardless of policy. It cannot protect the running role's own store (which must stay readable to authenticate); it protects every other store. |
| `engine_deny` | the wrapped CLI's per-invocation tool-use denylist | fail-open | Denies the agent's `Read`/`Bash` tools on the config store of **every** registered CLI strategy — the self-store included. Covers the one store `os_hide` cannot, but is a string-pattern speed bump that determined shell indirection can evade: it deters, it does not bound. Supplied per-invocation; the operator's global CLI config is never mutated. |

The running role's own CLI store stays readable by that same-uid subprocess because the CLI must read it to authenticate. Disclosure of that one store means a model could impersonate that CLI to *its own* provider — never reach another provider's credential or another secret. That residual is bounded by the single-store blast radius plus `engine_deny` deterrence.

### Supplied LLM keys for CLI roles (opt-in exposure)

A CLI/agentic role (a verifier gate, the agentic reviewer, an audit) authenticates **by default** from the wrapped CLI's own login/store — no LLM credential reaches the subprocess. That is the recommended, no-exposure default: **omit `api_key`** for these roles, and `api_key` is not required at config-load for them.

If you *do* supply an `api_key` for a CLI role, it is passed to the CLI so the CLI uses that key — `claude`/`agy` via the subprocess env (`ANTHROPIC_API_KEY` / `AV_API_KEY`), `opencode` via an `{env:...}` reference in the workspace `opencode.json` resolved from the env (the raw secret is never written into that committed file). Because the model and the wrapped CLI share the **same process and uid**, a key the CLI can use is one the model can ultimately read: `engine_deny` deters file reads but cannot bound an env read. Supplying a key is therefore a deliberate **opt-in to that exposure**; the daemon logs one startup WARN per keyed CLI role as a reminder. For untrusted inputs, prefer the no-key default. In-process HTTP roles (the `oneshot` reviewer, RAG) are unaffected — their key stays in the daemon process and never reaches a subprocess.

### Presets (documentation over the two switches)

| Preset | `os_hide` | `engine_deny` | When |
|--------|-----------|---------------|------|
| **Secure default** | on | on | Every normal repository. |
| **CLI-wrapping repo** | **off** | on | A repository whose code *develops CLI wrappers* and needs a nested CLI to authenticate live against another CLI's store. **This repository (autocoder itself) is in this category** — under the secure default, its live cross-CLI development breaks, so it sets `os_hide: false` (and is logged at startup, see below). |
| **Credential-grab testing** | off | off | A repository whose explicit purpose is testing credential-grab behavior. |

### Precedence and logging

- A per-repository `sandbox.os_hide` / `sandbox.engine_deny` value **overrides** the global `executor.sandbox` value for that repository; absent both, the secure default (ON) applies. There is no implicit downgrade.
- Loosening either toggle is explicit and **logged**: the daemon emits a per-repository startup **WARN** naming each toggle that is OFF for that repository.

### No-mechanism fail-closed + opt-in

When **no** sandbox mechanism is available (neither `systemd-run` service mode nor `bwrap` can apply the sandbox), agentic runs **fail closed** with a clear error naming the missing mechanism — no unsandboxed subprocess is spawned — **unless** the operator explicitly sets `executor.sandbox.allow_unsandboxed: true`. With that opt-in, runs proceed and the daemon emits a loud startup WARN that subprocesses are running unsandboxed. The opt-in is daemon-wide (not per-repository) and is **not recommended**: a wrapped model can then reach host credentials.

```yaml
executor:
  kind: claude_cli
  sandbox:
    os_hide: true          # default; hide other CLIs' stores
    engine_deny: true      # default; deny-read every CLI store at the tool layer
    allow_unsandboxed: false  # default; fail closed when no mechanism is available

repositories:
  - url: "git@github.com:you/cli-wrapping-repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
    sandbox:
      os_hide: false       # this repo wraps CLIs; a nested CLI must authenticate live
```

---
