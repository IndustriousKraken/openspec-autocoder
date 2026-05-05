## Why

When an agent fails to implement a change correctly, or a PR is rejected by a reviewer, the dependent tasks in the serial queue are now invalid or conflicted. We need a robust mechanism to "rewind" the queue, discard the corrupted `agent-q` branch, and put the failed and subsequent changes back into the active queue so they can be re-attempted cleanly.

## What Changes

- Implement the full logic for the `rewind` CLI subcommand introduced in the skeleton.
- Add git utilities to safely delete the local and remote `agent-q` branch.
- Add queue utilities to identify archived changes and move them back to the active queue.
- Implement interactive confirmation or dry-run features to prevent accidental data loss when destroying branches.

## Capabilities

### New Capabilities
<!-- No new capabilities, we are implementing the rewind specs defined in the orchestrator-architecture change -->

### Modified Capabilities
<!-- No modified capabilities, we are just implementing the initial draft of the existing specs -->

## Impact

This provides the critical safety net for the autonomous factory. By automating the cleanup and requeuing process, it ensures that a single AI failure doesn't permanently stall the pipeline or require complex manual git surgery.
