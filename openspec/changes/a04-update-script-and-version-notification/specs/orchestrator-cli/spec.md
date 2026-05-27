## ADDED Requirements

### Requirement: Daemon emits a startup version notification on every successful boot
After `autocoder run`'s startup pipeline completes (configs validated, chatops backend constructed, repositories enumerated) AND before the first polling iteration begins, the daemon SHALL post a one-line notification to chatops naming the binary version AND the count of configured repositories. The notification SHALL fire on every successful startup — not only after an `update.sh`-driven restart — because every restart is a meaningful operator signal. The notification SHALL be suppressed when no chatops backend is configured AND SHALL NOT be gated by any flag under `chatops.notifications.*` (those flags govern per-change and per-event signals; the startup line is a daemon-lifecycle signal).

#### Scenario: Startup notification fires once per boot with version and repo count
- **WHEN** the daemon starts up against a config with `chatops.provider: slack` AND 3 configured repositories
- **THEN** exactly one `post_notification` call fires to the resolved default channel
- **AND** the message contains the literal `🆙` prefix
- **AND** the message contains `autocoder v<X.Y.Z>` where `X.Y.Z` matches `env!("CARGO_PKG_VERSION")`
- **AND** the message contains `3 repository(ies) configured`
- **AND** the notification fires before any polling iteration begins

#### Scenario: No chatops backend suppresses the notification
- **WHEN** the daemon starts up against a config with no `chatops:` block
- **THEN** no `post_notification` call fires
- **AND** the daemon emits an INFO log line `startup version: v<X.Y.Z>; <N> repositories` to journalctl as the fallback signal
- **AND** the daemon proceeds to the polling loop without error

#### Scenario: Notification is not gated by `notifications.*` flags
- **WHEN** the daemon starts up against a config with `chatops.notifications.start_work: false` AND `chatops.notifications.failure_alerts: false` AND `chatops.notifications.pr_opened: false`
- **THEN** the startup version notification STILL fires (those flags do not apply to lifecycle signals)
- **AND** an operator who silenced per-change signals still sees the once-per-boot version line

#### Scenario: Notification failure is non-fatal
- **WHEN** the chatops backend's `post_notification` call errors (network blip, channel renamed, scope revoked)
- **THEN** the daemon logs a WARN naming the error AND proceeds to the polling loop
- **AND** no startup is blocked by a notification failure
