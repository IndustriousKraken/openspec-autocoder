## ADDED Requirements

### Requirement: `daemon_started` is a recognized notification category in the emoji-prefixed family
The chatops notification surface SHALL admit a new category, `daemon_started`, whose visual signature is the `🆙` emoji prefix — consistent with the existing per-event emoji family (`🚀` start-of-work, `⚠️` failure alert, `🔍` proposal created, `📐` `🧭` `📋` audit findings, `❌` validation exhausted, `🛑` revision cap, `✅` PR opened / clean audit). The category SHALL be dispatched via the existing `ChatOpsBackend::post_notification` surface (no thread; single-line message) — there is no body content that benefits from threading.

#### Scenario: `daemon_started` notification shape matches the emoji family
- **WHEN** the daemon emits a startup version notification
- **THEN** the message text begins with `🆙 ` (the emoji followed by a single space)
- **AND** the format is `🆙 autocoder v<X.Y.Z> started — <N> repository(ies) configured`
- **AND** the notification routes through `post_notification`, NOT through `post_notification_with_thread`
- **AND** the message is short enough that no truncation logic is needed (the longest plausible form fits well under Slack's 40,000-character limit)

#### Scenario: `daemon_started` is independent of other notification toggles
- **WHEN** the operator's config sets `chatops.notifications.start_work: false`
- **THEN** the `daemon_started` notification still fires on next startup
- **AND** the `start_work` flag continues to gate ONLY the per-change `🚀` notifications it was designed for
- **AND** the two notification surfaces are operationally independent
