# Implementation tasks

## 1. Yield-on-pending-request in the queue walk

- [ ] 1.1 In `walk_queue` (`autocoder/src/polling_loop.rs:2210`), after each change reaches its outcome (the `for change in pending` loop body), check whether any operator chatops request is pending for this repo: a non-empty `pending_triages`, `pending_proposal_requests`, OR `pending_changelog_requests`. Thread whatever handles are needed into `walk_queue` (or read them via the same shared `Arc<Mutex<…>>` the iteration-top drains use) — peek WITHOUT draining (the iteration-top drain is what actually consumes them next iteration).
- [ ] 1.2 When any is pending, break out of the walk loop (stop starting new changes) and return the changes archived so far, so the caller opens the accumulated PR exactly as it does at the natural end of a batch.
- [ ] 1.3 Do NOT interrupt the change currently executing — the check happens between changes, after the current change's outcome is recorded. Preserve the existing "any non-Archive outcome halts the walk" behavior (this adds an additional, request-driven halt condition).

## 2. Tests

- [ ] 2.1 With a `pending` list of ≥2 changes AND a queued `changelog` request, the walk processes exactly one change, then yields (returns) — assert it did NOT process the remaining changes, and that the changelog request is still pending (to be drained next iteration).
- [ ] 2.2 Same for a queued `propose` request and a queued `send it` request (the bound applies to all three operator-request queues).
- [ ] 2.3 With a `pending` list of ≥2 changes AND no operator request pending, the walk processes the full batch (existing behavior unchanged).
- [ ] 2.4 The currently-executing change is always allowed to finish: a request that becomes pending during change N does not abort change N; the yield happens only after N's outcome is recorded.

## 3. Acceptance gate

- [ ] 3.1 `cargo test` passes for the autocoder crate.
- [ ] 3.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] 3.3 `openspec validate a71-operator-requests-not-starved --strict` passes.
