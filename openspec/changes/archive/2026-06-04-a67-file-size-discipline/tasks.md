# Implementation tasks

## 1. Standard (declarative layer)

- [x] 1.1 Document the size budget for contributors in the appropriate project doc (e.g. `CONTRIBUTING.md` or `docs/ARCHITECTURE.md`): files target ~500 lines / functions ~50, judgment-based; past the brightline thresholds a file/function is a structural defect that escalates with how far over it is; duplicated logic is a defect; enforcement is advisory (audits + review), never a hard gate. This is the human-readable face of the `Source files and functions stay within a size budget` requirement.

## 2. Brightline: graduated severity + production/test split (`src/audits/brightline.rs`)

- [x] 2.1 Add a `severity_for_ratio(n: u64, threshold: u64) -> Severity` helper: `Low` when `n < threshold * 3 / 2`, `Medium` when `threshold * 3 / 2 <= n < threshold * 5 / 2`, `High` when `n >= threshold * 5 / 2`. Use integer-ratio arithmetic (no floats) so the band edges are exact.
- [x] 2.2 Change `check_file_size` (currently returns a flat `Severity::Medium`) to set `severity: severity_for_ratio(n, threshold)`. Keep the existing `subject` format (`"file {rel} is {n} lines (threshold: {threshold})"`) and `anchor` (`{rel}:1`) byte-identical.
- [x] 2.3 In `check_file_size`, when the file has identifiable test-only regions (Rust: lines inside `#[cfg(test)]` modules — reuse the same module-boundary detection `strip_rust_tests_modules` / the duplicate-signature scan uses), compute `production_lines` and `test_lines` and include both alongside `lines` in the finding `body`. When no test-only region is found, keep the body reporting the total only.

## 3. Brightline: function-size metric (`src/audits/brightline.rs`)

- [x] 3.1 Add `const DEFAULT_FUNCTION_LINES_THRESHOLD: u64 = 200;` and `const SETTINGS_KEY_FUNCTION_LINES: &str = "function_lines_threshold";`. Pull `function_lines_threshold` from `audit_settings` in `new()` alongside the existing `file_lines_threshold`, defaulting to the constant.
- [x] 3.2 Add `check_function_sizes(path, root, threshold) -> Vec<Finding>` that walks each function definition outside test-only regions (reuse the existing function-signature scanning that powers the duplicate-signature metric to find each function's start line and closing delimiter), measures its line span, and emits a finding for any span over `threshold` with `subject = "function <name> in <rel> is <N> lines (threshold: <T>)"`, `anchor = Some("<rel>:<start-line>")`, and `severity = severity_for_ratio(span, threshold)`. Functions inside `#[cfg(test)]` modules are skipped.
- [x] 3.3 Invoke `check_function_sizes` for each scanned file in the audit's collect loop and extend the findings vector (the existing severity-then-subject ordering already covers the new findings).

## 4. Brightline: duplicate-body metric (`src/audits/brightline.rs`)

- [x] 4.1 Add `check_body_duplicates(files, root, ignore_entries) -> Vec<Finding>`: for each function outside test-only regions, extract its body, normalize it (strip comments, collapse whitespace, canonicalize local identifier and string-literal spellings so rename-only clones collide), and hash the normalized body. Group functions by hash; for each group of ≥2 in ≥2 distinct files, emit one `Severity::Low` finding listing the sites.
- [x] 4.2 Route duplicate-body findings through the same `.brightline-ignore` suppression used by `check_signature_duplicates` (match on `file` / `function` / `signature_match`), reusing `ignore::entry_matches_site`.

## 5. Brightline: I/O-profile signature key + chatops top-line (`src/audits/brightline.rs`)

- [x] 5.1 Change `extract_signature_sites` so the `sig_key` is the function's I/O profile — name + the sequence of parameter *types* with parameter names normalized away + the return type where the language's `signature_regex` exposes it — instead of the verbatim (name-bearing) parameter text. For languages without static parameter types, fall back to name + parameter arity. The raw `signature_line` returned for `.brightline-ignore` matching is unchanged (ignore matches the signature line, not the key).
- [x] 5.2 Extend the chatops top-line so it reads `📐 architecture_brightline on <repo>: <N> file(s) over line threshold; <P> function(s) over line threshold; <M> duplicate signature(s); <Q> duplicate body group(s); <K> stale ignore entries to clean up`, deriving `<P>` and `<Q>` from the function-size and duplicate-body finding counts (use the same subject-prefix discrimination the formatter already uses; add subject-prefix constants for the new finding kinds if needed).

## 6. Consultative prompt directive (`prompts/architecture-consultative.md`)

- [x] 6.1 Add a prioritization section directing the agent to: rank a file/function that is large relative to the codebase AND multi-responsibility as a first-rank "should this split, and along what seams?" question; reason about cohesion rather than raw line count (leave a large-but-cohesive file unflagged; a smaller multi-responsibility file may be raised); and flag families of near-identical functions (same control-flow skeleton, different names) that signature comparison misses. Keep the existing rewrite-at-scale prohibitions, language-agnostic stance, and 0-5 cap intact.

## 7. Reviewer advisory size flag (`src/code_reviewer.rs`)

- [x] 7.1 After the verdict/markdown are assembled, compute per-changed-file and per-function size state from the `ReviewContext` (full file contents) AND the unified diff: resulting line count, whether the file/function exceeds the file/function thresholds, AND whether the pass added net lines to it (additions − deletions within that file's / function's hunks). Read the same thresholds the brightline audit uses (config; defaults file `800`, function `200`).
- [x] 7.2 When a file/function is over threshold AND the pass added net lines, append an advisory `## Size advisory` section to `ReviewReport.markdown` naming the path/function, the resulting line count, and (for whole-file findings with identifiable test-only regions) the production/test split. Do NOT modify `verdict`. Skip any file/function the pass leaves the same size or smaller.

## 8. Tests

- [x] 8.1 `severity_for_ratio`: a file/function at `1×`, `1.49×`, `1.5×`, `2.49×`, `2.5×`, AND `10×` the threshold maps to `Low/Low/Medium/Medium/High/High` respectively (assert the derived `Severity`, not message text).
- [x] 8.2 File metric: a file just over threshold yields `Low`; a file at `>= 2.5×` yields `High`. A file with a `#[cfg(test)]` region reports a production/test split whose production + test counts sum to the total.
- [x] 8.3 Function metric: a non-test function over `200` lines is reported with the correct graduated severity and a `<file>:<start-line>` anchor; an equally-long function inside a `#[cfg(test)]` module is NOT reported.
- [x] 8.4 Duplicate-body metric: two functions with different names but identical bodies modulo renamed locals/literals, in different files, produce one duplicate-body finding; the same body inside a `#[cfg(test)]` module is excluded; an ignore entry covering the sites suppresses it.
- [x] 8.5 Signature I/O profile: two functions with the same name + parameter types but different parameter *names* now collide (one duplicate-signature finding); two with the same name but different parameter types do NOT.
- [x] 8.6 Top-line: a run with file, function, duplicate-signature, and duplicate-body findings renders all four counts (assert the derived counts, not the exact sentence).
- [x] 8.7 Reviewer: a `ReviewContext` whose diff grows a changed file past the file threshold yields markdown containing a size advisory for that file AND an unchanged `verdict`. A diff that only shrinks an over-threshold file yields NO advisory. A diff that grows a single function past the function threshold yields a function-level advisory. (Assert presence/absence + unchanged verdict — behavior, not copy.)

## 9. Acceptance gate

- [x] 9.1 `cargo test` passes for the autocoder crate.
- [x] 9.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 9.3 `openspec validate a67-file-size-discipline --strict` passes.
