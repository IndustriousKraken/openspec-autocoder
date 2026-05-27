## Context

autocoder's polling iteration today logs progress to stderr but only
emits ChatOps messages on the AskUser escalation path. Operators
watching the daemon's chat channel see escalation questions but nothing
else — not when work is picked up, not when work succeeds, not when
infrastructure breaks. A daemon that's silently failing to clone a
moved repository for three days looks identical to a daemon that
correctly finds nothing to do.

The fix has two faces:

1. **Positive signal** — when autocoder finds work, it should say so.
   One line per change pickup is enough; the channel becomes a low-fi
   activity feed.
2. **Negative signal** — when autocoder repeatedly hits a *predictable*
   infrastructure failure, the operator deserves an alert. But the
   poll interval is 5 minutes; alerting on every failed iteration
   would be intolerable. Throttle to once-per-24h-per-category-per-repo,
   self-clearing on the next success.

## Goals / Non-Goals

**Goals:**

- A chatops-channel-watching operator can tell, without checking logs
  or GitHub, that autocoder is alive and finding work.
- Predictable infrastructure failures (clone failure, push rejected, PR
  creation 4xx) surface in chat exactly once per 24h until fixed.
- The throttle state is per-repo + per-category; one repo's broken
  workspace does not silence another repo's push failure.
- Operators who consider notifications noise can disable each type
  independently.
- A broken chat backend never produces a recursive cascade — chatops
  failures log to stderr and never re-route through chatops.

**Non-Goals:**

- **Success/PR-opened notifications.** GitHub already sends PR-opened
  emails and webhooks; replicating that signal in chat adds noise to
  the channel without new information. If an operator wants
  end-of-work signal, they subscribe to repo notifications on GitHub.
- **Executor-Failed alerts.** Executor failures are usually transient
  (LLM hiccup, sandbox timeout) and resolve on the next iteration
  without operator action. Out of scope; might revisit if a pattern
  emerges where Failed becomes persistent.
- **Reviewer-failed alerts.** The reviewer is already non-blocking:
  failure surfaces in the PR body. No chat alert needed.
- **Daily summary mode.** "Here's what autocoder did today" is a
  natural follow-on but adds enough complexity (digest assembly,
  scheduling, multi-repo aggregation) to warrant its own change.
- **Custom message templates.** The text of each notification is
  hard-coded for this change. Templating is deferred until at least
  one operator asks for it.
- **Alert state persistence across host reboots.** `.alert-state.json`
  lives in `/tmp/workspaces/<repo>/`; reboot → operator gets re-alerted.
  This is acceptable because reboot is a rare and intentional event.
- **Smart de-duplication beyond category.** Two distinct
  `pr_creation_failure` instances (one a 403, one a 404) collapse to
  one alert in the 24h window. The error excerpt in the alert message
  reflects whichever instance triggered the most recent alert.

## Decisions

### Notification text

Three message templates:

```
🚀 `<repo-url>`: starting work on `<change-name>` — <first-line-of-Why>
```

```
⚠️ `<repo-url>`: <category-label> for the past 24h. Latest: <error-excerpt-truncated-to-200-chars>
```

(One template per failure category, differing only in `<category-label>`:
`workspace_init_failure` → "workspace init keeps failing";
`branch_push_failure` → "branch push keeps failing";
`pr_creation_failure` → "PR creation keeps failing".)

The start-of-work template reuses the existing first-line-of-Why
extraction from `commit_subject` building (line 474 in polling_loop.rs
today, via `first_line_of_section(proposal, "## Why")`).

### Configuration

```yaml
slack:
  # existing fields...
  notifications:
    start_work: true       # default true; one message per change pickup
    failure_alerts: true   # default true; throttled per (repo, category)
```

Both keys are optional. Absent `notifications:` parses to the defaults
(both true). Setting an individual key to `false` suppresses that type
without affecting the other.

Naming note: the keys live under `slack:` today; the
`experimental-chatops-providers` change will move them to
`chatops.notifications:` when it lands. No design decision here other
than to follow whatever the parent block is called.

### Failure categories

Limited to three for this change:

| Category                   | Trigger site in code                                            | Suppression scope                                              |
|----------------------------|------------------------------------------------------------------|----------------------------------------------------------------|
| `workspace_init_failure`   | `workspace::ensure_initialized` returns Err                     | per repo: subsequent identical category within 24h is silent   |
| `branch_push_failure`      | `git::push_force_with_lease` returns Err                        | same                                                            |
| `pr_creation_failure`      | `github::create_pull_request` returns Err                       | same                                                            |

Other failure surfaces are deliberately excluded (see Non-Goals).
Adding categories later is purely additive — same state file, new key.

### State file

Path: `<workspace>/.alert-state.json` (lives alongside `.in-progress`
markers; per-repo isolation via workspace separation).

Shape:

```json
{
  "alerts": {
    "workspace_init_failure": {
      "last_alerted_at": "2026-05-13T20:00:00Z",
      "last_error_excerpt": "git fetch failed: Authentication failed"
    },
    "branch_push_failure": {
      "last_alerted_at": "2026-05-13T22:14:00Z",
      "last_error_excerpt": "refusing to update protected branch..."
    }
  }
}
```

Categories with no entry mean "no recent alert; next failure alerts
immediately." Categories cleared on success: the polling iteration
that produces a non-Err result for ANY change in this repo clears the
entire `alerts` map, on the principle that "the daemon is making
progress here again."

Atomic writes via the existing tempfile-then-rename pattern (already
in chatops state-file helpers).

### Algorithm at each failure site

```rust
async fn handle_failure(
    workspace: &Path,
    chatops: Option<&ChatOpsContext>,
    notifications_enabled: bool,
    category: AlertCategory,
    err: &anyhow::Error,
) {
    if !notifications_enabled { return; }
    let Some(ctx) = chatops else { return; };
    let now = Utc::now();
    let mut state = AlertState::load_or_default(workspace);
    let should_alert = state
        .alerts
        .get(&category)
        .map(|entry| now - entry.last_alerted_at >= Duration::hours(24))
        .unwrap_or(true);
    if !should_alert { return; }
    let text = format_alert_text(category, err);
    if let Err(post_err) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::error!(?post_err, "chatops alert post failed; not retrying through chatops");
        return;  // do NOT update timestamp; next 24h will re-attempt
    }
    state.alerts.insert(category, AlertEntry {
        last_alerted_at: now,
        last_error_excerpt: excerpt(err, 200),
    });
    state.save(workspace);
}
```

Two important properties of this algorithm:

- If the post itself fails, the timestamp is NOT updated. The next
  iteration's identical failure will attempt to alert again. (Without
  this, a transiently-broken chatops + a recurring infra failure could
  silence the alert window completely.)
- The "clear on success" logic lives in the calling code, not here.
  The successful path calls `AlertState::clear(workspace)`
  unconditionally; cheaper than diff-checking, and idempotent.

### `post_notification` vs `post_question`

Two distinct methods on the chatops surface:

- `post_question(channel, change, text) -> Result<String>` — returns
  the thread handle the polling loop tracks for replies. Existing.
- `post_notification(channel, text) -> Result<()>` — one-way; no
  thread tracking; no handle returned. New.

Could `post_question` cover both cases by returning `Result<Option<String>>`
where notifications return `None`? Could, but it conflates two
semantically distinct operations and makes the polling loop's call
sites less readable. Two methods is clearer.

### Wiring sites

Three call sites in `polling_loop.rs` change:

1. `run_pass_through_commits` after the workspace init succeeds and
   before walking the queue: clear alert state.
2. `walk_queue` when a change is dequeued: emit start-of-work
   notification (if enabled).
3. Each of the three failure sites: call `handle_failure` with the
   matching category.

The clear-on-success in #1 happens BEFORE any work is attempted in
that iteration. Subtle: if iteration #N's workspace init succeeds but
its push fails, the iteration cleared `workspace_init_failure`'s
state (correctly, since init now works) and then sets
`branch_push_failure`'s state (correctly, new problem).

## Risks / Trade-offs

- **Risk:** clear-on-success is too aggressive — a transient success
  between two consistent failures clears the alert, then the next
  failure re-alerts immediately (potentially within the same hour as
  the first alert).
  - **Mitigation:** this is correct behavior. The semantics are
    "the daemon recovered, then broke again." The operator wants to
    know about the new failure even if it looks similar to the old
    one; the 24h throttle is for continuously-failing scenarios.

- **Risk:** `.alert-state.json` joins a growing set of workspace state
  files (`.in-progress`, `.question.json`, `.answer.json`); operators
  may not realize they exist.
  - **Mitigation:** document in ChatOps Escalation section's
    "Workspace artifacts" note. The file is JSON, safe to inspect,
    safe to delete (deleting just resets the alert window).

- **Risk:** notification spam on initial deployment if multiple repos
  have queued pending changes — the operator sees a flurry of
  start-of-work messages.
  - **Mitigation:** intentional. The flurry signals "yes, autocoder
    is doing work on N changes right now." Operators who hate the
    noise set `start_work: false`.

- **Risk:** chatops backend itself flaky — failure alerts get
  swallowed.
  - **Mitigation:** the "don't update timestamp on post failure"
    rule means a flaky backend re-attempts every iteration until it
    succeeds, then enters the normal 24h cycle. Stderr logs always
    capture the alert text whether or not it posts.
