## Context

We need an automated code review step. Since we already have `config.yaml` defining a `reviewer` LLM block and we already have a robust HTTP integration approach, building this module natively is straightforward.

## Goals / Non-Goals

**Goals:**
- Provide a `reviewer::run_review(diff: &str, config: &config::AgentConfig) -> Result<String>` function.
- Execute the HTTP request to the configured API endpoint.
- Capture and return the LLM's text output.

**Non-Goals:**
- We are not giving the reviewer agent MCP tools or write access. It is a read-only process that takes a string (the diff) and returns a string (the review).

## Decisions

- **Git Diff:** The orchestrator loop will use `std::process::Command` to run `git diff <base_branch>...<agent_branch>` and pass the result to the reviewer.
- **LLM Library:** Use `rig-core` (or `reqwest` manually) to interact with the LLM API using the configured model and API key environment variable.

## Risks / Trade-offs

- **Risk:** Large pull requests might exceed the context window of the reviewer model.
  - **Mitigation:** We can truncate the diff or use models with massive context windows (like Claude 3.5 Sonnet or Gemini 1.5 Pro). For the MVP, we will send the full diff.
