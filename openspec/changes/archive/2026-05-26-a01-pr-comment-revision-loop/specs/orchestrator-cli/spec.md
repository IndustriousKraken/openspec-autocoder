## ADDED Requirements

### Requirement: PR comments matching `@<bot> revise <text>` trigger an in-place revision of the autocoder-opened PR
Each polling iteration, before processing pending changes for a repository, the daemon SHALL fetch open pull requests whose head branch matches `repositories[].agent_branch` AND poll each one's issue comments for revision-trigger messages. A comment qualifies as a trigger when its body's first non-whitespace token is `@<bot-username>` (case-insensitive on the username) AND its next whitespace-separated token (case-insensitive) is `revise` AND at least one non-whitespace character follows. The revision text is everything after `revise` with leading whitespace trimmed. Comments authored by the bot itself (`user.login == self.bot_username`) SHALL be filtered before parsing. The bot's GitHub username SHALL be learned at startup via `GET /user` and cached for the process lifetime.

#### Scenario: Triggering comment is detected
- **WHEN** an open PR has a new comment whose body is `@<bot> revise the find_user function drops error info`
- **THEN** the daemon parses the body as a revision trigger
- **AND** extracts the revision text `the find_user function drops error info`

#### Scenario: Non-triggering comment is ignored
- **WHEN** an open PR has a new comment whose body is `@<bot> looks good`
- **THEN** the daemon does NOT treat the body as a trigger
- **AND** no revision is attempted

#### Scenario: Bot's own comments are filtered
- **WHEN** the daemon's previous revision reply (`✅ Revision applied: ...`) appears in the comment fetch
- **THEN** the daemon filters it out before parsing
- **AND** the same reply does not trigger a recursive revision

### Requirement: Revision execution updates the agent branch and posts a reply comment
On a triggering comment for an open PR, the daemon SHALL re-invoke the executor in revision mode (passing the original change material, the current PR diff, and the revision text). The executor's outcome drives the next step: `Completed` → commit + force-with-lease push + success reply comment; `AskUser` → existing chatops escalation (no commit, no count increment, no PR reply yet, revision is treated as in-progress); `Failed` → failure reply comment + count increment.

#### Scenario: Completed revision updates the PR
- **WHEN** the executor returns `Completed` for a revision context
- **THEN** the daemon commits the workspace changes with subject `revise: <change>: <first 60 chars of revision text>`
- **AND** force-pushes with `--force-with-lease` to `repositories[].agent_branch`
- **AND** posts a PR issue comment whose body starts with `✅ Revision applied:`
- **AND** the PR's diff updates to reflect the revision

#### Scenario: AskUser during revision escalates without committing
- **WHEN** the executor returns `AskUser { question, resume_handle }` during revision execution
- **THEN** the existing chatops escalation path fires (the question is posted to the configured channel)
- **AND** no commit is made on the agent branch
- **AND** no PR reply comment is posted
- **AND** the revision-count counter is NOT incremented
- **AND** the comment's `created_at` is NOT marked as processed (so the next iteration after the human answer can resume against the same trigger comment)

#### Scenario: Failed revision posts a failure comment
- **WHEN** the executor returns `Failed { reason }` for a revision context
- **THEN** the daemon posts a PR issue comment whose body starts with `✗ Revision attempt failed:` and includes the reason
- **AND** the revision-count counter IS incremented (a failed attempt counts toward the cap)
- **AND** no commit or push is made

### Requirement: Revision cap per PR, with one-time decline
The `executor.max_revisions_per_pr` config (default `5`, capped at `20` with WARN-and-clamp at startup) bounds revisions per PR. When the cap is reached, the daemon SHALL post a one-time decline comment on the PR AND a chatops notification, then silently ignore subsequent triggering comments on that PR (timestamps still advance so processed comments are not re-evaluated).

#### Scenario: First over-cap trigger posts the decline once
- **WHEN** an open PR has had `max_revisions_per_pr` revisions applied AND a new triggering comment arrives
- **THEN** the daemon posts a PR comment whose body starts with `🛑 Revision cap reached`
- **AND** a chatops notification fires whose text starts with `🛑 <repo>: PR #<num> hit the revision cap`
- **AND** `cap_decline_posted` in the per-PR state file is set to `true`

#### Scenario: Subsequent over-cap triggers are silently ignored
- **WHEN** a PR already has `cap_decline_posted: true` AND a new triggering comment arrives
- **THEN** the daemon advances `last_seen_comment_at` to the new comment's `created_at`
- **AND** no PR reply is posted
- **AND** no chatops notification fires
- **AND** no executor invocation is performed

### Requirement: Revisions block per-repo queue, take priority over pending changes
The revision dispatcher SHALL run synchronously inside the polling iteration, AFTER waiting-change processing AND BEFORE pending-change processing. Revisions on different repos SHALL run independently (cross-repo polling tasks SHALL NOT be affected by another repo's in-flight revision). On a same-repo iteration, all open-PR revision requests SHALL be processed in PR-number order before the pending-change walk begins.

#### Scenario: Revision in flight blocks pending walk on the same repo
- **WHEN** a polling iteration begins for a repo with one open-PR revision request AND two pending changes
- **THEN** the revision is processed first
- **AND** the pending-change walk begins only after the revision completes (or escalates via AskUser)

#### Scenario: Cross-repo revisions are independent
- **WHEN** repo A's polling iteration is processing a revision AND repo B's polling iteration is processing a pending change
- **THEN** the two proceed independently in their own per-repo tasks

#### Scenario: AskUser during revision blocks the rest of the iteration (same as AskUser during a pending change)
- **WHEN** a revision raises `AskUser` AND the iteration also had a pending change queued
- **THEN** the pending change is NOT processed in this iteration
- **AND** the existing same-repo serial-queue invariant from the AskUser path applies

### Requirement: Per-PR state file persists revision count and last-seen timestamp; closed PRs are pruned
Each open PR being tracked has a state file at `<workspace>/.autocoder/revisions/<pr_number>.json` containing `pr_number`, `agent_branch`, `last_seen_comment_at`, `revisions_applied`, `revision_cap`, and `cap_decline_posted`. At iteration start, before any comment fetching, the daemon SHALL prune state files whose PR number is no longer in the set of open PRs returned by `list_open_prs_for_head`.

#### Scenario: Closed PRs have their state pruned
- **WHEN** a polling iteration runs AND a previously-tracked PR is no longer in the open-PRs response
- **THEN** the state file at `<workspace>/.autocoder/revisions/<pr_number>.json` is removed
- **AND** no future revision processing references that PR

#### Scenario: New PR initializes state lazily
- **WHEN** a polling iteration sees an open PR that has no existing state file AND the PR has new comments
- **THEN** a fresh `RevisionState` is initialized with `last_seen_comment_at = pr.created_at`, `revisions_applied = 0`, `cap_decline_posted = false`, and the resolved `revision_cap`
- **AND** the state is written to disk after any comment processing

#### Scenario: State writes are atomic
- **WHEN** the daemon writes a `RevisionState` file
- **THEN** the write uses temp-file-then-rename (matching the daemon's other state-file writes)
- **AND** an interrupted write does NOT leave a partial canonical file on disk
