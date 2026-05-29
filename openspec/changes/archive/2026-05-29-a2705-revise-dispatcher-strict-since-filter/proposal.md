## Why

The revision dispatcher in `autocoder/src/revisions.rs::process_one_pr` re-processes operator revise comments under a reproducible latent condition: ONE operator comment can trigger TWO `execute_revision` calls (and counts) without any operator action, daemon restart, OR client-visible cause. Observed in production on `IndustriousKraken/openspec-autocoder` PR #71:

- Single operator comment at `created_at = 2026-05-29T17:18:11.847Z` (sub-second precision per GitHub's internal storage).
- Single autocoder process running continuously (verified via `journalctl`: one `Started` at 17:08:08, next `Stopped` at 19:29:35).
- Two distinct PR replies posted within minutes of each other:
  1. `✗ Revision attempt failed: commit/push failed: nothing to commit` — first iteration (narrative bail).
  2. `✅ Revision applied: revise: a26-oss-fork-support: ... Revision count: 2 of 5.` — second iteration (success). The counter at the time was 2, meaning two iterations had completed.

Diagnosis: the dispatcher's GitHub fetch helper `github::list_issue_comments_since` formats the `since` query parameter using `chrono::SecondsFormat::Secs`:

```rust
let since_str = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
// e.g. "2026-05-29T17:18:11Z" — sub-second precision truncated
```

GitHub's `since` filter on the issue-comments endpoint compares against the comment's internal `updated_at` (which equals `created_at` for an unedited comment). GitHub stores `updated_at` at sub-second precision. The comparison `updated_at > since` becomes `17:18:11.847 > 17:18:11.000 = TRUE` — the comment is returned by the API on every subsequent polling iteration AFTER the marker was supposedly advanced past it.

The dispatcher's outer loop has no client-side strict-inequality check: it processes every comment returned by the API, only filtering out bot-authored comments (which the operator's revise comment is not) AND the reviewer-revision HTML marker (which doesn't apply here). So the re-fetched comment is parsed as a fresh trigger AND `execute_revision` runs again.

This explains:

- Why "Revision count" routinely shows numbers higher than the operator expects.
- Why "phantom" duplicate iterations appear on PRs with no second operator comment.
- Why the pattern is *more visible* on narrative-bail runs (each iteration produces a distinct PR reply) AND less visible on successful runs (the second iteration sees the work already done in the diff, emits Completed with a clean tree, AND posts an `nothing to commit` failure that operators may not associate with the success comment minutes earlier).

The cap of 5 per PR has been silently masking this — operators see iterations 2, 3, 4 of 5 instead of 1, 2, 3 of 5, AND the cap-decline fires earlier than they expect. The wall-clock cost per PR is up to 2x the intended cost.

## What Changes

**Two-layer fix.** Either layer alone would close the hole; both together provide defense in depth AND reduce wasted GitHub API roundtrips.

**Layer 1: sub-second precision in the `since` query parameter.** `github::list_issue_comments_since` SHALL format the `since` parameter using millisecond precision (or higher) so the truncation-induced inclusivity disappears at the source. Change `SecondsFormat::Secs` to `SecondsFormat::Millis` (OR `SecondsFormat::AutoSi` if a future caller wants nanosecond precision). With millisecond precision, the marker `17:18:11.847Z` round-trips cleanly to GitHub AND the comment's own `updated_at` no longer exceeds the marker.

**Layer 2: client-side strict-since filter in the dispatcher.** The revision dispatcher SHALL apply `comment.created_at > state.last_seen_comment_at` as a strict-inequality filter BEFORE any trigger-parsing OR processing. Comments at OR BEFORE the marker SHALL be skipped AND the marker SHALL be advanced no further (the marker is already at or past their timestamp). This protects against:

- GitHub silently changing its `since` filter semantics in a future API revision.
- Future helpers that format the marker differently (regression-proof at the boundary).
- Edge cases where the same comment appears in two `list_issue_comments_since` responses due to GitHub-side replication lag (rare but documented for high-traffic repos).

The filter SHALL use `created_at` (not `updated_at`) to keep edited comments out of the re-trigger path — editing a comment SHALL NOT cause a re-process of the original revise text. This matches today's already-encoded semantic: the dispatcher advances the marker using `comment.created_at`, AND consistency between advancement AND filtering is the goal.

**State-file marker semantics are unchanged.** The marker continues to track the latest processed comment's `created_at`. The post-loop write at the end of `process_one_pr` continues to update `state.last_seen_comment_at` from the local `latest_seen`. No state-file migration is needed.

**No regression on the AskUser path.** The existing canonical scenario `AskUser during revision escalates without committing` requires that the marker NOT be advanced past an AskUser-triggering comment, so the resumed iteration can re-process the same comment. With the strict-since filter applied client-side, an AskUser comment whose `created_at` equals the unchanged marker is NOT filtered out (the marker was not advanced) AND IS re-processed on resume. This matches today's intended behavior; the strict-since filter does not affect AskUser semantics.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — ADDED requirement for strict-since filtering on the revision dispatcher's comment-fetch path. Existing requirements (`Failed revision posts a failure comment`, `Revision cap per PR`, `Revisions block per-repo queue`, `Per-PR state file persists revision count and last-seen timestamp`, `AskUser during revision escalates without committing`) are unchanged in semantics; the new requirement layers above them to ensure no comment is processed twice.
- **Affected code:**
  - `autocoder/src/github.rs::list_issue_comments_since` — change `SecondsFormat::Secs` to `SecondsFormat::Millis` in the `since` query formatting. ~1-line change.
  - `autocoder/src/revisions.rs::process_one_pr` — add `if comment.created_at <= state.last_seen_comment_at { advance_seen(...); continue; }` filter at the top of the per-comment loop (between line 504 `for comment in comments` AND the existing bot-author filter at line 521). ~3 lines.
  - Tests: new unit-test fixtures asserting that a comment at exact marker timestamp is skipped, AND a fresh polling cycle that re-fetches the same comment (e.g. via a mocked GitHub returning a duplicate) processes it at most once.
- **Operator-visible behavior:**
  - "Revision count: N of 5" values now reflect the actual number of operator-initiated revise comments processed, NOT inflated by API re-fetches.
  - The revision cap is reached only when the operator has genuinely posted 5 trigger comments. PRs that previously hit the cap "too early" now have their full cap available.
  - No new log lines OR PR comments. The fix is silent on the happy path; the change is the absence of duplicate iterations.
- **Backward compatibility:** state files written by older daemons load cleanly (no schema change). State files written by this version are readable by older daemons (no schema change). The marker field's semantics are unchanged.
- **Dependencies:** none. Independent of the `a27a*` outcome-tools stack AND of `a31` revise-lifecycle notifications. Can land before, after, OR alongside any of them.
- **Acceptance:** `cargo test` passes; `openspec validate a2705-revise-dispatcher-strict-since-filter --strict` passes. Tests:
  - `list_issue_comments_since` query string contains `2026-05-29T17:18:11.847Z` (millisecond precision) when given a sub-second-precision marker.
  - Dispatcher loop receives a mocked-comments response containing a comment with `created_at == state.last_seen_comment_at`: the comment is skipped without `execute_revision` being called.
  - Dispatcher loop receives a comment with `created_at < state.last_seen_comment_at`: same — skipped without invocation.
  - Dispatcher loop receives a comment with `created_at > state.last_seen_comment_at`: processed (the happy path is preserved).
  - End-to-end regression: a synthetic scenario where the dispatcher runs two iterations on the same `RevisionState` AND the same GitHub mock returns the same comment twice (simulating GitHub's truncation-induced double-fetch). The second iteration's `execute_revision` is NOT called.
  - The AskUser-marker-preservation path is regression-tested: an AskUser outcome leaves the marker at its prior value, AND a subsequent fetch returning the same comment IS processed (the marker was not advanced past it).
