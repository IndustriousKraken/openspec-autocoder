# code-reviewer — delta for a46-auto-revise-fires-on-actionable

## REMOVED Requirements

### Requirement: Reviewer-initiated revision comments on Block verdicts

Replaced by "Reviewer-initiated revision comments on actionable concerns" (ADDED below). The Block-verdict gate made auto-revise dormant for conservative reviewers that emit `Concerns` for actionable findings; the trigger is moving from the verdict signal to the per-concern `should_request_revision` signal.

## ADDED Requirements

### Requirement: Reviewer-initiated revision comments on actionable concerns
When `reviewer.auto_revise` is `true`, the daemon SHALL post one PR issue comment per concern where the reviewer marked `should_request_revision: true` AND supplied a non-empty `actionable_request`, REGARDLESS of the review's verdict (`Pass`, `Concerns`, OR `Block`), subject to the per-PR revision-cap budget. Each comment's body SHALL begin with the marker line `<!-- reviewer-revision -->` followed by a newline, then the trigger pattern `@<bot-username> revise <actionable_request>`. The marker enables the revision dispatcher's self-author-filter bypass; without it the dispatcher would (correctly) filter the comment as bot-authored noise. The feature is off by default; the config flag must be explicitly enabled.

The verdict is no longer consulted when deciding whether to post reviewer-revision comments. The `Block` verdict retains its separate effect of marking the PR as draft (per the existing draft-on-Block behavior); it simply no longer gates auto-revise. The actionability signal is the per-concern `should_request_revision` + `actionable_request` pair.

The config flag is `reviewer.auto_revise`. The legacy name `reviewer.auto_revise_on_block` SHALL continue to be accepted as an alias so existing config files load unchanged.

The per-PR revision-cap budget that bounds this posting (currently `executor.max_revisions_per_pr`) is unchanged by this requirement; it bounds all reviewer-revision posts the same as today. (Refining that cap to bound only automatic chains while uncapping human-initiated revisions is a separate change.)

#### Scenario: Off-by-default has no behavioural change
- **WHEN** `reviewer.auto_revise` is absent OR `false` AND the reviewer returns any verdict
- **THEN** no reviewer-revision comments are posted
- **AND** the existing PR-body `## Code Review` section is the only reviewer output channel

#### Scenario: Concerns verdict with actionable concerns posts comments
- **WHEN** `auto_revise: true` AND the reviewer returns `Concerns` AND the response contains two concerns with `should_request_revision: true` and non-empty `actionable_request` AND the per-PR remaining cap budget is at least 2
- **THEN** exactly two PR issue comments are posted
- **AND** each comment's body starts with `<!-- reviewer-revision -->\n`
- **AND** each comment's body's second non-whitespace line matches `@<bot-username> revise <actionable_request>` for that concern

#### Scenario: Pass verdict with an actionable concern posts a comment
- **WHEN** `auto_revise: true` AND the reviewer returns `Pass` AND one concern has `should_request_revision: true` with a non-empty `actionable_request` AND the remaining cap budget is at least 1
- **THEN** exactly one reviewer-revision comment is posted (the verdict does NOT gate posting)

#### Scenario: Block verdict with actionable concerns still posts comments
- **WHEN** `auto_revise: true` AND the reviewer returns `Block` AND the response contains concerns with `should_request_revision: true` and non-empty `actionable_request` within the remaining cap budget
- **THEN** one reviewer-revision comment per such concern is posted (the Block path is preserved, not regressed)
- **AND** the PR is also marked draft per the existing draft-on-Block behavior

#### Scenario: No actionable concerns posts nothing under any verdict
- **WHEN** `auto_revise: true` AND the reviewer returns any verdict AND every concern has `should_request_revision: false` OR an empty `actionable_request`
- **THEN** no reviewer-revision comments are posted
- **AND** the daemon logs a WARN noting that auto-revise is enabled but the reviewer produced no actionable-revision concerns (signals operator that the reviewer template may need updating)

#### Scenario: Legacy `auto_revise_on_block` config key still works
- **WHEN** a config file sets `reviewer.auto_revise_on_block: true` (the legacy key)
- **THEN** it loads identically to `reviewer.auto_revise: true` via the serde alias
- **AND** no deprecation warning is emitted (the alias is a silent compatibility path)

## MODIFIED Requirements

### Requirement: Backwards compatibility for unaware reviewer templates
Operators with customized reviewer templates that have NOT been updated to emit the new `actionable_request` and `should_request_revision` per-concern fields SHALL see no behavioural change: the response parser defaults missing fields to `actionable_request: None` and `should_request_revision: false`, the posting step finds zero should-revise concerns, and posts zero reviewer-revision comments. The daemon SHALL log a one-shot WARN on the first reviewer-pass in such a session when `reviewer.auto_revise` is enabled, naming the gap so operators see the actionable diagnostic.

#### Scenario: Customized template missing the new fields produces no comments
- **WHEN** the reviewer's response (from an operator-customized template that pre-dates this change) contains concerns without `should_request_revision` fields AND `reviewer.auto_revise: true`
- **THEN** the parser defaults `should_request_revision: false` for every concern
- **AND** zero reviewer-revision comments are posted
- **AND** the daemon logs a WARN naming the gap and pointing at the prompt-template documentation
