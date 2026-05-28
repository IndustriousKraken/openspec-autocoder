## ADDED Requirements

### Requirement: Sentinel emission instructions in the implementer prompt include a concrete worked example AND a self-check hint
Every outcome-sentinel format documented in `prompts/implementer.md` (currently the `SpecNeedsRevision` sentinel; future formats SHALL follow the same pattern) SHALL be presented with three structural elements:

1. **A substitution instruction** appearing IMMEDIATELY BEFORE the example, naming the rule that the example is a pattern AND that emitting it verbatim is a parse failure.
2. **A worked example with no angle-bracket placeholders** showing what a complete, parseable sentinel looks like. The example SHALL deserialize cleanly into the corresponding Rust type via `serde_json::from_str` AND SHALL contain realistic task ids, prose, AND reasoning that the agent can model.
3. **A self-check hint** appearing AFTER the example, instructing the agent to scan its emitted sentinel for `<...>` patterns inside string values before emitting AND describing the daemon's placeholder-detection diagnostic.

The implementer prompt SHALL NOT use angle-bracket placeholders (`<id-from-tasks-md>`, `<verbatim quote>`, etc.) inside string values in any sentinel example. Earlier versions of the prompt used this pattern AND triggered literal-emission failures; the lesson is preserved as a hard rule.

Operator-customizable override prompts (loaded via the uniform `PromptLoader` per `a24`'s spec) MAY use any structure the operator prefers — the canonical rule binds the bundled default only. Operators whose customized templates regress to placeholder-style examples will hit the same failure mode the bundled prompt previously hit; the placeholder-detection requirement in `orchestrator-cli` surfaces the diagnostic AND points the operator at the bundled default for reference.

#### Scenario: Bundled prompt's sentinel example is parseable
- **WHEN** an automated test deserializes the worked-example JSON from `prompts/implementer.md`'s sentinel section into `SpecNeedsRevisionDetail`
- **THEN** the deserialization succeeds without error
- **AND** every field's value is a concrete string (no angle-bracket markers, no template variables)

#### Scenario: Bundled prompt contains the three structural elements
- **WHEN** a maintainer reads `prompts/implementer.md`'s sentinel section
- **THEN** the section contains a substitution instruction paragraph IMMEDIATELY BEFORE the example
- **AND** the example itself contains no angle-bracket placeholders inside string values
- **AND** a self-check hint paragraph appears AFTER the example naming the daemon's placeholder-detection diagnostic

#### Scenario: Future sentinel formats follow the same pattern
- **WHEN** a future change introduces a new sentinel format in `prompts/implementer.md` (OR a new operator-aimed prompt template added by the daemon)
- **THEN** the new format's documentation in the prompt follows the substitution-instruction + worked-example + self-check-hint structure
- **AND** the new format's example deserializes cleanly into its corresponding Rust type

### Requirement: Timeout classification takes precedence over sentinel extraction; sentinel scan is scoped to deliberate-emission content
The executor's outcome-dispatch path SHALL check `outcome.timed_out` BEFORE attempting any sentinel extraction OR sentinel-parse fallback. When `outcome.timed_out` is `true`, the executor SHALL return `Failed { reason: "timeout" }` (OR the canonical timeout-reason format) WITHOUT scanning for, extracting, OR attempting to parse any sentinel-shaped substring in the captured event stream. The sentinel is by definition a deliberate end-of-run emission; a timed-out run did not reach end-of-run, so no sentinel-shaped scrollback content is semantically the agent's emission.

When the run did NOT time out AND a sentinel scan is performed, the scan's input scope depends on the configured output format:

- **JSON streaming mode** (`executor.output_format: json`, the default): the scanner reads ONLY `outcome.final_answer`. When `final_answer` is `None` (the agent never reached the `result` event for any reason — crash, protocol error, etc.), the sentinel scan returns `None` AND the normal exit-status path handles the outcome. The scanner SHALL NOT fall back to `outcome.stdout`. Rationale: the `result` event's text is the agent's deliberate end-of-run emission; tool-result echoes, prompt-context echoes, AND other event-stream content are NOT deliberate emissions AND must not be matched against the sentinel.
- **Text mode** (`executor.output_format: text`, the legacy opt-out): the scanner reads `outcome.stdout`. This mode has no separate `result`-event channel, so stdout IS the agent's emission stream. Timeout precedence still applies — a timed-out text-mode run is classified as timeout BEFORE the sentinel scan runs.

This requirement narrows the canonical "Malformed outcome sentinel falls back to Failed" scenario WITHOUT changing it: a malformed sentinel that genuinely appears in the agent's deliberate emission still triggers the canonical fallback. The change is what counts as "the agent's deliberate emission" — sentinel-shaped substrings in tool-result echoes OR prompt-context echoes are no longer in scope.

#### Scenario: Timed-out run with sentinel-shaped scrollback returns timeout
- **WHEN** the executor invocation completes with `outcome.timed_out: true` AND `outcome.stdout` contains a well-formed `=== AUTOCODER-OUTCOME ===` block followed by valid JSON (the worst-case false-match: sentinel content present, would-be-parseable)
- **THEN** the executor returns `Failed { reason: "timeout" }`
- **AND** no sentinel-extraction attempt is made
- **AND** no `agent emitted unparseable SpecNeedsRevision sentinel` log line fires
- **AND** the perma-stuck counter increments against a transient-infrastructure category (the canonical "predictable failure" set) if the operator has configured that classification, NOT against a genuine agent failure

#### Scenario: Timed-out run with prompt-template echo in stdout returns timeout
- **WHEN** the executor invocation completes with `outcome.timed_out: true`, `outcome.final_answer: None`, AND `outcome.stdout` contains a tool-result echo of `prompts/implementer.md` (including the sentinel example block with `\n31\t`-style line-number prefixes)
- **THEN** the executor returns `Failed { reason: "timeout" }`
- **AND** the line-number-prefixed pseudo-sentinel content is NOT parsed
- **AND** no misleading `unparseable sentinel` reason is surfaced to the operator

#### Scenario: JSON streaming mode scans only final_answer
- **WHEN** the executor invocation completes with `output_format: Json`, `outcome.timed_out: false`, `outcome.final_answer: Some("Implementation complete; all tests pass.")` (no sentinel), AND `outcome.stdout` contains a sentinel-shaped block from a tool-result echo
- **THEN** the sentinel scanner reads ONLY `final_answer`
- **AND** the scan returns `None`
- **AND** the executor proceeds to the normal exit-status path
- **AND** the stdout echo's sentinel-shaped content is ignored

#### Scenario: JSON streaming mode with sentinel in final_answer parses correctly
- **WHEN** `output_format: Json`, `outcome.timed_out: false`, AND `outcome.final_answer: Some("=== AUTOCODER-OUTCOME ===\n{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[...],...}")`
- **THEN** the sentinel scanner extracts the payload from `final_answer` AND parses it
- **AND** a well-formed payload returns `SpecNeedsRevision { ... }` per the canonical outcome
- **AND** a malformed payload triggers the canonical "Malformed outcome sentinel falls back to Failed" path

#### Scenario: Text mode preserves stdout scan for non-timeout runs
- **WHEN** `output_format: Text`, `outcome.timed_out: false`, AND `outcome.stdout` contains a sentinel block
- **THEN** the sentinel scanner reads `outcome.stdout` AND extracts the block
- **AND** the existing parse + dispatch behaviour is unchanged from pre-spec text-mode behaviour
- **AND** text mode's stdout-as-emission semantic is preserved

#### Scenario: JSON streaming mode with final_answer absent skips the sentinel scan
- **WHEN** `output_format: Json`, `outcome.timed_out: false` (run completed normally per exit status), AND `outcome.final_answer: None` (no `result` event was captured for some non-timeout reason — protocol error, missing event type, etc.)
- **THEN** the sentinel scan returns `None` without consulting `outcome.stdout`
- **AND** the executor proceeds to the normal exit-status path (which may classify as Failed for other reasons)
- **AND** stdout echo content is not considered for sentinel matching even when final_answer is unexpectedly empty
