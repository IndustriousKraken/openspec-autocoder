---
changelog: skip
---

## Why

Three audit modules each have a `run_subprocess` helper with the same
shape: a `tokio::select!` between `child.wait()` and `tokio::time::sleep`,
where a fired sleep produces `SubprocessOutcome { timed_out: true, .. }`
and the audit's `run` returns `Err("<audit_type>: CLI exceeded the {N}s
timeout")`.

The timeout branch is **untested** in every audit:

- `autocoder/src/audits/drift.rs:188-197` — `outcome.timed_out` →
  `Err("drift_audit: CLI exceeded the {}s timeout")`.
- `autocoder/src/audits/architecture_consultative.rs:185-193` —
  `Err("architecture_consultative: CLI exceeded the {}s timeout")`.
- `autocoder/src/audits/specs_writing.rs:133-142` — `Err("{audit_type}:
  CLI exceeded the {}s timeout")`, reached transitively through
  `MissingTestsAudit` and `SecurityBugAudit`.

Both the non-zero-exit and malformed-stdout branches already have tests
(e.g. `run_returns_err_on_nonzero_exit`, `run_returns_err_on_malformed_stdout`
in `drift.rs`). The timeout branch — which involves the more complex
`tokio::select!` + start_kill + wait-after-kill sequencing — has never
been exercised by CI. If a regression broke the kill-on-timeout path
(e.g. forgetting `start_kill`, or swapping the select-bias and dropping
the timeout case), the existing tests would still pass.

## What Changes

Add per-audit timeout tests that:

- Configure the audit with a short `executor_timeout_secs` (e.g. 1).
- Use a fake CLI script (per the existing `write_script` test helper)
  that runs `sleep 10` so the wall-clock budget is guaranteed to fire
  before the child exits.
- Invoke `audit.run(&mut ctx).await` and assert the result is an `Err`
  whose `format!("{err:#}")` contains both the audit-type label and the
  substring `timeout`.
- Verify (via the `AuditLogWriter` path) that the audit log captured
  the `kind: Err\nreason: timeout` section that the production code
  writes before returning.

Because `specs_writing::run_specs_writing_audit` is the shared driver
behind `missing_tests` and `security_bug`, one timeout test exercising
it via `MissingTestsAudit` is sufficient to lock in the timeout branch
for both audits.

No production code changes.

## Impact

- Affected code:
  - `autocoder/src/audits/drift.rs` (`#[cfg(test)] mod tests`).
  - `autocoder/src/audits/architecture_consultative.rs`
    (`#[cfg(test)] mod tests`).
  - `autocoder/src/audits/missing_tests.rs` (`#[cfg(test)] mod tests`)
    — covers `specs_writing.rs` by delegation.
- Test runtime: each new test waits ~1s of wall clock for its timeout,
  so the suite gains ≤3s total. Acceptable.
- No spec changes (no capability currently spells out the audit
  timeout contract).
- Breaking: no.
