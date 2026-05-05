## Why

When an agent encounters ambiguity, it uses an `ask_user` tool to request human clarification. Since the orchestrator is an automated daemon, it must intercept these tool calls and route them to a human via ChatOps (Slack), pausing the execution loop without blocking the rest of the queue.

## What Changes

- Create a `Comms Manager` module to handle the Slack Web API.
- Update the internal LLM execution loop in `llm_client.rs` to intercept `ask_user` tool calls.
- When intercepted, the orchestrator posts the question to Slack and creates a `.waiting.json` file in the change directory to save the thread state.
- The orchestrator gracefully suspends the LLM loop and moves to the next change in the queue.
- An asynchronous Slack polling loop checks for human replies in `.waiting.json` threads, capturing the answer into `.answer.json` and allowing the LLM loop to resume.

## Capabilities

### New Capabilities
- `chatops-manager`: Manages the communication strategy (Slack integration) and the filesystem state machine for questions/answers.

### Modified Capabilities
- `orchestrator-cli`: Modifies the main polling daemon to handle the "Ask User" state and resume agents from `.answer.json` contexts.

## Impact

This prevents catastrophic queue blockage by gracefully handling AI confusion. It turns a rigid, headless daemon into a collaborative, asynchronous ChatOps partner, directly injecting human context back into the LLM conversation stream.
