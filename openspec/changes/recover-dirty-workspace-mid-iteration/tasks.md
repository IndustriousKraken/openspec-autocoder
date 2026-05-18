## 1. Per-iteration auto-recovery

- [x] 1.1 In `polling_loop.rs`, factor the recovery sequence into a private helper `attempt_dirty_workspace_recovery(workspace: &Path, base_branch: &str) -> Result<()>` that runs (best-effort `git checkout <base>`) → `git::reset_hard_to_remote(workspace, base_branch)` → `git::clean_force(workspace)`. Mirrors the startup helper's exact sequence so behavior is uniform across the two recovery sites.
- [x] 1.2 In `run_pass_through_commits`, when the pre-pass dirty check finds non-empty `filter_alert_state_lines(...)`, log a WARN naming the dirty entry count and the recovery attempt, then call `attempt_dirty_workspace_recovery`. On success, re-run `git::status_porcelain` + `filter_alert_state_lines`. If clean, log INFO "workspace recovered mid-iteration; proceeding" and continue past the dirty check as if it had passed initially. If still dirty (or recovery itself errored), preserve the existing alert + return-Err behavior.
- [x] 1.3 Test `dirty_workspace_recovers_and_iteration_proceeds`: build a fixture that creates an `M`-state file in the workspace before invoking `run_pass_through_commits`. Assert (a) no `WorkspaceDirtyMidIteration` alert was posted, (b) the iteration completed normally (returned Ok), (c) the modified file's content matches origin (reset succeeded).
- [x] 1.4 Test `dirty_workspace_recovery_failure_still_alerts`: build a fixture where recovery itself fails (simulate by checking out a non-existent base branch, or by using a workspace whose origin remote points at a deleted path). Assert (a) `WorkspaceDirtyMidIteration` alert was posted, (b) the iteration returned Err.
- [x] 1.5 Test `dirty_workspace_persistent_after_recovery_alerts`: build a fixture where the dirt is something `git clean -fd` cannot remove (e.g. a gitignored file that's actually tracked — emulate by writing a `.gitignore` entry AFTER the file was committed). Assert the second status check still shows dirt → alert + Err.

## 2. Alert template cleanup

- [x] 2.1 In `alerts.rs::format_alert_text`, change the format string from `"⚠️ \`<repo>\`: <label> for the past 24h. Latest: <excerpt>"` to `"⚠️ \`<repo>\`: <label>. Latest: <excerpt>"`. Drop the misleading duration claim.
- [x] 2.2 Audit `alerts::tests` for any test asserting on the substring `"for the past 24h"` and update to assert on the new wording. The existing `repeat_within_24h_is_silent` and `beyond_24h_re_alerts_and_updates_state` tests are about throttle behavior and shouldn't depend on the format string text.
- [x] 2.3 Spot-check any other consumer of `format_alert_text` (e.g. polling-loop tests that match on alert text) and update assertions to the new format.

## 3. Spec deltas

- [x] 3.1 Add an ADDED requirement "Dirty workspace auto-recovers mid-iteration" to `orchestrator-cli` (next to the existing startup variant). Three scenarios: workspace dirty due to prior failed iteration (recovery succeeds; no alert), workspace remains dirty after recovery (alert + Err preserved), workspace already clean (no recovery commands run).
- [x] 3.2 Modify the "Mid-iteration dirty workspace alerts via chatops" scenario under `Iteration-level error tolerance` to specify the alert fires AFTER recovery has been attempted and failed (currently it specifies the alert fires immediately on dirty detection).

## 4. README documentation

- [x] 4.1 Update the "Dirty workspace auto-recovery" subsection in README's Operating Notes: today it only describes the startup case. Add a paragraph for the per-iteration case explaining that a failed executor's uncommitted residue is auto-recovered before the next iteration; the chatops alert appears only on the first occurrence (until the 24h throttle expires) or when recovery itself fails to clean the workspace.

## 4b. Latent bug uncovered while testing — perma-stuck marker gitignored

Implementation revealed that the existing pre-change code was tripping its OWN dirty check on per-change `.perma-stuck.json` markers (the test `removing_marker_re_enables_change` was masking this by discarding iteration errors). Pre-change behavior on perma-stuck: every subsequent iteration errored at the pre-pass dirty check and the polling loop sat idle until the operator deleted the marker. Post-change, the new recovery would have actively DELETED the operator's marker via `git clean -fd` — a much worse failure mode.

Complete fix: register `.perma-stuck.json` in `.git/info/exclude` inside `workspace::ensure_initialized`, alongside the existing `.failure-state.json` and `.audit-state.json` entries. Gitignored files are omitted from `git status --porcelain` (so the dirty check doesn't trip) AND preserved by `git clean -fd` (which only removes untracked-not-ignored), which gets both desired behaviors with one entry.

- [x] 4b.1 Add `ensure_git_info_excluded(workspace, ".perma-stuck.json")` to the existing chain in `workspace::ensure_initialized`. The pattern matches at any depth, covering the `openspec/changes/<change>/.perma-stuck.json` location.

## 5. Verification

- [x] 5.1 `cargo test` passes.
- [x] 5.2 `openspec validate recover-dirty-workspace-mid-iteration --strict` passes.
