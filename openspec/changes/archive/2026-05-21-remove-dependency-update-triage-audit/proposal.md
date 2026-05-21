## Why

The `dependency_update_triage` audit was built on a premise that doesn't survive scrutiny:

1. **The approval action is theatre in any realistic operator setup.** Granting a bot merge rights on a production repo is something a careful operator shouldn't and won't do. Without merge rights, an autocoder "approval" on a Dependabot PR is just a comment a human reviewer reads and either trusts or doesn't. If they trust autocoder's read of the diff they're not adding value over autocoder. If they don't, the LGTM is dangerous noise — it anchors the human toward approve on something they should be scrutinizing themselves.
2. **The "flag the weird ones" half of the audit (chatops findings for unsafe diffs) automates what a careful human reviewer would catch anyway.** The structural filter (no script hooks, no URL changes, no new top-level entries) is a thin slice of what makes a dependency PR risky. The deeper failure mode — a subtle behavior change in a minor-version bump that compiles, passes CI, and breaks production — is invisible to both this filter AND to GitHub Actions' default test runs. Neither structural triage nor CI catches it; only human judgment does.
3. **The audit is the only surface in autocoder that bypasses human-reviewed-PR-as-the-output.** Every other path — change implementation, self-heal, rewind, fork recreation — produces a PR for a human to merge. This audit alone interacts with GitHub by writing approval reviews directly. That inconsistency is a smell; the audit's design doesn't match the project's design philosophy.
4. **Production deployment surfaced a separate bug:** in fork-PR mode the audit queries `<fork-owner>/<repo>` for Dependabot PRs, but Dependabot is installed on the upstream account and opens PRs there. The audit reports "no findings" while the operator has five queued Dependabot PRs against upstream. We diagnosed this and considered fixing it before concluding the underlying audit is the wrong shape regardless.

Removing the audit is cleaner than fixing the bug and continuing to maintain code we wouldn't enable in the first place. If dependency automation becomes desirable later, the right shape is the maximalist version — autocoder owns the bump, applies it on its own branch, runs tests, and opens its own human-reviewed PR. That's a separate, future feature; building it from scratch would be cleaner than retrofitting this audit.

## What Changes

- **ADDED requirement** under `orchestrator-cli`: "Registered periodic audits" enumerating the current audit registry membership and explicitly excluding `dependency_update_triage`. The requirement also pins the contract that future changes adding or removing an audit MUST update this enumeration. This is the first contractual statement about the audit registry in the canonical spec (the audit framework's `a01`–`a06` archived changes never propagated their spec deltas to the canonical `orchestrator-cli/spec.md`, so there is no existing "Dependency update triage audit" requirement to REMOVE here).
- **Code deletions:**
  - `autocoder/src/audits/dependency_update.rs` — entire file.
  - `autocoder/src/audits/mod.rs` — `pub mod dependency_update;` declaration.
  - `autocoder/src/cli/run.rs` — the `dependency_update::DependencyUpdateAudit` import and the corresponding `registry.register(Arc::new(DependencyUpdateAudit::new(...)))` call.
  - `autocoder/src/github.rs` — orphaned helper functions and their tests/types that exist only for this audit:
    - `list_open_prs_by_author` + `_at` + `_at_for_test`
    - `fetch_pr_diff` + `_at` + `_at_for_test`
    - `list_pr_reviews` + `_at` + `_at_for_test`
    - `approve_pr` + `_at` + `_at_for_test`
    - Types: `PullRequestSummary`, `PullRequestReview`, the corresponding `RawPullSummary`/`RawUser`/`RawReview` deserialization shapes (verify no other callers before deleting; if any used elsewhere, leave those alone).
- **Documentation updates:**
  - README.md "Configuration Reference" → remove `dependency_update_triage` from the list of registered audit slugs (around line 142) and from the `extra` knobs paragraph (around line 155). Remove the entire `dependency_update_triage` row from the audit table (around line 724). Remove both occurrences in the example YAML snippet (around lines 742 and 752).
  - README.md may also have an "audits" overview section mentioning the audit — sweep for any remaining mentions.
- **`config.example.yaml` updates:**
  - Remove the per-repo override commented example (around line 39).
  - Remove the `- dependency_update_triage` bullet from the registered-audits comment (around line 216).
  - Remove from the commented `audits.defaults` example (around line 234).
  - Remove the commented `audits.settings.dependency_update_triage` block (around lines 251–255).

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement establishing the audit-registry enumeration). No REMOVED requirement because the original audit's spec delta never propagated to the canonical spec — a known issue with the `openspec archive` operation on this repo.
- Affected code: removal of `~520 lines from dependency_update.rs` + tests, plus the orphaned `github.rs` helpers (~300 lines counting their tests and types). One-line edits in `cli/run.rs` (import + register call) and `audits/mod.rs` (module declaration).
- Operator-visible behavior change: operators with `audits.defaults.dependency_update_triage: <cadence>` in their config will start getting a config-load error at next start ("nonexistent_audit_xyz" validation already catches typos in audit slugs; the existing `validate_audit_type_names` check will reject the now-unregistered slug). The release notes / commit message MUST call this out so operators know to remove the entry before redeploying.
- Breaking: minor. Operators who configured the audit need to remove its entries from their YAML. There is no in-flight functional loss because the audit was either disabled by default or, in fork-PR-mode deployments, was silently doing nothing useful.
- Acceptance: `cargo test` passes; `openspec validate remove-dependency-update-triage-audit --strict` passes; grep across the workspace for `dependency_update_triage`, `DependencyUpdateAudit`, `dependabot` returns zero hits outside this change's spec/proposal files.
