# Tasks

## 1. Sub-second precision in the `since` query parameter

- [x] 1.1 In `autocoder/src/github.rs::list_issue_comments_since`, change the `since_str` formatter from `chrono::SecondsFormat::Secs` to `chrono::SecondsFormat::Millis`.
- [x] 1.2 Unit-test: `list_issue_comments_since` issued against a mocked GitHub records the query string AND the recorded `since` value contains millisecond precision (e.g. `2026-05-29T17:18:11.847Z`). The existing `list_issue_comments_since_parses_response` test (`github.rs:1958` area) can be extended OR a new sibling test added.
- [x] 1.3 Unit-test: a marker constructed from a `chrono::DateTime<Utc>` with zero milliseconds (e.g. `2026-05-29T17:18:11.000Z`) still produces a millisecond-precision query string (`...11.000Z`), NOT the second-precision `...11Z`. Verifies the formatter doesn't strip trailing zeros.

## 2. Client-side strict-since filter in the dispatcher loop

- [x] 2.1 In `autocoder/src/revisions.rs::process_one_pr`, insert a strict-inequality filter at the top of the per-comment loop, BEFORE the existing bot-author filter at line 521. Body:
  ```rust
  if comment.created_at <= state.last_seen_comment_at {
      advance_seen(&mut latest_seen, comment.created_at);
      continue;
  }
  ```
  Rationale for `advance_seen` call: keeps the in-loop tracking consistent with comments that ARE processed; harmless because the local `latest_seen` is only used to update `state.last_seen_comment_at` at the end, AND the value is already at OR before the state's marker so the post-loop assignment is a no-op when `latest_seen <= state.last_seen_comment_at`.
- [x] 2.2 Unit-test: a dispatcher fixture is invoked with a `RevisionState` whose `last_seen_comment_at` exactly matches a comment's `created_at` in the mocked-fetch response. The stub executor's `run_revision` call count is `0`.
- [x] 2.3 Unit-test: a dispatcher fixture is invoked with a `RevisionState` whose `last_seen_comment_at` is later than a comment's `created_at`. The stub executor's `run_revision` call count is `0`.
- [x] 2.4 Unit-test: a dispatcher fixture is invoked with a `RevisionState` whose `last_seen_comment_at` is earlier than a comment's `created_at`. The stub executor's `run_revision` call count is `1`. (Happy-path regression.)

## 3. End-to-end regression test

- [x] 3.1 Add an integration test in `revisions.rs`'s test module that drives a two-iteration scenario:
  - Iteration 1: `RevisionState { last_seen_comment_at: T0, revisions_applied: 0 }`. Mocked GitHub returns one operator comment at `created_at: T1 > T0`. Stub executor returns `Failed { reason: "timeout" }`. After processing, state is `RevisionState { last_seen_comment_at: T1, revisions_applied: 1 }`.
  - Iteration 2 (simulating the next polling cycle): the SAME `RevisionState` is loaded. Mocked GitHub returns the SAME comment at `created_at: T1` (simulating the truncation-induced re-fetch). Stub executor's `run_revision` call count is asserted to be `1` (from iteration 1 only — the strict-since filter skips the duplicate).
  - Final state: `RevisionState { last_seen_comment_at: T1, revisions_applied: 1 }` (counter NOT incremented twice).
- [x] 3.2 Add a regression test for the AskUser path: an AskUser outcome in iteration 1 leaves the marker at `T0` (per the existing canonical behavior). Iteration 2 receives the same comment at `T1 > T0` AND IS processed (`run_revision` call count for iteration 2 is `1`). The strict-since filter does NOT regress AskUser semantics.

## 4. Validation

- [x] 4.1 `cargo test` passes.
- [x] 4.2 `cargo clippy` produces no NEW warnings against the existing baseline.
- [x] 4.3 `openspec validate a2705-revise-dispatcher-strict-since-filter --strict` passes.
