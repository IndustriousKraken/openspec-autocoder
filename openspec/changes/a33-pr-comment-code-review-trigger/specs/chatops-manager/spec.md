## ADDED Requirements

### Requirement: Operator-initiated re-review posts lifecycle notifications to chatops

When the revisions dispatcher processes an operator-posted `@<bot> code-review` PR comment, the daemon SHALL post chatops notifications at three points in the re-review lifecycle, mirroring the revise-lifecycle pattern established in `a31`:

1. **Code review triggered.** Posted BEFORE the reviewer pipeline launches. Signals to the operator that the verb was parsed AND dispatched.
2. **Code review complete.** Posted AFTER the reviewer returns a verdict AND the fresh PR comment is posted. Signals successful completion AND surfaces the verdict.
3. **Code review failed.** Posted on reviewer error, LLM client failure, OR PR-comment-post failure. Signals the re-review did NOT complete cleanly.

Each notification SHALL be routed through the existing chatops channel resolution (per-repo `chatops_channel_id` override; fallback to `chatops.default_channel_id`). When no chatops backend is configured, all three notifications SHALL be silently skipped.

Each notification SHALL respect the `failure_alerts_enabled` toggle. When the toggle is `false`, NONE of the three fire.

Each notification SHALL be deduplicated keyed by the operator comment's GitHub `comment_id`. The deduplication storage is implementer-discretion (extending `a31`'s `revise_notifications` map OR a sibling `code_review_notifications` map). The spec binds the dedup semantic, not the field name.

**Canonical notification text shapes:**

- **Code review triggered:**

  ```
  🔍 `<repo_url>`: code review triggered on PR #<num> by @<operator_login>
  <pr_url>
  ```

- **Code review complete:**

  ```
  ✓ `<repo_url>`: code review complete on PR #<num> — verdict: <Approve|Block>
  <pr_url>
  ```

- **Code review failed:**

  ```
  ✗ `<repo_url>`: code review failed on PR #<num>: <reason>
  <pr_url>
  ```

  When `<reason>` is longer than 35,000 characters, the failed notification SHALL use the threaded-notification path (per the existing canonical "ChatOpsBackend exposes a threaded-notification method with graceful degradation" requirement) AND truncate per the existing canonical "Thread body truncates at 35,000 characters" requirement.

#### Scenario: Code review triggered fires before reviewer launches
- **WHEN** the revisions dispatcher decides to dispatch an operator `@<bot> code-review` comment AND the cap is not exhausted
- **AND** `chatops_ctx` is configured AND `failure_alerts_enabled: true`
- **THEN** before `review_pr_at_state` is invoked, the daemon posts the canonical "Code review triggered" text to the per-repo chatops channel
- **AND** the dedup storage records the triggered-at timestamp for this comment_id

#### Scenario: Code review complete fires after fresh PR comment posts
- **WHEN** an operator-initiated re-review's `review_pr_at_state` returns AND the fresh PR comment is posted successfully
- **THEN** the daemon posts the canonical "Code review complete" text including the verdict
- **AND** the dedup storage records the complete-at timestamp

#### Scenario: Code review failed fires on reviewer error
- **WHEN** an operator-initiated re-review's reviewer pipeline returns `Err(e)` (LLM client failure, validation failure, etc.)
- **THEN** the daemon posts the canonical "Code review failed" text with `<reason>` derived from `e`
- **AND** the dedup storage records the failed-at timestamp

#### Scenario: Reviewer-disabled path does NOT fire complete/failed notifications
- **WHEN** an operator posts `@<bot> code-review` AND `reviewer.enabled: false`
- **THEN** the daemon posts the canonical PR comment `✗ Code review not available: reviewer is disabled in config`
- **AND** the "Code review triggered" chatops notification fires (the dispatcher DID receive the verb)
- **AND** the "Code review complete" notification does NOT fire (the reviewer was not invoked)
- **AND** the "Code review failed" notification does NOT fire (this is not a failure, it's a configuration state)

### Requirement: Diff-overlap-driven re-review suggestion

When `reviewer.suggest_rereview_threshold: f32` is set in config (default unset = disabled), the daemon SHALL post a chatops suggestion notification after each operator-initiated revision iteration's Completed outcome AND successful push, when the iteration's cumulative-since-original-review diff overlap exceeds the threshold.

Overlap is computed as:

```
overlap = lines_changed(state.original_review_head_sha → pr.current_head_sha)
        / lines_changed(pr.base_sha → state.original_review_head_sha)
```

The numerator is the cumulative lines changed across ALL revisions on the PR since the original review's head. The denominator is the lines changed in the original PR diff (the diff the original review evaluated). Both counts SHALL use `git diff --numstat`-equivalent semantics (additions + deletions, ignoring binary files which contribute zero).

The suggestion SHALL fire ONLY when ALL of the following hold:

- `reviewer.suggest_rereview_threshold` is `Some(threshold)`.
- `state.original_review_head_sha` is `Some` (the original review completed AND recorded its head SHA).
- `state.last_suggested_rereview_at_revisions_count != Some(state.revisions_applied)` (we haven't suggested for this revision count yet).
- `overlap >= threshold`.
- `chatops_ctx.failure_alerts_enabled` is `true`.

When the suggestion fires, the daemon SHALL post:

```
💡 `<repo_url>`: PR #<num> has been substantially revised (~<percent>% of original diff changed across <N> revisions). Consider `@<bot> code-review` to re-evaluate.
<pr_url>
```

where `<percent>` is `(overlap * 100).round()` AND `<N>` is `state.revisions_applied`.

After a successful suggestion post, the daemon SHALL set `state.last_suggested_rereview_at_revisions_count = Some(state.revisions_applied)` AND write the state file. This deduplicates the suggestion against the current revision count: the same revision iteration's polling cycles do NOT re-suggest. A subsequent revision iteration that increments `revisions_applied` becomes a fresh opportunity to suggest (gated by the same threshold check).

A successful re-review (via the verb) does NOT reset the deduplication field. The operator's act of running the re-review consumes the suggestion's prompt; we do not re-suggest for the same revision count even if the re-review happened.

When the threshold is unset, NO suggestion fires regardless of overlap. When `original_review_head_sha` is unset (state files from before this change was deployed), NO suggestion fires regardless of threshold OR overlap (graceful degradation; missing baseline is not an error).

#### Scenario: Threshold met fires the suggestion once per revision count
- **WHEN** a revision iteration completes successfully with `revisions_applied: 3`, overlap `0.6`, threshold `0.5`, AND no prior suggestion at count 3
- **THEN** the daemon posts the canonical "💡 ... has been substantially revised" notification with `~60%` AND `3 revisions`
- **AND** `state.last_suggested_rereview_at_revisions_count` is set to `Some(3)`

#### Scenario: Same revision count does NOT re-suggest on subsequent polling cycles
- **WHEN** a subsequent polling cycle runs the same Completed outcome's post-step (e.g. due to the dispatcher's iteration loop running multiple times)
- **AND** `state.last_suggested_rereview_at_revisions_count: Some(3)` AND `state.revisions_applied: 3`
- **THEN** the suggestion does NOT post again
- **AND** the state field is NOT updated

#### Scenario: Threshold unset → no suggestion regardless of overlap
- **WHEN** a revision iteration completes with overlap `0.95` AND `reviewer.suggest_rereview_threshold` is unset
- **THEN** no suggestion is posted

#### Scenario: Missing baseline → no suggestion (graceful degradation)
- **WHEN** `state.original_review_head_sha` is `None` (older state file before this change deployed)
- **AND** a revision iteration completes with any overlap value
- **THEN** no suggestion is posted (the overlap calculation cannot be performed without the baseline)
- **AND** no error is logged at WARN OR higher (the missing field is the expected default for legacy state)

#### Scenario: failure_alerts_enabled gates the suggestion
- **WHEN** all suggestion conditions hold EXCEPT `failure_alerts_enabled` is `false`
- **THEN** no suggestion is posted
- **AND** `state.last_suggested_rereview_at_revisions_count` is NOT updated (so a later toggle to `true` can re-evaluate)

#### Scenario: New revision iteration becomes a fresh suggestion opportunity
- **WHEN** the daemon previously suggested at `revisions_applied: 2` AND a new revision iteration completes with `revisions_applied: 3` AND overlap still exceeds threshold
- **THEN** the suggestion DOES fire (because `last_suggested_rereview_at_revisions_count: Some(2)` != `state.revisions_applied: 3`)
- **AND** `state.last_suggested_rereview_at_revisions_count` updates to `Some(3)`
