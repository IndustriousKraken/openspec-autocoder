## Context

The project requires an automated way to implement OpenSpec proposals in a serial queue across multiple repositories. To achieve production-grade reliability, the orchestrator cannot rely on external CLI tools (like `gemini-cli`). It must act as a first-class AI client, utilizing the Model Context Protocol (MCP) to expose its local workspace to a hosted LLM.

## Goals / Non-Goals

**Goals:**
- Create a single Rust binary to orchestrate the AI software factory.
- Implement a serial queue reading from `openspec/changes/` per repository.
- Manage git operations using `std::process::Command` for local branching, but `reqwest` for external PR creation.
- Execute implementations using an internal LLM loop (`rig-core`) integrated with `rust-mcp-sdk`.
- Provide an asynchronous Slack integration to resolve AI ambiguity gracefully.

**Non-Goals:**
- A web interface or complex dashboard.
- Automatic Git merge conflict resolution.
- Re-implementing the OpenSpec schema parsing. The orchestrator will shell out to the `openspec` Node binary purely to generate the prompt instructions, but will handle the actual LLM execution internally.

## Decisions

- **Language & Runtime:** Rust with `tokio`. Essential for managing concurrent polling loops across multiple repositories while maintaining in-memory LLM conversation state.
- **AI Execution:** We will salvage core routing logic from the `rabs-coder-agent` project, leveraging `rig-core` to hit provider APIs and `rust-mcp-sdk` to execute file operations locally.
- **Git Branching Strategy:** Feature branches per queue execution pass. The orchestrator implements changes serially on a single `agent_branch`, and opens one monolithic PR at the end of the pass.
- **Queue State:** The file system (`openspec/changes/`) acts as the state database. Locks (`.in-progress`) and ChatOps state (`.waiting.json`) are stored alongside the proposals.

## Risks / Trade-offs

- **Risk:** Implementing an internal LLM loop and MCP server takes significantly more time and lines of code than wrapping an existing CLI.
  - **Mitigation:** We will salvage proven components from the `rabs-coder-agent` project.
- **Risk:** Agent processes hanging indefinitely on complex logic.
  - **Mitigation:** Strict timeouts on LLM API calls and local tool executions.
