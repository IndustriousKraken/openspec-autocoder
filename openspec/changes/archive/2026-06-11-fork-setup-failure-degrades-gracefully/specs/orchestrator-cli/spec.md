# orchestrator-cli — delta for fork-setup-failure-degrades-gracefully

## MODIFIED Requirements

### Requirement: Startup verification of fork existence
When `github.fork_owner` is set, autocoder SHALL ensure each configured repository has a reachable fork at the derived URL before spawning that repository's polling task. Forks that are missing or unreachable SHALL be created automatically via `POST /repos/{upstream-owner}/{upstream-repo}/forks` using the PAT resolved for the upstream owner; the daemon then polls the fork URL via `git ls-remote` until it becomes reachable or until a 60-second timeout elapses.

A fork-setup failure for one repository — creation returns non-2xx, OR the fork is not reachable within the timeout — SHALL NOT abort daemon startup. The daemon SHALL instead: record the failure (the upstream URL, the expected fork URL, AND the cause); **skip that repository for the process lifetime** (no polling task is spawned for it) — the same per-repo skip-for-lifetime behavior already used when a fork URL cannot be derived; emit a chatops alert through the standard outbound notification path that identifies the repository AND carries a brief remedy hint; AND continue setting up AND serving every other repository. A fork-setup failure for one repository SHALL NEVER prevent the daemon from starting, from serving other repositories, OR from serving chatops. The daemon exits non-zero at startup only for non-per-repo fatal conditions (e.g. config-load failure) — NEVER for a per-repo fork-setup failure, even when every configured repository fails fork setup (it stays up so an operator can remediate AND recover the repository by fixing the fork, then restarting or reloading).

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
- **AND** autocoder skips that repository for the process lifetime (no
  polling task is spawned for it) AND emits a chatops alert that
  identifies the repository AND carries a remedy hint
- **AND** autocoder continues setting up the remaining repositories AND
  the daemon does NOT exit

#### Scenario: Fork-creation succeeds but the fork is not yet reachable
- **WHEN** the POST returns 2xx AND `git ls-remote <fork-url> HEAD`
  fails for 60 seconds of polling at 2-second intervals
- **THEN** that repository's failure is recorded as
  "fork creation succeeded but the fork at `<fork-url>` was not
  reachable within 60s"
- **AND** that repository is skipped for the process lifetime AND a
  chatops alert identifying it is emitted
- **AND** the daemon proceeds to serve the other repositories without
  exiting

#### Scenario: A fork already exists when creation is attempted
- **WHEN** autocoder issues the fork-creation POST AND the upstream
  has already been forked to the destination user
- **THEN** the GitHub API returns 2xx with the existing fork's
  metadata (idempotent behavior)
- **AND** autocoder treats this as success and proceeds with the
  reachability probe normally

#### Scenario: One repository's fork failure does not take down the others
- **WHEN** autocoder starts with multiple repositories AND one
  repository's fork cannot be set up (creation fails OR the fork is not
  reachable within the timeout) AND the other repositories' forks are
  reachable
- **THEN** the daemon spawns polling tasks for the reachable
  repositories AND enters normal polling
- **AND** chatops is served (the daemon does not exit)
- **AND** the failed repository is absent from the active polling set
  until the operator remediates AND restarts or reloads

#### Scenario: Every repository's fork fails
- **WHEN** every configured repository fails fork setup
- **THEN** the daemon still starts AND stays up serving chatops, having
  emitted one chatops alert per failed repository
- **AND** it does NOT exit non-zero
