# orchestrator-cli — delta for a67-file-size-discipline

## MODIFIED Requirements

### Requirement: Architecture-brightline audit
autocoder SHALL ship an `architecture-brightline` audit in the periodic audit framework. The audit is pure-code (no LLM invocation), `requires_head_change = true`, AND `WritePolicy::None`. It SHALL produce `AuditOutcome::Reported(findings)` containing structural metrics that exceed configured (or default) thresholds: whole-file length, function length, duplicate function signatures, AND duplicate function bodies.

**Graduated severity.** For the size metrics (file length AND function length), the finding's severity SHALL be determined by the ratio of the measured line count `N` to the applicable threshold `T`: `low` when `N` is at least `T` but below `1.5 × T`, `medium` when `N` is at least `1.5 × T` but below `2.5 × T`, AND `high` when `N` is at or above `2.5 × T`. This replaces the previous flat `medium` severity for the file-size metric, so a file barely over threshold reads as `low` while a file many multiples over reads as `high`.

**Function-size metric.** In addition to whole-file length, the audit SHALL measure the line span of each function definition (its signature line through its closing delimiter) outside test-only regions — e.g. Rust `#[cfg(test)]` modules, consistent with the duplicate-signature metric's exclusion of `mod tests {}` blocks — AND report any function whose span exceeds the function-line threshold. The file-line threshold defaults to `800` AND the function-line threshold defaults to `200`; both are operator-configurable via the audit's settings (`file_lines_threshold`, `function_lines_threshold`).

**Production/test split.** Where the audit can identify test-only regions within a flagged file (e.g. `#[cfg(test)]` modules), the file-size finding's body SHALL report the production-line AND test-line breakdown alongside the total, so the operator can tell a file that needs its tests extracted from one that needs its production code decomposed. Where no test-only region is identifiable, the body reports the total only.

**Duplicate-body metric.** Beyond identical signatures, the audit SHALL detect groups of two or more functions in different files (outside test-only regions) whose normalized bodies are identical — normalization strips comments, collapses whitespace, AND canonicalizes local identifier and string-literal spellings, so that rename-only clones (e.g. a family of helpers that differ only in a constant name and a few words of message) are matched despite differing function names. Each group emits one finding of severity `low` listing the sites. Duplicate-body findings participate in `.brightline-ignore` suppression on the same `file` / `function` / `signature_match` basis as duplicate-signature findings. This is the metric that catches copy-paste families, which the signature metric — keyed on the interface, not the body — cannot.

**Signature metric uses the function's I/O profile.** The duplicate-signature metric SHALL key on the function's interface — its name, the sequence of parameter *types* (parameter names normalized away), AND, where the language exposes it, the return type — rather than the verbatim parameter text, so two declarations with the same interface but different parameter names are recognized as the same signature AND cosmetic naming differences do not split a genuine collision. For languages without static parameter types, the key falls back to name plus parameter arity.

The audit SHALL load a per-workspace `.brightline-ignore` file (if present) AND apply match-suppression to duplicate-signature findings whose constituent sites are all listed in the ignore file. The audit SHALL also validate ignore entries against the current workspace state AND report stale entries via the chatops top-line (informational; the audit does NOT modify the ignore file itself given its `WritePolicy::None`).

The ignore file's YAML schema:

```yaml
ignore:
  - file: <workspace-relative path>
    function: <function or method name>
    signature_match: <substring of the function's signature line>
    reason: <one-line operator-readable explanation>
```

All four fields are required per entry. An entry with a missing field triggers a WARN log AND the entry is skipped.

Match-suppression rule: a duplicate-signature finding is suppressed in full when EVERY constituent site matches an ignore entry. A partial match (some sites match, some don't) emits the finding with only the unmatched sites listed in the body. No match → the finding is emitted in full (today's behavior).

Stale-entry rule: each ignore entry is validated against the current workspace at audit time. Validation fails when (a) the named file doesn't exist, (b) the file doesn't contain a function with the named name, OR (c) the function's signature no longer contains `signature_match`. The audit collects the stale entries AND adds a trailing clause to the chatops top-line:

```
📐 architecture_brightline on <repo>: <N> file(s) over line threshold; <P> function(s) over line threshold; <M> duplicate signature(s); <Q> duplicate body group(s); <K> stale ignore entries to clean up
```

The threaded body lists each stale entry's `file + function + reason` so the operator knows what to remove. The audit does NOT modify `.brightline-ignore` on disk (given `WritePolicy::None`); cleanup is operator-driven.

#### Scenario: Reports files exceeding the size threshold with graduated severity
- **WHEN** the audit runs AND a tracked file under the repository's source root has more lines than the file-line threshold (default `800`)
- **THEN** a finding is included with `subject = "file <path> is <N> lines (threshold: <T>)"` AND `anchor = Some("<path>:1")`
- **AND** its severity is `low` when `N < 1.5 × T`, `medium` when `1.5 × T ≤ N < 2.5 × T`, AND `high` when `N ≥ 2.5 × T`
- **AND** where the audit can identify test-only regions in the file, the finding body reports the production-line / test-line split alongside the total

#### Scenario: Reports functions exceeding the function-line threshold
- **WHEN** the audit runs AND a function defined outside any test-only region spans more lines than the function-line threshold (default `200`)
- **THEN** a finding is included with `subject = "function <name> in <path> is <N> lines (threshold: <T>)"` AND `anchor = Some("<path>:<start-line>")`
- **AND** its severity follows the same graduated scale (`low` / `medium` / `high` at `1× / 1.5× / 2.5×` the threshold)
- **AND** a function defined inside a `#[cfg(test)]` module is NOT measured

#### Scenario: Reports identical function signatures across files
- **WHEN** the audit detects two or more functions in different files (excluding `mod tests {}` blocks) whose I/O-profile signatures match — same name, same parameter-type sequence (parameter names normalized away), AND same return type where the language exposes it
- **AND** no ignore entry suppresses the finding (see the ignore scenarios below)
- **THEN** a finding of severity `low` lists each occurrence

#### Scenario: Reports near-identical function bodies across files
- **WHEN** two or more functions in different files (excluding `mod tests {}` blocks) have identical normalized bodies, differing only in their names AND in renamed local identifiers or string literals
- **AND** no ignore entry suppresses the finding
- **THEN** a finding of severity `low` lists each occurrence
- **AND** the duplicate-body group is counted in the `<Q> duplicate body group(s)` clause of the chatops top-line

#### Scenario: Reports dead public items
- **WHEN** the audit (or a static-analysis subprocess it invokes) identifies public items with zero references in the repository
- **THEN** a finding of severity `low` lists the items

#### Scenario: No findings produces silent outcome
- **WHEN** no metric exceeds its threshold AND no ignore entries are stale
- **THEN** the audit returns `AuditOutcome::Reported(vec![])`
- **AND** unless `notify_on_clean: true` is set, no chatops message is posted (per the framework-level scenario)

#### Scenario: Ignore entry suppresses a fully-matching finding
- **WHEN** a duplicate-signature finding involves 3 sites (file1.ts, file2.ts, file3.ts with function `foo` AND signature substring `async function foo(req`)
- **AND** `.brightline-ignore` contains entries for all 3 sites with matching `file`, `function`, AND `signature_match`
- **THEN** the audit does NOT emit the finding
- **AND** the `<M> duplicate signature(s)` count in the chatops top-line does NOT include this finding

#### Scenario: Partial ignore matches still emit with the unmatched sites
- **WHEN** a duplicate-signature finding involves 3 sites
- **AND** `.brightline-ignore` contains entries for 2 of the 3 sites
- **THEN** the audit emits the finding listing only the 1 unmatched site
- **AND** the chatops body for that finding names the unmatched site AND notes that 2 sites were suppressed by ignore entries

#### Scenario: Stale ignore entries are reported but not removed
- **WHEN** `.brightline-ignore` contains an entry for `examples/site-x/auth.ts:handleAuthCallback`
- **AND** that file has been deleted from the workspace
- **THEN** the audit marks the entry as stale
- **AND** the chatops top-line gains the trailing `; <K> stale ignore entries to clean up` clause
- **AND** the threaded body lists the stale entry with its `file + function + reason`
- **AND** the audit does NOT modify `.brightline-ignore` on disk

#### Scenario: Malformed entries WARN and are skipped
- **WHEN** `.brightline-ignore` contains an entry missing the `reason` field
- **THEN** the audit logs a WARN naming the offending entry AND skips it (treats it as if it didn't exist for the run)
- **AND** other valid entries continue to apply
- **AND** the on-disk file is unchanged

#### Scenario: Missing `.brightline-ignore` behaves identically to today
- **WHEN** the workspace has no `.brightline-ignore` file
- **THEN** the audit loads an empty ignore list
- **AND** no suppression occurs
- **AND** no stale-cleanup clause appears in the chatops output
- **AND** behavior is byte-identical to pre-spec runs

## ADDED Requirements

### Requirement: Consultative audit prioritizes oversized, low-cohesion code
The `architecture_consultative` audit's prompt SHALL direct the agent to treat code size as a priority signal. Among the observations it is allowed to raise (per the `Architecture consultative audit` requirement's 0-5 cap, question framing, AND finding schema — this requirement adds a prioritization directive only; it does NOT change the audit's output transport, severity range, or cap), the agent SHALL rank a file or function that is large relative to the rest of the codebase AND exhibits multiple responsibilities as a first-rank "should this split, and along what seams?" question. The prompt SHALL direct the agent to reason about cohesion rather than raw line count — a large file that is genuinely one cohesive responsibility is left unflagged, while a smaller file that mixes unrelated responsibilities may be raised — AND to flag families of near-identical functions (the same control-flow skeleton under different names) that an identical-signature comparison cannot detect.

#### Scenario: An oversized, multi-responsibility file is raised as a split question
- **WHEN** the consultative audit runs against a codebase containing a file that is large relative to its peers AND spans several unrelated responsibilities
- **THEN** the prompt directs the agent to raise that file as a "should this split, and along what seams?" question, anchored to the file per the consultative audit's anchoring rule
- **AND** the question is ranked ahead of lower-priority observations within the 0-5 cap

#### Scenario: A large but cohesive file is not flagged for splitting
- **WHEN** the consultative audit encounters a file that exceeds typical size but implements a single cohesive responsibility
- **THEN** the prompt directs the agent NOT to raise a split question on size alone
- **AND** size without a cohesion problem does not consume one of the 0-5 finding slots

#### Scenario: Near-identical function families are flagged
- **WHEN** the codebase contains several functions sharing one control-flow skeleton under different names, which an identical-signature comparison does not match
- **THEN** the prompt directs the agent to raise the family as a consolidation observation anchored to the constituent sites
