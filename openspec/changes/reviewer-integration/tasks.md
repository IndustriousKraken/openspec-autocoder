## 1. Reviewer Module Overhaul

- [ ] 1.1 Update `src/reviewer.rs` to accept `reviewer_config: &config::AgentConfig`.
- [ ] 1.2 Replace the `grok-cli` subprocess call with a native Rust implementation.
- [ ] 1.3 Use `std::process::Command` to execute `git diff base_branch...agent_branch` and capture the output.
- [ ] 1.4 Use `rig-core` or `reqwest` to send the diff directly to the configured LLM API endpoint.

## 2. Git Workflow Integration

- [ ] 2.1 Pass the `reviewer_config` down from `main.rs` when calling `reviewer::run_review_agent()`.
