## 1. Dual-file structured log writer

- [ ] 1.1 In `autocoder/src/executor/claude_cli.rs` (OR the file containing `StructuredLogWriter`), refactor the writer to manage TWO output files instead of one:
  - **Summary log**: `<logs_dir>/runs/<basename>/<change>.log`. Contains `=== PROMPT (<n> bytes) ===`, the prompt text, `=== ACTIONS (see <change>.stream.log) ===` (pointer line, NO actions content), `=== FINAL ANSWER (<n> bytes) ===`, the final answer text, `=== STDERR (<n> bytes) ===`, AND the stderr bytes.
  - **Stream log**: `<logs_dir>/runs/<basename>/<change>.stream.log`. Contains the verbose action stream — `[tool_use] ...`, `[tool_result] (N bytes returned)`, `[assistant] ...`, `[raw] ...`, `[unknown:<type>] ...` lines as today. No section headers. One continuous stream.
- [ ] 1.2 Event-dispatch routing:
  - The prompt content (written once at run start) → summary log.
  - tool_use / tool_result / intermediate assistant events → stream log.
  - The closing `result` event → summary log's FINAL ANSWER section.
  - Stderr bytes captured at end-of-run → summary log's STDERR section.
- [ ] 1.3 The summary log's `=== ACTIONS (see <change>.stream.log) ===` pointer line is written ONCE between the PROMPT AND FINAL ANSWER section markers, regardless of whether the stream log has content. (If a run produced zero tool_use / tool_result events, the stream log is still created — empty — for diagnostic consistency.)
- [ ] 1.4 Both files SHALL be created lazily on first relevant event, but the summary log SHALL have all four section headers written before run-end so its structural completeness invariant (per canonical "log file is structurally complete (all section headers present; size annotations updated)") continues to hold.
- [ ] 1.5 Tests:
  - Happy-path successful run: summary log has all four sections with correct headers; stream log has the verbose action lines; their concatenation in section order equals the pre-split single-file content (semantic equivalence under the new structure).
  - Zero-action run (e.g., immediate timeout, agent never called a tool): summary log is structurally complete; stream log is empty but exists.
  - Verbose run with N tool calls: stream log has N lines per tool call (tool_use + tool_result pair plus any intermediate assistant); summary log has the pointer line AND no `[tool_*]` content.
  - File-path test: both files land at `<logs_dir>/runs/<basename>/<change>.{log,stream.log}` per the documented convention.

## 2. Retention pass updates

- [ ] 2.1 In the retention pass module (`autocoder/src/executor/log_retention.rs` OR equivalent), update the eligibility check AND deletion logic:
  - **Eligibility unchanged**: a `<change>.log` summary log is eligible for deletion when its mtime is older than `executor.log_retention_days * 86400` seconds AND its corresponding `openspec/changes/<change>/` directory does NOT exist.
  - **Deletion atomic over the pair**: when the summary log is deleted, the sibling `<change>.stream.log` (if present) SHALL be deleted in the same retention pass. The order is summary-first, then stream; partial-success (summary deleted, stream missed due to e.g. permission error) logs WARN naming the orphan AND the retention pass continues.
- [ ] 2.2 Active-change preservation extends to the pair: when `<change>.log` is preserved (change still active), the sibling stream log SHALL also be preserved regardless of mtime.
- [ ] 2.3 Edge case: a stream log present WITHOUT its summary log (manual deletion, FS corruption, or pre-spec layout artifact). The retention pass SHALL delete orphan stream logs whose age exceeds the threshold AND whose change directory does not exist, logging WARN naming the orphan. Operators inspecting logs see WARN entries indicating cleanup of the legacy-or-broken state.
- [ ] 2.4 Tests:
  - Retention-pair deletion: an aged summary + stream pair for an archived change → both deleted.
  - Retention-pair preservation: an aged pair for an ACTIVE change → both preserved.
  - Stream-only orphan: stream log without summary, aged, no change directory → stream log deleted with WARN.
  - Permission failure on stream delete: simulate failure; assert WARN fired AND retention pass continued.

## 3. Daemon-internal consumers unchanged

- [ ] 3.1 Verify no daemon code path reads `<change>.log` for the ACTIONS content. The canonical PR-comment composer reads FINAL ANSWER from the summary log — unchanged AND still correct. The `a20a1` sentinel scanner reads `outcome.final_answer` directly from the executor's structured outcome — independent of the log file structure.
- [ ] 3.2 If any other daemon-internal consumer of the per-change log exists (audit log readers, future scrapers, etc.), update them per the rule: read summary-log sections for daemon-meaningful content; read the stream log only when explicitly diagnostic AND with explicit awareness that content is agent-controllable.
- [ ] 3.3 Tests: a search of `autocoder/src/` for reads against the per-change log path reveals only the documented consumers; no incidental reads of ACTIONS content remain.

## 4. Docs update

- [ ] 4.1 In `docs/OPERATIONS.md`'s "Per-change run log shape" section, update the content:
  - Describe the two-file layout (summary at `<change>.log`, stream at `<change>.stream.log`).
  - Note the pointer line in the summary log that names the stream log path.
  - Update sample CLI snippets: `tail -f <change>.log` is for the operator-readable summary (PROMPT + FINAL ANSWER + STDERR); `tail -f <change>.stream.log` is for the live action stream during a run; `grep '[tool_use]' <change>.stream.log` (with the file path corrected) is for action-pattern searches.
  - Explicit migration note for operators with existing tooling: "Tools that grep the per-change log for `[tool_use]`, `[tool_result]`, `[assistant]`, `[raw]`, OR `[unknown:<type>]` patterns SHALL be redirected to the `<change>.stream.log` file. The summary log no longer contains those patterns."
- [ ] 4.2 No other doc updates needed — the canonical retention requirement language already says "per-change log files" (plural-friendly) AND the retention text doesn't enumerate specific filenames.

## 5. Spec deltas

- [ ] 5.1 `openspec/changes/a20a2-split-actions-stream-log/specs/executor/spec.md` MODIFIES `Executor invokes Claude CLI in JSON event streaming mode and captures events to a structured log` (preserving the 4 canonical scenarios with updated content; adding 2 new scenarios for the split).
- [ ] 5.2 The same file MODIFIES `Per-change log files are pruned after executor.log_retention_days days, preserving active-change logs` (preserving existing scenarios with updated content; adding 1 new scenario for pair-atomic deletion AND 1 for orphan stream cleanup).

## 6. Verification

- [ ] 6.1 `cargo test` passes (new tests in section 1 AND 2 + existing tests covering the structured log writer AND retention pass).
- [ ] 6.2 `openspec validate a20a2-split-actions-stream-log --strict` passes.
- [ ] 6.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
- [ ] 6.4 Manual verification:
  - Trigger any iteration that produces a non-trivial action stream (e.g., the implementer working on a small change). Inspect `<logs_dir>/runs/<basename>/<change>.log` AND confirm it contains the pointer line in the ACTIONS slot AND is materially shorter than pre-split.
  - Inspect the sibling `<change>.stream.log` AND confirm it contains the verbose action lines.
  - `tail -f` the stream log during a live iteration AND verify lines arrive as the agent emits events.
  - Verify the PR's "Agent implementation notes" comment continues to contain ONLY the FINAL ANSWER text (no leakage of action stream into PR comments).
  - Wait for a change's archive + retention cycle (OR force one in test) AND confirm both files are deleted as a pair.
