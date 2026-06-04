## Why

The audit framework already counts file length — the `architecture-brightline` audit reports any source file over a line threshold (default `800`) as a finding. But the signal is **flat**: a file one line over threshold and a file twenty times over both produce the same quiet `medium`, and nothing escalates a runaway file into action. LLM-driven development tends to grow single files without bound, and a concrete module in this codebase has reached **17,943 lines** — its production half is thirteen distinct responsibilities (alerts, queue walking, preflight, review-context assembly, PR construction, rebuild, triage, proposals, outcome handling), its largest single function is **571 lines**, and a family of ~12 near-identical alert helpers collapses to one parameterized helper.

The most important property of that file: an audit found **near-zero dead code** — every function is reachable and compiler-clean, with no `#[allow(dead_code)]`. The defect class is *reachable structural bloat* — size and duplication, not abandonment. A dead-code linter passes such a file; only a size/duplication signal catches it. That is exactly what the auditors must be sharpened to flag.

Three gaps keep the existing audit from flagging this well:

1. **Flat severity** — no escalation as a file climbs past the threshold, so a 22×-over file reads the same as a barely-over one.
2. **No function-size metric** — a 571-line function sails through once it sits in a small file.
3. **No production/test split** in the finding — the remedy differs (extract a test module vs. decompose production), and a bare line count doesn't tell the operator which.

And the **judgment layer is missing**: a line count is mechanical, but whether a large file *should* split depends on cohesion — a genuinely single-responsibility file may justify its length, while a smaller multi-responsibility file may not. That judgment belongs to the LLM-driven consultative audit and to code review, not to a line counter.

## What Changes

**A declarative standard (the architectural layer).** A new `project-documentation` requirement states the size budget as canon — files target ~500 lines, functions ~50, both judgment targets (cohesion is the test, not the count); past the brightline thresholds a file/function is a structural defect to address, escalating with how far over it is; duplicated logic is likewise a defect. The standard is the single source of the budget the three enforcement points reference, and it fixes the posture: advisory, never a hard gate. This is the "one other level of protection" — the law, independent of the audit that detects violations.

**Brightline (mechanical) gains escalation, reach, and a real duplication detector.** The file-size finding's severity becomes **graduated** by how far over threshold the file is (`low` / `medium` / `high` at `1×` / `1.5×` / `2.5×` the threshold). A new **function-size metric** flags overlong functions on the same graduated scale. The file-size finding body reports the **production/test line split** where the audit can identify test-only regions, so the operator knows whether to extract tests or decompose. A new **duplicate-body metric** detects near-identical function bodies across files — the copy-paste families (e.g. the ~12 alert helpers) that the signature metric can't catch because they have different names. The existing **duplicate-signature metric is corrected** to key on the function's I/O profile (name + parameter *types* + return type) instead of the verbatim parameter text, so it matches the conventional meaning of "signature" rather than keying on parameter names. Thresholds stay operator-configurable with defaults (file `800`, function `200`); `.brightline-ignore` suppression is unchanged and now also covers duplicate-body findings.

**Consultative (judgment) prioritizes the worst offenders.** The `architecture_consultative` audit's prompt is directed to surface the most over-sized, *least cohesive* file or function as a first-rank "should this split, and along what seams?" question — while explicitly leaving large-but-cohesive code alone — and to flag families of near-identical functions (same skeleton, different names) that the brightline's identical-signature detector cannot catch.

**Code review catches growth at the gate.** The reviewer adds an **advisory, non-blocking** observation when a pass pushes a changed file or function past a size threshold, or grows one already over it — distinguishing newly-introduced bloat from pre-existing bloat, and not penalizing a pass that shrinks an oversized file. Size is a maintainability signal, so it never on its own forces a `Block` verdict.

The chosen posture is **advisory + reviewer catch**: oversized code escalates in severity and rank in the periodic audits, the worst offenders surface as consultative split questions, and PRs that grow files past a threshold get a review note — but nothing blocks a PR or archive on size alone.

## Impact

- **Affected specs:**
  - `project-documentation` — ADD `Source files and functions stay within a size budget` (the declarative standard).
  - `orchestrator-cli` — MODIFY `Architecture-brightline audit` (graduated severity, function-size metric, production/test split, duplicate-body metric, I/O-profile signature key, top-line gains function-count and duplicate-body clauses); ADD `Consultative audit prioritizes oversized, low-cohesion code`.
  - `code-reviewer` — ADD `Reviewer flags files and functions that breach the size brightline`.
- **Affected code:**
  - `autocoder/src/audits/brightline.rs` — `check_file_size` returns graduated severity and a body with the production/test split; a new function-size check (`DEFAULT_FUNCTION_LINES_THRESHOLD = 200`, settings key `function_lines_threshold`) reusing the existing function-signature scanning; a new duplicate-body check (normalized-body clone grouping) sharing the `.brightline-ignore` suppression path; `extract_signature_sites` re-keyed to the I/O profile (strip parameter names, add return type); the chatops top-line formatter gains `<P> function(s) over line threshold` and `<Q> duplicate body group(s)` clauses.
  - `prompts/architecture-consultative.md` — adds the size-prioritization and near-identical-family directives.
  - `autocoder/src/code_reviewer.rs` — appends a deterministic, advisory size observation to `ReviewReport.markdown`; the verdict is untouched.
- **Operator-visible behavior:** oversized files/functions escalate in severity and rank; the worst offenders surface as consultative split questions; PRs that grow a file/function past a threshold get an advisory review note. No hard gate — nothing blocks on size alone.
- **Dependencies:** none. This change MODIFIES only the brightline requirement (which no in-flight change touches) and ADDs two new requirements, so it does **not** collide with a57's transport change to the consultative audit and is independent of the fleet stream. It is a standalone code-health control.
- **Acceptance:** `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean; `openspec validate a67-file-size-discipline --strict` passes.
