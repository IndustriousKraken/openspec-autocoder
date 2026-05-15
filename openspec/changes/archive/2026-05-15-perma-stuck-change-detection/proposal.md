## Why

When an agent runs against a change and consistently fails the same way (e.g. it refuses a task it cannot perform from the sandbox, or hits a spec contradiction it cannot resolve), the current daemon re-queues the change and retries on every poll. The same agent makes the same decision, fails for the same reason, and the loop burns Claude tokens forever — across 9 repos at 30-minute intervals that is meaningful spend on what is effectively a dead-end. The operator has no signal that human attention is required; the only sign is the journalctl log noise.

A consecutive-failure counter per change with a low threshold (default 2) plus a chatops escalation cuts this off cleanly: after two failures the daemon parks the change, alerts via chatops, and refuses to retry until a human clears the marker.

## What Changes

- **ADDED capability:** `orchestrator-cli` SHALL maintain a per-repo `.failure-state.json` file recording consecutive-failure counts per change. After each Failed outcome the counter for that change is incremented; after each Archived outcome the counter is cleared. When a change's counter reaches the configured threshold (default 2), autocoder writes a `.perma-stuck.json` marker into the change directory, posts a chatops alert (best-effort), and excludes the change from `list_pending` on subsequent passes until the marker is manually removed.
- **MODIFIED capability:** `openspec-queue-engine`'s `list_pending` SHALL exclude change directories containing a `.perma-stuck.json` marker file (in addition to its existing exclusions for `.in-progress`, `.question.json`, and `archive/`).
- **Config:**
  ```yaml
  executor:
    perma_stuck_after_failures: 2   # default; minimum 1
  ```
- **`.failure-state.json` schema** (in workspace root, gitignored just like `.alert-state.json`):
  ```json
  {
    "<change-name>": {
      "count": 1,
      "last_reason": "...",
      "last_failed_at": "RFC 3339 UTC timestamp"
    }
  }
  ```
- **`.perma-stuck.json` schema** (inside the change directory):
  ```json
  {
    "change": "<change-name>",
    "consecutive_failures": 2,
    "last_reason": "...",
    "marked_stuck_at": "RFC 3339 UTC timestamp",
    "operator_action": "Delete this file to retry the change."
  }
  ```
- **Counter semantics:**
  - Only "executor returned `Failed`" and "Completed-with-empty-workspace transformed to Failed by self-heal/no-op-completion logic" increment the counter. Daemon-side errors before executor invocation (workspace init failure, openspec preflight failure, transport errors talking to GitHub) do NOT increment, since those are transient.
  - On Archived (including self-heal), the counter for that change is cleared.
  - On the threshold transition (count == threshold), autocoder writes `.perma-stuck.json` and posts the chatops alert.
- **Chatops alert** (best-effort, 24h-throttled like the existing failure alerts):
  ```
  :no_entry: autocoder: change perma-stuck
  repo: <repo url>
  change: <change name>
  consecutive_failures: 2
  last_reason: <truncated to 200 chars>
  
  This change has failed two iterations in a row. autocoder will not retry until an operator removes /tmp/workspaces/<basename>/openspec/changes/<change>/.perma-stuck.json.
  ```
- **No chatops backend configured:** the marker is still written and the change is still excluded from `list_pending`. An ERROR log line replaces the missing chatops notification.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement), `openspec-queue-engine` (one MODIFIED scenario in `list_pending`).
- Affected code: `autocoder/src/polling_loop.rs` (counter increment/clear sites + threshold check), `autocoder/src/queue.rs` (`list_pending` exclusion), `autocoder/src/config.rs` (new optional field).
- New file in each affected workspace: `.failure-state.json`. Should be added to `.git/info/exclude` at workspace init the same way `.alert-state.json` is.
- Marker file `.perma-stuck.json` lives inside the change directory. When the operator deletes it (or runs the planned `autocoder unstick <change>` subcommand) the change re-enters `list_pending` on the next poll, and the counter is reset (because the marker's removal is the operator's signal that they intend to retry from scratch).
- Token cost reduction: a perma-stuck change costs one pass to detect + one alert. Compare to the current cost of one Claude run per poll, indefinitely.
- Edge case: if an operator deletes `.perma-stuck.json` without fixing the underlying issue, autocoder will retry, fail twice more, mark perma-stuck again. The chatops alert is throttled to once per 24h so the operator does not get spammed during a fix-test-fail cycle.
