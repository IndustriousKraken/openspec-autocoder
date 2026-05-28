## 1. Revise `prompts/implementer.md`

- [ ] 1.1 Replace the existing sentinel section (currently around lines 25-39 of the shipped file) with the new structure:
  - **Substitution instruction (before the example)**: a single paragraph naming the rule. Required text: "When you emit the sentinel below, REPLACE every value in the example with concrete data from THIS change. The angle-bracket-free example shows the shape; emitting it verbatim is a parse failure that triggers Failed-outcome handling AND eventually perma-stuck."
  - **Worked example (no placeholders)**: a complete, parseable JSON sentinel with realistic task ids AND prose. Use the example from the proposal (task `6.4`, "Manual: SSH into the production host...", with a concrete revision_suggestion). The example SHALL NOT contain any `<...>` markers.
  - **Field-by-field instruction**: a short list describing what to put in each field — `task_id` is the exact id from tasks.md; `task_text` is the verbatim text of the unimplementable task; `reason` is one line naming why it can't run in your sandbox; `revision_suggestion` is a concrete edit the operator can make to tasks.md.
  - **Self-check hint**: a final paragraph: "Before emitting, scan your sentinel for `<...>` patterns inside string values. If you see any, you have not substituted — re-read this section AND fix before emitting. The daemon detects this specific failure mode AND will surface it in the WARN log."
- [ ] 1.2 The sentinel format itself does NOT change (the JSON shape is the same `{"type":"spec_needs_revision","unimplementable_tasks":[...],"revision_suggestion":"..."}`); only the surrounding instructions + the example change.
- [ ] 1.3 Tests (manual + the per-`a24` PromptLoader unit tests): the embedded `prompts/implementer.md` parses cleanly AND a regression test confirms the worked-example JSON deserializes via `serde_json` to `SpecNeedsRevisionDetail` cleanly.

## 2. Parser-side placeholder detection

- [ ] 2.1 In `autocoder/src/executor/claude_cli.rs` (OR wherever the `SpecNeedsRevision` sentinel parse + fallback fires), extend the fallback path:
  - When `serde_json::from_str::<SpecNeedsRevisionDetail>(payload)` SUCCEEDS, scan each `task_id`, `task_text`, AND `reason` field for the regex `<[a-z][a-z0-9 _-]*>`. If any field matches, treat the sentinel as malformed (placeholder failure mode) AND fall through to the Failed-outcome path described below.
  - When `serde_json::from_str` FAILS outright (existing behavior), continue with the existing Failed-outcome path.
  - In both cases, emit the WARN log AND Failed-reason. For the placeholder failure mode specifically, the WARN log line AND Failed-reason include the diagnostic: `looks like un-substituted placeholders — the agent emitted the prompt's example verbatim instead of substituting concrete values; see prompts/implementer.md sentinel section`.
- [ ] 2.2 The regex is intentionally narrow (lowercase letters / digits / spaces / underscores / hyphens) to avoid matching legitimate `<...>` text in task descriptions (e.g., `<repo>` in a task verb syntax). Treat false positives as acceptable: a real task whose text happens to match the pattern triggers the WARN but the operator's diagnosis is unchanged (the message names the failure mode).
- [ ] 2.3 Tests:
  - Unit: a `SpecNeedsRevisionDetail` payload with literal `<id-from-tasks-md>` triggers placeholder detection; resulting Failed-reason contains the documented diagnostic text.
  - Unit: a well-formed sentinel (the proposal's worked example) parses cleanly AND does NOT trigger placeholder detection.
  - Unit: a sentinel with `<my-tool>` inside `task_text` (a legitimate-looking false positive) DOES trigger placeholder detection; the test asserts this is intentional behavior, not a defect.
  - Unit: a sentinel that fails `serde_json::from_str` outright (e.g., malformed JSON, missing `type` field) follows the existing fallback path with the original WARN text (no regression).

## 3. Spec deltas

- [ ] 3.1 `openspec/changes/a20a1-fix-sentinel-handling/specs/executor/spec.md` ADDs the worked-example-mandate requirement AND the timeout-precedence + scan-scoping requirement.
- [ ] 3.2 `openspec/changes/a20a1-fix-sentinel-handling/specs/orchestrator-cli/spec.md` ADDs the placeholder-detection requirement.

## 4. Timeout precedence AND sentinel-scope tightening

- [ ] 4.1 In `autocoder/src/executor/claude_cli.rs` (around lines 985-1016), reorder the outcome-dispatch path so the `outcome.timed_out` check fires BEFORE the sentinel extraction call. The new order:
  ```rust
  // Timeout takes precedence: a timed-out run by definition didn't
  // reach a deliberate end-of-run point, so any sentinel-shaped
  // substring in the event stream is NOT the agent's deliberate
  // emission. Classify as timeout and return.
  if outcome.timed_out {
      return Ok(ExecutorOutcome::Failed {
          reason: "timeout".to_string(),
      });
  }

  // Sentinel scan — only when the run reached normal completion.
  let sentinel_source: Option<&str> = match output_format {
      OutputFormat::Json => outcome.final_answer.as_deref(),
      OutputFormat::Text => Some(&outcome.stdout),
  };
  if let Some(source) = sentinel_source
      && let Some(payload) = Self::extract_outcome_sentinel(source)
  {
      // existing parse + dispatch — unchanged
  }
  ```
- [ ] 4.2 Replace `outcome.final_answer.as_deref().unwrap_or(&outcome.stdout)` with a `match output_format` block. In JSON-streaming mode (default), the sentinel scan considers ONLY `final_answer`. In text mode (legacy opt-out), the scan considers stdout. Both modes inherit the timeout-precedence rule from 4.1.
- [ ] 4.3 If the executor doesn't already have an `OutputFormat` enum accessible at this code site, thread the configured format (`executor.output_format`) through to the dispatch path. The field is already specced in canonical orchestrator-cli; this task just makes it observable at the right call site.
- [ ] 4.4 Tests:
  - **Timeout precedence (regression):** a fixture executor invocation with `outcome.timed_out: true` AND `outcome.stdout` containing a complete well-formed sentinel block returns `Failed { reason: "timeout" }`, NOT `SpecNeedsRevision`. The sentinel content is irrelevant when timed_out is set.
  - **Timeout precedence (the actual incident):** a fixture executor invocation with `outcome.timed_out: true`, `outcome.final_answer: None`, AND `outcome.stdout` containing the prompt-template's sentinel echo (including `\n31\t` line-number prefixes that fail JSON parse) returns `Failed { reason: "timeout" }`. The pre-fix code would have returned `Failed { reason: "unparseable sentinel ..." }` here.
  - **JSON-streaming final_answer scoping:** a fixture executor invocation with `output_format: Json`, `outcome.final_answer: Some("<agent's normal completion text — no sentinel>")`, AND `outcome.stdout` containing a sentinel-shaped block (from a tool-result echo) does NOT trigger the sentinel-failure path. The scan considers only `final_answer`; the stdout echo is ignored.
  - **JSON-streaming with sentinel in final_answer:** a fixture invocation with `output_format: Json`, `outcome.final_answer: Some("=== AUTOCODER-OUTCOME ===\n{\"type\":\"spec_needs_revision\",...}")` correctly parses AND returns `SpecNeedsRevision`. Happy path preserved.
  - **Text-mode preserves stdout scan:** a fixture invocation with `output_format: Text`, `outcome.stdout` containing a sentinel block, AND `outcome.timed_out: false` correctly extracts AND parses the sentinel. Text-mode behavior unchanged for non-timeout runs.

## 5. Verification

- [ ] 5.1 `cargo test` passes (new tests in 4.4 + existing tests covering the sentinel-extraction code).
- [ ] 5.2 `openspec validate a20a1-fix-sentinel-handling --strict` passes.
- [ ] 5.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
- [ ] 5.4 Manual verification (post-implementation):
  - Apply the changes locally.
  - Author a tasks.md with one obviously-unimplementable task. Trigger the implementer. Confirm the emitted sentinel contains substituted values, NOT `<id-from-tasks-md>` or similar.
  - Trigger a timeout artificially (set `executor.timeout_secs` very low — e.g., 5 — AND queue any non-trivial change). Confirm the daemon reports `Failed { reason: "timeout" }` AND NOT `Failed { reason: "unparseable sentinel ..." }`.
  - Hand-craft a test fixture sentinel with `<id-from-tasks-md>` literal placeholders. Confirm the placeholder-detection WARN fires with the documented diagnostic.
  - Resolve the production `a21-canonical-spec-rag-via-mcp` perma-stuck after this change ships: clear the `.perma-stuck.json` marker AND confirm subsequent iterations either complete cleanly OR (if the change has a genuine implementability gap) emit a properly-formed `SpecNeedsRevision` sentinel.
