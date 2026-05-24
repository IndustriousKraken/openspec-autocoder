## 1. CLI subcommand scaffolding

- [ ] 1.1 Create `autocoder/src/cli/sync_specs.rs` with `pub async fn execute(args: SyncSpecsArgs) -> Result<()>` and `pub struct SyncSpecsArgs { workspace: PathBuf, rebuild: bool, immediate: bool }`. `rebuild` defaults to true (it's the only mode for v1, but the flag exists for clarity and future-proofing).
- [ ] 1.2 Add `SyncSpecs(SyncSpecsArgs)` variant to the clap enum in `autocoder/src/main.rs` and wire dispatch to `cli::sync_specs::execute`.
- [ ] 1.3 Validate args at entry: `workspace` must exist and contain `openspec/changes/archive/`; otherwise exit non-zero with a clear error.

## 2. Rebuild orchestration

- [ ] 2.1 In `cli/sync_specs.rs`, implement `pub async fn rebuild_canonical(workspace: &Path) -> Result<RebuildReport>` exposing the core logic so both the CLI subcommand AND the polling-loop hook use the same implementation.
- [ ] 2.2 `RebuildReport` captures: total archived changes processed, successful, failed (with per-failure reason), and the list of canonical spec files that ended up modified vs unchanged after rebuild. Used by the CLI to print a summary and by the polling loop to construct a meaningful commit message.
- [ ] 2.3 Implementation steps in `rebuild_canonical`:
  1. Enumerate `<workspace>/openspec/changes/archive/*/` directories. Sort by name (ascending; chronological because of the YYYY-MM-DD prefix). Filter to actual directories (skip dot-files, the `archive` subdir if any nesting weirdness).
  2. Clear: `rm -rf <workspace>/openspec/specs/*/` for every capability subdirectory. Preserve `openspec/specs/` itself (the parent dir).
  3. For each archived change in order:
     - `original_name = entry.file_name()` (e.g. `2026-05-15-foo-bar`).
     - `slug = strip_date_prefix(original_name)` (regex `^\d{4}-\d{2}-\d{2}-(.+)$`, capture group 1).
     - `fs::rename(archive/original_name, changes/slug)`.
     - Subprocess: `std::process::Command::new("openspec").args(["archive", slug, "-y"]).current_dir(workspace).output()`. Capture stdout + stderr + exit code.
     - On exit 0:
       - openspec just placed the change at `archive/<today>-<slug>`. Compute that name from `Utc::now()`.
       - `fs::rename(archive/<today>-<slug>, archive/original_name)` to preserve the original date prefix.
       - Record success in the report.
     - On non-zero exit:
       - Leave the change at the active path (`openspec/changes/<slug>`). Operator can decide whether to manually restore or fix the spec deltas and retry.
       - Record failure in the report with the openspec stderr (truncated to a reasonable size).
       - Continue to the next change.
  4. After the loop: walk `openspec/specs/` post-rebuild and record which files exist (so the report's "modified vs unchanged" assertion is computable).
  5. Return the `RebuildReport`.
- [ ] 2.4 Pure-function helpers (extract for unit testability):
  - `fn strip_date_prefix(name: &str) -> Result<&str>` — `^\d{4}-\d{2}-\d{2}-` regex strip; Err on names that don't match.
  - `fn today_dated_name(slug: &str) -> String` — formats `<UTC YYYY-MM-DD>-<slug>` matching openspec's convention.

## 3. CLI summary output

- [ ] 3.1 The CLI subcommand prints a human-readable report after `rebuild_canonical` returns:
  ```
  Rebuild complete.

  Processed: N changes (in chronological order)
  Successful: M
  Failed:     N-M
  
  [if any failed:]
  Failures:
    - 2026-05-15-foo-bar: <openspec stderr first line>
    - 2026-05-18-baz: <openspec stderr first line>
  
  Canonical specs:
    - openspec/specs/orchestrator-cli/spec.md  (modified)
    - openspec/specs/workspace-manager/spec.md (modified)
    - openspec/specs/git-workflow-manager/spec.md (unchanged)
    [...]
  ```
- [ ] 3.2 Exit 0 if `report.failed == 0`; exit non-zero (e.g. 1) otherwise. Makes the subcommand CI-friendly.

## 4. --immediate flag handling

- [ ] 4.1 When `--immediate` is set AND a daemon is running on this workspace: send SIGTERM to the executor subprocess via the busy marker's recorded pid. Wait up to 30 seconds for the busy marker to be released. If still held after 30s, log a WARN and proceed anyway (the rebuild's first `git status` check + dirty-workspace recovery will clean up any partial state from the killed iteration).
- [ ] 4.2 When `--immediate` is NOT set AND a daemon is running: wait politely for the busy marker to be released (poll every few seconds). Log progress so the operator sees what's happening. The CLI blocks until the iteration finishes.
- [ ] 4.3 When NO daemon is running (no busy marker file exists): both modes proceed immediately to the rebuild. `--immediate` is a no-op in this case.

## 5. Control-socket action

- [ ] 5.1 Add `RebuildSpecs { url: String, immediate: bool }` variant to the control socket's action enum.
- [ ] 5.2 Handler: resolve the repo's workspace path from the configured `repositories[]` map via the url. If url doesn't match any configured repo, return an error message.
- [ ] 5.3 If `immediate`: invoke the same SIGTERM-and-wait logic as the CLI's `--immediate` path, then call `rebuild_canonical(workspace)`. Return the report (or its summary) as the action response.
- [ ] 5.4 If not immediate: set `pending_rebuild = true` on the named repo's polling task state. The action response is "rebuild scheduled" (with timing estimate based on `poll_interval_sec`).

## 6. Polling-loop coordination

- [ ] 6.1 Add `pending_rebuild: bool` to the per-repo polling task state (the same state struct that already holds the busy marker, cancellation token, etc.).
- [ ] 6.2 At iteration start (after the busy marker is acquired, before the queue walk): check `pending_rebuild`. If true:
  - Clear the flag.
  - Log INFO `"iteration: running spec rebuild instead of queue walk"`.
  - Call `cli::sync_specs::rebuild_canonical(workspace)`.
  - Stage all changes (`git add -A`), commit with message `spec rebuild: <N> capability(ies) rebuilt from archive history` (or "0 capability(ies) — no drift detected" if no files changed).
  - If any commits were produced, the existing push + PR creation logic runs as normal. PR title uses the existing `informative-pr-title-and-body` logic; the commit message + the rebuild's distinctive shape gives operators a clear cue.
  - Skip the queue walk entirely for this iteration. The next iteration resumes normal queue processing.
- [ ] 6.3 If `pending_rebuild` is false (the normal case): iteration proceeds with queue walk as today. No behavior change.
- [ ] 6.4 Test: a polling-loop fixture with `pending_rebuild = true` set before the iteration starts; assert the iteration calls `rebuild_canonical` once, calls the existing commit/push hooks, does NOT invoke the executor, and clears the flag before returning.

## 7. Chatops verb

- [ ] 7.1 In `autocoder/src/chatops/operator_commands.rs` (from the `chatops-operator-commands` change): add `OperatorCommand::RebuildSpecs { repo_substring: String }` variant and parser support for `@<bot> rebuild-specs <repo-substring>`.
- [ ] 7.2 Handler in the chatops listener: resolve repo via `match_repo`. On unique match: submit `RebuildSpecs { url, immediate: false }` to the control socket (chatops never triggers `immediate`). On multiple/no match: reply with the disambiguation message per the existing pattern.
- [ ] 7.3 Reply formatting:
  - Success: `✓ rebuild scheduled for <repo> — will run within ~Ns (current iteration must finish first)`.
  - Errors: per the existing `✗ <message>` shape.
- [ ] 7.4 Update the `chatops-operator-commands` requirement's "Pause / resume / clear-alert-throttle are deliberately absent" scenario list to acknowledge that `rebuild-specs` is now a recognized verb. This is a downstream spec-edit concern; the cleanest approach is to file a tiny spec-delta updating the canonical orchestrator-cli requirement to add `rebuild-specs` to the verb list.

## 8. README updates

- [ ] 8.1 Add a new subsection under "Operating Notes" titled "Rebuilding canonical specs from archive history." Content:
  - When to use rebuild (drift detected after onboarding a new repo, after a host without `openspec sync` enabled archived changes, etc.).
  - The CLI invocation: `autocoder sync-specs --rebuild --workspace <path>` for operator local clones.
  - The chatops invocation: `@<bot> rebuild-specs <repo>` for daemon-managed repos. Notes that this schedules for after the current iteration finishes.
  - The `--immediate` flag (CLI only): describes the disruption (SIGTERM the running executor) and why it's not exposed via chatops.
  - Caveat: rebuild discards any hand-edited canonical content (preamble Purpose paragraphs are reset to placeholders by openspec; manually-added requirements without archive sources are lost). Review the rebuild PR's diff before merging.
- [ ] 8.2 In the chatops verb reference table (added by `chatops-operator-commands`): add a row for `rebuild-specs` with a one-line description.

## 9. Spec delta

- [ ] 9.1 Author the ADDED requirement under `orchestrator-cli` titled "Rebuild canonical specs from archive." Scenarios cover:
  - Basic rebuild from a workspace with drift produces correct canonical state
  - Rebuild on a clean repo (no drift) is a noop diff
  - Date prefixes preserved via in-place rename
  - Per-change openspec archive failure logs + continues; non-zero exit
  - Chatops verb schedules; polling loop runs at next iteration boundary
  - `--immediate` SIGTERMs the running executor; rebuild proceeds after the iteration cleans up
  - Without `--immediate`, CLI blocks waiting for current iteration to finish

## 10. Verification

- [ ] 10.1 `cargo test` passes.
- [ ] 10.2 `openspec validate rebuild-canonical-specs-from-archive --strict` passes.
- [ ] 10.3 Integration test in `cli::sync_specs::tests` (gated with `#[ignore]` so it runs only when explicitly opted into via `cargo test -- --ignored`, OR runs unconditionally with a `which openspec` check that skips with a clear message if openspec isn't on PATH). The test builds a fixture workspace via `tempfile::TempDir`: minimal `openspec/specs/<cap>/spec.md` baseline + two archived changes whose deltas (`## ADDED` requirements) are NOT yet present in canonical. Calls `rebuild_canonical(workspace)`. Asserts: (a) returned `RebuildReport.failed == 0`; (b) the canonical spec at `openspec/specs/<cap>/spec.md` now contains both ADDED requirements; (c) the archive directory entries still have their original date prefixes (the in-place rename restored them); (d) exit through the CLI path also returns 0. This test is autocoder's e2e — replaces the prior "manual run on this repo" instruction with something `cargo test` actually exercises.
