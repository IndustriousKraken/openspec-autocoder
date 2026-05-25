## ADDED Requirements

### Requirement: Status reply always shows live workspace snapshot
The `status` verb's reply SHALL always include five sections regardless of whether the repo has any markers, throttled alerts, or queued changes: (1) `branches: base=<base>, agent=<agent>`; (2) one `last commit on <branch>` line per branch (base and agent), each rendering as `<short_sha> "<subject>" (<age> ago)` when a commit exists or `(none)` when the branch does not exist or has no commits; (3) `latest PR: ...` with a URL on the following line when a PR exists from the agent branch, or `latest PR: (none)` otherwise; (4) `currently: idle` OR `currently: working on <change> (started <age> ago)` based on the per-repo busy marker; (5) the existing `next iteration: in <age> ...` line. These sections SHALL precede the existing marker / throttled-alert / queue sections.

#### Scenario: All sections present for a healthy repo
- **WHEN** an operator issues `status <repo>` against a repo with commits on both branches, an open PR from the agent branch, an idle daemon, and an empty queue
- **THEN** the reply contains all five always-present sections in the documented order
- **AND** the `currently:` line reads `idle`
- **AND** the queue section either reads `queue: 0 pending, 0 waiting, 0 excluded` (one-liner form) or is omitted entirely per the queue-one-liner requirement

#### Scenario: Absent data renders `(none)`, not blank or missing
- **WHEN** the agent branch does not exist yet (fresh clone)
- **THEN** `last commit on <agent_branch>:` reads `(none)`
- **AND** the line is still present (the section is always shown)

#### Scenario: GitHub failure does not break the reply
- **WHEN** the GitHub API call for `latest PR` returns an error (network failure, 4xx, 5xx, rate-limit)
- **THEN** the daemon logs a WARN with the underlying error
- **AND** the reply's `latest PR:` line reads `(none)`
- **AND** every other section is rendered normally
- **AND** the status reply succeeds — the operator gets the local-state half even when GitHub is unreachable

#### Scenario: Local git failure does not break the reply
- **WHEN** `git log -1` returns an error (workspace not yet cloned, .git directory corrupt)
- **THEN** the daemon logs a WARN with the underlying error
- **AND** the affected `last commit on <branch>:` line reads `(none)`
- **AND** every other section is rendered normally

#### Scenario: Currently-busy line reflects the live busy marker
- **WHEN** the daemon is mid-iteration on change `a05-foo` started 2 minutes ago
- **THEN** the `currently:` line reads `working on a05-foo (started 2m ago)`
- **AND** the busy-marker file is read but NOT taken, held, or released by the status path

### Requirement: Queue one-liner for small queues
When `pending_changes`, `waiting_changes`, and the marker-excluded set each contain 5 or fewer entries, the status reply SHALL render the queue as a single line: `queue: N pending (<list>), M waiting (<list>), K excluded`. When any of those lists exceeds 5 entries, the reply SHALL fall back to the existing per-line format (one line per change). Empty lists in the one-liner form SHALL render as `N pending` (no parenthetical) rather than `0 pending ()`.

#### Scenario: All three lists are small → one-liner
- **WHEN** the queue has 2 pending, 1 waiting, 0 excluded changes
- **THEN** the queue section is rendered as one line: `queue: 2 pending (a06-foo, a07-bar), 1 waiting (a10-secrets), 0 excluded`

#### Scenario: A list exceeds 5 entries → per-line fallback
- **WHEN** `pending_changes` has 6 entries
- **THEN** the queue section is rendered in the existing per-line format (one line per change, grouped by status)

#### Scenario: Empty list renders count only
- **WHEN** the queue has 0 pending and the threshold path applies
- **THEN** the one-liner contains `0 pending` (no empty parens)

### Requirement: Slack-escape user-controlled fields
The status formatter SHALL escape Slack-special characters (`<`, `>`, `&`) in every user-controlled string field before including it in the reply text. The escape substitutions SHALL be applied in the order `&` → `&amp;`, then `<` → `&lt;`, then `>` → `&gt;` so the substitution does not double-escape its own output. User-controlled fields in the status reply are: every commit subject, the PR title, and every change name. Operator-controlled or daemon-controlled fields (branch names from config, repo URLs, marker timestamps) are not escaped because they are not author-supplied.

#### Scenario: Commit subject with channel-mention is escaped
- **WHEN** a commit subject is the literal string `<!channel> ping everyone`
- **THEN** the reply contains the escaped form `&lt;!channel&gt; ping everyone`
- **AND** the reply does not contain the literal sequence `<!channel>` that would ping the channel when posted

#### Scenario: PR title with user-mention is escaped
- **WHEN** a PR title is the literal string `<@U123> please review`
- **THEN** the reply contains `&lt;@U123&gt; please review`
- **AND** Slack does NOT render this as a mention because the angle brackets are escaped

#### Scenario: Escape order avoids double-escape
- **WHEN** the input string is `&<`
- **THEN** the escaped output is `&amp;&lt;`
- **AND** the output is NOT `&amp;lt;` (which would be the result of escaping `<` first then `&`)

#### Scenario: Plain ampersand is escaped
- **WHEN** a commit subject contains `foo & bar`
- **THEN** the reply contains `foo &amp; bar`
