## Why

Production incident on `coterie`: an executor invocation modified `openspec/changes/a06-refactor-portal-handlers-to-fromref/tasks.md` and `src/web/portal/admin/...`, then returned `Failed` (or timed out) without committing. The polling loop's `handle_outcome` unlocks the change but leaves tracked-file modifications in place. The NEXT iteration's pre-pass dirty check (`polling_loop::run_pass_through_commits` ~line 582) bails immediately with "workspace dirty before pass; refusing to proceed". The repo stalls until the operator manually deletes the workspace — and as soon as the next executor invocation fails the same way, the cycle repeats.

The startup path (`cli/run.rs::repo_passes_startup_check` ~line 594) already auto-recovers by running `git checkout <base>` + `git reset --hard origin/<base>` + `git clean -fd`. The per-iteration path doesn't — it just alerts and bails. That asymmetry is the entire reason an operator has to step in for what is otherwise a purely mechanical recovery (the agent branch is rebuilt from base each iteration via `recreate_branch` anyway, so wholesale wiping is safe).

A secondary nit: the alert template (`alerts.rs::format_alert_text`) hardcodes `"for the past 24h"` regardless of how long the failure has actually been ongoing. The phrase is the throttle-window display, not a measured duration — but it reads as a measurement. Recent operator confusion ("how can it be 24h if it's only been 20 minutes?") confirms this is misleading.

## What Changes

- **ADDED capability requirement** under `orchestrator-cli`: "Dirty workspace auto-recovers mid-iteration" mirroring the existing startup recovery. Per-iteration pre-pass dirty checks SHALL attempt `git checkout <base>` + `git reset --hard origin/<base>` + `git clean -fd` before falling back to the existing alert-and-bail behavior. Recovery success allows the iteration to proceed normally; only persistent dirt (filesystem read-only, file locks, gitignored state, etc.) reaches the alert path.
- **MODIFIED scenario** under `orchestrator-cli::Iteration-level error tolerance`: the "Mid-iteration dirty workspace alerts via chatops" scenario is updated to specify that the alert fires only AFTER recovery has been attempted and failed.
- **Code**:
  - `polling_loop::run_pass_through_commits`: when the pre-pass dirty check returns non-empty, attempt recovery in-place. Re-check; if still dirty, alert + return Err as today.
  - `alerts.rs::format_alert_text`: drop the misleading `"for the past 24h"` phrase. The new format is `"⚠️ <repo>: <label>. Latest: <excerpt>"`. The 24h throttle remains an implementation detail of `handle_predictable_failure`; operators don't need it in every alert.
  - Tests in `polling_loop::tests` for the new recovery: a fixture that pre-dirties the workspace, runs `run_pass_through_commits`, asserts the iteration proceeds AND no alert was posted. Companion test for "recovery still leaves the workspace dirty" → alert + Err preserved.
  - Tests in `alerts::tests` updated for the new wording.
- **Documentation**:
  - README "Operating Notes" → "Dirty workspace auto-recovery" subsection: extend the existing entry (which today only describes startup) to cover the per-iteration path.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement, one MODIFIED scenario).
- Affected code: `autocoder/src/polling_loop.rs` (recovery wiring + tests), `autocoder/src/alerts.rs` (template + test), `README.md` (one subsection).
- Operator-visible behavior change: a previously stuck repo will now self-heal on the iteration after a failed executor invocation. Operators get one alert (the first failure) and then the daemon recovers automatically; if the recovery doesn't work, the alert fires again at the 24h boundary. The `coterie`-style "5-6 iterations failed; delete workspace; same alert next iteration" loop disappears.
- Operator-visible cosmetic change: alert wording drops the misleading "for the past 24h" claim. The 24h throttle still applies; the operator just doesn't see a confusing duration claim in every alert body.
- Breaking: no. The alert is purely informational; downstream operators who pattern-match on the alert text would need to update their regex (unlikely to exist in practice).
- Acceptance: `cargo test` passes; new recovery test confirms a pre-dirtied workspace iterates normally without alerting; updated alert-format test confirms the new wording.
