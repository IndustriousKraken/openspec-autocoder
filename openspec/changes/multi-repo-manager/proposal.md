## Why

To support a multi-repo strategy, the orchestrator needs to accept dynamic configuration for repositories, branches, and API providers, and it needs to execute its polling loop asynchronously.

## What Changes

- Add a YAML configuration parser (`config.yaml`) to define watched repositories, polling schedules, branch names, and global API provider settings (like OpenRouter keys and models).
- Transition the `main.rs` entry point to `#[tokio::main]`.
- Implement a `workspace` manager to clone or pull target repositories into `/tmp/workspaces/<repo-name>`.
- Implement asynchronous processing to poll and process multiple repositories in parallel.

## Capabilities

### New Capabilities
- `multi-repo-config`: Parsing and managing the configuration of watched repositories and LLM providers.
- `workspace-manager`: Managing the lifecycle of local clones for the watched repositories.
- `concurrency-and-locking`: Parallel execution of queue engines using `tokio`.

### Modified Capabilities
- `orchestrator-cli`: Modifies the entry point to act as a daemon/polling loop over multiple workspaces.

## Impact

This elevates the orchestrator to a centralized server application capable of managing an entire fleet of AI-driven projects, and provides the essential configuration scaffolding required to make direct API calls to LLM providers.
