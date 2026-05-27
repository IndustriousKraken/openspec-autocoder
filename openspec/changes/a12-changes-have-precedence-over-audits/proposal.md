## Why

The current iteration sequence runs audits BEFORE `queue::list_pending`. The design intent was that a spec-writing audit (missing_tests, security_bug) could add new changes to the queue AND have them implemented in the same iteration's queue walk. In practice, this design causes an "audit storm" pattern that monopolizes the daemon for hours and blocks pending changes from being processed.

Real incident: after a PR merge updates the base branch HEAD, every `requires_head_change=true` audit (`architecture_brightline`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`, `architecture_consultative`) becomes eligible simultaneously. The audit phase of the next iteration runs one of them (taking ~20-30 minutes per LLM audit). The change phase never runs because the iteration ends. The next iteration runs the next audit. And so on. A repo with 5 eligible audits and 5-min polling spends ~2-2.5 hours running audits sequentially before ANY pending change touches the implementer.

For operators with pending changes ready, this is unacceptable. A change waiting 2+ hours behind audits looks (and feels) like the daemon is broken. The operator-visible symptom: `@<bot> status` shows `idle` (or shows the audit if `a11`'s status enhancement has shipped), the queue has pending changes, and they don't move for hours.

The fix is to reverse the ordering: changes get processed first, audits run only on remaining time. The trade-off — audit-generated changes wait one iteration for implementation instead of riding the same iteration's PR — is acceptable and arguably better for review (audit creation vs. audit-implementation become separable PRs).

## What Changes

**Iteration sequence becomes**: `recreate_branch → waiting → pending → audits → push/PR`. Was: `recreate_branch → audits → waiting → pending → push/PR`.

**Audit-generated changes wait one iteration.** When an audit's `SpecsWritten(names)` outcome creates `openspec/changes/<slug>/` directories AND commits them, those changes become pending. The CURRENT iteration's queue walk has already completed (it ran before audits), so the new pending changes wait for the NEXT iteration's `list_pending` to pick them up. The audit's creation commit ships in this iteration's PR; the implementation commits ship in next iteration's PR. **Net effect: each audit's "create proposals" step becomes a separable PR from its "implement proposals" step.** Reviewers see the spec proposals in one PR (and can `revise` them via the existing PR-comment loop before implementation) and the implementations in a follow-up PR.

**Push + PR step is unchanged.** Whichever phase produced commits (audit creation OR change implementation OR both) — if commits exist at end of iteration, push and open a PR.

**Audit phase respects iteration wall-clock.** With this reordering, an iteration's audit phase only fires if `recreate_branch → waiting → pending → push/PR` left time before the next polling tick. In practice with the bounded-audits-per-iteration cap from `a13`, this means at most one audit per iteration regardless of how many are eligible, and the audit only runs after the pending queue has had its chance.

**The `requires_head_change` gate is unchanged.** Audits skipping when `last_run_sha == HEAD` still works. The reorder doesn't change which audits are eligible; only when in the iteration they run.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one MODIFIED requirement: the existing `Periodic audit framework`'s scenario `Framework runs registered audits at startup-defined cadence` changes the ordering claim from "after `recreate_branch` AND BEFORE `list_pending`" to "after `list_pending` AND the pending queue walk AND BEFORE push+PR". A separate scenario clarifies that spec-writing audits' generated changes wait one iteration for implementation.
  - `project-documentation` — one ADDED requirement: `OPERATIONS.md describes the new iteration ordering and the audit-to-implementation one-iteration delay`.
- **Affected code:**
  - `autocoder/src/polling_loop.rs` (or wherever the iteration sequence is composed) — move the audit-scheduler invocation from its current position (between `recreate_branch` and `list_pending`) to AFTER the pending queue walk completes, BEFORE the push+PR step.
  - The audit framework's `SpecsWritten` handling continues to commit the new change directories AS the audit completes. The CHANGE in this spec is just where in the iteration sequence the audit runs — its internal behavior is unchanged.
- **Operator-visible behavior:**
  - The post-PR-merge audit-storm pattern stops monopolizing the daemon. The first iteration after a HEAD change processes any pending changes, then runs at most one audit (per `a13`'s bound), then pushes a PR. The next iteration processes more pending changes + the next eligible audit.
  - Audit-generated changes ship in separable PRs. Reviewers see "audit created 5 missing-tests proposals" in one PR (just the proposal directories) and "implementer ran on 3 of those proposals" in a follow-up PR. Currently these are bundled into one PR with the proposals + their implementations.
  - The PR commit ordering changes: the order of `pending change implementation commits` + `audit creation commits` flips. Previously audits came first; now changes come first. Reviewers reading the PR's commit list see the change work AT THE TOP.
- **Breaking:** technically yes — operators relying on the audit-and-implementation-in-one-PR shape see audit-generated work spread across two PRs. The dependent specs (audit framework, queue walk) preserve their per-component semantics; only the iteration's macro-ordering changes.
- **Acceptance:** `cargo test` passes; `openspec validate a09-changes-have-precedence-over-audits --strict` passes. New unit test exercises an iteration with both pending changes AND eligible audits — pending changes run first, audit fires after, both phases' commits ship in the same PR.
