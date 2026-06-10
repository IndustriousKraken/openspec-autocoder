You are checking a single OpenSpec change for contradictions against the project's EXISTING canonical specs. A contradiction here is a requirement IN THIS CHANGE that conflicts with a requirement that the project has ALREADY locked into canon — not a conflict the change has with itself (a separate check covers that).

A `## MODIFIED Requirements` delta REPLACES the same-titled canonical requirement when the change is archived. It is therefore EXPECTED to differ from the current canonical text — that difference IS the change, and it is the sanctioned mechanism for evolving canon. A MODIFIED delta is NEVER a contradiction with the canonical requirement that shares its title: the new version supersedes the old; the two never hold at once. Only report a conflict when honoring this change would violate a DIFFERENT canonical requirement that the change does NOT modify (or when an ADDED requirement conflicts with existing canon). Comparing a MODIFIED delta against its own same-titled canonical requirement and calling the difference a contradiction is the single most common false positive — do not make it.

The change's spec-delta files (ADDED + MODIFIED + REMOVED + RENAMED blocks across every capability the change touches) are listed below; read each one with the `Read` tool. The project's canonical specs live under `openspec/specs/<capability>/spec.md`; their paths are listed below too. Read the canonical specs that cover the same — or related — capabilities as the change's deltas, so you can compare the change against canon. If a `query_canonical_specs` MCP tool is available, you MAY use it for focused retrieval of the canonical requirements most relevant to a delta (especially when canon is large); reading `openspec/specs/*/spec.md` directly works just as well when it is not.

A change-vs-canonical contradiction is when honoring a requirement THIS CHANGE introduces or modifies would violate a requirement that is ALREADY canonical. Examples:
- The change ADDS "secrets MAY live in config.yaml"; canon already SHALL "store all secrets in env vars only."
- The change's MODIFIED scenario asserts a default ("audits run on every iteration"); a canonical requirement forbids it ("audits are strictly opt-in").
- The change re-specifies a behavior canon has already locked elsewhere with an incompatible value (a cap of 5 minutes where canon SHALL cap at 60 seconds).

NOT a change-vs-canonical contradiction:
- The change ADDS a brand-new capability that canon says nothing about.
- The change MODIFIES a canonical requirement coherently (the delta IS the intended evolution of canon, and the two do not assert conflicting behavior simultaneously).
- Wording differences with no semantic conflict, or scenarios that cover different cases under the same requirement.

Read the relevant delta blocks AND the canonical requirements they bear on. Apply domain knowledge — a "5-minute workflow" IS longer than a "60-second cap" even if the math isn't spelled out; MongoDB IS NoSQL even if neither requirement says "NoSQL."

When your analysis is complete, call the `submit_canon_contradictions` MCP tool exactly once with:

```json
{ "contradictions": [{ "change_requirement": "...", "canonical_capability": "...", "canonical_requirement": "...", "summary": "..." }] }
```

`change_requirement` names the requirement in THIS CHANGE; `canonical_capability` is the capability slug of the conflicting canonical spec (the `<capability>` in `openspec/specs/<capability>/spec.md`); `canonical_requirement` is the title of the conflicting canonical requirement; `summary` is a one-line explanation of why the two cannot both hold. Pass an empty `contradictions` array if you find none. Do NOT print the result to stdout — the daemon reads it ONLY from the `submit_canon_contradictions` tool call.
