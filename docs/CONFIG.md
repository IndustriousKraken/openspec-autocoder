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
| `spec_storage`       | no       | unset (workspace-internal specs) | OSS-fork support (a26): see [`spec_storage`](#repositoriesspec_storage). |
| `upstream`           | no       | unset (no upstream remote management) | OSS-fork support (a26): see [`upstream`](#repositoriesupstream). |
| `auto_submit_pr`     | no       | `true`  | OSS-fork support (a26): see [`auto_submit_pr`](#repositoriesauto_submit_pr). |

### `repositories[].spec_storage` {#repositoriesspec_storage}

OSS-fork support (a26). When set, autocoder treats `<path>/openspec/`
as the canonical-spec source AND destination INSTEAD OF
`<workspace>/openspec/`. Used when canonical specs cannot live inside
the code repo (typically an upstream OSS project that does not use
spec-driven development).

| Field  | Required | Default | Description |
|--------|----------|---------|-------------|
| `path` | yes      | —       | Workspace-relative OR absolute path to a directory. SHALL be a git working tree containing an `openspec/` subdirectory. |

Validation (config-load fail-fast):

- The resolved `path` MUST exist AND MUST be a directory.
- The directory MUST be a git working tree (verified via
  `git -C <path> rev-parse --is-inside-work-tree`).
- The subdirectory `<path>/openspec/` MUST exist.

Cross-link: see the OSS contribution workflow in
[`docs/OPERATIONS.md`](OPERATIONS.md#oss-contribution-workflow) for
the recommended setup.

### `repositories[].upstream` {#repositoriesupstream}

OSS-fork support (a26). When set, autocoder ensures the workspace has
a git remote named `<remote>` pointing at `<url>` AND opportunistically
runs `git fetch <remote>` at the start of every polling iteration. The
fetch is best-effort: failures log a WARN but do not block the
iteration. Enables — but does NOT trigger — automatic upstream
syncing; syncing is operator-initiated via the `@<bot> sync-upstream`
chatops verb (see [`docs/CHATOPS.md`](CHATOPS.md)).

| Field    | Required | Default      | Description |
|----------|----------|--------------|-------------|
| `remote` | no       | `"upstream"` | Git remote name to manage. |
| `branch` | no       | `"main"`     | The upstream repo's primary branch. |
| `url`    | yes      | —            | The upstream repo's git URL (SSH OR HTTPS). |

Validation: `url` MUST be non-empty. Reachability is NOT checked at
config-load — that's the polling iteration's concern.

Cross-link:
[`docs/OPERATIONS.md`'s OSS contribution workflow](OPERATIONS.md#oss-contribution-workflow).

### `repositories[].auto_submit_pr` {#repositoriesauto_submit_pr}

OSS-fork support (a26). Gates whether autocoder auto-opens the
end-of-iteration PR.

| Value           | Behavior |
|-----------------|----------|
| `true` (default)| Existing behavior: the agent branch is pushed AND a PR is opened via the GitHub REST API. The chatops thread reply is `✅ PR opened: <url>`. |
| `false`         | The agent branch is pushed BUT the PR-creation API call is skipped. The chatops thread reply is `📦 Branch pushed: <branch-url>` followed by `Run: gh pr create --base <upstream.branch \| base-branch> --head <agent-branch>` so the operator can submit the upstream PR manually after local review. |

`auto_submit_pr: false` is the recommended setting for OSS-fork
deployments where a bad PR damages your reputation with upstream
maintainers in a way an internal PR does not. All other end-of-pass
behaviors (implementer-summary capture, reviewer run, etc.) execute
unchanged.

`auto_submit_pr` applies UNIFORMLY to both code-workspace PR creation
AND `spec_storage` PR creation (when set). Operators wanting
different behavior for the two cases SHALL split the workspace into
separate per-repo configurations.

Cross-link:
[`docs/OPERATIONS.md`'s OSS contribution workflow](OPERATIONS.md#oss-contribution-workflow).

## `executor:` (required)

| Field                       | Required | Default       | Description |
|-----------------------------|----------|---------------|-------------|
| `kind`                      | yes      | —             | Currently only `claude_cli` is supported. |
| `command`                   | no       | `claude`      | Path to the wrapped CLI. Set only if `claude` isn't on `$PATH`. |
| `timeout_secs`              | no       | `1800`        | Wall-clock budget per change. Killed-and-Failed on overrun. |
| `sandbox`                   | no       | safe defaults | Tool-use restrictions applied to every executor invocation. See [Executor tool sandbox](SECURITY.md#8-executor-tool-sandbox). |
| `implementer_prompt_path`   | no       | _embedded_    | Path to a file overriding the built-in implementer prompt template. The template must contain the literal `{{change_body}}` placeholder, which is replaced with `openspec instructions apply` output at each invocation. Unset means use the template compiled into the binary. Operators with override templates MAY mention `query_canonical_specs` (a21 — see `canonical_rag:`) in their prompt OR ignore the new tool entirely; the tool stays registered regardless. |
| `perma_stuck_after_failures`| no       | `2`           | Consecutive Failed iterations after which a change is marked perma-stuck. See [Perma-stuck change detection](OPERATIONS.md#perma-stuck-change-detection). A value of `0` is clamped to `1` with a WARN log at startup. |
| `max_changes_per_pr`        | no       | `3`           | Default cap on archived changes committed in one iteration's PR; per-repo `max_changes_per_pr` overrides. Operators with long queues see them ship across multiple iterations instead of one large PR. A value of `0` is clamped to `1` with a WARN log at startup. |
| `startup_jitter_max_secs`   | no       | `30`          | Each polling task waits a uniformly random `[0, startup_jitter_max_secs]` seconds before its first iteration. Staggers a fleet of concurrent `git fetch` operations so an IDS does not see a synchronized burst. Set to `0` to disable. See [Polling cadence and your firewall](OPERATIONS.md#polling-cadence-and-your-firewall). |
| `inter_iteration_jitter_pct`| no       | `10`          | Each inter-iteration sleep is `poll_interval_sec` adjusted by ±this percent (uniform random offset). Prevents long-term re-synchronization of multiple tasks. Set to `0` for exact intervals. Values above `100` are clamped to `100`. |
| `wipe_drain_timeout_secs`   | no       | `30`          | Seconds the `@<bot> wipe-workspace` flow waits for the in-flight per-repo polling iteration to drain (release its busy marker) after the operator types `confirm`. The wipe runs regardless of whether the drain completes within the window — the directory is going away one way or another; the drain is a politeness, not a hard precondition. `0` skips the await entirely (the wipe runs immediately whether the iteration responded or not). Values above `300` (5 minutes) are clamped to `300` with a WARN log at startup: a longer wait holds the chatops listener busy for too long and almost always indicates misconfiguration. See [Two-step confirmation for `wipe-workspace`](CHATOPS.md#two-step-confirmation-for-wipe-workspace). |
| `output_format`             | no       | `json`        | Output format for the wrapped Claude CLI. `json` (default) invokes the CLI with `--output-format stream-json` and parses one JSON event per stdout line into a structured per-change log (PROMPT / ACTIONS / FINAL ANSWER / STDERR). Operators reading the log get the agent's tool-call history even when a timeout-kill ended the run mid-flight. `text` is the opt-out: the streaming parser is skipped, the log uses the legacy PROMPT / STDOUT / STDERR shape, and the PR comment reads raw stdout (today's pre-streaming behavior). Use `text` when a custom Claude CLI build lacks the streaming JSON format OR when debugging the executor itself. See [Per-change run log shape](OPERATIONS.md#per-change-run-log-shape). |
| `log_retention_days`        | no       | `30`          | Per-change run-log retention window in days. At daemon startup and every 24 hours during operation, logs older than `now - log_retention_days × 86400` seconds whose corresponding change directory is no longer active are deleted. Active changes' logs are preserved regardless of age — operators triaging a long-running stuck change want its log even if old. Values above `365` are clamped to `365` with a WARN log at startup. |
| `busy_marker_stale_threshold_secs` | no | `600` | Stale-threshold (in seconds) for the live-PID busy-marker recovery branch. The next polling iteration finding an existing marker whose recorded PID is STILL ALIVE AND older than this value treats the pass as stuck: `SIGTERM`s the process group, waits 5s, `SIGKILL`s if still alive, clears the marker, and proceeds. **Decoupled** from `executor.timeout_secs` — raising the executor timeout for one legitimately long-running change does NOT delay stale-marker recovery on unrelated iterations. Dead-PID markers (recorded `pid` no longer in `/proc`) are recovered IMMEDIATELY regardless of this value; this field gates ONLY the live-PID branch. `0` is permitted — every live-PID marker is then treated as stale on inspection (useful for diagnostics). Values above `7200` (2 hours) are clamped to `7200` with a WARN log at startup. See [Busy marker](OPERATIONS.md#busy-marker). |
| `change_internal_contradiction_check` | no | `disabled` | Opt-in gate for the change-internal contradiction pre-flight (a19). `disabled` (default) skips the LLM call entirely. `enabled` runs the check AFTER `a17`'s archivability check AND BEFORE the executor; non-empty findings write `.needs-spec-revision.json` and halt the queue walk. Enabling without `change_internal_contradiction_check_llm` is a fail-fast startup error. See [Pre-flight checks](OPERATIONS.md#pre-flight-checks). |
| `change_internal_contradiction_check_prompt_path` | no | _embedded_ | Path to a file overriding the built-in contradiction-check prompt template. Unset → use the template compiled into the binary from `prompts/change-contradiction-check.md`. An empty override file is rejected at use time so the daemon does NOT feed an empty prompt to the LLM. See [Pre-flight checks](OPERATIONS.md#pre-flight-checks). |
| `change_internal_contradiction_check_llm` | required when enabled | _absent_ | LLM block for the contradiction check. Fields parallel the `reviewer:` LLM surface — `provider` (`anthropic` \| `openai_compatible` \| `ollama`), `model`, `api_key_env` / `api_key`, `api_base_url`. `provider` may be **omitted** to reference a [`models:` nickname](#models-optional) (its `model` is then the nickname). Held as its own block so operators can pick a cheaper model than the reviewer (the prompt is small AND the failure mode is fail-open). See [Pre-flight checks](OPERATIONS.md#pre-flight-checks). |

## `github:` (required)

| Field          | Required | Default          | Description |
|----------------|----------|------------------|-------------|
| `token_env`    | no       | `GITHUB_TOKEN`   | Name of the env var holding the fallback PAT. |
| `token`        | no       | _absent_         | Inline alternative to `token_env`: `{ value: "ghp_..." }`. When set, `token_env` is ignored. See [Secrets in `config.yaml`](SECURITY.md#5-secrets-in-configyaml-inline-vs-env-var). |
| `owner_tokens` | no       | _absent_         | Optional map of GitHub owner → env var name **or** inline `{ value: "..." }`. See [Multiple GitHub Tokens](CONFIG.md#multiple-github-tokens). |
| `fork_owner`   | no       | _absent_         | Enables fork-and-PR mode. Names the GitHub handle that owns the forks. See [Fork-and-PR workflow](SECURITY.md#7-fork-and-pr-workflow-recommended-for-org-repos). |
| `recreate_fork_on_reinit` | no | `false` | When `true` AND fork-PR mode is active AND the workspace directory was absent at iteration start (fresh clone), autocoder deletes the existing fork on GitHub and re-forks upstream before initializing the workspace. Recovers cleanly when the fork has accumulated stale branches no one cares about. **Destructive**: any open PRs whose head branch lives on the deleted fork are closed by GitHub when the head ref disappears. Requires the operator's PAT to include the `delete_repo` scope (without it, the DELETE returns 403, autocoder logs ERROR, and falls back to the conservative non-recreating init path). See [Operating notes — fork recreation on workspace reinitialization](OPERATIONS.md#fork-recreation-on-workspace-reinitialization). |
| `command_authorization` | no | _default-deny_ | Authorizes who may trigger GitHub comment-sourced verbs (`@<bot> revise`, `@<bot> code-review`). See [`github.command_authorization`](#githubcommand_authorization) below. |

### `github.command_authorization`

Before dispatching **any** verb parsed from a GitHub PR/issue comment
(`@<bot> revise`, `@<bot> code-review`, and any future comment verb), the
daemon authorizes the commenter. This is the GitHub analog of the Slack
channel allowlist; the Slack path is unaffected. Authorization passes when
**either**:

- the comment's GitHub `author_association` is in `allowed_associations`; **or**
- the comment author's `login` is in `allowed_users`.

A comment that parses as a verb but whose author is **not** authorized is
**dropped before dispatch** (default-deny): no executor, reviewer, or other
billed/LLM work runs, the seen-marker is advanced so it does not re-fire,
and the drop is logged at INFO with the `login` and `author_association`.
An absent or unrecognized association is treated as unauthorized (it can
still pass via `allowed_users`).

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `allowed_associations` | no | `[OWNER, MEMBER, COLLABORATOR]` | GitHub `author_association` values that authorize a commenter — by default exactly those carrying write/triage permission. Each entry is validated at startup against the GitHub set (`OWNER`, `MEMBER`, `COLLABORATOR`, `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, `FIRST_TIMER`, `NONE`); an unknown value fails config load with a clear error. |
| `allowed_users` | no | `[]` | Additional trusted GitHub logins authorized regardless of association (for individuals who are not formal collaborators). Empty or whitespace-only entries are rejected at startup so an operator typo fails config load with a clear error rather than sitting silently in the allowlist. |
| `decline_comment` | no | `false` | When `true`, the daemon posts exactly one polite decline reply per dropped trigger. When `false` (the default), unauthorized triggers are silently ignored (no comment spam, no reply/feedback loops). |

```yaml
github:
  command_authorization:
    allowed_associations: [OWNER, MEMBER, COLLABORATOR]   # default
    allowed_users: [trusted-maintainer-handle]            # default []
    decline_comment: false                                # default
```

See [Operating notes — authorizing PR-comment triggers](OPERATIONS.md#authorizing-pr-comment-triggers).

## `models:` (optional)

A top-level **model registry**: define a model once under a nickname, then
reference it by name from any LLM-consuming block (`reviewer:`,
`canonical_rag:`, `executor.change_internal_contradiction_check_llm:`).
Absent block → no registry; every LLM block must be inline. The registry is
the de-duplicated form, **not** a forced migration — inline blocks remain
valid indefinitely.

`models:` is a map from nickname to an entry carrying the same four LLM
fields the per-subsystem blocks use, plus an optional `cli:` override:

| Field | Required | Default | Description |
|---|---|---|---|
| `provider` | yes | — | `anthropic`, `openai_compatible`, or `ollama`. Always inline on a registry entry (the nickname shorthand lives on the *referencing* block, not here). |
| `model` | yes | — | Provider-specific model identifier. |
| `api_base_url` | depends | provider default | Required for `openai_compatible` and `ollama`; optional for `anthropic` (defaults to `https://api.anthropic.com`). |
| `api_key_env` / `api_key` | depends | _absent_ | Provider API key (env-var name, or inline `{ value: "..." }`). Required for `anthropic`/`openai_compatible`; **forbidden** for `ollama`. Inline wins over `api_key_env` (dual-set logs a WARN). |
| `cli` | no | from `provider` | Overrides the default agentic CLI for this model (`claude` \| `opencode`). |

Each entry is validated at config-load via the same per-provider auth rules
as an inline block — e.g. an `ollama` entry with an `api_key` fails load even
if no block references it.

### Nickname references (omit `provider`)

An LLM block is **discriminated by the presence of `provider`**:

- A block that **sets** `provider` is the legacy inline form; the registry
  is **not** consulted. Every existing config takes this path unchanged.
- A block that **omits** `provider` has its `model` field interpreted as a
  `models:` nickname and resolved to that entry's
  `(provider, model, api_base_url, api_key/api_key_env)` before the block's
  downstream consumer runs.

```yaml
models:
  beefy_security:
    provider: openai_compatible
    model: moonshotai/kimi-k2
    api_base_url: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_KEY
reviewer:
  enabled: true
  model: beefy_security        # no provider → registry nickname
change_internal_contradiction_check_llm:
  provider: anthropic          # provider present → legacy inline, unchanged
  model: claude-opus-4-8
  api_key_env: ANTHROPIC_API_KEY
```

A nickname that names no registry entry fails config-load with an error
naming both the missing nickname and the referencing block. The resolved
provider passes the same per-subsystem validity gate as an inline provider
(e.g. a `canonical_rag` block resolving to `anthropic` is rejected, because
Anthropic exposes no embeddings API).

### Provider → default CLI rule

Each entry's `provider` fixes the default agentic CLI for that model:

| Provider | Default CLI |
|---|---|
| `anthropic` | `claude` |
| `openai_compatible` | `opencode` |
| `ollama` | `opencode` |

The optional per-entry `cli:` field overrides this default (e.g. drive an
`openai_compatible` model through the `claude` CLI with `cli: claude`). The
CLI strategies that consume this rule arrive with the agentic-run primitive
(a later change); the registry defines the rule so model and CLI selection
are specified in one place.

## `reviewer:` (optional)

See [Code Review](CODE-REVIEW.md). Absent block disables the reviewer step.

| Field                      | Required | Default | Description |
|----------------------------|----------|---------|-------------|
| `enabled`                  | no       | `false` | Master toggle. When `false`, the reviewer step is skipped entirely even if the block is present. |
| `provider`                 | no¹      | —       | `anthropic`, `openai_compatible`, or `ollama`. **Omit** to interpret `model` as a [`models:` nickname](#models-optional); when set, the block is inline and the registry is not consulted. |
| `model`                    | yes      | —       | Provider-specific model identifier, **or** a `models:` nickname when `provider` is omitted. |
| `api_key_env`              | no       | _absent_ | Name of the env var holding the provider API key. Used when `api_key` is unset. |
| `api_key`                  | no       | _absent_ | Inline alternative to `api_key_env` (`{ value: "..." }`); when set, `api_key_env` is ignored. |
| `api_base_url`             | no       | provider default | Override the base URL — useful for OpenRouter, Grok, local Ollama, etc. |
| `prompt_template_path`     | no       | _embedded_ | Path to a file overriding the built-in reviewer prompt template. Must contain `{{change_context}}`, `{{changed_files}}`, and `{{diff}}` placeholders. |
| `auto_revise`              | no       | `false` | When `true`, posts one `<!-- reviewer-revision -->` PR comment per concern the reviewer marked `should_request_revision: true` (with a non-empty `actionable_request`) — fires on actionable concerns **regardless of verdict** (`Pass`, `Concerns`, or `Block`). The [PR-comment revision dispatcher](OPERATIONS.md#revising-an-open-pr-via-comment) picks them up on the next iteration. Reviewer-initiated revisions are **automatic** and count against the per-PR `executor.max_auto_revisions_per_pr` cap (human `@<bot> revise` requests do not); concerns dropped due to the cap budget are annotated in the `## Code Review` PR-body section with `(not auto-revised; cap budget exhausted)`. Operator-customized reviewer templates must be updated to emit the structured `revision-requests` YAML block at the end of the response — see [Reviewer-initiated revisions on actionable concerns](CODE-REVIEW.md#reviewer-initiated-revisions-on-actionable-concerns) for the schema and the operator-template migration steps. The legacy key `auto_revise_on_block` is accepted as a silent alias. Default `false` (no behavioural change for sites already running the reviewer). |
| `prompt_budget_chars`      | no       | `2000000` | Maximum size (in chars) of the rendered reviewer prompt body — change context + changed files + diff combined. No hard ceiling; operator matches the value to their LLM provider's actual context window (Grok-4 / Claude Sonnet 4.6 fit `4000000`+; smaller-window providers may want a tighter cap). YAML integers do NOT accept underscore separators — write the value as a plain decimal. Hot-applicable via `autocoder reload`. See [Prompt budget](CODE-REVIEW.md#prompt-budget) for the full discussion. |
| `mode`                     | no       | `bundled` | Reviewer dispatch mode. `bundled` (default) keeps the existing one-reviewer-call-per-PR behaviour. `per_change` dispatches one reviewer call per change in a multi-change PR, emits a separate `## Code Review: <slug>` section per change in the PR body, and scales LLM cost linearly with the change count. See [Per-change reviewer mode](CODE-REVIEW.md#per-change-reviewer-mode) for the full discussion. |

¹ `provider` is required **unless** the block references a [`models:`
nickname](#models-optional), in which case it must be omitted (its presence
is what discriminates the inline form from the nickname form). When omitted,
`api_base_url` and `api_key`/`api_key_env` are also resolved from the
registry entry and should not be set on the block.

## `chatops:` (optional)

See [ChatOps](CHATOPS.md). The block carries a required `provider:` field (`slack` officially supported; `discord`, `teams`, `mattermost`, `matrix` are [EXPERIMENTAL](CHATOPS.md#experimental-chatops-backends)) plus a `default_channel_id:` and a per-provider sub-block. Absent block disables ChatOps; an executor `AskUser` outcome falls back to "log and exit the iteration" behavior.

### `chatops.slack:` (when `provider: slack`)

| Field | Type | Description |
|---|---|---|
| `bot_token_env` / `bot_token` | string / inline | Slack bot token (`xoxb-*`). Set one or the other; inline takes precedence. |
| `app_token_env` / `app_token` | string / inline (optional) | Slack app-level token (`xapp-*`) used by the Socket Mode inbound listener. When absent, the listener is not started; outbound chatops still works. |
| `listen_channels` | `[string]` (optional) | Extra channel IDs the inbound listener honours commands in, on top of every `repositories[].chatops_channel_id` and `chatops.default_channel_id`. |
| `dedup_cache_capacity` | `usize` (default `100`, max `10000`) | Maximum number of recently-processed `app_mention` events the inbound listener remembers in its dedup cache. Slack's Socket Mode delivery is at-least-once; the cache suppresses redeliveries so a single operator message never produces a duplicate bot reply. Values above the cap are clamped to `10000` with a WARN log at startup. Setting `0` disables dedup entirely (every redelivery is dispatched — pre-spec behaviour). See [CHATOPS.md → Duplicate-delivery suppression](CHATOPS.md#duplicate-delivery-suppression). |
| `dedup_cache_ttl_secs` | `u64` (default `600`, max `3600`) | Per-entry TTL (seconds) for the dedup cache — entries older than this are treated as not-present on the next lookup. Values above the cap are clamped with a WARN log; `0` is clamped to `1` with a WARN (use `dedup_cache_capacity: 0` to disable dedup, not TTL `0`). Raise to e.g. `3600` only when Slack-side redelivery storms span more than the default 10 minutes (rare). |

## `audits:` (optional)

Top-level periodic-audit framework configuration. Absent block → every audit's effective cadence is `disabled` and the daemon behaves identically to a build without the framework. See [Periodic audits](OPERATIONS.md#periodic-audits) for the full operational model.

> **Already installed via the wizard?** The `autocoder install` flow already wrote your cadence choices into this block. This section is for operators editing `config.yaml` directly, onboarded via the [source build](INSTALL.md), or adjusting cadences after first install.

| Field | Type | Description |
|---|---|---|
| `defaults` | `map<audit-slug, Cadence>` | Global default cadence per audit type. Audit slugs must match a registered type (currently `architecture_brightline`, `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`); typos fail at config load with a list of known slugs. Operators can re-prompt these cadences via `autocoder install --reconfigure audits` as an alternative to editing YAML directly — see [docs/CLI.md](CLI.md) for the full flag reference. |
| `settings` | `map<audit-slug, AuditSettings>` | Per-audit knobs. See below. |
| `max_validation_retries` | `u32` (default `1`, max `5`) | Number of retry attempts after an LLM-driven audit's generated proposal fails `openspec validate --strict`. Each retry re-invokes the audit's LLM with the validation error appended to the prompt. `0` disables retries (first failure → `ValidationExhausted`, discard, chatops `❌` notification). Values above `5` are clamped at load with a WARN log. See [TROUBLESHOOTING.md](TROUBLESHOOTING.md#audit-produces-invalid-proposal--what-to-do) for the operator-side workflow. |
| `max_audits_per_iteration` | `usize` (default `1`, max `<count of registered audits>`) | Per-iteration cap on how many audits run before the scheduler returns control to the iteration loop. The default `1` keeps audit work staggered across polling iterations even when many audits are eligible at once (typical: after a HEAD change unblocks every `requires_head_change` audit); raise to e.g. `3` for faster drainage during onboarding at the cost of longer per-iteration wall-clock. On-demand queued runs (`@<bot> audit <name>`) count against the same bound. Values above the number of registered audits clamp at the registry count with a startup WARN. Value `0` is permitted and disables audits behaviourally (every iteration skips the audit phase). See [Periodic audits → Per-iteration audit bound](OPERATIONS.md#periodic-audits) for the full discussion. |

Per-repo override: each entry under `repositories[]` accepts an `audits:` field that maps audit slugs to cadences. Per-repo entries take precedence over `audits.defaults`; an absent entry in both locations resolves to `disabled`.

**`Cadence` syntax (string):** `disabled` | `daily` | `every-N-days` (N must be a positive integer; `every-0-days` and negative N are rejected at load time) | `weekly` | `monthly` | `quarterly`.

**`AuditSettings` fields:**

| Field | Type | Description |
|---|---|---|
| `prompt_path` | `path` (optional) | Override the audit's embedded LLM prompt template. Used by `drift_audit` (embedded default: `prompts/drift-audit.md`), `missing_tests_audit` (embedded default: `prompts/missing-tests-audit.md`), `security_bug_audit` (embedded default: `prompts/security-bug-audit.md`), `architecture_consultative` (embedded default: `prompts/architecture-consultative.md`), and `documentation_audit` (embedded default: `prompts/documentation-audit.md`). An empty file is rejected at audit invocation so the daemon does not feed an empty prompt to the wrapped CLI. |
| `notify_on_clean` | `bool` (default `false`) | When `true`, an empty-findings `Reported` outcome posts `✅ <repo>: <audit_type> — no findings` to chatops. When `false`, silence is success. |
| `extra` | `map<string, yaml>` | Free-form per-audit knobs. `architecture_brightline` reads `file_lines_threshold` (default `800`) from here. `missing_tests_audit` and `security_bug_audit` each read `max_proposals_per_run` (`u32`, default `2`) from here. `documentation_audit` reads two knobs: `readme_max_lines` (`usize`, default `200`) and `page_max_lines_without_toc` (`usize`, default `500`). `drift_audit` and `architecture_consultative` do not currently read any `extra` knobs; configure them via the top-level `prompt_path` and `notify_on_clean` fields under `audits.settings.<slug>`. |

**`documentation_audit` `extra` knobs.** The `documentation_audit` exposes two organizational thresholds via `audits.settings.documentation_audit.extra`:

- **`readme_max_lines`** (`usize`, default `200`) — the body-line threshold at which the audit emits a "README too long" organization finding. The LLM applies this number when assessing `README.md`'s structure; raising it suppresses the finding for repos whose README has organic length (an explicit TOC, a clearly sectioned layout that does not need TOC navigation, etc.). Larger projects with extensive front-page docs typically raise this to `300`–`500`; smaller projects keep the default.
- **`page_max_lines_without_toc`** (`usize`, default `500`) — the total-line threshold at which a docs page is expected to carry a top-of-file TOC or summary table. The LLM applies this number when assessing `docs/*.md` files; raising it suppresses the finding for repos whose docs pages run long by design (operator manuals, reference pages). Lower it for tighter operator-self-service expectations.

Both knobs are thresholds the LLM applies when emitting `category: organization` findings. Operators in larger projects raise them; operators in smaller projects keep defaults. The audit's prompt receives the knob values verbatim as part of its input and respects them when deciding whether to flag a page.

---

## Prompt overrides

Every embedded prompt the daemon ships is loaded through a uniform
`PromptLoader` that walks four precedence levels for each call:

1. **Per-workspace nested override** — the modernized form, e.g.
   `executor.implementer.prompt_path` OR
   `audits.settings.drift_audit.prompt_path`. Paths are workspace-
   relative when not absolute.
2. **Per-workspace flat-legacy override** — the older suffix form,
   e.g. `executor.implementer_prompt_path` OR
   `reviewer.prompt_template_path`. Still accepted indefinitely for
   backward compatibility.
3. **Daemon-level flat-legacy override** — the same field treated as
   the daemon-wide value when no per-workspace config layer overrides
   it. In the current code there is one config file per daemon, so
   levels 2 and 3 collapse to the same field; the loader still walks
   both tiers so future per-workspace config layering can plug in
   without changing call sites.
4. **Embedded default** — the template compiled into the binary via
   `include_str!("../../prompts/<name>.md")`.

A configured override path that does NOT exist (OR points at an
empty file) produces a **one-shot WARN** naming the
`(PromptId, path)` pair on the first attempted load AND falls
through to the next precedence level. Repeated loads of the same
`(PromptId, path)` stay silent so reloads do NOT spam the log.

**Prompt registry.** The table below lists every embedded prompt
the daemon ships, its logical id, its embedded path, its per-
workspace override field, AND the legacy daemon-level field where
one exists. `—` (em-dash) marks prompts that had no operator
override at all before the uniform loader landed.

| Logical id                       | Embedded path                              | Per-workspace override field                                | Legacy daemon-level field                                  |
|----------------------------------|--------------------------------------------|--------------------------------------------------------------|------------------------------------------------------------|
| `Implementer`                    | `prompts/implementer.md`                   | `executor.implementer.prompt_path`                           | `executor.implementer_prompt_path`                         |
| `ImplementerRevision`            | `prompts/implementer-revision.md`          | `executor.implementer_revision.prompt_path`                  | —                                                          |
| `ChangelogStylist`               | `prompts/changelog-stylist.md`             | `executor.changelog_stylist.prompt_path`                     | `executor.changelog_stylist_prompt_path`                   |
| `CodeReview`                     | `prompts/code-review-default.md`           | `reviewer.code_review.prompt_path`                           | `reviewer.prompt_template_path`                            |
| `AuditTriage`                    | `prompts/audit-triage.md`                  | `executor.audit_triage.prompt_path`                          | —                                                          |
| `ChatRequestTriage`              | `prompts/chat-request-triage.md`           | `executor.chat_request_triage.prompt_path`                   | —                                                          |
| `AuditArchitectureConsultative`  | `prompts/architecture-consultative.md`     | `audits.settings.architecture_consultative.prompt_path`      | —                                                          |
| `AuditDrift`                     | `prompts/drift-audit.md`                   | `audits.settings.drift_audit.prompt_path`                    | —                                                          |
| `AuditMissingTests`              | `prompts/missing-tests-audit.md`           | `audits.settings.missing_tests_audit.prompt_path`            | —                                                          |
| `AuditSecurityBug`               | `prompts/security-bug-audit.md`            | `audits.settings.security_bug_audit.prompt_path`             | —                                                          |
| `AuditDocumentation`             | `prompts/documentation-audit.md`           | `audits.settings.documentation_audit.prompt_path`            | —                                                          |
| `BrownfieldDraft`                | `prompts/brownfield-draft.md`              | `features.brownfield.prompt_path`                            | —                                                          |
| `BrownfieldSurvey`               | `prompts/brownfield-survey.md`             | `features.brownfield_survey.prompt_path`                     | —                                                          |
| `Scout`                          | `prompts/scout.md`                         | `features.scout.prompt_path`                                 | —                                                          |
| `ChangeContradictionCheck`       | `prompts/change-contradiction-check.md`    | `executor.change_internal_contradiction_check_prompt_path`   | —                                                          |

**Naming convention going forward.** Any new embedded prompt added
in a future change SHALL declare its override field using the
nested `<area>.<thing>.prompt_path` form (matching the rows above).
Flat-suffix forms (`<area>.<thing>_prompt_path`) remain accepted
only for the legacy fields documented in this registry; new
prompts MUST NOT introduce additional flat-suffix overrides.

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

## `executor.max_auto_revisions_per_pr`

Maximum number of **automatic** revision rounds applied to a single open
PR before further automatic triggers are silently ignored. Only revisions
the code-reviewer auto-revise path posts — the comments carrying the
`<!-- reviewer-revision -->` marker — count against this cap. Human
`@<bot> revise <text>` requests are **not** counted against this cap; they
are bounded separately by
[`executor.max_revise_triggers_per_pr`](#executormax_revise_triggers_per_pr).
Default `5`. A value of `0` disables the PR-comment revision channel
entirely (the dispatcher becomes a no-op).

> **Renamed (was `executor.max_revisions_per_pr`).** The legacy key is
> still accepted as a silent serde alias, so existing config files load
> unchanged — it now bounds automatic revisions specifically.

Values above `20` are clamped to `20` at startup with a WARN log line —
the ceiling exists so a runaway reviewer-driven chain
(`max_auto_revisions_per_pr: 1000`) cannot let one PR burn tokens forever.

```yaml
executor:
  kind: claude_cli
  max_auto_revisions_per_pr: 5    # default; set to 0 to disable, max 20
  # legacy alias still accepted:
  # max_revisions_per_pr: 5
```

See [OPERATIONS.md](OPERATIONS.md#revising-an-open-pr-via-comment) for the
full revision-loop flow. The cap is per PR (not per repository); each PR
tracks its own automatic-revision count under
`<state_dir>/revisions/<repo-sanitized>/<pr-number>.json`. When a PR is closed
or merged, its state file is pruned automatically — the cap resets if the
PR is re-opened.

## `executor.max_revise_triggers_per_pr`

Per-PR cap on **human-initiated** `@<bot> revise` triggers the daemon acts
on (a000). This closes the previously-uncapped human-revise path and
complements [`executor.max_auto_revisions_per_pr`](#executormax_auto_revisions_per_pr)
(reviewer-initiated revisions) and `reviewer.max_code_reviews_per_pr`
(re-reviews) — all three caps are independent. Past this many **authorized**
human revisions on one PR, further `@<bot> revise` triggers are declined
with exactly one notice and **do not** invoke the executor. Default `10`.

The human-revise count is tracked in the per-PR state file (distinct from
the automatic-revision and re-review counters); the cap itself is read live
from config, so a `reload` applies to subsequent triggers. Only triggers
from an **authorized** commenter (see
[`github.command_authorization`](#githubcommand_authorization)) count.

```yaml
executor:
  kind: claude_cli
  max_revise_triggers_per_pr: 10   # default
```

## `paths:` (optional)

Operator-visible overrides for the four daemon data directories. Each
field is optional; absent fields fall through to the default
resolution chain (`AUTOCODER_*_DIR` env var → systemd-set
`$STATE_DIRECTORY` family → XDG defaults under `$HOME` → hard
fallback to `/var/lib/autocoder` etc.).

| Field         | Description                                                                                       |
|---------------|---------------------------------------------------------------------------------------------------|
| `state_dir`   | Persistent state: audit cadence, failure counters, revision state, alert throttles.               |
| `cache_dir`   | Re-creatable but kept: repo workspaces under `<cache_dir>/workspaces/<sanitized>/`.               |
| `logs_dir`    | Per-change run logs (`<logs_dir>/runs/<repo>/<change>.log`) and audit logs.                       |
| `runtime_dir` | Control socket and transient pid/lock files; cleared on reboot.                                   |

Each value must be absolute. No two fields may resolve to the same
directory — a collision is a startup error.

```yaml
paths:
  state_dir: /var/lib/autocoder
  cache_dir: /var/cache/autocoder
  logs_dir: /var/log/autocoder
  runtime_dir: /run/autocoder
```

See [`STATE-LAYOUT.md`](STATE-LAYOUT.md) for the full directory
layout, the resolution precedence, and the migration behaviour on
first startup after upgrade.

## `features:` (optional)

Per-workspace feature flags. Each sub-block is opt-in; absent sub-blocks
take their type-default behaviour. Invalid value types (non-bool where
a bool is expected, non-string where a string is expected) cause
config-load to fail-fast with an error naming the offending field.

### `features.brownfield` {#featuresbrownfield}

Config for the `brownfield` chatops verb (a23). The verb drafts an
initial canonical spec for one named capability that already exists in
the repository. See [`CHATOPS.md → brownfield`](CHATOPS.md#drafting-a-spec-for-existing-behavior-brownfield)
for the verb syntax, refusal cases, AND lifecycle-thread behavior, AND
[`OPERATIONS.md → onboarding existing projects`](OPERATIONS.md#onboarding-existing-projects)
for the recommended cadence.

```yaml
features:
  brownfield:
    enabled: true             # default true; set false to refuse the verb at parse time
    prompt_path: null         # default null; relative path to a custom brownfield-draft prompt
```

| Field          | Type             | Default | Description                                                                                                                                                                                                                                                                                                                       |
|----------------|------------------|---------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`      | `bool`           | `true`  | Per-workspace enable flag. When `false`, the dispatcher refuses `@<bot> brownfield ...` at parse time with `✗ brownfield: disabled in this workspace's config (features.brownfield.enabled=false).`. No state file is written.                                                                                                       |
| `prompt_path`  | `Option<String>` | `None`  | Workspace-relative path to a custom brownfield-draft prompt template. When set AND the file exists at run time, the polling handler uses it instead of the embedded `prompts/brownfield-draft.md` template. When set BUT the file is missing OR unreadable, the handler logs a WARN naming the path AND falls back to the embedded default. |

**Default behaviour.** Omitting the `features.brownfield` block (or omitting the entire `features:` parent block) is equivalent to `enabled: true` AND `prompt_path: null`. The verb works out of the box on a fresh install.

**Forward-compatibility note.** The per-workspace prompt override knob's location is provisional. When the broader per-workspace-prompt schema lands (covering implementer, audit-triage, changelog-stylist, brownfield-draft, etc. under a unified shape), brownfield's override SHALL conform to it; the current `features.brownfield.prompt_path` MAY be relocated at that time. Operators using the override should expect a migration step in the corresponding release notes.

### `features.scout` {#featuresscout}

Config for the `scout` chatops verb (a25). The verb queues an on-demand survey of the workspace AND produces a curated triage list of opportunities for the operator to consider. See [CHATOPS.md → scout](CHATOPS.md#scout) for the verb syntax, refusal cases, AND lifecycle-thread behavior, AND [OPERATIONS.md → Finding things to work on](OPERATIONS.md#finding-things-to-work-on) for the recommended scout → pick → spec-it cadence.

```yaml
features:
  scout:
    enabled: true                 # default true; set false to refuse scout/spec-it/clear-scout at parse time
    prompt_path: null             # default null; relative path to a custom scout prompt
    max_items: 30                 # default 30; valid range 1..=50
    include_issues: true          # default true; controls whether scout attempts `gh api` for open issues
    staleness_warn_days: 7        # default 7; threshold for the spec-it staleness warning
```

| Field                | Type             | Default | Description                                                                                                                                                                                                                                                                                                                       |
|----------------------|------------------|---------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`            | `bool`           | `true`  | Per-workspace enable flag. When `false`, the dispatcher refuses `@<bot> scout ...`, `@<bot> spec-it ...`, AND `@<bot> clear-scout ...` at parse time with `✗ scout: disabled in this workspace's config (features.scout.enabled=false).` (or the analogous spec-it/clear-scout text). No state file is written.                       |
| `prompt_path`        | `Option<String>` | `None`  | Workspace-relative path to a custom scout prompt template. Resolved via the uniform [Prompt overrides](#prompt-overrides) table — see the `Scout` row.                                                                                                                                                                              |
| `max_items`          | `usize`          | `30`    | Maximum number of opportunity items the scout-mode executor may return. **Valid range: `1..=50`**. Values outside this range cause config-load to fail-fast with an error naming `features.scout.max_items` AND the valid range.                                                                                                     |
| `include_issues`     | `bool`           | `true`  | When `true`, the scout handler attempts `gh api repos/<owner>/<repo>/issues?state=open --paginate` AND interpolates the result into the prompt. On `gh` failure, a WARN logs AND scout proceeds with code-derived items only. When `false`, the call is skipped entirely (use for repos where issues are noise).                       |
| `staleness_warn_days`| `u64`            | `7`     | When `spec-it` is invoked against a scout run whose `completed_at` is older than this many days OR whose `head_sha_at_run` differs from the workspace's current HEAD, the polling handler posts a one-time warning naming the gap BEFORE submitting the propose-request. The warning does NOT block — staleness is operator judgment. |

**Default behaviour.** Omitting the `features.scout` block (or omitting the entire `features:` parent block) is equivalent to all five defaults above. The verb works out of the box on a fresh install.

**Prompt override.** See the [Prompt overrides](#prompt-overrides) table for the `Scout` entry — `features.scout.prompt_path` is workspace-relative AND falls back to the embedded `prompts/scout.md` template when the configured file is missing or empty.

### `features.brownfield_survey` {#featuresbrownfield_survey}

Config for the `brownfield-survey` chatops verb (a29). The verb surveys an existing codebase AND returns a curated list of proposed capabilities the operator can batch-generate via `send it`. See [CHATOPS.md → brownfield-survey](CHATOPS.md#brownfield-survey) for the verb syntax, refusal cases, AND lifecycle-thread behavior, AND [OPERATIONS.md → Bootstrapping specs for an existing project](OPERATIONS.md#bootstrapping-specs-for-an-existing-project) for the recommended survey → review → batch cadence.

```yaml
features:
  brownfield_survey:
    enabled: true                 # default true; set false to refuse the verb at parse time
    prompt_path: null             # default null; relative path to a custom survey prompt
    max_capabilities: 20          # default 20; valid range 1..=50
```

| Field              | Type             | Default | Description                                                                                                                                                                                                                                                                                                            |
|--------------------|------------------|---------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`          | `bool`           | `true`  | Per-workspace enable flag. When `false`, the dispatcher refuses `@<bot> brownfield-survey ...`, `@<bot> clear-survey ...`, AND `@<bot> send it` in survey threads at parse time with `✗ brownfield-survey: disabled in this workspace's config (features.brownfield_survey.enabled=false).` (or the analogous text). |
| `prompt_path`      | `Option<String>` | `None`  | Workspace-relative path to a custom survey prompt template. Resolved via the uniform [Prompt overrides](#prompt-overrides) table — see the `BrownfieldSurvey` row.                                                                                                                                                       |
| `max_capabilities` | `usize`          | `20`    | Maximum number of proposed-capability items the survey-mode executor may return. **Valid range: `1..=50`**. Values outside this range cause config-load to fail-fast with an error naming `features.brownfield_survey.max_capabilities` AND the valid range.                                                              |

**Default behaviour.** Omitting the `features.brownfield_survey` block (or omitting the entire `features:` parent block) is equivalent to all three defaults above. The verb works out of the box on a fresh install.

**Prompt override.** See the [Prompt overrides](#prompt-overrides) table for the `BrownfieldSurvey` entry — `features.brownfield_survey.prompt_path` is workspace-relative AND falls back to the embedded `prompts/brownfield-survey.md` template when the configured file is missing or empty.


## `canonical_rag:` (optional)

Retrieval-augmented context for the implementer (a21). When the block
is absent OR present with `enabled: false`, no embedding pipeline runs
and the implementer's `query_canonical_specs` MCP tool returns an empty
array with `error_hint: "rag disabled in config"`. When enabled, the
daemon embeds every `openspec/specs/<capability>/spec.md` at workspace
init AND re-embeds affected capabilities after archives that touch
canonical specs.

| Field                | Type                  | Default              | Description                                                                                                                          |
|----------------------|-----------------------|----------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`            | `bool`                | `false`              | Master switch. `false` (the implicit default) makes the daemon behave as if the block were absent — useful for documenting a parked config without enabling. |
| `provider`           | `ollama \| openai_compatible` | required¹    | Embedding provider. `ollama` posts to `<base>/api/embed`; `openai_compatible` posts to `<base>/embeddings` with `Authorization: Bearer <api_key>`. **Omit** to interpret `model` as a [`models:` nickname](#models-optional); a nickname resolving to `anthropic` is rejected here (no embeddings API). |
| `model`              | `string`              | required             | Provider-specific embedding model identifier (e.g. `nomic-embed-text`, `qwen3-embedding:4b`, `voyage-2`), **or** a `models:` nickname when `provider` is omitted.                            |
| `api_base_url`       | `string`              | required             | For `ollama`: `http://host:11434` (the `/api` prefix is implicit). For `openai_compatible`: include the `/v1` prefix.                |
| `api_key_env`        | `string?`             | `None`               | Env-var name carrying the API key. Required for `openai_compatible`. Ignored for `ollama` unless the endpoint requires auth.         |
| `api_key`            | `{ value: string }?`  | `None`               | Inline alternative to `api_key_env`. Mutually exclusive with `api_key_env`; inline wins with a WARN if both set (same as `reviewer:`). |
| `top_k`              | `usize`               | `10`                 | Default chunk count when the tool caller omits `top_k`. Clamped to `[1, 100]` with WARN at startup.                                  |
| `chunk_strategy`     | `per_requirement \| per_scenario \| per_capability` | `per_requirement` | Chunk granularity. `per_requirement` (default) is one chunk per `### Requirement:`. The other two are reserved for future variants. |
| `reembed_on_archive` | `bool`                | `true`               | When `true`, post-archive iterations whose archive touched a canonical spec re-embed the affected capability. When `false`, the store goes stale until daemon restart. |

¹ `provider` is required **unless** the block references a [`models:`
nickname](#models-optional) (omit `provider` and set `model` to the
nickname); `api_base_url` and `api_key`/`api_key_env` then come from the
registry entry.

```yaml
canonical_rag:
  enabled: true
  provider: ollama
  model: nomic-embed-text
  api_base_url: http://localhost:11434
  top_k: 10
  chunk_strategy: per_requirement
  reembed_on_archive: true
```

See [OPERATIONS.md → Canonical-spec RAG](OPERATIONS.md#canonical-spec-rag)
for the operational discussion (re-embed cadence, in-memory persistence,
failure modes, cost expectations) AND
[DEPLOYMENT.md → Self-hosted Ollama for RAG](DEPLOYMENT.md#self-hosted-ollama-for-rag)
for the docker-compose quick-start AND the remote-Ollama options.

**Override prompt note.** Operators using
`executor.implementer_prompt_path` to ship a custom template can either
mention `query_canonical_specs` in their override OR ignore the new
tool entirely. The tool stays registered in the MCP child regardless;
omitting the mention just means the agent won't be guided to call it.
