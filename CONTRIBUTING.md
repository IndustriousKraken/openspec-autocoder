# Contributing

## Source files and functions stay within a size budget

Source-file and function length are a **maintainability budget**, not just a
metric an audit happens to report. Keep them in mind as you write:

- A source file **should** stay at or under roughly **500 lines**.
- A function **should** stay at or under roughly **50 lines**.

These are **judgment targets, not hard caps**. Genuinely cohesive,
single-responsibility code *may* exceed them when splitting would only add
indirection without reducing complexity — the test is **cohesion, not the
line count**. A large file that does exactly one thing is fine; a smaller
file that mixes three unrelated concerns is not.

### When it becomes a defect

Past the **brightline thresholds** — file `800` lines, function `200` lines,
both operator-configurable — a file or function is treated as a **structural
defect to address**, and the concern **escalates the further over it goes**.
The `architecture-brightline` audit grades the overage:

| Size vs. threshold | Severity |
| ------------------ | -------- |
| `1×` – `<1.5×`     | low      |
| `1.5×` – `<2.5×`   | medium   |
| `≥ 2.5×`           | high     |

**Duplicated logic is likewise a defect.** Near-identical function bodies, or
the same signature repeated across files, are flagged — because in an
LLM-grown codebase the bloat is *reachable* (it passes a dead-code linter) and
only a size/duplication signal catches it. Prefer one parameterized helper
over a family of copy-paste clones.

### Enforcement is advisory, never a gate

Size and duplication are surfaced in three places, none of which block:

- **`architecture-brightline` audit** — grades file/function length and
  duplication mechanically (graduated severity above).
- **`architecture-consultative` audit** — reasons about cohesion (not raw
  lines) and raises the worst over-sized, least-cohesive code as a "should
  this split, and along what seams?" question.
- **Code review** — adds an advisory note when a pass enlarges an
  over-budget file or function (it does not penalize a pass that shrinks
  one).

A size or duplication finding **never, on its own, blocks a pull request or a
change from archiving**. It is a maintainability signal that informs
prioritization, not a correctness gate. When you legitimately exceed a
threshold for a cohesive reason, record an intentional duplicate in
`.brightline-ignore` (for duplicate signatures/bodies) or simply leave the
cohesive file as-is — the audits surface it, the operator decides.
