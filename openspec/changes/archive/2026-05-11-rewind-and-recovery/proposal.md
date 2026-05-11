## Why

The architecture spec defines the rewind subcommand's behavior, but phase-1-foundation deliberately implemented it minimally (single-repo, no real branch deletion logic) and `multi-repo-manager` extended the daemon to multiple repos without yet teaching rewind about them. This change finishes the rewind subcommand end-to-end and adds the `--repo` selector required for multi-repo configurations.

## What Changes

- Implement the full `rewind` subcommand: confirmation prompt (unless `--hard`), local + remote agent-branch deletion when `--hard`, unarchive of named changes via `queue::unarchive`, reset of the agent branch back to base.
- Modify `orchestrator-cli`: when the config contains multiple repositories, `rewind` requires `--repo <selector>`; with exactly one repo, the argument is optional and defaults to that repo.
- Add git utilities `delete_branch_local` and `delete_branch_remote` if not already implemented in phase-1.
- Add an interactive confirmation prompt that defaults to "no" so accidental Enter presses don't destroy work.

## Capabilities

### New Capabilities
<!-- None. This change implements existing architecture-level rewind requirements and modifies the orchestrator-cli rewind subcommand for multi-repo dispatch. -->

### Modified Capabilities
- `orchestrator-cli`: the rewind subcommand takes a `--repo <selector>` argument required for multi-repo configs and optional for single-repo configs.

## Impact

After this change, the rewind subcommand works as documented in the architecture spec across both single-repo and multi-repo deployments. Operators can recover individual repositories without affecting others, and the destructive `--hard` mode requires either explicit flag or interactive confirmation.
