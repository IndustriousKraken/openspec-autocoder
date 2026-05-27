## Why

Even with the change-precedence ordering from `a12`, an iteration that has time for both pending changes AND audits can still get bogged down if many audits are eligible simultaneously. The "PR merge → HEAD changes → all 5 audits eligible at once" pattern causes the audit phase to run multiple audits in sequence, consuming wall-clock that could be returned to the next polling iteration.

The fix is to cap how many audits run per iteration. With one audit per iteration AND 5-minute polling, an audit storm of 5 eligible audits drains in ~25 minutes elapsed wall-clock — distributed across iterations that ALSO process pending changes — instead of one iteration running back-to-back audits for hours.

## What Changes

**New config field `audits.max_audits_per_iteration`** (`usize`, default `1`, max `<count of registered audits>` enforced at startup). The audit framework's per-iteration scheduler iterates the audit registry in declaration order AND runs at most `max_audits_per_iteration` eligible audits before returning control to the iteration. Subsequent eligible audits defer to the next iteration.

**Declaration order is the fairness key.** Audits are tried in the registry's declaration order. The first N eligible (per cadence + `requires_head_change` + queue-of-on-demand-runs) get to run; the rest wait. The declaration order is deterministic across iterations, so a long-running audit at position 1 doesn't permanently block audits at later positions — it just makes them wait until the position-1 audit is done.

**On-demand queued runs count toward the bound.** A chatops `@<bot> audit <name>` queues an audit that bypasses cadence + head-change. Queued audits drain FIRST (per the existing spec), but they count against `max_audits_per_iteration`. If 3 on-demand audits are queued AND the bound is 1, the first one runs this iteration; the other 2 wait for next iteration.

**Default value rationale.** `1` audit per iteration matches the audit-as-low-priority-background-task design intent. An operator who wants faster audit drainage (e.g., during initial onboarding when many audits are eligible from a fresh-clone state) can set `audits.max_audits_per_iteration: 3` and accept the corresponding wall-clock cost per iteration.

**Defer notifications.** When eligible audits defer to the next iteration, no chatops notification fires for them — the deferral is invisible. (A future enhancement could emit a "deferred N eligible audits to next iteration" log line at DEBUG; not in scope.) The startup log line names the resolved bound so operators see the value.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one ADDED requirement: `Audit framework bounds audits per iteration`. References the existing audit-framework requirement; adds the cap behavior.
  - `project-documentation` — one ADDED requirement: `OPERATIONS.md and CONFIG.md document max_audits_per_iteration`.
- **Affected code:**
  - `autocoder/src/config.rs` — extend `AuditsConfig` (or wherever audit config lives):
    ```rust
    #[serde(default = "default_max_audits_per_iteration")]
    pub max_audits_per_iteration: usize,
    ```
    Plus `fn default_max_audits_per_iteration() -> usize { 1 }`.
  - Clamp at startup: values > `<count of registered audits>` clamp to that count with a WARN. Value 0 is permitted (every iteration skips all audits — useful for diagnostics).
  - `autocoder/src/audits/scheduler.rs` (or wherever the per-iteration scheduler lives) — add a counter that increments each time an audit runs in the current iteration; when the counter reaches `max_audits_per_iteration`, the scheduler stops AND returns control to the iteration loop. Both cadence-driven audits AND on-demand queued audits count against the same bound.
  - `docs/OPERATIONS.md` and `docs/CONFIG.md` updates.
- **Operator-visible behavior:**
  - When many audits are eligible at once (typical: after a HEAD change unblocks every `requires_head_change=true` audit), they distribute across iterations instead of running back-to-back in one iteration.
  - Pending changes continue to be processed each iteration (per `a12`'s change-precedence rule), so operators see normal change flow alongside the staggered audit work.
  - The first audit eligible at any moment runs immediately; subsequent eligible audits wait one polling cycle each. With default 5-min polling, 5 eligible audits drain in ~25 minutes elapsed total — much better than the pre-spec pattern where they could consume 2+ hours of one iteration.
- **Breaking:** no for the default behavior change. The default `1` audit per iteration is slower for the audit-only path than today's unbounded behavior, BUT (a) on-demand audits still run on the next iteration, just one at a time; (b) cadence-driven audits at default `weekly` rarely accumulate enough eligibles to matter; (c) operators who want today's unbounded behavior set `max_audits_per_iteration` to a high value.
- **Acceptance:** `cargo test` passes; `openspec validate a10-bounded-audits-per-iteration --strict` passes. New unit tests cover: default bound is 1; iteration with 3 eligible audits runs 1 and defers 2; bound=0 skips all audits; on-demand queued audits count against the bound.
