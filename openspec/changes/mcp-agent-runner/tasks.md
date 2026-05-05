## 1. Internal LLM Client Setup

- [ ] 1.1 Rename `src/agent.rs` to `src/llm_client.rs`.
- [ ] 1.2 Add dependencies: `rig-core` (or similar LLM library) and `reqwest`.
- [ ] 1.3 Implement `execute_openspec_change(change_name: &str, provider_config: &config::AgentConfig) -> Result<AgentStatus>` replacing the `Command::new("gemini-cli")` call.
- [ ] 1.4 Inside `execute_openspec_change`, run `openspec instructions apply --json` as a subprocess to retrieve the system prompt, constraints, and tasks.

## 2. MCP Tool Server Integration

- [ ] 2.1 Add dependency: `rust-mcp-sdk`.
- [ ] 2.2 Create `src/mcp_tools.rs` to define the local filesystem and shell execution tools.
- [ ] 2.3 Integrate the `mcp_tools` into the `rig-core` execution loop so the LLM can autonomously execute actions.

## 3. Orchestrator Loop Integration

- [ ] 3.1 Update `main.rs` to import and configure the new `llm_client` module instead of the old `agent` module.
