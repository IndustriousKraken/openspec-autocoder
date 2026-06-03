# Implementation tasks

## 1. Decouple the auto-revise trigger from the `Block` verdict

- [x] 1.1 In `autocoder/src/polling_loop.rs::partition_and_annotate_reviewer_revisions` (~line 5105), DELETE the leading `if report.verdict != ReviewVerdict::Block { return Vec::new(); }` guard. The function SHALL no longer consult `report.verdict`.
- [x] 1.2 The existing `revisable` filter (concerns with `should_request_revision == true` AND non-empty `actionable_request`) becomes the sole gate. When `revisable` is empty, keep the existing WARN ("auto-revise is enabled but no concerns had `actionable_request` + `should_request_revision: true` populated; ...") AND return empty. The cap-budget truncation + dropped-concern annotation logic below it is unchanged.
- [x] 1.3 Confirm the caller in `polling_loop.rs` (~line 1130) still gates the call on `r.auto_revise()` (renamed accessor, task 2) — the function is only invoked when the flag is on. The `draft = matches!(report.verdict, ReviewVerdict::Block)` line is UNCHANGED (Block still marks the PR draft; it just no longer gates auto-revise).
- [x] 1.4 Check the rerun path in `revisions.rs` (~line 1280) where `reviewer.auto_revise_on_block()` gates reviewer-revision posting on the operator-trigger route. Apply the same decoupling: post reviewer-revision comments for actionable concerns regardless of verdict, gated only on the (renamed) flag. Verify it routes through the same `partition` logic OR mirror the change.

## 2. Rename the config flag with a backward-compat alias

- [x] 2.1 In `autocoder/src/config.rs`, rename `ReviewerConfig::auto_revise_on_block` → `auto_revise` AND add `#[serde(alias = "auto_revise_on_block")]` so existing config files using the old key load unchanged with no warning.
- [x] 2.2 Rename the accessor `auto_revise_on_block()` → `auto_revise()` (OR keep the accessor name and only rename the field — pick the lower-churn option; the spec references `reviewer.auto_revise` as the field name, not the accessor). Update all call sites (`polling_loop.rs`, `revisions.rs`).
- [x] 2.3 Update `config.example.yaml`: the `reviewer:` block's `auto_revise_on_block` example comment becomes `auto_revise`, with a one-line note that the old key is still accepted as an alias.

## 3. Tests

- [x] 3.1 Invert the existing `partition_and_annotate_reviewer_revisions` tests that assert "Pass/Concerns post nothing": a `Concerns` verdict with one `should_request_revision: true` concern (non-empty `actionable_request`) now returns that concern.
- [x] 3.2 Add: `Block` verdict + actionable concerns still returns them (Block path preserved, not regressed).
- [x] 3.3 Add: `Pass` verdict + one actionable concern returns the concern (verdict fully decoupled).
- [x] 3.4 Preserve: any verdict + zero actionable concerns returns empty AND logs the WARN.
- [x] 3.5 Config test: `auto_revise_on_block: true` deserializes to `auto_revise == true` via the alias; `auto_revise: true` deserializes identically.
- [x] 3.6 If the rerun path (task 1.4) has its own posting logic, add a parallel test there for the Concerns-with-actionable-concern case.

## 4. Documentation

- [x] 4.1 `docs/CODE-REVIEW.md` (and any reviewer-config reference in `docs/CONFIG.md`): update `auto_revise_on_block` → `auto_revise`, note the alias, AND correct the description to "fires on actionable concerns regardless of verdict" (was "on Block verdicts"). No kitsch.
- [x] 4.2 `README.md` — the reviewer line mentions `auto_revise_on_block`; update to `auto_revise` with the corrected semantics.

## 5. Acceptance gate

- [x] 5.1 `cargo test` passes for the autocoder crate.
- [x] 5.2 `openspec validate a46-auto-revise-fires-on-actionable --strict` passes.
- [ ] 5.3 Manual end-to-end: against a test PR, drive an initial review that returns `Concerns` with one actionable concern; confirm exactly one `<!-- reviewer-revision -->` comment is posted AND the next iteration's dispatcher executes the revision (bounded by the existing `executor.max_revisions_per_pr` cap). (NOT performed inside the autocoder sandbox — requires a live deployed daemon, a real reviewer LLM provider, and a configured test repo with an open PR. This is a post-deploy operator verification step; the runtime behavior it checks is covered by the §3 unit/integration tests: 3.3 proves a non-Block verdict returns the actionable concern from the initial-review partition, 3.6 (`rerun_concerns_verdict_with_actionable_concern_posts_reviewer_revision`) drives the rerun path end-to-end with a stub reviewer + mockito GitHub backend and asserts exactly one `<!-- reviewer-revision -->` comment is posted under a Concerns verdict, and the existing `post_reviewer_revision_comments_posts_marker_and_trigger` test asserts the marker + `@<bot> revise <actionable_request>` body shape the dispatcher consumes.)
