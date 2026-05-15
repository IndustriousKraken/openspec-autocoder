# Design notes

This document captures decisions worth understanding before reading the spec or modifying the implementation. The proposal describes *what* the change does; this file explains *why* the design is shaped the way it is.

## State split: per-repo counter file + per-change marker

Two files do related work:

- `.failure-state.json` at the workspace root holds the running consecutive-failure counter for every change in that repo (`{ <change>: { count, last_reason, last_failed_at } }`).
- `.perma-stuck.json` inside an individual change directory is a presence-only flag: when present, the change is parked.

A simpler design would put both bits of state in a single per-change file. Rejected for one reason: `list_pending` runs on every poll, walks the active changes directory, and is currently a cheap filesystem scan (directory enumeration + a couple of `exists()` checks per entry). If the perma-stuck signal lived inside a JSON file, `list_pending` would need to read and parse that file for every active change to decide whether to include it. The marker-as-presence-flag keeps `list_pending` proportional to directory size with no parsing on the hot path.

The counter file does need to be read and written on every Failed/Archived outcome — but those are cold-path events relative to per-poll enumeration, so the JSON cost is acceptable there.

Consequence: there are two files that must stay coherent. The proposal addresses this by clearing the counter entry on Archived (regardless of how the archive happened — normal or self-heal). The marker is removed only by the operator (or by future `autocoder unstick`); when removed, the counter for that change resets on the next failure because the counter increment path is "fetch entry, increment or create at 1."

## Operator-clears-marker is the retry signal (no auto-retry)

When a change hits the threshold, the marker stays until a human deletes it. The alternative — auto-retry after some timeout (24 hours, a day's worth of poll passes, etc.) — was rejected because the threshold's whole point is to surface "a human needs to look at this." Auto-retry would just resume the loop after a delay; the original loop is itself wasteful even at 30-minute intervals, and a 24-hour-spaced loop still adds up across 9+ repos.

The cost of the operator-action requirement is one journalctl/Slack ping per stuck change, after which the change is genuinely paused. That's a better tradeoff than burning tokens periodically.

If we later add an `autocoder unstick <change>` CLI subcommand (mentioned as future work in tasks.md 5.5), it would do exactly what manual `rm .perma-stuck.json` does today — the contract doesn't change, just the ergonomics.

## What counts as a failure (and what doesn't)

The counter increments only on outcomes where the executor ran and the agent itself either Failed or produced a no-op Completed that the daemon transformed into Failed (via `no-op-completion-is-failure` or, when this change lands together with `self-heal-already-implemented`, the not-self-healed branch). Daemon-side errors that happen BEFORE the executor invocation explicitly do not count:

- Workspace init failure (clone errored, fork URL unreachable, dirty workspace can't auto-recover).
- openspec preflight failing mid-runtime.
- GitHub API transport errors.
- Busy-marker stuck-state detection that skips the iteration.

These are infrastructure-flakiness signals, not "the agent can't do this change" signals. Counting them would unfairly drive perma-stuck status during network blips or operator-fix-in-progress windows. The user would then have to clear markers while triaging an unrelated outage, which inverts the cost calculation.

The boundary: increment the counter only after the executor has been given a fair attempt at the change. If the daemon never reached the point of asking the agent to work on this specific change in this pass, the counter is unchanged.

This makes the counter a reasonably accurate signal of "the agent has now repeatedly tried and repeatedly given up on this change." Two such attempts (the default threshold) is sufficient evidence to escalate.

## Why threshold defaults to 2, not 3

A single failure could plausibly be transient even at the agent level — Claude hits a rate limit mid-run, the workspace had an unusual file state from a prior pass, etc. Two consecutive failures with the same change in the same conditions is strong evidence of a non-transient blocker.

Three would add a redundant retry that doesn't change the outcome in the common case; the only scenario where the third attempt succeeds without operator intervention is "rate limit cleared and Claude actually had it figured out the whole time." That's rare enough that the cost of the third attempt across 9 repos isn't worth the small probability of avoiding an operator ping.

The threshold is configurable (`executor.perma_stuck_after_failures`) so operators with a flakier environment can bump it; we just don't default to that.

## Interaction with `self-heal-already-implemented`

If both this change and `self-heal-already-implemented` are deployed, the interaction is:

- A change whose implementation is already in HEAD: self-heal archives it on the first pass. Counter never increments. No perma-stuck escalation.
- A change with a genuine blocker: counter increments on each Failed pass. Threshold hit → perma-stuck. Self-heal does not interfere because its precondition (`openspec validate --strict` passes AND all tasks are `[x]`) is not met for a genuinely blocked change.

Order of landing doesn't matter — both changes are additive at the failure-handling sites, and the interaction is well-defined either way.

## What we are NOT doing

- **Per-failure-reason classification.** The counter doesn't distinguish "agent said `task X is impossible`" from "agent said `infrastructure broken`." Either could repeat. We treat any Failed identically. A future improvement could pattern-match failure reasons to fail-fast on known-unresolvable patterns, but that's brittle and out of scope.
- **Cross-repo correlation.** If the same change name appears in multiple repos (rare but possible), each repo has its own counter and its own perma-stuck state. They do not share signals.
- **Time-based auto-clear.** A `.perma-stuck.json` file does not expire. It sits there until removed manually.
