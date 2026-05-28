## ADDED Requirements

### Requirement: Inbound listener recognizes the `scout` verb AND submits a `ScoutAction`
The inbound chatops listener SHALL recognize `@<bot> scout <repo-substring> [optional guidance]` as a known verb. The listener SHALL parse:

- `<repo-substring>` — case-insensitive substring match via the existing `match_repo` rule.
- Optional guidance — everything after the repo-substring (trimmed, line breaks preserved, capped at 10,000 characters).

On a unique repo match AND `features.scout.enabled: true`, the dispatcher SHALL: generate a `request_id`, post a top-level ack `✓ Queued scout for <repo_url>. The next polling iteration will run it (~Nm). Follow along in this thread.`, capture the ack's `ts` as `thread_ts`, AND submit `ScoutAction { repo_url, guidance: Option<String>, channel, thread_ts, request_id }` over the control socket.

#### Scenario: Happy-path queueing
- **WHEN** an operator posts `@<bot> scout myrepo` AND `myrepo` uniquely resolves AND scout is enabled
- **THEN** the bot posts the top-level ack containing the `Follow along in this thread.` phrase
- **AND** a `ScoutAction` is submitted with `guidance: None`
- **AND** the per-repo `pending_scout_requests` queue gains the request_id

#### Scenario: Happy-path with guidance
- **WHEN** an operator posts `@<bot> scout myrepo focus on security fixes and helpful error messages`
- **THEN** the `ScoutAction.guidance` is `focus on security fixes and helpful error messages`
- **AND** the polling iteration passes the guidance to the scout prompt

#### Scenario: Scout disabled per workspace
- **WHEN** the resolved repo has `features.scout.enabled: false`
- **THEN** the bot replies `✗ scout: disabled in this workspace's config (features.scout.enabled=false).`
- **AND** no state file is written AND no action is submitted

#### Scenario: Ambiguous repo substring
- **WHEN** the repo-substring matches multiple configured repos
- **THEN** the bot replies with the existing `match_repo`-style candidate list
- **AND** no action is submitted

### Requirement: Inbound listener recognizes the `spec-it` verb when posted in a scout lifecycle thread
The inbound listener SHALL recognize `@<bot> spec-it <item-number> [optional guidance]` ONLY when the message is posted as a reply within a known scout lifecycle thread. The listener SHALL identify a scout thread by looking up the parent message's `ts` across the per-repo set of `ScoutRunState.thread_ts` values; a match means the reply is in-scope.

On a valid in-thread invocation, the listener SHALL: parse the item-number as a positive integer; validate it against the scout's item ids; submit `SpecItAction { repo_url, scout_request_id, item_id, guidance: Option<String>, channel, thread_ts }`.

#### Scenario: Valid spec-it within a scout thread
- **WHEN** an operator replies `@<bot> spec-it 3` inside a scout lifecycle thread for `myrepo` AND the scout's item list contains an item with `id: 3`
- **THEN** a `SpecItAction` is submitted with `item_id: 3`, `guidance: None`
- **AND** the polling iteration translates it into a `ProposeRequest`

#### Scenario: Spec-it with operator guidance refining scope
- **WHEN** an operator replies `@<bot> spec-it 5 stick to the OAuth scope, ignore the rate-limit angle`
- **THEN** the `SpecItAction.guidance` is `stick to the OAuth scope, ignore the rate-limit angle`
- **AND** the polling iteration concatenates the scouted item body with the guidance before submitting the propose-request

#### Scenario: Spec-it outside a scout thread is rejected
- **WHEN** an operator posts `@<bot> spec-it 3` at top level (not in a scout thread) OR in a non-scout thread
- **THEN** the bot replies `✗ spec-it: only valid as a reply in a scout thread. Run @<bot> scout <repo> first.`
- **AND** no action is submitted

#### Scenario: Non-integer item-number is rejected
- **WHEN** an operator replies `@<bot> spec-it foo` inside a scout thread
- **THEN** the bot replies `✗ spec-it: foo is not a valid item number. Usage: @<bot> spec-it <N> [guidance]`
- **AND** no action is submitted

#### Scenario: Out-of-range item-number is rejected
- **WHEN** an operator replies `@<bot> spec-it 999` inside a scout thread whose list has 12 items
- **THEN** the bot replies `✗ spec-it: item #999 not in this scout's list (range: 1..12).`
- **AND** no action is submitted

### Requirement: Inbound listener recognizes the `clear-scout` verb
The inbound listener SHALL recognize `@<bot> clear-scout <repo-substring>` as an operator-recovery verb (alongside `clear-perma-stuck`, `clear-revision`, `wipe-workspace`, etc.). The listener SHALL parse the repo-substring per the existing match rule AND submit `ClearScoutAction { repo_url, channel, thread_ts }`.

#### Scenario: Clear-scout submits the action
- **WHEN** an operator posts `@<bot> clear-scout myrepo` AND the repo resolves uniquely
- **THEN** a `ClearScoutAction` is submitted
- **AND** the polling iteration handles deletion AND replies in the same channel with the count cleared

#### Scenario: Clear-scout with no runs present
- **WHEN** an operator posts `@<bot> clear-scout myrepo` AND no `ScoutRunState` files exist for that repo
- **THEN** the polling iteration deletes zero files
- **AND** replies `✓ Cleared 0 scout run(s) for <repo_url>.` (the verb is idempotent)

#### Scenario: Help verb lists the new verbs
- **WHEN** an operator posts `@<bot> help`
- **THEN** the help output lists `scout`, `spec-it`, AND `clear-scout`
- **AND** scout's description names its chat-driven-workflow placement
- **AND** spec-it's description notes it is scout-thread-only
- **AND** clear-scout's description names it as an operator-recovery verb
