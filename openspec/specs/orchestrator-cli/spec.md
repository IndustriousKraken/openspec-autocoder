# orchestrator-cli Specification

## Purpose
TBD - created by archiving change orchestrator-architecture. Update Purpose after archive.
## Requirements
### Requirement: Daemon entry point
The orchestrator SHALL provide a `run` subcommand that loads a YAML configuration file and starts an asynchronous polling loop for each configured repository, terminating only on signal (SIGINT/SIGTERM) or fatal initialization error. In each polling iteration, the orchestrator SHALL process waiting (escalated) changes BEFORE pending (fresh) changes. If after the waiting-processing step ANY change in the same repository is still waiting, the orchestrator SHALL skip the pending-change loop for that iteration. The pending-change loop SHALL halt on the first non-Archive outcome (`Failed` or `Escalated`); remaining pending changes wait for the next iteration. Together these rules preserve the architecture's serial-queue invariant — pending changes are not processed while an earlier-or-equal change is unresolved, AND a mid-iteration failure does not let later (potentially dependent) changes proceed past an unfixed earlier one. **The binary that exposes this subcommand is named `autocoder`; the full invocation is `autocoder run --config <path>`.**

#### Scenario: Iteration processes waiting changes before pending
- **WHEN** a polling iteration begins for a repository
- **THEN** the orchestrator first calls `queue::list_waiting(workspace)` and processes each waiting change in order
- **AND** only after all waiting changes have been processed does the orchestrator call `queue::list_pending(workspace)` and process pending changes

#### Scenario: Resuming a change after an answer arrives
- **WHEN** the orchestrator processes a waiting change AND `chatops_manager.poll_thread_for_human_reply` returns `Some(reply)`
- **THEN** the orchestrator (in this exact order) writes `.answer.json` containing the reply, reads `resume_handle` from `.question.json`, deletes `.question.json`, calls `executor.resume(resume_handle, &reply.text)`, and on any returned outcome deletes `.answer.json`
- **AND** the resumed call's outcome is handled identically to a fresh `executor.run` outcome: `Completed` ⇒ commit (if diff exists) and archive; `AskUser` ⇒ post a new question and write a fresh `.question.json` (after deleting `.answer.json`); `Failed` ⇒ log the reason naming the change

#### Scenario: Initial AskUser handling during pending iteration
- **WHEN** `executor.run` returns `Ok(ExecutorOutcome::AskUser { question, resume_handle })` during a pending-change iteration
- **THEN** the orchestrator calls `chatops_manager.post_question(channel, change, &question)` to obtain `thread_ts`, then writes `.question.json` containing the `thread_ts`, channel id, `resume_handle`, and current RFC3339 timestamp under key `asked_at`
- **AND** the orchestrator unlocks the change by removing `.in-progress`
- **AND** the change is NOT archived; it remains in the workspace and is enumerated by `list_waiting` on subsequent iterations
- **AND** the orchestrator halts the pending-change loop for this iteration (the just-escalated change is now waiting; subsequent pending changes may depend on it and SHALL NOT be attempted until the next iteration after the human reply arrives)

#### Scenario: Channel resolution per change
- **WHEN** the orchestrator needs the Slack channel id for a change
- **THEN** the orchestrator uses `repository.slack_channel_id` if set on the per-repo config
- **AND** otherwise uses `slack.default_channel_id` from the global config
- **AND** if neither is set, the AskUser handling fails with an error naming the missing config key

#### Scenario: Polling iteration does not block on a stuck waiting change
- **WHEN** a waiting change has not received a human reply
- **THEN** the iteration's processing of that change completes within one Slack polling round-trip (no internal sleep or retry loop)
- **AND** other waiting changes in the same repo continue to be polled in the same iteration

#### Scenario: Same-repo queue blocking when a change is still waiting
- **WHEN** an iteration completes the waiting-processing step AND `queue::list_waiting(workspace)` still returns a non-empty list
- **THEN** the orchestrator SHALL NOT call `queue::list_pending(workspace)` for that repository in this iteration
- **AND** the iteration emits a single log line of the form `"queue blocked for <url>: <N> change(s) still waiting on human reply"` listing the names
- **AND** other repositories' polling tasks are unaffected (cross-repo blocking is not implied)
- **AND** the iteration proceeds to its sleep step normally so a future iteration can re-check Slack

#### Scenario: Queue resumes after waiting set empties
- **WHEN** an iteration completes the waiting-processing step AND every previously-waiting change has either resumed-to-completion (archived) or resumed-to-Failed (returned to pending) AND `queue::list_waiting(workspace)` is now empty
- **THEN** the orchestrator proceeds to the pending-change loop in the same iteration
- **AND** any pending changes that were blocked in earlier iterations are eligible for processing now

#### Scenario: Failed change halts the pending-change loop
- **WHEN** the pending-change loop processes change N AND its
  outcome is `Failed` (executor returned Failed OR the post-
  classification rules transformed Completed-with-no-diff into
  Failed)
- **THEN** the orchestrator records the failure via the existing
  perma-stuck counter mechanism AND immediately halts the
  pending-change loop for this iteration
- **AND** changes N+1, N+2, ... in the pending list are NOT
  attempted in this iteration (they remain in `list_pending` for
  the next iteration)
- **AND** the iteration's PR is opened with whatever was archived
  before N (could be zero archived changes → no PR opened)
- **AND** the perma-stuck mechanism continues to bound repeat
  failures: once N's failure counter reaches
  `executor.perma_stuck_after_failures`, the perma-stuck marker
  is written and N is excluded from `list_pending`, allowing
  subsequent iterations to proceed past it

#### Scenario: Escalated change halts the pending-change loop
- **WHEN** the pending-change loop processes change N AND its
  outcome is `Escalated` (the executor returned AskUser AND
  chatops is configured AND the question was posted successfully)
- **THEN** the orchestrator halts the pending-change loop for
  this iteration
- **AND** changes N+1, N+2, ... are NOT attempted in this
  iteration (per the same dependency rationale as the Failed
  case)
- **AND** the iteration's PR is opened with whatever was archived
  before N (could be zero archived changes → no PR opened)
- **AND** the next iteration will be naturally blocked by the
  existing waiting-change rule (the just-escalated change is now
  enumerated by `list_waiting`)

#### Scenario: Archived outcome continues the pending-change loop
- **WHEN** the pending-change loop processes change N AND its
  outcome is `Archived` OR `ArchivedSelfHeal`
- **THEN** the orchestrator continues to change N+1 (subject to
  the existing per-PR archive cap `max_changes_per_pr`)
- **AND** the walk halts only when the cap is reached OR a
  non-Archive outcome occurs OR the pending list is exhausted

### Requirement: Rewind subcommand
The orchestrator SHALL provide a `rewind` subcommand that recovers from a failed PR or bad implementation by unarchiving specified changes and resetting the relevant agent branch. **The subcommand SHALL accept a `--repo <selector>` argument; the argument is required when the config contains multiple repositories AND optional (defaulting to the only configured repo) when the config contains exactly one.**

#### Scenario: Multi-repo rewind requires --repo
- **WHEN** the loaded config contains 2 or more repositories AND the user invokes `orchestrator rewind <change> --config <path>` without `--repo`
- **THEN** the process exits non-zero within 5 seconds
- **AND** stderr names the missing argument AND lists the configured repositories' short names as candidate selectors

#### Scenario: Single-repo rewind defaults to the only repo
- **WHEN** the loaded config contains exactly one repository AND `--repo` is omitted
- **THEN** the process operates against that repository's workspace without prompting for the selector
- **AND** a log line at start of execution names the repository being rewound

#### Scenario: Selector resolution by URL or short-name
- **WHEN** `--repo <selector>` is provided
- **THEN** the orchestrator attempts to match the selector against each configured repository's full URL (exact string equality) AND against a derived short-name (the URL's basename with any `.git` suffix removed)
- **AND** if exactly one repository matches, the rewind proceeds against that repo
- **AND** if zero repositories match, the process exits non-zero with stderr naming the unmatched selector and listing the available short-names
- **AND** if two or more repositories match (ambiguous selector), the process exits non-zero with stderr naming all matching repository URLs

#### Scenario: Soft rewind requires confirmation
- **WHEN** the user invokes rewind WITHOUT `--hard` (after selector resolution)
- **THEN** the process prints to stderr the line `This will delete branch '<agent_branch>' (local) and unarchive <N> change(s) (<comma-separated names>). Proceed? [y/N]`
- **AND** reads one line from stdin
- **AND** if the trimmed input is not exactly `y` or `Y`, the process logs `rewind cancelled` and exits with status 0 without modifying any branch or archive state

#### Scenario: Hard rewind deletes the agent branch locally and remotely
- **WHEN** the user invokes rewind WITH `--hard`
- **THEN** the process skips the confirmation prompt
- **AND** runs `git branch -D <agent_branch>` against the resolved repository's workspace
- **AND** runs `git push origin --delete <agent_branch>` against the resolved repository's workspace
- **AND** if remote deletion fails because the remote branch does not exist, the failure is logged at debug level and rewind proceeds; other remote-deletion failures (auth, network) are logged at error level but do NOT halt unarchive

#### Scenario: Unarchive of multiple changes
- **WHEN** the user passes two or more change names to rewind
- **THEN** the process attempts unarchive for each in command-line order
- **AND** if any individual unarchive fails (no matching archive entry, destination collision), the process continues with the remaining changes
- **AND** at the end, if any unarchive failed, the process exits non-zero with stderr listing the failed changes and their reasons; otherwise it exits 0 with a summary log line naming all rewound changes

### Requirement: Per-owner GitHub token routing
autocoder SHALL resolve the GitHub PAT used for each PR-creation call by
parsing the repository URL's owner segment and consulting an optional
`owner_tokens` map in the `github:` config block. Map values MAY be
either a bare string (interpreted as an env var name) or
`{ value: "..." }` (interpreted as an inline secret). When no
owner-specific entry matches, autocoder SHALL fall back in priority
order to `github.token` (inline, when set) then to the env var named by
`github.token_env`. When neither route resolves, autocoder SHALL fail
at startup before any polling task is spawned.

#### Scenario: Owner-specific token used when configured (env var name)
- **WHEN** `config.yaml`'s `github.owner_tokens` map contains an entry
  whose key matches the URL owner of a configured repository
  (case-insensitive) AND the value is a bare string
- **THEN** the PR-creation HTTP call for that repository uses the value
  of the environment variable named by `owner_tokens[<matched-key>]`
- **AND** if that environment variable is unset at startup, autocoder
  exits non-zero with stderr naming both the owner and the missing env
  var

#### Scenario: Owner-specific token used when configured (inline)
- **WHEN** `config.yaml`'s `github.owner_tokens` map contains an entry
  whose key matches the URL owner of a configured repository
  (case-insensitive) AND the value is `{ value: "..." }`
- **THEN** the PR-creation HTTP call for that repository uses the
  inline `value` verbatim
- **AND** no environment variable is consulted for that owner

#### Scenario: Fallback to inline global token
- **WHEN** `owner_tokens` does not match the repository's owner AND
  `github.token` is set
- **THEN** the PR-creation HTTP call uses the inline value verbatim
- **AND** `github.token_env` is NOT consulted; if both
  `github.token` and `github.token_env`'s named env var are set,
  autocoder emits exactly one `warn`-level log line at startup noting
  that the inline value takes precedence

#### Scenario: Fallback to env-var global token
- **WHEN** `owner_tokens` does not match the repository's owner AND
  `github.token` is unset
- **THEN** the PR-creation HTTP call uses the value of the environment
  variable named by `github.token_env`
- **AND** if `github.token_env`'s named environment variable is unset,
  autocoder exits non-zero with stderr naming the missing env var AND
  the repository whose owner has no `owner_tokens` route

#### Scenario: Startup logs name the source per repository
- **WHEN** autocoder starts and successfully resolves a token route for
  every configured repository
- **THEN** for each repository, autocoder emits an info-level log line
  of the form `repository <url> will use GitHub token from <source>`
- **AND** `<source>` is `env var <name>` for env-var resolution, or
  `inline (<field-path>)` for inline resolution, with `<field-path>`
  being one of `github.token`, `github.owner_tokens[<owner>]`, or the
  env-var name path
- **AND** the log line NEVER contains the secret value itself

#### Scenario: Case-insensitive owner matching
- **WHEN** `owner_tokens` contains a key like `My-Org` AND a repository
  URL has owner `my-org`
- **THEN** the entry matches and its resolved secret (env-var or
  inline) is used
- **AND** the same applies in reverse (config key `my-org`, URL owner
  `My-Org`)

#### Scenario: Backward compatibility — config with only `token_env`
- **WHEN** `config.yaml` has a `github:` block with `token_env` set AND
  no `owner_tokens` key AND no `token` key
- **THEN** every repository uses the env var named by `token_env`, with
  no behavior change from the prior single-token implementation

### Requirement: Per-repository asynchronous polling loop
autocoder SHALL implement the per-repository polling task referenced in `orchestrator-architecture/specs/orchestrator-cli/spec.md` as a sleep-then-iterate cycle that runs the architecture's single-pass workflow on every iteration. Each polling task SHALL apply a startup jitter (a random sleep in `[0, startup_jitter_max_secs]`) before its first iteration, and an inter-iteration jitter (a random uniform offset in `[-jitter_pct%, +jitter_pct%]` of `poll_interval_sec`) on every subsequent sleep. Both jitter sleeps SHALL respect the task's cancellation token.

#### Scenario: Spawn count matches config
- **WHEN** the daemon starts with a config containing N repositories AND the workspace collision check passes
- **THEN** exactly N polling tasks are spawned via `tokio::task::JoinSet`
- **AND** each task owns its own workspace path (no two tasks share a path; collision detection at startup enforces non-overlap)

#### Scenario: Startup jitter staggers first iterations
- **WHEN** the daemon spawns N polling tasks with default
  `startup_jitter_max_secs = 30`
- **THEN** each task draws a random sleep duration uniformly from
  `[0, 30]` seconds and waits that long BEFORE its first iteration
- **AND** different tasks draw independently, so the first iterations
  of the N tasks are spread across the 30-second window rather than
  beginning simultaneously

#### Scenario: Startup jitter of zero disables staggering
- **WHEN** `executor.startup_jitter_max_secs == 0`
- **THEN** every task begins its first iteration immediately on spawn
  (matching the pre-change behavior); no startup sleep occurs

#### Scenario: Normal iteration
- **WHEN** a polling task wakes (start of process or end of previous sleep)
- **THEN** it runs the full single-pass workflow for its repository: workspace init → stale-lock cleanup → dirty-workspace refusal → branch recreation → queue walk → push and PR creation if any commits were produced
- **AND** the task then sleeps for a jittered duration of
  `poll_interval_sec ± (poll_interval_sec * jitter_pct / 100)`
  before iterating again
- **AND** no two iterations within the same task overlap

#### Scenario: Inter-iteration jitter offset is uniformly distributed
- **WHEN** `executor.inter_iteration_jitter_pct = 10` AND
  `repo.poll_interval_sec = 300`
- **THEN** each inter-iteration sleep duration is drawn uniformly from
  `[270, 330]` seconds (300 ± 30, i.e. ±10%)
- **AND** the draw is independent per iteration; back-to-back
  iterations do not share a fixed offset

#### Scenario: Inter-iteration jitter of zero gives exact interval
- **WHEN** `executor.inter_iteration_jitter_pct == 0`
- **THEN** every inter-iteration sleep is exactly `poll_interval_sec`
  seconds (matching the pre-change behavior); the offset is not drawn

#### Scenario: Iteration runtime exceeds poll interval
- **WHEN** an iteration's wall-clock runtime exceeds the (possibly-jittered) `poll_interval_sec`
- **THEN** the next iteration begins immediately after the current one finishes
- **AND** no negative sleep is attempted; no two iterations within the same task run in parallel

#### Scenario: Cancellation interrupts startup jitter
- **WHEN** SIGINT or SIGTERM arrives while a task is in its startup
  jitter sleep (i.e. before its first iteration)
- **THEN** the task observes the cancellation token within 200 ms,
  exits the jitter sleep, and does NOT begin its first iteration
- **AND** the main process exits within 30 seconds total

#### Scenario: Cancellation interrupts jittered inter-iteration sleep
- **WHEN** SIGINT or SIGTERM arrives while a task is in its
  inter-iteration sleep
- **THEN** the task exits the sleep within 200 ms and does not begin
  another iteration
- **AND** this holds whether or not the sleep was the jittered or
  non-jittered branch (a `jitter_pct == 0` configuration produces the
  same cancellation latency)

### Requirement: Iteration-level error tolerance
The polling loop SHALL continue running after a failed iteration; a single iteration's error MUST NOT terminate the task or affect other repositories. Predictable failure categories (workspace init, mid-iteration dirty workspace, branch push, PR creation) SHALL emit a throttled chatops alert via the existing `AlertCategory` + `handle_predictable_failure` mechanism before the iteration returns `Err`. For the mid-iteration dirty-workspace category, the alert SHALL fire only AFTER an auto-recovery attempt has been made and failed to clean the workspace (see "Dirty workspace auto-recovers mid-iteration").

#### Scenario: Iteration fails
- **WHEN** any error occurs during a polling iteration (workspace init, git operation, executor failure, PR creation)
- **THEN** the task emits a log line of the form `"polling iteration failed for <url>: <error chain>"` naming the failed step
- **AND** the task sleeps for `poll_interval_sec` and proceeds to the next iteration
- **AND** other repositories' polling tasks are unaffected (their iterations continue on schedule)

#### Scenario: Mid-iteration dirty workspace alerts via chatops
- **WHEN** `run_pass_through_commits` finds `git status --porcelain`
  non-empty at the start of a pass (after filtering autocoder
  bookkeeping files like `.alert-state.json`) AND auto-recovery
  (see "Dirty workspace auto-recovers mid-iteration") has been
  attempted AND a subsequent dirty check is STILL non-empty
  AND chatops is configured AND `failure_alerts_enabled` is true
- **THEN** autocoder posts a throttled chatops notification under
  `AlertCategory::WorkspaceDirtyMidIteration` naming the repository
  URL and a short excerpt of the porcelain output
- **AND** the iteration returns the existing `Err` ("workspace ... is
  dirty before pass; refusing to proceed: ...")
- **AND** subsequent iterations that produce the same dirty state
  within 24 hours do NOT re-post (the per-category 24h throttle
  suppresses duplicates, matching the existing
  `WorkspaceInitFailure`/`BranchPushFailure`/`PrCreationFailure`
  behavior)

#### Scenario: Mid-iteration dirty workspace without chatops still logs
- **WHEN** the dirty-workspace condition above occurs AND chatops is
  not configured (or `failure_alerts_enabled` is false)
- **THEN** no chatops post is attempted
- **AND** the existing ERROR log line is the operator's sole signal
- **AND** the iteration still returns `Err` and the polling loop
  proceeds to the next sleep

#### Scenario: Dirty-workspace alert clears after recovery
- **WHEN** a subsequent iteration succeeds (workspace no longer
  dirty AND the pass produces commits AND push+PR steps both
  succeed)
- **THEN** the existing on-success `AlertState::clear` call clears
  the `WorkspaceDirtyMidIteration` throttle alongside every other
  category
- **AND** if the workspace becomes dirty again later, the next
  occurrence re-alerts immediately (no leftover suppression)

### Requirement: Graceful shutdown on signal
The orchestrator SHALL respond to SIGINT or SIGTERM by cancelling all polling tasks; each task completes its current iteration (if any) and exits cleanly.

#### Scenario: Signal during inter-iteration sleep
- **WHEN** SIGINT or SIGTERM arrives while every polling task is sleeping
- **THEN** every task exits its sleep within 200 ms (verified in tests via the `CancellationToken` selecting against the sleep) and does not begin another iteration
- **AND** the main process exits within 30 seconds total

#### Scenario: Signal during iteration
- **WHEN** SIGINT or SIGTERM arrives while a polling iteration is in progress
- **THEN** the in-flight iteration runs to completion (mid-iteration cancellation is NOT performed); the task then observes the cancellation token and exits without sleeping or starting another iteration
- **AND** any child processes spawned by the iteration receive their normal lifecycle (the executor's child process completes or hits its own `executor.timeout_secs`)

### Requirement: Startup logging per repository
The orchestrator SHALL emit a startup log line per configured repository naming its URL, derived (or explicit) workspace path, and configured `poll_interval_sec`.

#### Scenario: Startup line emitted
- **WHEN** the daemon starts AND the workspace collision check passes
- **THEN** before any polling task begins iterating, the orchestrator emits one log line per repository containing the literal URL, the resolved workspace path, and the integer `poll_interval_sec`

### Requirement: `github.fork_owner` opt-in to fork-PR mode
autocoder SHALL accept an optional `github.fork_owner: String` field in
`config.yaml`. When present, autocoder operates in **fork-PR mode** for
all configured repositories: the agent branch is pushed to a fork
owned by `fork_owner`, and PRs are opened as cross-repository PRs from
the fork to the upstream. When absent, autocoder operates in
**direct-push mode** with no behavior change from the prior
implementation.

#### Scenario: `fork_owner` absent — direct-push mode
- **WHEN** `config.yaml` has no `github.fork_owner` key
- **THEN** every configured repository operates in direct-push mode:
  the agent branch is pushed to `origin` and PRs use the agent-branch
  name as the `head` parameter
- **AND** no `fork` remote is registered in any workspace

#### Scenario: `fork_owner` present — fork-PR mode active
- **WHEN** `config.yaml` has `github.fork_owner: <handle>` set
- **THEN** every configured repository operates in fork-PR mode: the
  agent branch is pushed to the `fork` remote (pointing at
  `git@github.com:<handle>/<repo>.git` or the HTTPS equivalent), and
  PRs are opened with `head: "<handle>:<agent-branch>"` against the
  upstream repository

#### Scenario: `fork_owner` is global, not per-repository
- **WHEN** `config.yaml` has `github.fork_owner: <handle>` set
- **THEN** the same `<handle>` is used as the fork owner for every
  configured repository
- **AND** per-repository fork-owner overrides are NOT supported

### Requirement: Startup verification of fork existence
When `github.fork_owner` is set, autocoder SHALL ensure each configured
repository has a reachable fork at the derived URL before spawning any
polling task. Forks that are missing or unreachable SHALL be created
automatically via `POST /repos/{upstream-owner}/{upstream-repo}/forks`
using the PAT resolved for the upstream owner; the daemon then polls
the fork URL via `git ls-remote` until it becomes reachable or until a
60-second timeout elapses. If creation fails (non-2xx) OR polling
times out, autocoder SHALL aggregate the failures into a single
startup error and exit non-zero before any polling task is spawned.

#### Scenario: All forks already exist
- **WHEN** autocoder starts with `github.fork_owner` set AND every
  configured repository's derived fork URL resolves via
  `git ls-remote <fork-url> HEAD` on the first probe
- **THEN** no fork-creation API calls are issued
- **AND** all polling tasks are spawned and the daemon enters its
  normal polling state

#### Scenario: A fork is missing and creation succeeds
- **WHEN** autocoder starts with `github.fork_owner` set AND at least
  one configured repository's derived fork URL fails the initial
  `git ls-remote` probe
- **THEN** autocoder issues `POST /repos/<upstream-owner>/<upstream-repo>/forks`
  with header `Authorization: Bearer <token>` (token resolved by the
  existing per-owner routing) for each missing fork
- **AND** on 2xx response from the POST, autocoder polls the fork URL
  via `git ls-remote` every 2 seconds for up to 60 seconds
- **AND** when polling succeeds, the daemon proceeds to spawn polling
  tasks normally
- **AND** the daemon emits one info-level log line per created fork
  of the form `created fork <fork-url> from upstream <upstream-url>`

#### Scenario: Fork-creation POST fails
- **WHEN** autocoder attempts to create a missing fork AND the
  `POST /repos/{upstream-owner}/{upstream-repo}/forks` call returns a
  non-2xx status code
- **THEN** that repository's failure is recorded with the upstream
  URL, the expected fork URL, and the HTTP status (plus a body snippet
  truncated to 200 chars)
- **AND** autocoder continues attempting the remaining repositories'
  forks before aggregating failures
- **AND** after all repositories are processed, if any failed,
  autocoder exits non-zero with a single error listing every failed
  repo

#### Scenario: Fork-creation succeeds but the fork is not yet reachable
- **WHEN** the POST returns 2xx AND `git ls-remote <fork-url> HEAD`
  fails for 60 seconds of polling at 2-second intervals
- **THEN** that repository's failure is recorded as
  "fork creation succeeded but the fork at `<fork-url>` was not
  reachable within 60s"
- **AND** the failure is included in the aggregated startup error
  (the daemon does NOT proceed with this repo missing)

#### Scenario: A fork already exists when creation is attempted
- **WHEN** autocoder issues the fork-creation POST AND the upstream
  has already been forked to the destination user
- **THEN** the GitHub API returns 2xx with the existing fork's
  metadata (idempotent behavior)
- **AND** autocoder treats this as success and proceeds with the
  reachability probe normally

### Requirement: Rewind --hard targets fork remote in fork-PR mode
autocoder SHALL delete the agent branch from the `fork` remote (not
`origin`) when `rewind` is invoked with `--hard` AND
`github.fork_owner` is set. The local-branch deletion semantics are
unchanged.

#### Scenario: Hard rewind in fork-PR mode
- **WHEN** the operator runs `autocoder rewind <change> --hard` AND
  `github.fork_owner` is set
- **THEN** the manager runs
  `git push fork --delete <agent_branch>` instead of
  `git push origin --delete <agent_branch>`
- **AND** the local branch is deleted via `git branch -D <agent_branch>`
  as in direct-push mode
- **AND** failures of the remote delete are logged but non-blocking,
  as in direct-push mode

#### Scenario: Soft rewind in fork-PR mode
- **WHEN** the operator runs `autocoder rewind <change>` (no `--hard`)
  AND `github.fork_owner` is set
- **THEN** the manager deletes only the local branch; neither remote
  is touched
- **AND** the resulting fork's agent branch is left intact for the
  next polling pass to force-push over

### Requirement: SecretSource accepts inline values
autocoder SHALL define a `SecretSource` enum with two variants:
`EnvVar(String)` (deserialized from a bare YAML string, interpreted as
an env var name) and `Inline { value: String }` (deserialized from a
YAML object of shape `{ value: "..." }`, interpreted as the secret
value verbatim). The enum SHALL expose a `resolve(field_label)` method
that returns the secret value or an error naming the originating field.

#### Scenario: Bare string parses as EnvVar
- **WHEN** a YAML field declared as `SecretSource` contains a bare
  string (`my_field: GITHUB_TOKEN`)
- **THEN** serde deserializes it as `SecretSource::EnvVar("GITHUB_TOKEN".into())`
- **AND** `resolve` reads the env var of that name; on miss, returns an
  error whose text contains the env var name AND the field label

#### Scenario: Object parses as Inline
- **WHEN** a YAML field declared as `SecretSource` contains
  `my_field: { value: "abc123" }`
- **THEN** serde deserializes it as `SecretSource::Inline { value: "abc123".into() }`
- **AND** `resolve` returns `"abc123"` directly without consulting the
  environment

#### Scenario: Invalid shape produces an intelligible error
- **WHEN** a YAML field declared as `SecretSource` contains a list, a
  number, or an object without a `value` key
- **THEN** `Config::load_from` returns an error mentioning the field
  whose value could not be parsed

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
autocoder SHALL emit a ChatOps notification at most once every 24 hours per (repository, failure category) combination for three categories of predictable infrastructure failure: `workspace_init_failure`, `branch_push_failure`, `pr_creation_failure`. Throttle state SHALL be persisted at `<state_dir>/alert-state/<workspace-basename>.json` (resolved via the daemon's `DaemonPaths.alert_state_path()` helper) AND cleared on the next successful iteration of the same repository. The state file lives outside the managed repository's workspace — daemon bookkeeping never appears in the managed repo's working tree, nor in `git status`, nor in any `git checkout` operation's clobber-protection logic.

#### Scenario: First failure in a category alerts immediately
- **WHEN** any of the three categorized failures occurs in a repository whose `<state_dir>/alert-state/<basename>.json` has no entry for that category AND `slack.notifications.failure_alerts` is unset OR `true`
- **THEN** autocoder calls `chatops.post_notification(channel, text)` with category-specific text containing the repo URL, a category label, and a truncated error excerpt (max 200 chars)
- **AND** on successful post, autocoder writes the category's `last_alerted_at` (current UTC) and `last_error_excerpt` to `<state_dir>/alert-state/<basename>.json` atomically (tempfile-then-rename)

#### Scenario: Repeat failure within 24h is silent
- **WHEN** a categorized failure occurs in a repository whose `<state_dir>/alert-state/<basename>.json` has an entry for that category with `last_alerted_at` within the past 24 hours
- **THEN** no notification is posted for that iteration
- **AND** the state file is NOT modified

#### Scenario: Repeat failure beyond 24h re-alerts
- **WHEN** a categorized failure occurs AND `now - last_alerted_at >= 24h`
- **THEN** a new notification is posted with the most recent error excerpt
- **AND** `last_alerted_at` is updated to the current UTC time

#### Scenario: Success clears alert state
- **WHEN** an iteration of a repository completes its `run_pass_through_commits` workflow without returning Err (regardless of whether any changes were processed or whether the queue was empty)
- **THEN** autocoder removes `<state_dir>/alert-state/<basename>.json` from disk (or writes an empty `{ "alerts": {} }` map, equivalent semantics)
- **AND** the next failure of any category re-alerts immediately

#### Scenario: Alert post failure does NOT update state
- **WHEN** a categorized failure occurs AND the 24h window is open AND `post_notification` itself returns Err
- **THEN** the failure is logged to stderr including the alert text that would have been posted
- **AND** the state file is NOT updated (so the next iteration re-attempts the alert immediately)

#### Scenario: Failure-alerts disabled
- **WHEN** `slack.notifications.failure_alerts` is `false`
- **THEN** no failure alerts are posted regardless of category or history
- **AND** the state file is NEITHER read NOR written
- **AND** the failure still produces the existing stderr log line

#### Scenario: Out-of-scope failures are not alerted
- **WHEN** an executor returns `Failed` OR the reviewer LLM call fails OR `post_notification` itself fails
- **THEN** no failure alert is posted (these categories are out of scope for this change)

#### Scenario: State file never appears in the managed workspace
- **WHEN** the daemon writes alert-state for any repository
- **THEN** no file named `.alert-state.json` exists at any path inside the repository's workspace directory
- **AND** `git status` in the workspace shows no daemon-bookkeeping file (the workspace contains only the repo's tracked content, the daemon's own `.git/info/exclude`-listed in-workspace bookkeeping per other specs, AND any operator-edited uncommitted work)
- **AND** the daemon's writes never interfere with the workspace's git checkout / dirty-check / pull operations

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

### Requirement: Startup preflight for openspec availability
autocoder SHALL verify that the `openspec` binary is available before the polling loop starts. A failed preflight aborts daemon startup with a non-zero exit code, ensuring a misconfigured deployment fails loudly instead of looping forever producing nothing.

#### Scenario: openspec is available
- **WHEN** the daemon starts and `Command::new("openspec").arg("--version")` exits 0
- **THEN** the preflight passes and the polling loop starts normally

#### Scenario: openspec binary not on PATH
- **WHEN** the daemon starts and spawning `openspec --version`
  returns a `NotFound` I/O error
- **THEN** the daemon exits non-zero before the polling loop starts
- **AND** stderr names the failure: `openspec preflight failed:
  binary not found on PATH. Install openspec and ensure the
  systemd unit's PATH covers its install directory.`

#### Scenario: openspec spawns but exits non-zero
- **WHEN** the daemon starts, `openspec --version` spawns
  successfully, but exits non-zero
- **THEN** the daemon exits non-zero before the polling loop starts
- **AND** stderr names the exit code and includes a tail of
  `openspec --version`'s stderr output (up to 200 chars)

### Requirement: Iteration lifecycle logging
autocoder SHALL emit INFO-level log lines marking the start and end of each polling pass and each per-change iteration. The lines are intended for operator visibility in journalctl at the default log level (`RUST_LOG=info`), so an iteration that takes minutes is not silent.

#### Scenario: Polling pass start
- **WHEN** `run_pass_through_commits` begins (after workspace
  initialization and dirty-check have passed)
- **THEN** autocoder emits one INFO log line with the message
  `polling pass starting` and structured fields including `url`,
  `pending` (count of pending changes), and `waiting` (count of
  waiting changes)

#### Scenario: Polling pass end
- **WHEN** `run_pass_through_commits` returns Ok, regardless of
  whether any changes were processed
- **THEN** autocoder emits one INFO log line with the message
  `polling pass complete` and structured fields including `url`,
  `committed` (count of changes that produced commits this pass),
  and `waiting` (count of changes still in waiting state)
- **AND** the previous "polling pass produced no changes" line is
  removed (subsumed by the new uniform message)

#### Scenario: Per-change iteration start
- **WHEN** autocoder is about to invoke the executor on a pending
  change (or resume a waiting change with a human reply)
- **THEN** autocoder emits one INFO log line with the message
  `starting work on change` and structured fields including `url`
  and `change`

#### Scenario: Per-change iteration end
- **WHEN** `handle_outcome` (or the equivalent resume-path handler)
  returns for a change
- **THEN** autocoder emits one INFO log line with the message
  `change finished` and structured fields including `url`,
  `change`, and `outcome` (one of `archived`, `failed`,
  `escalated`, `ask_user_exit_early`)

### Requirement: Per-repo busy marker prevents concurrent work
autocoder SHALL acquire a per-repo busy marker file at the start of each polling iteration and hold it through every stage of the pass (executor invocation, commit, review, push, PR creation). The marker lives at `<runtime_dir>/busy/<workspace-basename>.json` (resolved per the daemon's path resolver) and is created atomically via POSIX `O_EXCL`. Its presence prevents any other autocoder pass — same daemon or different — from concurrently working on the same repo. Crashes that bypass normal release (SIGKILL, segfault, host power loss, daemon restart mid-iteration) leave the marker behind for the next pass to detect and recover from. Stuck-state recovery SHALL prefer the subprocess-sidecar PGID (set by the executor after spawning Claude) over the marker's own `pgid` field when sending kill signals.

The stale-threshold SHALL be a dedicated `executor.busy_marker_stale_threshold_secs` config field (default `600` seconds, max `7200` with WARN-and-clamp), NOT a derived value from `executor.timeout_secs`. Raising the executor timeout for legitimately long work SHALL NOT proportionally delay stale-marker recovery on unrelated iterations.

Dead-pid recovery (the `Stuck threshold exceeded, PID dead` scenario below) SHALL fire IMMEDIATELY when the marker's recorded `pid` no longer exists in `/proc`, without waiting for the stale-threshold to elapse. A pid that no longer exists cannot be doing legitimate work; the marker is unambiguously stale the moment that's true.

The "busy marker present; skipping iteration" INFO log line SHALL include the marker's age, the resolved `busy_marker_stale_threshold_secs`, the PID-alive state, AND a `recovery_eligible` boolean computed as `!pid_alive || age >= threshold`. Operators reading `journalctl` can see the marker's recovery state inline without reading the marker file separately.

At daemon startup, after resolving both `executor.timeout_secs` AND `executor.busy_marker_stale_threshold_secs`, the daemon SHALL log one INFO line naming both resolved values. If the new threshold field was NOT explicitly set in config AND the pre-spec implicit formula (`timeout_secs + 600`) would have produced a longer threshold, an additional INFO line SHALL name the gap so operators migrating from the pre-spec behavior see the change.

#### Scenario: Acquire on a clean repo
- **WHEN** a polling iteration begins AND no marker file exists at the resolved `<runtime_dir>/busy/<workspace-basename>.json`
- **THEN** the daemon creates the marker via `OpenOptions::new().write(true).create_new(true).open(path)` (atomic against concurrent daemons)
- **AND** the marker contains a JSON document with fields `repo_url`, `pid` (this process's PID), `pgid` (this process's process group ID), `comm` (the value of `/proc/<pid>/comm` at acquire time, on Linux; empty string on other platforms), `started_at` (RFC 3339 UTC timestamp), AND `stage` (initially `"executor"`)
- **AND** the iteration proceeds normally

#### Scenario: Atomic stage transitions
- **WHEN** the iteration moves from one stage to the next (`executor → commit → review → push → pr`)
- **THEN** the daemon updates the marker's `stage` field via a write-to-temp-then-rename sequence so concurrent readers see either the prior stage or the new one, never a partial write
- **AND** stage names are exactly: `executor`, `commit`, `review`, `push`, `pr`

#### Scenario: Release on normal iteration end
- **WHEN** `execute_one_pass` returns (success or any error)
- **THEN** the RAII guard holding the marker drops, and the file is removed
- **AND** the next iteration finds no marker and proceeds normally

#### Scenario: Marker exists, age below stuck threshold
- **WHEN** acquire detects an existing marker AND its `started_at` is less than `executor.busy_marker_stale_threshold_secs` old AND the recorded `pid` is alive in `/proc`
- **THEN** the daemon logs INFO with the enhanced log line including `age`, `threshold`, `pid_alive=true`, `recovery_eligible=false` AND skips this iteration without modifying the marker
- **AND** the polling task continues with its normal sleep + next-iteration cycle

#### Scenario: Stuck threshold exceeded, PID dead
- **WHEN** acquire detects a marker whose recorded `pid` does NOT correspond to a running process (verified via `/proc/<pid>` stat returning `ENOENT`)
- **THEN** the daemon deletes the marker AND the subprocess sidecar file (if present), logs WARN naming the marker's prior contents (so operators see what crashed), AND proceeds to acquire a fresh marker and run the iteration
- **AND** the recovery fires IMMEDIATELY regardless of the marker's age — no age-threshold check applies to this branch
- **AND** this differs from pre-spec behavior, which gated recovery on `age > executor.timeout_secs + 600`, causing repos to remain stuck for up to 100 minutes after daemon restart

#### Scenario: Stuck threshold exceeded, PID alive, comm matches
- **WHEN** acquire detects a marker older than `executor.busy_marker_stale_threshold_secs` AND the recorded `pid` is alive in `/proc` AND the value of `/proc/<pid>/comm` matches the recorded `comm` field (Linux; the comm-check is skipped on non-Linux platforms and the PID liveness check is trusted alone)
- **THEN** the daemon reads the subprocess sidecar file at `<runtime_dir>/busy/<workspace-basename>.subprocess` (if present). If present, the recorded subprocess PID is used as the kill target (its PGID equals its PID because the executor spawns with `process_group(0)`); if absent, the marker's `pgid` field is used as the fallback
- **AND** the daemon sends `SIGTERM` to that process group via `killpg(target_pgid, SIGTERM)`, waits up to 5 seconds for the group to exit, sends `SIGKILL` via `killpg(target_pgid, SIGKILL)` if still alive
- **AND** the daemon deletes the marker AND the subprocess sidecar file, logs WARN with the action taken, attempts to post a chatops alert "repo recovered from stuck state" (best-effort), AND proceeds to acquire a fresh marker and run
- **AND** the iteration proceeds even when no chatops backend is configured

#### Scenario: Stuck threshold exceeded, PID alive, comm differs
- **WHEN** acquire detects a marker older than `executor.busy_marker_stale_threshold_secs` AND the recorded `pid` is alive in `/proc` AND the recorded `comm` field is non-empty AND differs from the live `/proc/<pid>/comm` value
- **THEN** the daemon logs ERROR naming the discrepancy, attempts to post a chatops alert "repo stuck — please investigate" (best-effort), AND SKIPS this iteration without modifying the marker or the subprocess sidecar
- **AND** the marker stays in place for human investigation; the next polling iteration will re-evaluate
- **AND** the iteration is skipped even when no chatops backend is configured (the ERROR log is the operator's only signal in that case)

#### Scenario: Malformed marker JSON
- **WHEN** acquire detects a marker file that cannot be parsed as the expected JSON shape
- **THEN** the daemon logs WARN naming the parse failure, deletes the marker AND the subprocess sidecar (if present), AND proceeds to acquire a fresh one

#### Scenario: Threshold change is independent of `executor.timeout_secs`
- **WHEN** an operator sets `executor.timeout_secs: 5400` AND does NOT explicitly set `executor.busy_marker_stale_threshold_secs`
- **THEN** the resolved threshold is `600` (the default), NOT `6000` (the pre-spec coupled formula)
- **AND** a startup INFO log notes the gap so operators migrating from pre-spec behavior see the change
- **AND** dead-pid markers continue to recover immediately regardless of either value

#### Scenario: Out-of-bounds threshold values are clamped
- **WHEN** an operator sets `executor.busy_marker_stale_threshold_secs: 10000`
- **THEN** the resolved value is `7200` (the max)
- **AND** a WARN log at startup names both the requested and clamped values

#### Scenario: PID-alive check uses `/proc/<pid>` stat
- **WHEN** the classification logic checks whether a pid is alive
- **THEN** the implementation stats `/proc/<pid>` (not signal-0 or other approaches)
- **AND** returns `false` on `ENOENT` (pid does not exist)
- **AND** returns `true` on successful stat
- **AND** on any other error (permission, transient) the implementation treats the pid as "unknown alive" — falling through to the age-based branches rather than incorrectly clearing a possibly-live marker

#### Scenario: Enhanced log line includes age, threshold, pid_alive, recovery_eligible
- **WHEN** any iteration's busy-marker classification produces a "busy marker present; skipping" log line
- **THEN** the line contains `age=<duration>`, `threshold=<duration>`, `pid_alive=<bool>`, AND `recovery_eligible=<bool>` fields
- **AND** the operator can determine from a single log line whether the marker is stale, when recovery will fire, AND whether the pid is alive — without reading the marker file separately

### Requirement: Dirty workspace auto-recovers at startup
autocoder SHALL attempt automatic recovery before falling back to the existing "skip for the process lifetime" behavior when a repository's workspace is dirty at startup (non-empty `git status --porcelain` output). Recovery consists of `git checkout <base_branch>`, `git reset --hard origin/<base_branch>`, and `git clean -fd`. After recovery, autocoder SHALL re-run the dirty check; if clean, the repository proceeds to normal polling.

#### Scenario: Workspace dirty due to prior failed iteration
- **WHEN** a repository's workspace has uncommitted changes at
  startup (residue from a previous executor run that crashed or
  was killed mid-iteration)
- **THEN** autocoder logs a `warn`-level line naming the dirty
  entry count and indicating recovery is being attempted
- **AND** autocoder runs `git checkout <base_branch>`, then
  `git reset --hard origin/<base_branch>`, then `git clean -fd`
  in the workspace
- **AND** autocoder re-runs `git status --porcelain`; if empty,
  logs `info` "workspace recovered" and the repository proceeds
  to normal polling

#### Scenario: Workspace remains dirty after recovery attempt
- **WHEN** the recovery commands all complete but a subsequent
  `git status --porcelain` is still non-empty (gitignored state,
  read-only mount, file-locking, etc.)
- **THEN** autocoder logs the existing skip-for-lifetime error
  message
- **AND** the repository is skipped for the process lifetime,
  preserving the prior conservative behavior for genuinely
  unrecoverable cases

#### Scenario: Workspace already clean
- **WHEN** the initial `git status --porcelain` is empty
- **THEN** no recovery commands are executed
- **AND** the repository proceeds to normal polling, identical
  to prior behavior

### Requirement: Reject archive-only iterations as Failed
autocoder SHALL treat an iteration as Failed (not Completed), revert the staged moves via `git reset --hard`, and leave the change pending for retry when the executor returns Completed AND the resulting working-tree changes consist *only* of file moves whose destination paths start with `openspec/changes/archive/`. The detection is structural — pattern-matching on rename destinations — and does not depend on which command produced the moves. autocoder SHALL treat Completed-with-clean-workspace as Failed by default — UNLESS the change's implementation is already on the base branch, in which case autocoder SHALL self-archive the change rather than fail (see "Self-heal: already-implemented change" scenario).

#### Scenario: Agent archives the change instead of implementing it
- **WHEN** the executor returns `Completed` for a change AND
  `git status --porcelain` reports a non-empty result AND every
  reported entry is a rename (status code `R`) whose target path
  begins with `openspec/changes/archive/`
- **THEN** autocoder reverts the working tree via
  `git reset --hard HEAD` to discard the staged moves
- **AND** autocoder treats the outcome as
  `Failed { reason: "agent appears to have archived without implementing the change" }`
- **AND** autocoder logs a `warn`-level line naming the change
- **AND** the change's `.in-progress` lock is removed via the
  existing Failed-handling code path so the next iteration
  retries

#### Scenario: Legitimate implementation that also moves an archive file
- **WHEN** the executor returns `Completed` AND the working tree
  contains at least one change that is NOT a rename into
  `openspec/changes/archive/` (e.g. modified `src/foo.rs`, added
  `tests/bar.rs`)
- **THEN** autocoder treats the outcome as Completed as before
- **AND** the commit + push + PR steps proceed normally
- **AND** archive-rename entries, if any, are included in the
  commit unchanged

#### Scenario: Workspace is clean (no changes at all)
- **WHEN** the executor returns `Completed` AND `git status
  --porcelain` is empty AND the self-heal criteria below are NOT
  all satisfied
- **THEN** autocoder treats the outcome as
  `Failed { reason: "agent reported Completed without modifying the workspace" }`
- **AND** autocoder logs a `warn`-level line naming the change
- **AND** autocoder does NOT commit, does NOT archive, and does
  NOT push
- **AND** the change's `.in-progress` lock is removed via the
  existing Failed-handling code path so the next iteration
  retries
- **AND** the lazy-archive detection does NOT fire (no staged
  moves to revert)

#### Scenario: Self-heal — already-implemented change
- **WHEN** the executor returns `Completed` AND `git status
  --porcelain` is empty AND `openspec validate <change> --strict`
  exits 0 AND every line in
  `openspec/changes/<change>/tasks.md` that matches the regex
  `^\s*-\s*\[([ x])\]` has `[x]` (and at least one such line
  exists)
- **THEN** autocoder treats the outcome as a self-heal Archive:
  it runs the archive move (renaming
  `openspec/changes/<change>/` to
  `openspec/changes/archive/<YYYY-MM-DD>-<change>/`) on the
  agent branch, commits the move with subject
  `archive: <change>: implementation already in base`, and
  proceeds through the normal push + PR flow
- **AND** the PR body for a self-heal pass includes the
  paragraph `_This PR archives a change whose implementation was
  already present on the base branch. No code diff is included;
  only the openspec archive move._` ahead of any other body
  content
- **AND** autocoder logs an INFO line naming the change and the
  self-heal classification, distinct from the Failed-path log

#### Scenario: Self-heal preconditions unmet
- **WHEN** the executor returns `Completed` AND `git status
  --porcelain` is empty AND any of the self-heal preconditions
  fails: `openspec validate --strict` errors or exits non-zero,
  OR any task in `tasks.md` is still `[ ]`, OR `tasks.md` cannot
  be read
- **THEN** autocoder falls through to the Failed path (as in
  "Workspace is clean (no changes at all)" above), preserving
  the prior behavior for non-self-heal cases

### Requirement: Skip iteration when an open PR exists for the agent branch
autocoder SHALL query GitHub for open PRs whose `head` matches the configured agent branch before running the executor on any pending changes. When such a PR exists, the iteration SHALL be skipped entirely — no executor invocation, no `recreate_branch` (which would obliterate the open PR's branch on the next force-push), no commit work. The skip persists across iterations until the open PR is closed or merged. This prevents redundant Claude executions, PR-diff thrashing under reviewers, and the 422 "PR already exists" loop that would otherwise occur every polling pass after a PR is opened but not resolved.

#### Scenario: An open PR exists for the agent branch
- **WHEN** the daemon completes workspace init and `pull --ff-only`
  succeeds AND a `GET /repos/{owner}/{repo}/pulls?state=open&head=<head>&base=<base>` query returns one or more PRs
- **THEN** the daemon logs an INFO line naming each PR number and
  the URL, and returns from the iteration without invoking
  `recreate_branch`, `walk_queue`, or any executor
- **AND** the polling task continues with its normal sleep + next-iteration cycle

#### Scenario: No open PR exists for the agent branch
- **WHEN** the GitHub query returns an empty list
- **THEN** the daemon proceeds with `recreate_branch` and the
  normal iteration as before

#### Scenario: GitHub query fails
- **WHEN** the `pulls` query errors at the transport layer or
  returns a non-2xx status
- **THEN** the daemon logs a WARN naming the failure (status code
  and/or error text) and proceeds with the iteration as if no PR
  existed
- **AND** the iteration is NOT blocked by a transient GitHub
  failure (the check is best-effort — false negatives just degrade
  to the prior pre-check behavior)

#### Scenario: Fork-PR mode head qualifier
- **WHEN** `github.fork_owner` is set
- **THEN** the `head` query parameter is
  `<fork_owner>:<agent_branch>` so GitHub disambiguates correctly
  against the upstream repo's PR list

#### Scenario: Direct mode head qualifier
- **WHEN** `github.fork_owner` is unset
- **THEN** the `head` query parameter is
  `<repo_owner>:<agent_branch>` where `<repo_owner>` is parsed
  from `repo.url`

### Requirement: Control socket for runtime daemon interaction
autocoder SHALL listen for control requests on a Unix domain socket at `<system-temp>/autocoder/control/control.sock` during the lifetime of the daemon process. The socket SHALL be created with permissions `0600` and owned by the user running the daemon, restricting access to that user. Control requests use a line-delimited JSON protocol; each connection accepts one request, responds with one JSON object, and closes.

#### Scenario: Socket is created and listening at startup
- **WHEN** the daemon starts
- **THEN** a Unix domain socket is created at
  `<system-temp>/autocoder/control/control.sock` with mode `0600`
- **AND** any pre-existing file at that path is removed before the
  new socket is created (stale socket from a previous run is not a
  startup failure)
- **AND** a tokio task accepts connections on the socket
  concurrently with the polling tasks

#### Scenario: Socket is removed at shutdown
- **WHEN** the daemon receives a shutdown signal AND the
  cancellation token fires
- **THEN** the socket file is removed before the process exits
- **AND** failure to remove the socket file is logged at WARN but
  does NOT block shutdown

#### Scenario: Request protocol
- **WHEN** a client connects to the control socket and sends a line
  of JSON terminated by `\n`
- **THEN** the daemon parses the line as a JSON object with at
  least an `action` field
- **AND** the daemon responds with a single line of JSON terminated
  by `\n` whose shape is `{"ok": true, ...}` on success or
  `{"ok": false, "error": "<message>"}` on failure
- **AND** the daemon closes the connection after sending the
  response

#### Scenario: Unknown action
- **WHEN** the request's `action` field is not one this daemon
  version recognizes
- **THEN** the response is `{"ok": false, "error": "unknown action: <action>"}`

#### Scenario: Malformed request
- **WHEN** the request is not valid JSON OR lacks an `action` field
- **THEN** the response is `{"ok": false, "error": "<parse error description>"}`
- **AND** the connection is closed

### Requirement: `autocoder reload` subcommand
autocoder SHALL provide a `reload` CLI subcommand that connects to the running daemon's control socket, sends `{"action":"reload"}`, prints the response, and exits 0 on success or non-zero on failure. The subcommand SHALL NOT require the daemon's `--config` path as an argument; the daemon already knows its config path and re-reads it from there.

#### Scenario: Successful reload
- **WHEN** the operator runs `autocoder reload`
- **THEN** the CLI connects to
  `<system-temp>/autocoder/control/control.sock`, sends the request,
  reads the response, prints it (pretty-printed JSON) to stdout,
  and exits 0 IF the response's `ok` field is `true`

#### Scenario: Reload rejected
- **WHEN** the daemon's reload handler returns `{"ok": false, ...}`
  (validation failure, IO error reading config, etc.)
- **THEN** the CLI prints the response to stderr and exits with
  a non-zero status

#### Scenario: Daemon not running
- **WHEN** `autocoder reload` is invoked and the control socket
  does not exist OR the connection is refused
- **THEN** the CLI prints an error message naming the expected
  socket path and exits non-zero
- **AND** the message hints at the likely cause: the daemon is
  not running, or is running under a different user

### Requirement: Reload handler hot-applies the safe config subset
The control socket's `reload` handler SHALL re-read the YAML config path the daemon was launched with, validate the new content fully (parse + semantic checks), and hot-apply changes to `github`, `reviewer`, `chatops`, AND `repositories` sections. Changes to the `executor` section SHALL NOT be hot-applied; the handler SHALL report it as `requires-restart` so the operator knows it still needs a full restart. The response SHALL include a `repositories_delta` field naming added / removed / changed repository URLs whenever the repository step modified the task set.

#### Scenario: Reload with no changes
- **WHEN** the YAML file is unchanged since startup AND the reload
  is triggered
- **THEN** the response is
  `{"ok": true, "applied": [], "requires_restart": [], "unchanged": ["github", "reviewer", "chatops", "repositories", "executor"], "repositories_delta": {"added": [], "removed": [], "changed": []}}`
- **AND** no in-memory state is modified

#### Scenario: Reload adds a new repository
- **WHEN** the new YAML contains a `repositories[]` entry whose
  `url` is not present in the current task map
- **THEN** autocoder spawns a new polling task for that URL
  (workspace path derivation, startup dirty-check, busy-marker
  acquire — all as at daemon startup)
- **AND** the new task receives an `Arc<ArcSwap<RepositoryConfig>>`
  seeded with the new entry's values
- **AND** the response's `applied` includes `"repositories"`
- **AND** the response's `repositories_delta.added` includes the
  new URL

#### Scenario: Reload removes a repository
- **WHEN** the new YAML omits a `repositories[]` entry whose `url`
  is currently in the task map
- **THEN** autocoder cancels that task's per-repo cancellation
  token
- **AND** the running task finishes its in-flight iteration
  normally (including push + PR if commits were produced) and
  exits at the next inter-poll sleep boundary
- **AND** the response's `repositories_delta.removed` includes the
  removed URL
- **AND** when the task exits, it removes its own entry from the
  daemon's task map

#### Scenario: Reload changes an existing repository's settings
- **WHEN** the new YAML contains a `repositories[]` entry whose
  `url` matches an existing task AND any other field
  (`base_branch`, `agent_branch`, `poll_interval_sec`,
  `chatops_channel_id`, `local_path`) differs
- **THEN** autocoder swaps the new values into that task's
  `ArcSwap<RepositoryConfig>` holder
- **AND** the next iteration of that task reads the new values
  (the current iteration, if one is in flight, completes with
  the old snapshot)
- **AND** the response's `repositories_delta.changed` includes
  the URL

#### Scenario: Reload changes a repository's URL
- **WHEN** the new YAML differs from the current YAML by replacing
  a repository's `url` value while leaving other fields the same
- **THEN** the diff treats this as `removed(old_url) +
  added(new_url)`: the old task is cancelled, a new task is
  spawned for the new URL
- **AND** the response's `repositories_delta` includes the old
  URL under `removed` and the new URL under `added`

#### Scenario: Reload during a repo's in-flight cancellation
- **WHEN** an earlier reload cancelled a repo's task but the
  task has not yet exited (its in-flight iteration is still
  running) AND a subsequent reload's new YAML re-adds that URL
- **THEN** autocoder logs a WARN naming the transient state
- **AND** the repo is NOT re-spawned on this reload (the URL is
  still in the task map but its token is cancelled)
- **AND** the response reports `"repositories"` as `unchanged`
  for this URL despite the YAML containing it; the next reload
  (after the old task has exited) will properly spawn the new
  task

#### Scenario: Reload with restart-required executor change
- **WHEN** the new YAML differs in `executor`
- **THEN** the executor section is NOT hot-applied
- **AND** the response includes `"executor"` under
  `requires_restart`
- **AND** other hot-applicable sections (including
  `repositories`) ARE applied if they also changed

#### Scenario: Reload rejected by validation
- **WHEN** the new YAML fails to parse (`serde_yaml` error) OR
  fails semantic validation (workspace collision between two
  repos, missing token route, etc.)
- **THEN** the response is `{"ok": false, "error": "<message>"}`
  naming the validation failure
- **AND** no in-memory state is modified, including no spawn / cancel
  of repository tasks
- **AND** the daemon continues running with the previous config

#### Scenario: Reload rejected by IO failure
- **WHEN** the YAML file cannot be read (permission denied, file
  missing)
- **THEN** the response is `{"ok": false, "error": "config file <path>: <error>"}`
- **AND** no in-memory state is modified

### Requirement: ChatOps provider selection at startup
autocoder SHALL read the `chatops.provider` field from `config.yaml` at
startup and construct a `Box<dyn ChatOpsBackend>` for the matching
provider via the chatops-manager factory. The supported values are
`slack`, `discord`, `teams`, `mattermost`, and `matrix`. Any other value
SHALL cause autocoder to exit non-zero at config-parse time.

#### Scenario: Slack provider selected
- **WHEN** `config.yaml` has `chatops.provider: slack` AND
  `chatops.slack.bot_token_env` names an env var whose value is set
- **THEN** the daemon constructs a `SlackBackend` and wraps it in
  `Arc<dyn ChatOpsBackend>` for the polling loop

#### Scenario: Experimental provider selected
- **WHEN** `config.yaml` has `chatops.provider:` set to any of `discord`,
  `teams`, `mattermost`, or `matrix` AND the matching `chatops.<provider>:`
  sub-block is present AND all required env vars are set
- **THEN** the daemon constructs the matching backend and wraps it in
  `Arc<dyn ChatOpsBackend>` for the polling loop

#### Scenario: Unknown provider rejected at config parse
- **WHEN** `config.yaml` has `chatops.provider:` set to a value not in the
  supported set
- **THEN** `Config::load_from` returns an error whose text names the
  invalid value AND lists the supported values

### Requirement: Loud warning when an experimental backend is active
autocoder SHALL emit exactly one startup log line per process declaring the
active ChatOps backend. When the active backend's `is_experimental()`
returns `true`, the log line SHALL be `warn`-level and SHALL contain the
substrings `"EXPERIMENTAL"` AND `"best-effort"` AND the provider name.
When `is_experimental()` returns `false`, the log line SHALL be
`info`-level and name the provider without the experimental markers.

#### Scenario: Slack backend logs info-level
- **WHEN** `chatops.provider: slack` is in use
- **THEN** the startup log emits one `info`-level line containing
  `"ChatOps escalation enabled via slack"`
- **AND** the line does NOT contain `"EXPERIMENTAL"` or `"best-effort"`

#### Scenario: Experimental backend logs warn-level
- **WHEN** `chatops.provider:` is `discord`, `teams`, `mattermost`, or
  `matrix`
- **THEN** the startup log emits one `warn`-level line containing
  `"EXPERIMENTAL"` AND `"best-effort"` AND the selected provider name
- **AND** the warning is emitted ONCE at startup, NOT per AskUser
  iteration

### Requirement: Missing provider sub-block fails fast
autocoder SHALL fail at startup, before spawning any polling task, when
the selected `chatops.provider` has no matching `chatops.<provider>:`
sub-block or when a required env var for the selected provider is unset.

#### Scenario: Provider selected with missing sub-block
- **WHEN** `chatops.provider: discord` AND `chatops.discord:` is absent
- **THEN** autocoder exits non-zero before spawning any polling task with
  an error message naming both `discord` and the missing sub-block

#### Scenario: Provider selected with missing env var
- **WHEN** `chatops.provider: discord` AND `chatops.discord.bot_token_env`
  names an env var that is unset
- **THEN** autocoder exits non-zero with an error naming the missing env
  var AND the provider it was needed for

### Requirement: Per-repository ChatOps channel override
autocoder SHALL allow each repository to override the global default
ChatOps channel by setting `chatops_channel_id` (provider-native format)
on the `repositories[]` entry. When absent, the repository uses
`chatops.default_channel_id`. The legacy `slack_channel_id` key on
repositories is removed from the config schema as part of the broader
`slack:` → `chatops:` rename.

#### Scenario: Per-repo override present
- **WHEN** a repository entry has `chatops_channel_id: <value>` set
- **THEN** AskUser escalations for that repository post to `<value>`

#### Scenario: Per-repo override absent
- **WHEN** a repository entry does NOT set `chatops_channel_id`
- **THEN** AskUser escalations for that repository post to
  `chatops.default_channel_id`

### Requirement: Per-repository config schema for the polling loop
The `RepositoryConfig` schema SHALL include an optional `max_changes_per_pr: u32` field that bounds the number of archived changes committed in one iteration's PR. When unset on a repository, the value SHALL fall back to the executor-level default `executor.max_changes_per_pr`; when both are unset, the global default of `3` SHALL apply.

#### Scenario: Per-repo override takes precedence
- **WHEN** a repository sets `max_changes_per_pr: 5` AND
  `executor.max_changes_per_pr` is unset (or set to a different value)
- **THEN** the effective cap for that repository is `5`

#### Scenario: Executor-level fallback applies when per-repo is unset
- **WHEN** a repository does NOT set `max_changes_per_pr` AND
  `executor.max_changes_per_pr` is `2`
- **THEN** the effective cap for that repository is `2`
- **AND** other repositories that also do not set the field also get
  `2` (the executor-level default is global)

#### Scenario: Global default when neither is configured
- **WHEN** neither `RepositoryConfig.max_changes_per_pr` nor
  `executor.max_changes_per_pr` is set
- **THEN** the effective cap is `3` for every repository

#### Scenario: A configured zero is clamped to one with a warning
- **WHEN** a configured value (per-repo or executor-level) is `0`
- **THEN** autocoder treats the effective cap as `1` AND emits exactly
  one WARN-level log line at startup naming the field path (e.g.
  `repositories[2].max_changes_per_pr` or
  `executor.max_changes_per_pr`) and the clamp
- **AND** the loaded `Config` retains the raw `0` so operator-visible
  diagnostics show what was configured (matching the
  `perma_stuck_after_failures` precedent)

### Requirement: Perma-stuck change detection
autocoder SHALL track consecutive failures per change in a per-repo `.failure-state.json` file at the workspace root. After the executor returns `Failed` for a change (or the daemon transforms a Completed-with-empty-workspace outcome to Failed), the counter for that change SHALL be incremented. After the executor returns `Archived` (including via self-heal), the counter for that change SHALL be cleared. When a change's counter reaches the configured `executor.perma_stuck_after_failures` threshold (default 2), autocoder SHALL write a `.perma-stuck.json` marker into the change directory, post a chatops alert, AND exclude the change from subsequent polling iterations until the marker is removed manually.

A `.perma-stuck.json` marker SHALL ALSO block the queue walk for subsequent pending changes in the same repository, per the same-repo blocking policy that already applies to `.in-progress*` AND `.needs-spec-revision.json` markers. The block is downgradeable per the `Ignore-for-queue marker downgrades blocking-marker behavior` requirement: an operator who stamps `.ignore-for-queue.json` alongside the perma-stuck marker tells the daemon "I know this one's broken; skip it AND proceed with the rest." The change stays excluded from `list_pending` (perma-stuck markers always exclude); the ignore-marker only releases the sibling-blocking effect.

#### Scenario: Failure increments the counter
- **WHEN** `handle_outcome` produces a `Failed` result for a change (whether the executor returned Failed or the daemon transformed a Completed-with-empty-workspace via the no-op-completion or self-heal logic into Failed)
- **THEN** autocoder reads `.failure-state.json` from the workspace root, increments the entry for that change (or creates it with `count: 1` if absent), sets `last_reason` and `last_failed_at`, and writes the file back atomically (write-temp-then-rename)
- **AND** transient daemon-side errors that prevent the executor from running (workspace init failure, openspec preflight failure, GitHub API transport error) do NOT increment the counter — only outcomes where the executor itself ran and Failed (or was forced to Failed by post-execution classification) count

#### Scenario: Archive clears the counter
- **WHEN** `handle_outcome` produces an `Archived` result for a change (including via the self-heal path from `self-heal-already-implemented`)
- **THEN** autocoder removes that change's entry from `.failure-state.json` and writes the file back atomically
- **AND** the next failure of any change starts fresh from `count: 1`

#### Scenario: Threshold reached → mark perma-stuck
- **WHEN** incrementing the counter results in `count >= executor.perma_stuck_after_failures` (default 2)
- **THEN** autocoder writes a `.perma-stuck.json` marker file inside the change directory containing the change name, consecutive_failures count, last_reason, marked_stuck_at timestamp, and the operator_action message
- **AND** autocoder posts a chatops alert via the configured backend with subject "change perma-stuck" and a body naming the repo, change, count, and last reason. The alert is subject to the existing 24h throttle so repeat-mark events do not spam
- **AND** autocoder logs an ERROR line naming the change and the marker file path
- **AND** when no chatops backend is configured, the ERROR log is the operator's only signal — the marker is still written and the change is still excluded from `list_pending` going forward

#### Scenario: Operator clears the marker
- **WHEN** the operator deletes `.perma-stuck.json` from a change directory (manually or via `@<bot> clear-perma-stuck`)
- **THEN** the next polling iteration sees the change in `list_pending` again and runs the executor against it
- **AND** the counter starts fresh at 0 (or whatever `.failure-state.json` records for that change after the removal — implementations MAY also clear the change's entry in `.failure-state.json` at marker-removal time; either is acceptable as long as the operator's "retry" signal does reset behavior)
- **AND** if a `.ignore-for-queue.json` marker accompanied the perma-stuck marker, `clear-perma-stuck` removes BOTH files (full resolution)

#### Scenario: Threshold is one
- **WHEN** `executor.perma_stuck_after_failures` is set to `1`
- **THEN** the very first Failed outcome for a change marks perma-stuck (no retry at all)

#### Scenario: Default threshold
- **WHEN** `executor.perma_stuck_after_failures` is unset
- **THEN** autocoder uses `2` as the threshold value

#### Scenario: Perma-stuck marker blocks subsequent pending changes by default
- **WHEN** a repository has a change with `.perma-stuck.json` AND a subsequent change in `list_pending` (no markers)
- **AND** the perma-stuck change does NOT also have `.ignore-for-queue.json`
- **THEN** the polling iteration's queue walk halts before processing the subsequent change
- **AND** an INFO log line names the blocking change AND the marker file path
- **AND** the operator's options are: (a) fix the perma-stuck change AND run `@<bot> clear-perma-stuck`, OR (b) run `@<bot> ignore-and-continue` to skip the broken change AND let siblings proceed

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

### Requirement: Periodic audit framework
autocoder SHALL include a periodic audit framework that runs registered audit tasks on per-audit cadences, persists last-run state per workspace, applies per-audit sandbox profiles, enforces post-hoc write restrictions, writes per-invocation logs, AND integrates with the polling loop. **The audit phase SHALL run AFTER the pending change queue walk completes, not before.** This change prevents an audit storm (e.g., 5 audits becoming eligible simultaneously after a HEAD change) from monopolizing the daemon for hours and blocking pending changes. Spec-writing audits' generated changes wait one iteration for implementation — the audit's creation commits ship in iteration N's PR; the implementer's commits for those generated changes ship in iteration N+1's PR.

#### Scenario: Framework runs registered audits after the pending queue walk
- **WHEN** a polling iteration completes its `recreate_branch` step
  AND completes `queue::list_waiting` AND `queue::list_pending`
- **AND** the iteration has remaining wall-clock budget AND has not been gated by an open PR
- **THEN** the framework iterates registered audits in declaration order
- **AND** for each audit, checks `.audit-state.json` to determine whether the configured cadence has elapsed AND `requires_head_change` is satisfied
- **AND** runs the audit only when due

#### Scenario: requires_head_change suppresses re-runs when HEAD unchanged
- **WHEN** an audit's `requires_head_change()` returns `true` AND the recorded `last_run_sha` for that audit equals the current `HEAD` SHA on the base branch
- **THEN** the framework skips the audit for this iteration even if the cadence interval has elapsed
- **AND** the next iteration after a HEAD change re-evaluates cadence and runs the audit if due

#### Scenario: requires_head_change false runs on cadence regardless of HEAD
- **WHEN** an audit's `requires_head_change()` returns `false` AND the cadence has elapsed since `last_run_at`
- **THEN** the framework runs the audit regardless of whether `HEAD` has changed
- **AND** this allows audits whose inputs are external (e.g. package registries, GitHub PR lists) to run periodically without depending on local code changes

#### Scenario: WritePolicy::None audit cannot modify the workspace
- **WHEN** an audit declares `WritePolicy::None` AND it runs
- **THEN** the audit's sandbox allows only `Read`, `Glob`, `Grep`, `Bash` — `Write` and `Edit` are denied at the tool layer
- **AND** after the audit returns, the framework runs `git status --porcelain` and asserts the workspace is clean
- **AND** if either the sandbox blocks a write attempt OR the post-hoc diff is non-empty, the audit is treated as failed: state is NOT updated, a chatops alert is posted, and the diff is reverted via `git reset --hard HEAD`

#### Scenario: WritePolicy::OpenSpecOnly audit may only write under openspec/changes/
- **WHEN** an audit declares `WritePolicy::OpenSpecOnly` AND it runs
- **THEN** the audit's sandbox allows `Write` and `Edit`
- **AND** after the audit returns, the framework inspects `git status --porcelain` and asserts every modified or new path begins with `openspec/changes/`
- **AND** if any path outside that prefix is touched, the audit is treated as failed: state is NOT updated, chatops alert is posted, the entire workspace diff is reverted

#### Scenario: Audit-run log written per invocation
- **WHEN** an audit runs (regardless of outcome)
- **THEN** autocoder writes a timestamped log at the resolved logs-dir path
- **AND** the log contains the audit type, workspace path, start AND end timestamps, resolved cadence + last-run info, the prompt used (for LLM audits), the raw audit output, AND the final `AuditOutcome` variant

#### Scenario: AuditOutcome::Reported posts to chatops
- **WHEN** an audit returns `AuditOutcome::Reported(findings)` AND chatops is configured
- **THEN** autocoder posts a single chatops message with a header line `📋 <repo>: <audit_type> — <N> finding(s)` followed by a bullet list of finding subjects

#### Scenario: AuditOutcome::Reported with no findings posts a brief OK
- **WHEN** an audit returns `AuditOutcome::Reported(vec![])` AND chatops is configured AND the operator has set `audits.<audit_type>.notify_on_clean: true` (default `false`)
- **THEN** autocoder posts `✅ <repo>: <audit_type> — no findings`
- **AND** when `notify_on_clean` is unset or `false`, no chatops post is made for an empty-findings outcome (silence is success)

#### Scenario: AuditOutcome::SpecsWritten records the change names; implementation waits one iteration
- **WHEN** an audit returns `AuditOutcome::SpecsWritten(names)` with non-empty `names`
- **THEN** the framework logs an info line naming each created change
- **AND** the audit's creation commit (one commit titled `audit: <type> proposals (N change(s))`) is on the agent branch when the iteration's push+PR step runs
- **AND** the new changes are NOT processed by THIS iteration's queue walk (because the queue walk already completed before the audit ran)
- **AND** the new changes ARE picked up by the NEXT iteration's `queue::list_pending` for normal implementer processing
- **AND** the implementer's commits for those changes ship in iteration N+1's PR — separable from iteration N's PR which contains only the audit creation commits

#### Scenario: State persists across daemon restarts
- **WHEN** the daemon stops AND restarts later
- **THEN** the framework reads `<workspace>/.audit-state.json` at startup AND resumes the existing cadence
- **AND** an audit due during the daemon's downtime runs on the first qualifying iteration after restart

#### Scenario: Audit failure does not abort the iteration
- **WHEN** an audit's `run()` returns `Err`
- **THEN** the framework logs the error at ERROR level naming the audit type and excerpt
- **AND** `.audit-state.json` is NOT updated for that audit
- **AND** the iteration continues to the push+PR step normally — the audit failure is isolated to that audit; other audits AND the push step are unaffected

#### Scenario: Iteration with pending changes processes them before audits
- **WHEN** an iteration begins AND has 2 pending changes in the queue AND 1 audit eligible to run
- **THEN** the iteration first processes both pending changes via the implementer (commits + archives)
- **AND** THEN runs the eligible audit
- **AND** the push+PR step at iteration end includes commits from both phases
- **AND** an operator watching chatops sees `🚀 starting work on <change>` BEFORE any `🔍 created proposal` or `📋 audit findings` messages for that iteration

#### Scenario: Iteration with only audits processes them when no pending exist
- **WHEN** an iteration begins AND has 0 pending changes AND 1 audit eligible to run
- **THEN** the iteration runs the audit
- **AND** the push+PR step ships the audit's commits (if any)
- **AND** if the audit created new proposals, those become pending for next iteration's queue walk

### Requirement: Audit cadence config schema
autocoder SHALL accept an optional top-level `audits:` block with `defaults:` (global) and per-repository `audits:` overrides. Each entry maps an audit type name to a `Cadence`. The `Cadence` enum SHALL accept the literal strings `disabled`, `daily`, `every-N-days` (where `N` is a positive integer), `weekly`, `monthly`, `quarterly`. Every audit defaults to `disabled` when unset in both global defaults and per-repo overrides.

#### Scenario: Per-repo cadence overrides global default
- **WHEN** `audits.defaults.architecture_brightline: weekly` AND a
  repository sets `audits.architecture_brightline: every-3-days`
- **THEN** the effective cadence for that repository is
  `every-3-days`

#### Scenario: Audit absent from both global and per-repo is disabled
- **WHEN** the operator's config has no entry for an audit type
  in either `audits.defaults` or any `repositories[].audits`
- **THEN** the audit's effective cadence is `disabled` AND the
  framework never invokes it

#### Scenario: every-N-days requires a positive integer
- **WHEN** a config entry uses `every-N-days` where N is `0` OR
  negative OR non-integer
- **THEN** config load fails at startup with an error naming the
  offending field path AND the parsed value

#### Scenario: Unknown audit type names fail config load
- **WHEN** a config entry under `audits.defaults` or
  `audits` (per-repo) uses a name that does not match a
  registered audit type
- **THEN** config load fails at startup with an error naming
  the field path AND the unknown audit type AND listing the
  known audit type names
- **AND** the daemon does NOT start

### Requirement: Architecture-brightline audit
autocoder SHALL ship an `architecture-brightline` audit in the periodic audit framework. The audit is pure-code (no LLM invocation), `requires_head_change = true`, AND `WritePolicy::None`. It SHALL produce `AuditOutcome::Reported(findings)` containing structural metrics that exceed configured (or default) thresholds.

The audit SHALL load a per-workspace `.brightline-ignore` file (if present) AND apply match-suppression to duplicate-signature findings whose constituent sites are all listed in the ignore file. The audit SHALL also validate ignore entries against the current workspace state AND report stale entries via the chatops top-line (informational; the audit does NOT modify the ignore file itself given its `WritePolicy::None`).

The ignore file's YAML schema:

```yaml
ignore:
  - file: <workspace-relative path>
    function: <function or method name>
    signature_match: <substring of the function's signature line>
    reason: <one-line operator-readable explanation>
```

All four fields are required per entry. An entry with a missing field triggers a WARN log AND the entry is skipped.

Match-suppression rule: a duplicate-signature finding is suppressed in full when EVERY constituent site matches an ignore entry. A partial match (some sites match, some don't) emits the finding with only the unmatched sites listed in the body. No match → the finding is emitted in full (today's behavior).

Stale-entry rule: each ignore entry is validated against the current workspace at audit time. Validation fails when (a) the named file doesn't exist, (b) the file doesn't contain a function with the named name, OR (c) the function's signature no longer contains `signature_match`. The audit collects the stale entries AND adds a trailing clause to the chatops top-line:

```
📐 architecture_brightline on <repo>: <N> file(s) over line threshold; <M> duplicate signature(s); <K> stale ignore entries to clean up
```

The threaded body lists each stale entry's `file + function + reason` so the operator knows what to remove. The audit does NOT modify `.brightline-ignore` on disk (given `WritePolicy::None`); cleanup is operator-driven.

#### Scenario: Reports files exceeding the size threshold
- **WHEN** the audit runs AND a tracked file under the repository's source root has more lines than the threshold (default `800`)
- **THEN** a finding of severity `medium` is included with `subject = "file <path> is <N> lines (threshold: <T>)"` AND `anchor = Some("<path>:1")`

#### Scenario: Reports identical function signatures across files
- **WHEN** the audit detects two or more functions with identical name + parameter list signatures in different files (excluding `mod tests {}` blocks)
- **AND** no ignore entry suppresses the finding (see the ignore scenarios below)
- **THEN** a finding of severity `low` lists each occurrence

#### Scenario: Reports dead public items
- **WHEN** the audit (or a static-analysis subprocess it invokes) identifies public items with zero references in the repository
- **THEN** a finding of severity `low` lists the items

#### Scenario: No findings produces silent outcome
- **WHEN** no metric exceeds its threshold AND no ignore entries are stale
- **THEN** the audit returns `AuditOutcome::Reported(vec![])`
- **AND** unless `notify_on_clean: true` is set, no chatops message is posted (per the framework-level scenario)

#### Scenario: Ignore entry suppresses a fully-matching finding
- **WHEN** a duplicate-signature finding involves 3 sites (file1.ts, file2.ts, file3.ts with function `foo` AND signature substring `async function foo(req`)
- **AND** `.brightline-ignore` contains entries for all 3 sites with matching `file`, `function`, AND `signature_match`
- **THEN** the audit does NOT emit the finding
- **AND** the `<M> duplicate signature(s)` count in the chatops top-line does NOT include this finding

#### Scenario: Partial ignore matches still emit with the unmatched sites
- **WHEN** a duplicate-signature finding involves 3 sites
- **AND** `.brightline-ignore` contains entries for 2 of the 3 sites
- **THEN** the audit emits the finding listing only the 1 unmatched site
- **AND** the chatops body for that finding names the unmatched site AND notes that 2 sites were suppressed by ignore entries

#### Scenario: Stale ignore entries are reported but not removed
- **WHEN** `.brightline-ignore` contains an entry for `examples/site-x/auth.ts:handleAuthCallback`
- **AND** that file has been deleted from the workspace
- **THEN** the audit marks the entry as stale
- **AND** the chatops top-line gains the trailing `; <K> stale ignore entries to clean up` clause
- **AND** the threaded body lists the stale entry with its `file + function + reason`
- **AND** the audit does NOT modify `.brightline-ignore` on disk

#### Scenario: Malformed entries WARN and are skipped
- **WHEN** `.brightline-ignore` contains an entry missing the `reason` field
- **THEN** the audit logs a WARN naming the offending entry AND skips it (treats it as if it didn't exist for the run)
- **AND** other valid entries continue to apply
- **AND** the on-disk file is unchanged

#### Scenario: Missing `.brightline-ignore` behaves identically to today
- **WHEN** the workspace has no `.brightline-ignore` file
- **THEN** the audit loads an empty ignore list
- **AND** no suppression occurs
- **AND** no stale-cleanup clause appears in the chatops output
- **AND** behavior is byte-identical to pre-spec runs

### Requirement: Dependency update triage audit
autocoder SHALL register a `dependency_update_triage` audit in the periodic-audit framework. The audit SHALL list Dependabot pull requests on the bot's fork (or upstream when no fork is configured), classify each by a strict "safe shape" filter, approve the safe ones via the GitHub Reviews API, and report unsafe ones via chatops. The audit is `requires_head_change = false` and `WritePolicy::None`.

#### Scenario: Lists Dependabot PRs on the fork in fork-PR mode
- **WHEN** the audit runs AND `github.fork_owner` is set
- **THEN** autocoder calls
  `GET /repos/<fork_owner>/<repo_name>/pulls?state=open` with the
  appropriate token, filters the response to PRs whose author
  `login` is `dependabot[bot]` OR `dependabot-preview[bot]`, AND
  iterates the resulting list

#### Scenario: Lists Dependabot PRs on upstream when fork mode is disabled
- **WHEN** the audit runs AND `github.fork_owner` is NOT set
- **THEN** autocoder lists PRs on the upstream repository
  (`<owner>/<repo_name>`) with the same Dependabot author filter
- **AND** the operator is responsible for ensuring the configured
  token has approval rights on upstream (the audit does not
  pre-check this)

#### Scenario: Safe-shape filter approves manifest-only version bumps
- **WHEN** a Dependabot PR's diff modifies only files matching the
  known-manifest list (`Cargo.toml`, `Cargo.lock`, `package.json`,
  `package-lock.json`, `yarn.lock`, `requirements.txt`,
  `pyproject.toml`, `*.csproj`, `packages.lock.json`, `go.mod`,
  `go.sum`, `Gemfile`, `Gemfile.lock`, `composer.json`,
  `composer.lock`, `pom.xml`, `build.gradle`, `build.gradle.kts`)
  AND every change within those files is a version-string update
  (no new top-level dependency entries, no removed entries, no
  `repository` / `homepage` / `registry` field changes, no new
  `scripts` / `postinstall` / `preinstall` / `prepublish` entries)
- **THEN** the audit submits an approving review:
  `POST /repos/<owner>/<repo>/pulls/<number>/reviews`
  with `{"event": "APPROVE", "body": "autocoder: safe-shape
  filter passed (manifest-only version bumps)"}`
- **AND** the approval counts toward the per-run cap

#### Scenario: Adding a new dependency entry fails safe-shape filter
- **WHEN** a Dependabot PR adds a `[dependencies] foo = "1.0"`
  line that did not exist in the base, OR adds a key to
  `package.json`'s `dependencies` / `devDependencies` map
- **THEN** the audit does NOT approve the PR
- **AND** posts a chatops finding of severity `medium` with
  subject `"PR #<num> adds new dependency entry — manual review
  required"`

#### Scenario: Changes to scripts / postinstall fail safe-shape filter
- **WHEN** a Dependabot PR adds or modifies any of:
  - `package.json`'s `scripts.postinstall`,
    `scripts.preinstall`, `scripts.prepublish`
  - any new top-level `scripts.*` entry that didn't exist before
  - `Cargo.toml`'s `build = "..."` field
  - a `pre-commit-hook` or `prepare` script field
- **THEN** the audit does NOT approve AND posts a chatops finding
  of severity `high` with subject `"PR #<num> modifies install
  scripts — manual review required"`

#### Scenario: Changes to URL/registry fields fail safe-shape filter
- **WHEN** a Dependabot PR modifies a `registry`, `repository`,
  `homepage`, `download-url`, or equivalent URL-bearing field for
  an existing dependency
- **THEN** the audit does NOT approve AND posts a chatops finding
  of severity `high` with subject `"PR #<num> changes dependency
  source URL — manual review required"`

#### Scenario: Non-manifest files in diff fail safe-shape filter
- **WHEN** a Dependabot PR's diff includes any file NOT in the
  known-manifest list (e.g. source files, README changes,
  workflow files)
- **THEN** the audit does NOT approve AND posts a chatops finding
  of severity `low` with subject `"PR #<num> modifies non-manifest
  files — manual review required"` and the body lists the
  unexpected paths

#### Scenario: Per-run approval cap enforced
- **WHEN** the audit's per-run `max_approvals_per_run` (default
  `5`) has been reached during the current invocation AND
  additional safe PRs remain in the list
- **THEN** the audit stops approving for this run
- **AND** posts a single chatops finding of severity `low` listing
  the deferred PR numbers, so the operator knows how many remain
- **AND** the next audit invocation continues from the same list
  (idempotent on already-approved PRs — GitHub returns the
  existing review without creating a duplicate)

#### Scenario: Already-approved PR is not re-approved
- **WHEN** a Dependabot PR has already been approved by the
  bot's user (visible in
  `GET /repos/<owner>/<repo>/pulls/<num>/reviews`)
- **THEN** the audit skips it for this run AND does NOT count it
  toward `max_approvals_per_run`
- **AND** does NOT post a chatops finding for it

#### Scenario: GitHub API failure on listing aborts the audit
- **WHEN** `GET /repos/<owner>/<repo_name>/pulls?state=open`
  returns non-2xx
- **THEN** the audit returns `Err` with the status code and
  response excerpt
- **AND** the framework treats this as audit failure: state is
  NOT updated, chatops alert is posted under the existing
  `audit-failure` category

#### Scenario: GitHub API failure on individual diff fetch skips that PR
- **WHEN** fetching a single PR's diff fails
- **THEN** the audit logs WARN, posts a chatops finding of
  severity `low` with subject `"PR #<num> diff fetch failed,
  skipping"`, AND continues to the next PR
- **AND** the audit itself returns successfully (so cadence
  advances normally)

### Requirement: Drift audit
autocoder SHALL register a `drift_audit` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with a read-only sandbox and a drift-detection prompt, then surfaces findings via chatops. The audit is `requires_head_change = true` and `WritePolicy::None`.

#### Scenario: Invokes the CLI with a read-only sandbox
- **WHEN** the audit runs
- **THEN** autocoder spawns the configured `executor.command`
  (typically `claude`) with `--settings` pointing at a generated
  sandbox file whose `permissions.deny` excludes `Write` and
  `Edit` and whose `allowed_tools` contains only
  `["Read", "Glob", "Grep", "Bash"]`
- **AND** the prompt is the embedded `prompts/drift-audit.md`
  template OR the operator-supplied override at
  `audits.drift_audit.prompt_path`
- **AND** the agent's working directory is the repository's
  workspace root

#### Scenario: Reads canonical specs from openspec/specs
- **WHEN** the drift-audit prompt instructs the agent to examine
  canonical specs
- **THEN** the prompt directs the agent to glob
  `openspec/specs/*/spec.md` AND read each capability's
  requirements
- **AND** the prompt directs the agent to ignore
  `openspec/changes/` (in-flight changes) and
  `openspec/changes/archive/` (historical changes)

#### Scenario: Outputs findings in a parseable format
- **WHEN** the agent completes
- **THEN** the agent's stdout SHALL be a single JSON object of
  shape:
  ```json
  {
    "findings": [
      {
        "capability": "orchestrator-cli",
        "requirement": "Per-repository asynchronous polling loop",
        "severity": "high",
        "code_anchors": ["autocoder/src/polling_loop.rs:45-95"],
        "divergence": "Spec requires <X>; code does <Y>."
      }
    ]
  }
  ```
- **AND** autocoder parses this JSON to produce `Finding`
  values for the `AuditOutcome::Reported(...)` return

#### Scenario: Filters out low-severity wording-only differences
- **WHEN** the prompt instructs the agent on severity classification
- **THEN** the prompt explicitly states: "Do NOT report findings
  whose only divergence is wording, formatting, or phrasing.
  Only report divergences with behavioral consequences."
- **AND** the agent SHOULD self-filter such findings before
  emitting the JSON

#### Scenario: Empty findings list produces silent outcome
- **WHEN** the agent returns an empty `findings` array
- **THEN** the audit returns `AuditOutcome::Reported(vec![])`
- **AND** per the framework-level "Reported with no findings"
  scenario, no chatops post is made unless
  `notify_on_clean: true`

#### Scenario: Malformed agent output fails the audit
- **WHEN** the agent's stdout is not parseable as the expected
  JSON shape (missing top-level `findings`, non-array value,
  malformed JSON, etc.)
- **THEN** the audit returns `Err` with the parse error AND a
  truncated stdout excerpt
- **AND** the framework treats this as audit failure: state is
  NOT updated, chatops alert posts under the existing
  audit-failure category, the next iteration retries

#### Scenario: Write attempt is blocked and treated as failure
- **WHEN** the agent attempts to call `Write` or `Edit` despite
  the sandbox
- **THEN** the CLI's permission system denies the call (the agent
  observes a tool error) AND on audit return the post-hoc
  `git status --porcelain` is empty
- **AND** if for any reason the post-hoc diff IS non-empty (e.g.
  the agent shelled out through Bash to a writeable command),
  the foundation's `WritePolicy::None` enforcement reverts via
  `git reset --hard HEAD` AND fails the audit

#### Scenario: Audit-run log captures the full agent output
- **WHEN** the audit runs (success or failure)
- **THEN** the audit-run log at
  `/tmp/autocoder/logs/<basename>/audits/drift_audit-<timestamp>.log`
  contains the prompt sent to the CLI AND the full raw stdout
  AND the full raw stderr AND the final outcome variant
- **AND** operators reviewing a confusing chatops finding can
  consult this log to see exactly what the agent produced

### Requirement: Missing-tests audit
autocoder SHALL register a `missing_tests_audit` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with an OpenSpec-only sandbox and a missing-tests prompt; it creates new OpenSpec change directories under `openspec/changes/`, commits them to the agent branch, and returns the created change names so the same iteration's queue walk implements them. The audit is `requires_head_change = true` and `WritePolicy::OpenSpecOnly`.

#### Scenario: Invokes the CLI with an OpenSpec-only sandbox
- **WHEN** the audit runs
- **THEN** autocoder spawns the configured `executor.command` with
  a sandbox whose `allowed_tools` includes `Write` and `Edit`
  alongside the read tools
- **AND** the prompt is the embedded
  `prompts/missing-tests-audit.md` template OR the
  operator-supplied override at
  `audits.missing_tests_audit.prompt_path`

#### Scenario: Prompt instructs additive-only output
- **WHEN** the prompt is loaded
- **THEN** the prompt explicitly states:
  - "Do NOT propose deleting existing tests."
  - "Do NOT propose modifying existing tests unless they are
    factually broken (failing or unreachable). When in doubt,
    leave the existing test alone and propose a NEW test."
  - "Suppress trivial gaps: getters, setters, single-line
    constructors, `Default` impls, `From`/`Into` conversions
    with no behavior."
- **AND** the prompt directs the agent to focus on uncovered
  error paths, edge cases, and branches without assertions

#### Scenario: Audit creates new OpenSpec changes
- **WHEN** the audit identifies N coverage gaps (where N is
  capped by `audits.missing_tests_audit.max_proposals_per_run`,
  default `2`)
- **THEN** the audit creates N change directories at
  `openspec/changes/<change_name>/` where each contains a
  proposal.md, tasks.md, and (when the gap implies a capability
  invariant) a `specs/<capability>/spec.md` delta
- **AND** each created change is named with a `tests-` prefix
  (e.g. `tests-error-paths-in-queue-engine`) so operators can
  recognize audit-produced changes at a glance

#### Scenario: Audit commits created changes to agent branch
- **WHEN** the agent finishes creating files
- **THEN** the audit framework's WritePolicy::OpenSpecOnly check
  passes (every modified path is under `openspec/changes/`)
- **AND** the audit runs `git add openspec/changes/ && git commit
  -m "audit: missing-tests proposals (N change(s))"`
- **AND** the audit returns
  `AuditOutcome::SpecsWritten(change_names)` where
  `change_names` is the list of newly-created change directory
  names

#### Scenario: Same iteration's queue walk picks up created changes
- **WHEN** the audit returns `SpecsWritten(names)` AND the
  iteration proceeds to `list_pending`
- **THEN** `list_pending` observes the new directories (they have
  `proposal.md`, no `.in-progress`, no `.question.json`)
- **AND** the iteration's `walk_queue` includes them in its
  archive cap, ordered by their `proposal.md` mtime
  (per the existing time-based ordering)

#### Scenario: Cap on proposals per run
- **WHEN** the prompt would produce more than
  `max_proposals_per_run` changes
- **THEN** the prompt instructs the agent to pick the N highest-
  priority gaps (by severity / risk) and emit only those
- **AND** the agent does NOT create more than N changes in this
  run; remaining gaps will be re-surfaced on subsequent runs as
  the audit re-evaluates the codebase

#### Scenario: Write outside openspec/changes triggers framework revert
- **WHEN** the agent writes a file outside `openspec/changes/`
  (e.g. a `src/foo.rs` modification or a `README.md` edit)
- **THEN** the foundation's `WritePolicy::OpenSpecOnly` post-hoc
  check fails AND the framework reverts via `git reset --hard
  HEAD + git clean -fd`
- **AND** the audit is treated as failed (state NOT updated,
  chatops alert posted, audit re-runs next iteration)
- **AND** no OpenSpec changes are committed from this run

#### Scenario: Empty findings produce no spec changes and no chatops post
- **WHEN** the audit identifies zero meaningful coverage gaps
- **THEN** the audit returns `AuditOutcome::SpecsWritten(vec![])`
- **AND** no commit is made, no chatops post is sent (per
  framework behavior for spec-writing audits)

### Requirement: Security & bug audit
autocoder SHALL register a `security_bug_audit` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with an OpenSpec-only sandbox and a security-and-bug-detection prompt; it creates new OpenSpec change directories under `openspec/changes/` describing proposed fixes, commits them, and returns the change names so the same iteration implements them. The audit is `requires_head_change = true` and `WritePolicy::OpenSpecOnly`.

The prompt's confidence-filtering and scope guidance below is design intent verified by the drift audit's semantic judgment; it SHALL NOT be pinned by a unit test asserting verbatim substrings of the prompt (per the project-documentation requirement `Tests assert behavior or derivation, never message wording`).

#### Scenario: Prompt steers the agent toward high-confidence, in-scope findings
- **WHEN** the security-bug audit prompt is loaded
- **THEN** it instructs the agent to report only findings it is
  reasonably confident about and to err toward NOT reporting when
  uncertain, because a false positive becomes wasted implementer
  work downstream
- **AND** it instructs the agent not to propose stylistic
  "best-practice" changes that do not address a concrete security
  issue or bug
- **AND** it scopes findings to concrete in-scope categories
  (injection, auth/authz mistakes, hard-coded secrets, unsafe
  deserialization, missing input validation at trust boundaries,
  race conditions, resource leaks, off-by-one, wrong operator,
  mishandled None/null, missing error propagation) and excludes
  out-of-scope categories (code style, naming, architectural
  opinions, performance unless measurable, anything the project
  has explicitly accepted)

#### Scenario: Created changes use fix- or secure- prefix
- **WHEN** the audit creates a change for a proposed fix
- **THEN** the change directory name uses `fix-` prefix for bug
  fixes (e.g. `fix-off-by-one-in-queue-walker`) AND `secure-`
  prefix for security hardening (e.g.
  `secure-sanitize-user-paths`)
- **AND** the operator can recognize audit-produced security/bug
  changes by their prefix at a glance

#### Scenario: Each proposed change includes a fix specification
- **WHEN** the audit creates a change
- **THEN** the change SHALL contain:
  - `proposal.md` naming the issue, citing the source location,
    and explaining the fix.
  - `tasks.md` listing the implementation steps.
  - When the fix implies a capability invariant (e.g. "every
    operation X SHALL validate Y"), a `specs/<capability>/spec.md`
    delta MODIFYING the relevant requirement OR adding a new
    requirement.
- **AND** validation via `openspec validate <name> --strict`
  passes before the audit commits the change

#### Scenario: Validation failure rejects the change without committing
- **WHEN** the agent produces a change that fails `openspec
  validate --strict`
- **THEN** the audit deletes the offending change directory AND
  records a WARN log entry naming the validation error
- **AND** the audit does NOT chatops-alert per-change validation
  failures (the audit-run log is sufficient operator signal)
- **AND** if every proposed change fails validation, the audit
  returns `AuditOutcome::SpecsWritten(vec![])` and no commit
  is made

#### Scenario: Per-run proposal cap
- **WHEN** the agent would produce more than
  `max_proposals_per_run` (default `2`) changes
- **THEN** the prompt instructs the agent to pick the
  highest-severity issues and emit only those
- **AND** the cap is enforced post-hoc: if the agent produces
  more, the audit keeps the first N (in directory-listing order
  after the post-run snapshot) and deletes the rest with a WARN
  log

#### Scenario: Write outside openspec/changes triggers framework revert
- **WHEN** the agent writes a file outside `openspec/changes/`
  (attempts to fix the bug directly, edits a source file, etc.)
- **THEN** the foundation's `WritePolicy::OpenSpecOnly` post-hoc
  check fails AND the framework reverts via
  `git reset --hard HEAD + git clean -fd`
- **AND** the audit is treated as failed; chatops alert posted;
  the audit re-runs next iteration

#### Scenario: Empty findings produce no spec changes and no chatops post
- **WHEN** the agent identifies zero confident security or bug
  issues
- **THEN** the audit returns `AuditOutcome::SpecsWritten(vec![])`
- **AND** no commit, no chatops post, the iteration proceeds
  normally

### Requirement: Architecture consultative audit
autocoder SHALL register an `architecture_consultative` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with a read-only sandbox and a consultative architecture prompt; it returns 0-5 anchored architecture questions as findings via chatops. The audit is `requires_head_change = true` and `WritePolicy::None`.

#### Scenario: Prompt forbids "rewrite at scale" suggestions
- **WHEN** the prompt is loaded
- **THEN** the prompt explicitly forbids the agent from suggesting:
  - splitting the codebase into microservices, separate processes,
    or separate binaries
  - rewrites in a different programming language
  - new infrastructure dependencies (message queues, databases,
    caches, RPC frameworks) unless the project already uses one
    of equivalent shape
  - patterns implying team-of-50 scale (event sourcing for a
    single-operator daemon, CQRS where a simple function would
    do, etc.)
- **AND** the prompt explicitly directs the agent to:
  - frame observations as questions, not directives
  - anchor each observation to a specific `file:line` range
  - drop suggestions whose implementation adds more code than
    it removes

#### Scenario: Prompt is language-agnostic
- **WHEN** the prompt is loaded
- **THEN** the prompt makes NO assumptions about programming
  language, framework, or runtime
- **AND** the prompt operates from observable structure (file
  organization, function boundaries, module interfaces) without
  language-specific idioms
- **AND** the prompt explicitly allows polyglot codebases
  (front-end + back-end, multi-language tools, language
  bridges) as a normal configuration to be observed, not
  flagged

#### Scenario: Returns 0-5 findings per run
- **WHEN** the audit runs
- **THEN** the agent's output contains a JSON object of shape:
  ```json
  {
    "findings": [
      {
        "subject": "Should X be its own module?",
        "body": "<one paragraph of context>",
        "anchor": "path/to/file.ext:120-180",
        "severity": "low" | "medium"
      }
    ]
  }
  ```
- **AND** the `findings` array contains AT MOST 5 entries
- **AND** if the audit produces 0 findings (no observations rise
  above the prompt's quality bar), the result is
  `AuditOutcome::Reported(vec![])` and per framework behavior no
  chatops post is sent unless `notify_on_clean: true`

#### Scenario: Findings render as questions in chatops
- **WHEN** the audit produces N findings AND posts to chatops
- **THEN** each bullet in the message is the finding's `subject`,
  which by prompt construction is phrased as a question
- **AND** the `anchor` is included so the operator can navigate
  directly to the cited code
- **AND** the full body text is preserved in the audit-run log
  (chatops only shows the subject + anchor for compactness)

#### Scenario: Malformed agent output fails the audit
- **WHEN** the agent's stdout cannot be parsed as the expected
  JSON shape OR includes more than 5 findings
- **THEN** the audit returns `Err` with the parse error AND a
  truncated stdout excerpt
- **AND** the framework treats this as audit failure: state is
  NOT updated, chatops alert posts under the existing
  audit-failure category, the next iteration retries

#### Scenario: Audit-run log captures the full agent output
- **WHEN** the audit runs (success or failure)
- **THEN** the audit-run log contains the prompt sent to the CLI,
  the full raw stdout, the full raw stderr, and the final
  outcome variant
- **AND** operators reviewing a confusing chatops finding can
  consult this log to see exactly what the agent produced

### Requirement: github.recreate_fork_on_reinit config field
The `github:` config block SHALL accept an optional `recreate_fork_on_reinit: bool` field that defaults to `false` when unset. When `true`, the workspace manager applies the destructive re-fork behavior described in `workspace-manager`'s "Optional fork recreation on workspace reinitialization" requirement.

#### Scenario: Field defaults to false when absent
- **WHEN** the operator's `github:` block does NOT include a
  `recreate_fork_on_reinit` key
- **THEN** the effective value is `false` AND the conservative
  fetch-fork-at-init behavior applies on fresh clones

#### Scenario: Field is global, not per-repo
- **WHEN** the operator sets `github.recreate_fork_on_reinit: true`
- **THEN** the flag applies to every configured repository in this
  daemon process AND there is no per-repo override
- **AND** the rationale is that `github.fork_owner` is itself global
  (all repos in one autocoder process share the same fork owner),
  so re-fork policy follows the same scope

#### Scenario: Field requires fork-PR mode to have any effect
- **WHEN** `recreate_fork_on_reinit: true` AND `github.fork_owner`
  is unset (direct-push mode)
- **THEN** config load succeeds without error (the field is not
  invalid; it's just inactive)
- **AND** the daemon emits an INFO log at startup noting that
  `recreate_fork_on_reinit: true` has no effect when fork mode is off
- **AND** no re-fork attempts are made at runtime

### Requirement: Perma-stuck chatops alert content
When autocoder writes a `.perma-stuck.json` marker for a change AND chatops is configured AND `failure_alerts_enabled` is true, autocoder SHALL post exactly one chatops notification (subject to the existing per-change 24h throttle) whose body names the repository URL, the change name, the consecutive failure count, the last reason excerpt, the marker file path, AND the per-change run log path.

#### Scenario: Alert body includes the run log path
- **WHEN** autocoder writes the perma-stuck marker for change
  `<change>` in workspace `<workspace>` AND the alert is not
  throttled
- **THEN** the posted chatops message body contains a line of
  the form `run_log: <log_path>` where `<log_path>` is the
  per-change run log written by the executor (for the Claude
  CLI executor, this is `/tmp/autocoder/logs/<workspace_basename>/<change>.log`)
- **AND** the line appears BEFORE the operator-action sentence
  describing how to retry (so the operator reads the diagnostic
  pointer before the action they would take to re-engage)

#### Scenario: Alert body retains pre-existing fields
- **WHEN** the alert is posted
- **THEN** the body still contains: `repo:`, `change:`,
  `consecutive_failures:`, `last_reason:`, AND a sentence
  naming the marker path that the operator must remove to
  retry
- **AND** the existing 24h-per-change throttle still applies
  (a second perma-stuck mark within the throttle window does
  not re-post)

#### Scenario: Log path is omitted when not derivable
- **WHEN** the executor backend does not expose a per-change
  run log path (e.g. a future executor with no run-log
  convention)
- **THEN** the `run_log:` line is omitted from the message body
  rather than rendering an empty path
- **AND** the rest of the body is unchanged

### Requirement: PR title and body describe what landed
PRs opened by autocoder SHALL carry a title and body that describe the actual changes shipped, derived from data already on hand at PR-creation time (the change slugs and each change's archived `proposal.md`). The title SHALL humanize the change slug — replacing hyphens with spaces and (when the slug uses the `aNN-` stacked-change convention) preserving the prefix as a labeled segment. The body SHALL include each change's `## Why` text under a per-change markdown heading. Both fields SHALL be deterministic functions of the changes processed in this iteration so re-running the same pass produces the same title and body.

#### Scenario: Single-change PR
- **WHEN** an iteration archives exactly one change `a06-refactor-portal-handlers-to-fromref` AND opens a PR
- **THEN** the PR title is `"a06: refactor portal handlers to fromref"`
  (or equivalent: the `aNN-` prefix is preserved as the label, the
  remainder has hyphens replaced with spaces, the colon separates
  them)
- **AND** the PR body contains a `## a06-refactor-portal-handlers-to-fromref`
  heading followed by the verbatim contents of that change's
  archived `proposal.md`'s `## Why` section
- **AND** the PR body ends with the existing `"Changes implemented
  in this pass:\n\n- <slug>\n"` reference list (one bullet per
  archived change)

#### Scenario: Multi-change PR
- **WHEN** an iteration archives three changes `a04-foo`, `a05-bar`,
  `a06-baz` AND opens a PR
- **THEN** the PR title is `"a04: foo (+2 more)"` — the first
  change's humanized form plus a count suffix naming the
  remaining changes
- **AND** the PR body contains three `## <slug>` sections in input
  order, each followed by that change's `## Why` text
- **AND** the PR body's final section is the slug-list reference

#### Scenario: A change's proposal.md is missing or malformed
- **WHEN** an iteration archives a change whose proposal.md is
  unreadable (file absent, permissions error, or no `## Why`
  heading present)
- **THEN** the PR body's section for that change uses
  `_(no proposal.md available)_` (or similar placeholder) instead
  of crashing or omitting the section
- **AND** the other changes' sections are unaffected — the
  fallback is per-change, not per-PR
- **AND** the build does not panic; the iteration completes
  normally and the PR opens with degraded body content

#### Scenario: Title length cap
- **WHEN** a change slug is long enough that the humanized title
  would exceed 80 characters
- **THEN** the title is truncated to fit, with the truncated
  portion replaced by `"…"`
- **AND** the `aNN-` prefix label (if present) is preserved at the
  start of the truncated title so the change identifier remains
  recognizable in GitHub's PR list

#### Scenario: Self-heal disclaimer interacts with the new body shape
- **WHEN** an iteration's commits include one or more self-heal
  archive-only commits (existing requirement: "Reject archive-only
  iterations as Failed", self-heal exception)
- **THEN** the PR body's first paragraph remains the existing
  self-heal disclaimer (`"_This PR archives one or more changes
  whose implementation was already present on the base branch..."`)
- **AND** the per-change `## Why` sections follow the disclaimer,
  preserving the existing reader cue that some changes have no
  code diff

### Requirement: Dirty workspace auto-recovers mid-iteration
autocoder SHALL attempt automatic recovery before falling back to the existing alert-and-return-Err behavior when a polling iteration's pre-pass dirty check finds a non-empty `git status --porcelain` output (after filtering autocoder bookkeeping files like `.alert-state.json`). Recovery consists of (best-effort) `git checkout <base_branch>`, `git reset --hard origin/<base_branch>`, and `git clean -fd` — identical to the startup recovery. After recovery, autocoder SHALL re-run the dirty check; if clean, the iteration proceeds past the dirty check as if the workspace had been clean initially.

Recovery is safe in this position because (a) the agent branch is rebuilt from base each iteration via `recreate_branch`, so wholesale wiping does not lose recoverable work, and (b) any uncommitted modifications at this point are by definition residue from a previously-failed executor invocation whose outcome was already `Failed`/`Escalated` and whose work the operator does not want to ship.

#### Scenario: Workspace dirty due to prior failed executor invocation
- **WHEN** a polling iteration's pre-pass `git status --porcelain` is
  non-empty after filtering autocoder bookkeeping files (typically
  because the previous iteration's executor modified tracked files but
  returned `Failed` or timed out without committing)
- **THEN** autocoder logs a `warn`-level line naming the dirty entry
  count and indicating recovery is being attempted
- **AND** autocoder runs (best-effort) `git checkout <base_branch>`,
  then `git reset --hard origin/<base_branch>`, then `git clean -fd`
  in the workspace
- **AND** autocoder re-runs `git status --porcelain`; if empty,
  logs `info` "workspace recovered mid-iteration; proceeding" and
  the iteration continues into its normal flow (fetch, checkout
  base, recreate agent branch, queue walk)
- **AND** NO `WorkspaceDirtyMidIteration` chatops alert is posted
  for this iteration — recovery succeeded, so the operator does
  not need to be notified

#### Scenario: Workspace remains dirty after recovery attempt
- **WHEN** the recovery commands all complete but a subsequent
  `git status --porcelain` is still non-empty (gitignored state,
  read-only mount, file-locking, etc.)
- **THEN** autocoder posts a `WorkspaceDirtyMidIteration` chatops
  alert (subject to the existing 24h throttle) naming the
  repository URL and a short excerpt of the porcelain output
- **AND** the iteration returns `Err` with the existing message
  shape, preserving prior conservative behavior for genuinely
  unrecoverable cases

#### Scenario: Workspace already clean
- **WHEN** the pre-pass `git status --porcelain` is empty
  (after filtering autocoder bookkeeping files)
- **THEN** no recovery commands are executed
- **AND** the iteration proceeds normally, identical to prior
  behavior — recovery is invoked ONLY when the dirty check would
  otherwise trip

#### Scenario: Recovery command itself fails
- **WHEN** any of the recovery commands (`git reset --hard`,
  `git clean -fd`) returns a non-zero exit
- **THEN** autocoder posts a `WorkspaceDirtyMidIteration` alert
  whose error excerpt names the recovery failure (not the
  original dirty state) so the operator sees the actionable
  problem
- **AND** the iteration returns `Err`; the polling loop proceeds
  to the next sleep as with any iteration-level failure

### Requirement: Periodic audits enforce their per-audit subprocess timeout
Every audit that spawns the wrapped agent CLI as a child process (`drift_audit`, `architecture_consultative_audit`, `missing_tests_audit`, `security_bug_audit`) SHALL kill the child and return `Err(_)` once the elapsed wall-clock time exceeds `executor.timeout_secs`. The error message SHALL name both the audit type and the timeout condition so the operator can tell from a single log line which audit hung and why. The audit log file SHALL record the timeout outcome before the error returns so post-mortem inspection of `/tmp/autocoder/logs/<basename>/audits/<audit_type>-<ts>.log` is conclusive.

#### Scenario: drift_audit subprocess exceeds timeout
- **WHEN** `DriftAudit::run` is invoked with `executor_timeout_secs = 1` AND the configured `executor.command` is a script that sleeps longer than the timeout
- **THEN** the call returns `Err(_)` whose `format!("{err:#}")` contains the substring `drift_audit` AND the substring `timeout`
- **AND** the audit log file written via the audit's `AuditLogWriter` contains a `kind: Err` section together with the substring `reason: timeout`
- **AND** the spawned child process does not survive past the call's return (no orphaned `sleep` left behind)

#### Scenario: architecture_consultative_audit subprocess exceeds timeout
- **WHEN** `ArchitectureConsultativeAudit::run` is invoked with `executor_timeout_secs = 1` AND the configured command sleeps longer than the timeout
- **THEN** the call returns `Err(_)` whose message contains `architecture_consultative` AND `timeout`
- **AND** the audit log file contains a `kind: Err` / `reason: timeout` section

#### Scenario: specs-writing audit (via missing_tests) subprocess exceeds timeout
- **WHEN** `MissingTestsAudit::run` is invoked with `executor_timeout_secs = 1` AND the configured command sleeps longer than the timeout
- **THEN** the call returns `Err(_)` whose message contains `missing_tests_audit` AND `timeout`
- **AND** no new directory is created under `<workspace>/openspec/changes/` as a side-effect of the timed-out run (defense-in-depth against the spec-writing audit's commit step running on a child that never finished)

### Requirement: Control socket rejects malformed requests with a named error
The control socket's `dispatch_request` SHALL respond with `{"ok": false, "error": "<message>"}` (the same envelope used for `unknown action`) when the incoming line cannot be turned into an `{action: ...}` request. The error message SHALL distinguish "the line was not JSON" from "the line was JSON but had no action field" so an operator running `nc -U <socket>` from a shell can tell whether the typo is in their JSON syntax or their field name.

#### Scenario: Request line is not valid JSON
- **WHEN** the daemon's control socket receives a line whose body is not valid JSON (e.g. `not-json\n`)
- **THEN** the response is a single JSON object with `ok == false` AND `error` containing the substring `malformed JSON`
- **AND** the connection is closed after the response is written

#### Scenario: Request JSON parses but lacks an `action` field
- **WHEN** the daemon's control socket receives a line whose body parses as a JSON object that has no `action` field (e.g. `{}` or `{"unrelated":"x"}`)
- **THEN** the response is a single JSON object with `ok == false` AND `error` containing the substrings `missing` AND `action`
- **AND** the response error is distinguishable from the `malformed JSON` error so log triage can tell typo-in-syntax from typo-in-field-name

### Requirement: Polling-loop helpers handle their boundary inputs without panicking
Three small pure helpers in the polling loop (`extract_stdout_section`, `filter_alert_state_lines`, `truncate_reason`) have branchy behavior whose boundaries change observable operator-facing output: the PR-comment summary the implementer posts, the workspace-dirty alert that fires when uncommitted changes are detected, and the perma-stuck chatops excerpt. Each helper SHALL behave deterministically across the boundary inputs below and SHALL NOT panic on malformed or multi-byte input.

#### Scenario: extract_stdout_section returns the slice between markers
- **WHEN** `extract_stdout_section` is called with a log body containing both a `=== STDOUT (...)` header line AND a `=== STDERR (...)` line
- **THEN** the returned slice is the text strictly between the newline after the STDOUT header and the start of the STDERR marker

#### Scenario: extract_stdout_section returns empty when STDOUT marker is missing
- **WHEN** `extract_stdout_section` is called with a body that contains no `=== STDOUT (` substring
- **THEN** the returned slice is empty (no panic, no false-positive content)

#### Scenario: extract_stdout_section returns empty when STDOUT header has no terminating newline
- **WHEN** `extract_stdout_section` is called with a body containing `=== STDOUT (n) ===` but no `\n` after that header
- **THEN** the returned slice is empty (the early-return guard against partial input fires)

#### Scenario: extract_stdout_section runs to EOF when STDERR marker is absent
- **WHEN** `extract_stdout_section` is called with a body whose STDOUT marker is present AND whose STDERR marker is absent
- **THEN** the returned slice is the body from just after the STDOUT header line through end-of-input

#### Scenario: filter_alert_state_lines strips only exact-path entries
- **WHEN** `filter_alert_state_lines` is called with porcelain text containing a mix of real-file entries AND a line whose path is exactly `.alert-state.json`
- **THEN** the returned text omits the `.alert-state.json` line AND preserves every other entry verbatim
- **AND** a line whose path is `subdir/.alert-state.json` OR `prefix.alert-state.json` is NOT filtered (the check is exact-equality, not substring match)

#### Scenario: truncate_reason boundary behavior
- **WHEN** `truncate_reason` is called with input length less than or equal to its cap
- **THEN** the returned string equals the input verbatim AND does not end with `…`
- **AND WHEN** the input length exceeds the cap
- **THEN** the returned string ends with `…` AND its `chars().count()` equals the cap plus one
- **AND** truncation respects UTF-8 char boundaries (no panic on multi-byte input even when byte-count and char-count diverge)

### Requirement: Registered periodic audits
autocoder SHALL register exactly the following audits in its `AuditRegistry` at startup, identified by their `audit_type()` slug: `architecture_brightline`, `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`. The slug `dependency_update_triage` SHALL NOT be registered. Each registered audit's cadence is independently configurable under `audits.defaults` and per-repo `repositories[].audits` overrides; an unregistered slug present in either location SHALL fail config validation at startup with the existing "unknown audit type" error message that lists the registered slugs.

This enumeration is the canonical contract for which audits exist. Future changes that add or remove an audit MUST update this requirement in the same commit so the spec and the registered set never drift. The `validate_audit_type_names` startup check enforces the spec/code consistency at runtime: an operator's YAML naming an unregistered slug is a startup-time failure with a clear list of valid slugs.

#### Scenario: Startup with default config registers the canonical set
- **WHEN** autocoder starts with a config whose `audits:` block is
  absent OR present but with all-`disabled` cadences
- **THEN** the in-memory `AuditRegistry` contains exactly the five
  audits enumerated above
- **AND** no audit runs (all are `Disabled` by effective cadence),
  preserving prior daemon behavior

#### Scenario: Operator configures a registered audit
- **WHEN** an operator sets a non-`disabled` cadence under
  `audits.defaults.<slug>` for any of the five registered slugs
  OR under `repositories[].audits.<slug>`
- **THEN** config validation succeeds AND the scheduler invokes
  that audit per its cadence on the appropriate iteration

#### Scenario: Operator configures the removed dependency_update_triage slug
- **WHEN** an operator's `audits.defaults` (or
  `repositories[].audits`, or `audits.settings`) contains the key
  `dependency_update_triage` (a slug that was registered in
  earlier versions of autocoder but has since been removed)
- **THEN** `validate_audit_type_names` fails at startup with an
  error naming `dependency_update_triage` as unknown AND listing
  the registered slugs so the operator knows what to use
- **AND** the daemon does NOT start (consistent with the existing
  behavior for typos in audit slugs); the operator must remove the
  entries from their YAML to recover

#### Scenario: Adding or removing an audit requires updating this requirement
- **WHEN** an implementing agent ships a change that registers a
  new audit (extending the registry list) or removes one (deleting
  a registration)
- **THEN** the change's spec delta MUST update this requirement's
  enumeration so the canonical list reflects the new state
- **AND** the change's commit SHOULD also update the
  `validate_audit_type_names` known-slug list, the README audit
  table, and `config.example.yaml` so all four artifacts (spec,
  validator, README, example) stay aligned

### Requirement: Install subcommand
autocoder SHALL ship an `install` subcommand alongside `run`, `rewind`, and `reload`. The subcommand SHALL collect the minimum configuration an operator needs for a working first-run (one repository URL, a GitHub PAT, optional chatops backend, optional reviewer backend), generate a `config.yaml` + `secrets.env` pair at the appropriate location for the chosen install mode (server vs dev), and on server mode generate + enable a systemd unit that runs the daemon as a dedicated `autocoder` system user. All OS-mutating actions (`useradd`, `chown`, `chmod`, `apt-get install`, `systemctl daemon-reload`, `systemctl enable`, `systemctl start`, claude installer subprocess) SHALL go through a `SystemActions` trait whose production implementation shells out and whose test implementation records calls — so `cargo test` covers the orchestration without needing a real host.

#### Scenario: First-time install (server mode)
- **WHEN** an operator runs `autocoder install` (typically via
  `install.sh`'s `exec autocoder install "$@"` handoff) on a
  Linux host with systemd available AND no existing
  `<config-dir>/config.yaml`
- **THEN** the subcommand creates the `autocoder` system user
  (idempotent: skipped if already present), prompts for the
  essential config fields, writes `/etc/autocoder/config.yaml`
  (chmod 640, owner root:autocoder) and
  `/etc/autocoder/secrets.env` (chmod 600, owner root:autocoder),
  renders and enables `/etc/systemd/system/autocoder.service`
  running as `User=autocoder` with
  `EnvironmentFile=/etc/autocoder/secrets.env`, starts the
  service (prompted, default yes), and prints a post-install
  summary

#### Scenario: First-time install (dev mode)
- **WHEN** an operator runs `autocoder install` on macOS OR on
  Linux without systemd available OR with the `--mode dev` flag
  AND no existing config
- **THEN** the subcommand prompts for the same essential
  fields, writes config to `~/.config/autocoder/config.yaml`
  (chmod 600, owned by the operator's UID), writes
  `~/.config/autocoder/secrets.env` (chmod 600), does NOT
  create a system user, does NOT install a systemd unit, AND
  prints `autocoder run --config ~/.config/autocoder/config.yaml`
  as the start command

#### Scenario: Existing config detected
- **WHEN** an operator runs `autocoder install` AND
  `<config-dir>/config.yaml` already exists
- **THEN** the subcommand prints a status block naming the
  existing config path, notes that any binary swap has already
  happened (in install.sh), AND exits 0 without prompting for
  anything
- **AND** the operator's existing config and secrets files are
  not touched

#### Scenario: Non-interactive mode with all required flags
- **WHEN** an operator runs
  `autocoder install --non-interactive --repo-url <url>
  --token-env-var GITHUB_TOKEN --chatops-backend none
  --reviewer-provider none`
- **THEN** the subcommand runs end-to-end without reading from
  stdin
- **AND** the generated config.yaml + secrets.env reflect the
  flag values verbatim
- **AND** the operator can drive `autocoder install` from
  Ansible, Terraform, cloud-init, etc. without a TTY

#### Scenario: Non-interactive mode missing a required flag
- **WHEN** an operator runs `autocoder install --non-interactive`
  WITHOUT supplying `--repo-url`
- **THEN** the subcommand exits non-zero with an error message
  naming the missing flag explicitly AND listing the full set of
  flags required for non-interactive mode
- **AND** no partial config is written to disk

#### Scenario: SystemActions abstraction tested via mock
- **WHEN** the install-subcommand tests run under `cargo test`
- **THEN** every test uses a `RecordingActions` impl of
  `SystemActions` that captures method calls into an in-memory
  vector
- **AND** tests assert the exact sequence of calls (e.g.
  `create_user("autocoder", ...)`, `daemon_reload()`,
  `enable_systemd_unit("autocoder")`,
  `start_systemd_unit("autocoder")`) for the server-mode flow
- **AND** no test ever calls the production
  `RealSystemActions::create_user` or runs `useradd` for real
  — the tests verify orchestration, not the underlying OS calls
- **AND** the production `RealSystemActions` impl is small
  enough (target ≤ 5 lines per method) to inspect by reading

#### Scenario: Wizard prompts are testable via scripted IO
- **WHEN** the wizard tests run
- **THEN** they use a `ScriptedIo` impl of the `WizardIo` trait
  that reads from a pre-loaded `VecDeque<String>` of answers
- **AND** assert the generated config.yaml + secrets.env match
  expected values for those answers
- **AND** no test depends on a TTY being available

### Requirement: Spec-needs-revision executor outcome + marker
The executor SHALL return a new `ExecutorOutcome::SpecNeedsRevision` variant when one or more tasks in a change's `tasks.md` require capabilities outside the executor's sandbox. The agent flags upfront — BEFORE making any changes to the workspace — by scanning `tasks.md` against an enumerated set of unimplementable-task patterns. When the outcome fires, autocoder SHALL write an operator-cleared `.needs-spec-revision.json` marker in the change's directory, post a chatops alert under a new `AlertCategory::SpecNeedsRevision` (24h-throttled per the existing per-category window), and halt the queue walk for the iteration (consistent with the existing halt-on-non-archive semantic). The marker SHALL exclude the change from `list_pending` until removed by the operator, mirroring the perma-stuck marker's pattern.

The agent SHALL NOT auto-edit `tasks.md` to make the spec implementable. The agent flags; the operator authors the edit. This preserves the project's invariant that no AI process modifies its own marching orders without human review.

#### Scenario: Agent flags unimplementable tasks before doing work
- **WHEN** the executor invokes the agent on a change whose
  `tasks.md` includes one or more tasks matching the
  unimplementable-task patterns documented in the implementer
  prompt template (e.g. `sudo` on real host, missing tools,
  real GitHub tag pushes, browser interactions, VM/container
  spin-up, manual smoke tests, manual external observation)
- **THEN** the agent emits the `SpecNeedsRevision` outcome
  with each flagged task's id + verbatim text + one-line
  reason AND a free-form `revision_suggestion` describing
  what to change in `tasks.md`
- **AND** the agent does NOT modify any files in the workspace
  before emitting the outcome (the flag-and-halt happens
  pre-implementation; no partial work is committed)

#### Scenario: autocoder writes the marker and alerts
- **WHEN** the executor returns `SpecNeedsRevision { ... }` for
  change `<slug>` in workspace `<workspace>`
- **THEN** autocoder writes
  `<workspace>/openspec/changes/<slug>/.needs-spec-revision.json`
  containing: `change` name, RFC-3339 `marked_at`, the full
  `unimplementable_tasks` list, the `revision_suggestion`, and
  a static `operator_action` field naming the file the
  operator needs to edit
- **AND** posts exactly one chatops notification under
  `AlertCategory::SpecNeedsRevision` (subject to the existing
  24h per-category throttle) whose body lists each flagged
  task's id + text, the agent's revision suggestion, the
  operator action checklist, AND the marker file path + the
  per-change run log path
- **AND** halts the queue walk for this iteration: no later
  pending changes are processed in this iteration (mirroring
  the `halt-queue-walk-on-non-archive` semantic)

#### Scenario: Marker excludes change from list_pending
- **WHEN** a subsequent iteration runs AND the marker
  `openspec/changes/<slug>/.needs-spec-revision.json` exists
- **THEN** `queue::list_pending` does NOT return `<slug>`
- **AND** the executor is never invoked for `<slug>` in this
  iteration
- **AND** the perma-stuck counter for `<slug>` is NOT
  incremented (the marker is operator-action territory, not
  repeat-failure territory)

#### Scenario: Marker is operator-cleared, not auto-cleared
- **WHEN** an operator edits `tasks.md` to revise the flagged
  tasks AND commits + pushes the revision
- **THEN** the marker file `.needs-spec-revision.json` is
  NOT auto-removed by autocoder on the next iteration
- **AND** the operator must delete the marker file
  (typically by `rm` and a subsequent commit, OR by deleting
  it locally and relying on autocoder's iteration to surface
  the now-cleaned state on next pass — the marker is in
  `.git/info/exclude` so it's never committed, but operators
  who want a literal git-tracked clear may use `git rm`)
- **AND** the next iteration after the marker is gone
  proceeds normally: the change re-enters `list_pending`
  and the executor is invoked against the revised tasks.md

#### Scenario: Operator overrides an over-conservative flag
- **WHEN** an operator reviews the flagged tasks AND judges
  the agent was overly cautious (e.g. the agent flagged a
  task the operator believes IS implementable)
- **THEN** the operator deletes the marker file WITHOUT
  editing tasks.md
- **AND** the change re-enters `list_pending` on the next
  iteration
- **AND** if the agent flags the same tasks again, the
  operator may add a comment in tasks.md near the flagged
  task explaining why it's implementable (e.g. naming a
  tool path or workflow that resolves the concern), OR they
  may update the implementer prompt template via a separate
  change to relax the relevant pattern

#### Scenario: Marker file is gitignored at workspace root
- **WHEN** `workspace::ensure_initialized` runs
- **THEN** `.git/info/exclude` contains
  `.needs-spec-revision.json` (added alongside the existing
  `.failure-state.json`, `.audit-state.json`,
  `.perma-stuck.json` entries)
- **AND** the marker file does NOT trip the pre-pass
  dirty-workspace check AND is NOT removed by
  `git clean -fd` during the per-iteration recovery path

#### Scenario: Agent does NOT auto-edit tasks.md
- **WHEN** the agent identifies one or more unimplementable
  tasks
- **THEN** the agent emits the outcome with the list AND a
  suggestion text, but does NOT modify `tasks.md` itself
- **AND** does NOT create or modify any spec artifacts under
  `openspec/changes/<slug>/`
- **AND** does NOT submit a PR proposing the revision
- **AND** the operator remains the sole author of the tasks.md
  edit, preserving the contract that no AI process edits its
  own marching orders without human review

#### Scenario: Malformed outcome sentinel falls back to Failed
- **WHEN** the agent emits a `SpecNeedsRevision` sentinel
  that fails to deserialize (missing required fields, unknown
  type, empty `unimplementable_tasks` list, etc.)
- **THEN** the Claude CLI executor logs a WARN naming the
  parse failure with an excerpt of the offending payload
- **AND** the executor returns `Failed { reason: "agent
  emitted unparseable SpecNeedsRevision sentinel: <excerpt>"
  }` instead of the new variant
- **AND** the polling loop's existing Failed-outcome handling
  kicks in (perma-stuck counter increments, no marker
  written) — the unparseable-sentinel case must NOT silently
  succeed

### Requirement: Archive-collision pre-flight exclusion
autocoder SHALL detect, at the top of every polling iteration's queue walk, the structural condition where a pending change would fail at archive time because its dated archive entry already exists. For each change name `<slug>` in the iteration's pending set, the polling loop SHALL check whether `openspec/changes/archive/<UTC-YYYY-MM-DD>-<slug>/` exists; if so, the change SHALL be excluded from this iteration without invoking the executor, AND a chatops alert under a new `AlertCategory::ArchiveCollision` SHALL be posted (subject to the existing per-category 24h throttle). The exclusion does NOT count as a perma-stuck failure — the situation is a structural one the operator must resolve, not a repeatable executor failure.

The motivation is cost: invoking the executor for a change that will demonstrably fail at archive time burns real agent-API tokens on work that cannot land. Pre-flight detection costs microseconds and prevents the full executor invocation.

#### Scenario: Both paths present blocks the executor
- **WHEN** an iteration enters `walk_queue` AND a pending change
  `foo` has BOTH `openspec/changes/foo/` AND
  `openspec/changes/archive/<today>-foo/` present on disk
- **THEN** autocoder excludes `foo` from this iteration's
  working set BEFORE the executor is invoked
- **AND** the executor is NEVER called for `foo` in this
  iteration
- **AND** autocoder posts exactly one chatops alert under
  `AlertCategory::ArchiveCollision` (subject to the 24h
  throttle) naming both paths AND describing the operator
  workflow to resolve the collision
- **AND** the per-change failure-state counter for `foo` is
  NOT incremented (collision is a structural condition, not
  an executor failure)

#### Scenario: Only the archive entry exists is the normal post-archive state
- **WHEN** an iteration runs AND a change `foo` has ONLY
  `openspec/changes/archive/<today>-foo/` present (no active
  dir at `openspec/changes/foo/`)
- **THEN** `list_pending` does not return `foo` at all (the
  active dir is absent, so the change is not pending)
- **AND** no collision check applies; no alert fires; the
  iteration proceeds normally with whatever other changes
  are in pending

#### Scenario: Mixed collision and clean changes in the same iteration
- **WHEN** an iteration's pending set contains `foo` (with
  the collision condition) AND `bar` (clean, archive entry
  absent)
- **THEN** `foo` is excluded with the collision alert
- **AND** `bar` is processed normally: executor invoked,
  outcome handled, archive moved, etc.
- **AND** the iteration's `processed` list contains `bar` (if
  it produced a diff) and does NOT contain `foo`

#### Scenario: Repeated collision within 24h is throttled
- **WHEN** a previous iteration in the last 24 hours has
  already posted an `ArchiveCollision` alert for repository
  `<repo>` AND a fresh iteration detects the same condition
- **THEN** no chatops post is made (24h per-category
  throttle applies, same as every other predictable failure
  category)
- **AND** the WARN-level log line still emits per-iteration
  so journalctl tailing shows the diagnosis even with
  chatops disabled

### Requirement: Perma-stuck counter covers all per-change errors
The perma-stuck failure-state counter SHALL increment for every per-change error returned from the polling loop's per-change processing function, not only for executor-reported Failed outcomes. Specifically: any `Err` returned by `queue::archive`, by the post-executor commit step, by `queue::unlock`, or by any other operation scoped to the per-change loop counts as one failure for the affected change. When the counter reaches `executor.perma_stuck_after_failures`, the existing perma-stuck marker is written AND the existing chatops alert fires.

Iteration-level errors that happen OUTSIDE the per-change loop (workspace init, dirty-workspace pre-pass check, branch push, PR creation) MUST NOT increment any change's counter — those have their own throttled chatops categories and are not attributable to a specific pending change.

#### Scenario: Executor Failed increments the counter (existing behavior pinned)
- **WHEN** the executor returns `Failed { reason }` for a
  change `foo`
- **THEN** `failure_state::record_failure(ws, "foo", reason)`
  is called exactly once for this iteration
- **AND** the counter for `foo` increments by 1

#### Scenario: Post-executor archive failure increments the counter (new behavior)
- **WHEN** the executor returns `Completed` for a change
  `foo` AND `queue::archive` (or any subsequent per-change
  step) returns `Err`
- **THEN** `failure_state::record_failure(ws, "foo", reason)`
  is called exactly once for this iteration, with `reason`
  naming the error origin (e.g. "archive failed: <message>")
- **AND** the counter for `foo` increments by 1

#### Scenario: Counter increment threshold writes the marker
- **WHEN** the counter for change `foo` reaches
  `executor.perma_stuck_after_failures` (default 2) via any
  combination of executor failures and post-executor
  failures
- **THEN** autocoder writes
  `openspec/changes/foo/.perma-stuck.json` AND the existing
  perma-stuck chatops alert fires (per the existing
  "Perma-stuck chatops alert content" requirement)
- **AND** subsequent iterations exclude `foo` from
  `list_pending` until the marker is removed by the operator

#### Scenario: Iteration-level error does not increment per-change counter
- **WHEN** an iteration fails at workspace init, OR fails the
  pre-pass dirty check (even after the auto-recovery
  attempt), OR fails at branch push, OR fails at PR creation
- **THEN** no per-change counter increments
- **AND** the iteration's failure routes through the
  appropriate iteration-level `AlertCategory`
  (`WorkspaceInitFailure`, `WorkspaceDirtyMidIteration`,
  `BranchPushFailure`, `PrCreationFailure`)
- **AND** the per-change processing function was either
  never entered (init/dirty failures) or did not return Err
  itself (push/PR failures happen after the per-change loop
  completes)

#### Scenario: No double-counting on executor-Failed
- **WHEN** the executor returns `Failed` AND the existing
  outcome handler calls `record_failure`
- **THEN** the broader wrapper does NOT also call
  `record_failure` for the same change in the same iteration
- **AND** the counter increments by exactly 1, not 2

### Requirement: Chatops operator commands
The chatops listener SHALL recognize a small set of operator-issued commands as in-channel equivalents of the most common SSH-and-edit operator workflows: querying daemon state, clearing exclusion markers, and wiping the local workspace. Commands SHALL be addressed to the bot via the per-backend mention syntax (Slack `<@bot>`, Discord `<@!bot>`, etc.) followed by a verb and arguments. Unrecognized verbs SHALL be silently ignored (no negative feedback for typos in normal channel chat). Recognized commands SHALL be parsed by a backend-independent parser, dispatched as actions through the existing Unix-domain control socket, and replied to in the same channel where the command arrived.

The initial verb set is:

- `status <repo-substring>` — returns a multi-line summary of the daemon's view of the named repo
- `clear-perma-stuck <repo-substring> <change-slug>` — removes the change's `.perma-stuck.json` marker
- `clear-revision <repo-substring> <change-slug>` — removes the change's `.needs-spec-revision.json` marker
- `wipe-workspace <repo-substring>` — destructive; requires two-step confirmation

The threat model is unchanged from existing chatops behavior: write access to the channel is the trust boundary. Sites needing finer-grained control configure per-repo channels via the existing `chatops_channel_id` override.

#### Scenario: status returns aggregated daemon state for the named repo
- **WHEN** an operator posts `@<bot> status your-repo` in a
  channel where the chatops listener is active AND `your-repo`
  resolves to exactly one configured repository
- **THEN** the bot posts a single multi-line reply containing
  (any subset of these sections may be empty and omitted):
  active markers (`.perma-stuck.json` and
  `.needs-spec-revision.json` entries with their metadata),
  currently-engaged 24h alert throttles, the last iteration's
  outcome + timestamp + next-iteration estimate, AND a queue
  snapshot (pending changes, waiting/escalated changes,
  marker-excluded changes)
- **AND** if `your-repo` matches multiple configured repos, the
  reply lists the matches AND asks for a more specific
  substring
- **AND** if no repo matches, the reply lists every
  configured repo's URL so the operator sees their options

#### Scenario: clear-perma-stuck removes the marker
- **WHEN** an operator posts
  `@<bot> clear-perma-stuck your-repo a06-foo`
- **THEN** the bot resolves the repo, submits a
  `ClearPermaStuckMarker` action to the control socket
- **AND** on success: the marker file is deleted from disk
  AND the bot posts a one-line confirmation
  `✓ cleared .perma-stuck.json for a06-foo on your-repo`
- **AND** the next polling iteration's `list_pending`
  returns the change (assuming no other markers exclude it)
- **AND** on marker-not-found: the bot posts
  `✗ no perma-stuck marker for change a06-foo on your-repo`
  (informational; not retried)

#### Scenario: clear-revision removes the spec-revision marker
- **WHEN** an operator posts
  `@<bot> clear-revision your-repo a07-bar`
- **THEN** the bot resolves the repo, submits a
  `ClearRevisionMarker` action, and on success deletes
  `openspec/changes/a07-bar/.needs-spec-revision.json` AND
  posts the success confirmation
- **AND** failure modes mirror `clear-perma-stuck`:
  no-such-marker / no-such-repo errors with the same shape

#### Scenario: wipe-workspace two-step confirmation
- **WHEN** an operator posts `@<bot> wipe-workspace your-repo`
  in channel `C` AND `your-repo` resolves to a unique repo
- **THEN** the bot posts a warning
  `⚠️ This will delete /tmp/workspaces/<sanitized-url>
  (forces a re-clone on the next iteration). Reply 'confirm'
  within 60 seconds.`
- **AND** the bot stores an in-memory pending-confirmation
  entry keyed by `C` with a 60-second expiry
- **WHEN** the operator (any channel member) replies
  `confirm` in `C` within 60 seconds
- **THEN** the bot submits the `WipeWorkspace` action,
  removes the pending entry, AND posts
  `✓ wiped /tmp/workspaces/<sanitized-url>; next iteration
  will re-clone`
- **AND** if no `confirm` reply arrives within 60 seconds,
  the pending entry expires AND a subsequent `confirm` reply
  is treated as if there were no pending confirmation
  (`✗ no pending wipe-workspace confirmation in this
  channel (or it expired)`)

#### Scenario: Cross-channel confirmations do not match
- **WHEN** the wipe-workspace command is issued in channel A
  AND the `confirm` reply is posted in channel B
- **THEN** channel B's `confirm` does NOT trigger the wipe
  (no pending confirmation exists in channel B)
- **AND** channel A's pending confirmation expires after 60s
  without firing

#### Scenario: Unknown verbs are silently ignored
- **WHEN** a message starts with the bot mention but the
  next token is not in the recognized verb set (e.g.
  `@<bot> hello`, `@<bot> please archive everything`, an
  AskUser reply that doesn't match an open question)
- **THEN** the operator-command parser returns `None`
- **AND** the chatops listener continues to the existing
  AskUser-reply detection path (so chatops-escalation
  replies still work as today)
- **AND** if neither path matches, the message is ignored
  silently (no error reply, no log spam beyond the existing
  message-received DEBUG log)

#### Scenario: Repo-substring matching is case-insensitive
- **WHEN** an operator posts `@<bot> status MYREPO`,
  `@<bot> status YOUR-REPO`, or `@<bot> status your-repo`
- **THEN** all three forms resolve to the same configured
  repository (assuming the substring is unique under
  case-insensitive matching)

#### Scenario: Chatops commands use the same control socket as autocoder CLI
- **WHEN** any operator command's action is performed
- **THEN** the chatops listener submits the action via the
  existing Unix-domain control socket (the same socket used
  by `autocoder reload`)
- **AND** the new action handlers (RepoStatus,
  ClearPermaStuckMarker, ClearRevisionMarker, WipeWorkspace)
  are reachable in principle to any future CLI subcommand
  (e.g. `autocoder clear-perma-stuck <repo> <change>`)
  without duplicating logic
- **AND** the control socket's existing authn
  (Unix-socket-perms, daemon-user-only) applies identically

#### Scenario: Pause / resume / clear-alert-throttle are deliberately absent
- **WHEN** an operator posts `@<bot> pause your-repo` (or
  `resume`, `clear-alert-throttle`)
- **THEN** the message is parsed as an unknown verb AND
  silently ignored (per the unknown-verbs scenario above)
- **AND** the spec explicitly leaves these verbs to
  follow-up changes when usage patterns indicate they're
  worth adding

### Requirement: Install wizard configures periodic audits
The `autocoder install` wizard SHALL prompt operators about periodic audits during first-time install, after the reviewer prompt and before the config-assembly step. The wizard offers a three-tier UX: (1) inline prompt for `spec_sync_audit` with default ON at daily cadence (cheap, defensive, no LLM cost); (2) a single yes/no gate for the LLM-driven audits (default no — operators who don't want a tour answer once and move on); (3) a fast-path "enable all five at recommended cadences" question for operators who answered yes to the gate, with per-audit walk-through as the fallback when the fast path is declined. The non-interactive mode SHALL mirror this with flags whose defaults match the conservative interactive defaults so existing IaC scripts that don't know about the new flags continue to work without behavior change.

#### Scenario: Default interactive path enables spec_sync_audit only
- **WHEN** an operator runs `autocoder install` AND accepts
  every audit-related default (bare-Enter on the spec-sync
  cadence prompt → `daily`; bare-Enter on the LLM-driven
  gate → `no`)
- **THEN** the wizard writes `audits.defaults.spec_sync_audit: daily`
  to config.yaml AND no other audit entries
- **AND** the operator's total interaction with the audits
  section is two prompts (cadence + gate)

#### Scenario: Operator declines spec_sync_audit
- **WHEN** the operator answers `n` (never) to the spec-sync
  cadence prompt
- **THEN** the wizard skips the LLM-driven-audits gate
  AND any subsequent per-audit prompts
- **AND** the rendered config.yaml omits the `audits:`
  block entirely (matching the `Option<AuditsConfig>`
  schema's `None` representation)

#### Scenario: Fast-path enables all six audits
- **WHEN** the operator chose a non-disabled cadence for
  spec-sync AND answered `y` to the LLM-driven-audits gate
  AND accepted the fast-path default `Y` on the "enable all
  five with recommended cadences" prompt
- **THEN** config.yaml contains all six audits at their
  recommended cadences:
  - `spec_sync_audit`: per the operator's spec-sync answer
  - `architecture_brightline`: weekly
  - `drift_audit`: weekly
  - `missing_tests_audit`: monthly
  - `security_bug_audit`: weekly
  - `architecture_consultative`: monthly
- **AND** total wizard interaction in this branch is three
  prompts (spec-sync cadence + LLM gate + fast-path
  acceptance)

#### Scenario: Individual cadence walk-through after declining fast-path
- **WHEN** the operator answered `y` to the LLM-driven gate
  AND `n` to the fast-path prompt
- **THEN** the wizard prompts for each of the five LLM-driven
  audits individually: slug + description + cadence choice
  (with the recommended cadence as the default)
- **AND** each audit's chosen cadence appears in
  `audits.defaults` UNLESS the operator chose `never`
  (those audits are omitted)
- **AND** the resulting config.yaml's audit count matches
  the operator's non-disabled choices (spec-sync + each LLM
  audit the operator did NOT decline)

#### Scenario: Non-interactive defaults match conservative interactive defaults
- **WHEN** an operator runs `autocoder install --non-interactive`
  with all the existing-spec's required flags AND NO new
  `--audits-*` flags
- **THEN** config.yaml contains exactly
  `audits.defaults.spec_sync_audit: daily` (the
  conservative default matching the interactive default-default)
- **AND** existing IaC scripts (Ansible playbooks, cloud-init,
  etc.) that pre-date this change continue to produce a
  working install without surprise behavior change

#### Scenario: Non-interactive recommended preset
- **WHEN** an operator runs
  `autocoder install --non-interactive --audits-llm-driven recommended`
  with all other required flags
- **THEN** config.yaml contains all six audits at their
  recommended cadences (same as the interactive fast-path)
- **AND** no per-audit `--audit-<slug>` flag is required

#### Scenario: Non-interactive per-audit override within recommended preset
- **WHEN** the operator passes
  `--audits-llm-driven recommended --audit-security-bug-audit disabled`
- **THEN** four of the five LLM-driven audits get their
  recommended cadences AND `security_bug_audit` is omitted
  from config.yaml (treated as disabled)
- **AND** spec-sync follows its own `--audits-spec-sync`
  flag (or default `daily` if unset)

#### Scenario: --audits-llm-driven none master switch overrides per-audit flags
- **WHEN** the operator passes
  `--audits-llm-driven none --audit-architecture-brightline weekly`
- **THEN** architecture_brightline is NOT enabled (the
  master switch wins)
- **AND** the rendered config.yaml has no
  architecture_brightline entry
- **AND** the wizard emits a one-line stdout note explaining
  that the per-audit flag was overridden by the master
  switch (so IaC logs distinguish "operator opted-out
  explicitly" from "operator forgot to set the flag")

#### Scenario: Audit description rendering
- **WHEN** the wizard prompts for any audit's cadence
- **THEN** the prompt body includes the audit's
  `description()` string (a one-line operator-facing
  description, ≤ 80 chars, from the `Audit` trait)
- **AND** the description is enough for an operator to
  recognize the audit in subsequent chatops alerts or
  config.yaml lines without needing to consult external
  documentation

### Requirement: autocoder invokes openspec archive for the archive step
autocoder SHALL perform per-change archive operations by invoking `openspec archive <change> -y` as a subprocess in the workspace directory, rather than doing its own filesystem move. The `-y` flag suppresses confirmation prompts so the subprocess runs cleanly in the non-interactive polling-loop context. On exit code 0, autocoder treats the change as successfully archived (the change directory has moved to `openspec/changes/archive/<UTC-date>-<slug>/` AND the canonical specs at `openspec/specs/<capability>/spec.md` have been merged with the change's `## ADDED`/`## MODIFIED`/`## REMOVED`/`## RENAMED` deltas). On any non-zero exit, autocoder treats the iteration as Failed for that change, with the openspec stderr as the failure reason; the change stays at the active path for the operator to investigate.

The merge step requires the openspec host profile to have the `sync` workflow enabled (one-time `openspec config profile`). Without `sync`, `openspec archive` will move the change directory but the canonical-spec merge will not run. autocoder iterations on such a host succeed at the file-move level; drift accumulates until either the operator enables `sync` and re-runs the backfill subcommand, OR (when OpenSpec re-bundles `sync` by default in a future release) the host's openspec installation acquires the workflow automatically.

#### Scenario: Successful archive merges canonical specs
- **WHEN** autocoder finishes implementing change `<slug>`,
  commits the working tree, and invokes
  `openspec archive <slug> -y`
- **AND** the host's openspec profile has `sync` enabled
- **THEN** the subprocess exits 0
- **AND** the change directory has moved from
  `openspec/changes/<slug>/` to
  `openspec/changes/archive/<UTC-date>-<slug>/`
- **AND** each capability spec under
  `openspec/specs/<capability>/spec.md` named in the
  change's deltas has been updated with the requirement
  blocks from the corresponding delta section

#### Scenario: openspec archive failure surfaces as Failed iteration
- **WHEN** `openspec archive <slug> -y` exits non-zero
  (validation error in the rebuilt canonical spec, the
  archive destination collides with an existing dated dir,
  the change is malformed, openspec is missing from PATH,
  etc.)
- **THEN** autocoder treats the change as Failed for the
  iteration with the openspec stderr (truncated to a
  reasonable size for log/alert display) as the failure
  reason
- **AND** the change stays at
  `openspec/changes/<slug>/` (the active path) for the
  operator to investigate
- **AND** the standard per-change failure handling applies
  (failure-state counter increments, perma-stuck after
  threshold, queue walk halts for this iteration per the
  existing halt-on-non-archive semantic)

#### Scenario: Host without openspec sync configured
- **WHEN** autocoder runs on a host whose openspec profile
  does NOT have `sync` enabled
- **AND** an iteration calls `openspec archive <slug> -y`
- **THEN** the subprocess still exits 0 (archive's file
  move always succeeds), the change is archived correctly,
  but the canonical specs at `openspec/specs/` are NOT
  updated for this change's deltas
- **AND** drift accumulates: the change's `## ADDED`
  requirements are documented in the archived entry but
  not present in the canonical spec
- **AND** the operator can reconcile via
  `autocoder sync-specs --backfill` (see below)

#### Scenario: openspec missing from PATH
- **WHEN** the openspec CLI is not on the autocoder user's
  PATH
- **THEN** `Command::new("openspec")` returns an
  ErrorKind::NotFound IO error
- **AND** autocoder surfaces this as the Failed reason for
  the change with an explicit "openspec not found on PATH"
  message and a pointer to the README's openspec install
  step
- **AND** the daemon does NOT crash or halt — the iteration
  fails, the polling loop continues to the next sleep

Backfill of pre-existing drift is a separate concern handled by the companion `rebuild-canonical-specs-from-archive` change. This change is scoped strictly to "stop creating new drift."

### Requirement: Rebuild canonical specs from archive
autocoder SHALL ship a mechanism to fully rebuild every canonical spec under `openspec/specs/` from the archived change history under `openspec/changes/archive/`. The mechanism SHALL be exposed via a CLI subcommand (`autocoder sync-specs --rebuild`) for operator use against any workspace AND via a chatops verb (`@<bot> rebuild-specs <repo>`) for in-channel triggering against daemon-managed repos. The rebuild SHALL iterate archives in chronological order, invoke `openspec archive` for each to replay the deltas onto a freshly-cleared canonical state, and preserve each archive directory's original date prefix via in-place rename after openspec produces a today-dated entry.

There is intentionally no incremental "sync only the missing changes" mode: incremental backfill is unreliable when drift is mid-history rather than end-of-history (later changes' MODIFIED requirements may have been built on top of merged versions of earlier changes; re-applying skipped earlier changes onto current canonical produces an incorrect end state). Full rebuild is the only safe operation; it's cheap enough that the simplicity is worth more than the small optimization a smarter mode would provide.

#### Scenario: Rebuild produces correct canonical state from archive history
- **WHEN** an operator runs
  `autocoder sync-specs --rebuild --workspace <path>` against
  a repo whose canonical specs are missing requirements that
  ARE present in the archived changes' deltas
- **THEN** the subcommand removes every existing canonical
  spec under `openspec/specs/<capability>/`
- **AND** iterates each archived change in chronological
  order (by name's date prefix)
- **AND** for each: moves the dated dir out of archive,
  invokes `openspec archive <slug> -y`, openspec applies
  the deltas (creating or updating canonical specs as
  needed), and the dir returns to archive with its original
  date prefix preserved via in-place rename
- **AND** at the end, every canonical spec contains every
  requirement from every archived change's deltas, in the
  correct chronologically-applied order

#### Scenario: Rebuild on a repo with no drift is a noop diff
- **WHEN** the rebuild runs on a repo whose canonical specs
  already match what would be produced by chronological
  replay (no drift)
- **THEN** the subcommand still runs the full rebuild cycle
  (clear + replay all archives) — there's no separate "is
  there drift?" mode
- **AND** `git diff openspec/specs/` after the rebuild
  shows no semantic changes (possibly minor formatting
  differences from openspec's serialization, but no
  requirement adds/removes/modifications)
- **AND** the operator reviewing the rebuild PR sees an
  empty-or-cosmetic diff and either merges (harmless) or
  declines

#### Scenario: Date prefixes preserved via in-place rename
- **WHEN** the rebuild processes archive
  `2026-05-15-foo-bar`
- **AND** `openspec archive foo-bar -y` succeeds, producing
  `archive/<today>-foo-bar`
- **THEN** the subcommand renames the new entry back to the
  original: `mv archive/<today>-foo-bar archive/2026-05-15-foo-bar`
- **AND** the archive directory's chronological order is
  preserved across the rebuild — subsequent rebuilds
  iterate in the same correct order
- **AND** the rebuild itself produces no net diff in the
  archive directory tree (each entry moves out and back
  with the same name)

#### Scenario: openspec archive failure during rebuild
- **WHEN** the rebuild is processing N changes and one
  fails (`openspec archive <slug> -y` exits non-zero — e.g.
  a delta references a requirement that openspec's
  validator rejects in the rebuilt context)
- **THEN** the subcommand logs an ERROR with the openspec
  stderr
- **AND** leaves the failing change at the active path
  (`openspec/changes/<slug>`) for the operator to inspect
- **AND** continues to the next archived change (subsequent
  changes may also fail if they depend on the failed one;
  these accumulate in the report)
- **AND** at the end the subcommand prints a summary listing
  every successful and every failed change with stderr
  excerpts, and exits non-zero

#### Scenario: Chatops verb schedules rebuild for next iteration
- **WHEN** an operator posts
  `@<bot> rebuild-specs <repo-substring>` in a chatops
  channel the listener is watching AND the substring
  resolves to exactly one configured repo
- **THEN** the listener submits a
  `RebuildSpecs { url, immediate: false }` action to the
  control socket
- **AND** the control socket sets `pending_rebuild = true`
  on the named repo's polling task in-memory state
- **AND** the bot replies in-channel:
  `✓ rebuild scheduled for <repo> — will run within ~Ns
  (current iteration must finish first)`
- **AND** when the polling loop's current iteration (if
  any) finishes, the next iteration checks the flag,
  clears it, runs the rebuild instead of the normal queue
  walk, commits the result, and the existing push/PR flow
  ships a PR with a recognizable title (e.g.
  `spec rebuild: <N> capability(ies) rebuilt`)

#### Scenario: --immediate cancels current iteration before rebuilding
- **WHEN** an operator runs
  `autocoder sync-specs --rebuild --immediate
  --workspace <path>` against a workspace where a daemon
  iteration is currently in progress
- **THEN** the subcommand reads the busy marker, sends
  SIGTERM to the recorded executor pid, and waits up to
  30 seconds for the busy marker to be released
- **AND** once released (or after the 30s timeout with a
  WARN log), runs the rebuild
- **AND** any partial workspace state left by the killed
  iteration is cleaned by the rebuild's first git-status
  check + dirty-workspace recovery (the existing
  recover-dirty-workspace-mid-iteration infrastructure)

#### Scenario: Without --immediate, CLI blocks waiting for iteration to finish
- **WHEN** an operator runs
  `autocoder sync-specs --rebuild --workspace <path>` (no
  `--immediate`) AND a daemon iteration is in progress
- **THEN** the CLI polls the busy marker periodically,
  logs progress so the operator can see what's happening,
  AND blocks until the iteration finishes naturally
- **AND** once the iteration releases the busy marker, the
  CLI proceeds with the rebuild
- **AND** the CLI never invokes SIGTERM in this mode

#### Scenario: Chatops verb does not support --immediate
- **WHEN** an operator posts
  `@<bot> rebuild-specs <repo-substring> --immediate`
- **THEN** the parser does NOT recognize `--immediate` as
  a valid argument in chatops; the verb parses as
  `rebuild-specs` with the entire remainder as the
  repo-substring (which won't match), OR the parser
  rejects the malformed invocation
- **AND** the bot replies with the same error shape used
  for any unrecognized verb shape: `✗ no repo matched
  '<repo-substring> --immediate'; configured: <list>`
- **AND** operators wanting `--immediate` must SSH to the
  daemon host and invoke the CLI directly

#### Scenario: Rebuild on a workspace with no daemon (local clone)
- **WHEN** the operator runs the CLI against a local clone
  of a repo (no autocoder daemon running on this host;
  no busy marker present)
- **THEN** the rebuild proceeds immediately
- **AND** `--immediate` and the absence of `--immediate`
  behave identically (no iteration to coordinate with)
- **AND** the operator commits + pushes the rebuild
  manually (the CLI does not push)

#### Scenario: Rebuild discards hand-edited canonical content
- **WHEN** a canonical spec contains a `## Purpose`
  paragraph OR a `### Requirement:` that was hand-edited
  into existence without any archived change introducing
  it
- **THEN** the rebuild discards that content (no archive
  references it, so the rebuilt canonical doesn't include
  it)
- **AND** any capability spec that openspec creates from
  scratch during the rebuild gets a placeholder Purpose
  (openspec's default: `"TBD - created by archiving
  change <X>. Update Purpose after archive."`)
- **AND** the README documents this loss-on-rebuild
  behavior so operators don't run rebuild expecting
  hand-edits to survive

#### Scenario: End-of-rebuild chatops notification — success with drift
- **WHEN** a rebuild iteration runs, every archived change
  re-archives successfully (`report.failed == 0`), the
  rebuild produces modified canonical files, and the
  iteration's push + PR creation succeed
- **THEN** exactly one chatops notification fires when
  chatops is configured:
  `✓ rebuild complete for <repo>: PR <pr_url> opened —
  <N> capability(ies) updated from <M> archived change(s)`
- **AND** the notification is NOT gated on
  `failure_alerts_enabled` or `pr_opened_enabled` (this
  is a direct response to an operator-triggered command;
  the operator wants the completion signal regardless of
  other notification toggles)
- **AND** the existing PR-opened notification ALSO fires
  per the established contract — operators see two posts:
  the generic "PR opened" notification and this rebuild-
  specific completion notification

#### Scenario: End-of-rebuild chatops notification — no drift
- **WHEN** a rebuild iteration runs AND every archived
  change re-archives successfully AND no canonical files
  end up modified (the rebuild reproduced the existing
  canonical exactly — no drift was present)
- **THEN** no commit is created (nothing to stage), no PR
  opens, no PR-opened notification fires
- **AND** exactly one chatops notification fires when
  chatops is configured:
  `✓ rebuild complete for <repo>: no drift detected,
  canonical specs already in sync`
- **AND** the operator gets explicit closure on the
  rebuild they requested — no silent disappearance

#### Scenario: End-of-rebuild chatops notification — partial failure
- **WHEN** a rebuild iteration runs AND one or more
  archived changes fail to re-archive (e.g. openspec
  archive exits non-zero on them; per the existing
  `Per-change failure during backfill does not abort the
  whole run` scenario, the rebuild continues with the
  remaining changes)
- **THEN** if any successful changes produced canonical
  modifications: those modifications are committed and a
  PR opens (containing the partial result)
- **AND** exactly one chatops notification fires:
  `⚠️ rebuild for <repo> completed with <N> failure(s);
  PR <pr_url-or-"(no PR — every change failed)"> opened
  with successful <M> change(s).
  Failed: <slug1>, <slug2>, ... [and K more].
  See journalctl -u autocoder for openspec stderr details.`
- **AND** the failed-slugs list truncates to the first 10
  entries with an `"and K more"` suffix to keep the
  notification body manageable in chat clients
- **AND** the failed changes' directories remain at the
  active path (`openspec/changes/<slug>/`) for the
  operator to inspect — they are NOT moved back to
  archive automatically

#### Scenario: End-of-rebuild notification when chatops is not configured
- **WHEN** a rebuild iteration completes AND
  `chatops_ctx.is_none()` (the daemon has no chatops
  configured)
- **THEN** no chatops post is attempted
- **AND** the rebuild iteration's outcome is unchanged
  (the existing INFO log lines + PR-creation flow still
  fire normally per their respective contracts)
- **AND** the operator monitors progress via
  `journalctl -u autocoder` as with any other iteration

### Requirement: Detect openspec abort marker in stdout
The `autocoder sync-specs --rebuild` subcommand SHALL inspect every successful (`exit 0`) `openspec archive` invocation's stdout for an abort marker BEFORE running the post-condition check. The marker is any line whose first non-whitespace token is `Aborted.` (with the trailing period). When the marker is present, the rebuild SHALL treat the archive call as failed regardless of the exit code: rollback runs, the change is recorded as failed, and the failure_reason starts with `openspec refused to apply: <reason>` where `<reason>` is the most informative preceding line (typically openspec's diagnostic that immediately precedes the `Aborted.` line). The post-condition check remains in place as a defense-in-depth fallback for cases where openspec's wording changes or the marker is absent.

#### Scenario: Aborted marker on its own line triggers failure path
- **WHEN** `openspec archive <slug> -y` exits 0 AND its stdout contains a line `Aborted. No files were changed.`
- **THEN** the rebuild treats the call as failed
- **AND** `record_failure_with_rollback` is invoked with `original_name`
- **AND** the change directory is moved back to `openspec/changes/archive/<original_name>/`
- **AND** the `ChangeOutcome.failure_reason` starts with `openspec refused to apply:`

#### Scenario: Preceding line is captured as the headline reason
- **WHEN** openspec stdout contains the lines `member-saved-cards MODIFIED failed for header "..." - not found\nAborted. No files were changed.`
- **THEN** the `failure_reason` headline is `openspec refused to apply: member-saved-cards MODIFIED failed for header "..." - not found`
- **AND** the full openspec output (subject to the existing report-size cap) is included after the headline so the operator has the complete context

#### Scenario: Word "aborted" mid-sentence does not trigger detection
- **WHEN** openspec stdout contains the substring `aborted` (lowercase, mid-sentence) but no line whose first non-whitespace token is `Aborted.`
- **THEN** the abort-marker detection returns `None`
- **AND** the rebuild proceeds to the post-condition check as if no marker were present

#### Scenario: Post-condition check remains as fallback
- **WHEN** openspec silently skips a change without emitting the `Aborted.` marker (e.g. a future openspec version changes its wording)
- **THEN** the abort-marker detection returns `None` and the rebuild proceeds to `verify_archive_post_condition`
- **AND** the post-condition check catches the silent skip via the existing `ActivePathStillPresent` path
- **AND** rollback runs through the existing per-change atomicity contract

### Requirement: Rebuild PR body accurately describes rollback behavior
The rebuild's generated PR body SHALL describe failures as rolled back to archive rather than left at the active path, matching the actual behavior introduced by the atomicity contract. The rebuild summary line SHALL include the rolled-back count when greater than zero, so the operator can confirm at a glance that the rollback count matches the failure count. When the counts differ (data-loss-shaped failures, rollback-of-rollback failures), the gap is visible in the summary and explained per-change in the failures list.

#### Scenario: Failed-rebuild PR body header describes rollback
- **WHEN** the rebuild generates a PR body for a run with at least one failed change
- **THEN** the failures-section header reads `**Failed changes** (rolled back to archive — see failure reasons below for the openspec output explaining each):`
- **AND** the header does NOT contain the phrase `left at active path`

#### Scenario: Summary line includes rolled-back count when non-zero
- **WHEN** the rebuild processed N changes, S succeeded, F failed, R rolled back, with R > 0
- **THEN** the summary line reads `Replayed N archived change(s) chronologically; S succeeded, F failed (R rolled back to archive).`

#### Scenario: Summary line omits rolled-back parenthetical when zero
- **WHEN** the rebuild processed N changes with R == 0 (typically because F == 0 too)
- **THEN** the summary line reads `Replayed N archived change(s) chronologically; S succeeded, F failed.` (no parenthetical)

#### Scenario: Rollback gap is visible when R < F
- **WHEN** the rebuild had 5 failed changes but only 4 rollbacks completed (1 rollback-of-rollback failure, or 1 data-loss-shaped failure that doesn't trigger rollback)
- **THEN** the summary line reads `..., 5 failed (4 rolled back to archive).`
- **AND** the failure_reason for the 5th entry contains either `rollback ALSO failed:` (rollback-of-rollback case) or `openspec archive reported success but the change is missing from both the active path and the archive` (data-loss case)

### Requirement: Per-change atomicity in sync-specs rebuild
The `autocoder sync-specs --rebuild` subcommand SHALL treat each archived change as an atomic unit: either the change is successfully re-archived (`openspec archive` exited zero AND the post-condition holds), or the workspace is restored to its pre-change state via rollback. The active path `openspec/changes/<slug>/` SHALL NOT be left containing a directory the rebuild placed there if the change fails to archive. Failed changes SHALL be reported with the openspec output that explains the failure.

#### Scenario: Happy path leaves the change in archive with original date prefix
- **WHEN** `openspec archive <slug> -y` exits zero AND `openspec/changes/<slug>/` no longer exists AND exactly one directory matches `openspec/changes/archive/*-<slug>/` with a date prefix
- **THEN** the rebuild renames the matched archive directory to the change's original name (preserving its historical date prefix) when the names differ
- **AND** records a successful outcome for the change

#### Scenario: Silent skip rolls the workspace back
- **WHEN** `openspec archive <slug> -y` exits zero BUT `openspec/changes/<slug>/` still exists (openspec did not move the directory)
- **THEN** the rebuild moves `openspec/changes/<slug>/` back to `openspec/changes/archive/<original_name>/`
- **AND** records a failed outcome for the change whose `failure_reason` includes openspec's captured stdout AND stderr
- **AND** the operator's `openspec/changes/` directory contains no active-path entry for this slug after the rebuild

#### Scenario: Non-zero exit rolls the workspace back
- **WHEN** `openspec archive <slug> -y` exits non-zero
- **THEN** the rebuild moves `openspec/changes/<slug>/` back to `openspec/changes/archive/<original_name>/`
- **AND** records a failed outcome whose `failure_reason` includes the exit status AND openspec's captured stderr (or stdout when stderr is empty), each truncated to the existing report-size cap

#### Scenario: Data-loss-shaped failure is detected explicitly
- **WHEN** `openspec archive <slug> -y` exits zero AND `openspec/changes/<slug>/` no longer exists BUT NO directory matches `openspec/changes/archive/*-<slug>/`
- **THEN** the rebuild records a failed outcome whose `failure_reason` describes "openspec archive reported success but the change is missing from both the active path and the archive"
- **AND** does NOT attempt a rollback (there is nothing in the active path to roll back)

#### Scenario: Archive-directory collision is detected, not silently picked
- **WHEN** `openspec archive <slug> -y` exits zero AND `openspec/changes/<slug>/` no longer exists AND more than one directory matches `openspec/changes/archive/*-<slug>/`
- **THEN** the rebuild records a failed outcome whose `failure_reason` lists all matching paths and instructs the operator to manually consolidate them
- **AND** does NOT attempt to rename any of the matches (the rebuild cannot tell which one is canonical)

#### Scenario: Rollback failure does not crash the rebuild
- **WHEN** a rollback is required AND the rollback rename itself fails (e.g. destination already exists, filesystem permission)
- **THEN** the rebuild logs at CRITICAL with both the original failure and the rollback failure
- **AND** records a failed outcome whose `failure_reason` concatenates both messages
- **AND** continues processing the next archived change

### Requirement: openspec output is captured regardless of exit code
The rebuild SHALL capture `openspec`'s stdout and stderr for every invocation, not only when the exit code is non-zero. Captured output SHALL be included in the per-change failure report when the post-condition fails on an exit-zero call. This ensures the operator can see the upstream skip-reason without re-running the rebuild under tracing.

#### Scenario: Silent-skip failure reason contains openspec's actual output
- **WHEN** the rebuild reports a change as failed because of post-condition failure on an exit-zero openspec call
- **THEN** the `failure_reason` string contains a non-empty excerpt of openspec's stdout OR stderr
- **AND** the excerpt is bounded by the existing report-size cap so the summary stays readable

### Requirement: Success-path archive directory is observed, not guessed
The rebuild SHALL locate the resulting archive directory after a successful `openspec archive` call by matching `openspec/changes/archive/*-<slug>/` where the prefix matches the date pattern `^\d{4}-\d{2}-\d{2}-`, rather than by constructing a predicted name from today's date. This makes the success path robust to local-timezone differences between openspec and the rebuild, collision suffixes added by openspec, and any future change to openspec's archive-naming format.

#### Scenario: Glob match handles collision suffix
- **WHEN** openspec produces an archive directory named `archive/2026-05-25-<slug>-2/` (a collision suffix because `archive/2026-05-25-<slug>/` already existed from a prior run)
- **THEN** the glob match returns `archive/2026-05-25-<slug>-2/`
- **AND** the rebuild renames that path to the change's original name

#### Scenario: Glob match handles timezone-difference date
- **WHEN** the rebuild's UTC date is `2026-05-25` and openspec uses a different timezone whose date is `2026-05-26`
- **THEN** the glob match returns `archive/2026-05-26-<slug>/` (the actual path openspec created)
- **AND** the rebuild renames that path to the change's original name without relying on `today_dated_name`

#### Scenario: Glob match ignores entries without a date prefix
- **WHEN** an unrelated directory `archive/foo-<slug>/` exists (operator-placed sidecar, nested archive) AND `archive/2026-05-25-<slug>/` also exists
- **THEN** only the date-prefixed match is returned
- **AND** the unrelated directory is not renamed or touched

### Requirement: LLM-driven audits validate their generated proposals before committing
Every LLM-driven audit (currently `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`) SHALL invoke `openspec validate <slug> --strict` against its just-written `openspec/changes/<slug>/` directory before returning success. The `architecture_brightline` audit, which does not generate spec proposals via LLM, is unaffected by this requirement. When validation passes, the audit returns its existing outcome variant. When validation fails AND the configured retry budget is not exhausted, the audit SHALL re-invoke its LLM with the validation error appended to the prompt and overwrite the change directory with the new response. When validation fails AND the retry budget IS exhausted, the audit SHALL discard the change directory AND post a chatops failure notification AND return a `ValidationExhausted` outcome.

#### Scenario: Valid proposal on first attempt
- **WHEN** an LLM-driven audit writes a proposal and `openspec validate <slug> --strict` exits 0 on first invocation
- **THEN** the audit returns its existing success outcome with `retries_used == 0`
- **AND** no retry is attempted
- **AND** no chatops failure notification fires

#### Scenario: Validation passes after one retry
- **WHEN** an LLM-driven audit writes an invalid proposal on attempt 0 AND `audits.max_validation_retries` is 1 AND the LLM produces a valid proposal on attempt 1 (with the prior validation error appended to its prompt)
- **THEN** the audit returns its existing success outcome with `retries_used == 1`
- **AND** the chatops notification (when `notify_on_clean=true` for this audit) includes the clause `validated on retry 1 of 1`
- **AND** the change directory at `openspec/changes/<slug>/` contains the second (valid) proposal, not the first

#### Scenario: Retry budget exhausted
- **WHEN** an LLM-driven audit writes invalid proposals on both attempt 0 and attempt 1 with `audits.max_validation_retries == 1`
- **THEN** the audit returns `AuditOutcome::ValidationExhausted { audit_type, retries_attempted: 1, final_error }`
- **AND** the `openspec/changes/<slug>/` directory does NOT exist after the call
- **AND** no commit is made to git
- **AND** a chatops `❌` notification is posted to the repo's resolved channel containing the audit type, the retry count, and a truncated excerpt of the final validation error

#### Scenario: max_validation_retries = 0 disables retries
- **WHEN** an LLM-driven audit writes an invalid proposal on the first attempt AND `audits.max_validation_retries == 0`
- **THEN** the audit returns `ValidationExhausted { retries_attempted: 0, .. }` immediately
- **AND** no second LLM call is made
- **AND** the discard-and-notify path runs the same as the exhausted case above

#### Scenario: Validation retry passes validation error in addendum
- **WHEN** the retry path invokes the LLM on attempt N > 0
- **THEN** the LLM prompt contains an addendum naming the previous attempt's openspec validation error verbatim
- **AND** the LLM's response replaces the change directory entirely (delete-and-rewrite, not patch)

### Requirement: Retry budget is operator-configurable with sensible defaults and bounds
The `audits` configuration block SHALL accept an optional `max_validation_retries: u32` field that defaults to `1` when absent. Values above `5` SHALL be clamped to `5` at config-load with a WARN log naming both the requested and clamped values. Value `0` is explicitly permitted (disables retries; first validation failure produces ValidationExhausted immediately).

#### Scenario: Default value is 1
- **WHEN** a `config.yaml` has an `audits:` block without `max_validation_retries`
- **THEN** the resolved config has `max_validation_retries == 1`

#### Scenario: Value above 5 is clamped with a WARN
- **WHEN** a `config.yaml` specifies `audits.max_validation_retries: 10`
- **THEN** the resolved config has `max_validation_retries == 5`
- **AND** the daemon emits a WARN at startup naming both the requested value (`10`) and the clamped value (`5`)

#### Scenario: Value 0 is permitted
- **WHEN** a `config.yaml` specifies `audits.max_validation_retries: 0`
- **THEN** the resolved config has `max_validation_retries == 0`
- **AND** no WARN is emitted at startup

### Requirement: Audit-state history records every attempt outcome including validation-failure metadata
Each audit type's state file SHALL maintain an `attempt_history` list of at most 20 entries, each capturing the timestamp, outcome kind, retries used, and (for ValidationExhausted outcomes) a truncated excerpt of the validation error. The list is FIFO-bounded: when a new entry would push it past 20, the oldest entry is dropped.

#### Scenario: Successful audit appends a Reported entry
- **WHEN** an LLM-driven audit returns `Reported { retries_used }`
- **THEN** the audit's state file's `attempt_history` gains one entry with `outcome_kind: "Reported"` and the matching `retries_used` value
- **AND** the entry's `error_excerpt` is `None`

#### Scenario: ValidationExhausted appends an entry with the error excerpt
- **WHEN** an LLM-driven audit returns `ValidationExhausted { retries_attempted, final_error }`
- **THEN** the audit's state file's `attempt_history` gains one entry with `outcome_kind: "ValidationExhausted"`, the matching `retries_used`, AND an `error_excerpt` containing the first 200 characters of `final_error`

#### Scenario: History is bounded at 20 entries
- **WHEN** an audit has produced 25 sequential runs
- **THEN** the audit's state file's `attempt_history` contains exactly 20 entries
- **AND** the entries are the most recent 20 (the oldest 5 have been dropped)

#### Scenario: Backwards compatibility with state files lacking attempt_history
- **WHEN** an audit reads its state file from a prior version that did not include the `attempt_history` field
- **THEN** the deserialization succeeds with `attempt_history` defaulting to an empty list
- **AND** subsequent audit runs append entries normally

### Requirement: Validation-exhausted notification fires regardless of notify_on_clean
The `❌ <audit-type> produced an invalid proposal` chatops notification SHALL fire on every `ValidationExhausted` outcome regardless of the audit's `notify_on_clean` configuration. An audit producing invalid proposals is operator-actionable feedback that the audit's prompt template or LLM is producing low-quality output; suppressing the signal would hide a real failure mode.

#### Scenario: notify_on_clean=false does not suppress validation-exhausted
- **WHEN** an audit configured with `notify_on_clean: false` returns `ValidationExhausted`
- **THEN** the chatops `❌` notification is posted
- **AND** the `notify_on_clean=false` setting does not block the notification

#### Scenario: notify_on_clean=true success-with-retry includes retry-count clause
- **WHEN** an audit configured with `notify_on_clean: true` returns `Reported { retries_used: 1 }`
- **THEN** the chatops success notification text includes the clause `validated on retry 1 of <max>`
- **AND** `<max>` is the resolved `audits.max_validation_retries` for this audit

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
On a triggering comment for an open PR, the daemon SHALL re-invoke the executor in revision mode (passing the original change material, the current PR diff, AND the revision text). The executor's outcome drives the next step: `Completed` → commit + force-with-lease push + success reply comment; `AskUser` → existing chatops escalation (no commit, no count increment, no PR reply yet, revision treated as in-progress); `Failed` → failure reply comment + count increment.

For the `Completed` outcome, the success reply comment SHALL carry the success line followed (when the executor's `final_answer` is non-empty after trimming) by a blank line AND the agent's `final_answer` text verbatim. The success line stays at the top so operators scanning for the ✓ confirmation see it immediately. When `final_answer` is `None` OR is empty after trimming, the comment body is the single-line success form (today's behavior); the change is purely additive.

The combined body SHALL be passed through the existing GitHub-comment-size truncation helper (`truncate_to_fit` OR equivalent) before posting, with a truncation marker appended when the body exceeds the limit. The marker text names the per-change log file path so operators can recover the full summary from disk.

#### Scenario: Completed revision updates the PR with a substantive summary
- **GIVEN** the executor returns `Completed { final_answer: Some("Did X. Declined Y because Z.") }` for a revision context
- **WHEN** the revision dispatcher composes the success comment
- **THEN** the daemon commits the workspace changes with subject `revise: <change>: <first 60 chars of revision text>`
- **AND** force-pushes with `--force-with-lease` to `repositories[].agent_branch`
- **AND** posts a PR issue comment whose body starts with `✅ Revision applied:`
- **AND** the comment body contains the agent's summary text `Did X. Declined Y because Z.` on the line(s) following a blank line after the success line
- **AND** the PR's diff updates to reflect the revision

#### Scenario: Completed revision without a substantive summary uses the single-line form
- **GIVEN** the executor returns `Completed { final_answer: None }` OR `Completed { final_answer: Some("   ") }` (empty after trim) for a revision context
- **WHEN** the revision dispatcher composes the success comment
- **THEN** the daemon posts a PR issue comment whose body is the single-line `✅ Revision applied: <subject>. Revision count: <n> of <cap>.` (no trailing blank line, no empty summary section)

#### Scenario: AskUser during revision escalates without committing
- **GIVEN** the executor returns `AskUser { question, resume_handle }` during revision execution
- **WHEN** the revision dispatcher processes the outcome
- **THEN** the existing chatops escalation path fires (the question is posted to the configured channel)
- **AND** no commit is made on the agent branch
- **AND** no PR reply comment is posted
- **AND** the revision-count counter is NOT incremented
- **AND** the comment's `created_at` is NOT marked as processed (so the next iteration after the human answer can resume against the same trigger comment)

#### Scenario: Failed revision posts a failure comment
- **GIVEN** the executor returns `Failed { reason }` for a revision context
- **WHEN** the revision dispatcher processes the outcome
- **THEN** the daemon posts a PR issue comment whose body starts with `✗ Revision attempt failed:` AND includes the reason
- **AND** the revision-count counter IS incremented (a failed attempt counts toward the cap)
- **AND** no commit or push is made

#### Scenario: Oversize summary is truncated with a marker pointing at the log file
- **GIVEN** the executor returns `Completed { final_answer: Some(very_long_text) }` where the composed body exceeds the GitHub comment-size limit
- **WHEN** the revision dispatcher composes the success comment
- **THEN** the body is truncated at the largest char boundary fitting under the limit
- **AND** a truncation marker is appended naming the per-change log file path on disk
- **AND** the operator can recover the full summary from `<logs_dir>/runs/<workspace-basename>/<change>.log`

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

### Requirement: Audit posts a chatops notification when it creates a queue-bound proposal
Every LLM-driven audit (`architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`) SHALL post a chatops notification immediately after `openspec validate <slug> --strict` passes for its just-written proposal AND before the audit function returns to the scheduler. The notification names the audit type, the change slug, and a one-line excerpt of the proposal's `## Why` section, so operators have clear provenance when the next polling iteration begins implementing the change. The notification fires regardless of the audit's `notify_on_clean` setting, since it signals "something was found" rather than "nothing was found." The pure-data `architecture_brightline` audit, which does not generate LLM proposals, is unaffected.

#### Scenario: Validated proposal fires the notification on first attempt
- **WHEN** an LLM-driven audit's proposal passes `openspec validate <slug> --strict` on the first attempt (`retries_used == 0`)
- **THEN** the audit posts exactly one chatops notification whose text matches `🔍 <repo_url>: <audit_type> created proposal \`<change_slug>\` — <why_excerpt>`
- **AND** the notification text does NOT contain a parenthetical about retries

#### Scenario: Validated proposal after retry includes the retry-count parenthetical
- **WHEN** an LLM-driven audit's proposal passes validation after one or more retries (`retries_used > 0`)
- **THEN** the notification text appends ` (validated on retry <retries_used> of <max_validation_retries>)`

#### Scenario: ValidationExhausted does NOT fire the proposal-created notification
- **WHEN** an LLM-driven audit's proposal fails validation through every retry and the audit returns `ValidationExhausted`
- **THEN** the `🔍 created proposal` notification SHALL NOT fire
- **AND** the existing `❌ <audit-type> produced an invalid proposal` notification (from `a01-audit-proposal-self-validation`) fires instead

#### Scenario: notify_on_clean=false does not suppress this notification
- **WHEN** an LLM-driven audit configured with `notify_on_clean: false` produces a valid proposal
- **THEN** the `🔍 created proposal` notification still fires
- **AND** the existing `notify_on_clean=false` semantics still suppress only the empty-findings success message

#### Scenario: architecture_brightline produces no proposal-created notification
- **WHEN** the `architecture_brightline` audit runs to completion AND produces any number of findings
- **THEN** no `🔍 created proposal` notification fires from this audit
- **AND** the audit's existing notification behaviour (if any) is unchanged

#### Scenario: chatops backend absent does not affect audit outcome
- **WHEN** the daemon has no chatops backend configured AND an LLM-driven audit produces a valid proposal
- **THEN** the audit returns its `Reported` outcome normally
- **AND** the missing notification does NOT affect the proposal commit, the queue insertion, or the iteration's overall success

#### Scenario: chatops post_notification failure does not affect audit outcome
- **WHEN** the chatops backend is configured AND `post_notification` returns Err during the `🔍` notification post
- **THEN** the failure is logged at WARN
- **AND** the audit's `Reported` outcome is unaffected
- **AND** the proposal commit proceeds normally

### Requirement: `send it` verb in an audit thread schedules a triage executor run
The chatops listener SHALL recognize `@<bot> send it` (case-insensitive on `send it`) as the `SendItOnAudit` command ONLY when the message arrives with a non-empty `thread_ts` AND the `thread_ts` matches a tracked audit-thread state with `status: Open` OR `status: TriageFailed`. Same text outside a thread SHALL parse as the unknown-verb fallback (existing `?` reaction). When recognized, the dispatcher SHALL submit a `trigger_audit_action` control-socket action AND flip the audit-thread state's `status` to `TriagePending`. The next polling iteration drains the triage queue and runs the executor in triage mode.

#### Scenario: Send-it in tracked, open audit thread schedules triage
- **WHEN** an operator posts `@<bot> send it` as a thread reply where `thread_ts` matches an `AuditThreadState` with `status: Open` AND `posted_at` within the last 7 days
- **THEN** the dispatcher submits `trigger_audit_action` with the `thread_ts`
- **AND** the state file's `status` is updated to `TriagePending`
- **AND** the bot replies in the thread `✓ Triage scheduled for <audit_type> on <repo_url>. The next polling iteration will run it (~Nm).`

#### Scenario: Send-it in untracked thread is politely refused
- **WHEN** an operator posts `@<bot> send it` in a thread that has no corresponding `AuditThreadState`
- **THEN** the bot replies `✗ This reply is in a thread autocoder is not tracking. The \`send it\` verb only acts in audit-notification threads.`
- **AND** no control-socket action is submitted

#### Scenario: Send-it on stale audit thread is politely refused
- **WHEN** an operator posts `@<bot> send it` in a tracked thread whose `posted_at` is older than 7 days
- **THEN** the bot replies `✗ This audit's findings are too old to act on (>7d). Re-run the audit via @<bot> audit <type> <repo>.`
- **AND** the state file remains unchanged (the prune-stale-entries pass will eventually remove it)

#### Scenario: Send-it on already-acted thread is politely refused
- **WHEN** an operator posts `@<bot> send it` in a thread with `status: Acted` OR `status: TriagePending`
- **THEN** the bot replies `✗ This audit thread is already <status>. No new action taken.`
- **AND** no new triage is scheduled

#### Scenario: Send-it on TriageFailed thread re-attempts triage
- **WHEN** an operator posts `@<bot> send it` in a thread with `status: TriageFailed`
- **THEN** the dispatcher treats the request like the Open case (triage re-scheduled)
- **AND** the state's `status` is reset to `TriagePending` for the new attempt

### Requirement: Triage mode runs the executor with an explore-then-classify prompt
The polling iteration SHALL drain its per-repo triage queue (alongside the existing revision-request queue) at iteration start. For each queued triage, the iteration SHALL invoke `executor.run_triage(workspace, ctx)` with a `TriageContext` carrying the audit findings, audit type, repo URL, and a brief canonical-specs index. The triage-mode prompt template (`prompts/audit-triage.md`) SHALL instruct the LLM to first explore the codebase, then triage findings into quick-fix vs spec-worthy categories, apply quick fixes directly to the working tree, and create new `openspec/changes/<derived-slug>/` directories for spec-worthy findings. The slug derives from `<audit-type>-<short-hash-of-findings>` with collision-suffixing when needed.

#### Scenario: Triage mode invokes the executor with the documented context
- **WHEN** the polling iteration drains a queued triage
- **THEN** the executor is invoked via `run_triage` with `TriageContext { findings, audit_type, repo_url, canonical_specs_index }`
- **AND** the prompt sent to the wrapped CLI contains the four substituted variables AND the four-step instruction (explore → classify → fix → spec)

#### Scenario: Triage executor returning AskUser escalates without committing
- **WHEN** the triage executor returns `AskUser { question, resume_handle }`
- **THEN** the existing chatops escalation fires (the question posts to the configured channel)
- **AND** no commit is made on any branch
- **AND** no PR is opened
- **AND** the audit-thread state's `status` stays `TriagePending`

#### Scenario: Triage executor returning Failed flips state and posts a reply
- **WHEN** the triage executor returns `Failed { reason }`
- **THEN** the audit-thread state's `status` flips to `TriageFailed` with `reason` populated
- **AND** the bot posts a reply in the audit thread naming the failure
- **AND** no PRs are created

### Requirement: Completed triage splits into one or two PRs by content path
After the triage executor returns `Completed`, the daemon SHALL inspect the working tree's changed paths AND keep ONLY paths inside `openspec/changes/<derived-slug>/`. Each path outside that subtree (code fixes, doc edits, ANY non-spec content) SHALL be reverted to its committed (HEAD) state BEFORE the spec-PR commit, by a strategy chosen by where the path lives: a tracked path PRESENT in HEAD (a modification, deletion, type-change, OR the source side of a rename) is restored — BOTH the index AND the worktree — via `git checkout HEAD -- <path>`, so a code edit the executor staged with `git add` cannot survive; a tracked path ABSENT from HEAD (a brand-new file the executor created AND staged — porcelain `A ` — OR a rename destination) is unstaged via `git reset HEAD -- <path>` AND removed from disk; an untracked addition is removed from disk via `std::fs::remove_file` / `remove_dir_all`. The not-in-HEAD case SHALL NOT be reverted with `git checkout HEAD` / `git restore --source=HEAD`, which abort with a "pathspec did not match any file(s) known to git" error for a path absent from HEAD on some git versions — exactly the common case where the executor `git add`ed a new code file. If any non-spec write cannot be reverted or removed, the daemon SHALL abort before the spec-PR commit rather than allow the write to leak into the spec PR. At most ONE PR is created per triage run — the spec PR. The fixes-PR path is removed entirely; code fixes flow through the standard implementer pipeline on a subsequent polling iteration after the operator merges the spec PR.

When the discard step drops non-empty paths (the agent wrote code despite the prompt's restriction), the daemon SHALL emit a WARN log naming the dropped paths AND post a chatops reply in the audit-thread naming the dropped paths AND directing the operator to capture the dropped fixes as `tasks.md` items in the spec if they were load-bearing.

When the discard step leaves NO spec content in `openspec/changes/<derived-slug>/` (the agent wrote only code AND no spec), NO PR is created AND the daemon posts a chatops reply in the audit-thread naming `no spec content produced; retry with a clearer directive`. The audit-thread's `status` flips to `TriageFailed`.

When the discard step leaves spec content, the daemon SHALL create the spec branch off the same base, commit the spec paths with subject `audit-triage spec proposal from <audit_type>`, push the branch, AND open the spec PR via the existing PR-creation helpers. PR-body text describes the spec content AND does NOT cross-link to any fixes PR (there is no fixes PR).

#### Scenario: Mixed diff produces one spec PR; code paths are discarded with chatops warning
- **GIVEN** the triage executor's Completed working tree contains BOTH new files in `openspec/changes/audit-fix-x/` AND modifications to `src/foo.rs`
- **WHEN** the audit-triage completion handler runs
- **THEN** `src/foo.rs` is reverted to its base-branch (HEAD) state — BOTH the index AND the worktree — BEFORE the commit (via `git checkout HEAD -- src/foo.rs`, since it exists in HEAD; a not-in-HEAD addition would instead be unstaged via `git reset HEAD --` AND removed from disk), so a code edit the executor staged with `git add` cannot survive into the spec commit
- **AND** the working tree's `src/foo.rs` reverts to the base-branch state
- **AND** a WARN log fires naming the audit type, the derived slug, AND `src/foo.rs` as the dropped path
- **AND** the daemon creates a spec branch + PR with ONLY `openspec/changes/audit-fix-x/` paths
- **AND** the PR body does NOT mention a companion fixes PR
- **AND** the daemon posts a chatops reply in the audit-thread naming `src/foo.rs` as dropped AND explaining `Per a43, code fixes go through the standard implementer pipeline. The spec PR has been opened; if the dropped fixes were load-bearing, revise the spec to capture them as tasks.md items.`
- **AND** the audit-thread's `status` flips to `Acted`

#### Scenario: A staged brand-new code file is discarded without a pathspec error
- **GIVEN** the triage executor's Completed working tree contains new files in `openspec/changes/audit-fix-x/` AND a brand-new file `src/new.rs` the executor created AND staged with `git add` (porcelain `A `, absent from HEAD)
- **WHEN** the audit-triage completion handler runs
- **THEN** `src/new.rs` is unstaged via `git reset HEAD -- src/new.rs` AND removed from disk, NOT reverted with `git checkout HEAD` / `git restore --source=HEAD` (which would abort with a pathspec error for a path absent from HEAD)
- **AND** the discard step does NOT error AND the triage flow proceeds to open the spec PR
- **AND** `src/new.rs` is named among the dropped paths in both the WARN log AND the chatops reply
- **AND** the spec PR's diff contains ONLY `openspec/changes/audit-fix-x/` paths

#### Scenario: Spec-only triage produces one spec PR with no warning
- **GIVEN** the triage executor's Completed working tree contains ONLY new files in `openspec/changes/audit-fix-x/`
- **WHEN** the audit-triage completion handler runs
- **THEN** the discard step finds no paths to drop AND emits NO WARN log
- **AND** the spec branch + PR is created with the spec content
- **AND** NO chatops warning is posted (the agent followed the restriction)
- **AND** the audit-thread's `status` flips to `Acted`

#### Scenario: Code-only triage produces NO PR; chatops reply explains no spec content
- **GIVEN** the triage executor's Completed working tree contains ONLY modifications to `src/foo.rs` (no `openspec/changes/<derived-slug>/` content)
- **WHEN** the audit-triage completion handler runs
- **THEN** the discard step restores `src/foo.rs` to the base-branch state
- **AND** no spec branch is created AND no PR is opened
- **AND** the daemon posts a chatops reply in the audit-thread naming `no spec content produced; retry with a clearer directive`
- **AND** the audit-thread's `status` flips to `TriageFailed`

#### Scenario: Empty-diff triage posts a no-action reply
- **GIVEN** the triage executor returns `Completed` but the working tree's diff is empty (the LLM decided nothing was actionable)
- **WHEN** the audit-triage completion handler runs
- **THEN** no PRs are created
- **AND** the bot posts a reply in the audit thread containing the LLM's final-summary text explaining the decision
- **AND** the audit-thread's `status` flips to `Acted`

#### Scenario: Slug collision is suffixed
- **GIVEN** the derived slug `<audit-type>-<hash>` already exists as `openspec/changes/<slug>/`
- **WHEN** the audit-triage completion handler builds the spec dir
- **THEN** the daemon increments a suffix (`-2`, `-3`, ...) until it finds a free path
- **AND** the resulting spec directory uses the suffixed slug

### Requirement: Triage-created PRs participate in the existing PR-comment-revision-loop
PRs spawned by audit-reply triage SHALL be structurally identical to polling-loop-spawned PRs from the revision-loop dispatcher's perspective. Operators replying `@<bot> revise <text>` on either the fixes PR OR the spec PR get revisions through the standard channel from `a01-pr-comment-revision-loop`; the dispatcher does not need to distinguish triage-PRs from regular PRs.

#### Scenario: Revision comment on a triage PR is processed normally
- **WHEN** a triage-spawned PR has an operator comment `@<bot> revise <text>`
- **THEN** the existing revision-loop dispatcher (per `a01-pr-comment-revision-loop`) picks up the comment
- **AND** the revision executes against that PR's branch normally
- **AND** the audit-thread state file is not consulted (the revision is its own scope, separate from the audit-thread tracking)

### Requirement: Audit-thread state files are pruned after 7 days
The daemon SHALL prune audit-thread state files whose `posted_at` is older than 7 days. The prune runs periodically (at iteration start, or once per day per the existing housekeeping pattern). Stale entries are removed regardless of `status` — even `Acted` entries are pruned eventually so the audit-threads directory stays bounded.

#### Scenario: Stale entry is removed
- **WHEN** the prune runs AND an `AuditThreadState` has `posted_at` more than 7 days in the past
- **THEN** the state file is removed
- **AND** subsequent `send it` replies in that thread fall through to the untracked-thread polite-refusal

#### Scenario: Fresh entry is preserved
- **WHEN** the prune runs AND an `AuditThreadState` has `posted_at` within the last 7 days
- **THEN** the state file is NOT removed regardless of status

### Requirement: Audits do not run against an invalid workspace
Every audit (LLM-driven and pure-data) SHALL verify the workspace is valid before performing any file IO or LLM-call setup. "Valid" means the workspace directory exists AND it contains a `.git/` subdirectory. When the check fails, the audit SHALL return `Ok(AuditOutcome::WorkspaceUnavailable { audit_type, workspace_path, reason })` immediately AND SHALL log a single INFO line naming the audit, the workspace path, and the reason. No file IO, no LLM call, no state mutation, and crucially no `fs::create_dir_all` (which would create the workspace's parent directories without a clone, producing exactly the broken state the gate exists to prevent).

#### Scenario: Audit skipped when workspace directory does not exist
- **WHEN** an audit is invoked AND the workspace directory does not exist on disk
- **THEN** the audit returns `Ok(AuditOutcome::WorkspaceUnavailable { reason: "workspace directory does not exist", .. })`
- **AND** no `fs::create_dir_all` was called against the workspace path
- **AND** the workspace path still does not exist after the call returns
- **AND** an INFO log fires naming the audit, the workspace, and the reason

#### Scenario: Audit skipped when workspace exists but has no .git/
- **WHEN** an audit is invoked AND the workspace directory exists AND it contains no `.git/` subdirectory
- **THEN** the audit returns `Ok(AuditOutcome::WorkspaceUnavailable { reason: "workspace exists but has no .git/ subdirectory", .. })`
- **AND** no new files or subdirectories were created in the workspace as a side effect of the audit call
- **AND** an INFO log fires naming the audit, the workspace, and the reason

#### Scenario: Audit proceeds normally against a valid workspace
- **WHEN** an audit is invoked AND the workspace exists AND it contains a `.git/` subdirectory
- **THEN** the workspace-validity gate passes
- **AND** the audit proceeds to its normal logic (LLM call, file IO, etc.)
- **AND** no `WorkspaceUnavailable` outcome is returned

### Requirement: Polling iteration gates audit-scheduler invocation on workspace-init success
The polling iteration SHALL invoke the audit scheduler only when its `ensure_initialized` call returned Ok. When `ensure_initialized` returns Err, the iteration SHALL skip the audit scheduler entirely AND proceed to its own existing failure path. The iteration-level gate is belt-and-braces with the per-audit gate: per-audit catches mid-iteration corruption; iteration-level catches the case where the workspace was already broken at iteration start.

#### Scenario: ensure_initialized failure skips the audit scheduler
- **WHEN** a polling iteration calls `ensure_initialized` AND it returns Err
- **THEN** the audit scheduler is NOT invoked in that iteration
- **AND** the iteration logs its failure as today (the workspace-init alert path) without any audit-related log lines for that iteration

#### Scenario: ensure_initialized success invokes the audit scheduler normally
- **WHEN** a polling iteration calls `ensure_initialized` AND it returns Ok
- **THEN** the audit scheduler is invoked as today
- **AND** each scheduled audit's per-audit gate runs (and almost always passes — `ensure_initialized` Ok means the workspace is valid)

### Requirement: Skipped audits do not consume cadence or trigger chatops notifications
A `WorkspaceUnavailable` outcome SHALL NOT update the audit's cadence-state file. The next iteration's cadence check re-evaluates and may attempt the audit again if the workspace has become valid (e.g. via `workspace-self-heal-partial-clone`'s auto-recovery or an operator's manual fix). Additionally, no chatops notification SHALL fire for a skipped audit — the iteration's own workspace-init alert is the operator-facing signal of the upstream problem; per-audit skip notifications would just flood the channel.

#### Scenario: Skipped audit's cadence state is unchanged
- **WHEN** an audit returns `WorkspaceUnavailable` AND its cadence-state file at `<state_dir>/audit-state/<audit-type>.json` previously recorded `last_run: <30 days ago>`
- **THEN** after the audit returns, the cadence-state file's `last_run` is still `<30 days ago>` (unchanged)
- **AND** the next polling iteration's cadence check sees the unchanged timestamp AND treats the audit as still-due

#### Scenario: No chatops notification on workspace-unavailable skip
- **WHEN** an audit returns `WorkspaceUnavailable` AND the chatops backend is configured AND the audit's `notify_on_clean` is `true`
- **THEN** no chatops `post_notification` call fires for the skipped audit
- **AND** the operator's signal of the underlying issue remains the iteration-level `workspace_init_failure` alert (which fires independently per existing behaviour)

#### Scenario: Multiple audits skipped in the same iteration produce no notification flood
- **WHEN** an iteration runs against an invalid workspace AND every scheduled audit returns `WorkspaceUnavailable`
- **THEN** zero chatops notifications fire for those skips
- **AND** the daemon logs one INFO line per skipped audit (operator can `journalctl` to see exactly which audits were skipped)

### Requirement: Chatops `audit` verb queues an on-demand audit run for the next polling iteration
The chatops listener SHALL recognize `@<bot> audit <audit-substring> <repo-substring>` as the `AuditNow` command. The audit-substring SHALL be matched case-insensitively against the registered audit-type names by substring (same rule the repo-substring uses against configured repository URLs). The repo-substring SHALL be matched per the existing repo-substring rules. On a unique match in both, the dispatcher SHALL submit a `queue_audit` control-socket action AND post a one-line ack naming the resolved audit-type and repo URL. On ambiguous or no-match, the dispatcher SHALL reply with the candidate list (mirroring the existing `match_repo` reply shapes).

#### Scenario: Unique substring matches queue the audit
- **WHEN** an operator posts `@<bot> audit sec myrepo` AND `sec` uniquely matches `security_bug_audit` AND `myrepo` uniquely matches a configured repo URL
- **THEN** the dispatcher submits a `queue_audit` action with both resolved names
- **AND** the bot posts a threaded reply whose first line is `✓ Queued security_bug_audit for <repo_url>. Will run on the next polling iteration (~Nm).` (where `~Nm` is the per-repo poll interval rounded to minutes, OR `imminently` when the next iteration is <30 seconds away)

#### Scenario: Ambiguous audit substring lists candidates
- **WHEN** an operator posts `@<bot> audit arch myrepo` AND `arch` matches both `architecture_brightline` and `architecture_consultative`
- **THEN** the bot replies `✗ audit substring \`arch\` matches multiple: architecture_brightline, architecture_consultative. Be more specific.`
- **AND** no audit is queued

#### Scenario: Unknown audit substring lists all registered names
- **WHEN** an operator posts `@<bot> audit gibberish myrepo`
- **THEN** the bot replies `✗ no audit matched \`gibberish\`; registered: architecture_brightline, architecture_consultative, drift_audit, missing_tests_audit, security_bug_audit.`
- **AND** no audit is queued

### Requirement: Queued audit runs bypass cadence on the next iteration
The audit scheduler SHALL, at the start of each iteration's audit-scheduling phase, drain the `pending_audit_runs` queue for the repo AND run each queued audit-type unconditionally (regardless of cadence or `last_run` timestamp). After running, the audit's `last_run` timestamp SHALL be updated as if it were a cadence-driven run. Cadence-driven scheduling continues to fire for audit types NOT already run via the queue in this iteration.

#### Scenario: Queued audit runs even when cadence says not due
- **WHEN** a repo's `pending_audit_runs` contains `security_bug_audit` AND `security_bug_audit`'s cadence says "not due for 28 more days"
- **THEN** the audit runs in this iteration
- **AND** its `last_run` timestamp is updated to the current time
- **AND** the cadence-based "next scheduled fire" effectively moves forward by the cadence interval from the new `last_run` (no double-run within the cadence window)

#### Scenario: De-duplicated queue entries produce one run
- **WHEN** the same audit-type appears in `pending_audit_runs` more than once for a single iteration
- **THEN** the audit runs exactly once in that iteration
- **AND** subsequent appearances of the same audit-type in the queue are no-ops

#### Scenario: Queue is drained after the iteration
- **WHEN** an iteration runs queued audits AND completes
- **THEN** the repo's `pending_audit_runs` is empty
- **AND** a subsequent iteration without new queue entries does NOT re-run those audits (cadence resumes)

#### Scenario: Cadence-driven audits coexist with queued audits in the same iteration
- **WHEN** an iteration has queued `security_bug_audit` AND cadence-due `drift_audit`
- **THEN** both audits run in the iteration
- **AND** the queue-drained audits run first, then the cadence-due audits

### Requirement: CLI `audit run` subcommand triggers on-demand from the command line
The `autocoder` CLI SHALL expose `audit run --workspace <path> --audit <name>` as a subcommand. The subcommand SHALL probe for the control socket at the resolved runtime path; when the socket is reachable, the subcommand sends the same `queue_audit` action a chatops `audit` verb would submit. When the socket is NOT reachable, the subcommand runs the audit standalone against the named workspace path AND prints the audit's findings to stdout.

#### Scenario: CLI talks to the running daemon when the socket is present
- **WHEN** the autocoder daemon is running on the host AND `autocoder audit run --workspace <path> --audit security_bug_audit` is invoked AND the workspace matches a repo the daemon is managing
- **THEN** the CLI connects to the control socket
- **AND** submits `queue_audit` with the resolved audit-type and repo URL
- **AND** prints the daemon's ack response to stdout
- **AND** exits 0

#### Scenario: CLI runs standalone when no daemon is present
- **WHEN** no autocoder daemon is running on the host AND `autocoder audit run --workspace <path> --audit security_bug_audit` is invoked
- **THEN** the CLI invokes the audit module directly against the workspace path
- **AND** prints the audit's findings to stdout
- **AND** exits 0 on successful audit, non-zero on audit failure

#### Scenario: CLI errors when daemon is running but workspace is not managed
- **WHEN** the daemon is running AND the named workspace is NOT in the daemon's configured repo list
- **THEN** the CLI prints a clear error naming the workspace path and the daemon's known repos
- **AND** exits non-zero
- **AND** does NOT fall back to standalone mode (the daemon is the owner of the workspace lifecycle when present; falling back would race the daemon)

### Requirement: PR-body proposal lookup falls back to the active path
The polling iteration's PR-body assembly SHALL look up each change's `proposal.md` in two steps: first under `openspec/changes/archive/*-<change>/proposal.md` (the established archived-change location), and on miss, second under `openspec/changes/<change>/proposal.md` (the active-path location). When the active-path fallback finds a proposal with a parseable `## Why` section, the lookup SHALL succeed AND the daemon SHALL emit a WARN log naming the change so operators can correlate the PR with the upstream archive-failure that left the change unarchived. When both paths miss OR neither yields a parseable `## Why`, the existing `_(no proposal.md available)_` PR-body fallback continues to render.

#### Scenario: Archive path wins when present
- **WHEN** a change's `proposal.md` exists at `openspec/changes/archive/<date>-<change>/proposal.md` with a parseable `## Why` section
- **THEN** the PR-body assembly returns the archive-path `## Why` content
- **AND** no active-path fallback is attempted
- **AND** no WARN log is emitted (the archived case is the happy path)

#### Scenario: Active path is consulted when archive is empty
- **WHEN** no `openspec/changes/archive/*-<change>/proposal.md` exists AND `openspec/changes/<change>/proposal.md` exists with a parseable `## Why` section
- **THEN** the PR-body assembly returns the active-path `## Why` content
- **AND** the daemon emits a single WARN log naming the change with text indicating the proposal was read from the active path

#### Scenario: Both paths missing
- **WHEN** neither the archive-path nor the active-path proposal file exists
- **THEN** the PR-body assembly returns no content for that change
- **AND** no WARN log is emitted (the operator already sees `_(no proposal.md available)_` in the PR body; a journal WARN for genuinely-missing files would be noise)

#### Scenario: Active path exists but lacks a `## Why` section
- **WHEN** no archive-path proposal exists AND `openspec/changes/<change>/proposal.md` exists but does NOT contain a `## Why` heading
- **THEN** the PR-body assembly returns no content for that change
- **AND** no WARN log is emitted (the fallback found a file but extracted no content, identical to the archive-path-with-malformed-proposal case)

#### Scenario: Archive present, active also present
- **WHEN** both `openspec/changes/archive/<date>-<change>/proposal.md` AND `openspec/changes/<change>/proposal.md` exist
- **THEN** the archive-path `## Why` content is returned (deterministic preference)
- **AND** no WARN log is emitted

### Requirement: Shared archive-with-postcondition helper covers every in-iteration openspec archive call
Every call site that runs `openspec archive <slug> -y` from inside the daemon SHALL go through a shared `openspec_archive_with_postcondition` helper that inspects stdout for the `Aborted.` marker AND verifies the post-condition (`openspec/changes/<slug>/` is gone AND exactly one `openspec/changes/archive/*-<slug>/` directory exists) before reporting success. The helper SHALL return a structured `ArchiveFailure` value naming the specific failure mode; each caller maps that to a domain-appropriate error type whose message includes the openspec output excerpt explaining the cause.

#### Scenario: Self-heal silent-skip surfaces the openspec cause
- **WHEN** an iteration enters self-heal AND `openspec archive <slug> -y` exits 0 AND its stdout contains a line beginning with `Aborted.`
- **THEN** `queue::archive` returns `Err` whose message contains `aborted by openspec:` and the preceding diagnostic line from openspec's stdout
- **AND** the self-heal call site's failure_reason is `self-heal archive failed: openspec archive `<slug>` aborted by openspec: <reason>; full output: <excerpt>`
- **AND** the change is NOT marked archived
- **AND** git commit is NOT attempted (the failure short-circuits before staging or commit)

#### Scenario: Rebuild path uses the same helper
- **WHEN** the rebuild loop processes any archived change and invokes the archive helper
- **THEN** the helper's `Err(AbortedMarker { .. })` triggers the existing rebuild rollback contract from `sync-specs-rebuild-atomicity` AND the existing failure-reason format from `sync-specs-detect-aborted-output`
- **AND** the rebuild behaviour is observationally identical to the pre-consolidation behaviour

#### Scenario: Active-path-still-present detection without marker
- **WHEN** `openspec archive <slug> -y` exits 0 AND stdout does NOT contain the `Aborted.` marker AND `openspec/changes/<slug>/` still exists
- **THEN** the helper returns `Err(ArchiveFailure::ActivePathStillPresent { path, full_output })`
- **AND** the caller's failure message reads `openspec archive `<slug>` reported success but the change directory at <path> still exists`

#### Scenario: Data-loss-shaped detection
- **WHEN** `openspec archive <slug> -y` exits 0 AND stdout has no marker AND `openspec/changes/<slug>/` is gone AND no `openspec/changes/archive/*-<slug>/` matches
- **THEN** the helper returns `Err(ArchiveFailure::NoArchiveEntryFound { full_output })`
- **AND** the caller's failure message names the data-loss condition explicitly

### Requirement: `run_git` surfaces stdout when stderr is empty or as supplementary context
The `run_git` helper SHALL include the failed command's stdout in the returned error message when stderr is empty, AND SHALL include both streams labelled `stderr:` / `stdout:` when both are non-empty. When both streams are empty (rare; failures with no diagnostic output), the error SHALL name the exit code in parentheses so the operator at least knows the command failed without producing output.

#### Scenario: `git commit` "nothing to commit" surfaces meaningfully
- **WHEN** `run_git` runs `git commit -m <subject>` against a workspace where `git status --porcelain` is empty, AND git exits non-zero with stdout `nothing to commit, working tree clean` and empty stderr
- **THEN** the returned `Err` contains the text `nothing to commit, working tree clean`
- **AND** the error message format is `git commit failed: nothing to commit, working tree clean`
- **AND** the error message does NOT end in a bare colon-space

#### Scenario: Both streams populated
- **WHEN** `run_git` runs a command that fails with non-empty stderr AND non-empty stdout
- **THEN** the returned `Err` contains both excerpts prefixed `stderr:` and `stdout:`

#### Scenario: Neither stream populated
- **WHEN** `run_git` runs a command that fails with both streams empty
- **THEN** the returned `Err` contains a parenthetical naming the exit code (e.g. `git commit failed: (no output; exit Some(1))`)
- **AND** the error does NOT end in a bare colon-space

### Requirement: Install wizard creates secrets file atomically with restrictive mode

The `autocoder install` subcommand SHALL create the `secrets.env` file
with mode `0o600` in the same syscall that creates the file. The
secrets file SHALL NEVER exist on disk with a mode wider than `0o600`,
even transiently between creation and a subsequent `chmod`. The
implementation MAY use `OpenOptions::mode(0o600).create_new(true)`
(or equivalent), `OpenOptions::mode(0o600).truncate(true)` over an
existing file, or any other mechanism that atomically associates the
creation event with mode `0o600`.

The `config.yaml` file SHALL be created with its target mode in the
same syscall — `0o600` in dev mode, `0o640` in server mode — using
the same approach. The post-write `chmod` calls MAY remain as
defense-in-depth but MUST NOT be the sole mechanism gating
permissions.

#### Scenario: Fresh install creates secrets.env with mode 0600 atomically

- **WHEN** `autocoder install` runs against a host with no existing
  `secrets.env` AND the wizard collects at least one secret (a
  GitHub PAT, a ChatOps bot token, or a reviewer API key)
- **THEN** the resulting file at `<config_dir>/secrets.env` has mode
  exactly `0o600` (owner read+write, no group, no other) as observed
  by `stat`
- **AND** at no point during the install does any process other than
  the install process and the eventual owner have permission to read
  the file's bytes

#### Scenario: Re-install over existing wider-perm secrets.env tightens before write

- **WHEN** `autocoder install --upgrade` runs against a host whose
  existing `secrets.env` has mode `0o644` (perhaps from a prior
  install that pre-dated this requirement) AND the wizard collects
  new secrets
- **THEN** the install path tightens the existing file to `0o600`
  BEFORE writing any new secret bytes into it (e.g. via
  `chmod`-then-truncate-then-write, or by removing the old file
  first and creating a new one with `OpenOptions::mode(0o600)`)
- **AND** the resulting file has mode `0o600` after the install
  completes

### Requirement: Daemon resolves four standard data-category paths with a defined precedence
The daemon SHALL resolve four data-category paths at startup: `state` (persistent state — audit cadence, failure counters, alert throttles, revisions), `cache` (re-creatable but kept — repo workspaces), `logs` (per-change run logs), and `runtime` (control socket, transient locks). Each path is resolved by this precedence: (1) an explicit `paths.<field>` value in `config.yaml`, (2) the per-field environment variable `AUTOCODER_STATE_DIR` / `AUTOCODER_CACHE_DIR` / `AUTOCODER_LOGS_DIR` / `AUTOCODER_RUNTIME_DIR`, (3) the systemd-set environment variable `$STATE_DIRECTORY` / `$CACHE_DIRECTORY` / `$LOGS_DIRECTORY` / `$RUNTIME_DIRECTORY`, (4) XDG-derived defaults (dev mode), (5) a hard fallback to `/var/lib/autocoder` and siblings. All four paths SHALL be absolute. No two paths may resolve to the same directory.

#### Scenario: Config explicit value wins over all env vars
- **WHEN** `config.yaml` sets `paths.state_dir: /custom/state` AND `AUTOCODER_STATE_DIR=/env/state` is set AND `$STATE_DIRECTORY=/var/lib/autocoder` is set
- **THEN** the resolved state path is `/custom/state`

#### Scenario: Env var wins over systemd-set var
- **WHEN** no config override AND `AUTOCODER_STATE_DIR=/env/state` AND `$STATE_DIRECTORY=/var/lib/autocoder`
- **THEN** the resolved state path is `/env/state`

#### Scenario: systemd-set var used when no config or env override
- **WHEN** no config override AND no env var AND `$STATE_DIRECTORY=/var/lib/autocoder`
- **THEN** the resolved state path is `/var/lib/autocoder`

#### Scenario: XDG defaults used in dev mode
- **WHEN** no config override AND no env var AND no systemd-set var AND `$HOME=/home/dev`
- **THEN** the resolved state path is `/home/dev/.local/state/autocoder` (or `$XDG_STATE_HOME/autocoder` when set)

#### Scenario: Relative-path config is rejected at startup
- **WHEN** `config.yaml` sets `paths.state_dir: relative/path`
- **THEN** the daemon fails to start with a clear error naming the field and requiring an absolute path

#### Scenario: Same path for two roles is rejected
- **WHEN** the resolution yields the same directory for two of the four roles
- **THEN** the daemon fails to start with an error naming both roles and the conflicting path

### Requirement: Workspaces, markers, and state move to standard locations; runtime remains ephemeral
Repo workspaces SHALL live under `<cache_dir>/workspaces/<sanitized-url>/` and SHALL include their in-tree marker files (`.perma-stuck.json`, `.needs-spec-revision.json`, `.question.json`, `.answer.json`, `.alert-state.json`, `.in-progress*`) as today. Per-audit-type cadence state SHALL live under `<state_dir>/audit-state/<audit-type>.json`. Per-change failure counters SHALL live under `<state_dir>/failure-state/<repo-sanitized>/<change-slug>.json`. Per-PR revision state SHALL live under `<state_dir>/revisions/<repo-sanitized>/<pr-number>.json`. Per-change run logs SHALL live under `<logs_dir>/runs/<repo-sanitized>/<change-slug>.log`. The control socket SHALL live at `<runtime_dir>/control.sock`. In-progress lock files SHALL live under `<runtime_dir>` so reboot clears them automatically.

#### Scenario: Workspace and its markers survive reboot under cache_dir
- **WHEN** the cache_dir resolves to `/var/cache/autocoder` (on real disk, not tmpfs) AND the workspace for repo X has `.perma-stuck.json` set for change Y AND the host reboots
- **THEN** after reboot the workspace at `/var/cache/autocoder/workspaces/<sanitized-X>/openspec/changes/Y/.perma-stuck.json` is still present
- **AND** the next polling iteration treats change Y as perma-stuck (no retry)

#### Scenario: Audit-state survives reboot under state_dir
- **WHEN** an audit ran 1 hour ago AND its state file at `<state_dir>/audit-state/<audit-type>.json` records that timestamp AND the host reboots
- **THEN** after reboot the daemon reads the state file at startup AND treats the audit's last-run as 1 hour ago
- **AND** the audit does NOT fire on the first polling iteration (its cadence has not elapsed)

#### Scenario: Control socket is recreated after reboot under runtime_dir
- **WHEN** the daemon starts AND the runtime_dir resolves to `/run/autocoder/` (tmpfs, cleared on reboot)
- **THEN** the daemon creates the control socket at `/run/autocoder/control.sock` regardless of whether it existed before
- **AND** the `autocoder reload` CLI's connection lookup uses the same resolved path

### Requirement: Audit-state is reloaded from disk on every daemon startup
The daemon SHALL scan `<state_dir>/audit-state/` on startup AND populate its in-memory audit cadence map from every parseable `<audit-type>.json` file found. Parse failures on individual files SHALL log a WARN naming the file and the parse error, and that audit treats as "never run" (the existing first-run fallback); other audits' state continues to load normally. Daemon restart without reboot SHALL NOT cause any audit to re-fire if its on-disk cadence timestamp shows the cadence has not elapsed.

#### Scenario: Audit-state reload populates the in-memory map
- **WHEN** the daemon starts AND `<state_dir>/audit-state/` contains valid state files for three audit types
- **THEN** the in-memory audit cadence map contains entries for all three audit types with their on-disk last-run timestamps

#### Scenario: One corrupt state file does not block other audits
- **WHEN** the audit-state dir has one parse-failing file AND two valid files
- **THEN** the in-memory map has the two valid entries
- **AND** a WARN is logged naming the corrupt file
- **AND** the corresponding audit treats as "never run"

#### Scenario: Daemon restart respects on-disk timestamps
- **WHEN** an audit's on-disk state shows `last_run: <30 minutes ago>` AND its cadence is `every-2-hours` AND the daemon restarts
- **THEN** the audit does NOT fire on the first polling iteration after restart
- **AND** the audit fires only after the cadence interval has elapsed from the on-disk timestamp

### Requirement: Legacy `/tmp` paths are auto-migrated on first startup
On daemon startup, if the file `<state_dir>/.migration-from-tmp-done` does NOT exist, the daemon SHALL scan well-known legacy `/tmp` paths and move their contents to the new locations. The migration is idempotent (a partially-completed migration resumes on the next startup), per-entry error-tolerant (one failing entry does not abort the rest), and writes the marker file only when every entry completed without error. Cross-partition moves (tmpfs → disk is the common case) fall back to recursive copy + delete-on-success when `fs::rename` fails with EXDEV. The daemon does NOT refuse to start if migration fails; partial migration is logged and operators can resolve orphan /tmp entries manually.

#### Scenario: First startup migrates legacy state
- **WHEN** the daemon starts AND no `.migration-from-tmp-done` marker exists AND legacy paths under /tmp contain state files / workspaces
- **THEN** each legacy entry is moved to its corresponding new location under state_dir / cache_dir / logs_dir
- **AND** the migration log line names the per-entry source and target paths

#### Scenario: Second startup skips migration
- **WHEN** the daemon starts AND `.migration-from-tmp-done` already exists
- **THEN** no legacy-path scan is performed
- **AND** no migration work is done

#### Scenario: Partial migration retries on next startup
- **WHEN** the daemon starts AND migration runs AND one entry fails (e.g. permission error) while others succeed
- **THEN** the marker file is NOT written
- **AND** the successful moves persist
- **AND** the next daemon startup re-scans, sees the migration is not complete, retries (entries already moved are skipped via the target-exists check; only the previously-failed entries are retried)

#### Scenario: Cross-partition move uses copy-and-delete fallback
- **WHEN** the source is on tmpfs AND the target is on a different partition AND `fs::rename` returns EXDEV
- **THEN** the migration falls back to recursive copy + delete-on-success
- **AND** the result is functionally identical to `fs::rename` (target populated, source removed)

#### Scenario: Target already exists is skipped
- **WHEN** a legacy source entry exists AND its corresponding target already exists
- **THEN** the entry is skipped (the target is treated as canonical)
- **AND** no overwrite is attempted
- **AND** the legacy source is left in place for operator inspection (the migration does not delete sources whose targets already exist)

### Requirement: systemd unit declares the four standard directories
The installed systemd unit template SHALL declare `StateDirectory=autocoder`, `CacheDirectory=autocoder`, `LogsDirectory=autocoder`, AND `RuntimeDirectory=autocoder` under `[Service]`. systemd auto-creates these directories with the service user's ownership at unit-start time and sets the `$STATE_DIRECTORY`, `$CACHE_DIRECTORY`, `$LOGS_DIRECTORY`, `$RUNTIME_DIRECTORY` environment variables, which the daemon's path-resolution reads (per the resolution-priority requirement).

#### Scenario: Rendered unit contains the four directives
- **WHEN** the install wizard renders the systemd unit template
- **THEN** the rendered unit text contains the lines `StateDirectory=autocoder`, `CacheDirectory=autocoder`, `LogsDirectory=autocoder`, AND `RuntimeDirectory=autocoder` under the `[Service]` section

#### Scenario: Daemon under systemd uses systemd-provided paths
- **WHEN** the daemon is started by systemd AND systemd has created the four directories AND set the corresponding env vars AND no config or `AUTOCODER_*_DIR` overrides exist
- **THEN** the resolved `DaemonPaths.state` matches `$STATE_DIRECTORY` (likely `/var/lib/autocoder`)
- **AND** the resolved `DaemonPaths.cache` matches `$CACHE_DIRECTORY` (likely `/var/cache/autocoder`)
- **AND** the resolved `DaemonPaths.logs` matches `$LOGS_DIRECTORY` (likely `/var/log/autocoder`)
- **AND** the resolved `DaemonPaths.runtime` matches `$RUNTIME_DIRECTORY` (likely `/run/autocoder`)

### Requirement: Dependency-aware ordering pre-pass in sync-specs rebuild
Before enumerating archived changes for chronological replay, the `autocoder sync-specs --rebuild` subcommand SHALL scan every archived change's spec deltas, build a dependency graph from `## MODIFIED Requirements` / `## REMOVED Requirements` / `## RENAMED Requirements` blocks to the changes that originally `## ADDED Requirements` those headers, and topologically reorder same-day archives so every ADDING change is processed before any change that operates on its requirement headers. The reordering is persisted as `aNN-` prefixes (two-digit zero-padded, after the date prefix) on the affected archive directory names so subsequent rebuilds see the dependency order encoded in alphabetical sort and no further reordering is needed.

#### Scenario: Same-day MODIFY-before-ADD inversion is automatically fixed
- **WHEN** the archive contains two same-day changes whose alphabetical order has a MODIFYING change sorting before its dependency-providing ADDING change
- **THEN** the pre-pass renames the ADDING change's directory to prefix it with `a01-` (after the date prefix) so it sorts first within the day-group
- **AND** the subsequent chronological-enumeration loop processes the ADDING change first
- **AND** the subsequent MODIFY succeeds against canonical state that now contains the required requirement

#### Scenario: Day with no within-day dependencies produces no renames
- **WHEN** all changes within a date prefix's day-group have no MODIFIED / REMOVED / RENAMED-FROM dependencies on requirements ADDED by other changes in the same day-group
- **THEN** the pre-pass produces zero `RenamePlan` entries for that day-group
- **AND** no archive directories in that day-group are renamed

#### Scenario: Minimum-renames principle
- **WHEN** a day-group requires reordering of K entries
- **THEN** only the K entries whose alphabetical position needs to change SHALL receive `aNN-` prefixes
- **AND** entries already in the correct alphabetical position SHALL NOT be renamed

#### Scenario: Renames are persistent across rebuild runs
- **WHEN** a second rebuild runs against an archive where a prior rebuild already applied `aNN-` prefix renames
- **THEN** the pre-pass produces zero new renames
- **AND** the archive directory names are unchanged

#### Scenario: Stable secondary sort preserves original alphabetical order
- **WHEN** two entries in a day-group have no mutual dependency
- **THEN** their relative order in the topological output matches their relative order in the original alphabetical sort

### Requirement: Rebuild aborts on unresolvable dependency conditions
The pre-pass SHALL detect two graph conditions that cannot be resolved by within-day prefix renames and SHALL abort the rebuild with a structured error before any rename or canonical-spec update is applied. The abort SHALL surface via `RebuildReport.abort_reason: Some(...)` carrying the offending change names and requirement headers, and SHALL post a chatops `❌` notification describing the condition.

#### Scenario: Cycle detection aborts the rebuild
- **WHEN** the dependency graph contains a cycle (e.g. A MODIFIES a requirement ADDED by B, and B MODIFIES a requirement ADDED by A)
- **THEN** the pre-pass returns `Err(RebuildAbortReason::Cycle { changes, requirements })` with both involved change names and both `(capability, requirement)` pairs populated
- **AND** the rebuild aborts without applying any renames
- **AND** the rebuild aborts without modifying any canonical spec files
- **AND** a chatops `❌` notification is posted naming both involved changes

#### Scenario: Cross-day backward dependency aborts the rebuild
- **WHEN** a change archived on day D MODIFIES / REMOVES / RENAMES-FROM a requirement first ADDED by a change archived on day D' where D' > D
- **THEN** the pre-pass returns `Err(RebuildAbortReason::CrossDayBackwardDependency { dependent, dependency, capability, requirement_header })`
- **AND** the rebuild aborts without applying any renames
- **AND** the rebuild aborts without modifying any canonical spec files
- **AND** a chatops `❌` notification is posted naming both involved changes and the date inversion

#### Scenario: Day-group with more than 99 reorderable entries aborts
- **WHEN** a single date prefix's day-group requires `aNN-` prefixes for more than 99 entries
- **THEN** the pre-pass returns `Err(RebuildAbortReason::ScanFailed { error })` whose message states "more than 99 same-day reorderable entries; manual intervention required"
- **AND** the rebuild aborts without applying any partial renames

### Requirement: Chatops notification surfaces the applied renames
When at least one rename is applied during a rebuild, the daemon SHALL post a chatops notification listing the renames before opening the rebuild PR. The notification groups renames by their date-group day, names each `FROM → TO`, and includes a one-line human-readable summary of the dependency that triggered each rename. When no renames are applied, no rename-notification fires (the existing PR-opened notification covers the normal case).

#### Scenario: Successful rebuild with renames posts the `🔀` notification
- **WHEN** `report.prefix_renames` is non-empty after a successful rebuild
- **THEN** the daemon posts a chatops notification whose first line is `🔀 <repo>: rebuild applied dependency-prefix renames in <N> day-group(s)`
- **AND** the body of the notification groups the renames by day
- **AND** each rename is listed in the form `<from> → <to>` with a parenthetical dependency_summary on the next line
- **AND** the notification is posted BEFORE the existing PR-opened notification so operators see the renames first

#### Scenario: Successful rebuild without renames posts no rename-notification
- **WHEN** `report.prefix_renames` is empty after a successful rebuild
- **THEN** no `🔀` notification is posted
- **AND** the existing PR-opened notification fires unchanged

#### Scenario: Notification failure does not block PR creation
- **WHEN** the chatops `post_notification` call fails (network blip, channel renamed, etc.) during the rename-notification post
- **THEN** the daemon logs at ERROR with the underlying error
- **AND** PR creation proceeds normally

### Requirement: PR body lists the renames
When the rebuild's `RebuildReport.prefix_renames` is non-empty, the generated PR body SHALL include a section titled `**Applied dependency-prefix renames**` listing each rename in the same `FROM → TO` form as the chatops notification, grouped by day. The section SHALL appear BEFORE the existing `**Canonical spec files**` section so the operator reviewing the PR diff sees the renames first and can decide whether to keep, edit, or reject them.

#### Scenario: Rename section appears in the PR body
- **WHEN** the rebuild applied at least one rename and successfully produced a PR
- **THEN** the PR body contains a section titled `**Applied dependency-prefix renames**`
- **AND** the section appears before the `**Canonical spec files**` section
- **AND** the section lists every rename grouped by day with dependency summaries

### Requirement: `propose` chatops verb queues a chat-driven triage request
The chatops listener SHALL recognize `@<bot> propose <repo-substring> <free-form text>` as the `ProposeRequest` command. The repo-substring follows the established case-insensitive substring-matching rules. The free-form text is everything after the substring (trimmed of leading/trailing whitespace, line breaks preserved internally, capped at 10,000 characters). On a unique repo match, the dispatcher SHALL: generate a `request_id`, post a one-line ack that includes the trailing phrase "Follow along in this thread.", capture the ack message's `ts` as the request's lifecycle `thread_ts`, write a `ProposalRequestState` file with `status: Pending`, AND submit a `queue_proposal_request` control-socket action so the next polling iteration picks up the request.

#### Scenario: Happy-path queueing with thread creation
- **WHEN** an operator posts `@<bot> propose myrepo add a /healthz endpoint` AND `myrepo` uniquely resolves to a configured repo
- **THEN** the bot posts a top-level ack message containing `✓ Queued proposal request for <repo_url>. The next polling iteration will run it (~Nm). Follow along in this thread.`
- **AND** the ack's `ts` becomes the request's `thread_ts`
- **AND** a `ProposalRequestState` file is written with `status: Pending`
- **AND** the per-repo `pending_proposal_requests` queue gains an entry

#### Scenario: Missing request text is rejected
- **WHEN** an operator posts `@<bot> propose myrepo` (no free-form text after the substring)
- **THEN** the bot replies `✗ propose: missing request text. Usage: @<bot> propose <repo> <free-form description>`
- **AND** no state file is written

#### Scenario: Repo substring ambiguity surfaces the candidate list
- **WHEN** the repo-substring matches multiple configured repos
- **THEN** the bot replies with the existing `match_repo`-style "be more specific" list
- **AND** no state file is written

### Requirement: Triage prompt classifies the request as DIRECTIVE, QUESTION, or AMBIGUOUS before acting
The triage-mode prompt for chat-driven requests (`prompts/chat-request-triage.md`) SHALL begin with a classification step. The LLM decides:

- **DIRECTIVE**: the input asks for a specific action a reasonable engineer could build. The LLM proceeds to explore the codebase, classify what needs to be done as fix-vs-spec, apply fixes, create spec proposals.
- **QUESTION**: the input asks for analysis, opinion, or exploration of options. The LLM writes its response to `<workspace>/.chat-reply.md` and STOPS. No source-file modifications.
- **AMBIGUOUS**: the request might be a directive but the LLM cannot pin down what to build. The LLM SHALL use the `ask_user` MCP tool to ask the operator for clarification. The existing chatops escalation posts the question in the request's thread and resumes the executor with the operator's answer.

#### Scenario: Directive proceeds to explore + classify + fix/spec
- **WHEN** the operator's request is `add a /healthz endpoint that returns 200 OK with the daemon's version and uptime`
- **THEN** the LLM classifies as DIRECTIVE
- **AND** proceeds with the explore + classify + fix-or-spec flow
- **AND** the diff after execution contains code changes (and optionally a new `openspec/changes/<derived-slug>/` directory)

#### Scenario: Question writes to .chat-reply.md and stops
- **WHEN** the operator's request is `what would it take to refactor the auth module to use the new error type?`
- **THEN** the LLM classifies as QUESTION
- **AND** writes its analysis to `<workspace>/.chat-reply.md`
- **AND** does NOT modify any other files
- **AND** `git status --porcelain` (after the executor returns) shows only `.chat-reply.md` as new/modified

#### Scenario: Ambiguous request escalates via ask_user
- **WHEN** the operator's request is `something something handler logic` (genuinely unclear)
- **THEN** the LLM classifies as AMBIGUOUS
- **AND** uses the `ask_user` MCP tool to post a clarifying question
- **AND** the existing chatops escalation posts the question in the request's `thread_ts`
- **AND** the operator's reply resumes the executor

### Requirement: `.chat-reply.md` marker drives the discussion-reply path
After the triage executor returns `Completed`, the polling iteration SHALL check for `<workspace>/.chat-reply.md` BEFORE running the diff-split + two-PR creation. The presence of this file means "the LLM classified as QUESTION and wrote its response here." The iteration SHALL: read the file contents, truncate at 35,000 characters with a daemon-log pointer when over, post the contents as a threaded reply in the request's `thread_ts`, delete `<workspace>/.chat-reply.md`, and set the state's `status` to `Discussed`. If `git status --porcelain` reports any OTHER modifications, the iteration SHALL log WARN naming them AND revert via `git reset --hard HEAD; git clean -fd`. No PRs are created.

#### Scenario: Clean discussion reply
- **WHEN** the executor returns Completed AND `.chat-reply.md` is the only modified file
- **THEN** the file contents post as a threaded reply in the request's thread
- **AND** the file is deleted
- **AND** the state's `status` is `Discussed`
- **AND** no PR is created
- **AND** no WARN log fires

#### Scenario: Discussion reply with leaked source modifications is cleaned up
- **WHEN** the executor returns Completed AND `.chat-reply.md` is present AND `git status --porcelain` ALSO shows modifications to other files
- **THEN** the file contents post as a threaded reply normally
- **AND** the state's `status` is `Discussed`
- **AND** a WARN log fires naming the unexpected other modifications
- **AND** the workspace is reverted via `git reset --hard HEAD; git clean -fd` so the next iteration sees a clean tree

#### Scenario: Long reply is truncated with daemon-log pointer
- **WHEN** the `.chat-reply.md` contents exceed 35,000 characters
- **THEN** the posted thread reply is truncated to 35,000 chars
- **AND** ends with `… [truncated; full reply at journalctl -u autocoder | grep request_id=<request_id>]`

### Requirement: Directive triage uses the existing two-PR mechanic; PRs participate in the revision-loop
When the executor returns `Completed` without a `.chat-reply.md` marker, the polling iteration SHALL discard non-spec writes from the working tree (via the same helper used by the audit-triage path) AND open AT MOST ONE PR — the spec PR — when spec content exists. Code-path writes are dropped before commit; a WARN log AND a chatops reply name the dropped paths when applicable. The two-PR shape from prior canonical text is removed; implementation flows through the standard implementer pipeline on a subsequent polling iteration after the operator merges the spec PR. Operators commenting `@<bot> revise <text>` on the spec PR continue to get revisions through `a01-pr-comment-revision-loop` per the unchanged revision-loop semantics.

#### Scenario: Mixed-diff directive produces one spec PR; code paths discarded with chatops warning
- **GIVEN** the directive's executor returns `Completed` with BOTH code changes in `src/foo.rs` AND new files in `openspec/changes/<chat-derived-slug>/`
- **WHEN** the chat-triage completion handler runs
- **THEN** the discard step restores `src/foo.rs`
- **AND** the daemon creates a spec branch + PR with ONLY the openspec paths
- **AND** the PR body does NOT mention a companion fixes PR
- **AND** the daemon posts a chatops reply in the proposal-thread naming `src/foo.rs` as dropped
- **AND** the proposal-request state's `status` flips to `Acted`

#### Scenario: Spec-only directive produces one spec PR
- **GIVEN** the directive's diff has only new `openspec/changes/<chat-derived-slug>/` paths
- **WHEN** the chat-triage completion handler runs
- **THEN** the spec PR is created
- **AND** no chatops warning is posted
- **AND** the proposal-request state's `status` flips to `Acted`

#### Scenario: Code-only directive produces NO PR
- **GIVEN** the directive's diff has only code paths (no new `openspec/changes/<chat-derived-slug>/`)
- **WHEN** the chat-triage completion handler runs
- **THEN** the discard step restores the code paths
- **AND** no PR is opened
- **AND** the daemon posts a chatops reply in the proposal-thread naming `no spec content produced; retry with a clearer directive`
- **AND** the proposal-request state's `status` flips to `TriageFailed`

#### Scenario: Empty-diff directive posts a no-action reply
- **GIVEN** the directive's executor returns `Completed` with an empty diff AND no `.chat-reply.md`
- **WHEN** the chat-triage completion handler runs
- **THEN** no PRs are created
- **AND** the bot posts a reply in the request's thread explaining no action was taken
- **AND** the proposal-request state's `status` flips to `Acted`

#### Scenario: Revision comments on a triage PR are processed normally
- **GIVEN** a chat-request-spawned PR has an operator comment `@<bot> revise <text>`
- **WHEN** the revision-loop dispatcher polls for new PR comments
- **THEN** the existing dispatcher (per `a01-pr-comment-revision-loop`) picks up the comment AND processes the revision against the PR's branch
- **AND** the proposal-request state file is not consulted (the revision is its own scope)
- **AND** the revision agent's writes remain scoped to the PR's diff (which by construction now contains only spec files)

### Requirement: Proposal-request state files are pruned after 7 days
The daemon SHALL prune `ProposalRequestState` files whose `submitted_at` is older than 7 days. The prune runs periodically (at iteration start or once per day per the existing housekeeping pattern). Stale entries are removed regardless of `status`.

#### Scenario: Stale entry is removed
- **WHEN** the prune runs AND a `ProposalRequestState` has `submitted_at` more than 7 days in the past
- **THEN** the state file is removed

#### Scenario: Fresh entry is preserved
- **WHEN** the prune runs AND a `ProposalRequestState` has `submitted_at` within the last 7 days
- **THEN** the state file is NOT removed regardless of status

### Requirement: Install wizard probes systemd for an existing installation before falling through to default-path checks
`autocoder install` SHALL probe `systemctl show autocoder.service` before its default-path idempotency check to detect existing installations whose config is at a non-default location. The probe SHALL extract three properties: `LoadState`, `FragmentPath`, and the `--config <path>` argument from `ExecStart`. The result SHALL drive a three-way branch:

- `LoadState=loaded` AND `--config <path>` extracted AND `<path>` exists → existing-install detected. The subcommand SHALL print a status block naming the existing config path and the three remediation verbs (`./update.sh` for binary update, `autocoder install --reconfigure <section>` for section-level re-prompt, `sudo rm -rf <config-dir> && ./install.sh` for full reset) AND exit 0 without invoking the wizard, creating users, installing packages, or rewriting any file.
- `LoadState=loaded` AND `--config <path>` extracted AND `<path>` does NOT exist → broken install. The subcommand SHALL exit non-zero with a diagnostic naming the unit's `FragmentPath`, the missing config path, and the suggested remediations.
- `LoadState=not-found` OR the probe itself fails (no systemd, command errors) OR `--config <path>` cannot be extracted from `ExecStart` → fall through to the existing `<config-dir>/config.yaml` idempotency check. Pre-spec behavior preserved.

Dev mode (`autocoder install --mode dev`, or auto-detected dev mode on macOS / non-systemd Linux) SHALL skip the probe entirely — dev mode has no systemd unit, and running `systemctl show` would either error or report `not-found`.

#### Scenario: Existing install at a non-default config location is detected and respected
- **WHEN** an operator runs `autocoder install` on a server-mode host AND `systemctl show autocoder.service` reports `LoadState=loaded` AND `ExecStart` contains `--config /home/autocoder/autocoder/config.yaml` AND that file exists
- **THEN** the subcommand prints a status block naming `/home/autocoder/autocoder/config.yaml` AND the three remediation verbs
- **AND** the subcommand exits 0
- **AND** the operator's existing config, secrets, and systemd unit are NOT modified
- **AND** `useradd`, `apt-get install`, `daemon-reload`, `enable_systemd_unit`, and `start_systemd_unit` are NOT called (verifiable via the `RecordedCall` log in `cargo test`)

#### Scenario: Broken install (unit loaded, config missing) is refused with a diagnostic
- **WHEN** the operator runs `autocoder install` AND `systemctl show autocoder.service` reports `LoadState=loaded` AND `ExecStart` contains `--config <path>` AND `<path>` does NOT exist on disk
- **THEN** the subcommand exits non-zero
- **AND** the error message names the unit's `FragmentPath`
- **AND** the error message names the missing config path
- **AND** the error message lists at least two remediation hints (restore the config from backup OR remove the unit file and re-run `install.sh`)
- **AND** no file is created or modified by the install subcommand

#### Scenario: No existing unit falls through to default-path check
- **WHEN** the operator runs `autocoder install` AND `systemctl show autocoder.service` reports `LoadState=not-found`
- **THEN** the subcommand proceeds to the existing default-path check at `<config-dir>/config.yaml`
- **AND** if that file exists, behavior matches the pre-spec "Existing config detected" scenario
- **AND** if that file does not exist, the wizard runs as it did pre-spec

#### Scenario: `systemctl` itself fails (host has no systemd binary)
- **WHEN** the operator runs `autocoder install` AND the `systemctl` command exits non-zero OR the binary is not on PATH
- **THEN** `probe_systemd_unit` returns `LoadState::NotFound` (treating the failure as "no unit found")
- **AND** the subcommand falls through to the default-path check
- **AND** the operator is not blocked from completing a fresh install on a non-systemd host

#### Scenario: Loaded unit with no `--config` flag falls through with a WARN
- **WHEN** the unit's `ExecStart` does NOT include `--config <path>` (operator launches autocoder against a config implied via env var, for example)
- **THEN** the subcommand logs a WARN naming the unit's `FragmentPath` and noting the missing `--config` flag
- **AND** the subcommand falls through to the default-path check (the parser cannot determine which config to respect; refusing to proceed on this ambiguity is worse than the default-path fallback)

#### Scenario: Dev mode skips the systemd probe
- **WHEN** the operator runs `autocoder install --mode dev` on any platform OR `autocoder install` on macOS / non-systemd Linux
- **THEN** `probe_systemd_unit` is NOT invoked (verifiable via the `RecordedCall` log)
- **AND** the existing dev-mode flow (write to `~/.config/autocoder/`, no systemd work) proceeds unchanged

#### Scenario: Probe surface is testable via the `SystemActions` trait
- **WHEN** the install-subcommand tests run under `cargo test`
- **THEN** every test uses a `RecordingActions` impl whose `probe_systemd_unit` returns a configured `SystemdUnitProbe` fixture
- **AND** tests cover at minimum: a loaded unit with a valid `--config` path; a loaded unit with a missing `--config` path; a not-found unit; a loaded unit with no `--config` flag; a probe-fails-entirely case
- **AND** no test invokes the production `RealSystemActions::probe_systemd_unit`

### Requirement: Install wizard `--reconfigure` flag re-runs one section against an existing install
`autocoder install` SHALL accept a `--reconfigure <section>` flag whose value is one of `audits`, `reviewer`, or `chatops`. The flag SHALL operate only against a detected existing install (located via the `a01` systemd probe OR the default-config-path fallback). The flag SHALL be mutually exclusive with `--non-interactive` AND with every prefill flag (`--repo-url`, `--token-env-var`, `--chatops-backend`, etc.); reconfigure is interactive and section-scoped by definition.

Per-section behavior:

- **`--reconfigure audits`** SHALL re-prompt every audit cadence with the operator's current cadence as the default, then patch ONLY the `audits.defaults.*` subtree of the existing `config.yaml` in place via atomic temp-file-then-rename. The patch overwrites the file; YAML comments outside the audits subtree are not preserved because `serde_yaml` does not round-trip comments.
- **`--reconfigure reviewer`** AND **`--reconfigure chatops`** SHALL re-prompt the relevant section, then show the operator a unified diff between the current `config.yaml` and the proposed new YAML AND prompt `Apply this patch? [y/N]`. The patch is applied only on `y/Y`; any other answer (including the default) leaves the file unchanged.

After a successful patch, the subcommand SHALL print restart guidance naming `sudo -u autocoder autocoder reload` as the apply step. The wizard SHALL NOT auto-reload — the operator decides when to apply.

The following knobs SHALL NOT be accessible via `--reconfigure`:

- `repositories` (use `autocoder reload`, which hot-applies add/remove without a daemon restart)
- `paths.*` (relocating data directories is destructive and restart-required)
- `executor.*` (the only block that requires a daemon restart)
- `audits.settings.*.prompt_path` and `audits.settings.*.extra.*` (advanced overrides; edit YAML directly)

#### Scenario: `--reconfigure audits` re-prompts cadences and patches in place
- **WHEN** the operator runs `autocoder install --reconfigure audits` against an existing server-mode install whose `audits.defaults.drift_audit` is `weekly`
- **THEN** the wizard prompts for each audit's cadence with the existing value as the displayed default
- **AND** if the operator answers `monthly` for `drift_audit`, the patched config has `audits.defaults.drift_audit: monthly`
- **AND** other top-level keys in `config.yaml` (`github`, `repositories`, `executor`, etc.) parse to the same values they had pre-patch
- **AND** the file is written via atomic temp-file-then-rename, preserving the existing mode and owner
- **AND** the wizard prints `Patched audits.defaults.* in <path>. To apply: sudo -u autocoder autocoder reload`

#### Scenario: `--reconfigure reviewer` shows a diff and applies only on confirmation
- **WHEN** the operator runs `autocoder install --reconfigure reviewer` against an existing install whose `reviewer.provider` is `anthropic` AND `reviewer.model` is `claude-sonnet-4-6`
- **AND** the operator answers `openai_compatible` for provider AND `grok-3` for model
- **THEN** the wizard generates the proposed full YAML
- **AND** prints a unified diff between the current file and the proposed file
- **AND** prompts `Apply this patch? [y/N]`
- **AND** if the operator answers `y`, the file is overwritten via atomic temp-file-then-rename
- **AND** if the operator answers `n` (or presses Enter to accept the default), the file is unchanged AND the wizard prints `no changes made`

#### Scenario: `--reconfigure` against a host with no existing install exits non-zero
- **WHEN** the operator runs `autocoder install --reconfigure audits` AND neither the systemd probe NOR `<default-config-dir>/config.yaml` resolves to an existing file
- **THEN** the subcommand exits non-zero
- **AND** the error message reads `no existing install detected; run install.sh for first-time setup`
- **AND** no file is created

#### Scenario: `--reconfigure` is mutually exclusive with `--non-interactive`
- **WHEN** the operator runs `autocoder install --reconfigure audits --non-interactive`
- **THEN** clap rejects the invocation at argument-parse time
- **AND** the error message names both flags AND the conflict
- **AND** no file is created or modified

#### Scenario: `--reconfigure repositories` is rejected (excluded from the surface)
- **WHEN** the operator runs `autocoder install --reconfigure repositories`
- **THEN** clap rejects the value with the standard `possible values: audits, reviewer, chatops` message
- **AND** the wizard does NOT prompt and does NOT modify any file
- **AND** the operator workflow for repository changes (`autocoder reload`) is documented in the help text or docs

#### Scenario: Probe-resolved config path is honored over default
- **WHEN** the systemd probe (from `a01`) reports an existing unit with `--config /home/autocoder/autocoder/config.yaml`
- **AND** the operator runs `autocoder install --reconfigure audits`
- **THEN** the wizard reads from AND writes to `/home/autocoder/autocoder/config.yaml`, NOT the default `/etc/autocoder/config.yaml`
- **AND** the operator's existing config location is respected throughout the reconfigure flow

#### Scenario: Reconfigure handlers are testable via ScriptedIo
- **WHEN** the reconfigure tests run under `cargo test`
- **THEN** each test uses a `ScriptedIo` impl with a pre-loaded answer queue
- **AND** the `apply_in_place_patch` and `confirm_diff_and_apply` helpers are exercised against temp files
- **AND** the recorded calls assert what was prompted AND what was written
- **AND** no test invokes systemctl, useradd, or any other OS-mutating action

### Requirement: `check-config` subcommand validates a config file without side effects
autocoder SHALL ship a `check-config` subcommand alongside `run`, `reload`, `rewind`, `audit run`, and `install`. The subcommand SHALL accept `--config <path>` (required) AND `--json` (optional flag). It SHALL run the same validation pipeline `autocoder run` executes at startup (YAML parse, schema validation, token-route resolution, workspace-collision check, audit-slug validation, path-collision check, secret-source check) AND exit with one of three codes: `0` on a fully-valid config, `1` on a config that passes hard-error checks but has at least one WARN-level finding, `2` on at least one hard error. The subcommand SHALL NOT spawn any daemon work, SHALL NOT mutate any file, AND SHALL NOT contact any external service.

A shared free function `validate_config(config: &Config) -> ValidationReport` SHALL host every check. The `check-config` subcommand AND the `autocoder run` startup path SHALL both call this function so the surface stays in sync — there is no "check-config validates extra things" OR "autocoder run skips a check" drift.

#### Scenario: Valid config exits 0 with OK lines
- **WHEN** an operator runs `autocoder check-config --config <valid-config-path>`
- **THEN** the subcommand exits 0
- **AND** stdout contains one `OK:` line per validated category (schema, token-route, workspace-collision, audit-slug, path-collision, secret-source)
- **AND** stderr is empty

#### Scenario: Schema violation exits 2 with an ERROR line and stderr summary
- **WHEN** the config has `repositories[0].poll_interval_sec: 0` (a schema violation)
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 2
- **AND** stdout contains a line starting with `ERROR: schema:` naming the offending field AND its `config_pointer` (e.g. `repositories/0/poll_interval_sec`)
- **AND** stderr contains a summary line: `check-config: 1 error(s), 0 warning(s) in <path>`

#### Scenario: Missing env var produces a WARN and exits 1
- **WHEN** the config references `github.token_env: GITHUB_TOKEN` AND the `GITHUB_TOKEN` env var is unset in the calling environment AND no inline `github.token` is set
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 1
- **AND** stdout contains a line starting with `WARN: secret-source:` naming the env var
- **AND** stderr contains `check-config: 0 error(s), 1 warning(s) in <path>`
- **AND** the WARN does not block: a config that has only WARNs but no ERRORs still exits 1 (not 2)

#### Scenario: Parse failure exits 2 with the serde_yaml diagnostic
- **WHEN** the config file contains malformed YAML
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 2
- **AND** stdout contains a line starting with `ERROR: parse:` AND the serde_yaml error message (including line/column where available)
- **AND** no other validation categories are reported (validation cannot continue past a parse failure)

#### Scenario: Token-route gap exits 2 with a structured diagnostic
- **WHEN** the config has `repositories[1].url` with owner `my-org-b` AND `github.owner_tokens` has no `my-org-b` entry AND `github.token_env` references an unset env var AND no inline `github.token` is set
- **AND** the operator runs `autocoder check-config --config <path>`
- **THEN** the subcommand exits 2
- **AND** stdout contains a line starting with `ERROR: token-route:` naming the unresolved owner AND the repo's `config_pointer`

#### Scenario: --json flag emits one JSON object per finding plus a summary
- **WHEN** the operator runs `autocoder check-config --config <path> --json`
- **THEN** stdout contains one JSON object per line, each shaped `{"level": "error"|"warn"|"ok", "category": "<slug>", "message": "<text>", "config_pointer": "..."}`
- **AND** the final line is `{"level": "summary", "errors": N, "warnings": M, "config": "<path>"}`
- **AND** every line is independently parseable as JSON
- **AND** exit code matches the non-JSON behavior (0 / 1 / 2)

#### Scenario: `autocoder run` startup uses the same validation pipeline
- **WHEN** `autocoder run` starts up against a config with a hard error
- **THEN** the startup path invokes `validate_config(&config)` AND reads `report.errors`
- **AND** if any errors are present, the daemon exits non-zero with the same error message `check-config` would produce
- **AND** the existing startup-error tests continue to pass without modification

### Requirement: Daemon emits a startup version notification on every successful boot
After `autocoder run`'s startup pipeline completes (configs validated, chatops backend constructed, repositories enumerated) AND before the first polling iteration begins, the daemon SHALL post a one-line notification to chatops naming the binary version AND the count of configured repositories. The version string SHALL come from `env!("AUTOCODER_VERSION")` — populated at build time by `build.rs` running `git describe --tags --always --dirty` — NOT from `env!("CARGO_PKG_VERSION")`. The notification SHALL fire on every successful startup — not only after an `update.sh`-driven restart — because every restart is a meaningful operator signal. The notification SHALL be suppressed when no chatops backend is configured AND SHALL NOT be gated by any flag under `chatops.notifications.*` (those flags govern per-change and per-event signals; the startup line is a daemon-lifecycle signal).

#### Scenario: Startup notification fires once per boot with version and repo count
- **WHEN** the daemon starts up against a config with `chatops.provider: slack` AND 3 configured repositories
- **THEN** exactly one `post_notification` call fires to the resolved default channel
- **AND** the message contains the literal `🆙` prefix
- **AND** the message contains `autocoder v<X>` where `<X>` matches `env!("AUTOCODER_VERSION")` verbatim
- **AND** the version string is the `git describe --tags --always --dirty` output (e.g. `v1.1.1` at a clean tag, `v1.1.1-23-g4abc123` at a development commit past the tag, OR the Cargo.toml fallback when `.git/` is absent)
- **AND** the message contains `3 repository(ies) configured`
- **AND** the notification fires before any polling iteration begins

#### Scenario: No chatops backend suppresses the notification
- **WHEN** the daemon starts up against a config with no `chatops:` block
- **THEN** no `post_notification` call fires
- **AND** the daemon emits an INFO log line `startup version: v<X>; <N> repositories` to journalctl as the fallback signal (using the same `env!("AUTOCODER_VERSION")` source)
- **AND** the daemon proceeds to the polling loop without error

#### Scenario: Notification is not gated by `notifications.*` flags
- **WHEN** the daemon starts up against a config with `chatops.notifications.start_work: false` AND `chatops.notifications.failure_alerts: false` AND `chatops.notifications.pr_opened: false`
- **THEN** the startup version notification STILL fires (those flags do not apply to lifecycle signals)
- **AND** an operator who silenced per-change signals still sees the once-per-boot version line

#### Scenario: Notification failure is non-fatal
- **WHEN** the chatops backend's `post_notification` call errors (network blip, channel renamed, scope revoked)
- **THEN** the daemon logs a WARN naming the error AND proceeds to the polling loop
- **AND** no startup is blocked by a notification failure

### Requirement: `changelog` subcommand harvests changelog entries from the OpenSpec archive
autocoder SHALL ship a `changelog` subcommand alongside `run`, `reload`, `rewind`, `audit run`, `install`, and `check-config`. The subcommand SHALL walk the OpenSpec archive directory (`openspec/changes/archive/`) of a target workspace, identify archives added within a tag range, extract per-archive summary text from `proposal.md`, group by primary affected capability, AND emit either markdown (default) or structured JSON to stdout.

The subcommand SHALL NOT spawn any daemon work, mutate any file, contact any external service, or invoke any LLM. It is a pure-data extractor — same archive contents + same tag range produce the same output every invocation.

**Flag surface:**

- `--workspace <path>`: directory containing `openspec/changes/archive/`. Defaults to the current working directory. Operators running against a managed workspace from the daemon host use this flag.
- `--since <tag-or-sentinel>`: lower bound (exclusive). Defaults to the most recent tag on `HEAD`'s ancestry as reported by `git describe --tags --abbrev=0 HEAD`. The literal value `ever` is a sentinel meaning "from the beginning of archive history" — useful for first-release runs.
- `--to <tag-or-ref>`: upper bound (inclusive). Defaults to `HEAD`.
- `--format markdown|json`: output shape. Default `markdown`.

**Tag-range resolution edge cases:**

- `--since` unset AND `git describe --tags --abbrev=0 HEAD` exits non-zero (no tags exist) → fall back to "from ever" AND emit one stderr line: `No tags found in this repo; emitting full archive history. Pass --since ever to suppress this notice.` Exit 0.
- `--since <tag>` referencing a tag that does not exist → exit non-zero with a clear error naming the missing tag.

**Frontmatter overrides** on a change's `proposal.md`:

- Absent OR no `changelog:` field → default behavior: use the first paragraph of `## Why` as the entry's summary.
- `changelog: skip` (or `internal`, `hidden` — accept synonyms) → omit the change from output AND record it in the `skipped` list (JSON output) or a footer (markdown output, when at least one change was skipped).
- `changelog: { summary: "<text>" }` → use the override summary instead of the first-`## Why` paragraph.
- Unrecognized `changelog:` value → emit a WARN log naming the value, fall through to default behavior.

#### Scenario: Default invocation emits markdown grouped by capability
- **WHEN** an operator runs `autocoder changelog` from a repo root with two prior tags AND three archives added since the most recent tag (`drift-audit-spec-contradictions`, `chatops-slack-event-dedup`, `executor-streams-output-incrementally`)
- **THEN** stdout contains a markdown document headed `## <to-ref> — <YYYY-MM-DD>`
- **AND** the changes group under `### chatops-manager` (one entry), `### executor` (one entry), AND `### orchestrator-cli` (one entry — whichever capability owns drift-audit's spec delta)
- **AND** each entry's bullet form is `- **<summary-first-line>** (<slug>) — <rest-of-summary-if-any>`
- **AND** stderr is empty

#### Scenario: No prior tags falls back to "ever" with an INFO line
- **WHEN** the operator runs `autocoder changelog` from a repo root with no tags AND `--since` unset
- **THEN** the subcommand emits one stderr INFO line naming the fallback AND pointing at `--since ever` as the explicit form
- **AND** stdout contains every archive in the repo's history, sorted by shipped-commit order
- **AND** the subcommand exits 0

#### Scenario: `--since ever` explicit form suppresses the INFO line
- **WHEN** the operator runs `autocoder changelog --since ever` from a repo (with or without tags)
- **THEN** stdout contains every archive in history
- **AND** stderr is empty (the INFO line only fires under the implicit fallback path)

#### Scenario: Frontmatter `changelog: skip` omits the change
- **WHEN** an archive's `proposal.md` carries frontmatter `changelog: skip`
- **AND** `autocoder changelog --format json` is run against a range that includes this archive
- **THEN** the change does NOT appear in the JSON output's `entries` array
- **AND** the change DOES appear in the `skipped` array with `{"slug": "...", "reason": "changelog: skip"}`

#### Scenario: Frontmatter `changelog.summary` override replaces the default summary
- **WHEN** an archive's `proposal.md` carries frontmatter `changelog: { summary: "Adds /healthz endpoint for liveness probes" }`
- **AND** the changelog is generated for a range that includes this archive
- **THEN** the entry's summary text is `Adds /healthz endpoint for liveness probes` exactly
- **AND** the first paragraph of `## Why` is NOT used

#### Scenario: JSON output is machine-readable
- **WHEN** the operator runs `autocoder changelog --format json`
- **THEN** stdout contains a single JSON object with `version`, `date`, `since`, `to`, `entries`, and `skipped` top-level fields
- **AND** each entry object includes `slug`, `archive_dir`, `primary_capability`, `summary`, `shipped_commit`, `shipped_date`
- **AND** the JSON parses without error via `serde_json::from_str`
- **AND** the output is pretty-printed (2-space indent) for human readability

#### Scenario: Cross-project usage via `--workspace`
- **WHEN** an operator runs `autocoder changelog --workspace /path/to/another-openspec-repo`
- **THEN** the subcommand reads the named workspace's archive AND git history
- **AND** the operator's cwd is irrelevant
- **AND** the subcommand works against any repo whose `openspec/changes/archive/` directory exists, not just autocoder's own repo

#### Scenario: Archive discovery uses git addition commits, not directory date prefixes
- **WHEN** an archive entry is added to `openspec/changes/archive/` in commit `<sha>`
- **AND** the operator runs `autocoder changelog --since <tag>` where `<tag>` is reachable from before `<sha>`
- **THEN** the entry appears in the output if and only if `<sha>` is reachable from `--to` BUT NOT from `--since`
- **AND** the directory's `YYYY-MM-DD` prefix is used only for the entry's `shipped_date` field, never for range filtering (so a manually-renamed archive directory does not affect what changelogs include)

#### Scenario: Subcommand is testable against synthetic fixtures
- **WHEN** the changelog tests run under `cargo test`
- **THEN** each test stands up a tempdir with a synthetic git repo (`git init`, a few commits adding archive entries, optional tags)
- **AND** the test invokes `execute` with a `ChangelogArgs` pointing at the tempdir
- **AND** assertions cover the markdown / JSON output text exactly
- **AND** no test depends on autocoder's own archive history

### Requirement: `changelog` chatops verb queues an LLM-styled CHANGELOG.md update via the standard triage path
The daemon SHALL accept a `ChangelogAction` over its Unix-domain control socket (submitted by the Slack inbound listener on `@<bot> changelog <repo-substring> [<args>]`). The action SHALL stamp a `ChangelogRequest` state file under `<state_dir>/changelog-requests/<request_id>.json`. On the next polling iteration AFTER the request is queued, the daemon SHALL: (a) run the `a05` deterministic extractor against the workspace's archive AND get the JSON output, (b) invoke the wrapped agent CLI with the embedded `prompts/changelog-stylist.md` system prompt + the JSON data as input, (c) validate the resulting diff's path scope (`CHANGELOG.md` AND optionally `openspec/changes/archive/<slug>/proposal.md` files; reject all others), (d) commit the diff to a `changelog-<short-hash>` branch, push it, AND open a single PR. The PR SHALL participate in the existing PR-comment revision loop without additional plumbing.

The stylist prompt SHALL instruct the agent to check for an existing `CHANGELOG.md` in the workspace root AND match its style if present, OR create a fresh file in the Keep a Changelog v1.1.0 format if absent. The agent SHALL also be permitted to propose `changelog:` frontmatter edits to source proposals when the changelog work surfaces a durable classification decision — but only when the operator's input (initial verb OR revision text) implies such a decision.

`ChangelogRequest` state files SHALL be pruned after 7 days regardless of terminal status (`Acted`, `Failed`, `InFlight`), parallel to the audit-thread and proposal-request pruning schedules.

#### Scenario: Verb queues a request and the next iteration produces a PR
- **WHEN** an operator types `@<bot> changelog coterie` in a watched channel
- **AND** the inbound listener parses the verb AND submits a `ChangelogAction`
- **THEN** the daemon writes a `ChangelogRequest` state file with `status: Pending`
- **AND** the bot posts `✓ Queued changelog request for <repo-url>. The next polling iteration will run it. Follow along in this thread.` as a top-level channel message
- **AND** the ack message's `ts` is stored as the request's `lifecycle_thread_ts`
- **WHEN** the next polling iteration runs
- **THEN** the handler runs the deterministic extractor, invokes the stylist via the executor, captures the diff
- **AND** validates that the diff touches only `CHANGELOG.md` AND/OR `openspec/changes/archive/<slug>/proposal.md` paths
- **AND** commits the diff to a `changelog-<short-hash>` branch
- **AND** opens a single PR
- **AND** posts a threaded reply in the lifecycle thread: `✓ Changelog draft ready at <PR-URL>. Review on GitHub; revise via @<bot> revise <text>.`
- **AND** the request's `status` advances to `Acted`

#### Scenario: Out-of-scope diff is refused
- **WHEN** the LLM's diff touches files outside `CHANGELOG.md` AND `openspec/changes/archive/<slug>/proposal.md`
- **THEN** the handler does NOT commit
- **AND** the handler posts `✗ changelog: LLM produced out-of-scope diff; refusing to commit. See <log-path>.` to the lifecycle thread
- **AND** the request's `status` advances to `Failed`
- **AND** the workspace is left clean (no partial branch, no orphan commit)

#### Scenario: Revision loop iterates the changelog
- **WHEN** an operator posts `@<bot> revise leave out the refactors from this changelog` on the changelog PR
- **THEN** the existing PR-comment revision dispatcher (from `a01-pr-comment-revision-loop`) parses the comment
- **AND** the next polling iteration re-invokes the stylist with the previous draft + the operator's instruction in context
- **AND** the handler validates the new diff's path scope AND force-pushes the updated commit to the `changelog-<short-hash>` branch
- **AND** the PR's diff updates in place; no PR close/re-open

#### Scenario: Revision proposes frontmatter edits when implied
- **WHEN** an operator's revision text implies a durable classification (e.g. `leave out the refactors` OR `internal changes shouldn't appear in the changelog`)
- **THEN** the stylist MAY include `changelog: skip` frontmatter edits to the relevant source proposals in the same diff
- **AND** the operator reviewing the PR sees both the CHANGELOG.md edit AND the proposal.md frontmatter edits in a single diff
- **AND** future invocations of the deterministic extractor honor the frontmatter, so the classification persists across releases

#### Scenario: Fresh-repo CHANGELOG.md creation
- **WHEN** the operator runs `@<bot> changelog <repo>` against a workspace with NO existing `CHANGELOG.md`
- **THEN** the stylist creates `CHANGELOG.md` in the Keep a Changelog v1.1.0 format
- **AND** the file starts with the project name as a top-level heading
- **AND** includes an `## [Unreleased]` placeholder
- **AND** the current release's section appears as `## [<version>] - <YYYY-MM-DD>`
- **AND** the operator reviewing the PR can validate the formatting choice before merging

#### Scenario: Polite refusal — missing repo substring
- **WHEN** an operator types `@<bot> changelog` with no first argument
- **THEN** the listener posts `✗ changelog: missing repo-substring.` as a threaded reply
- **AND** no state file is written
- **AND** the request is idempotent — re-issuing the verb with arguments works as if the first attempt never happened

#### Scenario: Polite refusal — repo substring matches nothing
- **WHEN** the operator's substring does not match any configured repository
- **THEN** the listener posts `✗ changelog: no repo matched '<sub>'; configured: <list>`
- **AND** the candidate list contains every configured `repositories[].url` so the operator can copy-paste a correction

#### Scenario: Polite refusal — chatops backend unconfigured
- **WHEN** the daemon's `OperatorCommandDispatcher` is constructed without a chatops backend
- **AND** an operator's `changelog` verb reaches the parser
- **THEN** the listener responds `✗ changelog: chatops backend not configured.`
- **AND** no state file is written

#### Scenario: Polite refusal — ack post failure
- **WHEN** the ack `post_notification` to the channel fails (HTTP error, scope revoked, channel renamed)
- **THEN** the listener posts the error inline (`✗ changelog: could not post ack to chat: <reason>`) where it can
- **AND** the state file is NOT written
- **AND** the operator can retry the verb after fixing the upstream chatops issue

#### Scenario: 7-day staleness pruning
- **WHEN** a `ChangelogRequest` state file's `submitted_at` is older than 7 days
- **THEN** the next polling iteration's pruning pass removes the file
- **AND** an INFO log line records the pruned `request_id`
- **AND** any PRs spawned from that request continue to work independently (their revision loop is keyed to the PR's branch, not the state file)

### Requirement: All daemon state-file reads and writes route through the `DaemonPaths` resolver
Every daemon-side code path that reads OR writes a state-file shall construct its path through the `DaemonPaths` resolver's helper methods (`state_dir()`, `cache_dir()`, `logs_dir()`, `runtime_dir()`, AND per-state-shape helpers like `audit_threads_dir()`, `busy_markers_dir()`, `proposal_requests_dir()`, etc.). Hard-coded `/tmp/autocoder/` literals SHALL NOT appear in any source file outside an explicit allowlist (today: only the migration scan logic that references the legacy path on purpose). A CI test SHALL grep the source tree for the literal substring AND fail on any unauthorized hit.

This rule eliminates a defect class where readers and writers drift to different paths after the legacy-to-standard migration. Operator-visible symptoms of the defect class included: `send it` returning `?` for real audit threads (read at `/tmp` while writes go to `<state_dir>`); `@<bot> status` reporting `idle` while the busy marker was held (status read at one path, daemon wrote to another).

#### Scenario: Every state-file consumer uses a `DaemonPaths` helper
- **WHEN** a developer searches the codebase for state-file path construction
- **THEN** every read AND every write goes through a `DaemonPaths` method
- **AND** no source file outside the allowlist contains the literal substring `/tmp/autocoder`
- **AND** the allowlist comment names why each allowed file is exempt

#### Scenario: CI test catches new literal hits
- **WHEN** a future contributor adds a hard-coded `/tmp/autocoder/...` path to a source file not in the allowlist
- **AND** `cargo test` runs
- **THEN** the `path_literals_audit` test fails with the offending file:line:line-contents listed
- **AND** the failure message points at the `DaemonPaths` resolver as the correct fix

#### Scenario: Path resolution is consistent across writer and reader for the same state shape
- **WHEN** the daemon's busy-marker writer stamps a marker at `<runtime_dir>/busy/<workspace>.json`
- **AND** the `@<bot> status` reply composer attempts to read the marker for that workspace
- **THEN** both code paths resolve `<runtime_dir>` through the same `DaemonPaths.runtime_dir()` call
- **AND** the reader finds the writer's marker

#### Scenario: Audit-thread state reader and stamper agree
- **WHEN** an audit's threaded-finding post stamps an audit-thread state file via `paths.audit_threads_dir().join(format!("{thread_ts}.json"))`
- **AND** the `send it` dispatcher looks up the same `thread_ts` via the same `paths.audit_threads_dir().join(...)` call
- **THEN** the dispatcher finds the stamped state
- **AND** the `send it` verb produces a triage run instead of a `?` reaction

#### Scenario: Migration code is explicitly allowed to reference the legacy path
- **WHEN** the migration scan (`autocoder/src/state/migration.rs` or equivalent) references `/tmp/autocoder/` to identify legacy state to move
- **THEN** the path-literals CI test treats that file as part of the allowlist
- **AND** the migration code can continue to reference the legacy path without triggering a CI failure

### Requirement: Audit framework bounds audits per iteration to prevent storm patterns
The audit framework's per-iteration scheduler SHALL run at most `audits.max_audits_per_iteration` eligible audits before returning control to the iteration loop. Subsequent eligible audits SHALL defer to the next iteration. The bound applies to BOTH cadence-driven AND on-demand queued runs — every run increments the counter regardless of trigger source. The default value is `1`, intentionally low to ensure pending changes (per `a12`) AND audit work share each iteration's wall-clock fairly. Operators wanting faster audit drainage during onboarding or after major refactors can raise the bound; values above the number of registered audits clamp at the registry count with a WARN.

Audits are tried in the registry's declaration order. On-demand queued audits drain FIRST within the loop (preserving the existing "queued bypasses cadence" semantics), then cadence-driven audits in order. Either source contributes to the per-iteration count.

#### Scenario: Default bound runs one audit per iteration
- **WHEN** `audits.max_audits_per_iteration` is unset (default `1`) AND 3 audits are eligible at the start of an iteration
- **THEN** the scheduler runs the first eligible audit in declaration order
- **AND** the scheduler returns control to the iteration loop after that audit completes
- **AND** the other 2 eligible audits do NOT run this iteration
- **AND** the unrun audits' `.audit-state.json` entries are unchanged — they remain eligible for the next iteration

#### Scenario: Raised bound runs multiple audits per iteration
- **WHEN** `audits.max_audits_per_iteration: 3` AND 5 audits are eligible
- **THEN** the scheduler runs the first 3 eligible audits in declaration order
- **AND** the other 2 defer to the next iteration

#### Scenario: Bound 0 skips all audits
- **WHEN** `audits.max_audits_per_iteration: 0`
- **THEN** the scheduler runs zero audits regardless of how many are eligible
- **AND** the iteration proceeds to push+PR (or no-op if no other commits exist)
- **AND** this behavior is useful for diagnostics OR for temporarily silencing the audit framework

#### Scenario: On-demand queued audits count against the bound
- **WHEN** `audits.max_audits_per_iteration: 1` AND the on-demand queue contains 2 audits AND 1 cadence-driven audit is eligible
- **THEN** the scheduler runs the FIRST queued audit (queued drain has priority)
- **AND** the counter increments to 1, hitting the bound
- **AND** the second queued audit AND the cadence-eligible audit do NOT run this iteration
- **AND** both unrun audits' state is preserved (queue retains the deferred entry; cadence audit's `.audit-state.json` is unchanged)

#### Scenario: Out-of-bounds bound is clamped at the registry count
- **WHEN** `audits.max_audits_per_iteration: 50` AND the registry contains 5 audits
- **THEN** the resolved value is 5
- **AND** a WARN log at startup names both the requested AND clamped values

#### Scenario: Bound interacts cleanly with change-precedence ordering
- **WHEN** an iteration begins AND 2 pending changes are in the queue AND 5 audits are eligible AND bound is default `1`
- **THEN** per `a12`'s change-precedence rule, the 2 pending changes process first
- **AND** the audit phase runs at most 1 audit
- **AND** the iteration's push+PR step ships commits from both phases
- **AND** the next iteration processes any remaining pending changes (likely none, but if any) AND runs 1 more audit, and so on

### Requirement: Mid-iteration recovery failures classify transient vs. permanent; transient retries on next iteration
When a mid-iteration recovery operation (workspace re-clone, dirty cleanup, git fetch retry) returns `Err`, the daemon SHALL classify the failure into one of two categories via a `classify_recovery_failure(err) -> RecoveryFailureClass` helper:

- **Transient**: network errors (DNS resolution failures, connection refused / reset / timed out, TLS handshake failures), GitHub HTTP 5xx (502, 503, 504, 522, 524), HTTP 401 / 403 (auth blip — recoverable on token rotation without daemon restart), HTTP 429 (rate limit), git exit code 128 with stderr matching common network strings ("Could not resolve host", "Connection timed out", "the remote end hung up"), I/O error kinds (`WouldBlock`, `TimedOut`, `ConnectionReset`, `ConnectionAborted`, `BrokenPipe`).
- **Permanent**: configuration errors (missing required field, malformed YAML, no matching token route), missing prerequisites (binaries not on PATH: `openspec`, `git`, `claude`), "remains dirty after recovery" (the existing scenario from `Dirty workspace auto-recovers at startup`).

The default classification for an unrecognized error SHALL be `Transient` — the conservative choice is to retry, since operators have `clear-perma-stuck` AND manual-skip escape hatches for genuinely-permanent failures that mis-classify.

**Transient** failures: log WARN with `class=transient`, fire the existing 24h-throttled `WorkspaceInitFailure` chatops alert with a ` (transient; retrying)` suffix, return from the iteration. The NEXT polling iteration retries automatically — no special backoff state is needed.

**Permanent** failures: log ERROR with `class=permanent`, mark the repo as skipped-for-lifetime (existing helper), fire the alert with a ` (permanent; skipped until daemon restart) — operator inspection required` suffix.

This requirement applies to MID-ITERATION recovery only. Startup-time recovery (the existing `Dirty workspace auto-recovers at startup` requirement) continues its conservative skip-for-lifetime behavior. A future spec MAY extend classification to startup; not in scope here.

#### Scenario: Transient network failure retries automatically
- **WHEN** a mid-iteration recovery operation returns an error whose source chain contains "Could not resolve host github.com"
- **THEN** `classify_recovery_failure` returns `Transient`
- **AND** the iteration logs WARN with `class=transient`
- **AND** a chatops alert (subject to the 24h throttle) fires with the ` (transient; retrying)` suffix
- **AND** the repo is NOT marked skipped-for-lifetime
- **AND** the next polling iteration attempts the recovery again
- **AND** if that iteration succeeds, the repo proceeds normally

#### Scenario: HTTP 503 from GitHub is transient
- **WHEN** a mid-iteration `POST /repos/.../pulls` call returns HTTP 503
- **THEN** the classification is `Transient` (per the 5xx pattern match)
- **AND** the iteration retries on the next polling tick

#### Scenario: 401 auth blip retries (operator may rotate token without restart)
- **WHEN** a GitHub API call returns HTTP 401
- **THEN** the classification is `Transient`
- **AND** the operator can rotate the env-var-backed token (and the daemon's hot-reload picks it up via `autocoder reload`) without restarting

#### Scenario: Permanent failure skips-for-lifetime as before
- **WHEN** the dirty-workspace recovery commands all complete BUT `git status --porcelain` is still non-empty
- **THEN** `classify_recovery_failure` returns `Permanent`
- **AND** the iteration logs ERROR with `class=permanent`
- **AND** a chatops alert fires with the ` (permanent; skipped until daemon restart) — operator inspection required` suffix
- **AND** the repo is skipped for the daemon's process lifetime (existing behavior preserved)

#### Scenario: Default-to-transient handles unknown errors conservatively
- **WHEN** a recovery operation returns an error whose source chain matches none of the documented transient OR permanent patterns
- **THEN** `classify_recovery_failure` returns `Transient`
- **AND** the iteration logs WARN with `class=transient (unclassified)` so the unfamiliar pattern is visible in journalctl
- **AND** the next iteration retries — the choice to retry on unknown failures favors operator-friendly resilience over fast-fail-on-uncertainty

#### Scenario: Startup recovery is unchanged
- **WHEN** a workspace is dirty at daemon startup AND recovery fails
- **THEN** the existing `Dirty workspace auto-recovers at startup` requirement's behavior applies (skip-for-lifetime regardless of classification)
- **AND** this requirement applies only to mid-iteration recovery

### Requirement: Alert-state migration from workspace to state-dir on first startup
On the first daemon start after this spec ships, autocoder SHALL migrate any pre-existing `<workspace>/.alert-state.json` files to their corresponding `<state_dir>/alert-state/<basename>.json` paths. The migration SHALL be per-repository AND idempotent. A daemon-wide migration marker `<state_dir>/alert-state/.migration-from-workspace-done` records that the scan ran AND prevents subsequent startups from re-attempting work.

The migration handles three cases per workspace:

1. **Workspace file exists, state-dir file absent**: move the file via `fs::rename` (same-filesystem) or copy + delete (cross-filesystem).
2. **Both files exist**: the state-dir version wins (more recently authoritative AND survived any prior workspace wipes). Delete the workspace file.
3. **Workspace file is git-tracked** (rare; only for repos whose history transiently committed it): run `git rm --cached <workspace>/.alert-state.json`, commit with subject `chore: untrack .alert-state.json (now stored in daemon state dir per a16)`, push to the base branch.

The migration runs at daemon startup BEFORE any polling task starts. Errors during migration are per-repository AND non-fatal: if one repository's migration fails (e.g., `git push` rejected due to branch protection), the daemon logs ERROR naming the repository AND the failure mode, continues processing other repositories, AND does NOT set the migration marker. Subsequent startups retry.

#### Scenario: Workspace file moves to state-dir cleanly
- **WHEN** the daemon starts up AND the migration marker is absent AND a configured repository has `<workspace>/.alert-state.json` (not git-tracked, no state-dir version present)
- **THEN** the daemon moves the file to `<state_dir>/alert-state/<basename>.json`
- **AND** logs INFO naming the repository AND the source + destination paths
- **AND** after all repositories complete, writes the migration marker

#### Scenario: Both-files-exist case prefers state-dir
- **WHEN** a configured repository has BOTH `<workspace>/.alert-state.json` AND `<state_dir>/alert-state/<basename>.json`
- **THEN** the daemon deletes the workspace file AND keeps the state-dir version unchanged
- **AND** logs INFO noting that the state-dir version was preferred

#### Scenario: Git-tracked workspace file is untracked + committed + pushed
- **WHEN** a configured repository has `<workspace>/.alert-state.json` AND `git ls-files` shows the file tracked
- **THEN** the migration runs `git rm --cached`, commits with the documented subject, AND pushes to the base branch
- **AND** the migration treats success as "complete for this repository"
- **AND** on push failure, the migration logs ERROR with the suggested operator action AND continues to other repositories

#### Scenario: Migration is idempotent via the marker
- **WHEN** the daemon starts up AND `<state_dir>/alert-state/.migration-from-workspace-done` exists
- **THEN** the migration code returns immediately without scanning any workspace
- **AND** the daemon proceeds to its normal startup flow

#### Scenario: Per-repository failure does not set the marker
- **WHEN** the migration attempts a repository whose `git push` fails
- **THEN** the daemon logs ERROR with the failure detail AND the operator action
- **AND** the migration marker is NOT set (so the next startup retries)
- **AND** other repositories' migrations are unaffected — they continue to attempt AND succeed independently

#### Scenario: No pre-existing workspace files means no-op migration
- **WHEN** the daemon starts up AND NO configured repository has `<workspace>/.alert-state.json`
- **THEN** the migration logs INFO noting that no files needed migration
- **AND** writes the migration marker (recording that the scan ran AND found nothing)
- **AND** subsequent startups skip the scan

### Requirement: Spec-delta archivability pre-flight check
Before invoking the executor against any change, autocoder SHALL verify that every spec-delta block in the change's `specs/<capability>/spec.md` files satisfies the header preconditions that `openspec archive` enforces at archive time. The check is mechanical AND cheap: parse each delta block, compare its `### Requirement: <title>` headers against the canonical `openspec/specs/<capability>/spec.md` for the same capability, AND verify per-kind preconditions:

- **ADDED**: title MUST NOT exist in canonical (duplicate-add → flag).
- **MODIFIED**: title MUST exist in canonical, exact match character-for-character (the a07-incident class — invented MODIFIED titles → flag).
- **REMOVED**: title MUST exist in canonical (remove-nothing → flag).
- **RENAMED**: `from:` title MUST exist; `to:` title MUST NOT exist.

On ANY precondition violation, autocoder SHALL write `.needs-spec-revision.json` with the existing schema EXTENDED by an `unarchivable_deltas: [{ capability, kind, header, reason }]` field, post the existing chatops alert under `AlertCategory::SpecNeedsRevision` (subject to the 24h throttle, body enumerating the violations), AND halt the queue walk for this iteration per the existing same-repo blocking policy. The executor SHALL NOT be invoked for this change OR any subsequent change in the same iteration. The principal cost savings: no LLM call against a change whose deltas would fail at archive time.

The check runs on EVERY change before EVERY executor invocation. No caching — the canonical specs might have changed since the last check (a previous iteration's archive could have updated them).

#### Scenario: MODIFIED header missing from canonical is flagged before executor runs
- **WHEN** a change's `specs/code-reviewer/spec.md` contains a `## MODIFIED Requirements` block with header `### Requirement: Reviewer prompt budget is operator-configurable`
- **AND** the canonical `openspec/specs/code-reviewer/spec.md` does NOT contain that title
- **THEN** the pre-flight check returns one `UnarchivableDelta` with `kind=Modified`, `header="Reviewer prompt budget is operator-configurable"`, `reason="header not found in canonical openspec/specs/code-reviewer/spec.md ..."`
- **AND** autocoder writes `.needs-spec-revision.json` with `unarchivable_deltas` populated
- **AND** the executor is NOT invoked for this change
- **AND** no LLM cost is incurred
- **AND** the chatops alert fires under `AlertCategory::SpecNeedsRevision` with body enumerating the violation

#### Scenario: ADDED header duplicate is flagged
- **WHEN** a change's ADDED requirements block contains a title that already exists in canonical
- **THEN** the pre-flight check flags it with `kind=Added`, `reason="header already exists in canonical openspec/specs/<cap>/spec.md — use MODIFIED instead"`

#### Scenario: REMOVED header that doesn't exist is flagged
- **WHEN** a change's REMOVED requirements block contains a title that does NOT exist in canonical
- **THEN** the pre-flight check flags it with `kind=Removed`, `reason="header not found in canonical openspec/specs/<cap>/spec.md — cannot remove non-existent requirement"`

#### Scenario: RENAMED with invalid `from:` is flagged
- **WHEN** a change's RENAMED requirements block has a `from:` title that doesn't exist in canonical
- **THEN** the pre-flight check flags it with `kind=Renamed`, `header="from <a> to <b>"`, `reason="from-title not found in canonical openspec/specs/<cap>/spec.md"`

#### Scenario: RENAMED with `to:` colliding with existing canonical title is flagged
- **WHEN** a change's RENAMED requirements block has a `to:` title that ALREADY exists in canonical (as a different requirement)
- **THEN** the pre-flight check flags it with `kind=Renamed`, `reason="to-title already exists in canonical openspec/specs/<cap>/spec.md — rename would create a duplicate"`

#### Scenario: Clean spec passes pre-flight without ceremony
- **WHEN** every delta block's header preconditions are satisfied
- **THEN** the pre-flight check returns an empty Vec
- **AND** the executor IS invoked (pre-flight is no-op for clean specs)
- **AND** no marker is written
- **AND** no chatops alert fires

#### Scenario: Capability without canonical spec accepts only ADDED
- **WHEN** a change's `specs/<new-cap>/spec.md` introduces a capability that doesn't yet exist in canonical
- **AND** the change's delta blocks are all `## ADDED Requirements`
- **THEN** the pre-flight check passes (no canonical to compare against; new capabilities are fine)
- **WHEN** the same change includes a `## MODIFIED Requirements` block for the new capability
- **THEN** the pre-flight flags it with `reason="capability <cap> has no canonical spec — cannot modify within it"`

#### Scenario: Marker schema is backwards-compatible
- **WHEN** the daemon writes a `.needs-spec-revision.json` with `unarchivable_deltas` populated AND `unimplementable_tasks` empty
- **THEN** the on-disk JSON has both fields (the empty one serialized as `[]` OR omitted via `skip_serializing_if`)
- **WHEN** the daemon reads a pre-spec `.needs-spec-revision.json` (only `unimplementable_tasks` field, no `unarchivable_deltas`)
- **THEN** deserialization succeeds; `unarchivable_deltas` defaults to empty
- **AND** the operator workflow for the pre-spec marker case (edit tasks.md, clear marker) is unchanged

#### Scenario: Check runs on every iteration, no caching
- **WHEN** a change passes pre-flight on iteration N
- **AND** between iterations N AND N+1 the canonical spec is updated such that the change's delta is no longer archivable (e.g. a sibling change archived AND renamed the requirement the MODIFIED targets)
- **THEN** the pre-flight on iteration N+1 catches the new mismatch AND flags the change
- **AND** the check does NOT memoize prior passes

### Requirement: Ignore-for-queue marker downgrades blocking-marker behavior without unblocking the change itself
autocoder SHALL recognize a per-change `.ignore-for-queue.json` marker file at `<workspace>/openspec/changes/<change>/.ignore-for-queue.json`. The marker downgrades any sibling operator-action marker (`.perma-stuck.json`, `.needs-spec-revision.json`) on the same change from "blocks subsequent queue processing" to "still excludes this change from `list_pending`, but doesn't block siblings." The marker is the operator's explicit "I know this change is broken; skip it AND proceed with the rest" signal.

The marker SHALL be writable via the `@<bot> ignore-and-continue` chatops verb (writes the file AND commits/pushes the change directory's update) AND removable via the `@<bot> clear-ignore` verb (removes the file AND commits/pushes the removal). The file is intentionally git-tracked, consistent with `.perma-stuck.json` AND `.needs-spec-revision.json`.

Removal of the underlying blocking marker (e.g. via `@<bot> clear-perma-stuck`) SHALL also remove the `.ignore-for-queue.json` marker — when the underlying marker is gone, the ignore-marker has nothing to downgrade AND becomes vestigial.

#### Scenario: Operator stamps ignore-for-queue; queue resumes for siblings
- **WHEN** a repository has change A with `.perma-stuck.json` AND change B pending (no markers) AND change A also has `.ignore-for-queue.json`
- **THEN** the polling iteration's queue walk processes change B (the ignore-marker downgrades A's blocking effect)
- **AND** change A remains excluded from `list_pending` (perma-stuck marker still applies to A's own status)
- **AND** the iteration's chatops `🚀 starting work on B` fires normally

#### Scenario: `@<bot> ignore-and-continue` writes the marker
- **WHEN** the operator runs `@<bot> ignore-and-continue <repo-substring> <change-slug>`
- **AND** the named change has at least one of `{.perma-stuck.json, .needs-spec-revision.json}`
- **THEN** the daemon writes `.ignore-for-queue.json` inside the change directory containing the change name, marked_at timestamp, marked_by operator identifier, AND the operator_action note
- **AND** the daemon commits the file AND pushes the commit to the agent branch (subject `chore: ignore-for-queue on <change> (operator <id>)`)
- **AND** the chatops reply confirms: `✓ Marked <change> as ignored for queue. Subsequent changes will process; <change> stays excluded until the underlying marker is cleared.`

#### Scenario: `@<bot> ignore-and-continue` rejects when no underlying marker exists
- **WHEN** the operator runs `@<bot> ignore-and-continue <repo> <change>` AND the named change has NEITHER `.perma-stuck.json` NOR `.needs-spec-revision.json`
- **THEN** the daemon refuses: `✗ <change> has no operator-action marker (perma-stuck OR needs-spec-revision). Ignore is a no-op; rejecting to prevent confusion.`
- **AND** no file is written

#### Scenario: `@<bot> clear-ignore` removes the marker, queue resumes blocking
- **WHEN** the operator runs `@<bot> clear-ignore <repo-substring> <change-slug>`
- **AND** the named change has `.ignore-for-queue.json`
- **THEN** the daemon removes the file AND commits/pushes the removal (`chore: clear ignore-for-queue on <change>`)
- **AND** subsequent polling iterations resume blocking the queue on the original marker (if still present)
- **AND** the chatops reply confirms: `✓ Cleared ignore-for-queue on <change>. Queue resumes blocking on <original-marker>.`

#### Scenario: `clear-perma-stuck` removes ignore-for-queue too
- **WHEN** the operator runs `@<bot> clear-perma-stuck <repo> <change>` AND the change has BOTH `.perma-stuck.json` AND `.ignore-for-queue.json`
- **THEN** BOTH files are removed by the same operation
- **AND** the chatops reply notes both removals: `✓ Cleared .perma-stuck.json AND .ignore-for-queue.json for <change>.`
- **AND** the change re-enters `list_pending` on the next iteration (per the existing clear-perma-stuck behavior)

### Requirement: Change-internal contradiction pre-flight check (opt-in)
autocoder SHALL provide an opt-in pre-flight check that detects semantic contradictions among the requirements WITHIN a single OpenSpec change before the executor is invoked. The check uses a configurable LLM to read the change's spec-delta files AND produce a structured JSON listing of contradictions (requirements that cannot all hold simultaneously). On non-empty findings, autocoder SHALL write `.needs-spec-revision.json` with `revision_suggestion` populated from the contradictions narrative, post the existing `AlertCategory::SpecNeedsRevision` chatops alert, AND halt the queue walk for this iteration. The executor SHALL NOT be invoked when contradictions are found.

The check SHALL be gated by `executor.change_internal_contradiction_check` (`disabled` default, `enabled` opt-in). The LLM is configured via `executor.change_internal_contradiction_check_llm` (parallel to the `reviewer:` config block — provider, model, api_key source, optional api_base_url). Enabling the check without configuring the LLM SHALL fail at daemon startup with a fail-fast validation error.

The check SHALL fail-open: LLM transport errors, parse failures, OR malformed responses log a WARN AND treat the check as "no contradictions found." The daemon does NOT gate work on a failed check — operators see the WARN in journalctl AND can investigate; the executor proceeds.

The check runs AFTER `a17`'s mechanical archivability check AND BEFORE the executor. The two checks are layered: `a17` catches structural defects (header mismatches), `a19` catches semantic ones (self-contradictions). Most clean changes pass both with no LLM cost beyond the contradiction check's own.

#### Scenario: Default-disabled produces no LLM call
- **WHEN** `executor.change_internal_contradiction_check` is unset (default `disabled`)
- **AND** any change reaches the pre-executor pipeline
- **THEN** no LLM call is made for the contradiction check
- **AND** the executor is invoked normally (assuming `a17`'s archivability check passed)

#### Scenario: Enabled mode invokes the LLM with the change's deltas
- **WHEN** `executor.change_internal_contradiction_check: enabled` AND the LLM config is set
- **AND** a change passes `a17`'s archivability check
- **THEN** the pipeline invokes the configured LLM with the embedded `prompts/change-contradiction-check.md` prompt + the change's concatenated spec-delta files
- **AND** parses the response as JSON conforming to `{ contradictions: [{ requirement_a, requirement_b, summary }] }`

#### Scenario: Empty contradictions array proceeds to executor
- **WHEN** the LLM returns `{"contradictions": []}`
- **THEN** the pipeline proceeds to the executor
- **AND** no marker is written
- **AND** no chatops alert fires

#### Scenario: Non-empty contradictions array writes marker and skips executor
- **WHEN** the LLM returns one or more contradictions
- **THEN** the pipeline writes `.needs-spec-revision.json` with `revision_suggestion` text populated from the contradictions narrative (per the documented format)
- **AND** the marker's `unarchivable_deltas` AND `unimplementable_tasks` arrays are empty (this case is semantic, not structural)
- **AND** the chatops alert under `AlertCategory::SpecNeedsRevision` fires (subject to the 24h throttle)
- **AND** the executor is NOT invoked for this change OR any subsequent change in this iteration

#### Scenario: LLM call failure fails open
- **WHEN** the LLM call returns Err (network, rate-limit, transport)
- **THEN** the pipeline logs a WARN naming the error
- **AND** treats the check as "no contradictions found"
- **AND** proceeds to the executor
- **AND** the daemon does NOT gate iteration progress on the failed check

#### Scenario: Malformed LLM response fails open
- **WHEN** the LLM returns a response that doesn't parse as the expected JSON shape
- **THEN** the pipeline logs a WARN naming the response excerpt (truncated to 200 chars)
- **AND** proceeds to the executor (same fail-open posture)

#### Scenario: Enabled without LLM config fails fast at startup
- **WHEN** `config.yaml` sets `executor.change_internal_contradiction_check: enabled`
- **AND** `executor.change_internal_contradiction_check_llm` is unset
- **THEN** daemon startup fails with the error `executor.change_internal_contradiction_check is enabled but executor.change_internal_contradiction_check_llm is not configured`
- **AND** the daemon does NOT begin polling
- **AND** the operator sees the error message on stderr AND in journalctl

#### Scenario: Prompt override replaces the embedded default
- **WHEN** `executor.change_internal_contradiction_check_prompt_path` points to an override file
- **THEN** the pipeline reads the override file AND uses its contents as the prompt template
- **AND** an empty override file produces an error at use time (the daemon does not feed an empty prompt to the LLM)

#### Scenario: Marker `revision_suggestion` enumerates findings clearly
- **WHEN** the LLM returns 2 contradictions
- **THEN** the marker's `revision_suggestion` text contains both findings numbered 1 AND 2, each with `requirement_a`, `requirement_b`, AND `summary` fields
- **AND** the text ends with operator guidance (`Edit the conflicting requirements... clear via @<bot> clear-revision`)

#### Scenario: Operator clearing the marker without spec edits is permitted
- **WHEN** the operator assesses the LLM's findings as a false positive AND runs `@<bot> clear-revision <repo> <change>` without editing the spec
- **THEN** the next polling iteration retries the change AND re-runs the contradiction check
- **AND** the operator's tolerance for false positives shapes their decision to enable the check OR keep it disabled

### Requirement: Binary version string is derived from `git describe` at build time
The autocoder binary SHALL embed a version string at build time via a `build.rs` script that runs `git describe --tags --always --dirty` AND exposes the output as `env!("AUTOCODER_VERSION")`. The build script SHALL fall back to `env!("CARGO_PKG_VERSION")` (Cargo.toml's `version =` field) when `git describe` cannot run OR returns empty — typical of tarball builds without `.git/` AND of `cargo install` from crates.io. The fallback chain SHALL ALWAYS produce a non-empty string; the build SHALL NEVER fail because of version-string resolution. The `build.rs` SHALL register `.git/HEAD`, `.git/index`, AND `.git/refs/tags` as rerun-if-changed inputs so dev builds reflect the working commit.

Every operator-facing version-string surface in the autocoder binary SHALL read `env!("AUTOCODER_VERSION")`, NOT `env!("CARGO_PKG_VERSION")`. Surfaces include: the `🆙` startup notification (per the modified requirement above), `autocoder --version` (clap's `#[command(version = ...)]` override), AND any future log lines OR PR-body footers that surface version.

The Cargo.toml `version =` field SHALL be operator-bumped only at semver-meaningful releases (major / minor / patch). Per-commit AND per-tag version bumps are NOT required — `git describe` provides the delta-past-tag info automatically.

#### Scenario: Build at a clean tag commit produces the tag string verbatim
- **WHEN** the daemon is built from a commit that has a `vX.Y.Z` tag pointing directly at it AND the working tree is clean
- **THEN** `git describe --tags --always --dirty` returns `vX.Y.Z` (no suffix)
- **AND** `env!("AUTOCODER_VERSION")` resolves to `vX.Y.Z`
- **AND** `autocoder --version` outputs `vX.Y.Z`

#### Scenario: Build past a tag produces the tag + commit-count + SHA
- **WHEN** the daemon is built from a commit that is N commits past the most recent tag `vX.Y.Z` AND the working tree is clean
- **THEN** `git describe` returns `vX.Y.Z-N-g<short-sha>` (e.g. `v1.1.1-23-g4abc123`)
- **AND** `env!("AUTOCODER_VERSION")` resolves to that string
- **AND** the `🆙` startup notification AND `autocoder --version` both show the development-build format

#### Scenario: Build with uncommitted local changes adds `-dirty` suffix
- **WHEN** the daemon is built from a commit AND the working tree has uncommitted modifications to tracked files
- **THEN** `git describe --tags --always --dirty` appends `-dirty` to the output
- **AND** the operator-visible version string AND `🆙` notification surface the `-dirty` suffix
- **AND** operators see clearly that the running binary was built from an in-progress local state

#### Scenario: Build with no `.git/` falls back to Cargo.toml
- **WHEN** the daemon is built from a source location with NO `.git/` directory (e.g., `cargo install autocoder` from crates.io, OR an unpacked source tarball)
- **THEN** `git describe` fails OR returns empty
- **AND** `env!("AUTOCODER_VERSION")` resolves to `env!("CARGO_PKG_VERSION")` (Cargo.toml's version)
- **AND** the build still succeeds
- **AND** the operator-visible version string is the Cargo.toml version verbatim

#### Scenario: Build with no `git` binary on PATH falls back to Cargo.toml
- **WHEN** the daemon is built on a host where the `git` binary is not on PATH
- **THEN** the build script's `Command::new("git")` fails to spawn
- **AND** the fallback to `env!("CARGO_PKG_VERSION")` kicks in
- **AND** the build still succeeds

#### Scenario: build.rs rerun inputs catch dev-build commit changes
- **WHEN** a developer makes a commit AND runs `cargo build` without modifying any source file
- **THEN** cargo re-runs `build.rs` because `.git/HEAD` (or `.git/index`) changed
- **AND** `env!("AUTOCODER_VERSION")` reflects the new commit's `git describe` output

#### Scenario: Clap `--version` override produces the embedded string
- **WHEN** an operator runs `autocoder --version`
- **THEN** the output is the `env!("AUTOCODER_VERSION")` value verbatim (with clap's standard `<binary-name> <version>` formatting)
- **AND** the output is NOT the Cargo.toml `version =` field (unless the fallback path fired at build time)

#### Scenario: Binary-release builds embed clean tag strings
- **WHEN** the GitHub Actions release workflow builds the daemon from a commit that has a `vX.Y.Z` tag (the workflow runs against the tagged commit)
- **THEN** the embedded version string is `vX.Y.Z` (no `-N-gSHA` suffix; no `-dirty` suffix)
- **AND** operators installing via `update.sh` see clean semver versions in their `🆙` notifications AND `--version` output

### Requirement: Documentation audit reports coverage, stale-reference, and organization findings
autocoder SHALL register a `documentation_audit` audit type in the periodic-audit framework. The audit is LLM-driven, declares `WritePolicy::None`, `requires_head_change = true`, AND a sandbox profile allowing `Read`, `Glob`, `Grep`, AND `Bash` (read-only). It produces `AuditOutcome::Reported(findings)` covering three categories of documentation defect:

1. **Coverage** — code or canonical-spec features that user-facing docs (`README.md`, `docs/*.md`) don't mention. Heuristic: any canonical-spec requirement whose body mentions operator-visible artifacts (`@<bot>` verbs, config keys, CLI flags, file paths the operator interacts with) is in scope. Pure-internal capabilities are NOT flagged.
2. **Stale references** — docs references to code symbols (function names in code blocks, CLI verbs, config fields, file paths under `src/`) that don't exist in the current code or canonical specs. Catches dead references from removed features.
3. **Organization** — qualitative structural findings: README exceeding `extra.readme_max_lines` lines (default `200`), docs pages exceeding `extra.page_max_lines_without_toc` (default `500`) without a TOC, important user-visible features buried below setup/admin material on their page, two docs pages covering the same topic without cross-linking, capabilities surfaced only in CHANGELOG but never in operator docs.

The audit's findings SHALL be tagged with `severity` of `low` OR `medium` ONLY — the audit deliberately does NOT emit `high` (documentation drift is rarely emergency-grade; promotion would crowd out genuinely-urgent audit signals from other types). An `anchor` field names `<file>:<line>` for stale-reference findings AND `<file>` (no line) for coverage AND organization findings.

The audit's prompt template `prompts/documentation-audit.md` ships embedded via `include_str!` AND is overridable via `audits.settings.documentation_audit.prompt_path`. Two `extra` knobs apply: `readme_max_lines` (default `200`) AND `page_max_lines_without_toc` (default `500`). The prompt receives these knobs as part of its input AND respects them when emitting organization findings.

The audit does NOT produce LLM-generated documentation proposals (unlike `missing_tests_audit` / `security_bug_audit`). Findings ship as `Reported` outcomes; operators run `@<bot> send it` in the audit's threaded notification to trigger a triage executor run that produces a docs-fix PR (NOT a spec PR). The PR participates in the standard `@<bot> revise <text>` revision loop.

When `a21`'s canonical-spec RAG is enabled in the same workspace, the audit's prompt MAY use the `query_canonical_specs` MCP tool to fetch focused canonical context. The audit functions correctly without RAG too; the RAG integration is an opportunistic enhancement, not a requirement.

#### Scenario: Audit detects implementation-without-documentation
- **WHEN** the canonical spec contains a requirement whose body mentions an operator-visible feature (e.g. `@<bot> propose` verb)
- **AND** none of `README.md` or `docs/*.md` mentions `propose`
- **THEN** the audit emits a finding with `category: coverage`, `severity: medium`, `anchor: <docs-or-spec-file-where-the-feature-is-defined>`, AND a body explaining the missing documentation

#### Scenario: Audit detects documentation-without-implementation
- **WHEN** `docs/CONFIG.md` references a config field `executor.foo_bar_quux` in a code block
- **AND** no Rust source file under `<workspace>/<source-tree>/` defines a field named `foo_bar_quux` in any struct
- **THEN** the audit emits a finding with `category: stale_reference`, `severity: medium`, `anchor: docs/CONFIG.md:<line>`, AND a body naming the missing referent

#### Scenario: Audit detects organization issues
- **WHEN** `docs/CHATOPS.md` is 600 lines long AND has no top-of-file TOC
- **AND** the page documents user-driving workflows (`propose`, `send it`) AND administrative recovery verbs (`clear-perma-stuck`)
- **AND** the user-driving content appears below the admin material
- **THEN** the audit MAY emit findings with `category: organization`, `severity: low` or `medium`, naming each separately (missing TOC; burial of user-driving content)

#### Scenario: Audit deliberately does not emit `high` severity
- **WHEN** the LLM's response contains a finding marked `"severity": "high"`
- **THEN** the audit demotes it to `"medium"` AND logs a WARN naming the demotion
- **AND** the operator-visible finding lists severity `medium`

#### Scenario: Audit honors `requires_head_change = true`
- **WHEN** the audit's `last_run_sha` equals the current base-branch HEAD AND the cadence has elapsed
- **THEN** the framework skips the audit (per the existing framework requirement)
- **AND** the next iteration after a HEAD change re-evaluates

#### Scenario: Pure-internal capability is NOT flagged for coverage
- **WHEN** a capability's canonical spec exists BUT every requirement body covers pure-internal mechanics (no operator-visible artifacts)
- **THEN** the audit does NOT emit a coverage finding for that capability
- **AND** the heuristic recognizes "internal" via the absence of `@<bot>` verbs, config keys, CLI flags, AND operator-facing file paths in the requirement bodies

#### Scenario: `extra` knobs apply to organization thresholds
- **WHEN** `audits.settings.documentation_audit.extra.readme_max_lines: 400`
- **AND** `README.md` is 300 lines
- **THEN** the audit does NOT emit a "README too long" finding (the threshold is operator-raised)
- **WHEN** the same config AND `README.md` grows to 500 lines
- **THEN** the audit emits the organization finding

#### Scenario: Audit works without `a21`'s RAG
- **WHEN** `canonical_rag` is disabled (no block OR `enabled: false`)
- **AND** `documentation_audit` runs
- **THEN** the audit completes successfully without invoking `query_canonical_specs`
- **AND** findings are emitted based on the prompt's direct access to canonical specs (read via the sandbox's `Read` tool)

#### Scenario: Audit uses RAG when available
- **WHEN** `canonical_rag` is enabled AND a documentation_audit run starts
- **THEN** the audit's executor invocation has access to `query_canonical_specs` via MCP
- **AND** the prompt MAY direct the LLM to use the tool for canonical-context retrieval
- **AND** the implementation detail (whether the LLM uses the tool) is left to the prompt's design — both with-RAG AND without-RAG produce valid output

#### Scenario: Findings can be acted on via `send it`
- **WHEN** the audit posts a threaded notification with findings AND the operator replies `@<bot> send it` in that thread
- **THEN** the existing `audit-reply-acts` mechanism triggers a triage executor run
- **AND** the triage produces a doc-fix PR (changes to `README.md` / `docs/*.md` files)
- **AND** the triage does NOT produce a spec PR (documentation is not OpenSpec material)
- **AND** the doc-fix PR participates in the standard `@<bot> revise <text>` revision loop

### Requirement: `brownfield` chatops verb queues a brownfield-draft executor request
The chatops listener SHALL submit a `BrownfieldAction` (per the chatops-manager requirement) which the daemon's control-socket handler converts into an entry on the resolved repo's `pending_brownfield_requests: VecDeque<RequestId>` queue. The daemon SHALL persist a per-request state file `<workspace>/.state/brownfield_requests/<request_id>.json` containing the request's `repo_url`, `capability_name`, `guidance: Option<String>`, `channel`, `thread_ts`, AND `status` (`Pending` | `InProgress` | `Acted` | `Failed` | `Aborted`).

Each polling iteration SHALL, after processing pending proposal requests AND before the standard change-processing pass, drain at most one brownfield request from the queue.

#### Scenario: Queue stores requests in submission order
- **WHEN** the operator posts two brownfield requests in sequence (`brownfield repo a`, `brownfield repo b`)
- **THEN** `pending_brownfield_requests` contains both request_ids in submission order
- **AND** the polling iteration drains them one per iteration

#### Scenario: State file persists across daemon restart
- **WHEN** a `BrownfieldRequestState` file exists with `status: Pending` AND the daemon restarts
- **THEN** the daemon's startup reads the file AND re-queues the request
- **AND** processing resumes on the next iteration

#### Scenario: Late conflict aborts the request
- **WHEN** a brownfield request reaches the polling iteration AND `openspec/specs/<capability-name>/spec.md` exists at the current workspace HEAD (created by a merge between dispatch AND processing)
- **THEN** the iteration posts a thread reply `✗ brownfield: openspec/specs/<capability-name>/spec.md now exists (created since the request was queued). Aborting.`
- **AND** the state's `status` becomes `Aborted`
- **AND** no executor invocation occurs

### Requirement: Brownfield-draft executor mode produces a spec-only change PR
When the polling iteration processes a brownfield request, it SHALL invoke the executor with `WritePolicy::OpenSpecOnly` AND a sandbox profile permitting `Read`, `Glob`, `Grep`, AND `Bash` (read-only). The executor's prompt SHALL be assembled from:

1. The embedded default template at `prompts/brownfield-draft.md` (via `include_str!`), OR the template at `features.brownfield.prompt_path` when configured AND the file exists.
2. The operator's guidance (when non-empty), interpolated into a `## Operator guidance` section of the prompt.
3. The capability name, the workspace's `README.md` contents, the list of `docs/*.md` filenames, AND a code-symbol overview built via `cargo metadata` (for Rust workspaces) OR a ripgrep pass for top-level public items (other languages).

On executor `Completed`, the iteration SHALL verify the change directory `openspec/changes/brownfield-<capability-name>/` contains `proposal.md`, `tasks.md`, AND `specs/<capability-name>/spec.md`. The iteration SHALL ALSO verify `git status --porcelain` shows no modifications outside `openspec/`; any such modification triggers `git reset --hard HEAD; git clean -fd`, a WARN log naming the leaked paths, AND a state transition to `Failed`.

On verification success, the iteration SHALL create a spec branch (NOT a fixes branch — brownfield never modifies source code), push, AND open a PR. The PR body SHALL include the proposal's "Why" section. The iteration SHALL post `✅ Brownfield draft PR opened: <pr_url>` to the request's thread AND set the state's `status` to `Acted` with the PR URL recorded.

On executor `Err` OR missing artifacts, the iteration SHALL post `✗ Brownfield draft failed: <reason>` to the request's thread, log the full error to the daemon log, revert the workspace, AND set the state's `status` to `Failed`.

#### Scenario: Successful run produces a spec-only PR
- **WHEN** the executor returns `Completed` AND `openspec/changes/brownfield-<cap>/` contains all required artifacts AND no source-file modifications leaked
- **THEN** the daemon creates a spec branch `<configured-prefix>brownfield/<cap>`, pushes, AND opens a PR
- **AND** the PR body contains the proposal's "Why" section
- **AND** the state's `status` is `Acted` with the PR URL
- **AND** the thread receives `✅ Brownfield draft PR opened: <pr_url>`
- **AND** NO fixes branch OR fixes PR is created

#### Scenario: Sandbox leak triggers cleanup
- **WHEN** the executor returns `Completed` AND `git status --porcelain` shows modifications under `src/` (in addition to `openspec/`)
- **THEN** the iteration reverts the workspace via `git reset --hard HEAD; git clean -fd`
- **AND** a WARN log fires naming the leaked paths
- **AND** the state's `status` is `Failed`
- **AND** the thread reply names the sandbox violation

#### Scenario: Missing change-directory artifacts produce a clear failure
- **WHEN** the executor returns `Completed` BUT `openspec/changes/brownfield-<cap>/specs/<cap>/spec.md` is absent
- **THEN** the state's `status` is `Failed`
- **AND** the thread reply names the missing artifact
- **AND** the workspace is reverted

#### Scenario: Operator guidance reaches the prompt
- **WHEN** the operator's request includes guidance `focus on the cron-trigger lifecycle; skip telemetry hooks`
- **THEN** the executor invocation's prompt contains a `## Operator guidance` section with the verbatim guidance text
- **AND** the LLM's draft scopes its requirements accordingly

#### Scenario: Per-workspace prompt override applies
- **WHEN** `features.brownfield.prompt_path: ./prompts/brownfield-custom.md` AND the file exists in the workspace
- **THEN** the iteration loads the custom template AND uses it instead of the embedded default
- **AND** the loaded template combines with the operator's guidance + the gathered inputs into the executor's prompt

#### Scenario: Missing override file falls back to embedded
- **WHEN** `features.brownfield.prompt_path: ./prompts/brownfield-custom.md` is configured BUT the file does not exist
- **THEN** the iteration logs a WARN naming the missing path
- **AND** the iteration falls back to the embedded default template
- **AND** the request proceeds successfully

#### Scenario: PR participates in standard revision loop
- **WHEN** the brownfield PR is open AND an operator comments `@<bot> revise add a requirement covering retry semantics` on it
- **THEN** the existing PR-comment revision-loop mechanism handles the comment
- **AND** the next polling iteration revises the spec PR per the operator's text

### Requirement: `features.brownfield` config schema
The daemon's per-repo config schema SHALL accept an optional top-level `features` block containing a `brownfield` sub-block with:

- `enabled: bool` (default `true`) — when `false`, the dispatcher refuses the verb at parse time.
- `prompt_path: Option<String>` (default `None`) — operator-supplied path (relative to workspace root) to a custom brownfield-draft prompt template.

Both fields are optional; absent fields take their defaults. Invalid values (non-boolean `enabled`, non-string `prompt_path`) cause config-load to fail-fast with a clear error naming the offending field.

#### Scenario: Default config enables brownfield
- **WHEN** a workspace's config omits the `features.brownfield` block
- **THEN** `features.brownfield.enabled` resolves to `true`
- **AND** `features.brownfield.prompt_path` resolves to `None`

#### Scenario: Explicit disable refuses the verb
- **WHEN** a workspace's config sets `features.brownfield.enabled: false`
- **THEN** the dispatcher refuses `@<bot> brownfield ...` requests for that workspace per the chatops-manager requirement

#### Scenario: Explicit override path resolves
- **WHEN** a workspace's config sets `features.brownfield.prompt_path: "./prompts/brownfield-custom.md"`
- **THEN** the polling iteration loads the file at that workspace-relative path
- **AND** uses its contents as the brownfield-draft prompt template

#### Scenario: Invalid field type fails config load
- **WHEN** a workspace's config sets `features.brownfield.enabled: "yes"` (string instead of bool)
- **THEN** config-load fails with an error naming `features.brownfield.enabled` AND the expected type

### Requirement: SpecNeedsRevision parser detects un-substituted placeholders AND surfaces a clear failure mode
The Claude CLI executor's `SpecNeedsRevision` sentinel parser SHALL, after a successful `serde_json::from_str` deserialization, scan each `task_id`, `task_text`, AND `reason` field's string value for the regex `<[a-z][a-z0-9 _-]*>`. When ANY field matches, the parser SHALL treat the sentinel as malformed (the "placeholder failure mode") AND fall through to the same Failed-outcome path the canonical "Malformed outcome sentinel falls back to Failed" scenario describes — with one refinement: the WARN log line AND the `Failed { reason }` string SHALL include the diagnostic phrase:

```
looks like un-substituted placeholders — the agent emitted the prompt's example verbatim instead of substituting concrete values; see prompts/implementer.md sentinel section
```

This refinement narrows the existing catch-all Failed-outcome message. The intent: when an operator inspects a Failed iteration's logs, they immediately know whether the failure is "agent emitted garbage JSON" (the original case) OR "agent emitted the prompt's example without filling in values" (the new placeholder-detection case). The two failure modes have very different operator responses — the first usually means the agent is confused about format; the second means the prompt template OR the operator's prompt override has regressed.

The detection regex is intentionally narrow (lowercase letters, digits, spaces, underscores, hyphens between the angle brackets) to avoid matching legitimate `<...>` text that might appear in task descriptions — e.g., a task body that names a chatops verb syntax like `@<bot>` OR a docs reference like `<repo-substring>`. False positives in this narrow sense ARE possible (a legitimate task whose text happens to include lowercase angle-bracket content); the regex SHALL be treated as a heuristic. The diagnostic phrase is helpful to the operator either way: if it's a true positive (prompt regression), the diagnostic points at the prompt; if it's a false positive (a real task with `<thing>` in its text), the operator's resolution is the same (review the agent's output AND the task text together).

This requirement is additive to the canonical "Malformed outcome sentinel falls back to Failed" scenario. That scenario still fires for any other parse failure (JSON syntax error, missing required field, unknown `type` value, empty `unimplementable_tasks` list, etc.); placeholder-detection adds a more specific diagnostic for one narrow case.

#### Scenario: Placeholder in task_id triggers the detection
- **WHEN** the agent emits a sentinel whose `task_id` field has the value `<id-from-tasks-md>` (literal angle-bracket content matching the regex)
- **AND** the sentinel otherwise deserializes successfully
- **THEN** the parser treats it as malformed AND falls through to the Failed-outcome path
- **AND** the WARN log line names `PromptId::Implementer` (OR the override path) AND the diagnostic phrase `looks like un-substituted placeholders — the agent emitted the prompt's example verbatim instead of substituting concrete values; see prompts/implementer.md sentinel section`
- **AND** the `Failed { reason }` string contains the same diagnostic phrase
- **AND** the polling loop's existing Failed-outcome handling kicks in (perma-stuck counter increments, no marker written)

#### Scenario: Placeholder in task_text triggers the detection
- **WHEN** the agent emits a sentinel whose `task_text` field has the value `<verbatim quote>`
- **THEN** the same placeholder-detection path fires as for task_id

#### Scenario: Placeholder in reason triggers the detection
- **WHEN** the agent emits a sentinel whose `reason` field has the value `<one-line why>`
- **THEN** the same placeholder-detection path fires

#### Scenario: Well-formed sentinel is unaffected
- **WHEN** the agent emits a sentinel with substituted values (task_id `6.4`, task_text `Run sudo systemctl restart nginx on the production host`, reason `executor sandbox has no sudo access on the production host`)
- **THEN** the parser proceeds with the normal `SpecNeedsRevision` outcome
- **AND** placeholder detection does NOT fire
- **AND** the polling loop writes the `.needs-spec-revision.json` marker AND posts the chatops alert per the canonical "autocoder writes the marker and alerts" scenario

#### Scenario: Narrow regex tolerates legitimate angle-bracket text
- **WHEN** the agent emits a sentinel whose `task_text` is `Document the @<bot> verb in docs/CHATOPS.md`
- **AND** the substring `@<bot>` matches the regex `<[a-z][a-z0-9 _-]*>` only at the inner `<bot>` portion
- **THEN** placeholder detection DOES fire (the `<bot>` portion matches the regex)
- **AND** the operator's resolution is to review the agent's output: if the task text genuinely needs `<bot>` AND the sentinel is otherwise correct, the operator clears the perma-stuck AND can comment on the task text to disambiguate; if the sentinel is a placeholder regression, the operator follows the diagnostic
- **AND** false positives are accepted as a tradeoff for the heuristic's narrow scope (we prefer over-flagging to under-flagging on this rare case)

#### Scenario: Existing malformed-sentinel path remains for non-placeholder failures
- **WHEN** the agent emits a payload that fails `serde_json::from_str` (e.g., malformed JSON, missing `type` field, empty `unimplementable_tasks` list)
- **THEN** the canonical "Malformed outcome sentinel falls back to Failed" scenario fires with its existing WARN text (`agent emitted unparseable SpecNeedsRevision sentinel: <excerpt>`)
- **AND** the placeholder-detection diagnostic does NOT appear (the new diagnostic is reserved for the deserialize-success-but-contains-placeholder case)

### Requirement: Canonical-spec RAG configuration and pipeline

autocoder SHALL support a per-workspace retrieval-augmented-context pipeline that embeds the workspace's canonical OpenSpec specs (`openspec/specs/<capability>/spec.md`) into an in-memory vector store AND exposes a retrieval surface for the implementer (via `a21`'s executor MCP requirement) AND for downstream pre-flight checks (`a22`'s change-vs-canon contradiction check). The pipeline is configured via a top-level `canonical_rag:` block in `config.yaml`; an absent block disables the feature entirely. A present block with `enabled: false` also disables; both forms preserve "no behavior change" for operators who don't opt in.

The `canonical_rag:` config block contains: `enabled: bool`, `provider: LlmProvider` (subsystem-valid subset: `ollama | openai_compatible`; `anthropic` is rejected at config-load per the per-subsystem provider-validity requirement), `model: string`, `api_base_url: string` (required for both valid providers), `api_key_env: string?` AND `api_key: SecretSource?` (mutually exclusive — inline wins with WARN if both set; same pattern as `reviewer:`; FORBIDDEN entirely when `provider: ollama` per the per-provider auth-semantics requirement), `top_k: usize` (default `10`, clamped `[1, 100]` with WARN), `chunk_strategy: per_requirement | per_scenario | per_capability` (default `per_requirement`), AND `reembed_on_archive: bool` (default `true`).

The embedding pipeline SHALL:
- Build an `EmbedClient` from the provider config — an Ollama adapter calling `<base_url>/api/embed` for Ollama, OR an OpenAI-compatible adapter calling `<base_url>/embeddings` with `Authorization: Bearer <api_key>` for the openai_compatible path.
- Glob `<workspace>/openspec/specs/<cap>/spec.md` files, chunk each per `chunk_strategy`, embed each chunk via the client, AND store `(chunk, embedding, source_path, capability, requirement_title)` tuples in an in-memory `CanonicalRagStore`.
- Maintain a per-workspace store registry keyed by sanitized workspace basename. Multiple managed repos each have their own store; the stores are independent.
- Persist NOTHING to disk. Daemon restart re-embeds from scratch on workspace-init.

Failure modes are fail-open: embedding-provider errors (network, auth, rate-limit) at init log WARN AND omit the workspace's store from the registry. Subsequent queries against the absent store return empty Vec with a structured error hint. The daemon does NOT gate iteration progress on RAG availability; the implementer's non-RAG fallback behavior remains correct.

The `Anthropic` arm of the embedding dispatch SHALL exist as a defensive backstop returning `Err(anyhow!("anthropic does not support embeddings; configure canonical_rag.provider as ollama or openai_compatible"))`. In normal operation this is unreachable (config-load rejects `anthropic` for RAG); the backstop exists in case the validation is bypassed by a future code change.

#### Scenario: Absent `canonical_rag:` block disables the feature
- **WHEN** `config.yaml` does NOT contain a `canonical_rag:` top-level block
- **THEN** the daemon's workspace-init step skips the RAG pipeline entirely
- **AND** no `CanonicalRagStore` is registered for any workspace
- **AND** the implementer's MCP tool `query_canonical_specs` returns empty Vec (per the executor spec) with the error hint `rag disabled in config`
- **AND** no embedding-provider HTTP calls are issued at any point

#### Scenario: Present block with `enabled: false` is also disabled
- **WHEN** `config.yaml` contains `canonical_rag: { enabled: false, provider: ollama, model: nomic-embed-text, api_base_url: http://localhost:11434 }`
- **THEN** behavior is identical to absent block (no embed calls, empty tool results)
- **AND** the config is preserved so operators can flip `enabled: true` without re-entering field values

#### Scenario: Ollama provider embeds via the `/api/embed` endpoint
- **WHEN** `canonical_rag.provider: ollama` AND the daemon's workspace-init step runs
- **THEN** the daemon POSTs to `<api_base_url>/api/embed` with `{"model": "<model>", "input": [<chunk1>, <chunk2>, ...]}` for batches of up to 32 chunks
- **AND** parses the Ollama embedding response format into `Vec<Vec<f32>>`
- **AND** stores the resulting embeddings paired with their chunk metadata

#### Scenario: OpenAI-compatible provider embeds via `/embeddings`
- **WHEN** `canonical_rag.provider: openai_compatible` AND the daemon's workspace-init step runs
- **THEN** the daemon POSTs to `<api_base_url>/embeddings` with `{"model": "<model>", "input": [...]}` AND header `Authorization: Bearer <resolved-api-key>`
- **AND** parses the OpenAI embeddings response format
- **AND** the resolved API key comes from `canonical_rag.api_key.value` (inline) OR `std::env::var(canonical_rag.api_key_env)` (env-var path); inline wins if both are set with a WARN log

#### Scenario: Per-workspace store registry
- **WHEN** the daemon manages two repositories AND RAG is enabled for both
- **THEN** the registry contains two distinct `CanonicalRagStore` instances, one per workspace
- **AND** a `query_canonical_specs` call routes to the store matching the calling workspace's basename
- **AND** the stores are independent — embeds from one workspace's specs never surface in the other's results

#### Scenario: Provider failure at init fails open
- **WHEN** `canonical_rag.provider: ollama` AND `api_base_url` points at an unreachable host
- **THEN** the workspace-init RAG step logs a WARN naming the error
- **AND** the workspace's store is NOT registered in the registry
- **AND** subsequent `query_canonical_specs` calls return empty Vec with `error_hint: "rag init failed; see daemon log"`
- **AND** the polling iteration proceeds normally (no gate on RAG availability)
- **AND** subsequent iterations retry the init (no permanent-skip)

#### Scenario: `top_k` is clamped at startup
- **WHEN** `canonical_rag.top_k: 500`
- **THEN** the resolved value is `100` (the max)
- **AND** a WARN log at startup names both the requested AND clamped values

#### Scenario: `api_key` and `api_key_env` mutually exclusive
- **WHEN** both `canonical_rag.api_key.value` AND `canonical_rag.api_key_env` are set
- **THEN** the inline value wins
- **AND** a WARN log at startup names that the env var is being ignored

#### Scenario: `canonical_rag.provider: anthropic` rejected at config-load
- **WHEN** `config.yaml` contains `canonical_rag: { enabled: true, provider: anthropic, model: <m>, api_base_url: <u> }`
- **THEN** config-load fails with `canonical_rag does not support provider 'anthropic'; available providers: ollama, openai_compatible`
- **AND** the daemon exits non-zero before any polling task is spawned

#### Scenario: `canonical_rag.provider: ollama` with `api_key` rejected at config-load
- **WHEN** `config.yaml` contains `canonical_rag: { enabled: true, provider: ollama, model: <m>, api_base_url: <u>, api_key: { value: "anything" } }`
- **THEN** config-load fails with `canonical_rag: ollama does not authenticate; remove api_key field`
- **AND** the daemon exits non-zero

### Requirement: RAG re-embed cadence (workspace init and post-archive)
The RAG pipeline SHALL re-embed canonical specs at two events ONLY:

1. **Workspace init** — the first iteration of a workspace after daemon start (OR after a workspace wipe). The full canonical corpus is embedded synchronously before the iteration's executor invocation.
2. **Post-archive** (when `canonical_rag.reembed_on_archive: true`, default) — after any iteration's archive step that modifies at least one `<workspace>/openspec/specs/<cap>/spec.md` file. ONLY the affected capabilities' embeds are rebuilt, not the entire corpus.

Detection of "archive touched canonical": after the archive commit lands, run `git diff --name-only HEAD~N HEAD -- openspec/specs/` where N is the number of newly-archived commits in this iteration. Each unique `<cap>` directory present in the diff is a capability whose store entries SHALL be rebuilt.

Re-embed failures are fail-open: a failed rebuild leaves the existing embeds in place AND logs a WARN. The store may be temporarily stale; the next archive that touches the same capability OR a daemon restart will refresh it.

#### Scenario: Cold start embeds the full corpus
- **WHEN** the daemon starts up against a workspace that has not been embedded before
- **AND** `canonical_rag.enabled: true`
- **THEN** the workspace-init step embeds every `<workspace>/openspec/specs/<cap>/spec.md` file
- **AND** the log records `canonical RAG embedded N chunks across M capabilities for workspace <basename>`
- **AND** the executor's first invocation has access to the populated store

#### Scenario: Archive touching canonical re-embeds affected capabilities
- **WHEN** an iteration's archive step commits a change that modifies `<workspace>/openspec/specs/code-reviewer/spec.md`
- **AND** `canonical_rag.reembed_on_archive: true` (the default)
- **THEN** the post-archive RAG step computes the affected capabilities via `git diff --name-only` against the iteration's commits
- **AND** calls `rebuild_capabilities` for `["code-reviewer"]`
- **AND** existing entries for other capabilities are unchanged
- **AND** the log records `canonical RAG re-embedded 1 capability (code-reviewer) after archive`

#### Scenario: Archive NOT touching canonical does not re-embed
- **WHEN** an iteration archives changes whose deltas include implementation files AND `tasks.md` updates but NO `openspec/specs/<cap>/spec.md` modifications
- **THEN** the post-archive RAG step computes affected capabilities AND finds none
- **AND** no rebuild happens
- **AND** the log records no re-embed activity

#### Scenario: `reembed_on_archive: false` disables post-archive rebuilds
- **WHEN** `canonical_rag.reembed_on_archive: false`
- **THEN** post-archive re-embeds are suppressed entirely
- **AND** stores become stale across canonical-changing archives
- **AND** operators can manually trigger a rebuild via daemon restart OR a future explicit verb (not in this spec)

#### Scenario: Re-embed failure leaves prior embeds intact
- **WHEN** a post-archive rebuild attempt fails (provider unreachable, network blip)
- **THEN** the prior embeds for the affected capabilities are retained in the store
- **AND** a WARN log records the failure naming the capabilities AND the error
- **AND** queries continue to return chunks from the pre-rebuild embeds (stale-but-usable)

#### Scenario: Daemon restart re-embeds from scratch
- **WHEN** the daemon is stopped AND restarted later
- **THEN** the in-memory store is empty at startup (no on-disk persistence)
- **AND** workspace-init re-runs the full embedding pipeline for every configured workspace
- **AND** the cost is `O(N capabilities × M chunks × embed-call-latency)` — typically sub-second on GPU, ~30 seconds on CPU for a typical corpus

### Requirement: Install-wizard graduated RAG-configuration flow
`autocoder install` (interactive mode) SHALL prompt the operator about RAG configuration AND walk them through a graduated set of options designed to find a working RAG setup for their environment without requiring API keys when avoidable. The flow:

1. Prompt: `Configure canonical-specs RAG? (Y/n)`. Default Y. If N, write no `canonical_rag:` block AND continue with the rest of the wizard.
2. If Y, probe Ollama on localhost: HTTP GET `http://localhost:11434/api/tags` with a 2-second timeout.
3. If localhost Ollama is reachable: suggest using it. Prompt for `model` (default `nomic-embed-text` for the docker-default-compatible case; the wizard may suggest `qwen3-embedding:4b` if operator inputs indicate GPU availability — but the spec doesn't mandate GPU detection).
4. If localhost Ollama is NOT reachable, present a four-option menu:
   - **(1) Install local Ollama via docker** — wizard copies `install/ollama-docker-compose.yml` to `<config_dir>/ollama-docker-compose.yml` AND prints the `docker compose -f <path> up -d` command. The wizard does NOT auto-run docker. Writes the `canonical_rag:` block pointing at `http://localhost:11434` so the daemon connects once the operator starts docker.
   - **(2) Remote Ollama** — prompt for `base_url` + `model`. Probe `<base_url>/api/tags`. On success, write the config block. On probe failure, prompt the operator to retry OR fall back to one of the other options.
   - **(3) OpenAI-compatible endpoint** — prompt for `base_url`, `model`, AND api-key source (env var name OR inline value). Probe via a small embed call. Same retry-or-fallback semantics as option 2.
   - **(4) Disable RAG** — write no block; continue with the wizard. Print a one-liner about how to enable later (`canonical_rag:` block in config.yaml).
5. Non-interactive mode: accept flags `--rag-provider <ollama|openai_compatible|none>`, `--rag-base-url <url>`, `--rag-model <model>`, `--rag-api-key-env <name>`. Failing to provide flags with `--rag-provider ollama|openai_compatible` is a startup error.

The wizard's RAG step is testable via the existing `ScriptedIo` test harness with mocked HTTP probes.

#### Scenario: Localhost Ollama detected and chosen
- **WHEN** the wizard reaches the RAG prompt AND `http://localhost:11434/api/tags` returns 200 within 2 seconds
- **AND** the operator confirms `Y` AND accepts the default model
- **THEN** the wizard writes `canonical_rag: { enabled: true, provider: ollama, model: <chosen>, api_base_url: http://localhost:11434, top_k: 10 }` to the config
- **AND** the wizard does NOT install Ollama, pull models, OR spawn docker

#### Scenario: Localhost not detected; docker option chosen
- **WHEN** the localhost probe fails AND the operator picks option 1 (docker)
- **THEN** the wizard copies `install/ollama-docker-compose.yml` from the in-tree path to `<config_dir>/ollama-docker-compose.yml`
- **AND** writes the `canonical_rag:` block pointing at `http://localhost:11434`
- **AND** prints `docker compose -f <config_dir>/ollama-docker-compose.yml up -d` as the operator's explicit next step
- **AND** the wizard does NOT auto-run docker

#### Scenario: Remote Ollama option with successful probe
- **WHEN** the operator picks option 2 AND enters `base_url: http://gpu-host:11434` AND model `qwen3-embedding:4b`
- **AND** the probe of `http://gpu-host:11434/api/tags` succeeds
- **THEN** the wizard writes the corresponding `canonical_rag:` block
- **AND** the wizard does NOT need any further operator action; the daemon's workspace-init will connect on first iteration

#### Scenario: OpenAI-compatible option with probe failure
- **WHEN** the operator picks option 3 AND enters a `base_url` that's unreachable OR an invalid API key
- **THEN** the probe (a tiny embed call) fails AND the wizard reports the error
- **AND** prompts the operator to retry (correct the inputs) OR fall back to option 4 (disable)
- **AND** does NOT write a misconfigured block

#### Scenario: Disable option (4) writes no block
- **WHEN** the operator picks option 4 OR answers `n` at the initial Y/n prompt
- **THEN** no `canonical_rag:` block is written
- **AND** a one-liner explains how to enable RAG later by editing config.yaml AND running `autocoder reload`

#### Scenario: Non-interactive mode requires RAG flags when provider is set
- **WHEN** `autocoder install --non-interactive --rag-provider ollama` is invoked WITHOUT `--rag-base-url`
- **THEN** the install fails fast with an error naming the missing flag
- **AND** no config is written

### Requirement: Control socket exposes `query_canonical_specs` action
The daemon's control socket (per the canonical "Control socket for runtime daemon interaction" requirement) SHALL accept a `query_canonical_specs` action that lets per-execution MCP children retrieve ranked canonical-spec chunks for any query string. Request shape:

```json
{"action":"query_canonical_specs","workspace_basename":"<sanitized-basename>","query":"<text>","top_k":<optional-number>}
```

Required fields: `action`, `workspace_basename`, `query`. Optional: `top_k`. When `top_k` is omitted, the handler SHALL apply `canonical_rag.top_k` from the daemon's config (default 10) per the existing clamp rules.

Response shape:

```json
{"ok":true,"hits":[<RagHit JSON>...],"error_hint":"<optional message>"}
```

Each `RagHit` SHALL contain the fields documented in the executor spec delta (`capability`, `requirement_title`, `requirement_body`, `scenario_titles`, `relevance_score`). The handler SHALL be fail-open: any error condition (missing workspace registry entry, query-time provider error, RAG disabled, etc.) returns `ok: true` with an empty `hits` array AND a structured `error_hint` naming the cause. A `ok: false` response is reserved for protocol-level failures (malformed request, missing required field) per the canonical socket-protocol requirement.

The handler SHALL look up the workspace's `CanonicalRagStore` in the daemon's per-workspace registry by `workspace_basename`. The handler SHALL NOT trust client-supplied input for any purpose other than registry lookup — the basename is matched verbatim against the registry's keys; if no match, the handler returns `{"ok":true,"hits":[],"error_hint":"no workspace registered for that basename"}`.

#### Scenario: Happy-path query
- **WHEN** a client (typically the per-execution MCP child) sends `{"action":"query_canonical_specs","workspace_basename":"github_com_foo_bar","query":"audit cadence","top_k":5}`
- **AND** the daemon's registry has a `CanonicalRagStore` for `github_com_foo_bar`
- **THEN** the handler calls `store.query("audit cadence", Some(5))`
- **AND** returns `{"ok":true,"hits":[<up to 5 RagHits sorted by relevance>]}`
- **AND** the connection is closed after the single response

#### Scenario: Default top_k from config when omitted
- **WHEN** a request omits `top_k` AND `canonical_rag.top_k: 15` is configured
- **THEN** the handler calls `store.query(query, Some(15))` (OR equivalently passes the config default through)
- **AND** the response contains up to 15 hits

#### Scenario: Missing workspace in registry
- **WHEN** a request's `workspace_basename` does not match any registry entry (RAG disabled for that workspace, OR no such workspace is configured)
- **THEN** the handler returns `{"ok":true,"hits":[],"error_hint":"no workspace registered for that basename"}`
- **AND** no exception OR `ok: false` response is emitted

#### Scenario: RAG init failed earlier — registry has no entry
- **WHEN** RAG is configured for the workspace BUT the workspace-init embed pipeline failed (provider unreachable) AND the daemon did NOT register a store
- **AND** a request arrives for that workspace's basename
- **THEN** the handler returns `{"ok":true,"hits":[],"error_hint":"rag init failed; see daemon log"}`
- **AND** the daemon log contains the original init-failure WARN line

#### Scenario: Query-time provider error
- **WHEN** the request reaches the handler AND `store.query(...)` returns an error (e.g., provider unreachable mid-query)
- **THEN** the handler returns `{"ok":true,"hits":[],"error_hint":"query failed: <reason>"}`
- **AND** the daemon logs the underlying error at WARN

#### Scenario: Malformed request fails per canonical socket protocol
- **WHEN** a request is missing the required `workspace_basename` field OR `query` field
- **THEN** the handler returns `{"ok":false,"error":"missing required field: <name>"}` per the canonical "Request protocol" scenario
- **AND** the connection is closed

#### Scenario: Unknown workspace_basename does not leak across workspaces
- **WHEN** workspace A's MCP child accidentally sends a query with workspace B's basename (env var misconfiguration OR a hypothetical compromised child)
- **THEN** the daemon's handler queries workspace B's store (the daemon trusts the basename for registry lookup)
- **AND** the response contains workspace B's hits
- **AND** the routing is "as configured" — workspace isolation depends on the env var the daemon itself sets via `ClaudeCliExecutor::write_mcp_config`; a child that controls its own env can in principle query any registered workspace
- **AND** this is acceptable because MCP children run as the daemon user with full local access; isolation across managed workspaces is a property of the env var the daemon writes, not a security boundary enforced by the control socket

### Requirement: Polling-iteration triage flows resolve their prompts via the uniform PromptLoader
The polling iteration's two triage flows — the `send it` audit-reply triage AND the `propose` chat-request triage — SHALL load their prompt templates through `PromptLoader::load(PromptId::AuditTriage, &workspace_config)` AND `PromptLoader::load(PromptId::ChatRequestTriage, &workspace_config)` respectively. Direct `include_str!("../../prompts/audit-triage.md")` AND `include_str!("../../prompts/chat-request-triage.md")` invocations at the call sites SHALL be removed.

The override fields `executor.audit_triage.prompt_path` AND `executor.chat_request_triage.prompt_path` (per the executor spec) SHALL take effect for these flows. The loader's uniform precedence (embedded → per-workspace → daemon-level → embedded fallback) applies as documented.

#### Scenario: Send-it triage uses the loader
- **WHEN** the polling iteration processes a pending `send it` triage AND the workspace has no override configured
- **THEN** the executor invocation's prompt is the embedded `prompts/audit-triage.md` returned by the loader

#### Scenario: Send-it triage honors the per-workspace override
- **WHEN** the polling iteration processes a pending `send it` triage AND the workspace has `executor.audit_triage.prompt_path: "./prompts/triage-custom.md"` AND the file exists
- **THEN** the executor invocation's prompt is the override file's contents
- **AND** the LLM's classification behavior is governed by the operator's customized template

#### Scenario: Propose-flow triage uses the loader
- **WHEN** the polling iteration processes a pending `propose` request AND the workspace has no override configured
- **THEN** the executor invocation's prompt is the embedded `prompts/chat-request-triage.md` returned by the loader

#### Scenario: Propose-flow triage honors the per-workspace override
- **WHEN** the polling iteration processes a pending `propose` request AND the workspace has `executor.chat_request_triage.prompt_path: "./prompts/chat-triage-custom.md"` AND the file exists
- **THEN** the executor invocation's prompt is the override file's contents

#### Scenario: Missing override path falls back to embedded via the loader
- **WHEN** the workspace's triage override path is configured to a non-existent file
- **THEN** the loader's one-shot WARN fires per the executor spec's uniform precedence
- **AND** the triage flow proceeds with the embedded default
- **AND** the triage completes successfully (the misconfigured path is not a triage-blocking condition)

### Requirement: GitHub `pulls` filter-by-head queries use `fork_owner` as the head qualifier owner in fork-PR mode
Any GitHub REST API request to `GET /repos/{owner}/{repo}/pulls` that filters by `head` SHALL construct the head qualifier as `<head_owner>:<head_branch>` where:

- `head_owner = github.fork_owner` when `github.fork_owner` is configured (fork-PR mode).
- `head_owner = <upstream_owner>` when `github.fork_owner` is absent (direct-push mode).

The `head_owner` SHALL be an explicit named variable in the call site, computed from `github.fork_owner.as_deref().unwrap_or(&upstream_owner)` (OR equivalent). Helper functions in the GitHub-API module SHALL accept the head qualifier owner as an explicit parameter; they SHALL NOT silently reuse the upstream-owner argument (used to construct the URL path) for the head qualifier. The construction-site discipline is what prevents the bug class — every caller is forced to think about which owner belongs in the head filter.

This requirement applies to every code path that issues a `pulls?head=...` query, including:

- The polling iteration's open-PR existence check (`open_pr_exists_for_agent_branch_at`).
- The PR-comment revision dispatcher's PR-list query (`process_revision_requests_at`).
- The operator-status reply's latest-PR query (`fetch_latest_pr`).
- Any future code that filters PRs by head.

The rationale: GitHub's `head` filter is an exact-string match on `<owner>:<branch>`. In fork-PR mode the PR's head is on the operator's fork, so `<fork-owner>:<branch>` matches AND `<upstream-owner>:<branch>` does not. Pre-spec code in two of the three head-filter queries used `<upstream-owner>:<branch>` (because the helper functions reused the URL-path owner parameter for the head qualifier construction), which never matched any PR in fork-PR mode. Operators in fork-PR mode lost `@<bot> revise` on PRs AND status's `latest PR` field with no log line — the helpers returned empty lists which the callers correctly treated as "no PR" without a way to distinguish that signal from a real "no PR exists" state.

The invariant is enforceable by code review: at every `head=...` query construction site, the `head_owner` variable's source MUST be visible AND MUST explicitly consult `github.fork_owner`.

#### Scenario: Fork-PR-mode revise dispatcher finds the PR
- **WHEN** `github.fork_owner` is configured AND an open PR exists with head `<fork_owner>:<agent_branch>` on the upstream repo
- **AND** the polling iteration's revise-dispatcher step runs
- **THEN** the GitHub `pulls?head=...` query is constructed with `head=<fork_owner>:<agent_branch>`
- **AND** the API returns the PR
- **AND** the dispatcher fetches the PR's comments AND proceeds with the revision flow per the canonical revise mechanism

#### Scenario: Fork-PR-mode status reply finds the PR
- **WHEN** `github.fork_owner` is configured AND an open PR exists with head `<fork_owner>:<agent_branch>`
- **AND** an operator runs `@<bot> status <repo>`
- **THEN** the status path's `fetch_latest_pr` call constructs `head=<fork_owner>:<agent_branch>`
- **AND** the reply's `latest PR` line names the PR number AND URL, NOT `(none)`

#### Scenario: Direct-push-mode behaviour unchanged
- **WHEN** `github.fork_owner` is absent (direct-push mode)
- **AND** any of the three head-filter code paths runs
- **THEN** the `head_owner` resolves to the upstream owner (via `unwrap_or(&upstream_owner)`)
- **AND** the constructed query exactly matches the pre-spec behaviour
- **AND** existing direct-push-mode operators see no behavioural change

#### Scenario: Helper functions require explicit head_owner parameter
- **WHEN** a maintainer inspects the GitHub-API helper signatures
- **THEN** `list_open_prs_for_head` AND `latest_pr_for_head` (AND any future helper that issues a head-filtered pulls query) take a separate `head_owner: &str` parameter alongside the URL-path `owner` parameter
- **AND** the helpers' internal `format!("{head_owner}:{head_branch}")` construction does NOT reuse the `owner` parameter
- **AND** every caller passes the explicitly-computed `head_owner`

#### Scenario: Regression test guards the construction
- **WHEN** the test suite runs
- **THEN** at least one unit test exercises `list_open_prs_for_head` with `owner != head_owner` AND asserts the query string contains `head=<head_owner>:<head_branch>` exactly
- **AND** at least one unit test exercises `latest_pr_for_head` with the same shape
- **AND** at least one integration test for the revise dispatcher exercises fork-PR mode end-to-end AND asserts the dispatcher proceeds past the open-PR-list step (i.e., the mock matched the fork-owner-qualified query)
- **AND** at least one integration test for the status reply exercises fork-PR mode end-to-end AND asserts `latest PR` is populated rather than `(none)`
- **AND** all four tests fail against any implementation that uses the upstream owner as the head qualifier

### Requirement: Revise dispatcher refuses to invoke the executor when PR-context assembly fails
The PR-comment revision dispatcher SHALL assemble the executor's `RevisionContext` from PR-sourced material (per the `executor` capability's `Revision prompt is constructed from PR-sourced material` requirement) BEFORE invoking the executor. The assembly step SHALL fetch:

- The PR body (one `GET /repos/{owner}/{repo}/pulls/{n}` call OR via the existing PR-list response if already in scope).
- The PR's issue comments via `list_issue_comments_since(..., since=None)` — fetching every comment, then filtering to those whose body starts with the canonical `## Agent implementation notes` heading.

When any of these fetches returns an `Err`, the dispatcher SHALL:

1. Post a clear failure comment to the PR naming the assembly failure:
   ```
   ✗ Cannot revise: failed to fetch PR context: <truncated-error-message>. The daemon will retry on the next polling iteration. If this persists, check journalctl for the daemon's GitHub API errors AND verify the bot's token has Read access on this repo.
   ```
2. NOT advance the comment-seen marker (`last_seen_comment_ts` in the per-PR state file). This guarantees the revise comment is NOT lost; the next iteration's dispatcher pass re-attempts the assembly. Transient API errors (rate limits, brief outages, 5xx) self-recover.
3. NOT invoke the executor. The placeholder fallback is removed entirely; there is no degraded-prompt path.

When the assembly succeeds, the dispatcher SHALL pass the populated `RevisionContext` to the executor's revision-mode entry point. The executor's `Completed` / `Failed` / `AskUser` outcomes are handled by the existing canonical revise mechanisms (per the canonical "Revising an open PR via comment" requirement).

#### Scenario: Successful assembly proceeds to the executor
- **WHEN** an operator's `@<bot> revise <text>` comment is detected on an open PR
- **AND** the dispatcher fetches the PR body successfully AND fetches the PR's issue comments successfully
- **THEN** the dispatcher constructs a `RevisionContext` with all five fields populated (`pr_body`, `pr_change_list`, `agent_implementation_notes`, `pr_diff`, `revision_text`)
- **AND** invokes the executor's revision-mode entry point with that context
- **AND** the executor's outcome is handled per the canonical revise mechanism

#### Scenario: PR body fetch failure produces refusal comment AND preserves marker
- **WHEN** the PR body fetch (`GET /repos/{owner}/{repo}/pulls/{n}`) returns an `Err` (HTTP 5xx, network timeout, etc.)
- **THEN** the dispatcher posts a comment beginning with `✗ Cannot revise: failed to fetch PR context:` AND naming the error
- **AND** the per-PR state file's `last_seen_comment_ts` is NOT advanced
- **AND** the executor is NOT invoked
- **AND** the next polling iteration's dispatcher re-attempts the assembly (the operator's revise comment is preserved)

#### Scenario: PR comments fetch failure has identical handling
- **WHEN** the PR-comments fetch returns an `Err`
- **THEN** the same refusal comment is posted, the marker is NOT advanced, AND the executor is NOT invoked

#### Scenario: Persistent assembly failure surfaces visibly
- **WHEN** the PR-body OR PR-comments fetch fails on N consecutive polling iterations (N > 1)
- **THEN** N refusal comments accumulate on the PR (one per iteration)
- **AND** the comment timestamps make the persistent-failure pattern visible to the operator on the PR page
- **AND** the operator can investigate via journalctl AND the GitHub API status pages

#### Scenario: PR has no `Agent implementation notes` comments
- **WHEN** the dispatcher fetches PR comments AND none match the `## Agent implementation notes` heading (e.g., revise was posted within the same iteration the PR was created, before the implementer-summary comment landed)
- **THEN** the assembly succeeds with an empty `agent_implementation_notes` field
- **AND** the executor is invoked with the empty field
- **AND** the LLM still has spec deltas (via `pr_diff`), the PR body, the change list, AND the revision request — sufficient to attempt the revision
- **AND** no refusal comment is posted (this is not a failure)

#### Scenario: No fallback placeholder is ever rendered
- **WHEN** the dispatcher invokes the executor's revision-mode entry point in any code path
- **THEN** the `RevisionContext` it passes has all five fields populated from PR-sourced material
- **AND** no field contains the pre-`a20a5` placeholder string `_(original change material unavailable — ...)_` OR any analogous "best-effort" stub
- **AND** the rendered prompt the executor sees is full-fidelity for the revision task

### Requirement: Polling iteration termination is gated on agent-branch commit count, NOT on implementer-queue outcome

The polling iteration's "no work to ship" early-return SHALL be gated EXCLUSIVELY on the agent branch's commit count relative to base — computed via `git rev-list --count <base_branch>..<agent_branch>` or equivalent. The early-return SHALL NOT use any higher-level signal (the implementer-processed-changes list, the audit-queue length, the reviewer-finding count, etc.) as the sole gate. The reason: any signal captured BEFORE the audit phase runs (the audit phase runs AFTER the queue walk per the canonical "audit phase runs AFTER pending change queue walk" requirement) can miss commits the audit phase subsequently produced. Using a stale signal to gate the push step causes audit-produced commits to be silently destroyed by the next iteration's `recreate_branch` step, which presented in production as "🔍 created proposal notifications without PRs" across multiple repos.

When the agent-branch commit count is zero — meaning neither the implementer NOR any audit produced commits — the iteration SHALL clear `AlertState` AND return `Ok(())`. When the agent-branch commit count is non-zero, the iteration SHALL proceed to the push + PR-creation steps EXCEPT when iteration-pending markers are present per the suppression rule below. The canonical "audit's creation commits ship in iteration N's PR" requirement is thereby implementable: an iteration that did no implementer work but had an audit produce proposal commits ships those commits in a PR.

**Iteration-pending suppression rule (new in this change).** Before invoking the push + PR-creation steps, the polling-loop SHALL scan `<workspace>/openspec/changes/*/.iteration-pending.json` markers. When one or more markers are present, the audit-only-PR path SHALL be SUPPRESSED for this iteration: the iteration SHALL log INFO `audit-only PR path suppressed: iteration-pending markers present for <change-list>` AND return `Ok(())` WITHOUT pushing OR opening a PR. Audit proposals committed during this iteration remain on agent-q AND on disk in `openspec/changes/<aXX>-*` directories; they ship in the NEXT iteration's PR after the iteration-pending change concludes (via `outcome_success`, `outcome_spec_needs_revision`, OR the `a27a1` 5-iteration cap).

The suppression rule trades off two failure modes: opening a PR that mixes iteration_request WIP with audit findings (operator-confusing, mergable-yet-shouldn't-be) versus deferring audit findings by a few polling cycles (operator-invisible, no data loss). The latter is preferred because audit cadences are periodic (operators don't expect immediate proposals on every iteration) AND because the iteration sequence is bounded by `a27a1`'s 5-iteration cap (worst-case audit-finding delay is 5 iterations of the in-progress change).

This requirement is additive: it codifies an invariant that protects against future regressions of the same bug class. Any code change that introduces a new commit-producing iteration phase (a future spec-writing audit type, a future autonomous-fix mechanism, etc.) automatically benefits — the termination gate is already correct, the new commits already get pushed when iteration-pending markers are absent.

#### Scenario: Audit-only iteration pushes and opens PR
- **WHEN** a polling iteration's queue walk produces zero implementer-processed changes (`processed` is empty)
- **AND** a spec-writing audit (e.g., `security_bug_audit`) runs during the audit phase AND returns `SpecsWritten` AND commits its produced proposal directories to the agent branch
- **AND** NO `.iteration-pending.json` markers are present in any change directory
- **THEN** the iteration's commit-count check returns a non-zero value (the audit's commits ARE on the agent branch)
- **AND** the iteration proceeds to `git::push_force_with_lease` AND to `github::create_pull_request`
- **AND** a `✅ PR opened: <url>` notification fires per the existing canonical PR-opened notification requirement
- **AND** the next iteration's `recreate_branch` step DOES NOT destroy the audit's commits (because they were pushed AND the next iteration's pull-from-remote step retrieves them via the open PR's branch state)

#### Scenario: Empty-implementer + empty-audit iteration still returns early correctly
- **WHEN** a polling iteration's queue walk produces zero implementer-processed changes
- **AND** no audit produces commits (either no audits are due OR all due audits return `NoFindings` / `Reported` outcomes that do NOT commit)
- **THEN** the commit-count check returns zero
- **AND** the iteration clears `AlertState` AND returns `Ok(())` without invoking the push step
- **AND** no `BranchPushFailure` chatops alert fires (there was nothing to push)

#### Scenario: Implementer-non-empty + commit-count-non-zero proceeds normally
- **WHEN** a polling iteration's queue walk processes one OR more changes AND produces at least one commit
- **AND** NO `.iteration-pending.json` markers are present (the implementer changes archived cleanly, not iteration_request)
- **THEN** the commit-count check returns non-zero
- **AND** the iteration proceeds to the reviewer step (if configured) AND to the push + PR-creation steps
- **AND** the canonical happy-path scenarios for end-of-iteration push + PR continue to hold

#### Scenario: Implementer-non-empty but commit-count-zero returns early
- **WHEN** a polling iteration's queue walk processes one OR more changes BUT every processed change's executor invocation produced an empty diff (the existing "all completed changes had empty diffs" path)
- **THEN** the commit-count check returns zero
- **AND** the iteration clears `AlertState` AND returns `Ok(())` per the canonical empty-diff handling
- **AND** the iteration logs an info-level line naming that the pass produced no commits

#### Scenario: Reviewer skipped on audit-only iterations
- **WHEN** the iteration reaches the reviewer step AND `processed.is_empty()` is true AND `commit_count > 0`
- **AND** NO `.iteration-pending.json` markers are present (suppression rule doesn't fire)
- **THEN** the reviewer's `review()` method is NOT invoked
- **AND** the PR is opened with NO `## Code Review` section
- **AND** the rationale is: the audit's own validation pass already gated each proposal (`openspec validate --strict` per the canonical "LLM-driven audits validate their generated proposals before committing" requirement); a code-quality reviewer adds no signal against mechanical proposal-writing

#### Scenario: PR body for audit-only iterations names commits by category
- **WHEN** the iteration opens a PR with `processed.is_empty()` AND `commit_count > 0` AND NO iteration-pending markers
- **THEN** the PR body composition partitions the agent-branch commits by message-prefix category: `audit: <type>` → audit-produced; `iteration N of <change>` → iteration WIP; `archive: <change>` → implementer-archived; anything else → manual / unknown
- **AND** the body includes only sections for non-empty categories (the "Audit-produced proposals" section appears ONLY when audit-produced commits exist)
- **AND** when ALL commits are audit-produced, the PR title takes the canonical form `audit-only: <N> proposal(s) from <comma-separated-audit-types>`
- **AND** the PR body lists the agent-branch commit subjects per the present categories (sourced from `git log <base>..<agent> --format=%s`) so reviewers see exactly what the PR contains
- **AND** the PR body notes that the produced `openspec/changes/<prefix>-*` directories will be picked up by the NEXT polling iteration's `list_pending` for implementer routing (when audit-produced commits are present)

#### Scenario: Regression test guards the gate
- **WHEN** the test suite runs
- **THEN** at least one test sets up a fixture iteration with empty `processed` AND a mock audit that produces commits AND NO `.iteration-pending.json` markers AND asserts: (a) the push function IS called, (b) the PR-creation function IS called, (c) the PR's head ref matches the agent branch
- **AND** the test fails against any implementation that gates the early-return on `processed.is_empty()` instead of on the agent-branch commit count

#### Scenario: Iteration-pending suppression with iteration_request WIP only
- **WHEN** a polling iteration's `IterationRequested` arm just committed iteration_request WIP to agent-q for change X
- **AND** the `.iteration-pending.json` marker for change X is present on disk
- **AND** no other commits are on agent-q ahead of base
- **THEN** the commit-count check returns non-zero (the iteration_request commit IS on agent-q)
- **AND** the iteration-pending scan returns `[X]`
- **AND** the audit-only-PR path is SUPPRESSED
- **AND** the iteration logs INFO `audit-only PR path suppressed: iteration-pending markers present for X`
- **AND** the iteration returns `Ok(())` WITHOUT calling `git::push_force_with_lease` OR `github::create_pull_request`
- **AND** no PR opens

#### Scenario: Iteration-pending suppression with audit commits AND iteration WIP
- **WHEN** a polling iteration has BOTH audit-produced commits AND iteration_request WIP commits on agent-q
- **AND** at least one `.iteration-pending.json` marker is present
- **THEN** the audit-only-PR path is SUPPRESSED (the suppression rule fires on ANY marker presence; mixed commit content doesn't change the suppression decision)
- **AND** the audit-produced commits remain on agent-q AND in their respective `openspec/changes/<aXX>-*` directories
- **AND** the next iteration (after the iteration-pending change concludes) opens an audit-only PR with the audit commits

#### Scenario: Iteration-pending absent → audit-only PR opens as today
- **WHEN** a polling iteration has audit-produced commits on agent-q AND NO `.iteration-pending.json` markers anywhere
- **THEN** the audit-only-PR path fires normally (existing happy path)
- **AND** a PR is opened with the canonical audit-only title format

### Requirement: Scout polling-iteration handler produces a triage list AND persists `ScoutRunState`
The daemon's per-repo polling iteration SHALL, after processing pending proposal AND brownfield requests AND before the standard change-processing pass, drain at most one pending scout request from `pending_scout_requests`. The handler SHALL invoke the executor in scout mode with `WritePolicy::None` AND a sandbox profile permitting `Read`, `Glob`, `Grep`, AND `Bash` (read-only, with `gh` permitted).

The scout prompt SHALL be loaded via `PromptLoader::load(PromptId::Scout, &workspace_config)` (per the executor spec). The prompt input SHALL be assembled from:

1. The resolved prompt template.
2. The operator's guidance (when non-empty), interpolated into a `## Operator guidance` section.
3. The workspace's `README.md` contents AND the list of `docs/*.md` filenames.
4. A code-symbol overview built via `cargo metadata` (Rust workspaces) OR a ripgrep pass for top-level public items (other languages).
5. `git log --since="<N> days ago" --pretty=oneline` output for recent-activity context, where N is `features.scout.staleness_warn_days * 4`.
6. The open-issues list via `gh api repos/<owner>/<repo>/issues?state=open --paginate` when `features.scout.include_issues: true`. On `gh` failure (auth, rate limit, network), the handler SHALL log a WARN naming the failure AND continue with an empty issue list.

The executor's response SHALL be a JSON array of opportunity items. Each item SHALL have:

- `id: usize` — 1-indexed sequential identifier.
- `category: String` — one of: `security`, `bug`, `error_handling`, `type_tightening`, `code_smell`, `perf`, `documentation`, `test_coverage`, `issue`, `todo_fixme`, `research`.
- `title: String` — one-line summary.
- `body: String` — one-paragraph description.
- `source: String` — `<file>:<line>` for code-derived, issue URL for issue-derived, OR commit-range for git-log-derived.
- `tractability: String` — one of: `small`, `medium`, `large`.

The handler SHALL validate the response: well-formed JSON, every item has all required fields, categories AND tractability values fall in the allowed sets, AND `items.len() <= features.scout.max_items`. On validation failure, the handler SHALL post a thread reply naming the failure AND NOT persist any state file.

On validation success, the handler SHALL: write `<workspace>/.state/scout_runs/<request_id>.json` with `ScoutRunState { request_id, repo_url, guidance, head_sha_at_run, completed_at, thread_ts, channel, items }`; render the list (grouped by category, compact per-item format) AND post it to the request's thread; append the closing note `Reply with @<bot> spec-it <N> [optional guidance] to scope work on any item.`. When the rendered list exceeds the threaded-notification length limit, the handler SHALL truncate the displayed list AND append `… (truncated; full list in <workspace>/.state/scout_runs/<request_id>.json)`.

#### Scenario: Happy-path scout run
- **WHEN** the executor returns a valid JSON list of 12 items AND the workspace has no `gh` failure
- **THEN** the handler persists `ScoutRunState` with 12 items
- **AND** posts a thread reply grouping items by category with the closing spec-it instruction
- **AND** the thread reply does NOT contain `(truncated; …)`

#### Scenario: Invalid JSON aborts the run
- **WHEN** the executor returns text that is not valid JSON OR is missing required item fields
- **THEN** no state file is written
- **AND** the thread reply names the validation failure AND points at the daemon log

#### Scenario: gh issues unavailable falls through gracefully
- **WHEN** `features.scout.include_issues: true` AND `gh api` returns a non-success exit code
- **THEN** a WARN is logged naming the gh failure
- **AND** the scout proceeds with code-derived items only
- **AND** the thread reply includes a note that issue-derived items were skipped this run

#### Scenario: Long list triggers truncation
- **WHEN** the rendered list exceeds the threaded-notification length limit
- **THEN** the handler posts the first N categories that fit
- **AND** appends `… (truncated; full list in <workspace>/.state/scout_runs/<request_id>.json)`
- **AND** the persisted state file contains ALL items (truncation affects display only)

#### Scenario: Max-items cap enforced
- **WHEN** `features.scout.max_items: 10` AND the executor returns a list with 15 items
- **THEN** the handler rejects the run via the validation step
- **AND** the thread reply names the cap violation

### Requirement: `spec-it` polling-iteration handler translates a scouted item into a `ProposeRequest`
The polling iteration SHALL drain at most one `SpecItAction` per iteration from `pending_spec_it_requests`. For each action, the handler SHALL: load the referenced `ScoutRunState`; look up the item by `item_id`; compute staleness; construct a propose-request-text per a documented shape; submit a `ProposeRequest` using the canonical propose machinery from the existing orchestrator-cli requirements.

The propose-request text SHALL be:

```
[scout-item #<N>] <item.title>

<item.body>

Source: <item.source>
Category: <item.category>
Tractability: <item.tractability>

<operator guidance, if any>
```

Status updates from the resulting propose lifecycle SHALL post into the scout's lifecycle thread (the spec-it action's `thread_ts`), keeping the scout → pick → spec → PR flow in a single visible conversation.

#### Scenario: Spec-it dispatches a ProposeRequest with the expected text shape
- **WHEN** the scout state contains an item with `id: 3, title: "Unauthenticated debug endpoint", body: "..."` AND the operator submits `SpecItAction { item_id: 3, guidance: None }`
- **THEN** a ProposeRequest is submitted with text matching the documented shape (header line, body, metadata lines)
- **AND** the resulting propose lifecycle's status updates post into the scout's thread

#### Scenario: Spec-it concatenates operator guidance
- **WHEN** the operator's `SpecItAction.guidance` is `stick to the OAuth scope, ignore the rate-limit angle`
- **THEN** the constructed propose-request text ends with `\n\nstick to the OAuth scope, ignore the rate-limit angle`

#### Scenario: Missing scout state aborts with thread reply
- **WHEN** the `SpecItAction.scout_request_id` references a state file that no longer exists (deleted by clear-scout between dispatch AND processing)
- **THEN** the handler posts `✗ spec-it: scout state for request <id> not found (was it cleared?). Re-run scout to refresh the list.`
- **AND** no propose-request is submitted

#### Scenario: Item not in scout's list aborts with thread reply
- **WHEN** the `SpecItAction.item_id` does not match any item id in the loaded state
- **THEN** the handler posts `✗ spec-it: item #<id> not present in scout state. The list may have changed; run @<bot> scout <repo> for a fresh list.`
- **AND** no propose-request is submitted

### Requirement: Scout staleness warning when scout is old OR HEAD has drifted
On each `spec-it` invocation, the handler SHALL compute two staleness signals:

1. `now - ScoutRunState.completed_at > features.scout.staleness_warn_days days`.
2. `current_workspace_HEAD_sha != ScoutRunState.head_sha_at_run`.

If either signal is true, the handler SHALL post a single thread reply BEFORE submitting the propose-request:

```
⚠️ Scout from <relative-time> ago; HEAD has <unchanged|moved <N> commits>. Proceeding with the scouted item; consider re-running scout for fresh results.
```

The handler SHALL warn AND PROCEED — staleness is not a blocking condition. Operators who want a fresh scout invoke `@<bot> scout <repo>` themselves.

#### Scenario: Scout older than threshold warns AND proceeds
- **WHEN** `features.scout.staleness_warn_days: 7` AND the scout's `completed_at` is 10 days ago
- **THEN** the handler posts the staleness warning naming `10 days`
- **AND** the propose-request still submits

#### Scenario: HEAD drift warns AND proceeds
- **WHEN** the scout's `head_sha_at_run` is `abc123` AND the workspace's current HEAD is `def456` AND the commit count between them is 5
- **THEN** the staleness warning names `HEAD has moved 5 commits`
- **AND** the propose-request still submits

#### Scenario: Both staleness signals combine in one warning
- **WHEN** both signals are true (scout old AND HEAD moved)
- **THEN** the handler posts ONE warning naming both conditions
- **AND** does NOT post two separate warnings

#### Scenario: Fresh scout produces no warning
- **WHEN** the scout completed less than `staleness_warn_days` days ago AND HEAD is unchanged
- **THEN** the handler does NOT post the staleness warning
- **AND** the propose-request submits without preamble

### Requirement: `features.scout` config schema
The per-repo config schema SHALL accept an optional `features.scout` block:

- `enabled: bool` (default `true`) — when `false`, the `scout`, `spec-it`, AND `clear-scout` verbs are refused at parse time.
- `prompt_path: Option<String>` (default `None`) — per the uniform PromptLoader pattern.
- `max_items: usize` (default `30`, valid range `1..=50`) — cap on the scout's item list.
- `include_issues: bool` (default `true`) — controls whether the handler attempts `gh api` for open issues.
- `staleness_warn_days: u64` (default `7`) — threshold for the staleness warning.

Invalid values (non-bool where bool expected; `max_items` outside `1..=50`) cause config-load to fail-fast with an error naming the offending field.

#### Scenario: Default config enables scout
- **WHEN** a workspace's config omits the `features.scout` block
- **THEN** all five fields take their defaults (`enabled: true, prompt_path: None, max_items: 30, include_issues: true, staleness_warn_days: 7`)

#### Scenario: Explicit disable refuses all three verbs
- **WHEN** a workspace's config sets `features.scout.enabled: false`
- **THEN** the dispatcher refuses `@<bot> scout`, `@<bot> spec-it`, AND `@<bot> clear-scout` for that workspace

#### Scenario: max_items outside valid range fails config load
- **WHEN** a workspace's config sets `features.scout.max_items: 0` OR `features.scout.max_items: 100`
- **THEN** config-load fails with an error naming `features.scout.max_items` AND the valid range `1..=50`

### Requirement: `spec_storage.path` config redirects spec reads AND writes to an external git working tree
The per-repo config schema SHALL accept an optional `spec_storage` block with one required field, `path: String` (workspace-relative OR absolute). When set, autocoder SHALL treat `<spec_storage.path>/openspec/` as the canonical-spec source AND as the destination for spec-change writes, INSTEAD OF `<workspace>/openspec/`.

Config-load SHALL fail-fast if any of the following holds when `spec_storage` is set:

- The resolved path does NOT exist OR is not a directory.
- The directory at the path is not a git working tree (verified via `git -C <path> rev-parse --is-inside-work-tree` returning a non-zero exit OR a value other than `true`).
- The subdirectory `<path>/openspec/` does NOT exist.

Every path-resolution call site that previously composed `<workspace>/openspec/...` SHALL go through a `SpecRoot` resolver that returns the correct root for the current config. When `spec_storage` is unset, the resolver returns `<workspace>/openspec/` (existing behavior preserved).

Spec-change commits (brownfield draft, scout spec-it, `openspec archive`) SHALL be made in the spec_storage repo's working tree when `spec_storage` is set; the spec_storage repo's remote AND base branch determine the push target AND PR base. Code-change commits continue to live in the code workspace repo.

#### Scenario: Default — no spec_storage configured
- **WHEN** a per-repo config omits the `spec_storage` block
- **THEN** the `SpecRoot` resolver returns `<workspace>/openspec/` for all spec-path queries
- **AND** spec-change commits target the code workspace repo's working tree (existing behavior unchanged)

#### Scenario: spec_storage configured — reads redirect
- **WHEN** a per-repo config sets `spec_storage.path: "../my-specs"` AND that directory is a valid git working tree containing `openspec/`
- **THEN** the implementer prompt's canonical-spec reads load from `../my-specs/openspec/`
- **AND** the audit framework discovers spec files via `../my-specs/openspec/specs/<cap>/spec.md`
- **AND** `openspec validate` invocations are run with `cwd: ../my-specs`

#### Scenario: spec_storage configured — writes redirect
- **WHEN** a per-repo config sets `spec_storage.path: "/abs/path/to/specs"` AND a brownfield iteration completes successfully
- **THEN** the change-directory `openspec/changes/brownfield-<cap>/` is created inside `/abs/path/to/specs/`
- **AND** the commit is made in the spec_storage repo's working tree
- **AND** the code workspace's `openspec/` directory is NOT modified

#### Scenario: spec_storage path is not a git working tree
- **WHEN** config-load encounters `spec_storage.path: "/tmp/not-a-repo"` AND that directory exists but is not a git working tree
- **THEN** config-load fails with `spec_storage.path: /tmp/not-a-repo is not a git working tree (git -C ... rev-parse --is-inside-work-tree failed)`
- **AND** the daemon exits non-zero before any polling task is spawned

#### Scenario: spec_storage path lacks openspec subdirectory
- **WHEN** config-load encounters `spec_storage.path: "../some-other-repo"` AND that path is a git working tree but contains no `openspec/` subdirectory
- **THEN** config-load fails naming the missing `openspec/` subdirectory
- **AND** the daemon exits non-zero

### Requirement: `upstream` config block declares a fetch-only remote AND opportunistic fetch on iteration start
The per-repo config schema SHALL accept an optional `upstream` block with fields:

- `remote: String` (default `"upstream"`) — the git remote name to use.
- `branch: String` (default `"main"`) — the upstream's primary branch.
- `url: String` (required when block is present) — the upstream repo's git URL (SSH OR HTTPS).

When `upstream` is configured, the polling iteration's startup sequence SHALL, AFTER the existing `git fetch origin` step:

1. Ensure the workspace has a remote named `<upstream.remote>` pointing at `<upstream.url>`. If absent, add it via `git remote add`. If present with a different URL, correct it via `git remote set-url`.
2. Run `git fetch <upstream.remote>` with a 30-second timeout.
3. On success: continue with the iteration.
4. On failure (timeout, network, auth): log a WARN naming the failure AND continue with the iteration. The fetch is best-effort.

The opportunistic fetch SHALL NOT trigger any rebase OR merge — it only updates remote-tracking branches so the workspace has fresh upstream state when the operator runs `sync-upstream`.

#### Scenario: Upstream unconfigured — no fetch
- **WHEN** a per-repo config omits the `upstream` block
- **THEN** the iteration's startup sequence runs only the existing `git fetch origin`
- **AND** no `upstream` remote is added OR fetched

#### Scenario: Upstream configured, remote missing — added on iteration start
- **WHEN** the per-repo config sets `upstream: { remote: "upstream", branch: "main", url: "https://github.com/foo/bar.git" }` AND the workspace has no remote named `upstream`
- **THEN** the polling iteration adds the remote via `git remote add upstream https://github.com/foo/bar.git`
- **AND** the subsequent `git fetch upstream` runs

#### Scenario: Upstream configured, remote URL drifted — corrected on iteration start
- **WHEN** the workspace's `upstream` remote points at a URL different from `upstream.url` (e.g., the config was updated)
- **THEN** the polling iteration corrects the remote via `git remote set-url upstream <upstream.url>`
- **AND** the subsequent `git fetch upstream` runs

#### Scenario: Upstream fetch failure does not block
- **WHEN** the `git fetch upstream` call returns non-zero (network, auth, timeout)
- **THEN** a WARN is logged naming the failure AND the remote URL
- **AND** the iteration proceeds to its normal change-processing pass

### Requirement: `sync-upstream` polling-iteration handler rebases the base branch onto upstream/<branch>
The polling iteration SHALL handle `SyncUpstreamAction` requests via a dedicated handler. The handler SHALL:

1. Verify `upstream` is configured for the repo. If not, post a thread reply `✗ sync-upstream: no upstream configured for this repo. Set the upstream block in config.yaml.` AND return without acquiring the busy marker.
2. Respect the per-repo busy-marker rule: if another iteration is currently working on this repo, the handler SHALL queue the request OR refuse with the standard busy reply per the existing convention.
3. Acquire the workspace busy marker.
4. Run `git fetch <upstream.remote>` with a 60-second timeout. On failure, post `✗ sync-upstream: fetch failed: <reason>.` AND release the busy marker.
5. Checkout the configured base branch.
6. Run `git rebase <upstream.remote>/<upstream.branch>`.
7. **On conflict**: run `git rebase --abort` to restore the workspace; post `✗ sync-upstream: rebase conflict on <list-of-conflicting-files>. Aborted. Resolve manually in the workspace AND re-run, OR merge manually.`; release the busy marker.
8. **On success**: post `✓ sync-upstream: pulled <N> commit(s) from <upstream.remote>/<upstream.branch>. Base branch is <M> commit(s) ahead of upstream.` where `<N>` is the rebase's incorporated-commit count AND `<M>` is `git rev-list --count <upstream.remote>/<upstream.branch>..HEAD`; release the busy marker.

The handler SHALL NOT push the rebased base branch — the operator decides when to push to their fork. The `auto_submit_pr` field is unrelated; sync-upstream does not produce PRs.

#### Scenario: No upstream configured
- **WHEN** the handler processes a `SyncUpstreamAction` for a repo whose config has no `upstream` block
- **THEN** the handler posts `✗ sync-upstream: no upstream configured for this repo. Set the upstream block in config.yaml.`
- **AND** no busy marker is acquired
- **AND** no git operations are run

#### Scenario: Happy-path rebase
- **WHEN** the handler runs for a configured repo AND `git fetch upstream` succeeds AND the subsequent rebase incorporates 7 commits cleanly AND the result is 0 commits ahead of upstream
- **THEN** the handler posts `✓ sync-upstream: pulled 7 commit(s) from upstream/main. Base branch is 0 commit(s) ahead of upstream.`
- **AND** the busy marker is released

#### Scenario: Rebase conflict aborts AND surfaces files
- **WHEN** the rebase encounters merge conflicts in `src/lib.rs` AND `tests/integration.rs`
- **THEN** the handler runs `git rebase --abort` so the workspace returns to its pre-rebase HEAD
- **AND** the handler posts `✗ sync-upstream: rebase conflict on src/lib.rs, tests/integration.rs. Aborted. Resolve manually in the workspace AND re-run, OR merge manually.`
- **AND** the busy marker is released

#### Scenario: No push by the handler
- **WHEN** the handler completes a happy-path rebase
- **THEN** the rebased base branch is NOT pushed to any remote
- **AND** the operator is responsible for pushing to the fork's remote when ready

### Requirement: `auto_submit_pr` config field gates PR creation per repo
The per-repo config schema SHALL accept an optional `auto_submit_pr: bool` field (default `true`). The git workflow manager SHALL honor this field at the end-of-iteration PR-creation step:

- `true` (default): existing behavior unchanged — push the agent branch AND open a PR per the canonical "Monolithic PR at end of pass" requirement.
- `false`: push the agent branch per the existing rules (direct-push OR fork-PR mode) BUT skip the PR-creation API call entirely. Return a `BranchPushedNoPr { branch_url, suggested_pr_command }` outcome where `suggested_pr_command` is `gh pr create --base <upstream.branch | base-branch> --head <agent-branch>`. If `upstream` is configured, the suggested base is `upstream.branch`; otherwise it is the workspace's configured base branch.

The polling iteration's chatops notification step SHALL post:

- On `PullRequestOpened`: the existing `✅ PR opened: <url>` thread reply.
- On `BranchPushedNoPr`: `📦 Branch pushed: <branch-url>\nRun: <suggested-pr-command>`.

`auto_submit_pr` applies UNIFORMLY to both code-workspace PR creation AND spec_storage PR creation (when `spec_storage` is also configured). Operators wanting different behavior for the two cases SHALL split the workspace into separate per-repo configurations.

#### Scenario: Default — auto_submit_pr true
- **WHEN** a per-repo config omits the `auto_submit_pr` field
- **THEN** the value resolves to `true`
- **AND** end-of-iteration behavior matches the existing "Monolithic PR at end of pass" requirement

#### Scenario: Explicit auto_submit_pr false
- **WHEN** a per-repo config sets `auto_submit_pr: false` AND an iteration produces a commit
- **THEN** the agent branch is pushed per the existing push rules
- **AND** no GitHub PR-creation API call is made
- **AND** the iteration's chatops thread reply contains `📦 Branch pushed: <branch-url>` followed by the templated `gh pr create` command

#### Scenario: Suggested gh-pr-create base comes from upstream config
- **WHEN** `auto_submit_pr: false` AND `upstream.branch: "main"` are configured
- **THEN** the suggested command is `gh pr create --base main --head <agent-branch>`

#### Scenario: Suggested gh-pr-create base falls back to base branch
- **WHEN** `auto_submit_pr: false` AND no `upstream` block is configured
- **THEN** the suggested command uses the workspace's configured base branch as `--base`

### Requirement: Production paths SHALL be threaded through APIs, NOT read from a process-global
The daemon SHALL construct exactly one `DaemonPaths` value at startup (in `main.rs` OR the equivalent entrypoint module) via the existing env-driven resolution AND SHALL thread that value into the rest of the codebase as an explicit constructor argument OR function parameter. Modules requiring path information SHALL accept a `DaemonPaths` value (by ownership, reference, OR `Arc<DaemonPaths>`) at their construction site OR on the function call path. No module SHALL read paths from a process-global cell, lazy-static, OR thread-local at runtime.

The following APIs that previously enabled global-state access SHALL be removed from `autocoder/src/paths.rs` AND SHALL NOT be reintroduced:

- `crate::paths::current()`
- `crate::paths::install_global(_)`
- `crate::paths::install_global_for_tests(_)`
- `crate::paths::test_fallback()`
- `crate::paths::get_global()`
- The underlying `OnceLock<DaemonPaths>` static.

The `DaemonPaths` struct itself, its helper methods (`alert_state_path`, `audit_logs_dir`, `control_socket_path`, `workspaces_dir`, etc.), AND its env-driven constructor SHALL be retained. The change is to WHO calls those helpers, NOT to what the struct provides.

Tests SHALL construct their own `DaemonPaths` via the existing `test_daemon_paths()` helper (which returns a tempdir-scoped instance) AND pass it explicitly into the production APIs they exercise. The test-suite invariant becomes: each test's fixtures live exclusively under its own tempdir, with no shared `<system-temp>/autocoder/...` location.

A CI scanner (an extension of the `a10` path-literals audit) SHALL fail the build if any of the removed function names reappears in `autocoder/src/` source files. The allowlist for this second-pass scanner SHALL be empty.

#### Scenario: Daemon entrypoint constructs the single instance
- **WHEN** the daemon starts up
- **THEN** `autocoder/src/main.rs` (OR the equivalent entrypoint module) constructs ONE `DaemonPaths` value via the env-driven resolution
- **AND** that value is handed (by ownership, reference, OR `Arc`) to the top-level orchestrator
- **AND** no other code path constructs an additional `DaemonPaths` for production use

#### Scenario: Module constructor accepts paths
- **WHEN** a module that requires path information is constructed (e.g., the audits scheduler, the busy-marker manager, the control-socket handler)
- **THEN** its constructor signature includes a `DaemonPaths` parameter (by ownership, reference, OR `Arc`)
- **AND** the module stores the value as a field for use by its methods
- **AND** the module does NOT call any removed global accessor

#### Scenario: Free function accepts paths as parameter
- **WHEN** a free function in `autocoder/src/` needs path information (e.g., a helper in `audits/threads.rs` OR `proposal_requests.rs`)
- **THEN** the function's signature includes a `paths: &DaemonPaths` (OR equivalent) parameter
- **AND** every caller passes the paths explicitly
- **AND** the function does NOT call any removed global accessor

#### Scenario: Test constructs its own DaemonPaths
- **WHEN** a test exercises production code that previously read from the global
- **THEN** the test calls `test_daemon_paths()` to obtain a `(TempDir, DaemonPaths)` pair
- **AND** passes the `DaemonPaths` explicitly into the production API
- **AND** the test's fixtures land under the tempdir, NOT a shared `<system-temp>/autocoder/...` location

#### Scenario: Concurrent tests do not collide on disk
- **WHEN** two tests run concurrently (cargo's default per-test thread) AND both invoke the same production API
- **THEN** each test's invocation uses ITS OWN `DaemonPaths` constructed via `test_daemon_paths()`
- **AND** the two tests' fixtures live under DISJOINT tempdir roots
- **AND** no fixture write OR read crosses between tests

#### Scenario: CI scanner blocks reintroduction
- **WHEN** the path-literals audit (extended per this requirement) runs against `autocoder/src/`
- **THEN** the scanner fails the build if it finds any reference to `paths::current`, `paths::install_global`, `paths::test_fallback`, OR `paths::get_global` in any `src/**/*.rs` file
- **AND** the scanner's own constants are constructed at runtime from fragments so it does not match itself

### Requirement: Revision dispatcher applies strict-since semantics to GitHub comment fetches

The PR-comment revision dispatcher (`autocoder/src/revisions.rs::process_one_pr`) SHALL guarantee that no operator-triggering comment is processed more than once across polling iterations, even when GitHub's `since` filter on the `/issues/<num>/comments` endpoint returns the same comment multiple times due to timestamp-precision boundary effects.

The guarantee is implemented in two complementary layers:

**Layer 1: sub-second precision in the GitHub query.** The dispatcher's GitHub fetch helper (`github::list_issue_comments_since`) SHALL format the `since` query parameter using millisecond OR finer precision. Second-precision truncation (e.g. `"2026-05-29T17:18:11Z"`) is FORBIDDEN because GitHub's internal `updated_at` storage uses sub-second precision AND its `since` filter compares against the full-precision value: a marker truncated to seconds AND a comment whose actual `updated_at` falls within the same second produces `updated_at > since` = TRUE, causing the comment to be returned on every subsequent fetch.

**Layer 2: client-side strict-since filter in the dispatcher loop.** Independent of how `since` is formatted in the GitHub query, the dispatcher SHALL apply a client-side `comment.created_at > state.last_seen_comment_at` strict-inequality check before processing each comment in the per-comment loop. Comments at OR before the marker SHALL be skipped without invoking the trigger parser, without calling `execute_revision`, AND without incrementing the per-PR revisions counter.

The client-side filter uses `comment.created_at` (NOT `updated_at`) as the comparison key, matching the marker's semantics: the marker tracks the latest processed comment's creation time, AND editing a previously-processed comment SHALL NOT cause its revise text to be re-processed.

The two layers are belt-and-suspenders. Layer 1 reduces wasted GitHub API roundtrips by not re-fetching the duplicate in the first place. Layer 2 ensures correctness even when Layer 1 fails (future GitHub API revisions, future helper modifications, GitHub-side replication lag returning the same comment in two different fetches, etc.).

The existing canonical scenarios on the revision dispatcher (`Failed revision posts a failure comment`, `Revision cap per PR, with one-time decline`, `AskUser during revision escalates without committing`, `Per-PR state file persists revision count and last-seen timestamp`) are unaffected. Their behavior is preserved; this requirement layers above them to ensure their underlying assumption — "each comment is processed at most once" — actually holds.

#### Scenario: GitHub query uses millisecond precision
- **WHEN** the dispatcher calls `list_issue_comments_since(api_base, token, owner, repo, pr_number, since)` with `since = "2026-05-29T17:18:11.847Z"`
- **THEN** the outgoing HTTP query string contains `since=2026-05-29T17:18:11.847Z` (millisecond precision preserved)
- **AND** the query string does NOT contain `since=2026-05-29T17:18:11Z` (second-precision truncation)

#### Scenario: Marker with zero milliseconds still uses millisecond-precision query
- **WHEN** the dispatcher calls `list_issue_comments_since` with `since` constructed from a `DateTime<Utc>` whose millisecond component is 0 (e.g. `"2026-05-29T17:18:11.000Z"`)
- **THEN** the outgoing HTTP query string contains `since=2026-05-29T17:18:11.000Z`
- **AND** the formatter does NOT strip trailing zero milliseconds back to `since=2026-05-29T17:18:11Z`

#### Scenario: Comment at exact marker timestamp is skipped client-side
- **WHEN** the dispatcher's per-comment loop receives a comment whose `created_at` exactly equals `state.last_seen_comment_at`
- **THEN** the strict-inequality filter skips the comment
- **AND** the bot-author filter is NOT evaluated for this comment
- **AND** the trigger parser is NOT invoked
- **AND** `execute_revision` is NOT called
- **AND** the revision counter is NOT incremented

#### Scenario: Comment before marker timestamp is skipped client-side
- **WHEN** the dispatcher's per-comment loop receives a comment whose `created_at` is strictly less than `state.last_seen_comment_at` (e.g. due to API caching, replication lag, OR a re-fetch that returns historical comments)
- **THEN** the strict-inequality filter skips the comment
- **AND** `execute_revision` is NOT called

#### Scenario: Comment after marker timestamp is processed normally
- **WHEN** the dispatcher's per-comment loop receives a comment whose `created_at` is strictly greater than `state.last_seen_comment_at`
- **THEN** the strict-inequality filter does NOT skip the comment
- **AND** the existing bot-author filter, trigger parser, AND outcome-dispatch path proceed as today

#### Scenario: Same comment re-fetched across polling cycles is processed at most once
- **WHEN** iteration N processes an operator comment at `created_at: T_comment` AND the post-iteration state has `last_seen_comment_at: T_comment`, `revisions_applied: 1`
- **AND** iteration N+1 receives the SAME comment in its `list_issue_comments_since` response (due to GitHub's timestamp-precision behavior OR replication lag)
- **THEN** the strict-inequality filter skips the comment in iteration N+1
- **AND** iteration N+1's `execute_revision` call count is `0`
- **AND** the state after iteration N+1 is unchanged from after iteration N (`revisions_applied: 1`, `last_seen_comment_at: T_comment`)

#### Scenario: AskUser comment is preserved across iterations
- **WHEN** iteration N processes an operator comment AND `execute_revision` returns `AskUser`
- **AND** the marker is NOT advanced past the comment (per the canonical `AskUser during revision escalates without committing` requirement)
- **AND** iteration N+1 receives the SAME comment in its `list_issue_comments_since` response
- **THEN** the strict-inequality filter does NOT skip the comment (because `comment.created_at > state.last_seen_comment_at` — the marker was held back)
- **AND** the comment IS reprocessed in iteration N+1
- **AND** the existing AskUser-resume semantics are preserved

### Requirement: Control socket exposes `record_outcome` AND `consume_outcome` actions for execution-scoped outcome storage

The control socket SHALL accept two new actions that mediate outcome-signaling between the per-execution MCP child AND the executor's classifier:

- **`record_outcome`** — writes a recorded outcome to the daemon's execution-scoped outcome store, keyed by `(workspace_basename, change)`. Request shape:

  ```json
  {
    "action": "record_outcome",
    "workspace_basename": "<sanitized basename of the workspace>",
    "change": "<openspec change name>",
    "outcome": { ... variant-tagged payload ... }
  }
  ```

  The `outcome` field is variant-tagged with `"type"`:

  - `"type": "success"` with optional `"final_answer": string` (defaults to empty string on absence).
  - `"type": "spec_needs_revision"` with `"unimplementable_tasks": Array<{ task_id: string, task_text: string, reason: string }>` (non-empty) AND `"revision_suggestion": string` (non-empty).

  Response shape on success: `{ "ok": true }`. Response shape on a malformed payload (missing fields, unknown variant tag, wrong types): `{ "ok": false, "error": "<message>" }`. The handler trusts the relayed payload (the MCP layer validated it before sending); the failure case exists to surface programmer error during development of new clients, NOT to enforce business rules.

  Storage semantics: last-writer-wins. A second `record_outcome` for the same `(workspace_basename, change)` key replaces the prior entry. This handles the corner case where the agent calls an outcome tool twice in the same session (e.g. retry after an error). The classifier consumes whichever entry was last written.

- **`consume_outcome`** — atomically reads AND removes the entry for a given key. Request shape:

  ```json
  {
    "action": "consume_outcome",
    "workspace_basename": "<sanitized basename>",
    "change": "<openspec change name>"
  }
  ```

  Response shape: `{ "ok": true, "outcome": <recorded outcome variant-tagged object OR null> }`. The `outcome` field is `null` when no entry exists for the key. A subsequent `consume_outcome` for the same key returns `null` (the read drained the store).

The daemon-side outcome store SHALL be in-memory (not file-backed). The store's lifecycle matches the daemon's lifecycle; a daemon restart loses any in-flight outcomes. This is acceptable: outcome reporting is synchronous (the MCP tool call happens milliseconds before the wrapped CLI exits AND the classifier's `consume_outcome` runs microseconds after); restart-survives durability is not required AND would create cleanup AND staleness concerns the file marker for `ask_user` deliberately accepts BECAUSE `ask_user` IS asynchronous.

The outcome store MAY periodically evict entries older than a coarse threshold (60 minutes is sufficient) to bound memory growth in the corner case where `consume_outcome` is never called for a recorded key (autocoder crashes between subprocess exit AND classifier drain). Implementation is OPTIONAL for this requirement; the implementer MAY defer it to a follow-on change if the immediate memory pressure is bounded.

Authorization: the same authn / authz that the existing control-socket actions use (per the canonical "Control socket for runtime daemon interaction" requirement) applies unchanged. The MCP child runs in the daemon's trust domain by virtue of being launched by the daemon's executor; no additional auth surface is introduced.

#### Scenario: `record_outcome` followed by `consume_outcome` round-trips a success outcome
- **WHEN** a client sends `{"action":"record_outcome","workspace_basename":"my-repo","change":"a30-foo","outcome":{"type":"success","final_answer":"done"}}` to the control socket
- **AND** receives `{"ok":true}` in response
- **AND** subsequently sends `{"action":"consume_outcome","workspace_basename":"my-repo","change":"a30-foo"}`
- **THEN** the response is `{"ok":true,"outcome":{"type":"success","final_answer":"done"}}`
- **AND** a second `consume_outcome` for the same key returns `{"ok":true,"outcome":null}`

#### Scenario: `record_outcome` round-trips a spec_needs_revision outcome with all fields
- **WHEN** a client sends a `record_outcome` with `{"type":"spec_needs_revision","unimplementable_tasks":[{"task_id":"6.4","task_text":"Manual: SSH...","reason":"no SSH access"}],"revision_suggestion":"Replace 6.4 with a mocked unit test"}` as the outcome
- **AND** subsequently sends `consume_outcome` for the same key
- **THEN** the consumed outcome's `unimplementable_tasks` array AND `revision_suggestion` match the recorded values byte-for-byte
- **AND** the store entry is cleared

#### Scenario: `record_outcome` for an already-occupied key replaces the prior entry
- **WHEN** a client sends a `record_outcome` for key `("my-repo", "a30-foo")` with a `success` variant
- **AND** subsequently sends a second `record_outcome` for the same key with a `spec_needs_revision` variant
- **THEN** a following `consume_outcome` for the key returns the `spec_needs_revision` variant
- **AND** the prior `success` variant is NOT returned

#### Scenario: `consume_outcome` for an unknown key returns null
- **WHEN** a client sends `{"action":"consume_outcome","workspace_basename":"my-repo","change":"never-recorded"}` to a control socket whose outcome store has no matching entry
- **THEN** the response is `{"ok":true,"outcome":null}`

#### Scenario: `record_outcome` with an unknown variant tag returns a structured error
- **WHEN** a client sends `{"action":"record_outcome","workspace_basename":"x","change":"y","outcome":{"type":"unknown_variant","data":{}}}` to the control socket
- **THEN** the response is `{"ok":false,"error":"<message naming the unknown variant tag>"}`
- **AND** the outcome store remains unchanged

#### Scenario: Outcome-store keys are per `(workspace_basename, change)` AND do not collide across repos
- **WHEN** a client sends `record_outcome` for `("repo-a", "a30-foo")` AND another `record_outcome` for `("repo-b", "a30-foo")`
- **THEN** a `consume_outcome` for `("repo-a", "a30-foo")` returns the first entry
- **AND** a `consume_outcome` for `("repo-b", "a30-foo")` returns the second entry
- **AND** neither read drains the other key's entry

### Requirement: Control socket's `record_outcome` action accepts `iteration_request` variant

The `record_outcome` control-socket action (added in `a27a0`) SHALL accept the `iteration_request` variant tag in its `outcome` payload, alongside the existing `success` AND `spec_needs_revision` variants.

Variant payload shape:

```json
{
  "type": "iteration_request",
  "completed_tasks": ["1", "2"],
  "remaining_tasks": ["3"],
  "reason": "task 3 needs a refactor I want to plan more carefully"
}
```

All three fields are required. The handler trusts the payload (the MCP layer validated it before relaying) AND stores it as the corresponding `RecordedOutcome::IterationRequest` enum variant. The store's last-writer-wins semantics (per a27a0) apply: a second `record_outcome` for the same `(workspace_basename, change)` key replaces the prior entry.

The `consume_outcome` action's response shape SHALL include `iteration_request` payloads with the same field set (the handler returns whatever was stored).

#### Scenario: `record_outcome` accepts iteration_request variant
- **WHEN** a client sends `{"action":"record_outcome","workspace_basename":"my-repo","change":"a30-foo","outcome":{"type":"iteration_request","completed_tasks":["1","2"],"remaining_tasks":["3"],"reason":"..."}}`
- **THEN** the response is `{"ok":true}`
- **AND** a subsequent `consume_outcome` for the same key returns the recorded payload byte-for-byte

#### Scenario: `consume_outcome` returns iteration_request payload
- **WHEN** a client has recorded an `iteration_request` outcome AND subsequently sends `consume_outcome` for the same key
- **THEN** the response shape is `{"ok":true,"outcome":{"type":"iteration_request","completed_tasks":[...],"remaining_tasks":[...],"reason":"..."}}`
- **AND** the store entry is cleared

### Requirement: Polling loop handles `IterationRequested` by committing WIP, pushing, marking, AND dropping the lock — without touching any PR

When the polling loop receives `ExecutorOutcome::IterationRequested { completed_tasks, remaining_tasks, reason, iteration_number }` from the executor, it SHALL perform the following actions in order:

1. **Commit the workspace's diff to the agent branch.** Commit message: `iteration <iteration_number> of <change>: <reason-truncated-to-80-chars>`. If the working tree is clean (the agent emitted `outcome_request_iteration` without modifying any files), the polling loop SHALL skip the commit step, emit `tracing::warn!` naming the anomaly, AND proceed to step 3 (the marker is still useful for the next iteration; the lack-of-progress will count against the cap on the next iteration request).
2. **Force-push the agent branch to the remote.** Push failure aborts the sequence: the polling loop emits `tracing::error!` naming the failure, SKIPS steps 3, AND proceeds to step 4 (drop lock). The change reverts to normal pending behavior on the next polling cycle.
3. **Write `.iteration-pending.json`** using atomic tempfile + rename, with the payload `{ completed_tasks, remaining_tasks, reason, iteration_number }`.
4. **Drop `.in-progress`** per the existing canonical "Unlocking after any executor outcome" requirement.

The polling loop SHALL NOT call any PR-open, PR-comment, OR PR-close routine on the `IterationRequested` arm. PR lifecycle is reserved for the `Completed` outcome (today's behavior, unchanged). An iteration sequence can run entirely without ever opening a PR; the PR opens on the FINAL iteration's `Completed` outcome AND the PR body includes the accumulated implementation-notes content from all prior iterations (per the implementer's open question in design.md; this is implementer scope, not spec-binding).

After step 4 completes, the polling loop continues normally. The next polling iteration on this repo picks up the iteration-pending change ahead of any alphabetically-earlier pending sibling (per the queue-engine deltas in this change).

#### Scenario: IterationRequested commits, pushes, marks, drops lock
- **WHEN** the polling loop receives `ExecutorOutcome::IterationRequested { completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "...", iteration_number: 2 }` AND the workspace has a dirty diff
- **THEN** the loop commits the diff with message `iteration 2 of <change>: <truncated reason>`
- **AND** force-pushes the agent branch to the remote
- **AND** writes `.iteration-pending.json` atomically with the documented payload
- **AND** drops `.in-progress`
- **AND** does NOT call any PR-related routine

#### Scenario: Clean working tree on IterationRequested skips commit, still writes marker
- **WHEN** the polling loop receives `ExecutorOutcome::IterationRequested { ... }` AND the workspace has no diff
- **THEN** the loop skips the commit step
- **AND** emits `tracing::warn!` naming the clean-tree anomaly
- **AND** still writes `.iteration-pending.json` atomically
- **AND** drops `.in-progress`

#### Scenario: Push failure aborts marker write, drops lock
- **WHEN** the polling loop receives `ExecutorOutcome::IterationRequested { ... }` AND the commit succeeds BUT the force-push fails (network error, upstream rejection, etc.)
- **THEN** the loop emits `tracing::error!` naming the push failure
- **AND** does NOT write `.iteration-pending.json`
- **AND** drops `.in-progress`
- **AND** the next polling cycle sees the change as a normal pending entry (no front-insertion preference)

#### Scenario: No PR is opened or modified during iteration sequence
- **WHEN** the polling loop processes an iteration sequence (iteration 1 → IterationRequested → iteration 2 → IterationRequested → iteration 3 → Completed)
- **THEN** no PR is opened OR commented on during iterations 1 AND 2
- **AND** the PR is opened on iteration 3's `Completed` outcome (today's behavior)
- **AND** the iteration 3 PR body reflects the cumulative work from iterations 1, 2, AND 3

### Requirement: Brownfield-survey polling-iteration handler produces a capability list AND persists `BrownfieldSurveyState`
The polling iteration SHALL, after processing other chatops-driven request queues AND before standard change processing, drain at most one pending brownfield-survey request from `pending_brownfield_survey_requests`. The handler SHALL invoke the executor in survey mode with `WritePolicy::None` AND a sandbox profile permitting `Read`, `Glob`, `Grep`, AND `Bash` (read-only).

The survey prompt SHALL be loaded via `PromptLoader::load(PromptId::BrownfieldSurvey, &workspace_config)` (per the executor spec, established by `a24`). The prompt input SHALL include:

1. The resolved prompt template.
2. The operator's guidance, when non-empty, in a `## Operator guidance` section.
3. `README.md` contents AND the list of `docs/*.md` filenames.
4. A code-symbol overview (`cargo metadata` for Rust workspaces; ripgrep for other languages).
5. The list of already-specced capabilities — directories present under `<spec-root>/specs/` where `<spec-root>` honors `a26`'s `spec_storage.path` config when set. These SHALL be excluded from the survey output by instruction.
6. `features.brownfield_survey.max_capabilities` (passed into the prompt context so the LLM respects the cap).

The executor's response SHALL be a JSON array. Each item SHALL have:

- `id: usize` — 1-indexed sequential identifier.
- `slug: String` — proposed capability slug; matches `^[a-z][a-z0-9-]*$`.
- `summary: String` — one-line description.
- `scope_in: String` — short paragraph naming what's IN.
- `scope_out: String` — short paragraph naming related concerns NOT in this capability.
- `source_modules: Vec<String>` — source-tree paths the capability covers.
- `estimated_complexity: String` — `"small" | "medium" | "large"`.

The handler SHALL validate the response: well-formed JSON, every item has all required fields, slug matches the regex AND is NOT in the already-specced set, complexity is in the allowed set, AND `items.len() <= features.brownfield_survey.max_capabilities`. On validation failure: post a thread reply naming the failure AND do not persist state.

On success, the handler SHALL persist `BrownfieldSurveyState` at `<workspace>/.state/brownfield_surveys/<request_id>.json` with `status: Pending` AND each `SurveyItem.status: pending`. The handler SHALL render the list to the lifecycle thread (one section per item, grouped in the order returned) AND append the closing note `Reply with @<bot> send it to batch-generate ALL <N> specs (one per iteration). Or re-run @<bot> brownfield-survey <repo> <refined guidance> to refresh.`

#### Scenario: Happy-path survey run
- **WHEN** the executor returns a valid JSON list of 8 capabilities AND none of them collide with existing `<spec-root>/specs/<cap>/` directories
- **THEN** the handler persists `BrownfieldSurveyState` with 8 items, all `pending`, AND the survey `status: Pending`
- **AND** the thread reply lists 8 numbered items with the closing send-it instruction

#### Scenario: Already-specced capability excluded
- **WHEN** the executor's response includes an item with `slug: "scheduler"` AND `openspec/specs/scheduler/` already exists in the workspace
- **THEN** the handler rejects the response via validation
- **AND** the thread reply names the collision so the operator can re-run with refined guidance

#### Scenario: Slug regex violation rejects the run
- **WHEN** the response includes an item with `slug: "Bad_Slug"` (uppercase / underscore)
- **THEN** the handler rejects the run via validation
- **AND** the thread reply names the slug-regex failure

#### Scenario: Max-capabilities cap enforced
- **WHEN** `features.brownfield_survey.max_capabilities: 10` AND the executor returns 15 items
- **THEN** the handler rejects the run via validation
- **AND** the thread reply names the cap violation

#### Scenario: Survey uses spec_storage.path when set
- **WHEN** the workspace has `spec_storage.path: "../my-specs"` set per `a26`
- **THEN** the already-specced-capabilities listing reads from `../my-specs/openspec/specs/`, NOT `<workspace>/openspec/specs/`
- **AND** the survey persistence at `<workspace>/.state/brownfield_surveys/<request_id>.json` remains in the code workspace (state files always live with their workspace, not the spec_storage repo)

### Requirement: Brownfield-batch polling-iteration handler drains one survey item per iteration AND runs brownfield generation per item
On receipt of a `BrownfieldBatchAction { survey_request_id, channel, thread_ts }`, the daemon SHALL:

1. Load the referenced `BrownfieldSurveyState`. If the file is missing (cleared between dispatch AND processing), post `✗ send it: survey state <request_id> not found (was it cleared?). Re-run brownfield-survey for a fresh list.` AND return.
2. If the survey's `status` is `InProgress` OR `Completed`, post a thread reply naming the no-op AND return without changing state.
3. If ANY other survey on the same workspace has `status: InProgress`, post `✗ send it: a brownfield batch is already in progress for this workspace (survey <other-request_id>). Wait for it to finish OR run @<bot> clear-survey <repo> to abort.` AND return. Only ONE batch per workspace at a time.
4. Otherwise, transition `status` to `InProgress` (atomic-rename) AND post `✓ Queued <N> capability spec generations. The first will start on the next iteration.`

Each subsequent polling iteration SHALL, after processing other queues AND before standard change processing, drain ONE item from the in-progress survey:

1. Identify the workspace's in-progress survey (only one possible).
2. Find the first `SurveyItem` whose status is `pending`.
3. Re-check `<spec-root>/specs/<slug>/spec.md` does NOT exist (where `<spec-root>` honors `a26`'s `spec_storage.path`). If it does exist, mark the item `skipped` (the operator may have merged a sibling brownfield PR) AND return without invoking the executor.
4. Mark the item `generating`.
5. Run the canonical brownfield-generation flow from `a23` for the item's `slug` with the following prompt-input extension: APPEND a `## Survey context` section to the brownfield prompt containing the item's `scope_in`, `scope_out`, AND `source_modules`. The LLM SHALL use this to scope its draft appropriately.
6. On `Completed` outcome with valid change-directory artifacts AND successful PR creation: mark the item `completed`, persist `pr_url`, post `✅ Spec PR opened for \`<slug>\` (M/N done): <pr-url>` to the lifecycle thread.
7. On any failure (executor `Err`, missing artifacts, sandbox leak, PR-create failure): mark the item `failed`, persist `failure_reason`, post `✗ Spec for \`<slug>\` failed: <reason> (continuing with next)`.
8. When ALL items in the survey reach a terminal state (`completed`, `skipped`, OR `failed`), transition the survey `status` to `Completed` AND post the summary: `✅ Brownfield batch complete. <X> succeeded, <Y> skipped (already specced), <Z> failed. See the survey thread for individual PR links AND failure reasons.`

The batch handler does NOT process more than one item per polling iteration even if multiple are `pending`. The one-per-iteration discipline gives each brownfield run its own fresh executor invocation, eliminating mid-batch context compression as a failure mode.

#### Scenario: Batch start acknowledges queue size
- **WHEN** a `BrownfieldBatchAction` arrives for a survey with 5 pending items AND no other batch is in progress
- **THEN** the survey's `status` transitions to `InProgress`
- **AND** the thread receives `✓ Queued 5 capability spec generations. The first will start on the next iteration.`

#### Scenario: One item per iteration
- **WHEN** the in-progress survey has 5 pending items
- **THEN** iteration N processes item 1; iteration N+1 processes item 2; etc.
- **AND** no iteration processes more than one item from the survey

#### Scenario: Spec-already-exists triggers skip mid-batch
- **WHEN** item 3 (slug `auth`) is the next pending item AND between iterations the operator manually merged a sibling brownfield PR creating `openspec/specs/auth/spec.md`
- **THEN** the iteration marks item 3 `skipped` AND posts the skip notice
- **AND** does NOT invoke the executor for item 3
- **AND** the next iteration processes item 4

#### Scenario: Generation failure does not abort the batch
- **WHEN** item 2 generation fails (e.g., executor returns Failed with reason `revision suggestion: scope is unclear`)
- **THEN** item 2 is marked `failed` with the reason persisted
- **AND** the thread receives `✗ Spec for \`<slug>\` failed: revision suggestion: scope is unclear (continuing with next)`
- **AND** the next iteration processes item 3
- **AND** the batch does NOT abort

#### Scenario: All items terminal triggers summary
- **WHEN** the last `pending` item reaches a terminal state
- **THEN** the survey's `status` transitions to `Completed`
- **AND** the thread receives the batch-complete summary with success / skipped / failed counts

#### Scenario: Concurrent batch rejection
- **WHEN** a `BrownfieldBatchAction` arrives for survey A while survey B is already `InProgress` on the same workspace
- **THEN** survey A's status remains `Pending`
- **AND** the thread reply names survey B's request_id AND advises waiting OR clearing

#### Scenario: Spec_storage.path applies to batch
- **WHEN** the workspace has `spec_storage.path` configured AND the batch handler runs
- **THEN** every spec PR is created in the spec_storage repo, NOT the code workspace
- **AND** the per-item `pr_url` records the spec_storage repo's PR URL

### Requirement: `features.brownfield_survey` config schema
The per-repo config schema SHALL accept an optional `features.brownfield_survey` block:

- `enabled: bool` (default `true`) — when `false`, the `brownfield-survey`, `send it`-in-survey-thread, AND `clear-survey` verbs are refused at parse time.
- `prompt_path: Option<String>` (default `None`) — per the uniform PromptLoader pattern.
- `max_capabilities: usize` (default `20`, valid range `1..=50`) — cap on survey item count.

Invalid values cause config-load to fail-fast with a clear error.

#### Scenario: Default config enables the survey verbs
- **WHEN** a per-repo config omits the `features.brownfield_survey` block
- **THEN** all three fields take their defaults

#### Scenario: Explicit disable refuses all three related verbs
- **WHEN** a per-repo config sets `features.brownfield_survey.enabled: false`
- **THEN** the dispatcher refuses `@<bot> brownfield-survey`, refuses `@<bot> send it` when posted in a (still-present) survey thread, AND refuses `@<bot> clear-survey` for that workspace

#### Scenario: max_capabilities outside valid range fails config load
- **WHEN** a per-repo config sets `features.brownfield_survey.max_capabilities: 100`
- **THEN** config-load fails with an error naming the field AND the valid range `1..=50`

### Requirement: Per-PR state file tracks code-review counts AND suggestion deduplication

The per-PR `RevisionState` JSON (at `<workspace>/.autocoder/revisions/<pr_number>.json`, per the canonical `Per-PR state file persists revision count and last-seen timestamp; closed PRs are pruned` requirement) SHALL gain the following fields. All fields SHALL have serde defaults so existing state files load cleanly without migration:

- `code_reviews_applied: u32` (default `0`). Counts operator-initiated re-reviews triggered via `@<bot> code-review`. Does NOT count the original automatic review at PR-open time.
- `code_review_cap: u32` (default populated from `reviewer.max_code_reviews_per_pr` config at write time; falls back to `5` if config is absent during deserialization). Per-PR upper bound on operator-initiated re-reviews.
- `cap_decline_posted_for_code_review: bool` (default `false`). Set `true` after the one-time cap-decline PR comment AND chatops notification are posted on cap exceeded. Prevents repeated decline messages.
- `last_suggested_rereview_at_revisions_count: Option<u32>` (default `None`). Records the `revisions_applied` count at which the most recent re-review suggestion fired. Used to deduplicate the suggestion across polling cycles on the same revision count.
- `original_review_head_sha: Option<String>` (default `None`). Records the agent-branch head SHA at the time the original automatic review completed. Set by the polling-loop's reviewer-completion path. Used as the baseline for the diff-overlap suggestion. State files written before this change deployed have this field as `None`; the suggestion path gracefully degrades to "no suggestion" in that case.

The state file's atomic-write semantics (per the existing canonical `State writes are atomic` requirement) are preserved unchanged.

The pruning behavior for closed PRs (per the existing canonical `Closed PRs have their state pruned` requirement) applies to the extended state file unchanged: when a PR closes, its entire state file is removed, including the new fields.

#### Scenario: New fields default cleanly when loading legacy state files
- **WHEN** the daemon loads a `RevisionState` JSON that was written by an older daemon AND contains NO `code_reviews_applied`, `code_review_cap`, `cap_decline_posted_for_code_review`, `last_suggested_rereview_at_revisions_count`, OR `original_review_head_sha` fields
- **THEN** the loaded `RevisionState` has `code_reviews_applied: 0`, `code_review_cap: 5` (the documented default), `cap_decline_posted_for_code_review: false`, `last_suggested_rereview_at_revisions_count: None`, AND `original_review_head_sha: None`
- **AND** no error is logged

#### Scenario: New fields round-trip cleanly when populated
- **WHEN** the daemon writes a `RevisionState` with `code_reviews_applied: 3`, `code_review_cap: 5`, `cap_decline_posted_for_code_review: false`, `last_suggested_rereview_at_revisions_count: Some(2)`, AND `original_review_head_sha: Some("abc123def")`
- **AND** the file is read back
- **THEN** the deserialized `RevisionState` matches the written values byte-for-byte

#### Scenario: Original-review-head-sha populated by polling-loop completion path
- **WHEN** the polling-loop's reviewer-completion code (the path that today writes `## Code Review` into the PR body) completes successfully for the FIRST review on a PR
- **THEN** the daemon writes `state.original_review_head_sha = Some(<current agent-branch head SHA>)` to the per-PR state file
- **AND** the state file write uses atomic-rename semantics (per the existing canonical `State writes are atomic` requirement)

#### Scenario: Re-review path does NOT overwrite original_review_head_sha
- **WHEN** an operator-initiated re-review (via `@<bot> code-review`) completes successfully
- **THEN** `state.code_reviews_applied` increments
- **AND** `state.original_review_head_sha` is NOT modified (the baseline for the suggestion's overlap calculation must remain the ORIGINAL review's head SHA, not subsequent re-reviews' SHAs)

#### Scenario: Cap field is independent of revision cap
- **WHEN** the daemon loads a state file with `revisions_applied: 5`, `revision_cap: 5`, `code_reviews_applied: 2`, AND `code_review_cap: 5`
- **THEN** an operator `@<bot> revise` comment is rejected as cap-exceeded (revisions are at cap)
- **AND** an operator `@<bot> code-review` comment IS dispatched (re-reviews are below cap; the two cap counters are independent)

### Requirement: Polling iteration classifies outcome as spec-only, code-only, OR dual-tree before commit + push + PR

The polling iteration's commit + push + PR step SHALL begin with a working-tree-status classification:

1. Run `git -C <code_workspace> status --porcelain` AND check for non-empty output.
2. When `spec_storage` is configured for the repo, run `git -C <spec_storage.path> status --porcelain` AND check for non-empty output.
3. Classify the iteration's outcome as:
   - **Code-only**: code workspace dirty AND spec_storage clean (OR not configured).
   - **Spec-only**: code workspace clean AND spec_storage dirty.
   - **Dual-tree**: both dirty.
   - **Clean**: both clean. (No commit + push + PR happens; the iteration's outcome was Completed with no diff, handled by the existing "exit-0 without modifying workspace" path.)

The classification determines which working trees are committed AND pushed AND which PRs are opened.

#### Scenario: Spec-only iteration commits to spec_storage tree only
- **WHEN** a polling iteration completes AND the code workspace is clean AND the spec_storage working tree has new files (e.g. brownfield draft, scout spec-it write, OR `openspec archive` rename)
- **THEN** the iteration's commit step runs `git -C <spec_storage.path> commit ...`
- **AND** the code workspace's working tree is NOT committed
- **AND** the push step targets the spec_storage repo's remote (per the resolution requirement below)

#### Scenario: Code-only iteration commits to code workspace tree only
- **WHEN** a polling iteration completes AND the code workspace is dirty AND the spec_storage working tree is clean (OR not configured)
- **THEN** the iteration's commit step runs `git -C <code_workspace> commit ...` (existing canonical behavior)
- **AND** the spec_storage repo is NOT committed (when configured AND clean)

#### Scenario: Dual-tree iteration produces TWO PRs
- **WHEN** a polling iteration completes AND BOTH the code workspace AND spec_storage working tree are dirty (the iteration drafted spec changes AND modified code-workspace fixtures)
- **THEN** the commit + push + PR step runs against BOTH working trees independently
- **AND** TWO PRs are opened (one per repo) with their respective title shapes (per the title-prefix requirement below)
- **AND** chatops notifications fire for each PR independently

### Requirement: Spec-storage push remote AND base branch resolution rules

When the polling iteration's classification is spec-only OR dual-tree AND `spec_storage` is configured, the push remote AND PR base branch SHALL be resolved per the following rules:

- **Push remote**: `spec_storage.push_remote` (new optional field; default `None`). When `None`, the runtime uses `"origin"`. The resolved value MUST exist in `git -C <spec_storage.path> remote` output; config-load SHALL fail-fast if the field is set to a non-existent remote name.
- **Base branch**: `spec_storage.base_branch` (new optional field; default `None`). When `None`, the runtime queries `git -C <spec_storage.path> symbolic-ref refs/remotes/<push_remote>/HEAD` AND parses the branch name (e.g. `refs/remotes/origin/main` → `main`). When the symbolic-ref query fails, fall back to `"main"`.
- **Spec-repo owner/name**: parsed from `git -C <spec_storage.path> remote get-url <push_remote>`. SSH (`git@github.com:owner/name.git`) AND HTTPS (`https://github.com/owner/name.git`) URL forms SHALL both be parsed. On parse failure, the iteration SHALL log WARN AND fall back to the code workspace's owner/name (degrades to opening the PR against the wrong repo; clearly visible to the operator).

The resolution SHALL happen once per polling iteration AND the resolved values SHALL be threaded through the commit + push + PR steps explicitly (no re-resolution mid-step).

#### Scenario: Default resolution uses `origin` AND remote-tracked HEAD
- **WHEN** a spec-only iteration runs AND `spec_storage.push_remote` AND `spec_storage.base_branch` are both unset
- **THEN** the resolved push remote is `"origin"`
- **AND** the resolved base branch is the branch name parsed from `git -C <spec_storage.path> symbolic-ref refs/remotes/origin/HEAD`

#### Scenario: Operator overrides take precedence
- **WHEN** `spec_storage.push_remote: "upstream-fork"` AND `spec_storage.base_branch: "develop"` are set
- **THEN** the resolved push remote is `"upstream-fork"`
- **AND** the resolved base branch is `"develop"`
- **AND** the iteration's `git push` targets `upstream-fork` AND the PR's `--base` is `develop`

#### Scenario: Push-remote validation at config-load
- **WHEN** config-load encounters `spec_storage.push_remote: "nonexistent-remote"` AND running `git -C <spec_storage.path> remote` returns a set that does NOT include `nonexistent-remote`
- **THEN** config-load fails with a message naming the missing remote AND the available remotes
- **AND** the daemon exits non-zero before any polling task is spawned

#### Scenario: Symbolic-ref query failure falls back to `main`
- **WHEN** `spec_storage.base_branch` is unset AND `git -C <spec_storage.path> symbolic-ref refs/remotes/origin/HEAD` returns non-zero (e.g. the remote has no default branch set)
- **THEN** the iteration logs WARN naming the failure
- **AND** the resolved base branch is `"main"` (the documented fallback)

### Requirement: Spec-only AND dual-tree's spec PR title is prefixed `[specs] `

PRs whose entire diff lives under `openspec/` SHALL have their titles prefixed with `[specs] `. This applies to:

- Spec-only iterations' PRs.
- The spec-storage PR half of dual-tree iterations.

Code-only iterations' PRs AND the code PR half of dual-tree iterations SHALL remain unprefixed (existing format preserved).

The prefix is operator-visible AND lets operators sort PR lists by title to find spec-only PRs quickly. It does NOT affect any automated processing — the revisions dispatcher, the reviewer, AND the chatops notifications all key on PR number, not title.

#### Scenario: Spec-only PR title carries the prefix
- **WHEN** a spec-only iteration produces a PR for a brownfield draft change `a36-brownfield-foo` (+ 0 more)
- **THEN** the PR title is `[specs] a36-brownfield-foo`

#### Scenario: Code-only PR title is unprefixed
- **WHEN** a code-only iteration produces a PR for change `a40-fix-bar` (+ 0 more)
- **THEN** the PR title is `a40-fix-bar` (no `[specs] ` prefix)

#### Scenario: Dual-tree produces one prefixed AND one unprefixed PR
- **WHEN** a dual-tree iteration produces two PRs for change `a42-mixed-baz`
- **THEN** the spec PR title is `[specs] a42-mixed-baz`
- **AND** the code PR title is `a42-mixed-baz`

### Requirement: Path-literals scanner SHALL be enabled unconditionally in CI

The path-literals audit test `no_removed_paths_global_accessor_references_in_src` in `autocoder/tests/path_literals_audit.rs` SHALL run unconditionally as part of `cargo test`. The `#[ignore]` attribute that was applied during the `a27-thread-daemon-paths` archive (with the comment `"enable once a27 removes all paths::current()/install_global()/test_fallback()/get_global() call sites"`) SHALL be removed in this change AND SHALL NOT be reintroduced.

The scanner enforces the canonical `Production paths SHALL be threaded through APIs, NOT read from a process-global` requirement's "CI scanner blocks reintroduction" scenario. Without active enforcement, the canonical's hard rule is unenforced — any future change can reintroduce a global path accessor without surfacing the regression.

The scanner SHALL match the literal substrings `paths::current`, `paths::install_global`, `paths::test_fallback`, AND `paths::get_global` against `autocoder/src/**/*.rs` source files. The scanner's own constants are constructed at runtime from fragments so it does NOT match itself (existing canonical behavior, preserved).

#### Scenario: Scanner runs unconditionally as part of `cargo test`
- **WHEN** a developer runs `cargo test` against the autocoder crate
- **THEN** `no_removed_paths_global_accessor_references_in_src` runs (NOT skipped via `#[ignore]`)
- **AND** the test passes IF the source tree contains no forbidden symbol references
- **AND** the test FAILS the build IF any forbidden symbol appears in any `autocoder/src/**/*.rs` file

#### Scenario: Scanner failure names the offending file AND symbol
- **WHEN** a synthetic `paths::current()` reference is inserted into `autocoder/src/some_module.rs`
- **AND** `cargo test no_removed_paths_global_accessor_references_in_src` runs
- **THEN** the test fails with a message naming `autocoder/src/some_module.rs` AND the symbol `paths::current`
- **AND** the failure message references the canonical `Production paths SHALL be threaded through APIs` requirement as the resolution

#### Scenario: Reintroduction of `#[ignore]` is forbidden
- **WHEN** a future change attempts to add `#[ignore = "..."]` back onto `no_removed_paths_global_accessor_references_in_src`
- **THEN** the change SHALL be rejected at review time (this requirement explicitly forbids the reintroduction)
- **AND** the canonical "CI scanner blocks reintroduction" scenario from the `Production paths SHALL be threaded` requirement remains active

### Requirement: Per-module migration of the production files that previously read paths globals

The 13 production files enumerated below SHALL be migrated to threaded `DaemonPaths` per the canonical "Production paths SHALL be threaded through APIs" requirement's two-pattern model (constructor-field pattern for struct-shaped modules; function-parameter pattern for free-function modules). After this change lands, EACH of the enumerated files SHALL be free of references to the four forbidden symbols.

The enumeration captures the per-file migration scope in the canonical record. The canonical's CI scanner enforces the invariant generically; this requirement makes the per-file scope auditable AND prevents accidental partial migration.

Files SHALL be migrated to the canonical pattern indicated:

| File | Pattern |
|------|---------|
| `autocoder/src/revisions.rs` | function-parameter |
| `autocoder/src/alert_state.rs` | function-parameter |
| `autocoder/src/workspace.rs` | function-parameter |
| `autocoder/src/busy_marker.rs` | constructor-field |
| `autocoder/src/failure_state.rs` | function-parameter |
| `autocoder/src/control_socket.rs` | constructor-field |
| `autocoder/src/audits/mod.rs` | constructor-field |
| `autocoder/src/audits/scheduler.rs` | constructor-field |
| `autocoder/src/audits/threads.rs` | function-parameter |
| `autocoder/src/proposal_requests.rs` | function-parameter |
| `autocoder/src/changelog_requests.rs` | function-parameter |
| `autocoder/src/executor/claude_cli.rs` | constructor-field |
| `autocoder/src/cli/run.rs` | entrypoint-construction (removes the `install_global` call site) |

The pattern assignment is a recommendation, NOT a binding rule — the implementer MAY choose a different pattern per file IF the chosen pattern still satisfies the canonical "Production paths SHALL be threaded" requirement's two scenarios (constructor-field OR function-parameter). The end state is what's binding: zero references to forbidden symbols in any of the enumerated files (AND in every other `autocoder/src/**/*.rs` file).

This change ships fully runnable. The implementer SHALL NOT split the migration across multiple changes: a half-migrated state where some modules thread paths AND others read the global cannot pass the CI scanner activation requirement above (because the scanner is unconditional). Partial migration is a non-shippable state.

#### Scenario: Every enumerated file is free of forbidden symbols after this change
- **WHEN** the build runs against the state of `autocoder/src/` after this change merges
- **THEN** grepping the source tree for `paths::current`, `paths::install_global`, `paths::test_fallback`, OR `paths::get_global` returns ZERO matches
- **AND** the path-literals audit (now unconditionally active per the requirement above) passes

#### Scenario: Daemon entrypoint constructs the single `Arc<DaemonPaths>` instance
- **WHEN** the daemon starts up after this change
- **THEN** the entrypoint module (`autocoder/src/main.rs` OR `autocoder/src/cli/run.rs::run_daemon`) constructs ONE `Arc<DaemonPaths>` via the env-driven resolution
- **AND** the value is passed to the top-level orchestrator constructor
- **AND** no other code path constructs a second `DaemonPaths` for production use
- **AND** no `install_global` call remains in any source file

#### Scenario: Concurrent test isolation invariant verified
- **WHEN** the test suite runs the new concurrent-isolation test (per task 4.4)
- **AND** two `std::thread::spawn`-spawned threads each invoke `AlertState::load_or_default` with DIFFERENT `Arc<DaemonPaths>` values (constructed via `test_daemon_paths()`)
- **THEN** each thread's write lands under its OWN tempdir
- **AND** neither thread can see the other thread's writes
- **AND** the test passes

#### Scenario: Implementation completion satisfies canonical's existing scenarios
- **WHEN** the build runs after this change
- **THEN** all six existing scenarios in the canonical `Production paths SHALL be threaded through APIs` requirement evaluate as TRUE against the actual code state:
  - "Daemon entrypoint constructs the single instance" — verified by §2.x tasks.
  - "Module constructor accepts paths" — verified by §3.x constructor-field tasks.
  - "Free function accepts paths as parameter" — verified by §3.x function-parameter tasks.
  - "Test constructs its own DaemonPaths" — verified by §4.x test-refactor tasks.
  - "Concurrent tests do not collide on disk" — verified by §4.4 concurrent-isolation test.
  - "CI scanner blocks reintroduction" — verified by §5.x scanner-activation tasks.

### Requirement: `autocoder inspect` subcommand surface for operator diagnostics

The `autocoder` CLI SHALL expose an `inspect` subcommand with three subsubcommands that wrap existing diagnostic data sources in operator-friendly forms. Each subsubcommand exits `0` on success, `2` on operator error (missing arg, unresolvable workspace, unreachable socket, missing log file), AND `1` on internal error (parse failure, IO error, etc.).

All three subsubcommands accept a `--workspace <basename-or-url>` argument. The argument SHALL be resolved as follows:

1. When the argument contains `:` OR starts with `http`/`https`: parsed as a git URL, sanitized to a basename via the existing workspace-basename-sanitization helper.
2. Otherwise: used as a basename verbatim.
3. When omitted AND the daemon's config has exactly one repository: that repository's basename is used.
4. When omitted AND the config has zero OR multiple repositories: the subcommand prints the list of available basenames AND exits `2`.

The workspace resolution rule is uniform across all three subsubcommands.

#### Subsubcommand: `autocoder inspect rag`

`autocoder inspect rag --workspace <basename-or-url> --query "<text>" [--top-k N] [--show-bodies] [--json]` SHALL:

1. Resolve the workspace basename per the rule above.
2. Resolve the control-socket path via `DaemonPaths.control_socket_path()`.
3. Connect via `UnixStream::connect`. On failure: print `error: control socket unreachable at <path>: <error>. Is the daemon running? (systemctl status autocoder)` to stderr AND exit `2`.
4. Send `{"action":"query_canonical_specs","workspace_basename":"<basename>","query":"<query>","top_k":<N>}` (defaulting `top_k` to the daemon's configured `canonical_rag.top_k` when the flag is omitted) on a single line followed by `\n`.
5. Read the single-line JSON response.
6. When `--json` is set: print the raw response to stdout AND exit `0`.
7. When `--json` is NOT set: render a table with columns SCORE, CAPABILITY, REQUIREMENT, BYTES (one row per hit, sorted by descending score), preceded by header lines naming the query, workspace, AND top_k. Below the table, print a one-line summary: `Total response size: <KB> KB (N hits)`.
8. When `--show-bodies` is set: after the table, render one section per hit with `## <capability>/<requirement_title>` followed by the first 500 characters of `requirement_body`.

#### Subsubcommand: `autocoder inspect log`

`autocoder inspect log --workspace <basename-or-url> <change> [--limit N] [--json]` SHALL:

1. Resolve the workspace basename per the rule above.
2. Resolve the stream-log path: `<logs_dir>/runs/<basename>/<change>.stream.log` via `DaemonPaths`.
3. On file-not-found: enumerate `*.stream.log` files in `<logs_dir>/runs/<basename>/`, print `error: no stream log at <path>. Available changes in this workspace: <comma-separated list>` to stderr, AND exit `2`.
4. Parse the stream log: each line is a `[tool_use] ...`, `[tool_result] ...`, OR `[assistant] ...` event with a timestamp prefix.
5. Group `tool_use` events with their matching `tool_result` events (by `tool_use_id` field if present, else by source-order positional pairing).
6. Render a header naming the change, workspace, summary-log path, AND stream-log path.
7. Render at most `--limit N` (default `30`) tool-call event groups, each formatted as `[timestamp] tool_use <name> <input-summary>` followed by `[timestamp] tool_result (<bytes-or-summary>)`. `--limit 0` means unlimited.
8. For `tool_use query_canonical_specs`: the input summary SHALL include the query text AND top_k. The matching `tool_result` summary SHALL include the hit count AND top relevance score.
9. After the tool-call section, render the FINAL ANSWER content from the summary `.log` file (the FINAL ANSWER section that a20a2 split out of the stream log).
10. When `--json` is set: print the parsed event stream as a JSON array AND skip the formatted-rendering AND FINAL ANSWER sections.

#### Subsubcommand: `autocoder inspect tool-usage`

`autocoder inspect tool-usage --workspace <basename-or-url> <change> [--json]` SHALL:

1. Resolve workspace basename AND stream-log path per the same rules as `inspect log`.
2. On file-not-found: same behavior as `inspect log` (error message + exit `2`).
3. Parse the stream log AND aggregate:
   - Duration: from the first event's timestamp to the last event's timestamp.
   - Tool-call counts grouped by tool name.
   - For `query_canonical_specs` calls specifically: total bytes returned (sum of tool_result content sizes), total hits returned (sum across calls), score distribution buckets (`high >= 0.7`, `medium 0.5–0.7`, `low < 0.5`), AND avg hits per call.
4. Render the aggregated stats per the canonical format (named sections for duration, tool calls, AND query_canonical_specs detail when present).
5. When `--json` is set: print the aggregated stats as a structured object AND skip the formatted rendering.

#### Scenario: `autocoder inspect rag` queries the live RAG store
- **WHEN** the operator runs `autocoder inspect rag --workspace github_com_foo_bar --query "audit framework cadence" --top-k 5` against a running daemon
- **THEN** the command connects to the control socket, sends the `query_canonical_specs` action, AND prints a table with one row per hit
- **AND** the table includes the SCORE, CAPABILITY, REQUIREMENT, AND BYTES columns
- **AND** the exit code is 0

#### Scenario: `autocoder inspect rag --json` prints raw response
- **WHEN** the operator passes `--json`
- **THEN** the command prints the raw control-socket response JSON to stdout
- **AND** no formatted table is rendered

#### Scenario: Unreachable control socket produces clear error
- **WHEN** `autocoder inspect rag` runs AND the control socket path does NOT exist (daemon not running OR socket path mismatch)
- **THEN** stderr contains `error: control socket unreachable at <resolved-path>: <error>. Is the daemon running? (systemctl status autocoder)`
- **AND** the exit code is 2

#### Scenario: Workspace omitted with single configured repo auto-selects
- **WHEN** `autocoder inspect rag --query "x"` runs (no `--workspace` flag) AND the daemon's config has exactly one repository
- **THEN** that repository's basename is used automatically
- **AND** the command proceeds without operator prompt

#### Scenario: Workspace omitted with multiple repos exits with list
- **WHEN** `autocoder inspect rag --query "x"` runs AND the daemon's config has more than one repository
- **THEN** stderr contains `error: --workspace required. Available basenames: <comma-separated list>`
- **AND** the exit code is 2

#### Scenario: `autocoder inspect log` renders tool-call-grouped output
- **WHEN** the operator runs `autocoder inspect log --workspace github_com_foo_bar a30-baz`
- **THEN** the command reads `<logs_dir>/runs/github_com_foo_bar/a30-baz.stream.log` AND renders a header followed by tool-call event groups
- **AND** each `query_canonical_specs` tool_use's matching tool_result shows hit count AND top score
- **AND** after the tool-call section, the FINAL ANSWER from the summary log is appended

#### Scenario: `autocoder inspect log` with missing file lists available changes
- **WHEN** the operator runs `autocoder inspect log --workspace github_com_foo_bar nonexistent-change`
- **THEN** stderr contains `error: no stream log at <path>. Available changes in this workspace: <list>`
- **AND** the listed names are the basenames (without `.stream.log` suffix) of files in `<logs_dir>/runs/github_com_foo_bar/`
- **AND** the exit code is 2

#### Scenario: `autocoder inspect tool-usage` produces aggregated stats
- **WHEN** the operator runs `autocoder inspect tool-usage --workspace github_com_foo_bar a30-baz`
- **THEN** the output includes a `duration:` line, a `tool calls:` section with per-tool counts, AND when `query_canonical_specs` calls are present, a `query_canonical_specs detail:` section with bytes returned, score distribution, AND avg hits per call

#### Scenario: URL-form workspace argument sanitizes correctly
- **WHEN** any `inspect` subsubcommand runs with `--workspace git@github.com:foo/bar.git`
- **THEN** the URL is sanitized to `github_com_foo_bar` via the existing workspace-basename-sanitization helper
- **AND** the resulting basename is used for control-socket queries OR log-path resolution

#### Scenario: `inspect rag` reads RAG-store data the daemon owns
- **WHEN** the operator runs `inspect rag` AND the daemon has a `CanonicalRagStore` registered for the workspace
- **THEN** the response's `hits` array reflects the store's actual content at that instant
- **AND** the subcommand does NOT load OR query embeddings independently (the daemon is the single source of truth)

### Requirement: Canonical `LlmProvider` enum AND per-provider auth semantics

The autocoder config schema SHALL define a single canonical `LlmProvider` enum with three variants AND their YAML strings:

- `anthropic` — Anthropic's hosted API (`https://api.anthropic.com` default).
- `openai_compatible` — Any OpenAI-API-shaped endpoint (OpenAI itself, Grok, OpenRouter, vLLM, local OpenAI-compat shims, etc.).
- `ollama` — Ollama's native API (`<base>/api/chat` for completion, `<base>/api/embed` for embeddings).

`LlmProvider` SHALL be the type of the `provider` field across every LLM-touching config block: `reviewer:`, `canonical_rag:`, AND `executor.change_internal_contradiction_check_llm:`. Backward compatibility: the existing `RagProvider` AND `ReviewerProvider` enum names SHALL be retained as type aliases (`pub type RagProvider = LlmProvider;` etc.) so external-crate or test-code consumers compile unchanged. Existing config files using `provider: anthropic`, `provider: openai_compatible`, AND `provider: ollama` parse identically post-spec.

The `api_key` field's mandatory-ness SHALL be determined by the resolved provider, NOT by the subsystem:

- `anthropic` → `api_key` REQUIRED (either via `api_key.value` inline OR `api_key_env` pointing at a set env var). Config-load fails-fast if absent.
- `openai_compatible` → `api_key` REQUIRED. Same fail-fast rule.
- `ollama` → `api_key` FORBIDDEN. Config-load fails-fast if the operator sets one with the message `<subsystem>: ollama does not authenticate; remove api_key field`. This is a behavioral departure from "silently ignore" — operators learn the auth model at startup rather than carrying dummy values forward.

The `api_base_url` field's mandatory-ness SHALL similarly be provider-driven:

- `anthropic` → OPTIONAL (defaults to `https://api.anthropic.com`).
- `openai_compatible` → REQUIRED (no sensible default for a generic compat endpoint).
- `ollama` → REQUIRED (operator's Ollama host).

The `api_base_url` SHALL be treated as the API root by every provider's client. Each client knows what protocol-specific path to append:

- `anthropic` → `<base>/v1/messages`.
- `openai_compatible` → `<base>/chat/completions` (for chat) OR `<base>/embeddings` (for embeddings).
- `ollama` → `<base>/api/chat` (for chat) OR `<base>/api/embed` (for embeddings).

Operators using `openai_compatible` against hosted services that require `/v1` in the URL (OpenAI, Grok, OpenRouter) SHALL include `/v1` in their `api_base_url`. The client does NOT auto-append `/v1`; the convention is "operator owns the API root."

Validation runs ONCE at config-load (not lazily). A misconfigured provider surfaces as a fail-fast error at `systemctl restart autocoder`, not as a 404 OR permission error on first feature trigger.

#### Scenario: `LlmProvider` round-trips through serde
- **WHEN** a config file contains `provider: anthropic` (OR `openai_compatible`, OR `ollama`)
- **THEN** the field deserializes into `LlmProvider::Anthropic` (resp. `OpenAiCompatible`, `Ollama`)
- **AND** re-serializing produces the same YAML string

#### Scenario: `RagProvider` AND `ReviewerProvider` aliases compile
- **WHEN** code references the type names `RagProvider` OR `ReviewerProvider`
- **THEN** the names resolve to `LlmProvider` via type aliases
- **AND** no source-code change is required to consumers of the old type names

#### Scenario: `anthropic` requires `api_key`
- **WHEN** a config block sets `provider: anthropic` AND omits both `api_key` AND `api_key_env`
- **THEN** config-load fails with `<subsystem>: anthropic requires api_key; set <subsystem>.api_key.value or <subsystem>.api_key_env`
- **AND** the daemon exits non-zero before any polling task is spawned

#### Scenario: `openai_compatible` requires `api_key`
- **WHEN** a config block sets `provider: openai_compatible` AND omits both `api_key` AND `api_key_env`
- **THEN** config-load fails with `<subsystem>: openai_compatible requires api_key; set <subsystem>.api_key.value or <subsystem>.api_key_env`

#### Scenario: `openai_compatible` requires `api_base_url`
- **WHEN** a config block sets `provider: openai_compatible` AND omits `api_base_url`
- **THEN** config-load fails with `<subsystem>: openai_compatible requires api_base_url; set the field to e.g. https://api.openai.com/v1`

#### Scenario: `ollama` forbids `api_key`
- **WHEN** a config block sets `provider: ollama` AND sets `api_key.value` OR `api_key_env`
- **THEN** config-load fails with `<subsystem>: ollama does not authenticate; remove api_key field`
- **AND** the failure message names that Ollama silently ignores Authorization headers

#### Scenario: `ollama` requires `api_base_url`
- **WHEN** a config block sets `provider: ollama` AND omits `api_base_url`
- **THEN** config-load fails with `<subsystem>: ollama requires api_base_url; set the field to e.g. http://localhost:11434`

#### Scenario: `anthropic` defaults `api_base_url` cleanly
- **WHEN** a config block sets `provider: anthropic`, `api_key.value: <some-key>`, AND omits `api_base_url`
- **THEN** config-load succeeds
- **AND** the resolved `api_base_url` is `https://api.anthropic.com`

### Requirement: Per-subsystem provider validity is enforced at config-load

Different LLM-using subsystems have different supported provider sets. Validity SHALL be enforced at config-load with a clear actionable error.

Subsystem validity table:

- `reviewer.provider` → `anthropic | openai_compatible | ollama` (all three valid; reviewer does completion).
- `executor.change_internal_contradiction_check_llm.provider` → `anthropic | openai_compatible | ollama` (all three valid; same shape as reviewer).
- `canonical_rag.provider` → `openai_compatible | ollama` (anthropic INVALID; Anthropic does not expose an embeddings API).

When an operator picks a provider NOT in a subsystem's valid set, config-load SHALL fail with the message `<subsystem> does not support provider '<rejected>'; available providers: <comma-separated valid list>` AND the daemon SHALL exit non-zero before any polling task is spawned.

#### Scenario: `canonical_rag.provider: anthropic` rejected
- **WHEN** a config file contains `canonical_rag: { enabled: true, provider: anthropic, ... }`
- **THEN** config-load fails with `canonical_rag does not support provider 'anthropic'; available providers: ollama, openai_compatible`
- **AND** the daemon exits non-zero

#### Scenario: `reviewer.provider: ollama` accepted
- **WHEN** a config file contains `reviewer: { enabled: true, provider: ollama, model: <model>, api_base_url: http://localhost:11434, ... }` (no api_key)
- **THEN** config-load succeeds
- **AND** the resolved reviewer config carries `LlmProvider::Ollama` AND `api_base_url: http://localhost:11434`
- **AND** the daemon proceeds with normal startup

#### Scenario: `change_internal_contradiction_check_llm.provider: ollama` accepted
- **WHEN** the contradiction-check LLM is configured with `provider: ollama` AND a base URL AND no api_key
- **THEN** config-load succeeds
- **AND** the contradiction-check uses the new `OllamaChatClient`

### Requirement: Integration test verifies `IterationRequested` arm writes the iteration-pending marker AND clears the in-progress lock

A canonical integration test SHALL exercise the polling-loop's `IterationRequested` outcome arm end-to-end against a temp workspace + temp bare git repo fixture AND assert two filesystem postconditions:

1. `<workspace>/openspec/changes/<change>/.iteration-pending.json` exists on disk after the arm completes AND parses to a payload containing `completed_tasks`, `remaining_tasks`, `reason`, AND `iteration_number` per `a27a1`'s "Iteration-pending marker file in the change directory carries state across iteration boundaries" requirement.
2. `<workspace>/openspec/changes/<change>/.in-progress` does NOT exist on disk after the arm completes, per the canonical openspec-queue-engine "Unlocking after any executor outcome" requirement.

This test pins the implementation against the silent-drop failure mode observed in production: the canonical specs require both filesystem effects, the implementation can silently skip either one, AND unit-level tests of the helpers don't exercise the integration. A failure in either postcondition fails the build with a clear message naming which file is missing AND which canonical requirement governs the expectation.

The test SHALL also assert that the iteration_request commit IS present on the agent branch (commit-count > 0) so a regression where the IterationRequested arm fails the commit + push step BUT still drops the lock is caught.

#### Scenario: IterationRequested arm writes marker AND clears in-progress
- **WHEN** the integration test drives a polling iteration whose stub executor returns `IterationRequested { completed_tasks: ["1", "2"], remaining_tasks: ["3"], reason: "scope-overflow", iteration_number: 2 }`
- **AND** the polling loop's `IterationRequested` arm runs end-to-end (commit + force-push + marker write + lock cleanup)
- **THEN** `<workspace>/openspec/changes/<change>/.iteration-pending.json` exists
- **AND** the file parses to `{"completed_tasks": ["1", "2"], "remaining_tasks": ["3"], "reason": "scope-overflow", "iteration_number": 2}` byte-for-byte
- **AND** `<workspace>/openspec/changes/<change>/.in-progress` does NOT exist
- **AND** `git rev-list --count <base>..<agent>` returns 1 (the iteration_request WIP commit was pushed)

#### Scenario: Missing marker fails the test with a clear message
- **WHEN** a hypothetical implementation regression causes the IterationRequested arm to skip the marker write
- **THEN** the integration test fails with a message naming the missing path (`<workspace>/openspec/changes/<change>/.iteration-pending.json`) AND the canonical `a27a1` "Iteration-pending marker file..." requirement as the resolution target

#### Scenario: Stale .in-progress fails the test with a clear message
- **WHEN** a hypothetical implementation regression causes the IterationRequested arm to skip the `.in-progress` cleanup
- **THEN** the integration test fails with a message naming the unexpected file (`<workspace>/openspec/changes/<change>/.in-progress`) AND the canonical "Unlocking after any executor outcome" requirement as the resolution target

#### Scenario: Both filesystem effects exercised in one test (NOT two)
- **WHEN** the integration test is designed
- **THEN** both postconditions are asserted in the SAME test against the SAME fixture (NOT split across two tests)
- **AND** the rationale: a single test that runs the full arm once AND asserts both effects catches the "implementation does the commit + push but neither filesystem effect" failure mode cleanly. Splitting into two tests can let the second test pass against a fixture that the first test mutated, masking the same-arm failure

### Requirement: Partial change-slug resolution in marker-clearing control-socket actions
The four marker-clearing control-socket actions — `clear_perma_stuck_marker`, `clear_revision_marker`, `ignore_for_queue_marker`, `clear_ignore_for_queue_marker` — SHALL resolve the operator-supplied `change` field as either an exact change-directory name OR a case-sensitive leading prefix, scoped to the directories carrying the action's relevant marker file. Resolution happens before any marker-removal or marker-writing filesystem call.

The per-action marker scope is:

| Action | Scope (directories carrying any of) |
| --- | --- |
| `clear_revision_marker` | `.needs-spec-revision.json` |
| `clear_perma_stuck_marker` | `.perma-stuck.json` |
| `ignore_for_queue_marker` | `.perma-stuck.json` OR `.needs-spec-revision.json` |
| `clear_ignore_for_queue_marker` | `.ignore-for-queue.json` |

Resolution algorithm:

1. **Exact-name path.** When the supplied `change` value names an existing directory under `<workspace>/openspec/changes/`, the resolution is bound to THAT directory and SHALL NOT fall through to prefix enumeration. If the named directory carries a scope-required marker, the resolved value is the supplied value verbatim (fast-path success). If it does NOT carry a scope-required marker, the resolver SHALL return `NoMatch` immediately. Falling through to prefix enumeration in this case is forbidden — doing so would let a longer prefix-extended sibling directory (e.g., `a37-foo-bar` when the operator typed exact `a37-foo`) silently hijack the operator-named slug.
2. **Prefix-enumeration path.** Reached ONLY when the supplied value does NOT name an existing change directory. The handler enumerates the change-root directory (skipping the archive subdirectory AND dotfile entries, matching the canonical `list_pending` skip rules), filters to directories carrying any scope-required marker, AND collects entries whose name `str::starts_with` the supplied value (case-sensitive). A unique candidate is the resolved value. Zero candidates produce a `NoMatch` error. Two or more candidates produce a `MultiMatch` error with the candidate list sorted ascending.

Error messages SHALL name the marker scope explicitly so the operator can act without consulting documentation: `no change matching prefix '<prefix>' has a .needs-spec-revision.json marker` for `clear_revision_marker`'s no-match path, AND analogous messages per action. The multi-match message SHALL list the candidates AND end with `Retype with a longer prefix or the full slug.`

The handler's success response JSON SHALL carry the resolved canonical slug in the `change` field, NOT the operator-supplied prefix, so downstream consumers (chatops formatter, journalctl, audit log) see the authoritative name.

When the supplied value exactly equals the canonical slug (the common case for operators who paste the full slug from an alert), the resolver SHALL return the value WITHOUT logging the resolution. A non-trivial resolution (prefix → canonical) SHALL log `INFO control_socket: resolved partial change '<prefix>' → '<canonical>' for action <action>` so operators reading journalctl can confirm the disambiguation.

#### Scenario: Exact slug match unchanged
- **GIVEN** `<workspace>/openspec/changes/a37-unify-llm-provider-config/.needs-spec-revision.json` exists
- **WHEN** the operator submits `clear_revision_marker` with `change: "a37-unify-llm-provider-config"`
- **THEN** the resolver returns `Ok("a37-unify-llm-provider-config")` via the exact-match fast path
- **AND** the marker file is removed
- **AND** the response is `{"ok": true, "change": "a37-unify-llm-provider-config", "url": "<repo-url>"}`
- **AND** NO `resolved partial change` INFO log is emitted (the value was already canonical)

#### Scenario: Unique prefix match resolves to canonical slug
- **GIVEN** the workspace contains exactly one change directory matching the prefix `a37` AND carrying `.needs-spec-revision.json` (`a37-unify-llm-provider-config`)
- **WHEN** the operator submits `clear_revision_marker` with `change: "a37"`
- **THEN** the resolver returns `Ok("a37-unify-llm-provider-config")`
- **AND** the marker file under the canonical directory is removed
- **AND** the response is `{"ok": true, "change": "a37-unify-llm-provider-config", "url": "<repo-url>"}` (the resolved canonical slug, NOT the supplied prefix)
- **AND** the daemon log contains `INFO control_socket: resolved partial change 'a37' → 'a37-unify-llm-provider-config' for action clear_revision_marker`

#### Scenario: Zero candidates with no matching marker produce a scope-naming error
- **GIVEN** no change directory has both the prefix match for `a99` AND a `.needs-spec-revision.json` marker
- **WHEN** the operator submits `clear_revision_marker` with `change: "a99"`
- **THEN** the resolver returns `Err(NoMatch { scope: NeedsRevision })`
- **AND** the response is `{"ok": false, "error": "no change matching prefix 'a99' has a .needs-spec-revision.json marker"}`
- **AND** no marker file is read or modified

#### Scenario: Multiple candidates produce a candidate-listing error
- **GIVEN** the workspace contains both `a37-foo/.needs-spec-revision.json` AND `a38-bar/.needs-spec-revision.json`
- **WHEN** the operator submits `clear_revision_marker` with `change: "a3"`
- **THEN** the resolver returns `Err(MultiMatch { candidates: ["a37-foo", "a38-bar"] })`
- **AND** the response is `{"ok": false, "error": "multiple changes match prefix 'a3': a37-foo, a38-bar. Retype with a longer prefix or the full slug."}`
- **AND** no marker file is read or modified

#### Scenario: Exact-named directory without scope marker is NoMatch (never a prefix-extension)
- **GIVEN** the workspace contains `a37-foo/` (no `.needs-spec-revision.json` marker) AND `a37-foo-bar/.needs-spec-revision.json`
- **WHEN** the operator submits `clear_revision_marker` with `change: "a37-foo"` (an exact directory name)
- **THEN** the resolver returns `Err(NoMatch { scope: NeedsRevision })` against the operator-named directory
- **AND** the resolver does NOT fall through to prefix enumeration
- **AND** the resolver does NOT return `Ok("a37-foo-bar")` — the prefix-extended sibling MUST NOT silently substitute for the operator-named slug
- **AND** the response is `{"ok": false, "error": "no change matching prefix 'a37-foo' has a .needs-spec-revision.json marker"}`
- **AND** no marker file is read or modified

#### Scenario: Per-action scope isolates markers correctly
- **GIVEN** the workspace contains `a37-foo/.perma-stuck.json` AND `a37-foo` carries no `.needs-spec-revision.json`
- **WHEN** the operator submits `clear_revision_marker` with `change: "a37"`
- **THEN** the resolver returns `Err(NoMatch { scope: NeedsRevision })` (the wrong marker for this action's scope)
- **AND** the response error names the `.needs-spec-revision.json` scope
- **AND** the same workspace responds to `clear_perma_stuck_marker` with `change: "a37"` by resolving to `a37-foo` (the perma-stuck scope DOES include the directory)

#### Scenario: `ignore_for_queue_marker` accepts either blocking marker
- **GIVEN** the workspace contains `a37-foo/.needs-spec-revision.json` AND `a38-bar/.perma-stuck.json` AND `a39-baz` carrying neither marker
- **WHEN** the operator submits `ignore_for_queue_marker` with `change: "a37"`
- **THEN** the resolver returns `Ok("a37-foo")` (the `EitherBlocking` scope accepts `.needs-spec-revision.json`)
- **AND** submitting `change: "a38"` to the same action resolves to `a38-bar` (the `EitherBlocking` scope also accepts `.perma-stuck.json`)
- **AND** submitting `change: "a39"` returns `Err(NoMatch { scope: EitherBlocking })` with the message naming both marker files

#### Scenario: End-to-end happy path — backtick-wrapped prefix from a marker alert
- **GIVEN** the chatops alert template has fired `⚠️ \`a37-unify-llm-provider-config\` has unarchivable spec deltas (pre-flight)...`
- **AND** the workspace contains exactly one change (`a37-unify-llm-provider-config`) carrying `.needs-spec-revision.json`
- **WHEN** the operator copies the alert's wrapped slug verbatim AND submits `@<bot> clear-revision myrepo \`a37\`` (a shortened prefix wrapped in backticks)
- **THEN** the parser strips the surrounding backticks AND extracts `change: "a37"` after regex validation
- **AND** the dispatcher submits a `clear_revision_marker` control-socket action carrying `change: "a37"`
- **AND** the control-socket handler resolves the prefix to `a37-unify-llm-provider-config` via `ChangePrefixMarkerScope::NeedsRevision`
- **AND** the `.needs-spec-revision.json` marker file under `a37-unify-llm-provider-config/` is removed
- **AND** the control-socket response is `{"ok": true, "change": "a37-unify-llm-provider-config", "url": "<repo-url>"}` (the canonical slug, NOT the supplied prefix)
- **AND** the chatops dispatcher's reply text names `a37-unify-llm-provider-config`
- **AND** the daemon log records `INFO control_socket: resolved partial change 'a37' → 'a37-unify-llm-provider-config' for action clear_revision_marker`

#### Scenario: Archive directory AND dotfile entries are skipped during enumeration
- **GIVEN** the workspace contains `archive/a01-something/.needs-spec-revision.json` (under the archive subdirectory) AND `.scratch/.needs-spec-revision.json` (a dotfile dir) AND `a37-foo/.needs-spec-revision.json`
- **WHEN** the operator submits `clear_revision_marker` with `change: "a"`
- **THEN** the resolver enumerates only `a37-foo` as a candidate
- **AND** archive entries AND dotfile entries are not considered for prefix matching even when their leading characters match the prefix

### Requirement: Audit-module tracing carries the repository URL as a structured field
Every `tracing::warn!`, `tracing::info!`, AND `tracing::error!` call site under `autocoder/src/audits/` that fires DURING OR AFTER a per-repository audit context is established SHALL include a structured field named `url` whose value is the repository URL the audit is running against (typically `url = %ctx.repo.url` when the function has access to an `&AuditContext`, OR `url = %repo_url` when the function takes the URL as a parameter). The field name SHALL be exactly `url` — matching the convention used by `polling_loop.rs` informational log lines — so operators filtering by repository see a uniform attribution key across audit AND polling code paths.

Truly repository-agnostic tracing calls (e.g., audit-registry initialization at daemon startup, scheduler top-line `no audits configured for any repo` messages) MAY omit `url` ONLY when annotated with a `// no-url: <reason>` comment on the line immediately preceding the macro invocation. The annotation makes the attribution choice explicit AND keeps the regression test self-enforcing for future contributors.

A regression test SHALL scan every `.rs` file under `autocoder/src/audits/` via `std::fs::read_to_string` AND verify every `tracing::(warn|info|error)!` site either contains `url =` in its structured-field set OR is preceded by a `// no-url:` annotation. The test SHALL produce a combined failure listing (NOT first-failure-only) so an operator fixing many sites at once sees every offender in one run.

This requirement applies ONLY to the audit modules (`autocoder/src/audits/*.rs`) AND ONLY to the three log levels named. Other modules (`polling_loop.rs`, `chatops/`, `executor/`) follow their own tracing conventions AND are out of scope for this requirement.

#### Scenario: Validation-failure WARN carries the repo URL
- **GIVEN** a daemon configured with two repositories AND an active `missing_tests_audit` run on the first repository (`https://example.invalid/repo-alpha`)
- **WHEN** the audit produces an invalid proposal AND the validation-rejection WARN fires from `audits/specs_writing.rs`
- **THEN** the log line's structured-field set contains `url=https://example.invalid/repo-alpha`
- **AND** the operator filtering with `journalctl -u autocoder | grep repo-alpha` sees the WARN line
- **AND** the operator filtering with `journalctl -u autocoder | grep repo-beta` does NOT see the WARN line (the second repo's audit run, if any, has its own `url` field)

#### Scenario: Chatops-post-failed WARN carries the repo URL
- **GIVEN** an audit's `ValidationExhausted` chatops notification post errors out
- **WHEN** the WARN at `audits/specs_writing.rs::run_specs_writing_audit` (the chatops-post-failed branch) fires
- **THEN** the log line's structured-field set contains `url=<repo-url>` where the URL is the same one the failed chatops post would have named in its message body

#### Scenario: Shared helper threads the URL through
- **GIVEN** a helper in `audits/mod.rs` that takes `repo_url: &str` as a parameter (e.g., `post_validation_exhausted_notification`)
- **WHEN** that helper's internal tracing call fires
- **THEN** the log line's structured-field set contains `url=<repo_url>` (the helper's parameter, threaded into the tracing call's field set)

#### Scenario: Scheduler-startup tracing without per-repo context is annotated
- **GIVEN** the audit scheduler's startup phase logs `audit registry initialized with N audit types` before any per-repository context exists
- **WHEN** that INFO line fires
- **THEN** the line on the preceding row in source contains `// no-url: registry init runs once at startup, no repo context yet` (OR equivalent reason text)
- **AND** the regression test treats this site as acceptable (the annotation is the escape hatch)

#### Scenario: Regression test catches a new tracing call added without attribution
- **GIVEN** a hypothetical future change adds a `tracing::warn!("something went wrong")` to `autocoder/src/audits/drift.rs` without `url =` AND without a `// no-url:` annotation
- **WHEN** the regression test runs in CI
- **THEN** the test fails with a diagnostic naming `autocoder/src/audits/drift.rs:<lineno>: tracing call missing 'url' field AND no '// no-url:' annotation`
- **AND** the change cannot merge until the contributor either adds the `url` field OR explicitly annotates the call as repo-agnostic
- **AND** the test reports EVERY offending site in one run, not just the first

