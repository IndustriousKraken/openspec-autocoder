## ADDED Requirements

### Requirement: Revision dispatcher applies strict-since semantics to GitHub comment fetches

The PR-comment revision dispatcher (`autocoder/src/revisions.rs::process_one_pr`) SHALL guarantee that no operator-triggering comment is processed more than once across polling iterations, even when GitHub's `since` filter on the `/issues/<num>/comments` endpoint returns the same comment multiple times due to timestamp-precision boundary effects.

The guarantee is implemented in two complementary layers:

**Layer 1: sub-second precision in the GitHub query.** The dispatcher's GitHub fetch helper (`github::list_issue_comments_since`) SHALL format the `since` query parameter using millisecond OR finer precision. Second-precision truncation (e.g. `"2026-05-29T17:18:11Z"`) is FORBIDDEN because GitHub's internal `updated_at` storage uses sub-second precision AND its `since` filter compares against the full-precision value: a marker truncated to seconds AND a comment whose actual `updated_at` falls within the same second produces `updated_at > since` = TRUE, causing the comment to be returned on every subsequent fetch.

**Layer 2: client-side strict-since filter in the dispatcher loop.** Independent of how `since` is formatted in the GitHub query, the dispatcher SHALL apply a client-side `comment.created_at > state.last_seen_comment_at` strict-inequality check before processing each comment in the per-comment loop. Comments at OR before the marker SHALL be skipped without invoking the trigger parser, without calling `execute_revision`, AND without incrementing the per-PR revisions counter.

The client-side filter uses `comment.created_at` (NOT `updated_at`) as the comparison key, matching the marker's semantics: the marker tracks the latest processed comment's creation time, AND editing a previously-processed comment SHALL NOT cause its revise text to be re-processed.

The two layers are belt-and-suspenders. Layer 1 reduces wasted GitHub API roundtrips by not re-fetching the duplicate in the first place. Layer 2 ensures correctness even when Layer 1 fails (future GitHub API revisions, future helper modifications, GitHub-side replication lag returning the same comment in two different fetches, etc.).

The existing canonical scenarios on the revision dispatcher (`Failed revision posts a failure comment`, `Revision cap per PR, with one-time decline`, `AskUser during revision escalates without committing`, `Per-PR state file persists revision count and last-seen timestamp`) are unaffected. Their behavior is preserved; this requirement layers above them to ensure their underlying assumption — "each comment is processed at most once" — actually holds.

#### Scenario: GitHub query uses millisecond precision
- **WHEN** the dispatcher calls `list_issue_comments_since(api_base, token, owner, repo, pr_number, since)` with `since = "2026-05-29T17:18:11.847Z"`
- **THEN** the outgoing HTTP query string contains `since=2026-05-29T17:18:11.847Z` (millisecond precision preserved)
- **AND** the query string does NOT contain `since=2026-05-29T17:18:11Z` (second-precision truncation)

#### Scenario: Marker with zero milliseconds still uses millisecond-precision query
- **WHEN** the dispatcher calls `list_issue_comments_since` with `since` constructed from a `DateTime<Utc>` whose millisecond component is 0 (e.g. `"2026-05-29T17:18:11.000Z"`)
- **THEN** the outgoing HTTP query string contains `since=2026-05-29T17:18:11.000Z`
- **AND** the formatter does NOT strip trailing zero milliseconds back to `since=2026-05-29T17:18:11Z`

#### Scenario: Comment at exact marker timestamp is skipped client-side
- **WHEN** the dispatcher's per-comment loop receives a comment whose `created_at` exactly equals `state.last_seen_comment_at`
- **THEN** the strict-inequality filter skips the comment
- **AND** the bot-author filter is NOT evaluated for this comment
- **AND** the trigger parser is NOT invoked
- **AND** `execute_revision` is NOT called
- **AND** the revision counter is NOT incremented

#### Scenario: Comment before marker timestamp is skipped client-side
- **WHEN** the dispatcher's per-comment loop receives a comment whose `created_at` is strictly less than `state.last_seen_comment_at` (e.g. due to API caching, replication lag, OR a re-fetch that returns historical comments)
- **THEN** the strict-inequality filter skips the comment
- **AND** `execute_revision` is NOT called

#### Scenario: Comment after marker timestamp is processed normally
- **WHEN** the dispatcher's per-comment loop receives a comment whose `created_at` is strictly greater than `state.last_seen_comment_at`
- **THEN** the strict-inequality filter does NOT skip the comment
- **AND** the existing bot-author filter, trigger parser, AND outcome-dispatch path proceed as today

#### Scenario: Same comment re-fetched across polling cycles is processed at most once
- **WHEN** iteration N processes an operator comment at `created_at: T_comment` AND the post-iteration state has `last_seen_comment_at: T_comment`, `revisions_applied: 1`
- **AND** iteration N+1 receives the SAME comment in its `list_issue_comments_since` response (due to GitHub's timestamp-precision behavior OR replication lag)
- **THEN** the strict-inequality filter skips the comment in iteration N+1
- **AND** iteration N+1's `execute_revision` call count is `0`
- **AND** the state after iteration N+1 is unchanged from after iteration N (`revisions_applied: 1`, `last_seen_comment_at: T_comment`)

#### Scenario: AskUser comment is preserved across iterations
- **WHEN** iteration N processes an operator comment AND `execute_revision` returns `AskUser`
- **AND** the marker is NOT advanced past the comment (per the canonical `AskUser during revision escalates without committing` requirement)
- **AND** iteration N+1 receives the SAME comment in its `list_issue_comments_since` response
- **THEN** the strict-inequality filter does NOT skip the comment (because `comment.created_at > state.last_seen_comment_at` — the marker was held back)
- **AND** the comment IS reprocessed in iteration N+1
- **AND** the existing AskUser-resume semantics are preserved
