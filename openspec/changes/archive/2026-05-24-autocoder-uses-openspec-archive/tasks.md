## 1. Switch queue::archive to subprocess

- [x] 1.1 In `autocoder/src/queue.rs::archive`, replace the `std::fs::rename(&src, &dst)` logic with a subprocess invocation: `std::process::Command::new("openspec").args(["archive", change, "-y"]).current_dir(workspace).output()`. Treat non-zero exit as Err with the openspec stderr as the error message; treat 0 exit as Ok.
- [x] 1.2 Keep the pre-flight `archive_collision_path` + `would_collide_on_archive` helpers — they remain valid (openspec archive aborts on date-collision too; pre-flight saves the executor invocation that produces the collision in the first place). The collision-check semantics don't change.
- [x] 1.3 Drop the now-unused imports in queue.rs (chrono::Utc, std::fs::rename if not used elsewhere in the file).
- [x] 1.4 Update existing tests in `queue::tests` that exercised the rename. Tests that asserted "archive moves a directory" become "archive invokes openspec successfully" — use a mock openspec via `PATH` shim OR (simpler) move those assertions to integration tests that require openspec on PATH and skip with `#[ignore]` otherwise. Unit-test what can be unit-tested (input validation, collision pre-flight); integration-test the actual archive end-to-end.

## 2. Remove the spec-sync audit + merge module

- [x] 2.1 `git rm autocoder/src/spec_sync.rs`.
- [x] 2.2 `git rm autocoder/src/audits/spec_sync.rs`.
- [x] 2.3 In `autocoder/src/audits/mod.rs`: remove `pub mod spec_sync;` line.
- [x] 2.4 In `autocoder/src/cli/run.rs`: remove the `use crate::audits::spec_sync::SpecSyncAudit;` import and the `registry.register(Arc::new(SpecSyncAudit::new()))` call.
- [x] 2.5 In `autocoder/src/config.rs::validate_audit_type_names`: remove `"spec_sync_audit"` from the recognized-slugs list.
- [x] 2.6 In `autocoder/src/audits/scheduler.rs` (or wherever `WritePolicy` lives): remove the `CanonicalSpecMerge` variant. Verify no other audit references it (`grep -rn CanonicalSpecMerge autocoder/src/` should return zero hits after deletion).
- [x] 2.7 Run `cargo build` and resolve any cascade-broken references (likely a handful of `use` statements that referenced the deleted module).

## 3. README updates

- [x] 3.1 Locate and remove the apologetic openspec-sync framing added in commit `085cb8d`. The current text presents `openspec config profile` as a workaround for an upstream bug; replace with a neutral prerequisite section:
  > "After installing the openspec CLI, run `openspec config profile` once on this host and enable the `Sync specs` workflow. autocoder's archive step shells out to `openspec archive`, which performs both the file move AND the merge of change deltas into canonical capability specs — but the merge step is only available when `sync` is enabled in the openspec profile. Without it, `openspec archive` will move the change directory but won't update canonical specs; autocoder iterations succeed but drift accumulates in `openspec/specs/`. To reconcile drift after the fact (e.g. for repos with pre-existing drift, or after onboarding a repo from a host that didn't have `sync` enabled), see the companion `rebuild-canonical-specs-from-archive` change."
- [x] 3.2 Remove the `spec_sync_audit` row from the audits table.
- [x] 3.3 Verify the README no longer contains the strings "OpenSpec is broken," "workaround for broken upstream," or similar apologetic framing related to sync.

## 4. config.example.yaml

- [x] 4.1 Remove the `spec_sync_audit` entry from the registered-audits comment list.
- [x] 4.2 Remove the `audits.settings.spec_sync_audit: {}` block.
- [x] 4.3 Final grep: `grep -n "spec_sync" config.example.yaml` returns zero hits.

## 5. Spec deltas

- [x] 5.1 In `openspec/changes/autocoder-uses-openspec-archive/specs/orchestrator-cli/spec.md`:
  - `## REMOVED Requirements` section listing `### Requirement: Archived-spec-sync audit` (the requirement added by the previous change that's now being rolled back).
  - `## ADDED Requirements` section with one new requirement:
    - **"autocoder invokes openspec archive for the archive step"** — the contract that autocoder shells out instead of doing its own file move; subprocess invocation form; error handling on openspec exit non-zero; the `openspec config profile` prerequisite for sync to work.

## 6. Verification

- [x] 6.1 `cargo test` passes (with deleted audit tests gone).
- [x] 6.2 `openspec validate autocoder-uses-openspec-archive --strict` passes.
- [ ] 6.3 Manual: run a single autocoder iteration in a test workspace where the host has openspec sync configured. Expect: the change directory moves to `openspec/changes/archive/<date>-<slug>` AND the corresponding `openspec/specs/<capability>/spec.md` is updated to merge the change's deltas, in one openspec archive subprocess call.
