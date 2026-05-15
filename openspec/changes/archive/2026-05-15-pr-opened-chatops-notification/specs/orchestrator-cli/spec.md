## ADDED Requirements

### Requirement: PR-opened ChatOps notification
After successfully creating a Pull Request via the GitHub API, autocoder SHALL post a one-line notification to the configured ChatOps channel naming the repository, the new PR's URL, and the number of changes included. The notification SHALL be best-effort: a ChatOps post failure is logged at WARN and does NOT cause the iteration to fail or block the existing post-PR comment step. The notification is suppressed when ChatOps is not configured OR when `chatops.notifications.pr_opened` is explicitly `false`.

#### Scenario: PR-opened post fires on successful creation
- **WHEN** `github::create_pull_request` returns `Ok(pr)` for the
  current pass AND ChatOps is configured AND
  `chatops.notifications.pr_opened` is unset OR set to `true`
- **THEN** autocoder posts a single ChatOps notification to the
  repository's resolved channel containing the literal repository
  URL, the literal `pr.html_url`, and the count of archived changes
  in the pass
- **AND** the post happens AFTER the PR creation succeeds AND BEFORE
  the existing post-PR implementer-summary comment step (so a
  failure of the latter never blocks the former)

#### Scenario: PR-opened post is suppressed when notifications.pr_opened is false
- **WHEN** ChatOps is configured AND
  `chatops.notifications.pr_opened` is explicitly `false`
- **THEN** autocoder does NOT post a PR-opened notification
- **AND** the existing INFO log line `"opened PR pr=<url>"` is
  emitted unchanged so operators tailing journalctl still see the
  event

#### Scenario: PR-opened post is suppressed when ChatOps is not configured
- **WHEN** the daemon's `chatops:` config block is absent
- **THEN** autocoder does NOT attempt any ChatOps post
- **AND** the iteration proceeds to the post-PR comment step
  exactly as it does today

#### Scenario: PR-opened post failure does not fail the iteration
- **WHEN** ChatOps is configured AND `notifications.pr_opened` is
  true AND the ChatOps backend's `post_notification` call returns
  `Err`
- **THEN** autocoder logs a WARN line naming the repository URL,
  the PR URL, and the error
- **AND** the iteration continues normally; the post-PR comment
  step still runs and the iteration's outcome is unchanged
- **AND** no chatops-failure alert is emitted (chatops failures are
  never re-routed through chatops, matching the existing
  `handle_predictable_failure` convention)

#### Scenario: PR-opened post uses the per-repo channel override
- **WHEN** the PR-opened notification is about to fire AND the
  current repository has `chatops_channel_id` set to a value
  different from `chatops.default_channel_id`
- **THEN** the notification posts to the per-repo channel, not the
  default channel
- **AND** the channel resolution matches the channel used for
  start-of-work and failure-alert notifications for the same
  repository

### Requirement: Notifications config gains pr_opened flag
`chatops.notifications` SHALL include an optional `pr_opened: bool` field that defaults to `true` when unset. The flag SHALL be the sole knob controlling whether the PR-opened notification fires; no other config field affects it.

#### Scenario: pr_opened defaults to true when notifications block is absent
- **WHEN** the operator's config has no `chatops.notifications`
  block at all
- **THEN** the effective `pr_opened` flag is `true`

#### Scenario: pr_opened defaults to true when notifications block is present but field is unset
- **WHEN** the operator's config has `chatops.notifications` with
  `start_work` and/or `failure_alerts` set but no `pr_opened` key
- **THEN** the effective `pr_opened` flag is `true`

#### Scenario: pr_opened explicit false suppresses the post
- **WHEN** the operator sets `chatops.notifications.pr_opened: false`
- **THEN** the effective flag is `false` and the PR-opened post
  does NOT fire
