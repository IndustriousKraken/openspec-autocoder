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

## Control-plane gatekeepers fail closed

A **control-plane gatekeeper** is any component whose job is to decide whether
work may proceed, or to attest that work meets a standard: the pre-flight
contradiction gates (`[in]`, `[canon]`), the code-implements-spec gate
(`[out]`), the code reviewer, any future verifier, and the audits that gate an
operator's `send it`. The invariant for every one of them:

> **An inability to run is a distinct, surfaced, non-passing state — never a
> pass.** A control that fails *open* (treats "I could not run" as "everything
> is fine") is not a control; it silently removes the rail while reporting
> green.

This is a canonical requirement (`project-documentation` → *Control-plane
gatekeepers fail closed, never to a passing verdict*), so the periodic
`drift_audit` and the `[canon]` gate read it and can flag a new gatekeeper that
defaults to pass. Apply it whenever you add or change a gate — these are the
exact traps it has caught before (each shape is "an inability-to-run collapsing
into a passing verdict"):

- **Verdict defaults and initializers are the non-passing state.** A verdict
  variable, accumulator, or struct default initializes to blocked / errored /
  unknown — never to approve / pass. `ContradictionCheckOutcome` has variants
  `Clean` / `Found` / `Errored` with no default-approve; the verdict ledger's
  `GateVerdict` is default-deny — "open" requires an affirmative, completed
  `Pass` (a crash or unhandled path leaves the gate non-passing).
- **Zero-item aggregations are non-passing.** An aggregation over zero
  evaluated items does not yield a pass (the empty-session bug). "I reviewed
  nothing, so everything's approved" is the exact failure this forbids.
- **Error paths do not collapse into pass.** A spawn or timeout failure, an
  unavailable / unregistered CLI, a missing or unparseable result, a
  schema-rejected submission the agent never corrects, or "no result recorded"
  is treated as **errored** — never as "no findings" / "approved" / "verified".
- **The errored state is operator-visible.** Surface it via chatops and/or the
  artifact the gatekeeper writes, naming the gatekeeper and the cause, so "ran
  and passed" is distinguishable from "could not run".

### The action on error follows the gatekeeper's role

| Role | On a can't-run error | Example |
| ---- | -------------------- | ------- |
| **Blocking** | Hold the gated work in an explicit failed-to-run state an operator clears — distinct from a "found a problem" verdict. Do NOT let work proceed as if it passed. | `[in]` / `[canon]` record `GateVerdict::FailedToRun`; the executor runs only when every blocking gate is `Pass` or `Disabled`, so the change is held. |
| **Advisory** | Render an explicit "failed to run" result rather than omitting output or reporting success. Never blocks. | `[out]` renders a `## Spec Verification: FAILED TO RUN — <cause>` PR-body section instead of dropping the section. |

The code reviewer is the reference conformant case: a session with no valid
submission returns `Discarded { reason }` — it never defaults to `Approve`.

### Transient tolerance is bounded retry, then errored

Where a gate retries a transient blip (e.g. a flaky no-submission session) to
avoid wedging on a one-off, it retries a **bounded** number of times
(`executor.verifier_gate_retries`, default `2`) and then enters the errored
state. Retrying forever, or falling through to pass once the bound is
exhausted, both violate the invariant.
