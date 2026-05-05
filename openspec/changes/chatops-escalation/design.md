## Context

Because the orchestrator manages the LLM conversation loop natively (via `llm_client.rs`), we have complete control over tool execution. When the LLM decides to call the `ask_user` tool, we can intercept that request, pause the LLM, and route the question to a human. 

## Goals / Non-Goals

**Goals:**
- Implement a `comms` module using `reqwest` to interact with the Slack Web API (`chat.postMessage` and `conversations.replies`).
- Intercept `ask_user` tool calls in the LLM loop.
- Use `.waiting.json` to store the Slack thread timestamp.
- Use `.answer.json` to store the human's reply.
- Resume the LLM loop by injecting the contents of `.answer.json` as the tool response to the pending `ask_user` call.

**Non-Goals:**
- We will not handle complex interactive Slack elements (buttons, modals); just plain text thread replies.

## Decisions

- **State Persistence:** We store the ChatOps state directly inside the `openspec/changes/<name>/` directory. This ensures the conversation context survives orchestrator restarts.
- **The Resumption Flow:** When the daemon finds an `.answer.json`, it re-invokes the `llm_client`. Because we rely on OpenSpec's `- [x]` tasks for state, the LLM will just read the tasks, read the `.answer.json` context, and immediately know how to proceed.

## Risks / Trade-offs

- **Risk:** The Slack API polling loop could hit rate limits.
  - **Mitigation:** We only poll the Slack API for threads that are actively in the `.waiting.json` state, bound to the existing `poll_interval_sec` for that repository.
