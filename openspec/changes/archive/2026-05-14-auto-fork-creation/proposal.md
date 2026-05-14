## Why

The `fork-and-pr-workflow` change shipped with manual fork creation as a
non-goal — operators were expected to pre-fork every upstream repository
to the machine-user account before pointing autocoder at it. In
practice, this means adding a repo to `config.yaml` requires two
coordinated actions: edit YAML, then fork-on-github.com (web UI or
`gh repo fork`) for every new entry. For a deployment scaling to 8+
repos and growing, the manual loop becomes operational friction the
daemon is supposed to remove.

This change makes fork creation automatic: when fork-PR mode is active
AND a configured repository's fork does not yet exist, autocoder
creates it via the GitHub REST API at startup, polls until the new
fork is reachable, then proceeds normally. Adding a repo to
`config.yaml` and restarting the daemon now Does The Right Thing.

The behavior is bounded and idempotent: forks that already exist are
detected via the existing `git ls-remote` probe and left alone; the
POST is only issued when the probe fails. The GitHub API itself
returns the existing fork's metadata when called against an
already-forked repo, so a race between two daemons starting against
the same config is harmless.

## What Changes

- Add `pub async fn create_fork(api_base, upstream_owner, upstream_repo, token)` in `src/github.rs` calling `POST /repos/{upstream-owner}/{upstream-repo}/forks` with `Authorization: Bearer <token>`. Returns Ok on 202 (Accepted) or 200 (already forked).

- Rename `cli::run::validate_fork_existence` to `ensure_forks_exist` (and make it `async`). Logic:
  1. For each configured repo in fork-PR mode, run the existing `git ls-remote` probe.
  2. If reachable → success for that repo; continue.
  3. If unreachable → call `github::create_fork` using the PAT resolved by the existing `resolve_token` for the upstream owner.
  4. After a successful POST, poll the fork URL via `git ls-remote` every 2 seconds for up to 60 seconds.
  5. If polling succeeds within the timeout → success for that repo.
  6. If polling times out, or if the POST itself fails (non-2xx) → record the failure with both the upstream URL and the underlying error.
  7. After iterating all repos, if any failures, abort startup with the aggregated message.

- The PAT used for the fork-creation API call is the same one already resolved for the upstream owner via `owner_tokens` / `token` / `token_env`. The minting user owns the destination of the fork (the machine user creates the fork in their own account, which is implicit because the PAT is theirs).

- README updates: `fork-and-pr-workflow` section 7 step 2 changes from "manually fork each repo" to "autocoder forks automatically on first startup; nothing to do." Step 3's PAT-permission note: for fine-grained PATs, "Administration: write" is typically required to fork.

- Logging: when autocoder creates a fork, emit one `info` line per fork naming the upstream URL and the created fork URL. When polling waits, emit one `info` line per repo named "waiting for fork `<url>` to become reachable (up to 60s)" so the operator sees progress.

## Capabilities

### Modified Capabilities

- `orchestrator-cli`: the fork-existence requirement gains an
  auto-creation step before its probe. The probe-and-fail behavior
  is preserved for the case where creation succeeds but propagation
  times out, or where the PAT cannot create the fork.

## Impact

Operators using fork-PR mode no longer need to pre-fork each repo
manually. Adding a new repo to `config.yaml` and restarting the
daemon is a complete workflow.

Existing operators who already have all their forks created will see
no observable change — the existing `git ls-remote` probe succeeds
on the first pass and the auto-create branch is never taken.

The PAT permission profile gains `Administration: write` for
fine-grained PATs that need to create forks (or `repo` scope for
classic PATs, which already covers fork creation). Operators with
fine-grained PATs minted before this change must add the
`Administration: write` permission and re-mint; the README updates
call this out.
