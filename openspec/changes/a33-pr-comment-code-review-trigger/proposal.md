## Why

The code reviewer (`code_reviewer.rs`) runs exactly once per PR pass — during the polling iteration that opens the PR. The canonical "No reviewer re-run after a reviewer-initiated revision lands" requirement explicitly freezes the verdict for the life of the PR AND notes that operators wanting re-evaluation "can wait for a separate change that adds reviewer re-evaluation as an explicit feature." This change IS that separate feature.

The reviewer-runs-once model was the right v1 default — it bounds cost AND avoids reviewer-revision loops. But it has an operator-facing problem: when a PR goes through multiple rounds of operator-initiated revision (via the `@<bot> revise` dispatcher) AND substantial portions of the code change between the original review AND the current state, the operator has no signal whether the new code still meets the quality bar the reviewer set. The original `## Code Review` section is stale; the operator either skims the diff manually OR closes-and-reopens the PR to force a re-review.

Two complementary additions close the gap:

1. **Operator-initiated re-review.** A new PR-comment verb `@<bot> code-review` triggers a fresh reviewer pass against the current PR state. Sibling to `revise`, parsed by the same dispatcher loop, capped per-PR like revisions, with results posted as a fresh PR comment (NOT a body edit — the original review stays where it is for history).
2. **Diff-overlap-driven suggestion.** A new optional `reviewer.suggest_rereview_threshold` config knob (default unset = no suggestion). When set, after each operator-initiated revision iteration's Completed outcome, the daemon SHALL compute the iteration's diff overlap with the original PR diff. When overlap exceeds the threshold, the daemon posts a chatops notification recommending `@<bot> code-review`. The operator decides whether to trigger; the suggestion is informational, NOT automatic.

The split — manual trigger via PR comment + opt-in informational suggestion via chatops — keeps reviewer cost bounded (every re-review is operator-authorized) while removing the "is this PR still good?" blind spot.

## What Changes

**New PR-comment verb: `@<bot> code-review`.** Parsed by the existing revisions-dispatcher comment loop alongside `revise`. When the verb matches:

1. The dispatcher verifies the reviewer is enabled (`reviewer.enabled: true` AND a usable `api_key` is present). When NOT enabled, the dispatcher posts a PR comment `✗ Code review not available: reviewer is disabled in config` AND advances the seen-marker. No reviewer pipeline is invoked.
2. The dispatcher fetches the PR's current state: head SHA, diff, change list (extracted from PR body per `a20a5`), file list.
3. The dispatcher invokes the reviewer pipeline with the same context the polling-iteration code would have built. The reviewer respects the configured `reviewer.mode` (`bundled` OR `per_change`).
4. The reviewer's output (verdict + per-concern text) is posted as a FRESH PR comment, NOT a body edit. The original `## Code Review` block in the PR body is preserved as historical record. The comment's body starts with the canonical marker `## Code Review (rerun N of M)` where `N` is `code_reviews_applied + 1` AND `M` is the cap.
5. On a `Block` verdict AND `reviewer.auto_revise_on_block: true`: the reviewer's per-concern revision comments fire (existing canonical "Reviewer-initiated revision comments on Block verdicts" requirement applies unchanged — those comments are `<!-- reviewer-revision -->`-marked AND the revise dispatcher picks them up next cycle).
6. The per-PR state file's `code_reviews_applied` counter increments AND the seen-marker advances past the triggering comment.

**Per-PR code-review cap.** New config knob `reviewer.max_code_reviews_per_pr` (default `5`, ceiling `20`, WARN-and-clamp at startup — mirrors `executor.max_revisions_per_pr`). The cap covers operator-initiated re-reviews via the verb; the original automatic review at PR-open time does NOT count against this cap. State file's `code_reviews_applied` counts only operator-triggered re-reviews. On cap exceeded, the daemon posts a one-time PR decline comment `🛑 Code review cap reached (N reruns). Further @<bot> code-review requests will be ignored. Close + re-open the PR or merge as-is.` AND a one-time chatops notification, AND silently ignores subsequent `code-review` triggers (the seen-marker still advances).

**Diff-overlap suggestion.** New optional config knob `reviewer.suggest_rereview_threshold: f32` (range `0.0..=1.0`; unset = disabled). After each operator-initiated revision iteration's Completed outcome AND successful push, the daemon SHALL compute the overlap ratio:

```
overlap = (lines_changed_in_revision_diff) / (lines_changed_in_original_pr_diff)
```

When `overlap >= threshold` AND a suggestion has NOT already been posted for the current `revisions_applied` count (deduplication keyed by revision count to avoid re-suggesting on the same iteration), the daemon SHALL post a chatops notification:

```
💡 `<repo_url>`: PR #<num> has been substantially revised (~<percent>% of original diff changed across <N> revisions). Consider `@<bot> code-review` to re-evaluate.
<pr_url>
```

The notification respects `failure_alerts_enabled` (off → no suggestion). The notification is INFORMATIONAL — it does NOT trigger anything automatically. The operator decides whether to post the `code-review` verb.

State-file tracking for suggestion deduplication: new field `last_suggested_rereview_at_revisions_count: Option<u32>`. Set after each successful suggestion post. Prevents re-suggesting on the same `revisions_applied` count.

**Reviewer pipeline reusability.** The existing reviewer entry point (currently invoked from `polling_loop.rs` after a Completed outcome opens a PR) is extracted into a reusable function that takes a `ReviewContext` (head SHA, diff, change list, files, mode) AND returns a `ReviewResult` (verdict, per-concern text, raw model output). The polling-loop caller AND the new operator-trigger caller invoke the same function with different `ReviewContext` constructors. Output disposition differs:

- Polling-loop caller writes the output into the PR body's `## Code Review` block (existing behavior).
- Operator-trigger caller writes the output as a fresh PR comment with the `## Code Review (rerun N of M)` heading.

PR draft status interactions:

- Original PR-open review's `Block` verdict sets the PR to draft (existing behavior, unchanged).
- An operator-triggered re-review's `Approve` verdict on a previously-Blocked PR DOES NOT auto-undraft the PR. Operators undraft manually after confirming the re-review. (Avoids surprise auto-undrafting; the operator is in the loop by definition since they triggered the re-review.)
- An operator-triggered re-review's `Block` verdict on a previously-Approved PR DOES NOT auto-draft the PR. Same rationale (no surprise transitions; operator triggered the review AND is in the loop).
- The reviewer's auto-revise-on-block path AND its per-concern comment posting apply on re-reviews exactly as they do on the original review.

## Impact

- **Affected specs:**
  - `code-reviewer` — MODIFIED `No reviewer re-run after a reviewer-initiated revision lands` requirement to add the explicit exception for operator-initiated `@<bot> code-review` triggers (the canonical text already cites this as a future feature). MODIFIED `Reviewer-initiated revision comments on Block verdicts` requirement to clarify the per-concern revision comments fire on both original AND re-review `Block` verdicts. ADDED requirements for the operator-trigger verb, the per-PR re-review cap, the reusable reviewer entry point's contract, the re-review output disposition (fresh PR comment), AND the draft-status preservation rules.
  - `chatops-manager` — ADDED requirements for the `code-review` PR-comment verb's notification lifecycle (mirrors `a31`'s revise-lifecycle pattern: triggered / complete / failed). ADDED requirement for the diff-overlap suggestion notification AND its deduplication.
  - `orchestrator-cli` — ADDED requirement for the per-PR state file extension (`code_reviews_applied`, `code_review_cap`, `cap_decline_posted_for_code_review`, `last_suggested_rereview_at_revisions_count`).
- **Affected code:**
  - `autocoder/src/config.rs` — `ReviewerConfig` gains `max_code_reviews_per_pr: u32` (default 5, ceiling 20, clamp-with-WARN) AND `suggest_rereview_threshold: Option<f32>` (range validation at config-load).
  - `autocoder/src/revisions.rs` — comment-loop verb dispatch gains a `code-review` branch alongside `revise`. New `parse_code_review_trigger` helper (sibling to `parse_revision_trigger`). New `execute_code_review` function (sibling to `execute_revision`) that invokes the extracted reviewer entry point AND posts the fresh PR comment.
  - `autocoder/src/code_reviewer.rs` — extract the polling-loop's reviewer invocation into a `review_pr_at_state(ctx: ReviewContext) -> Result<ReviewResult>` function. Caller decides output disposition.
  - `autocoder/src/alert_state.rs` — `RevisionState` gains `code_reviews_applied`, `code_review_cap`, `cap_decline_posted_for_code_review`, `last_suggested_rereview_at_revisions_count` fields. All `#[serde(default)]` for backward-compat with existing state files.
  - `autocoder/src/polling_loop.rs` — three new notification helpers for the code-review lifecycle (`maybe_post_code_review_triggered_alert`, `maybe_post_code_review_complete_alert`, `maybe_post_code_review_failed_alert`) AND one for the suggestion (`maybe_post_rereview_suggestion_alert`).
  - Diff-overlap helper: new `autocoder/src/code_review_suggestion.rs` (OR similar) hosting the lines-changed counting AND ratio computation against `git diff` output.
  - `prompts/code-review-default.md` — no change. Re-review uses the same reviewer prompt as initial review.
- **Operator-visible behavior:**
  - PR comment `@<bot> code-review` triggers a fresh review pass. Cost-bounded by `reviewer.max_code_reviews_per_pr`. Output posted as a fresh PR comment with the canonical rerun-N-of-M heading.
  - Operators who enable `reviewer.suggest_rereview_threshold` see chatops notifications when a PR has been substantially revised. The notification is informational only; the operator decides whether to trigger the re-review.
  - PR body's original `## Code Review` block is never modified by re-reviews. History is preserved.
  - Draft status changes only on the ORIGINAL review's Block verdict. Re-reviews never auto-draft or auto-undraft.
- **Backward compatibility:** existing state files load cleanly via serde defaults. Existing config files load cleanly (both new fields are optional with documented defaults). Existing PR review behavior is byte-identical when `reviewer.suggest_rereview_threshold` is unset AND no operator posts `@<bot> code-review`.
- **Dependencies:** none hard. Synergizes with `a27a2` (acceptance scan + recovery loop) AND with `a31` (revise-lifecycle notifications) — when revisions actually complete the work AND the operator sees the success notification, the suggestion mechanism becomes more useful. Can land before, after, OR alongside any of them.
- **Acceptance:** `cargo test` passes; `openspec validate a33-pr-comment-code-review-trigger --strict` passes. Tests:
  - PR-comment verb parsing: `@<bot> code-review` is recognized AND distinguished from `@<bot> revise`.
  - Disabled-reviewer path: an `@<bot> code-review` comment with `reviewer.enabled: false` posts the canonical "not available" reply AND does NOT invoke the reviewer.
  - Cap enforcement: 5 successful re-reviews advance the counter to 5; the 6th triggers the decline reply AND no reviewer invocation.
  - Backward compat: a `RevisionState` JSON without the new fields loads with defaults (`code_reviews_applied: 0`, `code_review_cap: <config default>`, AND the suggestion-deduplication field as `None`).
  - Output disposition: a re-review posts a PR comment whose body starts with `## Code Review (rerun N of M)`; the PR body's original `## Code Review` block is NOT modified.
  - Draft preservation: a re-review's `Approve` verdict on a Blocked PR leaves the draft status unchanged. A re-review's `Block` verdict on an Approved PR leaves the draft status unchanged.
  - Block + auto-revise: a re-review's `Block` verdict with `reviewer.auto_revise_on_block: true` posts the `<!-- reviewer-revision -->`-marked per-concern comments exactly like the original review's Block path.
  - Suggestion threshold: a revision iteration whose diff covers 60% of the original PR diff with `reviewer.suggest_rereview_threshold: 0.5` posts the suggestion. The same iteration count does NOT re-post on the next polling cycle (deduplication via `last_suggested_rereview_at_revisions_count`).
  - Suggestion threshold off: no suggestion is ever posted when `reviewer.suggest_rereview_threshold` is unset, regardless of diff overlap.
  - Suggestion respects `failure_alerts_enabled`: when the toggle is `false`, no suggestion fires even if the threshold is exceeded.
