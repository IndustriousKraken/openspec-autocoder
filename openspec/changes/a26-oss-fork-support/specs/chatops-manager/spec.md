## ADDED Requirements

### Requirement: Inbound listener recognizes the `sync-upstream` verb AND submits a `SyncUpstreamAction`
The inbound chatops listener SHALL recognize `@<bot> sync-upstream <repo-substring>` as a known operator verb. The listener SHALL parse the repo-substring per the existing case-insensitive match rule AND submit a `SyncUpstreamAction { repo_url, channel, thread_ts, request_id }` over the control socket. The ack message SHALL be a thread reply (NOT a top-level message) since the request operates on existing workspace state rather than initiating a long-running lifecycle. The polling iteration's handler posts the rebase result OR conflict notice as a follow-up reply.

#### Scenario: Happy-path verb queueing
- **WHEN** an operator posts `@<bot> sync-upstream myrepo` AND `myrepo` uniquely resolves to a configured repo
- **THEN** the bot replies in-thread (OR in-channel for a top-level invocation) with `✓ Queued sync-upstream for <repo_url>. Reply incoming.`
- **AND** a `SyncUpstreamAction` is submitted with the resolved `repo_url`

#### Scenario: Ambiguous repo substring
- **WHEN** the repo-substring matches multiple configured repos
- **THEN** the bot replies with the existing `match_repo`-style candidate list
- **AND** no action is submitted

#### Scenario: Missing repo substring
- **WHEN** an operator posts `@<bot> sync-upstream` with no arguments
- **THEN** the bot replies `✗ sync-upstream: missing repo. Usage: @<bot> sync-upstream <repo-substring>`
- **AND** no action is submitted

#### Scenario: Help verb lists sync-upstream
- **WHEN** an operator posts `@<bot> help`
- **THEN** the help output lists `sync-upstream` with the one-line description naming its fork-workflow purpose
