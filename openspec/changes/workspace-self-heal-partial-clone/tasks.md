## 1. Safety check for auto-cleanup

- [ ] 1.1 In `autocoder/src/workspace.rs`, add `fn safe_to_auto_clean(workspace: &Path) -> Result<(), &'static str>`:
  ```rust
  fn safe_to_auto_clean(workspace: &Path) -> Result<(), &'static str> {
      // Tripwire 1: in-progress lock files at any depth
      if walk_for_filename(workspace, ".in-progress").any() {
          return Err("contains .in-progress lock file");
      }
      // Tripwire 2: operator-meaningful markers under openspec/changes/
      let changes_root = workspace.join("openspec/changes");
      if changes_root.is_dir() {
          for entry in WalkDir::new(&changes_root) {
              let path = entry.path().file_name();
              if matches!(path, Some(n) if n == ".perma-stuck.json" || n == ".needs-spec-revision.json") {
                  return Err("contains .perma-stuck.json or .needs-spec-revision.json marker");
              }
              if matches!(path, Some(n) if n == ".question.json" || n == ".answer.json") {
                  return Err("contains AskUser .question.json or .answer.json marker");
              }
          }
      }
      Ok(())
  }
  ```
  (`WalkDir` is the `walkdir` crate; add it to Cargo.toml after verifying current version per `check-current-versions-not-training`.)
- [ ] 1.2 The function returns Ok if and only if the directory is structurally a partial-clone artifact with no operator-meaningful state. The `.alert-state.json` at the workspace root is explicitly NOT a tripwire (daemon-written, not operator-meaningful).
- [ ] 1.3 Tests:
  - Empty directory → Ok (nothing to protect).
  - Directory with only `.alert-state.json` at root → Ok (alert-state is daemon-written).
  - Directory with `openspec/changes/foo/proposal.md` (partial-clone artifact, no markers) → Ok.
  - Directory with `openspec/changes/foo/.perma-stuck.json` → Err naming the marker.
  - Directory with `openspec/changes/foo/.needs-spec-revision.json` → Err.
  - Directory with `openspec/changes/foo/.question.json` → Err.
  - Directory with `.in-progress-bar` at root → Err.

## 2. Wire auto-cleanup into ensure_initialized

- [ ] 2.1 In `autocoder/src/workspace.rs::ensure_initialized` (or wherever the "exists but no .git" detection lives), at the detection point:
  ```rust
  if workspace.is_dir() && !workspace.join(".git").is_dir() {
      match safe_to_auto_clean(workspace) {
          Ok(()) => {
              tracing::warn!(
                  workspace = %workspace.display(),
                  repo = %repo.url,
                  "workspace exists without .git; partial clone artifact detected. Deleting and re-cloning."
              );
              std::fs::remove_dir_all(workspace).with_context(|| {
                  format!("auto-cleanup of partial workspace at {} failed", workspace.display())
              })?;
              // Fall through to the normal clone path
          }
          Err(tripwire) => {
              return Err(anyhow!(
                  "workspace path exists but is not a git repository (no .git directory): {} \
                   (partial cleanup refused: {tripwire}; manual operator inspection required)",
                  workspace.display()
              ));
          }
      }
  }
  ```
- [ ] 2.2 After the auto-cleanup branch, fall through to the existing clone code path. The clone runs as if the workspace didn't exist. Its success/failure is reported normally.
- [ ] 2.3 If the clone after auto-cleanup ALSO fails, the returned error contains the real clone error from the git operation — NOT a recursive "exists but no .git" detection (because the directory was just deleted, the clone failure now has whatever git's actual stderr was).
- [ ] 2.4 If `fs::remove_dir_all` itself fails (permissions, disk full), the `with_context` wrapper provides a clear error message naming the workspace path. The iteration fails as today; recovery requires operator intervention.
- [ ] 2.5 Tests (using a fixture workspace + a controllable clone stub OR a real git clone against a tiny fixture repo):
  - Fixture: workspace dir exists with `openspec/` subdir but no `.git/`; stub clone succeeds → auto-cleanup runs (WARN captured), re-clone runs, `ensure_initialized` returns Ok.
  - Fixture: workspace dir exists with markers → auto-cleanup refused, Err returned with the partial-cleanup-refused hint, no `fs::remove_dir_all` called.
  - Fixture: workspace dir exists no `.git/`; stub clone fails with `auth failed` → auto-cleanup runs, re-clone runs, returned Err contains `auth failed` (not "exists but no .git").
  - Fixture: workspace doesn't exist at all → auto-cleanup path is NOT entered (the existing happy-path clone fires directly).
  - Fixture: workspace exists with valid `.git/` → auto-cleanup path is NOT entered (the existing fetch+pull path fires).

## 3. Iteration-level no-op for the auto-cleanup path

- [ ] 3.1 No changes needed in the polling iteration. The auto-cleanup is internal to `workspace::ensure_initialized`; from the iteration's perspective, the function either returns Ok (workspace is ready) or Err (workspace init failed). The Err path's reason text is now the real underlying error rather than a misleading secondary detection.
- [ ] 3.2 The chatops alert under `workspace_init_failure` category (when it fires) now carries the real clone error in its `last_error_excerpt` field — operator triage points at the actual cause (auth, network) rather than the secondary symptom.
- [ ] 3.3 Test: end-to-end iteration against a fixture where auto-cleanup is exercised; assert the iteration's reported outcome is normal Completed (not Failed with a recovery side-note).

## 4. Docs updates

- [ ] 4.1 In `docs/OPERATIONS.md`'s workspace section (or wherever workspace lifecycle is documented), add a paragraph describing the auto-cleanup behaviour and its safety-check tripwires. Operators benefit from knowing what triggers auto-cleanup and what would prevent it.
- [ ] 4.2 In `docs/TROUBLESHOOTING.md`, replace any entry recommending manual `rm -rf` for the "exists but no .git" case with a note that the daemon now auto-recovers, AND a section on what to do when the safety check refuses auto-cleanup (operator-inspects the workspace, decides whether to manually wipe or to preserve the marker state and address the underlying clone failure).

## 5. Spec delta

- [ ] 5.1 The ADDED requirement in `openspec/changes/workspace-self-heal-partial-clone/specs/workspace-manager/spec.md` codifies: the partial-clone detection trigger, the safety-check tripwires (in-progress locks, perma-stuck markers, needs-spec-revision markers, AskUser markers), the auto-cleanup + re-clone sequence, the safety-check-refused error format, and the iteration-outcome reporting rule.

## 6. Verification

- [ ] 6.1 `cargo test` passes (new + existing).
- [ ] 6.2 `openspec validate workspace-self-heal-partial-clone --strict` passes.
- [ ] 6.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
