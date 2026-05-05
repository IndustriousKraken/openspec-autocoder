## 1. Queue State Reader

- [ ] 1.1 Create `src/queue.rs` module.
- [ ] 1.2 Implement `list_pending_changes` to read `openspec/changes/`.
- [ ] 1.3 Filter out non-directories, the `archive` directory itself, and dotfiles (like `.DS_Store`).
- [ ] 1.4 Sort the resulting directory names alphabetically (which naturally handles `01-`, `02-` prefixes) or by modification date to determine priority.

## 2. Archiver Mechanism

- [ ] 2.1 Implement `archive_change(name)` to move the specified directory to `openspec/changes/archive/YYYY-MM-DD-<name>`.
- [ ] 2.2 Ensure the `archive` directory is created if it does not exist before attempting the move.
- [ ] 2.3 Add error handling for cases where the destination already exists.

## 3. Rewind Mechanism

- [ ] 3.1 Implement `unarchive_changes(names)` to move specified directories from `archive/` back to `openspec/changes/`.
- [ ] 3.2 Update the CLI `rewind` subcommand in `main.rs` to take a list of change names and invoke `unarchive_changes`.
- [ ] 3.3 Add logic in the `rewind` CLI command to call `git::checkout_dev()` or a similar reset function after unarchiving.

## 4. Integration

- [ ] 4.1 Update the CLI `run` subcommand to invoke `list_pending_changes`.
- [ ] 4.2 Print the queue status (e.g. "Found 3 pending changes in queue").
- [ ] 4.3 Add a unit test (or write a quick manual validation script) to test creating, parsing, and archiving a dummy folder in `openspec/changes/`.
