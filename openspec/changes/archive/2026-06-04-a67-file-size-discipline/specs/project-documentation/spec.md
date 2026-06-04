# project-documentation — delta for a67-file-size-discipline

## ADDED Requirements

### Requirement: Source files and functions stay within a size budget
The project SHALL treat source-file AND function length as a maintainability budget, not merely a metric an audit happens to report. A source file SHOULD stay at or under a target of roughly **500 lines** AND a function at or under roughly **50 lines**. These are judgment targets, NOT hard caps: genuinely cohesive, single-responsibility code MAY exceed them when splitting would add indirection without reducing complexity — the test is cohesion, not the line count.

Past the brightline thresholds (file `800`, function `200` — both operator-configurable), a file or function is treated as a **structural defect to be addressed**, with the concern escalating as it climbs further over: the architecture-brightline audit grades it `low` / `medium` / `high` at `1×` / `1.5×` / `2.5×` the threshold. Duplicated logic — near-identical function bodies, OR repeated signatures across files — is likewise a structural defect, because in an LLM-grown codebase the bloat is reachable (it passes a dead-code linter) and only a size/duplication signal catches it.

These defects are surfaced **advisorily** — graded by the architecture-brightline audit, prioritized by the architecture-consultative audit (which reasons about cohesion, not raw lines), AND noted by code review when a pass enlarges an over-budget file or function. A size or duplication finding SHALL NOT, on its own, block a pull request or a change from archiving; it is a maintainability signal that informs prioritization, not a correctness gate.

This requirement is the single source of the size budget that the `Architecture-brightline audit`, `Consultative audit prioritizes oversized, low-cohesion code`, AND `Reviewer flags files and functions that breach the size brightline` requirements enforce.

#### Scenario: A file far past the threshold is a high-severity structural defect
- **WHEN** a source file's length reaches or exceeds `2.5 ×` the file-line threshold
- **THEN** the project treats it as a high-severity structural defect to be addressed
- **AND** it is surfaced advisorily — `high` severity from the architecture-brightline audit, prioritized by the architecture-consultative audit, AND noted by code review when a pass enlarges it — without, on size alone, blocking a pull request or a change from archiving

#### Scenario: A cohesive file may exceed the target by judgment
- **WHEN** a file exceeds the ~500-line target but implements a single cohesive responsibility that splitting would only fragment into indirection
- **THEN** exceeding the target is not, by itself, a structural defect
- **AND** the architecture-consultative audit is directed to leave it unflagged (size without a cohesion problem)

#### Scenario: Duplicated logic is a structural defect
- **WHEN** two or more functions share a near-identical body, OR a signature is repeated across files
- **THEN** the duplication is treated as a structural defect surfaced by the architecture-brightline audit (duplicate-body AND duplicate-signature metrics respectively)
