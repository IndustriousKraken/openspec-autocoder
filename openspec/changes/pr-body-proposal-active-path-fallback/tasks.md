## 1. Rename + extend the proposal-reader

- [ ] 1.1 In `autocoder/src/polling_loop.rs`, rename `read_archived_why` → `read_change_why`. Update the one caller (`build_pr_body` at `polling_loop.rs:2750`).
- [ ] 1.2 Extend `read_change_why` body to perform a two-step lookup:
  ```rust
  fn read_change_why(workspace: &Path, change: &str) -> Option<String> {
      // Step 1: archive path (existing behaviour)
      if let Some(why) = read_proposal_why_from_archive(workspace, change) {
          return Some(why);
      }
      // Step 2: active path fallback
      let active = workspace.join("openspec/changes").join(change).join("proposal.md");
      if active.is_file() {
          let raw = std::fs::read_to_string(&active).ok()?;
          if let Some(why) = extract_why_section(&raw) {
              tracing::warn!(
                  change = %change,
                  "proposal read from active path, not archive — likely indicates an upstream archive failure for this iteration"
              );
              return Some(why);
          }
      }
      None
  }
  ```
- [ ] 1.3 Extract the archive-lookup half into a private `fn read_proposal_why_from_archive(workspace, change) -> Option<String>` so each lookup path is independently testable. This is the body of the current `read_archived_why` minus the new fallback wrapper.
- [ ] 1.4 Tests (extend the existing `read_archived_why` tests):
  - Archive path populated → returns `Some(why)`, NO WARN log captured.
  - Archive path missing AND active path populated with valid `## Why` → returns `Some(why)`, exactly one WARN log captured naming the change.
  - Archive path missing AND active path populated but proposal has no `## Why` section → returns `None`, no WARN log (the WARN fires only when the fallback actually surfaces content).
  - Both paths missing → returns `None`, no WARN log.
  - Both paths populated → archive path wins (deterministic preference for the archive lookup), no WARN log.
  - Capture WARN logs via `tracing-test` (already in `dev-dependencies`) using `traced_test` macro.

## 2. Spec delta

- [ ] 2.1 The ADDED requirement in `openspec/changes/pr-body-proposal-active-path-fallback/specs/orchestrator-cli/spec.md` codifies: the two-step lookup order, the active-path fallback semantics, the WARN-log obligation when the fallback succeeds, and the no-WARN policy when both paths miss or when the active path has a file but no `## Why` section.

## 3. Verification

- [ ] 3.1 `cargo test` passes (new + existing).
- [ ] 3.2 `openspec validate pr-body-proposal-active-path-fallback --strict` passes.
- [ ] 3.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
