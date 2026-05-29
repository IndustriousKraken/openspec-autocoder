# Design

## Decisions to lock in

### D1. Marker file in the change directory, NOT daemon-internal state.

The continuation state has to survive the gap between:

- The classifier mapping an `iteration_request` outcome (subprocess just exited).
- The next polling iteration on this repo (could be seconds later; could be after an autocoder restart if the operator chose to deploy mid-sequence).

a27a0's in-memory outcome store handles the first case (microseconds between subprocess exit AND classifier read), but it deliberately does NOT survive restarts. Iteration-pending state needs to.

The filesystem marker pattern is already in use AND well-understood: `.in-progress`, `.question.json`, `.perma-stuck.json` all live in the change directory AND give operators a single inspectable surface (`ls -a <workspace>/openspec/changes/<change>/`) for understanding queue state. `.iteration-pending.json` is the same pattern.

Marker file shape:

```json
{
  "completed_tasks": ["1", "2", "4"],
  "remaining_tasks": ["3"],
  "reason": "task 3 needs a refactor I want to plan; running out of time in this iteration",
  "iteration_number": 2
}
```

`iteration_number` is the iteration the operator will observe in the NEXT log line — the polling loop increments it when writing the marker.

### D2. Cumulative completed/remaining lists, reported by the agent. Daemon stores verbatim.

Alternative considered: agent reports just-this-iteration's completed; daemon merges with prior cumulative. Rejected because:

- Two-sided bookkeeping creates a contract the agent has to maintain across iterations (don't double-count, don't miss anything).
- Merging on the daemon side requires the daemon to understand the task identifier scheme. Right now nothing on the daemon side parses task ids; introducing that is scope creep.
- The agent sees the prior cumulative state in the continuation block of its prompt anyway. Copying it forward into the next outcome call is a trivial mechanical step the LLM handles well.

Cost of the chosen shape: the agent could report a smaller-than-actual cumulative list (forgetting a prior completed task). The daemon stores what it's told. In practice the continuation block makes this hard to forget — the prior list is right there in the prompt.

### D3. Iteration cap of 5. Hard, enforced by classifier override.

The cap is chosen for two reasons:

- **Empirical scope:** any reasonable mechanical-refactor decomposition that lands in 5 iterations probably lands in 1 or 2. A change needing 6+ iterations is signaling that the spec is genuinely too large AND should have been split — operator intervention is the right next step.
- **Anti-loop:** an LLM in a tight calibration loop (each iteration produces less progress than the prior) will hit the cap quickly. Failure visibility is what the operator needs.

Enforcement is at the classifier (NOT the MCP tool). Rationale:

- The classifier sees the marker file AND knows the prior iteration_number. The MCP tool sees only the relayed payload.
- Failing at the classifier produces a clean `Failed` outcome with a clear reason. Failing at the MCP tool would produce a tool error AND the agent might retry the call (which would just re-fail), wasting the agent's remaining wall-clock budget.
- The classifier override path preserves the marker file (so the operator can inspect cumulative state) AND preserves the WIP push (so the operator can review what was done before the cap).

If the operator wants to permit a 6th iteration, they delete `.iteration-pending.json` AND re-trigger the change. The change starts fresh from the agent branch's WIP commit.

### D4. Queue front-insertion via marker preference, NOT a separate queue data structure.

The queue is filesystem-driven: `list_pending` enumerates subdirectories of `openspec/changes/` AND filters/sorts them. Introducing an in-memory priority queue would create a second source of truth that the filesystem could disagree with.

The chosen mechanism: `list_pending` reorders its output so iteration-pending-marked changes come first (sorted by `iteration_number` ascending, so a 2nd-iteration ahead of a 3rd-iteration if both happen to exist in the same repo — vanishingly rare in practice, but the deterministic rule prevents surprise). Unmarked changes fall in their normal alphabetical order behind the marked ones.

Filter rules are unchanged. `.iteration-pending.json` is NOT in the exclusion list (unlike `.question.json`, which IS a block). The marker indicates "this is pending AND prioritized," not "this is waiting on something."

### D5. Polling-loop behavior on `IterationRequested`: commit, push, mark, drop lock. No PR touch.

The five actions are independent AND each MUST happen for the next iteration to work correctly:

1. **Commit WIP.** The agent has modified the workspace. Without a commit, the next iteration sees the agent branch's prior tip, NOT the work the agent just did.
2. **Force-push to agent branch.** The commit needs to be visible to the next iteration's workspace initialization (which clones / fetches the agent branch).
3. **Write `.iteration-pending.json`.** The marker carries the cumulative state into the next iteration.
4. **Do NOT open / modify / close any PR.** PR lifecycle remains `Completed`-triggered. An iteration sequence can run entirely without ever opening a PR (the PR opens when the FINAL iteration emits `Completed`).
5. **Drop `.in-progress`.** Per the existing canonical "Unlocking after any executor outcome" requirement; `IterationRequested` is an outcome.

The order matters: commit → push → marker write → drop lock. If autocoder crashes between steps, the recovery story should be:

- Crash between commit AND push: the agent branch is stale. The next iteration force-pushes (overwriting), so the loss is silent. Acceptable.
- Crash between push AND marker write: the next polling iteration sees no marker AND treats the change as a fresh first-iteration. The WIP commit is on the branch but the prompt has no continuation context. This is a soft recovery — the agent re-reads `tasks.md` (which has the prior iteration's checkmarks) AND infers state from there. Tolerable but degraded.
- Crash between marker write AND lock drop: stale `.in-progress` is cleaned up by the existing "Stale lock cleanup on startup" requirement.

The push-then-marker ordering is intentional: push failure leaves no marker, AND a missing marker means "no front-insertion preference," which means the next polling iteration runs alphabetically. The change is still in pending state; the operator sees normal queue behavior; the WIP is gone. This degrades gracefully.

### D6. Continuation block goes AFTER the change body, NOT before.

Options considered:
- BEFORE the change body: prior context sets the frame, then spec arrives, then agent implements.
- AFTER the change body: spec sets the frame, prior context is the most-recent instruction the agent reads.

AFTER wins because:
- The change body (proposal + tasks + specs) is the canonical truth. The continuation block is metadata about a particular run.
- Putting the continuation block last keeps it adjacent to the agent's first action AND less likely to be lost to context compression mid-iteration.
- The framing "your starting state already includes the prior progress" reads more naturally as a closing-pre-action instruction than as an opening framing.

### D7. Continuation block's framing pushes the agent to re-evaluate the prior blocker.

The block content (per the spec deltas):

```
--- BEGIN PRIOR ITERATION SUMMARY ---

A previous iteration of this same change reached a structured stopping
point. Your job is to overcome the prior blocker AND finish the
remaining tasks. The previous iteration's working tree has already been
committed AND pushed to the agent branch — your starting state already
includes its progress.

Cumulative completed (do NOT re-implement): <list>
Remaining: <list>
Prior iteration's stated reason for stopping: <reason>
Current iteration: N of 5 (cap)

Do NOT assume the prior reason still holds. Re-evaluate the blocker
with fresh eyes — the prior iteration's model may have miscalibrated
the scope, AND a different angle of attack may resolve the work in
this iteration. If you genuinely cannot finish in this iteration,
call outcome_request_iteration again with an updated cumulative state
AND a more specific reason. Note that the iteration cap is 5; runs
beyond that are auto-failed.

--- END PRIOR ITERATION SUMMARY ---
```

Two psychological levers built into the framing:
- "Do NOT assume the prior reason still holds" — explicit permission to override prior pessimism, which is the most common recovery path.
- "the iteration cap is 5" — anchors the agent to a finite horizon. Without this, an agent that emits `outcome_request_iteration` doesn't know whether the channel is rate-limited.

The block names the cumulative state AND the prior reason but does NOT name what the agent should do differently. That's deliberate: the agent re-reads the spec AND `tasks.md` AND figures it out. We are explicitly NOT putting prescriptive guidance about how to fix the prior blocker into the prompt; that would either be too generic to help OR too specific to the previous iteration's failure mode.

## Open questions for the implementer

- **Marker write atomicity.** Like the askuser marker, `.iteration-pending.json` SHOULD be written via tempfile + atomic rename to avoid partial-write corruption. The pattern is in `mcp_askuser_server::write_marker` AND can be lifted directly.
- **Marker schema versioning.** The marker is daemon-internal AND not user-facing, but a schema version field (`"version": 1`) is cheap insurance. The implementer MAY add it now OR defer.
- **PR-comment rendering on iteration sequences.** Today's polling-loop posts an `## Agent implementation notes` comment per iteration. During an iteration sequence (no PR open yet), comments have nowhere to go. The implementer SHOULD route the iteration's `final_answer`-equivalent content (the reason field) into the per-change run log AND defer PR-comment posting to the first `Completed` iteration's PR-open step, where ALL prior iterations' summaries are combined into the initial PR body. This is the simplest narrative shape for the operator AND avoids the "comments on nonexistent PR" failure mode.
- **Workspace initialization for iteration-pending changes.** The polling loop's `workspace::ensure_initialized` step normally fetches the agent branch AND checks it out. For an iteration-pending change, the agent branch has the prior iteration's WIP commit on top. The existing initialization SHOULD do the right thing here (fetch + checkout puts the WIP commit in the working tree), but the implementer SHOULD verify this AND add a test asserting that the workspace's HEAD matches the agent branch's tip after initialization for an iteration-pending change.
