## Context

The orchestrator currently assumes it is running from within a specific git repository. To graduate to a CI/CD server, it must manage multiple repositories independently. This requires cloning them to a local workspace, polling them on a schedule, and running the agent loops in parallel while preventing race conditions (e.g. starting a new agent run while the previous one is still working on the same change).

## Goals / Non-Goals

**Goals:**
- Load watched repositories from a `config.yaml` file.
- Use `tokio` to spawn asynchronous polling loops for each repository.
- Implement a `workspace` manager that clones or pulls the repo into `/tmp/workspaces/<repo-name>`.
- Implement an "in-progress" lock mechanism (e.g. creating an empty `.lock` file inside the change directory) so the poller skips changes currently being worked on.

**Non-Goals:**
- We are not building a fully distributed system (no Redis, no external message brokers). Local filesystem locks and `tokio` channels are sufficient.
- We will rely on environment variables for git credentials (e.g. `GITHUB_TOKEN` for HTTPS clones or standard `~/.ssh` for SSH). Managing credentials internally is out of scope.

## Decisions

- **Async Runtime:** `tokio` is the standard for Rust async. We will convert `main.rs` to `#[tokio::main]`.
- **Configuration:** Use `serde` and `serde_yaml` to parse a configuration file containing repo URLs, polling intervals, and custom branch names (`base_branch` for the source queue, `agent_branch` for implementations).
- **Locking Mechanism:** To prevent the 5-minute poller from spinning up a new agent for a change that takes 20 minutes, the queue engine will create a `.in-progress` file inside `openspec/changes/<name>/`. The queue engine will ignore changes that contain this file.

## Risks / Trade-offs

- **Risk:** Multi-threading the `git` CLI operations could lead to weird state if two threads try to modify the same local clone.
  - **Mitigation:** Each repository gets exactly ONE polling thread. Parallelism is *across* repositories, not within a single repository. The serial queue constraint within a single repo is maintained.
- **Risk:** Leftover `.in-progress` lock files if the orchestrator crashes unexpectedly.
  - **Mitigation:** We can implement a cleanup routine on startup that removes any `.in-progress` locks, since no agents are running when the orchestrator daemon starts.
