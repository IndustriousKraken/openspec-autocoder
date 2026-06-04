# code-reviewer — delta for a47-auto-only-revision-caps

## MODIFIED Requirements

### Requirement: Cap-budget interaction with reviewer-posted comments
The reviewer-posting step SHALL respect the per-PR `executor.max_auto_revisions_per_pr` cap (legacy alias `executor.max_revisions_per_pr`). Reviewer-revision comments are automatic revisions AND count against this cap. When the reviewer would generate more should-revise concerns than the remaining cap budget allows, the daemon SHALL post only the first N concerns (where N = remaining budget; concerns are taken in the reviewer's output order, which the reviewer's prompt template instructs to be most-critical-first) AND SHALL annotate the dropped concerns in the PR-body `## Code Review` section so the human sees what was skipped.

#### Scenario: Cap budget exhausted truncates posts and annotates drops
- **WHEN** the reviewer returns Block with 3 should-revise concerns AND the per-PR remaining cap budget is 2
- **THEN** exactly 2 reviewer-revision comments are posted (the first 2 in the reviewer's output order)
- **AND** the PR-body `## Code Review` section contains an entry for the third concern annotated `(not auto-revised; cap budget exhausted)`

#### Scenario: Cap budget zero posts nothing
- **WHEN** the reviewer returns Block with should-revise concerns AND the per-PR remaining cap budget is 0
- **THEN** no comments are posted
- **AND** every should-revise concern is annotated in the PR-body section with `(not auto-revised; cap budget exhausted)`

### Requirement: Re-review cap (`reviewer.max_code_reviews_per_pr`) is independent of revision cap

The `reviewer.max_code_reviews_per_pr` config field SHALL bound operator-initiated re-reviews per PR ONLY when the operator sets it; its default SHALL be UNLIMITED (unset). Re-reviews are uncapped by default because every re-review is a deliberate operator action triggered via `@<bot> code-review`, AND there is no automatic-re-review path (per the canonical "No reviewer re-run after a reviewer-initiated revision lands" requirement), so there is no runaway to bound. When set to a positive integer (ceiling `20`, WARN-and-clamp at startup), it acts as an opt-in ceiling.

The cap is independent of the `executor.max_auto_revisions_per_pr` cap — re-reviews AND automatic revisions consume separate counters in the same per-PR state file. The original automatic review at PR-open time does NOT count against the cap (it is not a re-review).

When the cap is set AND exceeded, the daemon SHALL post a one-time PR decline comment whose body starts with:

```
🛑 Code review cap reached (N reruns). Further @<bot> code-review requests will be ignored. Close + re-open the PR or merge as-is.
```

AND a one-time chatops notification:

```
🛑 <repo>: PR #<num> hit the code-review cap of N. Further @<bot> code-review requests ignored.
```

After posting the decline, the daemon SHALL silently ignore subsequent `code-review` verbs on the same PR (seen-marker still advances; no PR reply; no chatops notification beyond the one-time decline). When the cap is UNSET (the default), no decline is ever posted AND re-reviews always process.

#### Scenario: Default (unset) cap means unlimited re-reviews
- **GIVEN** `reviewer.max_code_reviews_per_pr` is unset (the default)
- **WHEN** an operator posts `@<bot> code-review` for the Nth time on a PR, for any N
- **THEN** the re-review IS dispatched
- **AND** no cap-decline comment is ever posted
- **AND** `state.code_reviews_applied` increments (tracked for display) but is never compared against a ceiling

#### Scenario: First over-cap trigger posts the decline once (cap set)
- **GIVEN** the operator has set `reviewer.max_code_reviews_per_pr`
- **WHEN** an open PR has had `max_code_reviews_per_pr` re-reviews applied AND a new `@<bot> code-review` comment arrives
- **THEN** the daemon posts a PR comment whose body starts with `🛑 Code review cap reached`
- **AND** a chatops notification fires whose text starts with `🛑 <repo>: PR #<num> hit the code-review cap`
- **AND** `state.cap_decline_posted_for_code_review` is set to `true`

#### Scenario: Subsequent over-cap triggers are silently ignored (cap set)
- **GIVEN** the operator has set `reviewer.max_code_reviews_per_pr`
- **WHEN** a PR already has `cap_decline_posted_for_code_review: true` AND a new `@<bot> code-review` comment arrives
- **THEN** the daemon advances `last_seen_comment_at` to the new comment's `created_at`
- **AND** no PR reply is posted
- **AND** no chatops notification fires
- **AND** the reviewer pipeline is NOT invoked

#### Scenario: Revision cap AND re-review cap are independent
- **WHEN** a PR has `auto_revisions_applied: 5` (at the automatic-revision cap) AND `code_reviews_applied: 2`
- **AND** an operator posts `@<bot> code-review`
- **THEN** the re-review IS dispatched (the automatic-revision cap does NOT block re-reviews)
- **AND** `state.code_reviews_applied` increments to 3
