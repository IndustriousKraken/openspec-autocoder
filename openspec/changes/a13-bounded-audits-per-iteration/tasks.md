## 1. Config schema

- [ ] 1.1 In `autocoder/src/config.rs`, extend `AuditsConfig`:
  ```rust
  #[serde(default = "default_max_audits_per_iteration")]
  pub max_audits_per_iteration: usize,
  ```
- [ ] 1.2 Add `fn default_max_audits_per_iteration() -> usize { 1 }`.
- [ ] 1.3 Clamp at startup: values > `<count of registered audits>` (currently 5: brightline, consultative, drift, missing_tests, security_bug) clamp to that count with a WARN. Value 0 is permitted.
- [ ] 1.4 Add the field to `config.example.yaml` under the `audits:` block, commented with explanation.
- [ ] 1.5 Update the `project-documentation` config-example-coverage test list.
- [ ] 1.6 Startup log: name the resolved value as part of the existing audit-config log line ("audits configured: <list>; max_per_iteration=<N>").
- [ ] 1.7 Tests: default parses; explicit values pass through; out-of-bounds clamps with WARN; value 0 disables audits behaviorally.

## 2. Scheduler bound

- [ ] 2.1 In `autocoder/src/audits/scheduler.rs` (or wherever the per-iteration audit loop lives), add a counter:
  ```rust
  let mut audits_run_this_iteration = 0;
  let bound = config.audits.max_audits_per_iteration;
  ```
- [ ] 2.2 The scheduler iterates the audit registry in declaration order. For each audit:
  - If `audits_run_this_iteration >= bound`: stop the loop, return control to the iteration.
  - If the audit is eligible (cadence elapsed + head-change satisfied + not already drained from on-demand queue): run it, increment counter.
  - Otherwise: skip.
- [ ] 2.3 On-demand queued audits (from chatops `@<bot> audit <name>`) drain FIRST in the loop, but each drained audit also counts against the bound. If 3 on-demand audits are queued AND bound is 1, the first runs this iteration; the others remain queued for next iteration.
- [ ] 2.4 Tests:
  - Default bound (1) + 3 eligible cadence-driven audits → 1 runs, 2 defer.
  - Bound = 2 + 3 eligible audits → 2 run, 1 defers.
  - Bound = 5 + 3 eligible audits → all 3 run.
  - Bound = 0 + any number eligible → no audits run.
  - On-demand queue has 2 audits + 1 cadence-eligible + bound=2 → both on-demand run (drained first), cadence one defers.

## 3. Docs

- [ ] 3.1 In `docs/OPERATIONS.md`'s `## Periodic audits` section, add a paragraph describing the bound:
  - The default (`1` audit per iteration) AND the rationale (prevent storm pattern; let pending changes get attention every iteration per `a12`).
  - The override (`audits.max_audits_per_iteration: N`).
  - The interaction with on-demand queue (queued audits count against the bound; queued first, cadence after).
- [ ] 3.2 In `docs/CONFIG.md`'s `audits:` table, add a row for `max_audits_per_iteration` (type `usize`, default `1`, max `<count of registered audits>`).

## 4. Spec deltas

- [ ] 4.1 `openspec/changes/a13-bounded-audits-per-iteration/specs/orchestrator-cli/spec.md` ADDs one requirement covering the bound, the declaration-order fairness, the on-demand-counts-too rule.
- [ ] 4.2 `openspec/changes/a13-bounded-audits-per-iteration/specs/project-documentation/spec.md` ADDs one requirement covering the OPERATIONS.md and CONFIG.md updates.

## 5. Verification

- [ ] 5.1 `cargo test` passes (new + existing).
- [ ] 5.2 `openspec validate a10-bounded-audits-per-iteration --strict` passes.
- [ ] 5.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
