# orchestrator-cli — delta for a47-auto-only-revision-caps

## MODIFIED Requirements

### Requirement: Revision cap per PR, with one-time decline
The `executor.max_auto_revisions_per_pr` config (default `5`, capped at `20` with WARN-and-clamp at startup; the legacy name `executor.max_revisions_per_pr` is accepted as a serde alias so existing config files load unchanged) SHALL bound only AUTOMATIC revisions per PR — those triggered by reviewer-marked comments carrying the `<!-- reviewer-revision -->` marker (the code-reviewer auto-revise path). Human-initiated `@<bot> revise` comments SHALL NOT be counted against this cap AND SHALL NOT be declined for cap reasons; an operator's deliberate revision request always processes.

The per-PR state file tracks the automatic-revision count separately from human revisions. When a reviewer-marked (automatic) revision would exceed the cap, the daemon SHALL post a one-time decline comment on the PR AND a chatops notification, then silently ignore subsequent AUTOMATIC triggering comments on that PR (their timestamps still advance so processed comments are not re-evaluated). Human `@<bot> revise` comments continue to process normally regardless of the automatic-cap state.

#### Scenario: First over-cap automatic trigger posts the decline once
- **WHEN** an open PR has had `max_auto_revisions_per_pr` automatic (reviewer-marked) revisions applied AND a new reviewer-marked (`<!-- reviewer-revision -->`) triggering comment arrives
- **THEN** the daemon posts a PR comment whose body starts with `🛑 Revision cap reached`
- **AND** a chatops notification fires whose text starts with `🛑 <repo>: PR #<num> hit the revision cap`
- **AND** `cap_decline_posted` in the per-PR state file is set to `true`

#### Scenario: Subsequent over-cap automatic triggers are silently ignored
- **WHEN** a PR already has `cap_decline_posted: true` AND a new reviewer-marked triggering comment arrives
- **THEN** the daemon advances `last_seen_comment_at` to the new comment's `created_at`
- **AND** no PR reply is posted
- **AND** no chatops notification fires
- **AND** no executor invocation is performed

#### Scenario: Human-initiated revisions are never capped
- **GIVEN** an open PR has reached `max_auto_revisions_per_pr` automatic revisions AND `cap_decline_posted: true`
- **WHEN** an operator posts a human `@<bot> revise <text>` comment (no `<!-- reviewer-revision -->` marker)
- **THEN** the daemon processes the revision normally (executor invoked; commit/push or reported declination; reply comment posted)
- **AND** the automatic-revision counter is NOT incremented
- **AND** no cap-decline comment is posted for the human request

#### Scenario: Legacy `max_revisions_per_pr` config key still works
- **WHEN** a config file sets `executor.max_revisions_per_pr: 8` (the legacy key)
- **THEN** it loads identically to `executor.max_auto_revisions_per_pr: 8` via the serde alias
- **AND** no deprecation warning is emitted (the alias is a silent compatibility path)
