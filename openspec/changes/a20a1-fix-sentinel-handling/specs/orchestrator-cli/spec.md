## ADDED Requirements

### Requirement: SpecNeedsRevision parser detects un-substituted placeholders AND surfaces a clear failure mode
The Claude CLI executor's `SpecNeedsRevision` sentinel parser SHALL, after a successful `serde_json::from_str` deserialization, scan each `task_id`, `task_text`, AND `reason` field's string value for the regex `<[a-z][a-z0-9 _-]*>`. When ANY field matches, the parser SHALL treat the sentinel as malformed (the "placeholder failure mode") AND fall through to the same Failed-outcome path the canonical "Malformed outcome sentinel falls back to Failed" scenario describes — with one refinement: the WARN log line AND the `Failed { reason }` string SHALL include the diagnostic phrase:

```
looks like un-substituted placeholders — the agent emitted the prompt's example verbatim instead of substituting concrete values; see prompts/implementer.md sentinel section
```

This refinement narrows the existing catch-all Failed-outcome message. The intent: when an operator inspects a Failed iteration's logs, they immediately know whether the failure is "agent emitted garbage JSON" (the original case) OR "agent emitted the prompt's example without filling in values" (the new placeholder-detection case). The two failure modes have very different operator responses — the first usually means the agent is confused about format; the second means the prompt template OR the operator's prompt override has regressed.

The detection regex is intentionally narrow (lowercase letters, digits, spaces, underscores, hyphens between the angle brackets) to avoid matching legitimate `<...>` text that might appear in task descriptions — e.g., a task body that names a chatops verb syntax like `@<bot>` OR a docs reference like `<repo-substring>`. False positives in this narrow sense ARE possible (a legitimate task whose text happens to include lowercase angle-bracket content); the regex SHALL be treated as a heuristic. The diagnostic phrase is helpful to the operator either way: if it's a true positive (prompt regression), the diagnostic points at the prompt; if it's a false positive (a real task with `<thing>` in its text), the operator's resolution is the same (review the agent's output AND the task text together).

This requirement is additive to the canonical "Malformed outcome sentinel falls back to Failed" scenario. That scenario still fires for any other parse failure (JSON syntax error, missing required field, unknown `type` value, empty `unimplementable_tasks` list, etc.); placeholder-detection adds a more specific diagnostic for one narrow case.

#### Scenario: Placeholder in task_id triggers the detection
- **WHEN** the agent emits a sentinel whose `task_id` field has the value `<id-from-tasks-md>` (literal angle-bracket content matching the regex)
- **AND** the sentinel otherwise deserializes successfully
- **THEN** the parser treats it as malformed AND falls through to the Failed-outcome path
- **AND** the WARN log line names `PromptId::Implementer` (OR the override path) AND the diagnostic phrase `looks like un-substituted placeholders — the agent emitted the prompt's example verbatim instead of substituting concrete values; see prompts/implementer.md sentinel section`
- **AND** the `Failed { reason }` string contains the same diagnostic phrase
- **AND** the polling loop's existing Failed-outcome handling kicks in (perma-stuck counter increments, no marker written)

#### Scenario: Placeholder in task_text triggers the detection
- **WHEN** the agent emits a sentinel whose `task_text` field has the value `<verbatim quote>`
- **THEN** the same placeholder-detection path fires as for task_id

#### Scenario: Placeholder in reason triggers the detection
- **WHEN** the agent emits a sentinel whose `reason` field has the value `<one-line why>`
- **THEN** the same placeholder-detection path fires

#### Scenario: Well-formed sentinel is unaffected
- **WHEN** the agent emits a sentinel with substituted values (task_id `6.4`, task_text `Run sudo systemctl restart nginx on the production host`, reason `executor sandbox has no sudo access on the production host`)
- **THEN** the parser proceeds with the normal `SpecNeedsRevision` outcome
- **AND** placeholder detection does NOT fire
- **AND** the polling loop writes the `.needs-spec-revision.json` marker AND posts the chatops alert per the canonical "autocoder writes the marker and alerts" scenario

#### Scenario: Narrow regex tolerates legitimate angle-bracket text
- **WHEN** the agent emits a sentinel whose `task_text` is `Document the @<bot> verb in docs/CHATOPS.md`
- **AND** the substring `@<bot>` matches the regex `<[a-z][a-z0-9 _-]*>` only at the inner `<bot>` portion
- **THEN** placeholder detection DOES fire (the `<bot>` portion matches the regex)
- **AND** the operator's resolution is to review the agent's output: if the task text genuinely needs `<bot>` AND the sentinel is otherwise correct, the operator clears the perma-stuck AND can comment on the task text to disambiguate; if the sentinel is a placeholder regression, the operator follows the diagnostic
- **AND** false positives are accepted as a tradeoff for the heuristic's narrow scope (we prefer over-flagging to under-flagging on this rare case)

#### Scenario: Existing malformed-sentinel path remains for non-placeholder failures
- **WHEN** the agent emits a payload that fails `serde_json::from_str` (e.g., malformed JSON, missing `type` field, empty `unimplementable_tasks` list)
- **THEN** the canonical "Malformed outcome sentinel falls back to Failed" scenario fires with its existing WARN text (`agent emitted unparseable SpecNeedsRevision sentinel: <excerpt>`)
- **AND** the placeholder-detection diagnostic does NOT appear (the new diagnostic is reserved for the deserialize-success-but-contains-placeholder case)
