## Why

With the core daemon running, we need the engine that reads and manages the OpenSpec queue. This engine will monitor the `openspec/changes/` directory, determine the next job to run, and handle state transitions (archiving, locking) to ensure thread-safe execution during concurrent multi-repo polling passes.

## What Changes

- Add a `queue` module to the Rust project to parse the OpenSpec directory structure.
- Implement logic to find the oldest unarchived change that is ready for implementation, filtering out items that are currently locked.
- Implement file system operations to archive changes upon completion.
- Implement locking mechanisms (`.in-progress`) to prevent parallel polling threads from starting duplicate implementations on the same change.
- Implement file system operations to unarchive changes when a rewind is triggered.

## Capabilities

### New Capabilities
<!-- No new capabilities, we are implementing the `openspec-queue-engine` spec defined in the orchestrator-architecture change -->

### Modified Capabilities
<!-- No modified capabilities, we are just implementing the initial draft of the existing specs -->

## Impact

This module provides the core state management for the orchestrator, turning the local file system into a robust, concurrent-safe job queue for the autonomous agents.
