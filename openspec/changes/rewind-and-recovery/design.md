## Context

The serial queue approach means that if Proposal A is implemented but flawed, and Proposal B is implemented on top of it, we cannot easily merge B without A. If A requires a fundamental rewrite, the fastest and most "AI-native" approach is to discard the `agent-q` branch entirely, return A and B to the active queue, and let the agent re-attempt them. This `rewind` mechanism automates that cleanup.

## Goals / Non-Goals

**Goals:**
- Provide a CLI command `orchestrator rewind <change_names...>` to select specific archived changes to retry.
- Automatically find the specified changes in `openspec/changes/archive/` and move them back to `openspec/changes/`.
- Add a `--hard` flag to automatically delete the `agent-q` branch locally and remotely, resetting the workspace back to `dev`.

**Non-Goals:**
- We are not implementing selective git commit reverting. If we rewind, we nuke the `agent-q` branch entirely. Granular fixes should be handled by a human developer.
- We will not automatically modify the OpenSpec markdown files (e.g., adding "Review feedback" sections). The human is expected to update `proposal.md` or `design.md` with instructions on what went wrong *after* rewinding but *before* restarting the queue.

## Decisions

- **Branch Deletion:** We will add `git branch -D agent-q` and `git push origin --delete agent-q` to the `git.rs` module. These are destructive, so we will require a explicit `--hard` flag or an interactive prompt to prevent accidental execution.
- **Unarchiving Strategy:** The `queue.rs` module will need to search the `archive/` directory for folders ending in the requested change name (since they are prefixed with dates like `YYYY-MM-DD-`). It will strip the date prefix when moving them back to the active queue.

## Risks / Trade-offs

- **Risk:** Deleting the `agent-q` branch destroys work that might have contained partially good ideas or code.
  - **Mitigation:** The human should review the branch before running `rewind --hard`. If they want to salvage code, they can stash it or create a patch. The orchestrator's job is just to clear the board.
- **Risk:** Unarchiving an older version of a change while a newer one exists.
  - **Mitigation:** OpenSpec schemas generally require unique names, but the date prefix logic needs to be careful if there are multiple archived versions. We will just pick the most recently archived one if there are duplicates.
