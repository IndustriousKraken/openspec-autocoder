## Context

We are implementing the initial phase of the OpenSpec Orchestrator as defined in the `orchestrator-architecture` change. This phase establishes the Rust foundation, command-line argument parsing, and the git utilities required to manipulate the `dev` and `agent-q` branches.

## Goals / Non-Goals

**Goals:**
- Setup a new Rust project using `cargo new orchestrator`.
- Integrate `clap` for subcommands and argument parsing.
- Provide a `git` module that wraps `std::process::Command` to execute git commands safely.
- Implement the core CLI entrypoint that parses commands and delegates them.

**Non-Goals:**
- Implementing the queue engine loop (reading/moving folders) - this will be in a subsequent change.
- Implementing the agent execution subprocess logic - this will also be in a subsequent change.
- Comprehensive PR creation using GitHub/GitLab API - for now, we just leave a placeholder or print a message indicating PR creation.

## Decisions

- **Error Handling**: Use `anyhow` for simple, idiomatic error handling across the application.
- **Git Execution**: Use `std::process::Command` over a native git library (like `git2`) to keep the binary small and leverage the host's configured git environment (ssh keys, global configs).
- **Subcommands**: Define `clap` subcommands. Specifically, we'll start with a `run` (or similar default) and a `rewind` command placeholder as defined in the specs.

## Risks / Trade-offs

- **Risk:** `std::process::Command` for git might fail unpredictably if the host environment is misconfigured.
  - **Mitigation:** Wrap git calls in robust error handling, checking exit status and capturing stderr to display useful messages to the user.
