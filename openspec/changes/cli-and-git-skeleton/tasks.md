## 1. Project Initialization

- [ ] 1.1 Run `cargo init orchestrator` (or similar) to create the base Rust project.
- [ ] 1.2 Add `clap` and `anyhow` to the `Cargo.toml` dependencies.

## 2. CLI Entry Point

- [ ] 2.1 Set up `src/main.rs` to parse CLI arguments using `clap`.
- [ ] 2.2 Define the `run` and `rewind` subcommands in the CLI struct.
- [ ] 2.3 Create a simple `match` statement in `main` to dispatch these subcommands (with placeholder print statements for now).

## 3. Git Wrapper Utilities

- [ ] 3.1 Create a new module `src/git.rs` to encapsulate git operations.
- [ ] 3.2 Implement a `checkout_dev` function that executes `git checkout dev` and `git pull`.
- [ ] 3.3 Implement a `create_agent_branch` function that executes `git checkout -b <branch_name>`.
- [ ] 3.4 Implement a `commit_and_push` function that stages all files, commits with a given message, and pushes to origin.
- [ ] 3.5 Implement a `create_pr_placeholder` function that simply logs the intended PR creation.

## 4. Integration

- [ ] 4.1 Update the main CLI `run` subcommand to execute a dummy sequence: checkout dev, create agent branch, and print "Ready for agent".
- [ ] 4.2 Verify the binary compiles and basic commands run without error.
