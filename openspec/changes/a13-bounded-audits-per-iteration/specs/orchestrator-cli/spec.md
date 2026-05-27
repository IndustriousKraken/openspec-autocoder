## ADDED Requirements

### Requirement: Audit framework bounds audits per iteration to prevent storm patterns
The audit framework's per-iteration scheduler SHALL run at most `audits.max_audits_per_iteration` eligible audits before returning control to the iteration loop. Subsequent eligible audits SHALL defer to the next iteration. The bound applies to BOTH cadence-driven AND on-demand queued runs — every run increments the counter regardless of trigger source. The default value is `1`, intentionally low to ensure pending changes (per `a12`) AND audit work share each iteration's wall-clock fairly. Operators wanting faster audit drainage during onboarding or after major refactors can raise the bound; values above the number of registered audits clamp at the registry count with a WARN.

Audits are tried in the registry's declaration order. On-demand queued audits drain FIRST within the loop (preserving the existing "queued bypasses cadence" semantics), then cadence-driven audits in order. Either source contributes to the per-iteration count.

#### Scenario: Default bound runs one audit per iteration
- **WHEN** `audits.max_audits_per_iteration` is unset (default `1`) AND 3 audits are eligible at the start of an iteration
- **THEN** the scheduler runs the first eligible audit in declaration order
- **AND** the scheduler returns control to the iteration loop after that audit completes
- **AND** the other 2 eligible audits do NOT run this iteration
- **AND** the unrun audits' `.audit-state.json` entries are unchanged — they remain eligible for the next iteration

#### Scenario: Raised bound runs multiple audits per iteration
- **WHEN** `audits.max_audits_per_iteration: 3` AND 5 audits are eligible
- **THEN** the scheduler runs the first 3 eligible audits in declaration order
- **AND** the other 2 defer to the next iteration

#### Scenario: Bound 0 skips all audits
- **WHEN** `audits.max_audits_per_iteration: 0`
- **THEN** the scheduler runs zero audits regardless of how many are eligible
- **AND** the iteration proceeds to push+PR (or no-op if no other commits exist)
- **AND** this behavior is useful for diagnostics OR for temporarily silencing the audit framework

#### Scenario: On-demand queued audits count against the bound
- **WHEN** `audits.max_audits_per_iteration: 1` AND the on-demand queue contains 2 audits AND 1 cadence-driven audit is eligible
- **THEN** the scheduler runs the FIRST queued audit (queued drain has priority)
- **AND** the counter increments to 1, hitting the bound
- **AND** the second queued audit AND the cadence-eligible audit do NOT run this iteration
- **AND** both unrun audits' state is preserved (queue retains the deferred entry; cadence audit's `.audit-state.json` is unchanged)

#### Scenario: Out-of-bounds bound is clamped at the registry count
- **WHEN** `audits.max_audits_per_iteration: 50` AND the registry contains 5 audits
- **THEN** the resolved value is 5
- **AND** a WARN log at startup names both the requested AND clamped values

#### Scenario: Bound interacts cleanly with change-precedence ordering
- **WHEN** an iteration begins AND 2 pending changes are in the queue AND 5 audits are eligible AND bound is default `1`
- **THEN** per `a12`'s change-precedence rule, the 2 pending changes process first
- **AND** the audit phase runs at most 1 audit
- **AND** the iteration's push+PR step ships commits from both phases
- **AND** the next iteration processes any remaining pending changes (likely none, but if any) AND runs 1 more audit, and so on
