## Why

To add a layer of safety before human approval, we need to integrate an automated code review step. Because the orchestrator is an API-first LLM client, this review can happen entirely in-memory using direct API calls, rather than relying on external scripts.

## What Changes

- Add a `reviewer` module.
- Retrieve the git diff between the `base_branch` and the newly committed `agent_branch`.
- Construct a system prompt for the review task and send the diff to the configured Reviewer LLM API (e.g. Anthropic, Grok, MiMo).
- Capture the API response as the review report.

## Capabilities

### New Capabilities
- `reviewer-agent-integration`: The logic required to invoke an AI reviewer on the newly committed changes and capture its feedback.

### Modified Capabilities
- `git-workflow-manager`: Modify the workflow to append the review report to the monolithic Pull Request.

## Impact

This provides an automated quality check, catching potential errors or regressions before the PR is presented to the human developer.
