## Why

This project requires an autonomous CI/CD pipeline to process and implement OpenSpec proposals in sequence across multiple repositories without human supervision. We need a robust, API-first Rust server daemon that acts as an orchestration brain, executing local filesystem tools via the Model Context Protocol (MCP) and integrating natively with Large Language Models (LLMs) to create an autonomous software factory.

## What Changes

- An `orchestrator` Rust application will be built to run as a persistent daemon.
- The queue will operate as a "Serial Queue" per repository to handle dependent features safely without causing git conflicts.
- The daemon will integrate the official `rust-mcp-sdk` and a native LLM client library (like `rig-core`) to execute implementations directly in-memory, avoiding fragile dependencies on external CLI wrappers.
- The daemon will generate a single, monolithic Pull Request at the end of each repository's polling pass, combining all processed changes.
- The architecture requires asynchronous communication (ChatOps) to handle AI ambiguity gracefully without blocking the queue.

## Capabilities

### New Capabilities
- `orchestrator-cli`: The core Rust asynchronous daemon entry point using `tokio`.
- `openspec-queue-engine`: Logic for watching the `openspec/changes` folder, determining the next task, and managing states (archiving, unarchiving, locking).
- `mcp-tool-server`: A Rust-native implementation of the Model Context Protocol providing local tools (read, write, shell) to the internal LLM client.
- `llm-orchestration-client`: The core loop that communicates directly with external LLM providers (Anthropic, OpenRouter) to execute tasks.
- `git-workflow-manager`: Automation around git branch creation, commits, and batching monolithic PRs via the GitHub API.

### Modified Capabilities
- (none)

## Impact

This establishes the foundational blueprint for a production-grade, multi-repo AI CI/CD server. It strictly mandates API-first integrations (MCP, GitHub, Slack) over brittle local scripts, ensuring high reliability and maintainability.
