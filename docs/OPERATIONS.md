# Operating Notes

## Workspace path derivation

If a repository entry omits `local_path`, the workspace path is derived deterministically from the URL:

1. Strip the protocol prefix (`git@`, `ssh://`, `https://`, `http://`).
2. Strip a trailing `.git`.
3. Replace any character that is not ASCII alphanumeric, `_`, or `-` with `_`.
4. Prepend `/tmp/workspaces/`.

`git@github.com:owner/repo.git` and `https://github.com/owner/repo.git` both map to `/tmp/workspaces/github_com_owner_repo`. At startup, autocoder runs a collision check: if two configured repositories resolve to the same workspace path (whether by derivation or by explicit `local_path`), the process exits non-zero before spawning any polling tasks. Set `local_path` explicitly to disambiguate.

## Multi-repo setup

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

## Polling cadence and your firewall

When autocoder spawns ≥5 polling tasks at process start, the simultaneous `git fetch` operations from a single source IP can look like a port scan or scraper to network IDS — one operator reported their IDS killing SSH connections the moment the daemon tried to poll 8–9 repos at once. Even without an IDS, tasks that all share the same `poll_interval_sec` (e.g. the default `300`) drift only marginally across iterations because `git fetch` dominates each iteration's wall-clock, so they tend to re-cluster over time.

Two defaults defuse this:

- `executor.startup_jitter_max_secs` (default `30`) — each task waits a uniformly-random `[0, 30]` seconds before its first iteration, smearing the first round of fetches across a 30 s window.
- `executor.inter_iteration_jitter_pct` (default `10`) — each inter-iteration sleep is `poll_interval_sec ± 10%`, so tasks that briefly synchronize drift apart again on the next cycle.

Both jitters cost almost nothing in wall-clock and respect SIGTERM/SIGINT (cancellation is observed within 200 ms during either sleep). Operators on isolated networks who prefer deterministic timing can set both to `0`. Operators who want a wider window — say, after seeing IDS alerts even with the defaults — can raise `startup_jitter_max_secs` to something like `120` or `300`.

## Queue order

Pending changes are processed in ascending entry-name order (UTF-8 byte order, which is alphabetical for ASCII names). Operators with stacked dependencies — i.e. change N+1 depends on change N — encode order explicitly by prefixing change names with a letter+number tag: `a01-rename-foo`, `a02-extract-bar`, `a03-wire-baz`. The prefix is the operator's contract for "this change depends on the previous in sequence." For a second unrelated stack, use a different letter group (`b01-`, `b02-`). For unrelated single changes, no prefix is needed; alphabetical order is arbitrary but deterministic.

Note: OpenSpec rejects change names that start with a digit. Plain `01-`/`02-` prefixes will fail at the prompt-building step (`openspec instructions apply --change <name>` returns "Invalid change name"). Always start with a letter.

Each iteration commits at most `max_changes_per_pr` archived changes (default `3`); any remaining pending changes wait for the next iteration. The cap is configurable per repository, or globally via `executor.max_changes_per_pr`. A long queue therefore ships as several reviewable PRs over time rather than one large PR.

A change that fails (or escalates to chatops) halts the queue walk for that iteration; remaining pending changes wait for the next iteration. This preserves the stacked-dependency assumption behind authoring-order processing: change N+1 may depend on change N having succeeded, so the bot does not attempt N+1 while N is unfixed. A persistently-failing change accumulates failure-counter increments and hits perma-stuck (default after 2 consecutive failures), at which point it drops out of `list_pending` and the queue resumes at N+1.

## Startup preflight

At startup, `autocoder run` invokes `openspec --version` once. If the binary is not on the daemon's PATH or exits non-zero, the daemon exits non-zero before any polling task is spawned. The stderr message names the failure (binary not found, non-zero exit code, etc.). This means a misconfigured deployment surfaces at startup rather than producing empty iterations.

If you see `openspec preflight failed: binary not found on PATH`, add the install directory to the systemd unit's `Environment="PATH=..."` line (see [Deployment](DEPLOYMENT.md)).

## Busy marker

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

## Workspace directory deleted

If a workspace directory under `/tmp/workspaces/` is removed while autocoder is running (or while stopped), the daemon's next iteration treats this as a fresh-clone case: it clones upstream into the path again. In fork-PR mode it also fetches ONLY the configured agent branch from the `fork` remote at that time (via `git fetch fork +refs/heads/<agent_branch>:refs/remotes/fork/<agent_branch>`) so the local `refs/remotes/fork/<agent_branch>` tracking ref reflects the fork's actual state. Without that fetch the next `git push --force-with-lease fork <agent_branch>` would compare an empty local tracking value against the fork's existing commits and reject with `! [rejected] <agent_branch> -> <agent_branch> (stale info)`, leaving the daemon stuck. The fetch deliberately restricts itself to one branch: a wholesale `git fetch fork` would populate `refs/remotes/fork/<every-branch>`, and if any fork branch shadows an upstream name (e.g. both `origin/dev` and `fork/dev` exist), the next `git checkout <base_branch>` would fail with `fatal: 'dev' matched multiple (2) remote tracking branches`. The post-clone fork fetch is best-effort: if it fails (network blip, fork doesn't yet exist, agent branch doesn't yet exist on the fork), the daemon proceeds and the next push will surface any real divergence via the existing branch-push-failure alert.

## Fork recreation on workspace reinitialization

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

## Rebuilding canonical specs from archive history

`openspec/specs/<capability>/spec.md` is rebuilt by the host's openspec install whenever an archived change has the `openspec sync` workflow enabled at archive time. When a repository was archived from a host without that workflow (or before that workflow existed), the canonical specs drift from what the archive history actually says. Symptoms: the archive contains 30 `## ADDED Requirements` blocks, but the canonical spec is missing 25 of them.

autocoder ships a full rebuild path for that case. Incremental backfill is intentionally unsupported — when drift is mid-history (an earlier change was never synced but later changes were), re-applying the skipped change onto the current canonical produces an incorrect end state. Full rebuild from scratch is the only safe answer.

**When to use rebuild.** When you onboard a repo that was archive-driven from a host without `openspec sync`, when `git diff openspec/specs/` after a successful archive shows nothing despite the change adding requirements, or when `openspec list` and the on-disk canonical specs disagree on capability content.

**CLI invocation** (against a local clone — no daemon required):

```bash
autocoder sync-specs --rebuild --workspace /path/to/repo
```

This iterates every archived change in chronological order, replays it via `openspec archive`, and preserves each archive's original date prefix via in-place rename. The CLI prints a summary listing successful and failed changes plus a modified-vs-unchanged tally for every canonical spec file. Exit code is non-zero if any archive failed to re-archive.

**Chatops invocation** (for daemon-managed repos):

```
@<bot> rebuild-specs <repo-substring>
```

This submits a `RebuildSpecs` action to the control socket, which sets a `pending_rebuild` flag on the named repo's polling task. The next iteration runs the rebuild instead of the normal queue walk. The rebuild's commits land on the agent branch via the existing push + PR flow; the PR title is `spec rebuild: <N> capability(ies) rebuilt from archive history` so operators can recognize it at a glance.

When the rebuild iteration finishes, the bot posts one of three chatops messages:

- `✓ rebuild complete for <repo>: PR <url> opened — <N> capability(ies) updated from <M> archived change(s)` (success with drift)
- `✓ rebuild complete for <repo>: no drift detected, canonical specs already in sync` (success no drift)
- `⚠️ rebuild for <repo> completed with <N> failure(s); ...` (partial failure)

The completion notification fires regardless of `chatops.notifications.pr_opened` or `failure_alerts` — it is the operator's direct response to a command they issued, so they always get the completion signal.

**The `--immediate` flag** (CLI only — never exposed via chatops):

```bash
autocoder sync-specs --rebuild --immediate --workspace /path/to/repo
```

Without `--immediate`, the CLI waits politely for the current iteration to release the busy marker before starting. With `--immediate`, the CLI sends `SIGTERM` to the executor subprocess (via the busy marker's recorded PID), waits up to 30 seconds for cleanup, and runs the rebuild even if the iteration was mid-flight. The cancelled iteration's partial workspace state is cleaned up by the rebuild's first dirty-workspace recovery pass.

Chatops deliberately does NOT support `--immediate`: killing a running executor mid-iteration is a foot-loaded gun that should require SSH access. Operators wanting `--immediate` SSH to the daemon host and run the CLI.

**What rebuild discards** — a caveat. The rebuild is "what would canonical look like if every archive had synced correctly the first time." It does NOT preserve:

- `## Purpose` paragraphs hand-edited into canonical specs without an archived change introducing them. New capability spec files openspec creates from scratch get a placeholder Purpose (`TBD - created by archiving change <X>. Update Purpose after archive.`); operators replace those manually after the rebuild PR merges.
- `### Requirement:` entries hand-added to canonical without an archive source. Anything not in the archive history is gone after rebuild.

Review the rebuild PR's diff before merging; treat it like any other autocoder PR.

## Perma-stuck change detection

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

See also [Spec marked as needing revision](#spec-marked-as-needing-revision) — its sibling pattern for the case where the operator (not the agent) is the one with work to do.

## Spec marked as needing revision

Sibling pattern to [Perma-stuck change detection](OPERATIONS.md#perma-stuck-change-detection). Where perma-stuck signals "the agent kept failing on this change," needs-spec-revision signals "the spec is asking the agent to do something it cannot do." Both are operator-action states; both are cleared by deleting the marker file.

**What triggers it.** Before doing any work, the agent scans `tasks.md` for tasks that require capabilities outside its sandbox: `sudo` on a real host, missing CLI tools, real GitHub tag pushes, browser interactions, VM/container spin-up, smoke tests on specific hardware or OS versions, manual external observation. If any task matches, the agent emits an `=== AUTOCODER-OUTCOME ===` block flagging the unimplementable tasks and exits without modifying the workspace. autocoder writes `<workspace>/openspec/changes/<change>/.needs-spec-revision.json`, posts a chatops alert under `AlertCategory::SpecNeedsRevision` (same 24-hour throttle as perma-stuck), and halts the queue walk for the iteration.

The agent does NOT auto-edit `tasks.md`. The flag-and-stop contract preserves the project invariant that no AI process edits its own marching orders without human review.

**The marker file** at `<workspace>/openspec/changes/<change>/.needs-spec-revision.json` has the schema:

```json
{
  "change": "<change-name>",
  "marked_at": "RFC 3339 UTC timestamp",
  "unimplementable_tasks": [
    {"task_id": "5.2", "task_text": "...", "reason": "..."}
  ],
  "revision_suggestion": "free-form text the agent wrote describing what to change",
  "operator_action": "Edit openspec/changes/<change>/tasks.md to remove or revise the flagged tasks, commit + push, then delete this marker file."
}
```

The marker is registered in `.git/info/exclude` at workspace init so it does not trip the pre-pass dirty check and survives `git clean -fd` during per-iteration recovery (same treatment as `.perma-stuck.json`).

**The chatops alert** lists each flagged task's id + text, the agent's revision suggestion, an operator-action checklist, and the marker file path + the per-change run log path. It is gated on `failure_alerts_enabled` and subject to the standard 24-hour per-category throttle.

**Operator workflow.**

1. Read the chatops alert. The flagged tasks and the agent's revision suggestion are in the body; the run log is named for deeper diagnosis if needed.
2. Edit `openspec/changes/<change>/tasks.md` to remove or revise the flagged tasks. Commit + push to the base branch.
3. Delete the marker file: `rm openspec/changes/<change>/.needs-spec-revision.json`. The next iteration picks the change back up.

**False-positive escape hatch.** If you review the flagged tasks and decide the agent was overly conservative, delete the marker WITHOUT editing `tasks.md`. The change re-enters `list_pending` on the next iteration. If the agent flags the same task again, you can add a comment in `tasks.md` near it explaining why it's implementable (e.g. naming a tool path or workflow that resolves the concern), or update the implementer prompt template via a follow-up change to relax the relevant pattern.

The marker is operator-cleared, not auto-cleared. autocoder does not remove it on the next iteration even when the spec has been revised — same rationale as the perma-stuck marker: the operator's audit trail is clearer when "did the issue actually get fixed?" requires an explicit human action.

## Self-heal for already-implemented changes

When a rebase or merge lands the work for a change on the base branch without moving the change directory into `archive/`, the agent sees the implementation already done and returns `Completed` without modifying the workspace. Normally that's classified as Failed (no-op completion) and retried on every poll, burning tokens to re-confirm the same answer. autocoder self-heals this case instead:

When the executor returns `Completed`, `git status --porcelain` is empty, `openspec validate <change> --strict` exits 0, AND every checkbox in `openspec/changes/<change>/tasks.md` is `[x]`, autocoder runs the archive move itself, commits it with subject `archive: <change>: implementation already in base`, and ships a PR through the normal push + PR flow.

If any of the four preconditions fails — including `openspec validate` erroring or any task still `[ ]` — autocoder falls through to the existing Failed path, so non-self-heal cases retain their prior behavior.

The PR body for a pass that self-healed one or more changes is prefixed with:

> _This PR archives one or more changes whose implementation was already present on the base branch. No code diff is included; only the openspec archive move._

The disclaimer identifies these passes for reviewers regardless of whether the pass also includes normally-implemented changes.

## Skipping iterations while a PR is open

Before each polling iteration begins its work, autocoder queries GitHub for open PRs whose `head` matches the configured agent branch (`<fork_owner>:<agent_branch>` in fork-PR mode, `<repo_owner>:<agent_branch>` in direct mode, base = the configured base branch). If an open PR is found, the iteration is skipped: no executor invocation, no commits, no push, no PR creation attempt. The skip persists until the open PR is closed or merged. This prevents the daemon from re-implementing the same changes on every poll while a PR sits awaiting review, which would otherwise force-push new commits over the PR's branch and burn agent tokens redundantly.

To re-implement after rejecting a PR: close it (don't merge). The next poll proceeds. To accept the implementation: merge it; the archive moves land on the base branch and the changes drop out of `list_pending`.

If the GitHub query itself fails (transport error, non-2xx), the iteration proceeds as if no PR existed — better to incur a redundant Claude run than to halt the repo on a flaky API. The failure is logged at WARN.

## Periodic audits

Beyond the OpenSpec change queue, autocoder runs a periodic-audit framework: a set of registered audits that fire on per-audit cadences, write per-invocation logs, and (depending on the audit) post chatops findings or write new OpenSpec changes that feed back into the queue.

The framework is **default-off**. With no `audits:` block in the config, every registered audit's effective cadence resolves to `disabled` and the daemon behaves exactly as it did before the framework existed. Operators opt in explicitly per audit.

**Registered audit type names:**

| Slug | What it does | LLM | Default cadence | WritePolicy |
|---|---|---|---|---|
| `architecture_brightline` | Pure-code metrics — file size, duplicate signatures across files. Surfaces oversize files and accidental copies. | No | `disabled` (opt-in via `audits.defaults` or per-repo) | `None` (read-only) |
| `drift_audit` | Invokes the wrapped agent CLI (typically `claude`) with a read-only sandbox (`Read`, `Glob`, `Grep`, `Bash`) and a drift-detection prompt. The agent compares each requirement in `openspec/specs/<capability>/spec.md` against observable code behavior and emits structured findings. Triggers on HEAD change at the configured cadence. Purely **advisory** — never modifies code or specs; the operator decides whether each finding becomes a code-fix change, a spec-fix change, or is dismissed. | Yes | `disabled` (opt-in via `audits.defaults` or per-repo) | `None` (read-only; sandbox blocks `Write`/`Edit`, post-hoc diff check reverts any sneaky writes) |
| `missing_tests_audit` | Invokes the wrapped agent CLI with a sandbox that allows `Write` and `Edit` under `openspec/changes/` only, plus the read tools. The agent surveys the source tree, identifies uncovered error paths / branches without assertions / obvious edge cases, and creates up to `max_proposals_per_run` (default `2`) new OpenSpec change directories under `openspec/changes/tests-*` proposing tests to fill those gaps. The audit validates each new change via `openspec validate --strict`, rejects invalid ones (deletes the directory), and commits the valid ones to the agent branch as `audit: missing-tests proposals (N change(s))`. Returns `AuditOutcome::SpecsWritten(names)` so the same iteration's `walk_queue` picks the new changes up and the implementer ships them in the same PR. **Additive only:** the prompt forbids deleting or modifying existing tests (except factually broken ones). All produced changes use the `tests-` prefix so operators recognize audit-produced work at a glance. Triggers on HEAD change at the configured cadence. | Yes | `disabled` (opt-in via `audits.defaults` or per-repo) | `OpenSpecOnly` (sandbox allows `Write`/`Edit`; post-hoc diff check reverts anything outside `openspec/changes/`) |
| `security_bug_audit` | Invokes the wrapped agent CLI with the same `OpenSpecOnly` sandbox as `missing_tests_audit`, but with a security-and-bug-detection prompt. The agent surveys the source tree for high-confidence security issues (injection, auth/authz mistakes, hard-coded secrets, unsafe deserialization, missing input validation, race conditions, resource leaks) and likely bugs (off-by-one, wrong operator, mishandled None/null, missing error propagation, panicking on attacker-controlled input). For each confirmed finding it creates an OpenSpec change directory under `openspec/changes/fix-*` (bug fixes) or `openspec/changes/secure-*` (security hardening), each describing the fix the implementer should make. Up to `max_proposals_per_run` (default `2`) per invocation. The audit validates each new change via `openspec validate --strict`, rejects invalid ones, and commits the valid ones as `audit: security-bug proposals (N change(s))`. Returns `AuditOutcome::SpecsWritten(names)` so the same iteration's `walk_queue` picks the new changes up and the implementer + reviewer pipeline catches any LLM mistakes before they hit a PR. The prompt aggressively filters low-confidence findings — a false positive becomes wasted implementer work. **Operator warning:** this audit can be noisy in early iterations on an unfamiliar codebase. Monitor the first few invocations and tighten the prompt (or disable the audit) if the false-positive rate is high. Triggers on HEAD change at the configured cadence. | Yes | `disabled` (opt-in via `audits.defaults` or per-repo) | `OpenSpecOnly` (sandbox allows `Write`/`Edit`; post-hoc diff check reverts anything outside `openspec/changes/`) |
| `architecture_consultative` | Invokes the wrapped agent CLI with a read-only sandbox (`Read`, `Glob`, `Grep`, `Bash`) and a *consultative* architecture prompt. The agent surveys the codebase and emits 0-5 anchored observations phrased as questions — "Should X be its own module?", "Is the boundary between Y and Z still meaningful?" — each tied to a specific `file:line` range and a one-paragraph body of context. Purely **advisory**: the operator decides which (if any) questions are worth acting on. The prompt's anti-pattern list specifically forbids the failure modes consultative LLMs are prone to: do NOT suggest microservices, do NOT suggest a different language, do NOT suggest new infrastructure dependencies, do NOT suggest team-of-50 patterns (event sourcing, CQRS, hexagonal overlays), do NOT suggest stylistic refactorings, and do NOT suggest changes that would add more code than they remove. The prompt is language-agnostic and explicitly tolerates polyglot codebases. The audit returns `Err` if the agent emits more than 5 findings — silent truncation would obscure prompt misbehavior. **Cadence intent:** designed for `monthly` or `quarterly` cadence; daily/weekly invocations produce noise. **Operator guidance on noise:** if the audit output is too noisy, tighten the prompt (override at `audits.settings.architecture_consultative.prompt_path`) before reaching for `disabled` — the anti-pattern list exists specifically to mitigate common LLM failure modes, so if output still misfires, the prompt is where to fix it. Triggers on HEAD change at the configured cadence. | Yes | `disabled` (opt-in via `audits.defaults` or per-repo) | `None` (read-only; sandbox blocks `Write`/`Edit`, post-hoc diff check reverts any sneaky writes) |

Each audit declares a `WritePolicy`:

- **`None`** — sandbox blocks `Write`/`Edit`; after `run()` returns the framework runs `git status --porcelain -uall` and asserts the workspace is clean. Any unexpected diff is treated as failure: the state file is NOT updated (so the cadence retriggers on the next iteration), the diff is reverted via `git reset --hard HEAD` + `git clean -fd`, and a throttled chatops alert under the `audit_write_policy_violation` category is posted.
- **`OpenSpecOnly`** — sandbox allows `Write`/`Edit`; after `run()` returns every modified or new path must begin with `openspec/changes/`. A diff outside that prefix triggers the same failure handling.
- **`Approved`** — full write access. Reserved for future audits with broader scope; not used by any audit shipped today.

**Cadence configuration:**

```yaml
audits:
  defaults:
    architecture_brightline: weekly      # disabled | daily | every-N-days | weekly | monthly | quarterly
    drift_audit: weekly                  # spec/code alignment audit; HEAD-change gated
    missing_tests_audit: weekly          # propose OpenSpec changes to fill test-coverage gaps; HEAD-change gated
    security_bug_audit: weekly           # propose OpenSpec changes for confirmed security issues and bugs; HEAD-change gated
    architecture_consultative: monthly   # consultative LLM architecture read; HEAD-change gated; recommended monthly/quarterly
  settings:
    architecture_brightline:
      notify_on_clean: false             # silence is success; set true for an explicit ✅ post each clean run
      extra:
        file_lines_threshold: 800        # override the brightline default (800)
    drift_audit:
      prompt_path: null                  # path to a markdown file overriding the embedded default prompt; null → embedded prompt
      notify_on_clean: false             # true → post a brief "no findings" chatops message on clean runs
    missing_tests_audit:
      prompt_path: null                  # path overriding the embedded prompts/missing-tests-audit.md; null → embedded prompt
      notify_on_clean: false             # missing-tests is a spec-writing audit (SpecsWritten outcome is silent regardless); this only affects the rare error case
      extra:
        max_proposals_per_run: 2         # cap on the number of new openspec/changes/tests-* directories created per invocation (default 2)
    security_bug_audit:
      prompt_path: null                  # path overriding the embedded prompts/security-bug-audit.md; null → embedded prompt
      notify_on_clean: false             # security-bug is a spec-writing audit (SpecsWritten outcome is silent regardless); this only affects the rare error case
      extra:
        max_proposals_per_run: 2         # cap on the number of new openspec/changes/fix-*|secure-* directories created per invocation (default 2)
    architecture_consultative:
      prompt_path: null                  # path overriding the embedded prompts/architecture-consultative.md; null → embedded prompt. If the audit's output is noisy, tighten the prompt here before disabling the audit.
      notify_on_clean: false             # true → post a brief "no findings" chatops message when the agent emits zero questions

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

**`requires_head_change` semantics:** Audits that compute over the codebase (like `architecture_brightline`, `architecture_consultative`, `drift_audit`, `missing_tests_audit`, and `security_bug_audit`) declare `requires_head_change = true`; the scheduler skips them when the base-branch HEAD SHA matches the recorded `last_run_sha`, regardless of cadence. Audits whose inputs are external (package registries, GitHub PR lists) return `false` and run on cadence alone.

**Audit-run logs:** Every invocation (success, failure, violation) writes a timestamped log file at:

```
/tmp/autocoder/logs/<workspace-basename>/audits/<audit_type>-<UTC-RFC3339-with-Z>.log
```

The log contains: the audit type, workspace path, start/end timestamps, the resolved cadence, the last-run record (if any), the prompt (for LLM audits), the raw audit output, every finding's full body, and the final `AuditOutcome` variant. The directory is created on first use. Cleanup is operator-driven (same model as the per-change run logs).

**State file:** Per-workspace audit run state lives at `<workspace>/.audit-state.json`. The file is autocoder bookkeeping and is registered in `.git/info/exclude` at workspace init so it does not trip the pre-pass dirty check. Missing/unparseable file → "no audits have ever run" (every audit is eligible on its next due iteration). Lost state safely re-runs all audits on schedule.

**Outcome dispatch:**

- `AuditOutcome::Reported(findings)` → chatops post with header `📋 <repo>: <audit_type> — N finding(s)` and a bullet list of severity-glyphed subjects (low: `•`, medium: `⚠`, high: `🔴`). Default per-finding excerpt is 200 chars; full bodies live in the audit-run log. Empty findings vector is silent unless `notify_on_clean: true`.
- `AuditOutcome::SpecsWritten(names)` → one `🔍 <repo-url>: <audit_type> created proposal \`<change-slug>\` — <first line of ## Why>` chatops post per validated change (see [CHATOPS.md → Proposal-created audit notifications](CHATOPS.md#progress-notifications)). The notification fires AFTER `openspec validate --strict` passes for each proposal AND BEFORE the audit's `git commit` ships it, so operators see provenance for the `🚀 starting work on …` line that follows on the next polling iteration. The framework also logs an info line naming each created change. The notification is always sent (not gated by `notify_on_clean`); brightline + the advisory `Reported`-only audits never fire it.
- `AuditOutcome::NoFindings` → silent.

**Failure modes:**

- An audit returning `Err` is logged at ERROR; the state file is NOT updated for that audit; the iteration continues to the remaining audits and then to `list_pending`. Other audits in the registry still run.
- A WritePolicy violation is treated the same way (state untouched), additionally reverts the workspace and posts the throttled `audit_write_policy_violation` chatops alert.

## Recovering from a bad run

The `rewind` subcommand discards the in-flight agent branch and re-queues one or more archived changes. See [CLI Reference → rewind](CLI.md#rewind) below.

## Dirty workspace auto-recovery

If a workspace under `/tmp/workspaces/` is left dirty between polls (uncommitted edits, untracked files, or a checked-out branch other than the base), autocoder recovers automatically at the next startup or poll cycle: it checks out the configured `base_branch`, runs `git reset --hard origin/<base_branch>`, and runs `git clean -fd`. The repo then re-enters its normal polling loop. If recovery itself fails (e.g. the remote is unreachable), the repo is skipped for the daemon's lifetime and an error is logged — restart the daemon once the underlying problem is fixed.

Recovery runs at two points in the lifecycle:

1. **Startup** (`autocoder run` boot): every configured repo passes through `repo_passes_startup_check`. A dirty workspace at this point usually means a daemon restart after a previous run was killed mid-iteration. Recovery resets the workspace and the repo proceeds to normal polling; if recovery itself fails the repo is excluded for the process lifetime.
2. **Per iteration** (`run_pass_through_commits` pre-pass check): a failed executor invocation that returned `Failed` or timed out without committing leaves tracked-file modifications behind. The next iteration's pre-pass dirty check runs the same recovery before the iteration's normal flow begins. On success the iteration proceeds and no operator notification fires. Only when recovery itself errors (or the workspace is somehow still dirty after the recovery commands complete) does autocoder post the `WorkspaceDirtyMidIteration` chatops alert and return the iteration as failed.

Wholesale wiping of the workspace is safe at both points because the agent branch is rebuilt from base each iteration via `recreate_branch` — any local state the recovery destroys would have been overwritten anyway. The recovery does NOT touch the fork remote; it operates purely on the local working tree.

Operators who want to inspect a dirty workspace before any daemon action should stop the systemd unit first:

```bash
sudo systemctl stop autocoder
# inspect /tmp/workspaces/<repo>/ at your leisure
sudo systemctl start autocoder
```

## Runtime control: live config reload

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

### Adding a repository at runtime

To add a repository without restarting the daemon:

1. Edit `config.yaml` (the path the daemon was launched with) and append the new entry under `repositories:`. Set its `url`, `base_branch`, `agent_branch`, and `poll_interval_sec` as usual.
2. Run `sudo -u autocoder autocoder reload` from the same host. The CLI prints the daemon's response.
3. Verify the response includes the new URL under `repositories_delta.added` and `"repositories"` appears in `applied`. The polling task is now running; it does workspace initialization on its first pass.

The reverse (remove a repository) works the same way: delete the entry, reload, and the new URL appears under `repositories_delta.removed`. The cancelled task finishes its current iteration before exiting, so a removal during an active push or PR step completes cleanly.

### In-flight iteration safety

A repo cancelled mid-iteration finishes its in-flight pass normally. The cancellation check sits in the inter-poll `tokio::select!`, so the next poll never starts after the cancel — but the current one runs to completion. A modify-in-place is observed at the *next* iteration; the current iteration uses the old snapshot. Both rules eliminate mid-iteration tearing of `RepositoryConfig` fields.

If you remove a repo and re-add it (or change a setting) before the previous task has fully exited (e.g. it is mid-push when the reload lands), the response logs a WARN and reports the URL as unchanged for that reload. Run `autocoder reload` again after a brief wait; the second reload sees the URL as absent and re-adds it cleanly.

---

## Revising an open PR via comment

autocoder treats a PR comment of the form `@<bot> revise <free-text>` as a
revision request against the agent branch the PR was opened from. On the
next polling iteration, the daemon:

1. Fetches the comment, parses the revision text (everything after `revise`).
2. Re-invokes the executor in revision mode with the original change
   material, the current PR diff, and the operator's text.
3. On `Completed`: commits the workspace, force-pushes (`--force-with-lease`)
   to the agent branch, and posts a reply comment starting with
   `✅ Revision applied:`. The PR's diff updates in place; no PR close /
   re-open is required.
4. On `Failed`: posts `✗ Revision attempt failed: <reason>`. The PR is
   unchanged; the operator can reply with another `@<bot> revise ...` to
   retry or close the PR.
5. On `AskUser` (executor needs clarification): no commit, no reply.
   The question is escalated via the existing ChatOps channel; once the
   operator answers in that thread, the next polling iteration resumes
   the revision against the same trigger comment.

The trigger pattern is strict: the comment body's first non-whitespace
token must be `@<bot>` (case-insensitive on the username) and the next
token must be `revise` (case-insensitive). Comments like `@<bot> looks
good` are conversational and are ignored. Anyone with GitHub write access
to the repo can post a revision — the trust boundary matches the existing
ChatOps channel.

**Revision cap.** Each PR has a per-PR cap (default `5`; configurable via
`executor.max_revisions_per_pr`, hard-clamped at `20`). When the cap is
reached, the daemon posts a one-time decline comment starting with
`🛑 Revision cap reached` AND a ChatOps notification, then silently
ignores subsequent triggering comments on that PR. Close + re-open or
merge as-is to reset the cap.

**State persistence.** Per-PR state (last-seen-timestamp, revision count,
cap-decline flag) lives at `<workspace>/.autocoder/revisions/<pr-number>.json`.
Files for closed/merged PRs are pruned automatically at iteration start.

**Disabling.** Set `executor.max_revisions_per_pr: 0` to opt out of the
PR-comment revision channel entirely.

### Reviewer-initiated revisions (cross-reference)

The same revision dispatcher described above also processes
`<!-- reviewer-revision -->`-marked comments posted by the code-quality
reviewer when `reviewer.auto_revise_on_block: true`. Both flows share the
per-PR `executor.max_revisions_per_pr` cap and the same per-PR state file
(`<workspace>/.autocoder/revisions/<pr-number>.json`); a reviewer-initiated
revision applied in iteration N counts against the same budget a
subsequent human `@<bot> revise ...` would consume.

See [Reviewer-initiated revisions on Block verdicts](CODE-REVIEW.md#reviewer-initiated-revisions-on-block-verdicts)
for the full reviewer-side flow, the per-concern decision the reviewer
makes, and the operator-template migration steps for sites that have
overridden the default reviewer prompt.

---
