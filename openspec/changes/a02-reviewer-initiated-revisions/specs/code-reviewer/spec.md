## ADDED Requirements

### Requirement: Reviewer-initiated revision comments on Block verdicts
When `reviewer.auto_revise_on_block` is `true` AND the reviewer returns a `Block` verdict, the daemon SHALL post one PR issue comment per concern where the reviewer marked `should_request_revision: true`, subject to the per-PR revision-cap budget. Each comment's body SHALL begin with the marker line `<!-- reviewer-revision -->` followed by a newline, then the trigger pattern `@<bot-username> revise <actionable_request>`. The marker enables the revision dispatcher's self-author-filter bypass; without it the dispatcher would (correctly) filter the comment as bot-authored noise. The feature is off by default; the config flag must be explicitly enabled.

#### Scenario: Off-by-default has no behavioural change
- **WHEN** `reviewer.auto_revise_on_block` is absent OR `false` AND the reviewer returns any verdict
- **THEN** no reviewer-revision comments are posted
- **AND** the existing PR-body `## Code Review` section is the only reviewer output channel

#### Scenario: Block verdict with should-revise concerns posts comments
- **WHEN** `auto_revise_on_block: true` AND the reviewer returns `Block` AND the response contains two concerns with `should_request_revision: true` and non-empty `actionable_request` AND the per-PR remaining cap budget is at least 2
- **THEN** exactly two PR issue comments are posted
- **AND** each comment's body starts with `<!-- reviewer-revision -->\n`
- **AND** each comment's body's second non-whitespace line matches `@<bot-username> revise <actionable_request>` for that concern

#### Scenario: Pass and Concerns verdicts post no revision comments
- **WHEN** `auto_revise_on_block: true` AND the reviewer returns `Pass` OR `Concerns`
- **THEN** no reviewer-revision comments are posted
- **AND** the existing PR-body section behaviour is unchanged

#### Scenario: Concerns without should_request_revision post no comments
- **WHEN** `auto_revise_on_block: true` AND the reviewer returns `Block` AND every concern has `should_request_revision: false`
- **THEN** no reviewer-revision comments are posted
- **AND** the daemon logs a WARN noting that auto-revise is enabled but the reviewer produced no actionable-revision concerns (signals operator that the reviewer template may need updating)

### Requirement: Cap-budget interaction with reviewer-posted comments
The reviewer-posting step SHALL respect the per-PR `executor.max_revisions_per_pr` cap. When the reviewer would generate more should-revise concerns than the remaining cap budget allows, the daemon SHALL post only the first N concerns (where N = remaining budget; concerns are taken in the reviewer's output order, which the reviewer's prompt template instructs to be most-critical-first) AND SHALL annotate the dropped concerns in the PR-body `## Code Review` section so the human sees what was skipped.

#### Scenario: Cap budget exhausted truncates posts and annotates drops
- **WHEN** the reviewer returns Block with 3 should-revise concerns AND the per-PR remaining cap budget is 2
- **THEN** exactly 2 reviewer-revision comments are posted (the first 2 in the reviewer's output order)
- **AND** the PR-body `## Code Review` section contains an entry for the third concern annotated `(not auto-revised; cap budget exhausted)`

#### Scenario: Cap budget zero posts nothing
- **WHEN** the reviewer returns Block with should-revise concerns AND the per-PR remaining cap budget is 0
- **THEN** no comments are posted
- **AND** every should-revise concern is annotated in the PR-body section with `(not auto-revised; cap budget exhausted)`

### Requirement: Self-author filter exception for reviewer-revision comments
The revision dispatcher from `a01-pr-comment-revision-loop` SHALL permit bot-authored comments whose body's first non-whitespace text is the literal HTML-comment marker `<!-- reviewer-revision -->` to bypass its self-author filter. All other bot-authored comments — the dispatcher's own `✅ Revision applied:` / `✗ Revision attempt failed:` replies, the cap-decline message, any future bot-posted content — SHALL continue to be filtered as today.

#### Scenario: Reviewer-marked comment bypasses self-author filter
- **WHEN** the dispatcher fetches a comment whose `user_login == self_bot_username` AND whose body starts with `<!-- reviewer-revision -->\n@<bot-username> revise foo`
- **THEN** the dispatcher passes the body to `parse_revision_trigger`
- **AND** the parser returns `Some("foo")`
- **AND** the dispatcher executes the revision normally

#### Scenario: Unmarked bot-authored comment continues to be filtered
- **WHEN** the dispatcher fetches a comment whose `user_login == self_bot_username` AND whose body does NOT start with the marker (e.g. body is `✅ Revision applied: foo`)
- **THEN** the comment is filtered out before parsing
- **AND** no recursive revision is triggered

#### Scenario: Human-authored comment is unaffected by the marker rule
- **WHEN** the dispatcher fetches a comment whose `user_login != self_bot_username`
- **THEN** the self-author filter is irrelevant
- **AND** the comment proceeds to `parse_revision_trigger` regardless of whether the body contains the marker

### Requirement: Backwards compatibility for unaware reviewer templates
Operators with customized reviewer templates that have NOT been updated to emit the new `actionable_request` and `should_request_revision` per-concern fields SHALL see no behavioural change: the response parser defaults missing fields to `actionable_request: None` and `should_request_revision: false`, the posting step finds zero should-revise concerns, and posts zero reviewer-revision comments. The daemon SHALL log a one-shot WARN on the first reviewer-pass in such a session when `auto_revise_on_block` is enabled, naming the gap so operators see the actionable diagnostic.

#### Scenario: Customized template missing the new fields produces no comments
- **WHEN** the reviewer's response (from an operator-customized template that pre-dates this change) contains concerns without `should_request_revision` fields AND `auto_revise_on_block: true` AND the verdict is Block
- **THEN** the parser defaults `should_request_revision: false` for every concern
- **AND** zero reviewer-revision comments are posted
- **AND** the daemon logs a WARN naming the gap and pointing at the prompt-template documentation

### Requirement: No reviewer re-run after a reviewer-initiated revision lands
The reviewer SHALL run exactly once per polling iteration's executor pass, as today. A reviewer-initiated revision committed in a subsequent iteration SHALL NOT trigger a re-evaluation by the reviewer; the verdict from the original pass is "frozen" for the life of the PR. Operators wanting iterative reviewer evaluation can manually re-issue the iteration (e.g. via `autocoder rewind` or by closing + re-opening the PR), or wait for a separate change that adds reviewer re-evaluation as an explicit feature.

#### Scenario: Reviewer does not re-run when a revision lands
- **WHEN** a reviewer-initiated revision is committed and force-pushed in iteration N+1
- **THEN** the reviewer is NOT invoked again in iteration N+1
- **AND** the existing `## Code Review` section in the PR body is not updated
- **AND** the PR's draft status (set by the original Block verdict) is preserved
