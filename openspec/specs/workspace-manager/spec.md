# workspace-manager Specification

## Purpose
TBD - created by archiving change orchestrator-foundation. Update Purpose after archive.
## Requirements
### Requirement: Deterministic workspace path derivation
The workspace manager SHALL derive a per-repository workspace path deterministically from the configured URL, so that restarting the daemon reuses existing local clones rather than creating new ones.

#### Scenario: Path derivation is stable
- **WHEN** the manager derives a path for a given URL
- **THEN** invoking the same derivation a second time with the same URL returns a path equal by `==` to the first
- **AND** the path is rooted at `/tmp/workspaces/`

#### Scenario: Distinct URLs produce distinct paths
- **WHEN** the manager derives paths for two URLs that differ in host, owner, or repo name
- **THEN** the resulting paths are not equal
- **AND** repeated derivations preserve the inequality

### Requirement: Cross-repository path collision detection at startup
The orchestrator SHALL detect any two configured repositories that resolve to the same workspace path and refuse to start, naming both URLs and the shared path in the error message.

#### Scenario: Two repos derive to the same path
- **WHEN** the orchestrator loads a config containing two repositories whose URLs sanitize to the same workspace path (or whose explicit `local_path` overrides collide)
- **THEN** the orchestrator emits a startup error whose text contains BOTH conflicting URLs verbatim AND the shared path
- **AND** no polling tasks are spawned for either repository
- **AND** the process exits non-zero within 5 seconds of config load

### Requirement: Idempotent workspace initialization
The workspace manager SHALL ensure a repository is locally cloned before each polling iteration begins, performing a clone if absent and a fetch if present, without losing existing local state. When fork-PR mode is active (`github.fork_owner` is configured), the manager SHALL ALSO ensure a second remote named `fork` is registered, pointing at the fork URL derived from the upstream URL and `fork_owner`. When the manager performs a clone (the workspace path was absent) AND fork-PR mode is active, the manager SHALL ALSO fetch from the `fork` remote so the local tracking ref `refs/remotes/fork/<branch>` reflects the fork's actual state — necessary so subsequent `git push --force-with-lease` operations compare against accurate data and do not falsely report "stale info."

#### Scenario: First-time clone (direct-push mode)
- **WHEN** the polling task begins an iteration AND the workspace path
  does not exist on disk AND `github.fork_owner` is unset
- **THEN** the manager runs `git clone <url> <workspace_path>`
- **AND** the resulting path contains a `.git` directory verifiable via
  filesystem inspection
- **AND** no additional remotes are registered (only `origin` exists)

#### Scenario: First-time clone (fork-PR mode)
- **WHEN** the polling task begins an iteration AND the workspace path
  does not exist on disk AND `github.fork_owner` is set
- **THEN** the manager runs `git clone <upstream-url> <workspace_path>`
- **AND** the manager then runs `git remote add fork <fork-url>` inside
  the workspace, where `<fork-url>` is derived from `<upstream-url>` by
  substituting `fork_owner` for the upstream owner segment
- **AND** the manager then runs `git fetch fork` inside the workspace,
  populating `refs/remotes/fork/*` so that local tracking reflects the
  fork's actual state
- **AND** the resulting workspace has exactly two remotes: `origin`
  pointing at the upstream URL AND `fork` pointing at the fork URL

#### Scenario: Fork fetch failure on first-time clone is non-fatal
- **WHEN** the post-clone `git fetch fork` step fails (network error,
  fork is empty, fork doesn't yet exist, authentication failure for
  the fork remote, etc.)
- **THEN** the manager logs the failure at WARN naming the fork URL
  and the error
- **AND** `ensure_initialized` still returns Ok — the clone +
  remote-registration succeeded, and the empty local tracking ref
  is no worse than the pre-fix behavior
- **AND** the next polling iteration proceeds normally; a real
  divergence will surface as a `--force-with-lease` failure with
  the existing branch-push-failure alert path

#### Scenario: Re-initializing an existing workspace (direct-push mode)
- **WHEN** the polling task begins an iteration AND the workspace path
  exists on disk AND `github.fork_owner` is unset
- **THEN** the manager runs `git fetch origin` inside the workspace and
  does NOT run a fresh clone
- **AND** any pre-existing local branches in the workspace are preserved

#### Scenario: Re-initializing an existing workspace (fork-PR mode)
- **WHEN** the polling task begins an iteration AND the workspace path
  exists on disk AND `github.fork_owner` is set
- **THEN** the manager runs `git fetch origin` AND ensures the `fork`
  remote exists with the correct URL (`git remote add fork <url>` if
  absent, OR `git remote set-url fork <url>` if present with a stale
  URL)
- **AND** the `fork` remote setup is idempotent across iterations
- **AND** the manager does NOT re-fetch the fork remote on every
  iteration — fork tracking refs persist across iterations and are
  updated by `git push` itself when autocoder pushes successfully

#### Scenario: Workspace exists but is not a git repository
- **WHEN** the workspace path exists but does not contain a `.git`
  directory
- **THEN** `ensure_initialized` returns an error naming the path and
  the missing `.git` marker
- **AND** the manager does NOT delete or modify the existing path

### Requirement: Fork URL derivation
The workspace manager SHALL derive the fork URL deterministically from
the upstream URL and `github.fork_owner` by substituting the owner
segment while preserving the URL scheme and the repository name.

#### Scenario: SSH upstream URL
- **WHEN** the upstream URL is `git@github.com:UpstreamOrg/repo.git` AND
  `github.fork_owner` is `machine-user`
- **THEN** the derived fork URL is `git@github.com:machine-user/repo.git`

#### Scenario: HTTPS upstream URL
- **WHEN** the upstream URL is `https://github.com/UpstreamOrg/repo.git`
  AND `github.fork_owner` is `machine-user`
- **THEN** the derived fork URL is
  `https://github.com/machine-user/repo.git`

#### Scenario: Unrecognized URL scheme
- **WHEN** the upstream URL uses a scheme other than
  `git@github.com:` or `https://github.com/` (e.g. an enterprise
  GitHub host)
- **THEN** fork URL derivation returns an error naming the upstream
  URL and the unsupported scheme

### Requirement: Optional fork recreation on workspace reinitialization
When `github.recreate_fork_on_reinit` is `true` AND fork-PR mode is active AND the workspace manager performs a clone (the workspace path was absent), the manager SHALL delete the existing fork on GitHub, recreate it from upstream, then proceed with the normal post-clone steps. This is destructive: any open PRs against branches on the deleted fork are closed by GitHub automatically. Default is `false`; operators opt in per their tolerance for losing fork-resident state.

#### Scenario: Recreate-on-reinit fires only when both conditions hold
- **WHEN** the workspace manager begins `ensure_initialized` AND the
  workspace path is absent (so a clone will happen) AND `fork_url` is
  `Some` (fork-PR mode is active) AND `recreate_fork_on_reinit` is
  `true`
- **THEN** before adding the `fork` remote, the manager resolves the
  upstream owner, repo name, and operator PAT
- **AND** calls `github::delete_repo(<fork_owner>, <repo_name>, token)`
  to delete the existing fork via `DELETE /repos/{owner}/{repo}`
- **AND** waits up to 5 seconds for the deletion to propagate
- **AND** calls `github::create_fork(<upstream_owner>, <repo_name>,
  token)` to re-fork upstream
- **AND** then proceeds with the existing `ensure_remote` +
  `fetch fork` sequence (the fetch returns empty tracking refs because
  the fork is freshly created)

#### Scenario: Recreate is skipped when workspace already exists
- **WHEN** `recreate_fork_on_reinit` is `true` AND
  `ensure_initialized` is called against an existing workspace (so
  no clone happens)
- **THEN** the re-fork path is NOT triggered
- **AND** the existing re-init-with-existing-workspace behavior runs
  unchanged

#### Scenario: Recreate is skipped when fork-PR mode is off
- **WHEN** `recreate_fork_on_reinit` is `true` BUT `fork_url` is
  `None` (direct-push mode)
- **THEN** the re-fork path is NOT triggered
- **AND** the manager runs the existing direct-push clone path unchanged

#### Scenario: Recreate is skipped when flag is false or unset
- **WHEN** `recreate_fork_on_reinit` is `false` OR unset (the default)
- **THEN** the re-fork path is NEVER triggered regardless of
  workspace state or fork-mode setting
- **AND** the conservative `fetch fork` behavior from
  `fetch-fork-at-workspace-init` applies

#### Scenario: Delete returns 404 is treated as success
- **WHEN** the `github::delete_repo` call returns 404 (the fork was
  already deleted out-of-band, e.g. via the GitHub UI before this
  iteration)
- **THEN** the manager logs INFO "fork already absent; proceeding to
  recreate" AND continues to the `create_fork` step
- **AND** does NOT treat this as an error

#### Scenario: Delete returns 403 (missing scope) falls back to conservative path
- **WHEN** the `github::delete_repo` call returns 403 (operator's
  PAT lacks the `delete_repo` scope)
- **THEN** the manager logs ERROR naming the missing scope AND posts
  a chatops alert (best-effort) telling the operator to add the
  `delete_repo` scope OR set `recreate_fork_on_reinit: false`
- **AND** falls back to the conservative `fetch fork` behavior so
  iteration can still proceed
- **AND** subsequent iterations continue to attempt re-fork on each
  fresh clone (the operator can disable the flag to silence)

#### Scenario: Recreate posts a destructive-action chatops notification
- **WHEN** the re-fork sequence (delete + create) completes
  successfully
- **THEN** autocoder posts a single ChatOps notification with body
  `:warning: <repo>: re-forked at workspace reinitialization
  (previous fork deleted; any open PRs from this fork are now closed)`
- **AND** the post is best-effort: a failure logs at WARN and
  does NOT propagate (the re-fork itself already succeeded)

### Requirement: Workspace init auto-recovers from partial-clone artifacts
When `workspace::ensure_initialized` detects a workspace directory that exists but contains no `.git/` subdirectory, the daemon SHALL attempt automatic recovery: verify the directory contains no operator-meaningful state, delete the partial directory, and re-attempt the clone. The recovery is a single self-healing pass — if the re-clone also fails, the iteration reports the real clone failure (not the secondary "exists but no .git" detection). A WARN log per recovery attempt names the workspace path and the action so operators see the auto-cleanup happened.

#### Scenario: Partial-clone artifact triggers auto-cleanup and re-clone
- **WHEN** the workspace directory exists AND it contains no `.git/` AND the safety check passes (no operator-meaningful state)
- **THEN** the daemon logs a WARN naming the workspace path, the repo URL, and the action ("partial clone artifact detected. Deleting and re-cloning.")
- **AND** the daemon calls `std::fs::remove_dir_all` on the workspace path
- **AND** the daemon re-attempts the normal clone path
- **AND** if the re-clone succeeds, `ensure_initialized` returns Ok
- **AND** the polling iteration's reported outcome is Completed (not Failed)

#### Scenario: Re-clone after auto-cleanup fails surfaces the real error
- **WHEN** auto-cleanup runs AND the re-clone attempt fails
- **THEN** the returned Err contains the actual clone-failure message from git (e.g. `Permission denied (publickey)` or `Could not resolve host github.com`)
- **AND** the returned Err does NOT contain the "exists but no .git" secondary detection text
- **AND** the chatops `workspace_init_failure` alert's `last_error_excerpt` field reflects the real clone failure

#### Scenario: Auto-cleanup itself fails on permissions or disk-full
- **WHEN** auto-cleanup runs AND `fs::remove_dir_all` returns an error
- **THEN** the daemon logs at ERROR naming the workspace path and the OS error
- **AND** the returned Err includes the OS error message
- **AND** the iteration reports Failed

### Requirement: Safety check protects operator-meaningful state from auto-cleanup
Before deleting a partial-clone artifact, the daemon SHALL verify the directory contains no operator-meaningful state. The check identifies tripwires that would indicate the broken state is NOT a freshly-failed clone but rather something the operator might care about preserving. If any tripwire fires, auto-cleanup is refused and a more informative error is returned.

#### Scenario: Marker file blocks auto-cleanup
- **WHEN** the partial-clone-state directory contains `openspec/changes/<slug>/.perma-stuck.json` OR `openspec/changes/<slug>/.needs-spec-revision.json` at any depth
- **THEN** the safety check returns Err naming the tripwire
- **AND** `ensure_initialized` returns Err with the existing "exists but no .git" message extended with `(partial cleanup refused: contains .perma-stuck.json or .needs-spec-revision.json marker; manual operator inspection required)`
- **AND** the directory is NOT deleted

#### Scenario: AskUser marker blocks auto-cleanup
- **WHEN** the directory contains `openspec/changes/<slug>/.question.json` OR `.answer.json`
- **THEN** the safety check returns Err naming the marker
- **AND** auto-cleanup is refused

#### Scenario: In-progress lock blocks auto-cleanup
- **WHEN** the directory contains a `.in-progress*` file at any depth
- **THEN** the safety check returns Err naming the lock
- **AND** auto-cleanup is refused

#### Scenario: Alert-state file is NOT a tripwire
- **WHEN** the directory contains `.alert-state.json` at the root AND has no other tripwires
- **THEN** the safety check returns Ok
- **AND** auto-cleanup proceeds normally
- **AND** `.alert-state.json` is deleted along with the partial workspace (it is daemon-written and will be re-created on the next failure if any)

#### Scenario: Empty directory passes the safety check
- **WHEN** the directory exists AND is empty (or contains only daemon-written artifacts)
- **THEN** the safety check returns Ok
- **AND** auto-cleanup proceeds

### Requirement: Existing workspace paths take the appropriate code path unchanged
The auto-cleanup branch SHALL fire ONLY when the workspace directory exists AND has no `.git/` subdirectory. Workspaces that don't exist at all SHALL continue to take the fresh-clone path (unchanged from prior behaviour). Workspaces with a valid `.git/` SHALL continue to take the fetch-and-pull path (unchanged from prior behaviour). The auto-cleanup must NOT alter either of these existing code paths.

#### Scenario: Workspace doesn't exist → fresh clone, no auto-cleanup path
- **WHEN** the workspace directory does not exist on disk
- **THEN** the auto-cleanup branch is NOT entered
- **AND** the normal clone path runs (creates the directory and clones into it)

#### Scenario: Workspace has valid .git/ → fetch/pull, no auto-cleanup path
- **WHEN** the workspace directory exists AND contains a `.git/` subdirectory
- **THEN** the auto-cleanup branch is NOT entered
- **AND** the normal fetch + pull path runs

