# Configuration Reference

Full schema of `config.yaml`. The minimal viable file is in [config.example.yaml](config.example.yaml); everything below is for tuning or enabling optional capabilities.

`config.example.yaml` ships annotated comments for every field documented below; copy it as a starting point for your own `config.yaml`.

## `repositories:` (required)

A list of one or more repositories to manage. Each entry:

| Field                | Required | Default | Description |
|----------------------|----------|---------|-------------|
| `url`                | yes      | —       | Git URL (SSH or HTTPS). |
| `base_branch`        | yes      | —       | The branch agent work is based off of (typically `main` or `dev`). |
| `agent_branch`       | yes      | —       | The branch the daemon pushes work to (typically `agent-q`). |
| `poll_interval_sec`  | yes      | —       | Seconds between iterations on this repo. |
| `local_path`         | no       | derived | See [Workspace path derivation](OPERATIONS.md#workspace-path-derivation). |
| `chatops_channel_id` | no       | falls back to `chatops.default_channel_id` | See [ChatOps Escalation](CHATOPS.md). |
| `max_changes_per_pr` | no       | falls back to `executor.max_changes_per_pr`, then `3` | Upper bound on archived changes committed in one iteration's PR. Remaining pending changes wait for the next iteration. A value of `0` is clamped to `1` with a WARN log at startup. |

## `executor:` (required)

| Field                       | Required | Default       | Description |
|-----------------------------|----------|---------------|-------------|
| `kind`                      | yes      | —             | Currently only `claude_cli` is supported. |
| `command`                   | no       | `claude`      | Path to the wrapped CLI. Set only if `claude` isn't on `$PATH`. |
| `timeout_secs`              | no       | `1800`        | Wall-clock budget per change. Killed-and-Failed on overrun. |
| `sandbox`                   | no       | safe defaults | Tool-use restrictions applied to every executor invocation. See [Executor tool sandbox](SECURITY.md#8-executor-tool-sandbox). |
| `implementer_prompt_path`   | no       | _embedded_    | Path to a file overriding the built-in implementer prompt template. The template must contain the literal `{{change_body}}` placeholder, which is replaced with `openspec instructions apply` output at each invocation. Unset means use the template compiled into the binary. |
| `perma_stuck_after_failures`| no       | `2`           | Consecutive Failed iterations after which a change is marked perma-stuck. See [Perma-stuck change detection](OPERATIONS.md#perma-stuck-change-detection). A value of `0` is clamped to `1` with a WARN log at startup. |
| `max_changes_per_pr`        | no       | `3`           | Default cap on archived changes committed in one iteration's PR; per-repo `max_changes_per_pr` overrides. Operators with long queues see them ship across multiple iterations instead of one large PR. A value of `0` is clamped to `1` with a WARN log at startup. |
| `startup_jitter_max_secs`   | no       | `30`          | Each polling task waits a uniformly random `[0, startup_jitter_max_secs]` seconds before its first iteration. Staggers a fleet of concurrent `git fetch` operations so an IDS does not see a synchronized burst. Set to `0` to disable. See [Polling cadence and your firewall](OPERATIONS.md#polling-cadence-and-your-firewall). |
| `inter_iteration_jitter_pct`| no       | `10`          | Each inter-iteration sleep is `poll_interval_sec` adjusted by ±this percent (uniform random offset). Prevents long-term re-synchronization of multiple tasks. Set to `0` for exact intervals. Values above `100` are clamped to `100`. |

## `github:` (required)

| Field          | Required | Default          | Description |
|----------------|----------|------------------|-------------|
| `token_env`    | no       | `GITHUB_TOKEN`   | Name of the env var holding the fallback PAT. |
| `token`        | no       | _absent_         | Inline alternative to `token_env`: `{ value: "ghp_..." }`. When set, `token_env` is ignored. See [Secrets in `config.yaml`](SECURITY.md#5-secrets-in-configyaml-inline-vs-env-var). |
| `owner_tokens` | no       | _absent_         | Optional map of GitHub owner → env var name **or** inline `{ value: "..." }`. See [Multiple GitHub Tokens](CONFIG.md#multiple-github-tokens). |
| `fork_owner`   | no       | _absent_         | Enables fork-and-PR mode. Names the GitHub handle that owns the forks. See [Fork-and-PR workflow](SECURITY.md#7-fork-and-pr-workflow-recommended-for-org-repos). |
| `recreate_fork_on_reinit` | no | `false` | When `true` AND fork-PR mode is active AND the workspace directory was absent at iteration start (fresh clone), autocoder deletes the existing fork on GitHub and re-forks upstream before initializing the workspace. Recovers cleanly when the fork has accumulated stale branches no one cares about. **Destructive**: any open PRs whose head branch lives on the deleted fork are closed by GitHub when the head ref disappears. Requires the operator's PAT to include the `delete_repo` scope (without it, the DELETE returns 403, autocoder logs ERROR, and falls back to the conservative non-recreating init path). See [Operating notes — fork recreation on workspace reinitialization](OPERATIONS.md#fork-recreation-on-workspace-reinitialization). |

## `reviewer:` (optional)

See [Code Review](CODE-REVIEW.md). Absent block disables the reviewer step.

## `chatops:` (optional)

See [ChatOps Escalation](CHATOPS.md). The block carries a required `provider:` field (`slack` officially supported; `discord`, `teams`, `mattermost`, `matrix` are [EXPERIMENTAL](CHATOPS.md#experimental-chatops-backends)) plus a `default_channel_id:` and a per-provider sub-block. Absent block disables ChatOps escalation; an executor `AskUser` outcome falls back to "log and exit the iteration" behavior.

## `audits:` (optional)

Top-level periodic-audit framework configuration. Absent block → every audit's effective cadence is `disabled` and the daemon behaves identically to a build without the framework. See [Periodic audits](OPERATIONS.md#periodic-audits) for the full operational model.

> **Already installed via the wizard?** The `autocoder install` flow already wrote your cadence choices into this block. This section is for operators editing `config.yaml` directly, onboarded via the [source build](INSTALL.md), or adjusting cadences after first install.

| Field | Type | Description |
|---|---|---|
| `defaults` | `map<audit-slug, Cadence>` | Global default cadence per audit type. Audit slugs must match a registered type (currently `architecture_brightline`, `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`); typos fail at config load with a list of known slugs. |
| `settings` | `map<audit-slug, AuditSettings>` | Per-audit knobs. See below. |

Per-repo override: each entry under `repositories[]` accepts an `audits:` field that maps audit slugs to cadences. Per-repo entries take precedence over `audits.defaults`; an absent entry in both locations resolves to `disabled`.

**`Cadence` syntax (string):** `disabled` | `daily` | `every-N-days` (N must be a positive integer; `every-0-days` and negative N are rejected at load time) | `weekly` | `monthly` | `quarterly`.

**`AuditSettings` fields:**

| Field | Type | Description |
|---|---|---|
| `prompt_path` | `path` (optional) | Override the audit's embedded LLM prompt template. Used by `drift_audit` (embedded default: `prompts/drift-audit.md`), `missing_tests_audit` (embedded default: `prompts/missing-tests-audit.md`), `security_bug_audit` (embedded default: `prompts/security-bug-audit.md`), and `architecture_consultative` (embedded default: `prompts/architecture-consultative.md`). An empty file is rejected at audit invocation so the daemon does not feed an empty prompt to the wrapped CLI. |
| `notify_on_clean` | `bool` (default `false`) | When `true`, an empty-findings `Reported` outcome posts `✅ <repo>: <audit_type> — no findings` to chatops. When `false`, silence is success. |
| `extra` | `map<string, yaml>` | Free-form per-audit knobs. `architecture_brightline` reads `file_lines_threshold` (default `800`) from here. `missing_tests_audit` and `security_bug_audit` each read `max_proposals_per_run` (`u32`, default `2`) from here. `drift_audit` and `architecture_consultative` do not currently read any `extra` knobs; configure them via the top-level `prompt_path` and `notify_on_clean` fields under `audits.settings.<slug>`. |

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

See [Secrets in `config.yaml`](SECURITY.md#5-secrets-in-configyaml-inline-vs-env-var) for the security tradeoff.

### git operations are separate

This routing affects only HTTP calls to GitHub's REST API (PR creation, optional label fallback). Git operations (`clone`, `fetch`, `push`) go through whichever authentication `git` itself uses — your SSH key, an HTTPS credential helper, etc.

**Recommendation for multi-owner setups:** use SSH URLs (`git@github.com:owner/repo.git`) in `config.yaml`. A single SSH key registered against each account/org covers the git side without per-owner credential-helper configuration, while autocoder's `owner_tokens` covers the API side. HTTPS URLs work but require a git credential helper that can map URLs to different PATs, which autocoder does not configure for you.

### Non-goal: per-repository overrides

Two repositories under the same owner cannot use different tokens. Token routing is per-owner only.

## `executor.max_revisions_per_pr`

Maximum number of `@<bot> revise <text>` rounds applied to a single open
PR before further triggering comments are silently ignored. Default `5`.
A value of `0` disables the PR-comment revision channel entirely (the
dispatcher becomes a no-op).

Values above `20` are clamped to `20` at startup with a WARN log line —
the ceiling exists so a runaway operator config (`max_revisions_per_pr:
1000`) cannot let one PR burn tokens forever.

```yaml
executor:
  kind: claude_cli
  max_revisions_per_pr: 5    # default; set to 0 to disable, max 20
```

See [OPERATIONS.md](OPERATIONS.md#revising-an-open-pr-via-comment) for the
full revision-loop flow. The cap is per PR (not per repository); each PR
tracks its own count under
`<workspace>/.autocoder/revisions/<pr-number>.json`. When a PR is closed
or merged, its state file is pruned automatically — the cap resets if the
PR is re-opened.

