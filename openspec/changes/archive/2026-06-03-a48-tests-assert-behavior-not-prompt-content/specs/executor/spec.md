# executor — delta for a48-tests-assert-behavior-not-prompt-content

## MODIFIED Requirements

### Requirement: MCP outcome-tool description fields encourage substantive content AND drop narrative history
The `description` field of each outcome tool advertised by the per-execution MCP child (currently `autocoder/src/mcp_askuser_server.rs`) SHALL be operationally focused — directing the agent what to do AND what content to produce — without narrative history about prior failure modes OR legacy mechanisms. The agent reads the `description` field from the MCP `tools/list` response to decide how to use the tool; that text is the primary surface for shaping agent behavior, so it SHALL carry the load-bearing operational guidance:

- `outcome_success` — names the `final_answer` field AND its reviewer-facing destination (the PR's implementation-notes section), AND directs the agent to pass a substantive end-of-run summary rather than treating the bare call as sufficient.
- `outcome_request_iteration` — names the cumulative completed/remaining state AND the blocker-naming `reason` field, AND distinguishes the tool from `outcome_spec_needs_revision`.
- `outcome_spec_needs_revision` — names the file the agent reads (`tasks.md`), the placeholder-rejection rule, AND where input validation runs (the MCP layer).

This is design intent for human-authored message content. It is verified by review AND the drift audit's semantic judgment — NOT by a unit test asserting substrings of the descriptions (per the project-documentation requirement `Tests assert behavior or derivation, never message wording`). A test that read the descriptions and asserted hand-authored wording is a change-detector that breaks on meaning-preserving rewrites; the descriptions' fitness is a judgment the drift audit makes against this requirement. The required/forbidden-substring contract AND the substring regression test mandated by the prior version of this requirement are removed.

This requirement covers description CONTENT ONLY. The tool schemas (`inputSchema`), behaviors (control-socket relay), AND output shapes are governed by the existing canonical "Per-execution MCP child exposes outcome tools via control-socket relay" AND "Per-execution MCP child exposes `outcome_request_iteration` tool" requirements AND are unchanged by this requirement.

#### Scenario: Descriptions carry operational guidance and omit narrative history
- **WHEN** the outcome-tool descriptions are reviewed against this requirement (by a human reviewer OR the drift audit)
- **THEN** each description directs the agent how to use the tool AND what content to produce
- **AND** `outcome_success`'s description directs the agent to pass a substantive `final_answer` summary AND names its reviewer-facing destination
- **AND** no description carries narrative history about prior failure modes OR superseded mechanisms (e.g. a stdout-block predecessor)

#### Scenario: Each outcome tool is advertised with a non-empty description
- **WHEN** the per-execution MCP child serves its `tools/list` response
- **THEN** each of `outcome_success`, `outcome_request_iteration`, AND `outcome_spec_needs_revision` is advertised with a non-empty `description` field
- **AND** this structural property is verified by a behavior test against the served `tools/list` output, independent of the description wording

#### Scenario: Description content intent is independent of tool schema
- **GIVEN** a future change rewrites a description AND inadvertently breaks the tool's `inputSchema` shape
- **WHEN** the change is evaluated
- **THEN** the schema violation surfaces via the existing canonical "Per-execution MCP child exposes outcome tools via control-socket relay" requirement's scenarios
- **AND** the description-content intent is governed by this requirement (review AND drift audit), independently of the schema
