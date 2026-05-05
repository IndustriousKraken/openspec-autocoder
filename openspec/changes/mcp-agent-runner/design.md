## Context

The orchestrator must process an OpenSpec change autonomously. By building an internal MCP-capable LLM client, we gain absolute control over the agent's tools, rate limits, and context window, avoiding the black-box nature of external CLI wrappers.

## Goals / Non-Goals

**Goals:**
- Implement `llm_client::execute_openspec_change(change_name)`.
- Use `std::process::Command` to run `openspec instructions apply --json` to get the context and instructions.
- Create an internal `mcp` module containing the tool schemas and execution functions for the local filesystem.
- Run a conversation loop that sends the instructions to the LLM, parses tool calls, executes them via the `mcp` module, and returns the result to the LLM until the LLM signals it is finished.

**Non-Goals:**
- We are not replacing the `openspec` binary for prompt generation. We rely on it to parse the Markdown/YAML safely.
- We will not implement a full web-socket MCP server. We only need the server logic running locally within the same Rust process to handle the LLM's requests.

## Decisions

- **LLM Library:** `rig-core` or a direct `reqwest` implementation to interact with the LLM API.
- **MCP Integration:** We will use `rust-mcp-sdk` to define the JSON schemas for the tools provided to the LLM.
- **Context Injection:** The output from `openspec instructions apply` contains `context`, `rules`, and `instruction`. These will form the System Prompt.

## Risks / Trade-offs

- **Risk:** The LLM context window fills up during long implementations.
  - **Mitigation:** We will implement token counting or a rolling context window (e.g., dropping older tool responses) to ensure long tasks do not fail mid-execution.
