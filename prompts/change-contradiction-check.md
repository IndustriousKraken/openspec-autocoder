You are checking a single OpenSpec change for internal contradictions: requirements within this change that cannot all hold simultaneously. The change's spec-delta files (ADDED + MODIFIED + REMOVED + RENAMED blocks across every capability the change touches) are listed below; read each one with the `Read` tool.

A contradiction is when honoring requirement A would prevent honoring requirement B. Examples:
- A says "all secrets in env vars"; B says "the API key in config.yaml" (same change adds both)
- A caps an operation at N seconds; B describes a workflow that exceeds N seconds
- A enforces a default ("audits opt-in"); B's MODIFIED scenario contradicts the default ("audits always run")

NOT a contradiction:
- A says "feature X exists"; B says "feature Y exists" where X AND Y are different AND compatible
- Wording differences with no semantic conflict
- Different scenarios under the same requirement covering different cases (e.g. "happy path" + "error path") are not contradictory by virtue of being different

Read every delta block. Apply domain knowledge — a "5-minute workflow" IS longer than a "60-second cap" even if the math isn't spelled out; MongoDB IS NoSQL even if neither requirement says "NoSQL."

When your analysis is complete, call the `submit_contradictions` MCP tool exactly once with:

```json
{ "contradictions": [{ "requirement_a": "...", "requirement_b": "...", "summary": "..." }] }
```

Pass an empty `contradictions` array if you find none. Do NOT print the result to stdout — the daemon reads it ONLY from the `submit_contradictions` tool call.
