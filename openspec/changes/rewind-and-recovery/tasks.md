## 1. Git Recovery Utilities

- [ ] 1.1 In `src/git.rs`, implement `delete_branch(branch_name: &str, force: bool)`.
- [ ] 1.2 Make `delete_branch` attempt both local (`git branch -D <name>`) and remote (`git push origin --delete <name>`) deletion.
- [ ] 1.3 Add a confirmation prompt in `delete_branch` if `force` is false, to prevent accidental deletion.

## 2. Queue Unarchive Logic

- [ ] 2.1 In `src/queue.rs`, implement `unarchive_changes(names: Vec<String>)` returning a `Result`.
- [ ] 2.2 Have `unarchive_changes` scan the `openspec/changes/archive/` folder, parse the `YYYY-MM-DD-<name>` formats to find matches for the requested names.
- [ ] 2.3 Once matched, use `std::fs::rename` to move them back to `openspec/changes/<name>`.

## 3. Rewind Command Implementation

- [ ] 3.1 In `src/main.rs`, implement the `rewind` subcommand logic under the main `match` block.
- [ ] 3.2 Accept a list of change names as arguments to the `rewind` subcommand.
- [ ] 3.3 Add a `--hard` flag to the `rewind` subcommand structure in `clap`.
- [ ] 3.4 Wire it together: First, call `git::delete_branch("agent-q", args.hard)`. If successful, call `git::checkout_dev()`. Finally, call `queue::unarchive_changes(names)`.

## 4. Testing & Refinement

- [ ] 4.1 Create a dummy archived change in `openspec/changes/archive/` and manually test the `rewind` command.
- [ ] 4.2 Verify that the `agent-q` branch is deleted (if it exists) and that `dev` is checked out.
