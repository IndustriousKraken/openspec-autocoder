## ADDED Requirements

### Requirement: Per-PR state file tracks code-review counts AND suggestion deduplication

The per-PR `RevisionState` JSON (at `<workspace>/.autocoder/revisions/<pr_number>.json`, per the canonical `Per-PR state file persists revision count and last-seen timestamp; closed PRs are pruned` requirement) SHALL gain the following fields. All fields SHALL have serde defaults so existing state files load cleanly without migration:

- `code_reviews_applied: u32` (default `0`). Counts operator-initiated re-reviews triggered via `@<bot> code-review`. Does NOT count the original automatic review at PR-open time.
- `code_review_cap: u32` (default populated from `reviewer.max_code_reviews_per_pr` config at write time; falls back to `5` if config is absent during deserialization). Per-PR upper bound on operator-initiated re-reviews.
- `cap_decline_posted_for_code_review: bool` (default `false`). Set `true` after the one-time cap-decline PR comment AND chatops notification are posted on cap exceeded. Prevents repeated decline messages.
- `last_suggested_rereview_at_revisions_count: Option<u32>` (default `None`). Records the `revisions_applied` count at which the most recent re-review suggestion fired. Used to deduplicate the suggestion across polling cycles on the same revision count.
- `original_review_head_sha: Option<String>` (default `None`). Records the agent-branch head SHA at the time the original automatic review completed. Set by the polling-loop's reviewer-completion path. Used as the baseline for the diff-overlap suggestion. State files written before this change deployed have this field as `None`; the suggestion path gracefully degrades to "no suggestion" in that case.

The state file's atomic-write semantics (per the existing canonical `State writes are atomic` requirement) are preserved unchanged.

The pruning behavior for closed PRs (per the existing canonical `Closed PRs have their state pruned` requirement) applies to the extended state file unchanged: when a PR closes, its entire state file is removed, including the new fields.

#### Scenario: New fields default cleanly when loading legacy state files
- **WHEN** the daemon loads a `RevisionState` JSON that was written by an older daemon AND contains NO `code_reviews_applied`, `code_review_cap`, `cap_decline_posted_for_code_review`, `last_suggested_rereview_at_revisions_count`, OR `original_review_head_sha` fields
- **THEN** the loaded `RevisionState` has `code_reviews_applied: 0`, `code_review_cap: 5` (the documented default), `cap_decline_posted_for_code_review: false`, `last_suggested_rereview_at_revisions_count: None`, AND `original_review_head_sha: None`
- **AND** no error is logged

#### Scenario: New fields round-trip cleanly when populated
- **WHEN** the daemon writes a `RevisionState` with `code_reviews_applied: 3`, `code_review_cap: 5`, `cap_decline_posted_for_code_review: false`, `last_suggested_rereview_at_revisions_count: Some(2)`, AND `original_review_head_sha: Some("abc123def")`
- **AND** the file is read back
- **THEN** the deserialized `RevisionState` matches the written values byte-for-byte

#### Scenario: Original-review-head-sha populated by polling-loop completion path
- **WHEN** the polling-loop's reviewer-completion code (the path that today writes `## Code Review` into the PR body) completes successfully for the FIRST review on a PR
- **THEN** the daemon writes `state.original_review_head_sha = Some(<current agent-branch head SHA>)` to the per-PR state file
- **AND** the state file write uses atomic-rename semantics (per the existing canonical `State writes are atomic` requirement)

#### Scenario: Re-review path does NOT overwrite original_review_head_sha
- **WHEN** an operator-initiated re-review (via `@<bot> code-review`) completes successfully
- **THEN** `state.code_reviews_applied` increments
- **AND** `state.original_review_head_sha` is NOT modified (the baseline for the suggestion's overlap calculation must remain the ORIGINAL review's head SHA, not subsequent re-reviews' SHAs)

#### Scenario: Cap field is independent of revision cap
- **WHEN** the daemon loads a state file with `revisions_applied: 5`, `revision_cap: 5`, `code_reviews_applied: 2`, AND `code_review_cap: 5`
- **THEN** an operator `@<bot> revise` comment is rejected as cap-exceeded (revisions are at cap)
- **AND** an operator `@<bot> code-review` comment IS dispatched (re-reviews are below cap; the two cap counters are independent)
