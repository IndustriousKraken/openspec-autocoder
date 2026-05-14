## MODIFIED Requirements

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
