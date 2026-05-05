## Context

The orchestrator requires a robust mechanism to parse the `openspec/changes` directory and figure out what the LLM should implement next. Because the orchestrator runs asynchronous polling loops for multiple repositories concurrently, this queue engine must be thread-safe and capable of handling local file-system locks.

## Goals / Non-Goals

**Goals:**
- Implement `queue::list_pending_changes(working_dir)`, returning a list of change names sorted by age (oldest first) that do NOT have a `.in-progress` lock.
- Implement `queue::archive_change(name, working_dir)` which moves a change folder to the `openspec/changes/archive/YYYY-MM-DD-<name>` folder.
- Implement `queue::lock_change` and `queue::unlock_change` to write and remove `.in-progress` files.
- Implement `queue::unarchive_changes(names, working_dir)` for the rewind command.

**Non-Goals:**
- Validating the OpenSpec schemas. We will rely on shelling out to the `openspec` binary later in the pipeline if we need deeper validation. Simple directory listing is enough for queue generation.
- Implementing an in-memory task queue (like Celery/RabbitMQ). The file system is the ultimate source of truth.

## Decisions

- **Queue Priority**: Sorting by folder creation/modification time, or simply alphabetical if the user prefixes with numbers (e.g. `01-`, `02-`). Relying on standard `std::fs::read_dir` and sorting alphabetically is robust enough for the MVP.
- **State Changes**: Using `std::fs::rename` to atomically move folders between the active directory and `archive/`.
- **Locking**: Creating an empty `.in-progress` file inside the change directory immediately upon selection prevents the next polling pass from picking it up while the LLM is running.

## Risks / Trade-offs

- **Risk:** Leftover `.in-progress` lock files if the orchestrator crashes unexpectedly.
  - **Mitigation:** If the daemon crashes, the lock remains. This is arguably safer than automatically resuming a half-finished LLM context. The user must manually remove the lock or use the rewind command to clear it.
