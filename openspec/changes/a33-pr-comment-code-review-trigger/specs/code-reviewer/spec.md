## MODIFIED Requirements

### Requirement: No reviewer re-run after a reviewer-initiated revision lands

The reviewer SHALL run exactly once per polling iteration's executor pass, as today. A reviewer-initiated revision committed in a subsequent iteration SHALL NOT trigger a re-evaluation by the reviewer; the verdict from the original pass is "frozen" for the life of the PR EXCEPT via the explicit operator-trigger path introduced in this change. Operators wanting iterative reviewer evaluation now have three options:

1. **Operator-initiated re-review via PR comment.** A `@<bot> code-review` PR comment triggers a fresh reviewer pass against the current PR state, bounded by the per-PR cap `reviewer.max_code_reviews_per_pr` (default `5`). See the requirement `Operator-initiated re-review via @<bot> code-review verb` below.
2. **`autocoder rewind`** to re-issue the iteration from scratch (existing).
3. **Close + re-open the PR** to force the polling loop to treat it as a new PR pass (existing).

The original "freeze" semantic remains the default for unattended operation: reviewer cost is bounded because re-runs require explicit operator action. Automatic re-evaluation on revision-lands is NOT introduced by this change.

#### Scenario: Reviewer does not re-run when a revision lands
- **WHEN** a reviewer-initiated revision is committed and force-pushed in iteration N+1
- **THEN** the reviewer is NOT invoked again in iteration N+1
- **AND** the existing `## Code Review` section in the PR body is not updated
- **AND** the PR's draft status (set by the original Block verdict) is preserved

#### Scenario: Operator-initiated re-review IS the explicit exception
- **WHEN** an operator posts `@<bot> code-review` as a PR comment AND the reviewer cap is not exhausted
- **THEN** the reviewer IS invoked against the current PR state (per the `Operator-initiated re-review via @<bot> code-review verb` requirement below)
- **AND** the output is posted as a fresh PR comment with the `## Code Review (rerun N of M)` heading
- **AND** the PR body's original `## Code Review` block is NOT modified

## ADDED Requirements

### Requirement: Operator-initiated re-review via `@<bot> code-review` verb

The revisions dispatcher (`autocoder/src/revisions.rs::process_one_pr`) SHALL recognize `@<bot> code-review` as a new PR-comment verb. The verb takes no arguments in v1; the dispatcher matches the verb pattern case-insensitively on `code-review` AND ignores any trailing text on the same line.

When the verb matches AND the cap is not exhausted, the daemon SHALL:

1. Verify `reviewer.enabled` is `true` AND a usable `api_key` is present. When NOT enabled, post a PR comment whose body starts with `✗ Code review not available: reviewer is disabled in config` AND advance the seen-marker. No reviewer pipeline is invoked.
2. Construct a `ReviewContext` from PR-sourced material: head SHA, full diff, change list (extracted from PR body per `a20a5`), file list, AND the configured `reviewer.mode` (`bundled` OR `per_change`).
3. Invoke the extracted reviewer entry point (`code_reviewer::review_pr_at_state(cfg, &ctx)`) AND await its `ReviewResult`.
4. Post the reviewer's output as a FRESH PR comment whose body starts with the canonical heading:

   ```
   ## Code Review (rerun {N} of {M})
   ```

   where `N` is `state.code_reviews_applied + 1` AND `M` is `state.code_review_cap`. The verdict, per-concern text, AND raw model output (truncated per the existing canonical reviewer-output discipline) follow.

5. On a `Block` verdict AND `reviewer.auto_revise_on_block: true`: post the per-concern `<!-- reviewer-revision -->`-marked PR comments (the existing canonical "Reviewer-initiated revision comments on Block verdicts" requirement applies AND its behavior is preserved).
6. Increment `state.code_reviews_applied`. Advance the seen-marker past the triggering comment. Write the state file.

The original PR body's `## Code Review` block SHALL NOT be modified by any re-review. History is preserved as a chronological sequence of fresh comments.

The PR's draft status SHALL NOT be auto-toggled by any re-review's verdict:

- A re-review's `Approve` verdict on a previously-Blocked PR leaves the draft status as draft (operator undrafts manually after confirming).
- A re-review's `Block` verdict on a previously-Approved PR leaves the draft status as ready (operator drafts manually if desired).
- The original PR-open review's draft-status semantics (Block → draft, Approve → ready) are unchanged. Only re-reviews are exempt from auto-toggling.

When `reviewer.enabled` is `false` OR no usable `api_key` is present, the verb SHALL produce the "not available" reply AND SHALL NOT invoke the reviewer pipeline OR increment any counter.

#### Scenario: Verb triggers fresh review against current PR state
- **WHEN** an operator posts `@<bot> code-review` as a PR comment on an open PR
- **AND** `reviewer.enabled: true` AND `state.code_reviews_applied < state.code_review_cap`
- **THEN** the dispatcher invokes `review_pr_at_state` with a `ReviewContext` built from the PR's current head SHA, diff, change list, AND files
- **AND** the reviewer's output is posted as a fresh PR comment whose body starts with `## Code Review (rerun 1 of 5)` (for the first rerun)
- **AND** the PR body's original `## Code Review` block is NOT modified
- **AND** `state.code_reviews_applied` increments by 1

#### Scenario: Reviewer disabled produces canonical reply without invocation
- **WHEN** an operator posts `@<bot> code-review` AND `reviewer.enabled: false`
- **THEN** the dispatcher posts a PR comment whose body starts with `✗ Code review not available: reviewer is disabled in config`
- **AND** the reviewer pipeline is NOT invoked
- **AND** `state.code_reviews_applied` is NOT incremented
- **AND** the seen-marker IS advanced (the trigger does not re-fire on subsequent polling cycles)

#### Scenario: Block verdict on re-review fires auto-revise comments when enabled
- **WHEN** an operator-initiated re-review produces a `Block` verdict
- **AND** `reviewer.auto_revise_on_block: true`
- **THEN** the daemon posts the per-concern `<!-- reviewer-revision -->`-marked PR comments (existing canonical behavior unchanged)
- **AND** the revise dispatcher picks them up on the next polling cycle exactly as it does for original-review Block verdicts

#### Scenario: Re-review never auto-toggles draft status
- **WHEN** a previously-Blocked PR receives an operator-initiated re-review with an `Approve` verdict
- **THEN** the PR remains in draft state (the operator must manually undraft)
- **AND** the polling-iteration code does NOT call `gh pr ready` OR any equivalent draft-toggle API

#### Scenario: Re-review under `per_change` mode emits per-change content
- **WHEN** an operator-initiated re-review runs with `reviewer.mode: per_change` AND the PR has 3 changes
- **THEN** the LLM client receives exactly 3 reviewer invocations (matching the original review's behavior)
- **AND** the resulting PR comment(s) contain per-change content under `## Code Review (rerun N of M)` heading (whether emitted as one combined comment OR three separate comments is implementer-discretion)

### Requirement: Re-review cap (`reviewer.max_code_reviews_per_pr`) is independent of revision cap

The `reviewer.max_code_reviews_per_pr` config field (default `5`, ceiling `20` with WARN-and-clamp at startup) SHALL bound operator-initiated re-reviews per PR. The cap is independent of the existing `executor.max_revisions_per_pr` cap — re-reviews AND revisions consume separate counters in the same per-PR state file.

The cap counts ONLY operator-initiated re-reviews triggered via the `@<bot> code-review` verb. The original automatic review at PR-open time does NOT count against the cap.

On cap exceeded, the daemon SHALL post a one-time PR decline comment whose body starts with:

```
🛑 Code review cap reached (N reruns). Further @<bot> code-review requests will be ignored. Close + re-open the PR or merge as-is.
```

AND a one-time chatops notification:

```
🛑 <repo>: PR #<num> hit the code-review cap of N. Further @<bot> code-review requests ignored.
```

After posting the decline, the daemon SHALL silently ignore subsequent `code-review` verbs on the same PR (seen-marker still advances; no PR reply; no chatops notification beyond the one-time decline).

#### Scenario: First over-cap trigger posts the decline once
- **WHEN** an open PR has had `max_code_reviews_per_pr` re-reviews applied AND a new `@<bot> code-review` comment arrives
- **THEN** the daemon posts a PR comment whose body starts with `🛑 Code review cap reached`
- **AND** a chatops notification fires whose text starts with `🛑 <repo>: PR #<num> hit the code-review cap`
- **AND** `state.cap_decline_posted_for_code_review` is set to `true`

#### Scenario: Subsequent over-cap triggers are silently ignored
- **WHEN** a PR already has `cap_decline_posted_for_code_review: true` AND a new `@<bot> code-review` comment arrives
- **THEN** the daemon advances `last_seen_comment_at` to the new comment's `created_at`
- **AND** no PR reply is posted
- **AND** no chatops notification fires
- **AND** the reviewer pipeline is NOT invoked

#### Scenario: Revision cap AND re-review cap are independent
- **WHEN** a PR has `revisions_applied: 5` (at the revision cap) AND `code_reviews_applied: 2` (below the re-review cap)
- **AND** an operator posts `@<bot> code-review`
- **THEN** the re-review IS dispatched (the revision cap does NOT block re-reviews)
- **AND** `state.code_reviews_applied` increments to 3

### Requirement: Reviewer entry point is reusable across polling-loop AND operator-trigger callers

The reviewer's LLM-invocation logic SHALL be exposed as a reusable function `code_reviewer::review_pr_at_state(cfg: &ReviewerConfig, ctx: &ReviewContext) -> Result<ReviewResult>`.

- `ReviewContext` SHALL carry `head_sha: String`, `diff: String`, `change_list: Vec<String>`, `files: Vec<FileEntry>`, AND `mode: ReviewerMode`.
- `ReviewResult` SHALL carry `verdict: Verdict (Approve | Block)`, `per_concern: Vec<ConcernEntry>`, AND `raw_output: String`.

The function SHALL NOT decide output disposition; the caller decides whether to write into the PR body's `## Code Review` block (polling-loop caller) OR post as a fresh PR comment with `## Code Review (rerun N of M)` heading (operator-trigger caller).

The function SHALL use the configured `reviewer.mode` per the existing canonical `reviewer.mode: per_change dispatches one reviewer call per change in the PR` requirement. The dispatch logic (one call per change in per_change mode; one call per PR in bundled mode) is unchanged.

The reviewer prompt template, LLM client, output validation, AND retry semantics are unchanged in this change.

#### Scenario: Polling-loop caller produces byte-identical PR body output to pre-spec behavior
- **WHEN** the polling-loop invokes `review_pr_at_state` with a canned PR state
- **AND** the function returns a `ReviewResult`
- **AND** the polling-loop's output-disposition code writes into the PR body's `## Code Review` block
- **THEN** the resulting PR body is byte-identical to pre-spec output for the same inputs (the extraction is refactor-only, no behavior change)

#### Scenario: Operator-trigger caller uses the same function with different disposition
- **WHEN** the operator-trigger dispatcher invokes `review_pr_at_state` with the SAME `ReviewContext` the polling-loop would have built
- **THEN** the function returns the SAME `ReviewResult` (the LLM call AND validation logic are identical)
- **AND** the operator-trigger's output-disposition code posts as a fresh PR comment instead of editing the PR body
