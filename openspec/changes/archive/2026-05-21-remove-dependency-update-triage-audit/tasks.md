## 1. Verify orphan analysis before any deletion

- [x] 1.1 Run `cargo check --tests 2>&1 | grep -E "never used|unused"` BEFORE making any changes; capture the baseline. The dependency-update audit's helpers should NOT be in this list (because they're currently used by the audit). Run again AFTER §2.1 deletes the audit file and BEFORE §3 deletes the github.rs helpers — anything newly-flagged as "never used" is in scope for removal in §3. Anything still in use (e.g. `PullRequestSummary` if some other code path picks it up) is NOT in scope and stays.

## 2. Delete the audit code

- [x] 2.1 Delete `autocoder/src/audits/dependency_update.rs` entirely.
- [x] 2.2 In `autocoder/src/audits/mod.rs`, remove the `pub mod dependency_update;` line.
- [x] 2.3 In `autocoder/src/cli/run.rs`:
  - Remove `dependency_update::DependencyUpdateAudit` from the multi-import `use crate::audits::{ ... };` block at the top of the file.
  - Remove the `registry.register(Arc::new(DependencyUpdateAudit::new(&audit_settings, cfg.github.clone())));` call (the only registration of this audit type).
- [x] 2.4 `cargo check --tests` and confirm no compile errors. Expect newly-flagged dead-code warnings on the github.rs helpers that only this audit consumed — those become §3's removal list.

## 3. Delete orphaned github.rs helpers

- [x] 3.1 Remove the following functions and their `_at` / `_at_for_test` siblings from `autocoder/src/github.rs`, plus the tests under `#[cfg(test)] mod tests {}` that exercise them. Verify each is unused by anything else before deletion (a `grep -rn '<function-name>' src/` should show only the function definition's own line):
  - `list_open_prs_by_author`
  - `fetch_pr_diff`
  - `list_pr_reviews`
  - `approve_pr`
- [x] 3.2 Remove the deserialization-only types `PullRequestSummary`, `PullRequestReview`, `RawPullSummary`, `RawUser`, `RawReview` IF AND ONLY IF they have no other usages anywhere in `autocoder/src/`. Spot-check the same way: `grep -rn 'PullRequestSummary\|PullRequestReview\|RawPullSummary\|RawUser\|RawReview' src/` — if hits exist outside `github.rs` definitions and dependency_update.rs (which is already deleted), preserve the type.
- [x] 3.3 `cargo check --tests` clean; `cargo clippy 2>&1 | grep "never used"` returns no hits in github.rs (or only pre-existing ones).

## 4. README updates

- [x] 4.1 Remove `dependency_update_triage` from the list of registered audit slugs in the `audits.defaults` row (currently around line 142).
- [x] 4.2 In the `extra` knobs paragraph (around line 155), remove the `dependency_update_triage reads max_approvals_per_run (u32, default 5) and fork_remote_name (string, default "fork") from here.` clause. Leave the surrounding text and the other audits' descriptions.
- [x] 4.3 Delete the entire `dependency_update_triage` row from the audit overview table (around line 724).
- [x] 4.4 In the example YAML snippet (around line 742), remove the `dependency_update_triage: daily` line.
- [x] 4.5 In the example YAML snippet (around line 752), remove the `dependency_update_triage:` block (the entire block including its nested keys).
- [x] 4.6 Final grep: `grep -n 'dependency_update_triage\|DependencyUpdateAudit\|dependabot' README.md` should return zero hits. If hits remain, remove them or update them to factual text (e.g. a note in a future "removed features" section if you choose to add one).

## 5. config.example.yaml updates

- [x] 5.1 Remove the per-repo `dependency_update_triage: daily` line under the commented `repositories[].audits:` example (around line 39).
- [x] 5.2 Remove the `- dependency_update_triage   — opens a change per ready dependabot PR` bullet from the registered-audits comment (around line 216).
- [x] 5.3 Remove the `dependency_update_triage: daily` line from the commented `audits.defaults:` example (around line 234).
- [x] 5.4 Remove the entire commented `dependency_update_triage:` block from `audits.settings:` (around lines 251–255), including its `extra.max_approvals_per_run` nested entry.
- [x] 5.5 Final grep: `grep -n 'dependency_update_triage\|dependabot' config.example.yaml` should return zero hits.

## 6. Operator-visible breakage release note

- [x] 6.1 The commit message MUST call out the breaking change: any operator with `audits.defaults.dependency_update_triage` or `audits.settings.dependency_update_triage` or `repositories[].audits.dependency_update_triage` configured will hit the existing `validate_audit_type_names` failure at startup ("nonexistent_audit_xyz") with this slug. Operators must remove the entries before redeploying. Suggested commit-body language:

  > BREAKING: operators with `dependency_update_triage` configured under `audits.defaults`, `audits.settings`, or `repositories[].audits` must remove those entries before redeploying — startup config validation will reject the now-unregistered slug. The audit was never functional in fork-PR mode (the fork doesn't host Dependabot PRs) and its approval action was theatre regardless of mode; the right shape for future dependency automation is a maximalist "apply bump + run tests + open our own PR" feature, not retrofitting this audit.

## 7. Spec delta

- [x] 7.1 Author the ADDED requirement under `orchestrator-cli` per the proposal: "Registered periodic audits" — enumerate the five remaining audits (`architecture_brightline`, `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`) and explicitly note that `dependency_update_triage` is NOT registered. Include a scenario stating that future changes adding/removing an audit MUST update this enumeration in the same commit.

## 8. Verification

- [x] 8.1 `cargo test` passes — expect the count to drop by the dependency-update audit's tests (~13 unit + 6 integration). The `DependencyUpdateAudit` symbol must not appear in any test output.
- [x] 8.2 `openspec validate remove-dependency-update-triage-audit --strict` passes.
- [x] 8.3 `grep -rn 'dependency_update_triage\|DependencyUpdateAudit\|dependabot' autocoder/src/ README.md config.example.yaml` returns zero hits.
- [x] 8.4 The previously-registered audit count in startup logs drops by one. Optional: spot-check by running `autocoder run --help` (or whatever closest dry-startup is available) and confirming no panic.
