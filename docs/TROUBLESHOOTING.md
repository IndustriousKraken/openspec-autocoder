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
- **Feature disabled:** `executor.max_revisions_per_pr: 0` in config
  disables the dispatcher entirely. Check `config.yaml`.

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
