# Design

## Decisions to lock in

### D1. PR-comment verb only. No chat-side `@<bot> code-review <repo> <pr>` form in v1.

The PR comment is the most discoverable place to trigger a re-review — operators are typically looking at the PR diff when they decide one is needed. Adding a chat-side form would duplicate the dispatch surface AND require a repo+PR resolver that the PR-comment form gets for free (the comment IS on the PR).

A chat-side form is a natural follow-on if operators ask for it. The PR-comment form covers the primary workflow without it.

### D2. Output goes to a fresh PR comment, NOT a body edit.

The original `## Code Review` block in the PR body is a historical artifact of the first review pass. Editing it on every re-review would:

- Erase the original verdict (operator loses context on what changed).
- Confuse reviewers reading the PR top-to-bottom (the "first impression" review is mutable).
- Mismatch the canonical "PR body's `## Code Review` block" semantic in the existing `Reviewer-initiated revision comments on Block verdicts` requirement.

A fresh PR comment per re-review:

- Preserves history (`gh pr view --comments` shows the chronology).
- Uses the same posting primitive every other dispatcher reply uses.
- Has a canonical heading (`## Code Review (rerun N of M)`) that makes the rerun count visible to a human skimmer.

The original review block stays in the body, unchanged, forever.

### D3. Re-review cap (`max_code_reviews_per_pr`) is independent of revision cap.

The existing `executor.max_revisions_per_pr` cap (default 5) governs operator-initiated REVISIONS — work that mutates the diff. The new `reviewer.max_code_reviews_per_pr` cap (default 5) governs operator-initiated RE-REVIEWS — review passes against the current diff state. They are independent because the rate-limiting concerns differ:

- Revisions are expensive (full implementer subprocess, can be 30-60 minutes). The cap exists to prevent runaway implementer cost.
- Re-reviews are cheaper (single reviewer LLM call, seconds-to-minutes). But still nonzero. The cap exists to prevent runaway reviewer LLM cost.

The same per-PR state file tracks both counters because they share lifecycle (same per-PR state, same eviction-on-close rule).

### D4. Re-reviews do NOT auto-undraft or auto-draft.

The original review's `Block` verdict drafts the PR (existing behavior). Re-reviews are operator-initiated, which means the operator is by definition in the loop. Auto-undrafting on `Approve` would surprise the operator (they triggered the re-review to GET a verdict, not to have the PR's status flipped for them). Auto-drafting on `Block` from a previously-Approved PR would be even more surprising.

The operator's workflow with a re-review verdict:

- `Approve` on a Blocked PR: operator reads the new verdict, decides to undraft AND merge.
- `Block` on an Approved PR: operator reads the new concerns, decides whether to draft AND revise.

In both cases the human is the agent of the state transition. The daemon respects that.

### D5. Diff-overlap suggestion uses lines-changed as the unit, not files-touched.

Two natural metrics:

- **Files touched.** Simpler to compute; reflects coverage of the codebase.
- **Lines changed.** More accurate proxy for "amount of code revised."

A revision iteration that touches one file but rewrites 80% of it carries more review-relevant change than a revision that touches three files with one-line tweaks each. Lines-changed captures this better. The implementation uses `git diff --numstat` against the original PR head AND the current head, sums additions + deletions for each metric, AND computes the ratio.

Implementation note: "lines_changed_in_revision_diff" is the cumulative across ALL revision iterations on this PR, NOT just the most recent one. The metric answers "how much has this PR changed since the original review" — which is what the operator cares about.

### D6. Suggestion deduplication keyed by `revisions_applied` count.

The suggestion fires after a revision iteration's Completed outcome. Without deduplication, every subsequent polling cycle that finds an over-threshold diff would re-fire the same suggestion, spamming the chat channel.

The dedupe key is the `revisions_applied` count at the time of the suggestion. Each new revision iteration increments the count AND becomes a fresh opportunity to suggest. The state field `last_suggested_rereview_at_revisions_count: Option<u32>` records the count at the last suggestion. The check is `state.last_suggested_rereview_at_revisions_count != Some(state.revisions_applied)` — meaning "we haven't suggested for this revision count yet."

A re-review actually being run (via the verb) does NOT reset this field. The operator's act of running the re-review consumes the suggestion's prompt; we don't re-suggest for the same revision count just because the re-review happened.

### D7. The reviewer entry point is extracted into a reusable function.

Today's polling-loop reviewer invocation is interleaved with PR-open AND PR-body composition. To support the operator-trigger caller, the LLM-invocation part is extracted into:

```rust
pub async fn review_pr_at_state(
    cfg: &ReviewerConfig,
    ctx: &ReviewContext,
) -> Result<ReviewResult>;
```

Where:

- `ReviewContext` carries `head_sha`, `diff`, `change_list`, `files`, AND the configured `mode` (which the caller may want to override in a future feature; v1 always uses the config-resolved mode).
- `ReviewResult` carries the verdict (`Approve` / `Block`), the per-concern text, AND raw model output (for log capture).

Both callers (polling-loop AND operator-trigger) construct `ReviewContext` from their available state AND consume `ReviewResult` according to their output disposition. The reviewer's prompt template, LLM client, AND validation logic are unchanged.

## Open questions for the implementer

- **Original PR diff baseline.** The diff-overlap computation needs the "original PR diff" — the diff at the time the FIRST review ran. Options: (a) store the original head SHA in the per-PR state file when the original review completes; (b) recover it from the bot's first PR comment (the `## Code Review` block, find the surrounding commit hash); (c) use the PR's base SHA + the agent branch's HEAD at the time of the first review (recoverable from `git log` on the agent branch with the right filter). Option (a) is cleanest; the implementer SHOULD extend the state-file write at the original review's completion to record the head SHA. Backward-compat: existing state files without the field treat the original SHA as "unknown" AND skip the suggestion path (the suggestion is opt-in via config anyway; missing baseline degrades gracefully to "no suggestion").
- **`per_change` mode rerun output.** Under `reviewer.mode: per_change`, the original review emits one `## Code Review: <slug>` section per change in the PR body. A re-review under the same mode would emit one PR comment per change. The implementer SHOULD post these as separate PR comments OR as a single combined comment with per-change subsections. Separate comments scale better with many changes; combined is easier to skim. The implementer MAY pick either; the spec binds the canonical heading (`## Code Review (rerun N of M)`) but not the granularity of comment posts.
- **Cost of cumulative diff overlap.** Computing `lines_changed_in_revision_diff` cumulatively requires walking the agent branch's commit log from the original review's head to the current head AND summing diffs. For PRs with many revisions, this is non-trivial. The implementer MAY cache the per-revision-iteration diff size in the state file AND sum from cached values. Implementation detail; the spec just requires the cumulative semantic.
- **Threshold validation at config-load.** `reviewer.suggest_rereview_threshold` is a ratio `0.0..=1.0`. Values outside the range MUST fail config-load with a clear message naming the field AND the valid range. A value of `0.0` means "suggest after any non-zero revision" (probably too noisy in practice but operator's choice). A value of `1.0` means "suggest only when revision diff equals or exceeds the original" (probably too conservative). The implementer SHOULD document these endpoints in `config.example.yaml`.

## Stack ordering

This change can land before, after, OR alongside the a27a stack AND a31. No hard dependencies. Synergies are real but not gating:

- After a27a2 (acceptance scan + recovery loop): re-reviews are more useful because revisions actually finish the work — the re-review evaluates substantive code rather than a vacuous "no diff" iteration.
- After a31 (revise-lifecycle notifications): the suggestion notification fits naturally alongside the lifecycle notifications a31 introduced; the chatops channel becomes the single place to learn about PR state changes.
- After a2705 (strict-since filter): unrelated, but a2705 fixes a real production bug AND should ship first regardless.

Recommended deploy order: a2705 → a27a0 → a27a1 → a27a2 → a31 → a33. The user-stated "this can definitely go last" matches.
