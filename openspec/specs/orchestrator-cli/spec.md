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
autocoder SHALL acquire a per-repo busy marker file at the start of each polling iteration and hold it through every stage of the pass (executor invocation, commit, review, push, PR creation). The marker lives outside the workspace at `/tmp/autocoder/busy/<workspace-basename>.json` and is created atomically via POSIX `O_EXCL`. Its presence prevents any other autocoder pass — same daemon or different — from concurrently working on the same repo. Crashes that bypass normal release (SIGKILL, segfault, host power loss) leave the marker behind for the next pass to detect and recover from. Stuck-state recovery SHALL prefer the subprocess-sidecar PGID (set by the executor after spawning Claude) over the marker's own `pgid` field when sending kill signals.

#### Scenario: Acquire on a clean repo
- **WHEN** a polling iteration begins AND no marker file exists at
  `/tmp/autocoder/busy/<workspace-basename>.json`
- **THEN** the daemon creates the marker via `OpenOptions::new()
  .write(true).create_new(true).open(path)` (atomic against
  concurrent daemons)
- **AND** the marker contains a JSON document with fields
  `repo_url`, `pid` (this process's PID), `pgid` (this process's
  process group ID), `comm` (the value of `/proc/<pid>/comm` at
  acquire time, on Linux; empty string on other platforms),
  `started_at` (RFC 3339 UTC timestamp), and `stage` (initially
  `"executor"`)
- **AND** the iteration proceeds normally

#### Scenario: Atomic stage transitions
- **WHEN** the iteration moves from one stage to the next
  (`executor → commit → review → push → pr`)
- **THEN** the daemon updates the marker's `stage` field via a
  write-to-temp-then-rename sequence so concurrent readers see
  either the prior stage or the new one, never a partial write
- **AND** stage names are exactly: `executor`, `commit`,
  `review`, `push`, `pr`

#### Scenario: Release on normal iteration end
- **WHEN** `execute_one_pass` returns (success or any error)
- **THEN** the RAII guard holding the marker drops, and the file
  is removed
- **AND** the next iteration finds no marker and proceeds normally

#### Scenario: Marker exists, age below stuck threshold
- **WHEN** acquire detects an existing marker AND its `started_at`
  is less than `executor.timeout_secs + 600 seconds` old
- **THEN** the daemon logs INFO with the marker contents and skips
  this iteration without modifying the marker
- **AND** the polling task continues with its normal sleep + next-iteration cycle

#### Scenario: Stuck threshold exceeded, PID dead
- **WHEN** acquire detects a marker older than the stuck threshold
  AND the recorded `pid` does not correspond to a running process
  (verified via `kill(pid, 0)` returning `ESRCH`)
- **THEN** the daemon deletes the marker AND the subprocess
  sidecar file (if present), logs WARN naming the marker's prior
  contents (so operators see what crashed), and proceeds to
  acquire a fresh marker and run the iteration

#### Scenario: Stuck threshold exceeded, PID alive, comm matches
- **WHEN** acquire detects a marker older than the stuck threshold
  AND `kill(pid, 0)` returns Ok AND the value of
  `/proc/<pid>/comm` matches the recorded `comm` field (Linux;
  the comm-check is skipped on non-Linux platforms and the PID
  liveness check is trusted alone)
- **THEN** the daemon reads the subprocess sidecar file at
  `/tmp/autocoder/busy/<workspace-basename>.subprocess` (if
  present). If present, the recorded subprocess PID is used as
  the kill target (its PGID equals its PID because the executor
  spawns with `process_group(0)`); if absent, the marker's
  `pgid` field is used as the fallback
- **AND** the daemon sends `SIGTERM` to that process group via
  `killpg(target_pgid, SIGTERM)`, waits up to 5 seconds for the
  group to exit, sends `SIGKILL` via `killpg(target_pgid,
  SIGKILL)` if still alive
- **AND** the daemon deletes the marker AND the subprocess
  sidecar file, logs WARN with the action taken, attempts to
  post a chatops alert "repo recovered from stuck state"
  (best-effort), and proceeds to acquire a fresh marker and run
- **AND** the iteration proceeds even when no chatops backend is
  configured

#### Scenario: Stuck threshold exceeded, PID alive, comm differs
- **WHEN** acquire detects a marker older than the stuck threshold
  AND `kill(pid, 0)` returns Ok AND the recorded `comm` field is
  non-empty AND differs from the live `/proc/<pid>/comm` value
- **THEN** the daemon logs ERROR naming the discrepancy, attempts
  to post a chatops alert "repo stuck — please investigate"
  (best-effort), and SKIPS this iteration without modifying the
  marker or the subprocess sidecar
- **AND** the marker stays in place for human investigation; the
  next polling iteration will re-evaluate
- **AND** the iteration is skipped even when no chatops backend
  is configured (the ERROR log is the operator's only signal in
  that case)

#### Scenario: Malformed marker JSON
- **WHEN** acquire detects a marker file that cannot be parsed as
  the expected JSON shape
- **THEN** the daemon logs WARN naming the parse failure, deletes
  the marker AND the subprocess sidecar (if present), and
  proceeds to acquire a fresh one

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
autocoder SHALL track consecutive failures per change in a per-repo `.failure-state.json` file at the workspace root. After the executor returns `Failed` for a change (or the daemon transforms a Completed-with-empty-workspace outcome to Failed), the counter for that change SHALL be incremented. After the executor returns `Archived` (including via self-heal), the counter for that change SHALL be cleared. When a change's counter reaches the configured `executor.perma_stuck_after_failures` threshold (default 2), autocoder SHALL write a `.perma-stuck.json` marker into the change directory, post a chatops alert, and exclude the change from subsequent polling iterations until the marker is removed manually.

#### Scenario: Failure increments the counter
- **WHEN** `handle_outcome` produces a `Failed` result for a
  change (whether the executor returned Failed or the daemon
  transformed a Completed-with-empty-workspace via the
  no-op-completion or self-heal logic into Failed)
- **THEN** autocoder reads `.failure-state.json` from the
  workspace root, increments the entry for that change (or
  creates it with `count: 1` if absent), sets `last_reason` and
  `last_failed_at`, and writes the file back atomically
  (write-temp-then-rename)
- **AND** transient daemon-side errors that prevent the
  executor from running (workspace init failure, openspec
  preflight failure, GitHub API transport error) do NOT
  increment the counter — only outcomes where the executor
  itself ran and Failed (or was forced to Failed by
  post-execution classification) count

#### Scenario: Archive clears the counter
- **WHEN** `handle_outcome` produces an `Archived` result for a
  change (including via the self-heal path from
  `self-heal-already-implemented`)
- **THEN** autocoder removes that change's entry from
  `.failure-state.json` and writes the file back atomically
- **AND** the next failure of any change starts fresh from
  `count: 1`

#### Scenario: Threshold reached → mark perma-stuck
- **WHEN** incrementing the counter results in `count >=
  executor.perma_stuck_after_failures` (default 2)
- **THEN** autocoder writes a `.perma-stuck.json` marker file
  inside the change directory containing the change name,
  consecutive_failures count, last_reason, marked_stuck_at
  timestamp, and the operator_action message
- **AND** autocoder posts a chatops alert via the configured
  backend with subject "change perma-stuck" and a body naming
  the repo, change, count, and last reason. The alert is
  subject to the existing 24h throttle so repeat-mark events
  do not spam
- **AND** autocoder logs an ERROR line naming the change and
  the marker file path
- **AND** when no chatops backend is configured, the ERROR log
  is the operator's only signal — the marker is still written
  and the change is still excluded from `list_pending` going
  forward

#### Scenario: Operator clears the marker
- **WHEN** the operator deletes `.perma-stuck.json` from a
  change directory
- **THEN** the next polling iteration sees the change in
  `list_pending` again and runs the executor against it
- **AND** the counter starts fresh at 0 (or whatever
  `.failure-state.json` records for that change after the
  removal — implementations MAY also clear the change's entry
  in `.failure-state.json` at marker-removal time; either is
  acceptable as long as the operator's "retry" signal does
  reset behavior)

#### Scenario: Threshold is one
- **WHEN** `executor.perma_stuck_after_failures` is set to `1`
- **THEN** the very first Failed outcome for a change marks
  perma-stuck (no retry at all)

#### Scenario: Default threshold
- **WHEN** `executor.perma_stuck_after_failures` is unset
- **THEN** autocoder uses `2` as the threshold value

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
autocoder SHALL include a periodic audit framework that runs registered audit tasks on per-audit cadences, persists last-run state per workspace, applies per-audit sandbox profiles, enforces post-hoc write restrictions, writes per-invocation logs, and integrates with the polling loop so any specs an audit creates are picked up by the same iteration's queue walk.

#### Scenario: Framework runs registered audits at startup-defined cadence
- **WHEN** a polling iteration completes its `recreate_branch` step
  AND BEFORE it calls `queue::list_pending`
- **THEN** the framework iterates registered audits in declaration
  order
- **AND** for each audit, checks `.audit-state.json` to determine
  whether the configured cadence has elapsed since the last run
- **AND** runs the audit only when due

#### Scenario: requires_head_change suppresses re-runs when HEAD unchanged
- **WHEN** an audit's `requires_head_change()` returns `true` AND
  the recorded `last_run_sha` for that audit equals the current
  `HEAD` SHA on the base branch
- **THEN** the framework skips the audit for this iteration even
  if the cadence interval has elapsed
- **AND** the next iteration after a HEAD change re-evaluates
  cadence and runs the audit if due

#### Scenario: requires_head_change false runs on cadence regardless of HEAD
- **WHEN** an audit's `requires_head_change()` returns `false` AND
  the cadence has elapsed since `last_run_at`
- **THEN** the framework runs the audit regardless of whether
  `HEAD` has changed
- **AND** this allows audits whose inputs are external (e.g.
  package registries, GitHub PR lists) to run periodically without
  depending on local code changes

#### Scenario: WritePolicy::None audit cannot modify the workspace
- **WHEN** an audit declares `WritePolicy::None` AND it runs
- **THEN** the audit's sandbox (when the audit uses the wrapped
  Claude CLI) allows only `Read`, `Glob`, `Grep`, `Bash` —
  `Write` and `Edit` are denied at the tool layer
- **AND** after the audit returns, the framework runs
  `git status --porcelain` and asserts the workspace is clean
- **AND** if either the sandbox blocks a write attempt OR the
  post-hoc diff is non-empty, the audit is treated as failed:
  state is NOT updated (so cadence triggers a re-run next iteration),
  a chatops alert is posted under a new audit-failure category,
  and the unexpected diff is reverted via `git reset --hard HEAD`

#### Scenario: WritePolicy::OpenSpecOnly audit may only write under openspec/changes/
- **WHEN** an audit declares `WritePolicy::OpenSpecOnly` AND
  it runs
- **THEN** the audit's sandbox allows `Write` and `Edit`
- **AND** after the audit returns, the framework inspects
  `git status --porcelain` and asserts every modified or new path
  begins with `openspec/changes/`
- **AND** if any path outside that prefix is touched, the audit
  is treated as failed: state is NOT updated, chatops alert is
  posted, the entire workspace diff is reverted via
  `git reset --hard HEAD` + `git clean -fd`

#### Scenario: Audit-run log written per invocation
- **WHEN** an audit runs (regardless of outcome)
- **THEN** autocoder writes a timestamped log at
  `/tmp/autocoder/logs/<workspace-basename>/audits/<audit_type>-<UTC-RFC3339-with-Z>.log`
  containing: the audit type, the workspace path, the start and
  end timestamps, the resolved cadence + last-run info, the prompt
  used (for LLM audits), the raw audit output, and the final
  `AuditOutcome` variant
- **AND** the log directory is created if absent

#### Scenario: AuditOutcome::Reported posts to chatops
- **WHEN** an audit returns `AuditOutcome::Reported(findings)` AND
  chatops is configured
- **THEN** autocoder posts a single chatops message with a header
  line `📋 <repo>: <audit_type> — <N> finding(s)` followed by a
  bullet list of finding subjects (each truncated to the
  per-finding excerpt limit, default 200 chars)
- **AND** the full body of each finding is preserved in the
  audit-run log

#### Scenario: AuditOutcome::Reported with no findings posts a brief OK
- **WHEN** an audit returns `AuditOutcome::Reported(vec![])` AND
  chatops is configured AND the operator has set
  `audits.<audit_type>.notify_on_clean: true` (default `false`)
- **THEN** autocoder posts `✅ <repo>: <audit_type> — no findings`
- **AND** when `notify_on_clean` is unset or `false`, no chatops
  post is made for an empty-findings outcome (silence is success)

#### Scenario: AuditOutcome::SpecsWritten records the change names
- **WHEN** an audit returns `AuditOutcome::SpecsWritten(names)`
  with non-empty `names`
- **THEN** the framework logs an info line naming each created
  change AND the iteration proceeds to `list_pending` which now
  observes those entries as pending
- **AND** no chatops post is made by the framework itself for
  spec-writing audits — the existing start-of-work +
  PR-opened notifications cover the subsequent flow

#### Scenario: State persists across daemon restarts
- **WHEN** the daemon stops AND restarts later
- **THEN** the framework reads `<workspace>/.audit-state.json` at
  startup AND resumes the existing cadence
- **AND** an audit due during the daemon's downtime runs on the
  first qualifying iteration after restart
- **AND** if `.audit-state.json` is missing or unparseable, the
  framework treats it as "no audits have ever run" — every audit
  is eligible on its next due iteration

#### Scenario: Audit failure does not abort the iteration
- **WHEN** an audit's `run()` returns `Err`
- **THEN** the framework logs the error at ERROR level naming the
  audit type and excerpt
- **AND** `.audit-state.json` is NOT updated for that audit (so
  the cadence will re-trigger it next iteration)
- **AND** the iteration continues to `list_pending` and the rest
  of the normal flow; other audits in the registry still run

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
autocoder SHALL ship an `architecture-brightline` audit in the periodic audit framework. The audit is pure-code (no LLM invocation), `requires_head_change = true`, and `WritePolicy::None`. It SHALL produce `AuditOutcome::Reported(findings)` containing structural metrics that exceed configured (or default) thresholds.

#### Scenario: Reports files exceeding the size threshold
- **WHEN** the audit runs AND a tracked file under the
  repository's source root has more lines than the threshold
  (default `800`)
- **THEN** a finding of severity `medium` is included with
  `subject = "file <path> is <N> lines (threshold: <T>)"` AND
  `anchor = Some("<path>:1")`

#### Scenario: Reports identical function signatures across files
- **WHEN** the audit detects two or more functions with
  identical name + parameter list signatures in different files
  (excluding `mod tests {}` blocks)
- **THEN** a finding of severity `low` lists each occurrence

#### Scenario: Reports dead public items
- **WHEN** the audit (or a static-analysis subprocess it invokes)
  identifies public items with zero references in the
  repository
- **THEN** a finding of severity `low` lists the items

#### Scenario: No findings produces silent outcome
- **WHEN** no metric exceeds its threshold
- **THEN** the audit returns `AuditOutcome::Reported(vec![])`
- **AND** unless `notify_on_clean: true` is set, no chatops
  message is posted (per the framework-level scenario above)

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

#### Scenario: Prompt instructs confidence-filtered output
- **WHEN** the prompt is loaded
- **THEN** the prompt explicitly states:
  - "Only report findings you are reasonably confident about. A
    false positive becomes wasted implementer work downstream;
    err strongly on the side of NOT reporting if you're uncertain."
  - "Do NOT propose stylistic 'best-practice' changes that don't
    address a concrete security issue or bug."
- **AND** the prompt provides categorical guidance: which
  categories are in-scope (injection, auth/authz mistakes,
  hard-coded secrets, unsafe deserialization, missing input
  validation at trust boundaries, race conditions, resource
  leaks, off-by-one, wrong operator, mishandled None/null,
  missing error propagation) and which are out-of-scope (code
  style, naming, architectural opinions, performance unless
  measurable, anything the project has explicitly accepted)

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
After the triage executor returns `Completed`, the daemon SHALL inspect the working tree's changed paths and split them by whether each path is inside `openspec/changes/<derived-slug>/`. Paths inside that subtree go to the spec PR; all other paths go to the fixes PR. Each PR is created on its own branch off the same base, with the existing PR-creation helpers. PR bodies cross-link each other when both are created.

#### Scenario: Mixed diff produces two PRs that cross-link
- **WHEN** the triage executor's Completed diff contains code changes outside `openspec/changes/<new_slug>/` AND new files inside `openspec/changes/<new_slug>/`
- **THEN** the daemon creates a fixes branch + PR with the code paths
- **AND** the daemon creates a spec branch + PR with the openspec paths
- **AND** each PR body contains a link to the other ("This PR carries the code fixes; see #<other_pr> for the new spec change." and vice versa)
- **AND** the audit-thread state's `status` flips to `Acted`

#### Scenario: Code-only triage produces only a fixes PR
- **WHEN** the triage diff has only code paths (no new `openspec/changes/<new_slug>/`)
- **THEN** only the fixes PR is created
- **AND** no spec PR is created
- **AND** the audit-thread state's `status` flips to `Acted`

#### Scenario: Spec-only triage produces only a spec PR
- **WHEN** the triage diff has only new `openspec/changes/<new_slug>/` paths
- **THEN** only the spec PR is created
- **AND** no fixes PR is created
- **AND** the audit-thread state's `status` flips to `Acted`

#### Scenario: Empty-diff triage posts a no-action reply
- **WHEN** the triage executor returns Completed but the diff is empty (the LLM decided nothing was actionable)
- **THEN** no PRs are created
- **AND** the bot posts a reply in the audit thread containing the LLM's final-summary text explaining the decision
- **AND** the audit-thread state's `status` flips to `Acted`

#### Scenario: Slug collision is suffixed
- **WHEN** the derived slug `<audit-type>-<hash>` already exists as `openspec/changes/<slug>/`
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
When the executor returns Completed without a `.chat-reply.md` marker, the polling iteration SHALL run the diff-split + two-PR creation logic from `a01-audit-reply-acts` (using the shared `split_diff_by_spec_dir` helper). The resulting fixes PR and spec PR are structurally identical to PRs spawned by `send it` and by polling-loop processing. Operators commenting `@<bot> revise <text>` on either get revisions through `a01-pr-comment-revision-loop`.

#### Scenario: Mixed diff produces two PRs that cross-link
- **WHEN** the directive's executor returns Completed with both code changes AND a new `openspec/changes/<chat-derived-slug>/`
- **THEN** the daemon creates a fixes branch + PR with the code paths
- **AND** the daemon creates a spec branch + PR with the openspec paths
- **AND** each PR body contains a link to the other
- **AND** the state's `status` is `Acted`

#### Scenario: Code-only directive produces only a fixes PR
- **WHEN** the directive's diff has only code paths
- **THEN** only the fixes PR is created
- **AND** the state's `status` is `Acted`

#### Scenario: Spec-only directive produces only a spec PR
- **WHEN** the directive's diff has only new `openspec/changes/<chat-derived-slug>/` paths
- **THEN** only the spec PR is created
- **AND** the state's `status` is `Acted`

#### Scenario: Empty-diff directive posts a no-action reply
- **WHEN** the directive's executor returns Completed with an empty diff AND no `.chat-reply.md`
- **THEN** no PRs are created
- **AND** the bot posts a reply in the request's thread explaining no action was taken
- **AND** the state's `status` is `Acted`

#### Scenario: Revision comments on a triage PR are processed normally
- **WHEN** a chat-request-spawned PR has an operator comment `@<bot> revise <text>`
- **THEN** the existing revision-loop dispatcher picks up the comment AND processes the revision against the PR's branch
- **AND** the proposal-request state file is not consulted (the revision is its own scope)

### Requirement: Proposal-request state files are pruned after 7 days
The daemon SHALL prune `ProposalRequestState` files whose `submitted_at` is older than 7 days. The prune runs periodically (at iteration start or once per day per the existing housekeeping pattern). Stale entries are removed regardless of `status`.

#### Scenario: Stale entry is removed
- **WHEN** the prune runs AND a `ProposalRequestState` has `submitted_at` more than 7 days in the past
- **THEN** the state file is removed

#### Scenario: Fresh entry is preserved
- **WHEN** the prune runs AND a `ProposalRequestState` has `submitted_at` within the last 7 days
- **THEN** the state file is NOT removed regardless of status

