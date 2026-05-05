## Why

With the queue state managed and async polling configured, the orchestrator needs the ability to actually execute the implementation tasks. Instead of shelling out to an external AI CLI (which requires complex external dependencies), we will implement an internal LLM execution loop using an MCP tool server to achieve a native, API-first "Autonomous Factory."

## What Changes

- Implement an `llm_client` module to handle direct API communication with an LLM provider (using `rig-core` or similar).
- Implement an `mcp_tools` server internally to expose `read_file`, `write_file`, and `run_shell_command` directly to the LLM.
- Have the orchestrator shell out to `openspec instructions apply` to retrieve the dynamically generated prompt, and feed that prompt into the internal LLM loop.
- Stream the LLM's thought process and tool execution to the console.

## Capabilities

### New Capabilities
- `mcp-tool-server`: A Rust-native implementation of the Model Context Protocol to provide local tools.
- `llm-orchestration-client`: The core loop that talks to external LLM providers and manages conversation state.

### Modified Capabilities
- (none)

## Impact

This transforms the orchestrator into a fully autonomous, self-contained AI Client. By managing the tool execution locally via MCP, it guarantees full visibility into the agent's actions and avoids fragile wrappers.
