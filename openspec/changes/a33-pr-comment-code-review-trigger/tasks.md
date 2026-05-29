# Tasks

## 1. Config extensions

- [ ] 1.1 Add `max_code_reviews_per_pr: u32` to `ReviewerConfig` (`autocoder/src/config.rs`) with `#[serde(default = "default_max_code_reviews_per_pr")]` returning `5`. Add ceiling constant `MAX_CODE_REVIEWS_PER_PR_CEILING: u32 = 20`.
- [ ] 1.2 Add `suggest_rereview_threshold: Option<f32>` to `ReviewerConfig` with default `None`. Add range-validation (`0.0..=1.0`) at config-load time; out-of-range values fail config-load with a clear message naming the field AND the valid range.
- [ ] 1.3 Document both fields in `config.example.yaml` with examples covering the defaults AND the threshold-disabled / threshold-set cases.
- [ ] 1.4 Unit-test: default-config round-trip preserves `max_code_reviews_per_pr: 5` AND `suggest_rereview_threshold: None`.
- [ ] 1.5 Unit-test: config-load with `suggest_rereview_threshold: 1.5` fails with the documented error message.

## 2. State-file extension

- [ ] 2.1 Add fields to `RevisionState` (`autocoder/src/revisions.rs`):
  - `code_reviews_applied: u32` (`#[serde(default)]`).
  - `code_review_cap: u32` (`#[serde(default)]`; populated from config at write time).
  - `cap_decline_posted_for_code_review: bool` (`#[serde(default)]`).
  - `last_suggested_rereview_at_revisions_count: Option<u32>` (`#[serde(default)]`).
  - `original_review_head_sha: Option<String>` (`#[serde(default)]`).
- [ ] 2.2 Unit-test: a state file JSON without any of the new fields loads cleanly with documented defaults.
- [ ] 2.3 Unit-test: a state file with new fields populated serializes AND deserializes byte-for-byte.

## 3. Verb parsing

- [ ] 3.1 Add `parse_code_review_trigger(body: &str, bot_username: &str) -> bool` in `revisions.rs` (sibling to `parse_revision_trigger`). The function matches `@<bot> code-review` (case-insensitive on `code-review`) followed by end-of-line or end-of-string. Trailing text after the verb is ignored (the verb takes no arguments in v1).
- [ ] 3.2 Update the dispatcher's per-comment loop in `process_one_pr` to try BOTH parsers. When `parse_revision_trigger` matches: route to `execute_revision` (existing path). When `parse_code_review_trigger` matches: route to a new `execute_code_review` (added in task 4). When neither matches: skip the comment (existing fallthrough).
- [ ] 3.3 Unit-test verb parsing: `@<bot> code-review` matches; `@<bot> code-review please` matches (trailing text ignored); `@<bot> revise` does NOT match the code-review parser; `@<bot> code review` (with space) does NOT match in v1 (the hyphenated form is canonical).
- [ ] 3.4 Unit-test ambiguity: a comment that contains BOTH `@<bot> revise` AND `@<bot> code-review` (on separate lines OR same line) matches whichever parser fires first per the existing dispatcher's leading-mention semantic. Document the behavior; do NOT special-case the both-verbs case.

## 4. Operator-trigger dispatcher

- [ ] 4.1 Add `execute_code_review(workspace, repo, reviewer_cfg, github_cfg, &pr, &change_list, head_sha, chatops_ctx, comment_id) -> Result<CodeReviewOutcome>` in `revisions.rs` (sibling to `execute_revision`).
- [ ] 4.2 At the top of `execute_code_review`:
  - Verify `reviewer_cfg.enabled` AND a usable `api_key` is present. On failure, return `CodeReviewOutcome::ReviewerDisabled`.
  - Verify `state.code_reviews_applied < state.code_review_cap`. On cap-exceeded, return `CodeReviewOutcome::CapExceeded`.
- [ ] 4.3 Construct a `ReviewContext { head_sha, diff, change_list, files, mode }` from PR-sourced material (per a20a5's pattern). The `diff` is fetched via the existing `gh api .../pulls/<num>/files` OR equivalent helper.
- [ ] 4.4 Invoke `code_reviewer::review_pr_at_state(reviewer_cfg, &ctx).await` (the extracted entry point per task 5). On error, return `CodeReviewOutcome::Failed { reason }`.
- [ ] 4.5 Post the reviewer's output as a fresh PR comment. Body starts with `## Code Review (rerun {N} of {M})` where N = `code_reviews_applied + 1`, M = `code_review_cap`. The verdict, per-concern text, AND raw model output (truncated per the existing canonical reviewer-output discipline) follow.
- [ ] 4.6 On `Block` verdict AND `reviewer_cfg.auto_revise_on_block: true`: post per-concern `<!-- reviewer-revision -->`-marked PR comments (same path the original-review code uses).
- [ ] 4.7 Increment `state.code_reviews_applied`, write state file.
- [ ] 4.8 Return `CodeReviewOutcome::Completed { verdict }`.
- [ ] 4.9 Per-PR cap decline path: on cap-exceeded, post the canonical decline PR comment AND chatops notification AS today's revision-cap-decline pattern. Set `cap_decline_posted_for_code_review: true` to make the decline one-time.

## 5. Reviewer entry point extraction

- [ ] 5.1 In `code_reviewer.rs`, extract the polling-loop's reviewer invocation into `pub async fn review_pr_at_state(cfg: &ReviewerConfig, ctx: &ReviewContext) -> Result<ReviewResult>`. The function:
  - Constructs the prompt template per the configured `mode`.
  - Invokes the LLM client.
  - Validates the output.
  - Returns `ReviewResult { verdict, per_concern, raw_output }`.
- [ ] 5.2 Update the polling-loop caller to use the extracted function AND retain its output-disposition behavior (write into the PR body's `## Code Review` block).
- [ ] 5.3 Unit-test: `review_pr_at_state` with a stub LLM client returning a canned `Approve` response produces `ReviewResult { verdict: Approve, ... }`.
- [ ] 5.4 Unit-test: `review_pr_at_state` with a stub returning `Block` produces `ReviewResult { verdict: Block, ... }`.
- [ ] 5.5 Regression test: the polling-loop's reviewer invocation against a canned PR state produces byte-identical PR body output to pre-spec behavior.

## 6. Chatops notifications

- [ ] 6.1 Add three notification helpers in `polling_loop.rs` mirroring `a31`'s pattern:
  - `maybe_post_code_review_triggered_alert(chatops_ctx, repo, pr_number, pr_url, operator_login, comment_id)` — posts `🔍 <repo>: code review triggered on PR #<num> by @<operator>`.
  - `maybe_post_code_review_complete_alert(chatops_ctx, repo, pr_number, pr_url, verdict, comment_id)` — posts `✓ <repo>: code review complete on PR #<num> — verdict: <Approve|Block>`.
  - `maybe_post_code_review_failed_alert(chatops_ctx, repo, pr_number, pr_url, reason, comment_id)` — posts `✗ <repo>: code review failed on PR #<num>: <reason>`.
- [ ] 6.2 Each helper respects `failure_alerts_enabled` (skips when off) AND uses the per-repo `chatops_channel_id` resolution.
- [ ] 6.3 Deduplication: extend `AlertState`'s `revise_notifications` map (added in `a31`) with `code_review_*` timestamps. OR add a sibling `code_review_notifications` map keyed by `comment_id` with `triggered_at`, `complete_at`, `failed_at` fields. The implementer picks; the spec binds the dedup semantic, not the field name.
- [ ] 6.4 Dispatch sites in `execute_code_review`: post triggered BEFORE invoking `review_pr_at_state`; post complete OR failed AFTER based on the outcome.
- [ ] 6.5 Unit-test each helper for the on/off, posted/skipped, dedup, AND per-repo-routing cases (mirrors a31's test coverage).

## 7. Diff-overlap suggestion

- [ ] 7.1 Add a new module `autocoder/src/code_review_suggestion.rs` hosting the diff-overlap computation.
- [ ] 7.2 At the original review's completion (in the polling-loop), record `original_review_head_sha` in the per-PR state file. Backward-compat: state files without the field treat the baseline as unknown AND skip the suggestion path.
- [ ] 7.3 After each operator-initiated revision iteration's Completed outcome AND successful push (in `revisions.rs::process_one_pr`'s Completed arm), invoke the suggestion check:
  - Verify `reviewer_cfg.suggest_rereview_threshold` is `Some`.
  - Verify `state.original_review_head_sha` is `Some`.
  - Verify `state.last_suggested_rereview_at_revisions_count != Some(state.revisions_applied)`.
  - Compute `overlap = lines_changed(original_review_head_sha, current_agent_head_sha) / lines_changed(pr.base_sha, original_review_head_sha)`.
  - If `overlap >= threshold`: post `maybe_post_rereview_suggestion_alert` AND set `state.last_suggested_rereview_at_revisions_count = Some(state.revisions_applied)`.
- [ ] 7.4 Add `maybe_post_rereview_suggestion_alert(chatops_ctx, repo, pr_number, pr_url, overlap_percent, revisions_count)` posting the canonical text:
  ```
  💡 `<repo_url>`: PR #<num> has been substantially revised (~<percent>% of original diff changed across <N> revisions). Consider `@<bot> code-review` to re-evaluate.
  <pr_url>
  ```
- [ ] 7.5 The suggestion respects `failure_alerts_enabled` (skips when off).
- [ ] 7.6 Unit-test: a revision Completed with overlap 60% AND threshold 0.5 posts the suggestion AND updates the dedup field.
- [ ] 7.7 Unit-test: the same setup on a second polling cycle (same `revisions_applied` count) does NOT re-post the suggestion.
- [ ] 7.8 Unit-test: threshold unset → no suggestion regardless of overlap.
- [ ] 7.9 Unit-test: `original_review_head_sha` absent → no suggestion (state file from before the field was added).
- [ ] 7.10 Unit-test: `failure_alerts_enabled: false` → no suggestion regardless of threshold.

## 8. Validation

- [ ] 8.1 `cargo test` passes.
- [ ] 8.2 `cargo clippy` produces no NEW warnings against the existing baseline.
- [ ] 8.3 `openspec validate a33-pr-comment-code-review-trigger --strict` passes.
