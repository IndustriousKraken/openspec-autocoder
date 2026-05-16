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
The orchestrator SHALL provide a `rewind` subcommand that recovers from a failed PR or bad implementation by unarchiving specified changes and resetting the relevant agent branch. The subcommand SHALL accept a `--repo <selector>` argument; the argument is required when the config contains multiple repositories AND optional (defaulting to the only configured repo) when the config contains exactly one. **The binary that exposes this subcommand is named `autocoder`; the full invocation is `autocoder rewind <change> --config <path> [--repo <selector>] [--hard]`.**

#### Scenario: Multi-repo rewind requires --repo
- **WHEN** the loaded config contains 2 or more repositories AND the user invokes `autocoder rewind <change> --config <path>` without `--repo`
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
The polling loop SHALL continue running after a failed iteration; a single iteration's error MUST NOT terminate the task or affect other repositories. Predictable failure categories (workspace init, mid-iteration dirty workspace, branch push, PR creation) SHALL emit a throttled chatops alert via the existing `AlertCategory` + `handle_predictable_failure` mechanism before the iteration returns `Err`.

#### Scenario: Iteration fails
- **WHEN** any error occurs during a polling iteration (workspace init, git operation, executor failure, PR creation)
- **THEN** the task emits a log line of the form `"polling iteration failed for <url>: <error chain>"` naming the failed step
- **AND** the task sleeps for `poll_interval_sec` and proceeds to the next iteration
- **AND** other repositories' polling tasks are unaffected (their iterations continue on schedule)

#### Scenario: Mid-iteration dirty workspace alerts via chatops
- **WHEN** `run_pass_through_commits` finds `git status --porcelain`
  non-empty at the start of a pass (after filtering autocoder
  bookkeeping files like `.alert-state.json`) AND chatops is
  configured AND `failure_alerts_enabled` is true
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
autocoder SHALL respond to SIGINT or SIGTERM by cancelling all polling tasks; each task completes its current iteration (if any) and exits cleanly.

#### Scenario: Signal during inter-iteration sleep
- **WHEN** SIGINT or SIGTERM arrives while every polling task is sleeping
- **THEN** every task exits its sleep within 200 ms (verified in tests via the `CancellationToken` selecting against the sleep) and does not begin another iteration
- **AND** the main process exits within 30 seconds total

#### Scenario: Signal during iteration
- **WHEN** SIGINT or SIGTERM arrives while a polling iteration is in progress
- **THEN** the in-flight iteration runs to completion (mid-iteration cancellation is NOT performed); the task then observes the cancellation token and exits without sleeping or starting another iteration
- **AND** any child processes spawned by the iteration receive their normal lifecycle (the executor's child process completes or hits its own `executor.timeout_secs`)

### Requirement: Startup logging per repository
autocoder SHALL emit a startup log line per configured repository naming its URL, derived (or explicit) workspace path, and configured `poll_interval_sec`.

#### Scenario: Startup line emitted
- **WHEN** the daemon starts AND the workspace collision check passes
- **THEN** before any polling task begins iterating, autocoder emits one log line per repository containing the literal URL, the resolved workspace path, and the integer `poll_interval_sec`

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

