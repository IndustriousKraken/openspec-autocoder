## Why

A real incident on `myrepo` ran the executor seven times against `a24-provisioning-wizard` — roughly 22 minutes of agent invocation each — before the operator noticed and stopped the daemon. Each iteration failed at the same step: `queue::archive` returned "archive destination already exists" because main carried BOTH `openspec/changes/a24-provisioning-wizard/` (an active spec) AND `openspec/changes/archive/2026-05-22-a24-provisioning-wizard/` (an earlier archive from a successful PR merge). The active dir kept showing up in `list_pending`; the agent kept implementing it; `queue::archive` kept colliding with the dated entry. The change burned through real Anthropic API budget across seven full executor runs before any human noticed.

Two distinct gaps allowed this:

1. **The collision was detectable before the executor ran**, but the check only fires at archive time — after the agent has already done its full implementation pass. The dated archive entry's existence is a pure filesystem check that takes microseconds; we have all the information we need at the top of the iteration.

2. **The perma-stuck counter only increments on executor outcomes** (`Failed`, no-op completion, lazy-archive). When the executor returns `Completed` and the iteration then fails at `queue::archive` (or any other per-change step downstream of the executor), the counter does not move. Seven iterations with `outcome="error"` produced zero counter increments. The marker that would have excluded the change from `list_pending` never got written.

Either fix on its own narrows the failure mode; together they leave no path for the loop to burn tokens indefinitely:

- (1) catches the specific failure mode without ever calling the executor — saves the 22 minutes per iteration in the common case
- (2) is the defense-in-depth backstop: if a future failure mode lands between the executor's `Completed` return and a successful PR (a class with several known members: queue::archive errors, post-executor commit failures, recovery-failure-during-iteration), the counter still progresses and perma-stuck eventually fires

## What Changes

**1. Pre-flight archive-collision detection.** When the polling loop assembles the iteration's working set (today: `queue::list_pending` returns every directory under `openspec/changes/` that's not `archive` and not perma-stuck-marked), it SHALL also exclude any change whose dated archive entry `openspec/changes/archive/<UTC-YYYY-MM-DD>-<slug>/` already exists at iteration start. The excluded change does NOT get an executor invocation and does NOT count as a perma-stuck failure (it's a structural problem the operator must resolve, not an executor failure).

For each excluded change, autocoder posts a chatops finding (gated by `failure_alerts_enabled`, throttled per the existing per-category 24h throttle) under a new `AlertCategory::ArchiveCollision` variant. The alert body describes the situation concretely — both paths, the workflow to fix — so the operator gets actionable diagnosis rather than "something's wrong."

**2. Broader perma-stuck counter.** The counter SHALL increment on ANY per-change error returned from the polling loop's per-change processing function — not just executor-outcome failures. This covers:
- The existing trio: `Failed`, no-op completion, lazy-archive (unchanged)
- New: any `Err` returned by `queue::archive`, `queue::unlock`, the post-executor commit step, or any other operation scoped to a single change inside `walk_queue`

What does NOT count (unchanged):
- Iteration-level failures that happen OUTSIDE the per-change loop: workspace init, dirty-workspace pre-pass check, branch push, PR creation. These already have their own throttled chatops alerts via the `WorkspaceInitFailure` / `WorkspaceDirtyMidIteration` / `BranchPushFailure` / `PrCreationFailure` categories.

**3. New `AlertCategory::ArchiveCollision`.** Follows the existing per-category 24h throttle pattern. Body shape:

```
⚠️ `<repo>`: archive collision detected for `<change-slug>`
The change at openspec/changes/<change-slug>/ would archive to
openspec/changes/archive/<today>-<change-slug>/, but that path already exists.

This usually means the change was archived earlier (via a merged PR) and re-added
to the active path without removing the prior archive entry. The change is
excluded from this iteration's queue walk to avoid burning agent tokens on a run
that will fail at archive time.

To resolve, on the base branch:
  - If the prior implementation is final: `git rm -r openspec/changes/<change-slug>` and push.
  - If the prior implementation should be reverted and re-done: `git revert -m 1 <merge-sha>` (the merge that landed the prior PR), keeping the revised spec via `git checkout --ours` on the conflicting spec files; push.

Iteration continues with the change excluded.
```

The alert is informational only — the iteration proceeds with the colliding change excluded; other pending changes are processed normally.

**4. Tests for both:**
- A polling-loop test seeds a workspace where both paths exist for a change, asserts `list_pending` (or whatever the assembly step is) excludes that change AND emits exactly one chatops post under `ArchiveCollision`.
- A polling-loop test seeds a queue::archive failure (e.g., by making the archive_root read-only) and asserts the failure counter increments after the executor returns Completed.
- A failure-state test asserts that a per-change Err originating outside the executor (e.g., a stubbed archive function that returns Err) increments the counter same as executor Failed.

## Impact

- Affected specs: `orchestrator-cli` — two ADDED requirements: "Archive-collision pre-flight exclusion" and "Perma-stuck counter covers all per-change errors". Both lift contractual statements about behaviors that exist (perma-stuck) or will be added (archive collision detection) but are not currently pinned in the canonical spec.
- Affected code:
  - `autocoder/src/alert_state.rs` — add `ArchiveCollision` variant to `AlertCategory`.
  - `autocoder/src/queue.rs` — new helper `archive_collision_path(workspace, change) -> PathBuf` that returns the dated archive path the change would target. Optionally a `would_collide(workspace, change) -> bool` wrapper.
  - `autocoder/src/polling_loop.rs` — pre-flight check inside `run_pass_through_commits` (after `list_pending` returns, before `walk_queue` enters per-change processing) that drops colliding changes from the working set and fires the chatops alert.
  - `autocoder/src/polling_loop.rs::walk_queue` (or wherever per-change error handling lives) — the broadened counter increment. Any `Err` returned from the per-change processing function calls `failure_state::record_failure` before propagating the error up.
  - Tests in `polling_loop::tests` for both scenarios.
- Operator-visible behavior:
  - Adversely-shaped repository state (archive collisions) gets diagnosed in one iteration instead of burning agent tokens repeatedly. The chatops alert is throttled to one per category per 24h, matching the existing failure-alert ergonomics.
  - Operators on the existing perma-stuck behavior see no change for changes that are perma-stuck for executor reasons. They DO see the marker fire sooner for changes that loop on non-executor errors (e.g., the myrepo incident would have written the marker after 2 iterations, not run 7 times).
- Breaking: no. The added behaviors strengthen existing contracts without modifying any user-facing API or config field.
- Acceptance: `cargo test` passes (new tests + existing). `openspec validate detect-archive-collision-and-broaden-perma-stuck --strict` passes. A unit test reproducing the myrepo incident's preconditions (both paths present, change in pending) asserts the executor is NOT invoked AND the chatops alert is posted.
