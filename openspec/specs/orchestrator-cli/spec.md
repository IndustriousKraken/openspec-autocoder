# orchestrator-cli Specification

## Purpose
TBD - created by archiving change orchestrator-architecture. Update Purpose after archive.
## Requirements
### Requirement: Daemon entry point
The orchestrator SHALL provide a `run` subcommand that loads a YAML configuration file and starts an asynchronous polling loop for each configured repository, terminating only on signal (SIGINT/SIGTERM) or fatal initialization error. In each polling iteration, the orchestrator SHALL process waiting (escalated) changes BEFORE pending (fresh) changes. If after the waiting-processing step ANY change in the same repository is still waiting, the orchestrator SHALL skip the pending-change loop for that iteration. This preserves the architecture's serial-queue invariant — pending changes are not processed while an earlier-or-equal change is unresolved. **The binary that exposes this subcommand is named `autocoder`; the full invocation is `autocoder run --config <path>`.**

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
- **AND** the orchestrator proceeds to the next pending change

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
autocoder SHALL implement the per-repository polling task referenced in `orchestrator-architecture/specs/orchestrator-cli/spec.md` as a sleep-then-iterate cycle that runs the architecture's single-pass workflow on every iteration.

#### Scenario: Spawn count matches config
- **WHEN** the daemon starts with a config containing N repositories AND the workspace collision check passes
- **THEN** exactly N polling tasks are spawned via `tokio::task::JoinSet`
- **AND** each task owns its own workspace path (no two tasks share a path; collision detection at startup enforces non-overlap)

#### Scenario: Normal iteration
- **WHEN** a polling task wakes (start of process or end of previous sleep)
- **THEN** it runs the full single-pass workflow for its repository: workspace init → stale-lock cleanup → dirty-workspace refusal → branch recreation → queue walk → push and PR creation if any commits were produced
- **AND** the task then sleeps for `poll_interval_sec` before iterating again
- **AND** no two iterations within the same task overlap

#### Scenario: Iteration runtime exceeds poll interval
- **WHEN** an iteration's wall-clock runtime exceeds `poll_interval_sec`
- **THEN** the next iteration begins immediately after the current one finishes
- **AND** no negative sleep is attempted; no two iterations within the same task run in parallel

### Requirement: Iteration-level error tolerance
The polling loop SHALL continue running after a failed iteration; a single iteration's error MUST NOT terminate the task or affect other repositories.

#### Scenario: Iteration fails
- **WHEN** any error occurs during a polling iteration (workspace init, git operation, executor failure, PR creation)
- **THEN** the task emits a log line of the form `"polling iteration failed for <url>: <error chain>"` naming the failed step
- **AND** the task sleeps for `poll_interval_sec` and proceeds to the next iteration
- **AND** other repositories' polling tasks are unaffected (their iterations continue on schedule)

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
autocoder SHALL resolve the GitHub PAT used for each PR-creation call by parsing the repository URL's owner segment and consulting an optional `owner_tokens` map in the `github:` config block. When no owner-specific entry matches, autocoder SHALL fall back to the global `github.token_env`. When neither route resolves, autocoder SHALL fail at startup before any polling task is spawned.

#### Scenario: Owner-specific token used when configured
- **WHEN** `config.yaml`'s `github.owner_tokens` map contains an entry whose key matches the URL owner of a configured repository (case-insensitive)
- **THEN** the PR-creation HTTP call for that repository uses the value of the environment variable named by `owner_tokens[<matched-key>]`
- **AND** if that environment variable is unset at startup, autocoder exits non-zero with stderr naming both the owner and the missing env var

#### Scenario: Fallback to global token when no owner match
- **WHEN** `config.yaml`'s `github.owner_tokens` map either is absent OR has no entry matching a given repository's URL owner
- **THEN** the PR-creation HTTP call for that repository uses the value of the environment variable named by `github.token_env`
- **AND** if `github.token_env`'s named environment variable is unset at startup, autocoder exits non-zero with stderr naming the missing env var AND the repository whose owner has no `owner_tokens` route

#### Scenario: Startup logs name the env var per repository
- **WHEN** autocoder starts and successfully resolves a token route for every configured repository
- **THEN** for each repository, autocoder emits an info-level log line of the form `repository <url> will use GitHub token from env var <env-var-name>`
- **AND** the log line names ONLY the environment variable name, never the token value itself

#### Scenario: Case-insensitive owner matching
- **WHEN** `owner_tokens` contains a key like `My-Org` AND a repository URL has owner `my-org`
- **THEN** the entry matches and the corresponding env var is used
- **AND** the same applies in reverse (config key `my-org`, URL owner `My-Org`)

#### Scenario: Backward compatibility — config with only `token_env`
- **WHEN** `config.yaml` has a `github:` block with `token_env` set AND no `owner_tokens` key
- **THEN** every repository uses the env var named by `token_env`, with no behavior change from the prior single-token implementation

