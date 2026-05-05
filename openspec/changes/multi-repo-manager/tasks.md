## 1. Async and Configuration Setup

- [ ] 1.1 Add `tokio` (with full features), `serde`, and `serde_yaml` to `Cargo.toml`.
- [ ] 1.2 Change `main.rs` entry point to `#[tokio::main]`.
- [ ] 1.3 Create `src/config.rs` to define and parse a `Config` struct from a YAML file.
- [ ] 1.4 Update the `Config` struct to include a `global` block containing `slack_bot_token_env`, and nested `reviewer` and `implementer` configurations (provider, model, api_key_env).
- [ ] 1.5 Update the `RepoConfig` struct to include `url`, `poll_interval_sec`, `base_branch`, `agent_branch`, and `slack_channel_id`.
- [ ] 1.6 Update the CLI `run` (or `start`) subcommand to accept a `--config` path argument.

## 2. Workspace Management

- [ ] 2.1 Create `src/workspace.rs` module.
- [ ] 2.2 Implement `initialize_workspace(repo_url: &str)` to `git clone` or `git pull` the target repository into a subdirectory under `/tmp/workspaces/`.
- [ ] 2.3 Update existing `git.rs` functions to accept an optional `working_dir` argument so they execute inside the correct `/tmp/workspaces/<repo>` folder rather than the current directory.

## 3. Queue Locking Mechanism

- [ ] 3.1 Update `src/queue.rs` `list_pending_changes` to filter out any directory that contains a `.in-progress` file.
- [ ] 3.2 Implement `lock_change(name: &str)` to create the `.in-progress` file.
- [ ] 3.3 Implement `unlock_change(name: &str)` to delete the `.in-progress` file (useful for cleanup or failure recovery).

## 4. Concurrent Polling Daemon

- [ ] 4.1 In `main.rs`, implement a function `start_polling_loop(repo_config, workspace_path, global_config)` that runs infinitely, sleeping for the configured interval.
- [ ] 4.2 In the loop: call `workspace::initialize_workspace()`, check the queue, lock the change, run the LLM client, unlock/archive, and loop.
- [ ] 4.3 In `main.rs` `run` command, iterate over the loaded config and use `tokio::spawn` to launch a polling loop for each configured repository.
- [ ] 4.4 Add a `tokio::signal::ctrl_c()` handler to gracefully shut down the daemon.
