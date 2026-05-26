## 1. Extract the shared archive helper

- [ ] 1.1 Create `autocoder/src/openspec_archive.rs` with the public surface:
  ```rust
  pub struct ArchiveRunOutput {
      pub status: std::process::ExitStatus,
      pub stdout: String,
      pub stderr: String,
  }
  pub enum ArchiveFailure {
      NonZeroExit { code: Option<i32>, stderr: String, stdout: String },
      AbortedMarker { reason: String, full_output: String },
      ActivePathStillPresent { path: PathBuf, full_output: String },
      NoArchiveEntryFound { full_output: String },
  }
  pub trait ArchiveRunner: Send + Sync {
      fn run(&self, workspace: &Path, slug: &str) -> Result<ArchiveRunOutput, String>;
  }
  pub struct RealArchiveRunner;  // shells out to `openspec archive <slug> -y`
  pub fn openspec_archive_with_postcondition(
      runner: &dyn ArchiveRunner,
      workspace: &Path,
      slug: &str,
  ) -> Result<PathBuf, ArchiveFailure>;
  ```
  The `ArchiveRunner` trait mirrors the pattern already used by `sync-specs-rebuild-atomicity` so tests can inject stubs without spawning real subprocesses.
- [ ] 1.2 `openspec_archive_with_postcondition` flow:
  1. Call `runner.run(workspace, slug)`.
  2. If exit non-zero → return `Err(ArchiveFailure::NonZeroExit { ... })`.
  3. Scan stdout for the `Aborted.` marker (use the existing `detect_openspec_abort` helper from `sync-specs-detect-aborted-output`; expose it publicly from `cli/sync_specs.rs` or move it into the new module). If matched → return `Err(ArchiveFailure::AbortedMarker { ... })`.
  4. Check `openspec/changes/<slug>/` — if it still exists → return `Err(ArchiveFailure::ActivePathStillPresent { ... })`.
  5. Glob `openspec/changes/archive/*-<slug>/` — if no match → return `Err(ArchiveFailure::NoArchiveEntryFound { ... })`.
  6. Otherwise return `Ok(matched_archive_path)`.
- [ ] 1.3 Move `detect_openspec_abort` from `cli/sync_specs.rs` to the new module. Re-export it from `cli/sync_specs.rs` for callers that still reference it (if any).
- [ ] 1.4 Tests for `openspec_archive_with_postcondition` using `MockArchiveRunner` stubs:
  - Happy path: runner returns exit 0 + clean stdout; fs fixture has the change moved to `archive/<today>-<slug>/`. Function returns `Ok(path_to_archive_entry)`.
  - Aborted marker: runner returns exit 0 + stdout containing `Aborted.` line preceded by a diagnostic line. Function returns `Err(AbortedMarker { reason: <diagnostic line>, .. })`.
  - Silent skip without marker: runner returns exit 0 + benign stdout; fs fixture has the change directory still at `changes/<slug>/`. Function returns `Err(ActivePathStillPresent { .. })`.
  - Data loss: runner returns exit 0 + benign stdout; fs fixture has neither the active path nor a matching archive entry. Function returns `Err(NoArchiveEntryFound { .. })`.
  - Non-zero exit: runner returns exit 1 + some stderr. Function returns `Err(NonZeroExit { code: Some(1), stderr: ..., stdout: ... })`.

## 2. Wire `queue::archive` through the helper

- [ ] 2.1 In `autocoder/src/queue.rs`, replace the direct `Command::new("openspec")` invocation in `pub fn archive` with a call to `openspec_archive_with_postcondition(&RealArchiveRunner, workspace, change)`. Map the structured `Err(ArchiveFailure)` to the existing `anyhow::Error` return type with a single message that names the failure variant and includes the openspec output excerpt (using the existing `truncate_for_report` cap from `sync_specs.rs`, or a parallel cap defined in the new module).
- [ ] 2.2 Failure-reason format strings:
  - `NonZeroExit` → `format!("openspec archive `{slug}` exited {code:?}: {stderr}")`
  - `AbortedMarker` → `format!("openspec archive `{slug}` aborted by openspec: {reason}; full output: {full}")`
  - `ActivePathStillPresent` → `format!("openspec archive `{slug}` reported success but the change directory at {path} still exists")`
  - `NoArchiveEntryFound` → `format!("openspec archive `{slug}` reported success but neither the active path nor any archive entry exists; full output: {full}")`
- [ ] 2.3 Tests:
  - `queue::archive` against a workspace where openspec succeeds: returns `Ok(())`, the change directory is now in archive.
  - `queue::archive` against a workspace where openspec emits `Aborted.`: returns `Err(...)` whose message starts with `openspec archive `slug` aborted by openspec:`.
  - Existing tests for `queue::archive` continue to pass (any that fixture the happy path through real subprocess invocation; if those are now testing through the trait, they get the `RealArchiveRunner`).

## 3. Self-heal failure-reason surfaces the new error

- [ ] 3.1 No change needed in `polling_loop.rs::execute_one_pass` (or wherever the self-heal call lives) — the existing `if let Err(e) = queue::archive(workspace, change)` block already formats the failure_reason as `format!("self-heal archive failed: {e:#}")`. With the new richer error message from `queue::archive`, the self-heal failure_reason becomes self-documenting.
- [ ] 3.2 Integration test in the self-heal path: stub `RealArchiveRunner` via a trait-object hook (or test the self-heal code path with a fixture that exercises the failure path). Assert the resulting `QueueStep::Failed { reason }`'s reason contains both `self-heal archive failed` and the openspec-supplied cause line.

## 4. Replace the rebuild loop's bespoke detection with the helper

- [ ] 4.1 In `cli/sync_specs.rs`, the rebuild loop currently performs its own marker detection + post-condition verification inline. Replace that with a call to `openspec_archive_with_postcondition` and map the structured failure to the existing `RebuildReport` failure entries. Behaviour is unchanged; the code path is now shared.
- [ ] 4.2 Tests:
  - All existing rebuild tests continue to pass.
  - The previously-bespoke abort-marker test is updated to drive through the helper (or kept as a regression test against the helper, since both now go through the same code).

## 5. `run_git` captures and surfaces stdout on failure

- [ ] 5.1 In `autocoder/src/git.rs`, update `run_git`'s failure path:
  ```rust
  if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
      let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
      let msg = match (stderr.is_empty(), stdout.is_empty()) {
          (false, _) if stdout.is_empty() => stderr,
          (false, false)                  => format!("stderr: {stderr}; stdout: {stdout}"),
          (true, false)                   => stdout,
          (true, true)                    => format!("(no output; exit {:?})", output.status.code()),
      };
      return Err(anyhow!("git {op} failed: {msg}"));
  }
  ```
- [ ] 5.2 Tests:
  - Existing tests that assert error messages from non-zero-exit + non-empty-stderr continue to pass.
  - New test: simulate `git commit` exit 1 with stdout `"nothing to commit, working tree clean"` and empty stderr (use a fixture workspace where every change is already committed). Assert the error contains `"nothing to commit"`.
  - New test: both stderr and stdout populated → error contains both prefixed with `stderr:` and `stdout:`.
  - New test: both empty → error contains `(no output; exit ...)`.
- [ ] 5.3 The git stdout helper change is a generic improvement that incidentally surfaces the self-heal "nothing to commit" case. It is included in this spec because the perma-stuck loop above requires both halves (helper + stdout capture) to fully surface the cause; shipping only the helper would still leave operators with `"self-heal git commit failed: "` if openspec ever fails in a way the helper misses.

## 6. Spec delta

- [ ] 6.1 The ADDED requirement in `openspec/changes/queue-archive-aborted-detection/specs/orchestrator-cli/spec.md` codifies: the shared archive-with-postcondition contract (the four failure variants and their detection rules), the failure-reason format rules at every caller, and the `run_git` stdout-inclusion rule.

## 7. Verification

- [ ] 7.1 `cargo test` passes (new + existing).
- [ ] 7.2 `openspec validate queue-archive-aborted-detection --strict` passes.
- [ ] 7.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
