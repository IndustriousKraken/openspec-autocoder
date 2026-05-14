## 1. Polling pass start/end

- [x] 1.1 In `polling_loop::run_pass_through_commits`, after the dirty-check passes and before `process_waiting_changes` is called, emit `tracing::info!(url = %repo.url, pending = pending.len(), waiting = waiting.len(), "polling pass starting")`. Compute `pending` via `queue::list_pending(workspace)?` and `waiting` via `queue::list_waiting(workspace)?` (the same calls used downstream — read-cheap, no API hit).
- [x] 1.2 At the end of `run_pass_through_commits`, replace the existing `tracing::info!(url = repo.url.as_str(), "polling pass produced no changes")` with `tracing::info!(url = %repo.url, committed = processed.len(), waiting = queue::list_waiting(workspace)?.len(), "polling pass complete")`. Emit unconditionally — the existing "if processed.is_empty()" guard goes away.

## 2. Per-change iteration logging

- [x] 2.1 In `polling_loop::walk_queue`, after `queue::lock` succeeds and before `executor.run(...)`, emit `tracing::info!(url = %repo.url, change = %change, "starting work on change")`.
- [x] 2.2 After `handle_outcome` returns, map the QueueStep to an `outcome` string (`archived` | `failed` | `escalated` | `ask_user_exit_early`) and emit `tracing::info!(url = %repo.url, change = %change, outcome = %outcome_str, "change finished")`. The existing per-outcome match (which logs error/break) is preserved; the new info log is in addition.
- [x] 2.3 In `polling_loop::process_one_waiting` (resume path): before `executor.resume(...)` is called, emit `tracing::info!(url = %repo.url, change = %change, "starting work on change (resume)")`. After the match on outcome, emit `tracing::info!(url = %repo.url, change = %change, outcome = %outcome_str, "change finished (resume)")` with `outcome_str` reflecting the resume disposition (`archived` if Some(name), otherwise `unchanged`).

## 3. Verification

- [x] 3.1 `cargo test` passes — no behavioral changes mean every existing test should still pass with no modification.
- [x] 3.2 `openspec validate iteration-lifecycle-logging --strict` passes.
