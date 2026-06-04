---
changelog: skip
---

## Why

`autocoder/src/llm.rs` defines two LLM clients used by the code-reviewer. Each
has two explicit error returns that **no current test exercises**:

- `AnthropicClient::complete` (llm.rs:48-87)
  - Line 78: `Err("anthropic response decode failed: {e}")` when the body
    is `2xx` but not valid JSON of shape `AnthropicResponse`.
  - Line 86: `Err("anthropic response contained no text block")` when the
    `content` array contains only non-text blocks (e.g. `image` or
    `tool_use`), or is empty.
- `OpenAiCompatibleClient::complete` (llm.rs:118-156)
  - Line 149: `Err("openai-compatible response decode failed: {e}")` on
    malformed `2xx` JSON.
  - Line 155: `Err("openai-compatible response contained no choices")`
    when the `choices` array is empty.

Existing tests in `autocoder/src/llm.rs` cover the happy path and the
non-2xx error path for both providers, but never the post-2xx parse and
shape branches above. A real provider returning a 200 with an unexpected
body shape (a partial outage, an SDK version mismatch, an unfamiliar
content block) would hit code that has never run in CI.

## What Changes

Add unit tests under the existing `tests` module in
`autocoder/src/llm.rs` that:

- Drive `AnthropicClient::complete` against a mockito server returning
  `200` with a body whose `content` array contains only a
  non-text block, and assert the returned error message names the
  missing-text-block condition.
- Drive `AnthropicClient::complete` against `200` with a body that is
  syntactically broken JSON, and assert the error names a decode
  failure.
- Drive `OpenAiCompatibleClient::complete` against `200` with
  `{"choices":[]}` and assert the error names the empty-choices
  condition.
- Drive `OpenAiCompatibleClient::complete` against `200` with broken
  JSON and assert the error names a decode failure.

No production code changes.

## Impact

- Affected code: `autocoder/src/llm.rs` (tests-only additions inside the
  existing `#[cfg(test)] mod tests` block).
- No spec changes — these are internal-helper error-path tests; no
  capability requirement currently spells out the empty-content shape
  contract.
- Breaking: no.
