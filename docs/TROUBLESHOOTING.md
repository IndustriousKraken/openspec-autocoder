# Troubleshooting

Diagnostic flows for the failure modes operators most often hit. Each section is a self-contained recipe: symptom → root cause → fix.

## Rebuild fails on some changes (`autocoder sync-specs --rebuild`)

When you trigger a canonical-spec rebuild — via the CLI subcommand OR the `@<bot> rebuild-specs <repo>` chatops verb — the resulting PR may report some archived changes as failed. The new failure messaging in the PR body (introduced in `sync-specs-detect-aborted-output`) gives you the upstream cause directly. A typical entry:

```
- `a03-narrow-saved-card-json-surface`: openspec refused to apply: member-saved-cards MODIFIED failed for header "### Requirement: Saved-card management uses /api/* JSON only for Stripe.js, HTMX HTML for everything else" - not found
```

The "openspec refused to apply" prefix tells you this is a spec-content problem — autocoder is reporting honestly; the broken delta is in the named change's source. The text after the colon is the actual openspec error.

### Common cause 1: a `MODIFIED` requirement was renamed elsewhere without a `RENAMED` block

This is the most common cause. A change in archive history retitled a requirement (e.g. via a `## MODIFIED Requirements` block that edits the header itself). Downstream changes that target the OLD header now fail because openspec can't find it.

**Fix:** add a `## RENAMED Requirements` block to the change that owns the rename. Format:

```markdown
## RENAMED Requirements

- FROM: `Saved-card management uses /api/* JSON for Stripe.js`
  TO: `Saved-card management uses /api/* JSON only for Stripe.js, HTMX HTML for everything else`
```

Once the renaming change is fixed, downstream changes resolve automatically on the next replay — you usually don't need to touch them.

### Common cause 2: requirement body lacks a normative keyword

Openspec rejects requirement bodies that don't include `SHALL`, `SHALL NOT`, or `MUST`. The error reads roughly `missing normative keyword`.

**Fix:** edit the requirement body in the named change's `specs/<capability>/spec.md` to include one of the normative keywords. Example: `is created` → `SHALL be created`; `is not sent` → `SHALL NOT be sent`.

### Common cause 3: the target requirement was never `ADDED` anywhere in archive history

If a `MODIFIED` references a requirement that was originally created via the manual-archive era (before sync was wired up), there may be no `## ADDED Requirements` record for it anywhere in the archive. The chronological replay then has nothing to apply the MODIFY against.

**Fix:** in the change where the requirement logically originated, add a `## ADDED Requirements` block introducing the requirement at its original shape. Replay the rebuild; the downstream MODIFYs now resolve.

### Cascade tip

Failures in stacked changes (`a08-foo`, `a09-foo`) often resolve themselves once their parent (`foo`) is fixed and re-archived. Fix the parent first, run the rebuild again, and re-check what's still broken. The chronological replay handles dependency ordering naturally as long as each individual change is internally valid.

### What rollback guarantees

The rebuild treats each change atomically. If openspec refuses to apply a change, the rebuild rolls that change back to `openspec/changes/archive/<original_name>/` so your working tree is never contaminated with active-path entries from a partial rebuild. The summary line in the PR body confirms the rollback count, e.g.:

```
Replayed 41 archived change(s) chronologically; 34 succeeded, 7 failed (7 rolled back to archive).
```

If `R == F`, your workspace is clean and you can safely edit the failed changes in `openspec/changes/archive/<original>-<slug>/specs/...` for the next replay. If `R < F`, the gap is explained per-change in the failures list (rollback-of-rollback failures, or data-loss-shaped failures that need operator attention).

### After fixing: re-running the rebuild

Once the fixes are committed and pushed, trigger another rebuild. The chatops verb `@<bot> rebuild-specs <repo>` schedules it for the next polling iteration; the CLI form is `autocoder sync-specs --rebuild --workspace <path>`. The fresh rebuild starts from the same archive history and applies all 41 changes again — the just-fixed entries will succeed, and the cascade-blocked dependents will resolve in the same pass.

## openspec archive aborts with 'MODIFIED failed for header'

You see (or used to see, pre-a17) one of:

```
code-reviewer MODIFIED failed for header "### Requirement: Reviewer prompt budget is operator-configurable" - not found
member-saved-cards MODIFIED failed for header "..." - not found
```

This is `openspec archive`'s late-stage rejection of a change whose `## MODIFIED Requirements` block names a `### Requirement: <title>` that doesn't exist in the canonical `openspec/specs/<capability>/spec.md`. It's a spec-content defect — the change's delta was authored against an invented title (typo, capitalisation drift, half-remembered header) and `openspec validate --strict` did not catch it because that pass only checks delta well-formedness.

**Pre-a17 behavior.** The defect surfaced AFTER the implementer ran to completion: the executor read tasks.md, produced a working diff (often ~$3 of LLM cost), and only at the `openspec archive` step did the spec mismatch abort the pass. The change dropped into the Failed bucket; the next iteration retried; the LLM cost was burned again. After perma-stuck-threshold iterations the change ended up perma-stuck (the real incident on 2026-05-27 — see archived change `a07-reviewer-prompt-budget-and-per-change-mode`).

**Post-a17 behavior.** The polling loop now runs a spec-delta archivability pre-flight BEFORE the executor. The check parses each delta block's `### Requirement:` headers and verifies the per-kind precondition against canonical:

- ADDED title must NOT already exist in canonical.
- MODIFIED title MUST exist (the a07 class — exact string match, including capitalisation).
- REMOVED title MUST exist.
- RENAMED `from:` MUST exist; `to:` MUST NOT exist.

On any precondition violation, autocoder writes `<workspace>/openspec/changes/<change>/.needs-spec-revision.json` with an `unarchivable_deltas` array enumerating every mismatch, posts the existing `AlertCategory::SpecNeedsRevision` chatops alert with a body framing the failure as "unarchivable spec deltas (pre-flight)", and halts the queue. The executor is NEVER invoked. No LLM cost is incurred for changes whose deltas would fail at archive time anyway.

**Where to find the diagnosis.** Read `unarchivable_deltas` in the marker file. Each entry names the capability, the delta kind, the offending header, and a one-line reason. The marker's `revision_suggestion` is auto-generated and lists every violation in a single block plus the next-step instructions.

**Fix.** Edit `openspec/changes/<change>/specs/<capability>/spec.md` so each delta block's `### Requirement:` header matches the canonical title character-for-character. The `unarchivable_deltas` array names the offending headers in the order they appear in the spec. After committing + pushing, clear the marker via `@<bot> clear-revision <repo> <change>` from chat (or `rm <workspace>/openspec/changes/<change>/.needs-spec-revision.json` directly). The next iteration retries the change with the corrected spec.

**Why this matters.** The pre-flight is sub-millisecond and runs on every change before every executor invocation (no caching — the canonical might have shifted since the prior check). The cost trade is dramatically favourable: a few markdown parses per iteration in exchange for never running an implementer against a change whose archive step is structurally guaranteed to fail.

## PR-comment revision keeps failing

You comment `@<bot> revise <text>` on an open PR and the bot replies
`✗ Revision attempt failed: ...` instead of applying the change. Possible
causes:

- **Executor failure (Failed outcome):** the wrapped CLI returned a
  non-zero exit. The reason in the reply comment is the executor's stderr
  tail. Investigate via `journalctl -u autocoder` for the full log;
  the per-change run log at `/tmp/autocoder/logs/<workspace>/<change>.log`
  contains the full prompt + stdout + stderr.
- **Commit/push failure:** the executor succeeded but `git push
  --force-with-lease` was rejected (typically because the remote agent
  branch moved between fetch and push). Retry by posting another
  `@<bot> revise ...` — the next iteration's force-push usually succeeds.
- **Failed attempts count toward the cap.** Five Failed revisions in a
  row will trip the cap-decline path. Close + re-open the PR to reset.

## PR-comment revision is silently ignored

No bot reply, no apparent action. Check:

- **Cap reached:** look for a `🛑 Revision cap reached` comment earlier
  in the PR thread. Once posted, further triggering comments are silently
  ignored. The chatops channel also got a `🛑 <repo>: PR #<num> hit the
  revision cap` notification when the cap tripped.
- **Trigger pattern is strict:** the comment body MUST begin with
  `@<bot>` (case-insensitive) followed by `revise` (case-insensitive)
  followed by at least one non-whitespace character. `@<bot> looks good`
  is conversational and is ignored. Leading whitespace before `@<bot>` is
  tolerated; a non-`@<bot>` prefix is not.
- **Wrong bot username:** if you have multiple bot users (e.g. one per
  GitHub org via `owner_tokens`), the trigger only fires when the
  mention matches the bot whose PAT is routed to this repo. Check the
  startup log for the resolved username (`self_bot_username` is called
  once at iteration start).
- **PR is not in autocoder's PR set:** the dispatcher only polls PRs
  whose head branch matches `repositories[].agent_branch`. PRs opened by
  hand on a different branch are not watched.
- **Feature disabled:** `executor.max_auto_revisions_per_pr: 0` (legacy
  alias `executor.max_revisions_per_pr: 0`) in config disables the
  dispatcher entirely. Check `config.yaml`.

## Agent timed out — what was it doing?

When a change hits `executor.timeout_secs`, autocoder SIGKILLs the wrapped Claude CLI and the iteration returns `Failed { reason: "timeout" }`. The PR comment for that change shows the fallback `(executor timed out before final summary; see daemon log for action stream)` rather than a normal summary.

To see what the agent was doing when the kill fired, read the per-change log:

```bash
sudo -u autocoder cat /<logs_dir>/runs/<workspace-basename>/<change>.log
```

The `=== ACTIONS ===` section is a chronological line-per-event record of every tool call (Read/Edit/Bash) and intermediate assistant text the agent emitted before the kill. **The last action line names what the agent was doing when timeout fired** — usually one of:

- `[tool_use] Bash ...` running a long-running command (test suite, build, network call). Increase `executor.timeout_secs` or narrow the prompt.
- `[tool_use] Read ...` repeating the same file. The agent is in a re-read loop; the spec may be ambiguous and the agent is hunting for context that isn't there.
- `[tool_use] Grep ...` searching broadly. The agent is exploring the codebase methodically — likely a high-complexity change that just needs more wall-clock budget.
- `[assistant] ...` with no follow-up tool call. The model returned a long reasoning paragraph and was killed mid-emission of the next action.

The `=== FINAL ANSWER (0 bytes) ===` section is empty by definition on timeout — the closing `result` event never arrived, so there is no conversational summary to surface in the PR. This is expected behavior, not a log-write bug.

If the timeout is recurring on the same change, consider:
- Raising `executor.timeout_secs` if the work genuinely takes longer than the default (1800s = 30min).
- Splitting the change into smaller pieces if the agent's action stream shows it bouncing between unrelated subsystems.
- Switching the change to `output_format: text` if you suspect the JSON-stream parsing itself is suspicious — text mode replicates the pre-streaming behavior verbatim.

## Bot replied multiple times to a single message

Slack's Socket Mode delivery contract is explicitly at-least-once: if
the WebSocket ack for an event doesn't reach Slack (transient network
blip, connection rotation, reconnect race), Slack redelivers the same
event on the next connection. Without protection, each redelivery
flows through the full listener pipeline and produces a duplicate
reply.

The inbound listener defends against this with an in-memory dedup
cache keyed by `(channel, ts, user)`. First delivery dispatches
normally; subsequent redeliveries within the cache window are
suppressed and logged at INFO. If you're still seeing duplicates,
check `journalctl -u autocoder | grep 'deduplicated event'` to
confirm the dedup is firing:

- If you see `deduplicated event` lines naming your message, the
  dedup is working — the duplicates were already suppressed; check
  whether your client is rendering a single message twice
  (rare client-side cache bug) versus the bot posting twice (look
  for two `chat.postMessage` calls in the journal).
- If you see NO `deduplicated event` lines for the redelivered
  message, your `dedup_cache_capacity` may be `0` (disabled) or
  too small for your traffic volume. Check `config.yaml` under
  `chatops.slack:`. Defaults are capacity `100` and TTL `600`
  seconds; raise either as needed.

See [CHATOPS.md](CHATOPS.md) "Duplicate-delivery suppression" for
the full picture.

## Bot didn't reply at all (no success, no failure)

The expected `✅ Revision applied` / `✗ Revision attempt failed` /
`🛑 Revision cap reached` comment never appeared. Causes:

- **Network blip:** the `POST /repos/.../issues/.../comments` call
  failed. Check `journalctl -u autocoder` for a WARN-level
  "failed to post ... PR comment" entry. The revision itself may have
  been applied (check the agent branch's commits and the PR diff); only
  the reply comment failed.
- **Auth failure:** the PAT routed for this repo lacks the scope to
  comment, or the token was revoked between startup and the revision
  attempt. The log will show a 401/403 from GitHub.
- **Dispatcher errored before reaching the PR:** check the iteration's
  log lines — if `self_bot_username` or `list_open_prs_for_head` failed
  at startup of the iteration, no PR was processed. The dispatcher logs
  at WARN on every per-PR error so the next iteration retries.

## Audit produces invalid proposal — what to do

Symptom: a chatops `❌ <repo-url>: <audit-type> produced an invalid
proposal that failed openspec validation after <N> retries` notification
fires (see
[CHATOPS.md](CHATOPS.md#progress-notifications)), and the next iteration
shows no commit from the audit on the agent branch. The audit's state
file (`.audit-state.json` at the workspace root) has an
`attempt_history` entry with `outcome_kind: "ValidationExhausted"` and
an `error_excerpt` field containing the first 200 chars of the
validator's stderr.

**What this means.** The audit's LLM produced one or more
`openspec/changes/<slug>/` directories that `openspec validate
<slug> --strict` rejected — typically a hallucinated `## MODIFIED
Requirements` block whose target header does not exist in canonical
state, or a requirement body missing the `SHALL` keyword. The audit
re-prompted the LLM (with the validation error appended) up to
`audits.max_validation_retries` times. None of the attempts produced a
valid proposal, so the audit deleted the change directory and gave up
for this run. No commit was made, no PR was opened, no downstream
cascade occurred.

**This is the right outcome.** Catching the invalid proposal at the
audit boundary is precisely what this validation loop is for. The
related cascade-prevention specs (`queue-archive-aborted-detection` and
`pr-body-proposal-active-path-fallback`) make the *symptoms* of
audit-generated invalid proposals visible downstream; this validation
loop prevents the proposal from entering the queue in the first place.

**What to do.**

1. **If this is a one-off:** ignore it. The audit will re-run on its
   next scheduled cadence (`audits.defaults.<slug>` /
   `repositories[].audits.<slug>`), with no special re-trigger needed.
   LLMs occasionally produce hallucinated headers; one validation
   failure with `max_validation_retries: 1` exhausted means two
   consecutive bad responses, which is unusual but not necessarily a
   pattern.
2. **If the same audit type fails repeatedly:** read the
   `error_excerpt` from `.audit-state.json` to see what the LLM keeps
   getting wrong. Then inspect the audit's prompt template. If you have
   not configured `audits.settings.<slug>.prompt_path`, the embedded
   default lives in `autocoder/prompts/<slug>.md` (cargo built-in). If
   you HAVE configured it, the override file is the place to tighten
   instructions — usually the OpenSpec delta-format rules (the
   `## ADDED Requirements` / `## MODIFIED Requirements` headers, the
   `### Requirement:` line followed by `SHALL`, the `#### Scenario:`
   block format).
3. **If many audit types fail in close succession:** the LLM model
   itself may have degraded (a routing change, a context-window
   regression). The chatops `❌` notification names the audit type so
   you can confirm whether the failures are concentrated in one audit
   or spread across the LLM-driven set (`drift_audit`,
   `missing_tests_audit`, `security_bug_audit`,
   `architecture_consultative`).
4. **If you want to disable retries entirely** (e.g. during a known
   LLM-side outage to stop burning calls): set
   `audits.max_validation_retries: 0`. The first failure becomes
   `ValidationExhausted` immediately. The
   [config field documentation](CONFIG.md) covers the clamp + default.

**Knobs.**

- `audits.max_validation_retries: u32` (default `1`, max `5`). Set to
  `0` to disable retries; higher values trade LLM calls for the chance
  to land a proposal that needed multiple corrections.
- The `attempt_history` in `.audit-state.json` is FIFO-bounded at 20
  entries per audit type. Older entries roll off automatically; nothing
  to garbage-collect by hand.

## Audit log shows `audit skipped: workspace not in a valid state`

Symptom: an audit run log contains an INFO line
`audit skipped: workspace not in a valid state` with fields naming the
audit type, the workspace path, and one of three reasons:
`workspace directory does not exist`,
`workspace exists but has no .git/ subdirectory`, or
`workspace failed validity check`. No chatops notification fires for
the skip, and the audit's `.audit-state.json` entry is unchanged from
the previous run (cadence is not consumed).

**This is informational.** The audit declined to run because the
workspace is in a broken state — typically a `rm -rf` of
`/tmp/workspaces/<sanitized>/` (operator action or a stale `wipe`
chatops command) that occurred between iterations, or a partial clone
that left a workspace directory without a `.git/`. The iteration's
own `workspace_init_failure` log entry a few lines earlier (and the
matching chatops alert under the `WorkspaceInitFailure` category)
names the real problem. The audit skip is the expected downstream
consequence: it exists to prevent audits from creating partial
workspace state via `fs::create_dir_all` that future iterations would
mistake for a real but broken clone.

**What to do.** Fix the workspace-init issue the upstream alert
identifies. The partial-clone case (`exists but no .git/`) is now
self-healing — `ensure_initialized` auto-deletes the partial directory
and re-clones; see [OPERATIONS.md → Partial-clone self-heal](OPERATIONS.md#partial-clone-self-heal).
For other causes (auth, network, missing remote), the underlying
`workspace_init_failure` chatops alert names the real error. Once the
workspace is back to a valid state, the audit will run on its next
cadence (the skipped run did not consume cadence, so the next due
window is unchanged). No special re-trigger is needed.

## Workspace exists but has no `.git/` (partial-clone artifact)

A previous clone attempt left the workspace directory created but
without a `.git/` (network drop, transient auth blip, signal). The
daemon now self-heals this case: each `ensure_initialized` pass runs a
safety check, deletes the partial directory, and re-clones fresh. The
journalctl signal is a single WARN per recovery:

```
WARN workspace=<path> repo=<url> workspace exists without .git; partial clone artifact detected. Deleting and re-cloning.
```

**You should NOT need to `rm -rf` the workspace manually for this
case.** Wait one polling cycle; either the re-clone succeeds and the
iteration proceeds normally, or the re-clone surfaces the REAL clone
failure (`Permission denied (publickey)`, `Could not resolve host
github.com`, etc.) in the next iteration's log and chatops alert. The
real cause is what to fix.

### When the safety check refuses auto-cleanup

If you see the daemon return:

```
workspace path exists but is not a git repository (no .git directory): <path> (partial cleanup refused: <tripwire>; manual operator inspection required)
```

the safety check found one of these in the partial directory:

- `.in-progress*` lock file at any depth.
- `openspec/changes/<slug>/.perma-stuck.json` or `.needs-spec-revision.json` at any depth.
- `openspec/changes/<slug>/.question.json` or `.answer.json` (AskUser markers).

The daemon refuses to silently destroy operator-meaningful state.
Inspect the directory manually:

```bash
ls -la <workspace>/
find <workspace>/openspec/changes -name '.perma-stuck.json' -o -name '.needs-spec-revision.json' -o -name '.question.json' -o -name '.answer.json' -o -name '.in-progress*' 2>/dev/null
```

Then decide:

1. **If the markers are stale** (the change they reference is long gone, or the markers were left over from a prior incident you've already resolved): `rm -rf <workspace>` manually. The next iteration re-clones fresh.
2. **If the markers are legitimate operator state you want to preserve** (e.g. an active perma-stuck change you're working on, an AskUser thread you're waiting on): the partial-clone artifact is the symptom, not the disease. The underlying clone keeps failing — the `.git/` never gets written. Diagnose why (run `git clone <url> /tmp/probe-clone` by hand and read the error), fix that, then `rm -rf <workspace>` so the next iteration starts fresh.

## `send it` got a polite refusal — what each `✗` reply means

The audit-reply-acts flow (`@<bot> send it` posted inside an audit
notification thread) has four refusal paths, each with a distinct
operator-facing reply. If your `send it` was refused, find your exact
reply text below.

### `✗ This reply is in a thread autocoder is not tracking. ...`

The dispatcher could not find an `AuditThreadState` for the thread's
`thread_ts`. Common causes:

- The reply landed on a thread that is NOT an audit notification — for
  example, a regular `@<bot> status` thread or an AskUser thread. The
  `send it` verb only acts on audit-notification threads.
- The audit-thread state file was pruned (entries older than 7 days
  are removed regardless of status — see "stale" below).
- The audit fired but the chatops backend doesn't support native
  threading (the default-impl concat path returns `Ok(None)` and no
  state is stamped). Slack supports threading; the experimental
  backends do not.

**What to do.** If you want to act on a real audit, find the audit's
top-line message in chatops and reply inside that thread. If the audit
was old enough to have been pruned, re-run it via the audit's normal
cadence (or trigger it ad-hoc if your operator workflow supports
that), then `send it` against the fresh thread.

### `✗ This audit's findings are too old to act on (>7d). ...`

The `posted_at` on the audit-thread state is more than 7 days old.
Stale audit findings probably no longer match the current code; acting
on them blindly tends to burn tokens producing a useless diff.

**What to do.** Re-run the audit (`@<bot> audit <type> <repo>` or wait
for the next cadence-driven run). The fresh audit posts a new thread,
and `send it` in THAT thread acts on the current findings.

### `✗ This audit thread is already <status>. No new action taken.`

The state's `status` is either `acted` (a triage already ran against
this thread and the bot's PRs are live) or `triage-pending` (a triage
is queued or in-flight). The verb does NOT re-trigger an already-
running triage; the deduplication prevents the operator from
double-spending the LLM budget on the same findings.

**What to do.**

- If `status = acted`: open the bot's resulting PRs from the previous
  triage. Revise them via `@<bot> revise <text>` on each PR if the
  classification was off. Don't re-trigger triage — the PRs are the
  artifact you wanted.
- If `status = triage-pending`: the previous trigger is still being
  processed. Wait one polling cycle; the bot posts back into the
  thread when the triage completes (either with PR links or with the
  agent's stated reasoning for declining to act).
- If a prior triage failed (`status = triage-failed`), the verb DOES
  re-schedule — `send it` again gets a fresh attempt. Watch the thread
  for the failure reason; if it's a transient infra issue, retry; if
  it's a real problem with the findings, revise the audit's source
  (e.g. tweak its config or fix what produced the noise) before
  retrying.

## `propose` got a polite refusal — what each `✗` reply means

The chat-request-triage flow (`@<bot> propose <repo> <free-form text>`)
has several refusal paths, each with a distinct operator-facing reply.
The most common ones:

### `✗ propose: missing request text. ...`

The operator typed `@<bot> propose myrepo` with nothing after the
repo-substring (or only trailing whitespace). The dispatcher needs a
non-empty description to hand to the chat-triage LLM.

**What to do.** Re-send the verb with a description: `@<bot> propose
myrepo add a /healthz endpoint that returns the daemon's version and
uptime`.

### `✗ propose: missing repo-substring. ...`

The operator typed `@<bot> propose` with nothing after the verb. The
dispatcher needs a repo-substring to pick which configured repository
the request targets.

**What to do.** Re-send with `<repo-substring> <text>` after the verb.

### `✗ propose: request text exceeds 10000 characters. ...`

The free-form text after the repo-substring is over the 10,000-character
cap. The cap keeps the inbound dispatch path bounded and the chat-triage
prompt's token budget predictable.

**What to do.** Put longer descriptions in an issue, doc, or RFC and
reference it in a shorter request — e.g.
`@<bot> propose myrepo see ISSUE-123 for the auth-extraction plan;
implement the storage-layer changes from the "Phase 1" section`.

### `✗ propose: chatops backend not configured; ...`

The daemon's `OperatorCommandDispatcher` was constructed without a
chatops backend. This is a configuration error — the `propose` verb
needs the backend to post its top-level ack (whose `ts` becomes the
proposal-request's lifecycle thread).

**What to do.** Make sure the daemon is started via the production
path that wires `.with_chatops(slot.backend.clone())` into the
dispatcher (the install / systemd path does this automatically). The
in-process test harness wires it through `with_chatops` directly.

### `✗ propose: could not post ack to chat: <reason>`

The dispatcher reached the chatops backend but the post request itself
failed (HTTP error, Slack API error, etc.). The state file is NOT
written and the control-socket action is NOT submitted — the operator
sees the refusal in the same channel/thread the verb came from.

**What to do.** Read the `<reason>` for the underlying error. Common
causes: bot token revoked, channel-write permission missing, Slack
rate-limit. Fix the upstream issue and re-send the verb; the request
is idempotent — a successful retry generates a new `request_id`.

### Untracked / stale / status-conflict cases

The `propose` lifecycle has no "untracked thread" path the way `send
it` does — the verb fires at channel level, not in a thread. But the
proposal-request's state file has the same 7-day staleness rule:
state files whose `submitted_at` is older than 7 days are pruned at
iteration start regardless of terminal status (`Acted`, `Discussed`,
`TriageFailed`). A pruned state file means the lifecycle thread is no
longer authoritative — subsequent `@<bot> revise` comments on the
PRs spawned from that request still work (revisions key off the PR's
branch, not the state file), but the request itself is closed.

If you need to start a fresh triage on the same topic after a stale
prune, just `@<bot> propose <repo> <text>` again; the verb generates
a new `request_id` and a new lifecycle thread.

### `✓ Wiped <path> (drain timeout — iteration may have been stuck)`

The wipe-workspace flow on `confirm` signals the in-flight per-repo
iteration's per-iteration cancel token and waits up to
`executor.wipe_drain_timeout_secs` (default 30) for the iteration to
release its busy marker before deleting the workspace directory.
"Drain timeout" means the iteration did not respond within that window.
The wipe still succeeded — the directory is gone and the next polling
tick will re-clone — but the timeout is a yellow flag worth
investigating.

The usual cause is a blocking syscall inside the iteration: a hung
executor subprocess (a `claude` CLI that never returned), a long `git
fetch` against a slow remote, or an external tool the iteration is
waiting on. The per-iteration cancel token can only fire at safety
points; a blocking syscall holds the iteration past those points.

**What to do.** After the wipe completes, open the stuck iteration's
log at `<logs_dir>/runs/<repo-sanitized>/<change>.log` (typically
`/var/log/autocoder/runs/<repo>/<change>.log` under systemd; the log
directory persists across a workspace wipe — only the workspace
itself is removed). Look at the tail to see what the iteration was
doing when the cancel signal arrived. Common findings:

- A `claude` subprocess hanging mid-tool-use → the wrapped CLI may
  have crashed without exiting. Restart the daemon (`autocoder run`) so
  any orphan subprocesses are reaped, and re-issue the change.
- A `git fetch` waiting on an unreachable remote → check network and
  the upstream's reachability from this host.
- A long executor invocation that simply needs more time → consider
  raising `executor.wipe_drain_timeout_secs` (capped at 300) so future
  wipes give the iteration the headroom it needed.

## Audit storm after reboot — daemon re-fires every audit on the first iteration

**Symptom.** A few minutes after a host reboot, the chatops channel
fills up with audit notifications — `🔍 created proposal`,
`🔍 architecture-brightline reported N findings`, etc. — for audits
that ran recently and were not due for hours or days.

**Root cause.** Pre-`state-paths-out-of-tmp`, autocoder wrote audit-
cadence state under `/tmp/`, which is tmpfs on most server distros and
gets wiped on reboot. When the cadence file disappears, every audit's
`last_run` defaults to "never" and the cadence check fires
unconditionally on the first iteration after startup. The same
mechanism reset failure counters and discarded operator-set markers
(`.perma-stuck.json`, `.needs-spec-revision.json`).

**Fix shipped.** Per-repo workspaces now live under `<cache_dir>/`
(real disk, not tmpfs) and the per-audit-type cadence files live
under `<state_dir>/audit-state/` (see [`STATE-LAYOUT.md`](STATE-LAYOUT.md)).
The daemon also reloads the audit cadence map from
`<state_dir>/audit-state/` before any cadence check fires, so
on-disk timestamps are respected. After the upgrade, the first
daemon start migrates legacy `/tmp` data into the new layout and
writes `<state_dir>/.migration-from-tmp-done`.

**If a storm still happens after this change shipped.** Check that
your daemon actually picked up the new paths:

1. `journalctl -u autocoder | grep "daemon paths resolved"` — confirms
   the resolved `state_dir`, `cache_dir`, `logs_dir`, `runtime_dir`
   are what you expect. If `state_dir` is still under `/tmp/`, your
   `paths:` config or `AUTOCODER_STATE_DIR` env var is pointing
   there.
2. `journalctl -u autocoder | grep "legacy /tmp migration"` — confirms
   the migration scan ran. If absent, the daemon never reached the
   migration call; look for an earlier startup error in the same log.
3. `ls <state_dir>/.migration-from-tmp-done` — confirms the migration
   wrote its marker. If missing, the migration encountered errors;
   re-check `journalctl` for the per-entry ERROR lines.
4. `ls <cache_dir>/workspaces/` — confirms workspaces moved off
   `/tmp/`. If empty, no workspaces have been initialized yet (first
   iteration after install).

To force a re-scan after restoring legacy data from backup, remove
`<state_dir>/.migration-from-tmp-done` and restart the daemon.

## Repo stuck on stale busy marker after daemon restart

**Symptom.** `@<bot> status <repo>` shows `currently: idle`, the queue has pending changes, but every polling iteration logs:

```
INFO busy marker present; skipping iteration url=git@github.com:owner/repo.git pid=490170 \
     stage=executor age=53m threshold=10m pid_alive=false recovery_eligible=true
```

The repo never progresses. New daemons started after the original daemon was killed (SIGTERM, restart, host reboot mid-iteration) inherit the old marker file and refuse to acquire because the recorded PID is from a process that no longer exists.

**Diagnostic commands.** Replace `<basename>` with the workspace directory name (e.g. `github_com_owner_repo`):

```bash
sudo -u autocoder ls -l /tmp/autocoder/busy/                          # marker present?
sudo -u autocoder cat /tmp/autocoder/busy/<basename>.json             # inspect contents
ps -p $(jq -r .pid /tmp/autocoder/busy/<basename>.json)               # is the PID alive?
```

If `ps` reports `no such process` AND the marker is still on disk, the daemon is running a pre-`a08-busy-marker-recovery-semantics` build (which gated dead-pid recovery on `age > timeout_secs + 600`). Upgrade to a build that ships the fix — the underlying cause is removed for the daemon-restart scenario.

**Immediate fix.** Stop the daemon, delete the marker file, and start the daemon again:

```bash
sudo systemctl stop autocoder
sudo -u autocoder rm /tmp/autocoder/busy/<basename>.json
sudo -u autocoder rm -f /tmp/autocoder/busy/<basename>.subprocess     # if present
sudo systemctl start autocoder
```

The next polling iteration acquires a fresh marker and proceeds normally.

**Why this used to happen.** Before `a08-busy-marker-recovery-semantics`, the marker classification logic gated dead-PID recovery on the marker's age exceeding `executor.timeout_secs + 600`. An operator who had bumped `timeout_secs` to e.g. 5400 (90 minutes for a long-running change) saw repos with daemon-restart-leftover markers stay stuck for up to 100 minutes before recovery fired — even though `/proc/<pid>` confirmed the recorded process was long gone.

**What the fix changes.** Dead-PID recovery now fires IMMEDIATELY in the classification logic, with no age check. The `recovery_eligible=true` in the log line above confirms the next iteration WILL recover the marker — under the new behavior, that next iteration happens within a few seconds (it's still gated on the polling cadence) and resolves the symptom on its own. Operators on the fixed build do not need to manually delete the marker for the daemon-restart case.

## `git checkout` fails with "local changes to `.alert-state.json`"

**Symptom.** A polling iteration logs an error containing one of:

```
error: Your local changes to the following files would be overwritten by checkout:
        .alert-state.json
```

```
error: The following untracked working tree files would be overwritten by checkout:
        .alert-state.json
```

The iteration fails on `recreate_branch` (which runs `git checkout -B <agent_branch>`) and the repo never progresses.

**Root cause.** Pre-`a16-consolidate-workspace-bookkeeping-to-state-dir`, the daemon wrote alert-throttle state at `<workspace>/.alert-state.json` — directly inside the managed repository's working tree. `git checkout` saw the daemon's writes as either uncommitted modifications (when the file was tracked) or as an untracked-clobber risk (when it wasn't), and aborted to protect them. The daemon then failed the iteration.

**Automatic fix.** Upgrade to a build that ships `a16`. On the next daemon startup, the first-startup migration (`alert-state-from-workspace`, see [OPERATIONS.md → Migrations](OPERATIONS.md#migrations)) moves `<workspace>/.alert-state.json` to `<state_dir>/alert-state/<workspace-basename>.json` and removes the workspace copy. The workspace will not contain the file again — the daemon writes to the state-dir path going forward, so subsequent `git checkout` operations never see it. Tracked-in-git copies are handled by `git rm --cached` + commit + push to the base branch (per-repo failure: branch protection rejection → ERROR log + manual operator action; the marker stays unset so the next startup retries).

**Immediate fix for operators stuck on a pre-`a16` build.** Until you can upgrade:

```bash
# Stop the daemon so it does not race the cleanup.
sudo systemctl stop autocoder

# In every affected workspace, remove the file. (If the file is tracked
# in git, also run `git rm --cached .alert-state.json` and commit + push
# the removal to the base branch.)
for ws in /var/cache/autocoder/workspaces/*; do
  rm -f "$ws/.alert-state.json"
done

sudo systemctl start autocoder
```

The next polling iteration recreates the alert-throttle state on demand the first time a categorized failure fires — there is no operator-meaningful data loss; the file holds only the 24h throttle window.
