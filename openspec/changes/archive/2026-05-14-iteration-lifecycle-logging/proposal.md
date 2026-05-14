## Why

Operator visibility gap: when autocoder is running well, journalctl shows almost nothing — no indication that a change is being worked on, no per-change outcome. The only signal is "polling pass produced no changes" which only fires on the empty-queue path. A change that takes 5+ minutes is invisible mid-iteration.

## What Changes

- **ADDED capability:** `orchestrator-cli` SHALL emit INFO-level lifecycle logs around each change iteration and each polling pass. Operators get a tail-able journal narrative without raising log level to DEBUG.
- **Code:**
  - In `polling_loop::walk_queue` (and `process_one_waiting` for the resume path): before invoking the executor, log `INFO starting work on change <name>` with the `url` field. After `handle_outcome` returns, log `INFO change <name> -> <outcome>` with `url` and the outcome variant.
  - In `run_pass_through_commits`: log `INFO polling pass starting` with `url`, `pending=<count>`, `waiting=<count>` at the top. Replace the existing "polling pass produced no changes" with a uniform `INFO polling pass complete` line that always fires, with `url`, `committed=<count>`, `waiting=<count>`.
- **Tests:** no new behavioral tests. Log assertions would require adding `tracing-test` for negligible value; the runtime impact is already exercised by every existing iteration test.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement)
- Affected code: `autocoder/src/polling_loop.rs`
- No config changes, no behavioral changes — purely additional INFO lines.
- Log volume increase: ~4 lines per polling pass + 2 lines per change processed. At default poll interval (300s), that's ~50 extra lines/hour for a single-repo deployment with one or two changes per pass.
