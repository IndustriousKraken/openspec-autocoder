## Why

Today autocoder's ChatOps channel only receives messages when the agent
needs human input (the AskUser escalation path). An operator watching the
channel has no positive signal that the daemon is alive, is finding
configured repositories, or is making progress through their pending
changes. They also get no early warning when predictable infrastructure
problems prevent work from happening — a wrong PAT scope, a stale branch
protection rule, a repository moved or renamed — until they notice that
no PRs are appearing.

This change adds two operator-facing signal types beyond the existing
AskUser escalation:

1. **Start-of-work notifications** — a brief one-liner whenever autocoder
   begins implementing a pending change, naming the repository and the
   change. Off-by-default for operators who consider it noise; on for
   everyone else.
2. **Throttled failure alerts** — when a recurring, *predictable* failure
   prevents work (clone/fetch failure, push rejected, PR-creation 4xx
   from GitHub), post once and suppress subsequent identical alerts for
   24 hours. Cleared when the next iteration of the same repo succeeds,
   so a transient outage doesn't leave the operator without a follow-up
   signal.

The goal is "the channel tells you what autocoder is doing." Silence
should mean it isn't finding work; activity should mean it is. A
predictable, recurring failure should produce exactly one alert per day
until fixed.

## What Changes

- Extend the `slack:` config block with a `notifications:` sub-block
  carrying two booleans:
  ```yaml
  slack:
    bot_token: { value: "xoxb-..." }
    default_channel_id: C0123456789
    notifications:
      start_work: true       # default true
      failure_alerts: true   # default true
  ```
  Absent block parses to "both true" — i.e. an operator with no
  `notifications:` key gets the notifications on by default.

- Add a `post_notification(channel, text)` method to the chatops-manager
  surface. Distinct from `post_question` because notifications are
  one-way (no thread to poll, no handle to return); the method returns
  `Result<()>`.

- In `polling_loop::run_pass_through_commits`, after a change is
  dequeued and locked but before the executor is invoked, post the
  start-of-work notification:
  ```
  🚀 `<repo-url>`: starting work on `<change-name>` — <first line of ## Why>
  ```
  Suppressed when `notifications.start_work: false` OR when no
  `chatops:` is configured.

- Add a per-workspace `.alert-state.json` tracking the
  `last_alerted_at` timestamp for each predictable-failure category.
  Three categories in this change:
  - `workspace_init_failure` — clone or fetch failure
  - `branch_push_failure` — git push (force-with-lease) rejected
  - `pr_creation_failure` — GitHub REST API returned non-2xx on PR
    creation

  Failures in other categories (executor-`Failed`, reviewer-failed,
  chatops-post-failed) are explicitly out of scope.

- At each predictable-failure site, if the failure recurs:
  1. Read `.alert-state.json` (absent → no prior alerts).
  2. If `now - last_alerted_at[category] >= 24h` (or no prior entry),
     call `post_notification` with a category-specific text, then write
     the updated timestamp.
  3. Otherwise, suppress the alert (the iteration still errors and
     logs as today).

- On the next *successful* iteration of the same repo, clear all
  `.alert-state.json` categories so the next failure (which could be
  the same or different category) re-alerts immediately.

- **chatops-post failures are never re-routed through chatops.** If
  `post_notification` itself fails, log only — no recursive alert
  attempt.

## Capabilities

### Modified Capabilities

- `chatops-manager`: gains a `post_notification` method on the
  same backend (currently SlackBackend; will become a trait method
  when `experimental-chatops-providers` lands).
- `orchestrator-cli`: emits start-of-work notifications when changes
  are dequeued, and emits throttled failure alerts at three
  predictable-failure sites via a per-workspace `.alert-state.json`.

## Impact

Operators get continuous low-volume signal in their ChatOps channel:
a notification when autocoder starts a change, a notification when
work succeeds (existing PR-creation flow already pings GitHub
notifications, no change needed here), and at most one alert per
24 hours per category per repo when something predictable breaks.
The signal is opt-out, not opt-in — first-time deployments see
useful traffic without further configuration.
