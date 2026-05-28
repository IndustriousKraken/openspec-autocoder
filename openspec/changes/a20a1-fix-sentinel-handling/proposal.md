## Why

Three compounding defects in the sentinel-handling path surfaced from a real perma-stuck incident on `a21-canonical-spec-rag-via-mcp`. The agent worked productively for ~60 minutes against a one-hour timeout, the timeout fired, AND the daemon reported `agent emitted unparseable SpecNeedsRevision sentinel: {"type":"spec_needs_revision","unimplementable_tasks":[\n31\t  {"task_id":"<id-from-tasks-md>",...` instead of `timeout`. The change perma-stuck'd at three iterations, masking the real cause from the operator. Diagnosis identified three layered defects:

### Defect 1 — angle-bracket placeholder template invites verbatim emission

The implementer prompt template at `prompts/implementer.md` (lines 28-33 of the current shipped version) specifies the `SpecNeedsRevision` outcome sentinel using angle-bracket placeholders:

```
=== AUTOCODER-OUTCOME ===
{"type":"spec_needs_revision","unimplementable_tasks":[
  {"task_id":"<id-from-tasks-md>","task_text":"<verbatim quote>","reason":"<one-line why>"}
],"revision_suggestion":"<free-form text describing what to change in tasks.md to make the spec verifiable>"}
```

No accompanying instruction tells the agent to substitute concrete values. The agent could treat the template as the literal output format AND emit it verbatim, with `<id-from-tasks-md>`, `<verbatim quote>`, AND the other markers unsubstituted. Even when the agent doesn't emit the sentinel itself, the placeholder block exists in the prompt template as documentation AND can be echoed by the wrapped CLI's event stream — see Defect 3.

### Defect 2 — sentinel scan precedes timeout classification

In `autocoder/src/executor/claude_cli.rs` (lines ~985-1016), the executor scans for the sentinel BEFORE checking `outcome.timed_out`. The current order:

```rust
let sentinel_source: &str = outcome
    .final_answer
    .as_deref()
    .unwrap_or(&outcome.stdout);
if let Some(payload) = Self::extract_outcome_sentinel(sentinel_source) {
    // ... parse and possibly return Failed { reason: "unparseable sentinel ..." }
}

if outcome.timed_out {
    return Ok(ExecutorOutcome::Failed { reason: "timeout".to_string() });
}
```

When a timeout fires AND any sentinel-shaped string exists anywhere in scrollback, the sentinel-failure path returns first AND the timeout classification is never reached. Operators see "unparseable sentinel" instead of the actual cause; perma-stuck increments against a transient infrastructure failure (timeout) rather than a genuine agent failure.

### Defect 3 — sentinel scanner falls back to full stdout when `final_answer` is absent

The same code's fallback (`final_answer.as_deref().unwrap_or(&outcome.stdout)`) is operationally wrong for the timeout case. When `final_answer` is `None` — which is EXACTLY the timeout case, since the agent never reaches the wrapped CLI's `result` event — the scanner falls back to scanning the entire captured event stream. That stream contains tool-call results AND prompt-echo content, both of which can include the sentinel marker as documentation (the implementer prompt itself contains the sentinel example; any agent that Reads `prompts/implementer.md` OR reads other documentation containing the marker echoes it into scrollback). The scanner then false-matches on the documentation echo, extracts what it thinks is JSON, AND fails to parse it because the echo includes line-number prefixes (the `\n31\t` in the incident report is the dead giveaway — `cat -n`-style Read tool output).

### Combined effect

The three defects compound. A timeout on a productive run produces:
1. `outcome.timed_out: true`, `outcome.final_answer: None`.
2. Defect 2: timeout check is skipped — sentinel scan runs first.
3. Defect 3: scan falls back to `outcome.stdout`, finds the prompt-echo / tool-result sentinel marker.
4. Parse fails because the echo isn't valid JSON.
5. Defect 1's symptom (placeholder text in error) appears, misleading diagnosis toward a prompt-template issue.
6. Operator gets `Failed { reason: "unparseable sentinel ..." }` instead of `Failed { reason: "timeout" }`.
7. Three iterations of the same → perma-stuck for the wrong reason.

The fix is layered: revise the prompt template (Defect 1), check `timed_out` BEFORE the sentinel scan (Defect 2), AND remove the `.unwrap_or(&outcome.stdout)` fallback in JSON streaming mode so the scanner SHALL only consider deliberate emissions (Defect 3). A perma-stuck incident currently in production (a21-canonical-spec-rag-via-mcp's third iteration) is unblocked once these land.

## What Changes

**Revise `prompts/implementer.md`'s sentinel section to anchor the agent's emission with a concrete worked example AND an explicit substitution instruction.** The change is content-only in `prompts/implementer.md`; no API surface or executor logic changes.

The revised sentinel section structure:

1. **Instruction paragraph** explicitly naming substitution: "When you emit the sentinel, REPLACE every value in the example below with concrete data from this change. The example is a pattern; emitting it verbatim is a parse failure."
2. **Worked example** showing what a real sentinel looks like, with realistic task ids AND prose:
   ```
   === AUTOCODER-OUTCOME ===
   {"type":"spec_needs_revision","unimplementable_tasks":[
     {"task_id":"6.4","task_text":"Manual: SSH into the production host and verify systemctl status autocoder","reason":"executor sandbox has no real SSH credentials and no production host access"}
   ],"revision_suggestion":"Replace task 6.4 with a unit test that mocks systemctl-status output, OR move the live-host check to docs/SMOKE.md as an operator step rather than an implementer task."}
   ```
3. **Validation hint** to help the agent self-check: "Before emitting, scan your sentinel for any `<...>` patterns. If you see angle-bracket text inside string values, you have not substituted — the daemon will reject the sentinel as a parse failure."

**Establish a canonical pattern for sentinel templates in implementer prompts** so future sentinel additions (and operator-authored override templates) follow the same shape: instruction + worked example + validation hint. The pattern is documented in a new requirement in the executor capability.

**Parser-side detection of the placeholder failure mode.** When the daemon's `SpecNeedsRevision` parser encounters a payload whose `task_id`, `task_text`, OR `reason` field contains `<...>` patterns that look like un-substituted placeholders (regex: `<[a-z][a-z0-9 _-]*>`), the WARN log SHALL name the specific failure mode (`looks like un-substituted placeholders — see prompts/implementer.md`) instead of just `unparseable sentinel: <excerpt>`. The Failed outcome's reason string gains the same hint. This makes the operator's diagnosis instant when the prompt regresses in the future.

This change does NOT alter the canonical "Malformed outcome sentinel falls back to Failed" scenario — that remains the behavior. The change adds a clearer log AND error message for the placeholder-specific case.

**Timeout takes precedence over sentinel classification.** The executor's outcome-dispatch code SHALL check `outcome.timed_out` BEFORE attempting any sentinel extraction. When the timeout is set, the executor SHALL return `Failed { reason: "timeout" }` (OR the canonical timeout-reason format if more specific) without scanning for OR attempting to parse a sentinel. The sentinel is a deliberate emission shape; a timed-out run by definition didn't reach a deliberate end-of-run point, so any sentinel-shaped substring in the captured event stream is by-construction NOT the agent's deliberate emission.

**JSON-streaming sentinel scan is scoped to `final_answer` only.** When `executor.output_format: json` (the default), the sentinel scanner SHALL read ONLY `outcome.final_answer`. The fallback to `outcome.stdout` (`final_answer.as_deref().unwrap_or(&outcome.stdout)`) SHALL be removed for the JSON-streaming case. Rationale: in JSON-streaming mode, the `result` event's text IS the agent's deliberate end-of-run emission; any sentinel must appear there. Tool-result echoes, prompt-context echoes, AND other event-stream content are NOT deliberate emissions AND SHALL NOT be considered. When `final_answer` is `None` (because the agent never reached `result` — timeout, crash, OR protocol error), the sentinel scan returns `None` AND the normal exit-status path handles the outcome.

When `executor.output_format: text` (the legacy opt-out), the stdout-fallback SHALL be retained as it's the only signal. Even in text mode, however, the `timed_out` precedence rule above applies: a timed-out text-mode run is classified as timeout BEFORE sentinel scanning, so the false-match path is eliminated there too.

## Impact

- **Affected specs:**
  - `executor` — ADDED requirement: `Sentinel emission instructions in the implementer prompt include a concrete worked example AND a self-check hint`.
  - `executor` — ADDED requirement: `Timeout classification takes precedence over sentinel extraction; sentinel scan is scoped to deliberate-emission content`. Covers the timed_out-first ordering AND the JSON-streaming-mode final_answer-only scoping.
  - `orchestrator-cli` — ADDED requirement: `SpecNeedsRevision parser detects un-substituted placeholders AND surfaces a clear failure mode`. This is additive to the canonical "Malformed outcome sentinel falls back to Failed" scenario (which remains the catch-all); the new requirement narrows the WARN log AND Failed-reason for the specific placeholder failure mode.
- **Affected code:**
  - `prompts/implementer.md` — revise the sentinel section per the proposed structure.
  - `autocoder/src/executor/claude_cli.rs` (lines ~985-1016) — reorder the outcome-dispatch checks so `outcome.timed_out` is consulted BEFORE the sentinel scan; replace `outcome.final_answer.as_deref().unwrap_or(&outcome.stdout)` with conditional logic that uses `final_answer` only in JSON-streaming mode AND falls back to stdout only in text mode; extend the parse-failure handler to detect un-substituted placeholders AND emit the enhanced WARN + Failed-reason text.
- **Operator-visible behavior:**
  - Future spec-revision-warranted changes produce parseable sentinels (the prompt no longer emits placeholders).
  - If a placeholder regression ever sneaks back (operator authors a custom prompt template AND copies the example without substitution guidance), the WARN log AND alert immediately name the failure mode, cutting diagnosis time.
  - No new config knobs.
- **Breaking:** no. Operator-customized implementer prompt templates remain valid; this change updates only the bundled default AND adds detection that helps operators whose customizations regress.
- **Acceptance:** `cargo test` passes; `openspec validate a28-fix-sentinel-template --strict` passes. Tests:
  - Unit test: a fixture sentinel payload with literal `<id-from-tasks-md>` triggers the placeholder-detection path; the resulting Failed-reason names the failure mode.
  - Unit test: a well-formed sentinel parses cleanly per existing behavior (no regression).
  - Manual: re-trigger `a21-canonical-spec-rag-via-mcp` after the prompt revision lands; verify the agent either implements cleanly (a21 is now revised) OR emits a parseable spec_needs_revision sentinel (if the agent finds any remaining gap).
