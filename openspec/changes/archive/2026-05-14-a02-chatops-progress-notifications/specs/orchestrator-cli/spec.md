## ADDED Requirements

### Requirement: Start-of-work chatops notification
autocoder SHALL post a one-line ChatOps notification each time a
pending change is dequeued and locked for execution, naming the
repository URL, the change name, and the first non-empty line of the
change's `## Why` section. The notification SHALL be suppressed when
`slack.notifications.start_work` is `false` OR when no `slack:` block
is configured.

#### Scenario: Change dequeued with notifications enabled
- **WHEN** a pending change is dequeued in `walk_queue` AND the
  change's `.in-progress` lock has been created AND
  `slack.notifications.start_work` is unset OR `true`
- **THEN** autocoder calls
  `chatops.post_notification(channel, text)` BEFORE invoking the
  executor on that change
- **AND** the text matches the form
  ``🚀 `<repo-url>`: starting work on `<change-name>` — <first-line-of-Why>``
- **AND** if `post_notification` itself fails, the failure is logged
  to stderr but does NOT prevent the executor from running

#### Scenario: Change dequeued with notifications disabled
- **WHEN** a pending change is dequeued AND
  `slack.notifications.start_work` is `false`
- **THEN** no notification is posted
- **AND** the executor proceeds as normal

#### Scenario: Change dequeued without any chatops config
- **WHEN** a pending change is dequeued AND no `slack:` block is in
  `config.yaml`
- **THEN** no notification is posted (no chatops backend to call)
- **AND** the executor proceeds as normal

### Requirement: Throttled predictable-failure alerts
autocoder SHALL emit a ChatOps notification at most once every 24
hours per (repository, failure category) combination for three
categories of predictable infrastructure failure:
`workspace_init_failure`, `branch_push_failure`,
`pr_creation_failure`. Throttle state SHALL be persisted in a
per-workspace `.alert-state.json` file and cleared on the next
successful iteration of the same repository.

#### Scenario: First failure in a category alerts immediately
- **WHEN** any of the three categorized failures occurs in a
  repository whose `.alert-state.json` has no entry for that category
  AND `slack.notifications.failure_alerts` is unset OR `true`
- **THEN** autocoder calls `chatops.post_notification(channel, text)`
  with category-specific text containing the repo URL, a
  category label, and a truncated error excerpt (max 200 chars)
- **AND** on successful post, autocoder writes the category's
  `last_alerted_at` (current UTC) and `last_error_excerpt` to
  `.alert-state.json` atomically (tempfile-then-rename)

#### Scenario: Repeat failure within 24h is silent
- **WHEN** a categorized failure occurs in a repository whose
  `.alert-state.json` has an entry for that category with
  `last_alerted_at` within the past 24 hours
- **THEN** no notification is posted for that iteration
- **AND** `.alert-state.json` is NOT modified

#### Scenario: Repeat failure beyond 24h re-alerts
- **WHEN** a categorized failure occurs AND
  `now - last_alerted_at >= 24h`
- **THEN** a new notification is posted with the most recent error
  excerpt
- **AND** `last_alerted_at` is updated to the current UTC time

#### Scenario: Success clears alert state
- **WHEN** an iteration of a repository completes its
  `run_pass_through_commits` workflow without returning Err
  (regardless of whether any changes were processed or whether the
  queue was empty)
- **THEN** autocoder removes `.alert-state.json` from that
  repository's workspace (or writes an empty `{ "alerts": {} }` map,
  equivalent semantics)
- **AND** the next failure of any category re-alerts immediately

#### Scenario: Alert post failure does NOT update state
- **WHEN** a categorized failure occurs AND the 24h window is open
  AND `post_notification` itself returns Err
- **THEN** the failure is logged to stderr including the alert text
  that would have been posted
- **AND** `.alert-state.json` is NOT updated (so the next iteration
  re-attempts the alert immediately)

#### Scenario: Failure-alerts disabled
- **WHEN** `slack.notifications.failure_alerts` is `false`
- **THEN** no failure alerts are posted regardless of category or
  history
- **AND** `.alert-state.json` is NEITHER read NOR written
- **AND** the failure still produces the existing stderr log line

#### Scenario: Out-of-scope failures are not alerted
- **WHEN** an executor returns `Failed` OR the reviewer LLM call
  fails OR `post_notification` itself fails
- **THEN** no failure alert is posted (these categories are out of
  scope for this change)

### Requirement: Notifications config schema
autocoder SHALL accept an optional `notifications:` sub-block inside
the existing `slack:` config block with two optional boolean fields:
`start_work` and `failure_alerts`. Both default to `true` when the
sub-block is absent OR when an individual key is omitted.

#### Scenario: notifications block absent
- **WHEN** `config.yaml`'s `slack:` block has no `notifications:` key
- **THEN** both `start_work` and `failure_alerts` are effectively `true`

#### Scenario: notifications block partially populated
- **WHEN** `slack.notifications.start_work` is set to `false` AND
  `failure_alerts` is omitted
- **THEN** `start_work` is `false` AND `failure_alerts` defaults to
  `true`

#### Scenario: invalid notifications field rejected
- **WHEN** `slack.notifications:` contains a key other than
  `start_work` or `failure_alerts`
- **THEN** `Config::load_from` returns an error naming the offending
  field
