## ADDED Requirements

### Requirement: Scout polling-iteration handler produces a triage list AND persists `ScoutRunState`
The daemon's per-repo polling iteration SHALL, after processing pending proposal AND brownfield requests AND before the standard change-processing pass, drain at most one pending scout request from `pending_scout_requests`. The handler SHALL invoke the executor in scout mode with `WritePolicy::None` AND a sandbox profile permitting `Read`, `Glob`, `Grep`, AND `Bash` (read-only, with `gh` permitted).

The scout prompt SHALL be loaded via `PromptLoader::load(PromptId::Scout, &workspace_config)` (per the executor spec). The prompt input SHALL be assembled from:

1. The resolved prompt template.
2. The operator's guidance (when non-empty), interpolated into a `## Operator guidance` section.
3. The workspace's `README.md` contents AND the list of `docs/*.md` filenames.
4. A code-symbol overview built via `cargo metadata` (Rust workspaces) OR a ripgrep pass for top-level public items (other languages).
5. `git log --since="<N> days ago" --pretty=oneline` output for recent-activity context, where N is `features.scout.staleness_warn_days * 4`.
6. The open-issues list via `gh api repos/<owner>/<repo>/issues?state=open --paginate` when `features.scout.include_issues: true`. On `gh` failure (auth, rate limit, network), the handler SHALL log a WARN naming the failure AND continue with an empty issue list.

The executor's response SHALL be a JSON array of opportunity items. Each item SHALL have:

- `id: usize` — 1-indexed sequential identifier.
- `category: String` — one of: `security`, `bug`, `error_handling`, `type_tightening`, `code_smell`, `perf`, `documentation`, `test_coverage`, `issue`, `todo_fixme`, `research`.
- `title: String` — one-line summary.
- `body: String` — one-paragraph description.
- `source: String` — `<file>:<line>` for code-derived, issue URL for issue-derived, OR commit-range for git-log-derived.
- `tractability: String` — one of: `small`, `medium`, `large`.

The handler SHALL validate the response: well-formed JSON, every item has all required fields, categories AND tractability values fall in the allowed sets, AND `items.len() <= features.scout.max_items`. On validation failure, the handler SHALL post a thread reply naming the failure AND NOT persist any state file.

On validation success, the handler SHALL: write `<workspace>/.state/scout_runs/<request_id>.json` with `ScoutRunState { request_id, repo_url, guidance, head_sha_at_run, completed_at, thread_ts, channel, items }`; render the list (grouped by category, compact per-item format) AND post it to the request's thread; append the closing note `Reply with @<bot> spec-it <N> [optional guidance] to scope work on any item.`. When the rendered list exceeds the threaded-notification length limit, the handler SHALL truncate the displayed list AND append `… (truncated; full list in <workspace>/.state/scout_runs/<request_id>.json)`.

#### Scenario: Happy-path scout run
- **WHEN** the executor returns a valid JSON list of 12 items AND the workspace has no `gh` failure
- **THEN** the handler persists `ScoutRunState` with 12 items
- **AND** posts a thread reply grouping items by category with the closing spec-it instruction
- **AND** the thread reply does NOT contain `(truncated; …)`

#### Scenario: Invalid JSON aborts the run
- **WHEN** the executor returns text that is not valid JSON OR is missing required item fields
- **THEN** no state file is written
- **AND** the thread reply names the validation failure AND points at the daemon log

#### Scenario: gh issues unavailable falls through gracefully
- **WHEN** `features.scout.include_issues: true` AND `gh api` returns a non-success exit code
- **THEN** a WARN is logged naming the gh failure
- **AND** the scout proceeds with code-derived items only
- **AND** the thread reply includes a note that issue-derived items were skipped this run

#### Scenario: Long list triggers truncation
- **WHEN** the rendered list exceeds the threaded-notification length limit
- **THEN** the handler posts the first N categories that fit
- **AND** appends `… (truncated; full list in <workspace>/.state/scout_runs/<request_id>.json)`
- **AND** the persisted state file contains ALL items (truncation affects display only)

#### Scenario: Max-items cap enforced
- **WHEN** `features.scout.max_items: 10` AND the executor returns a list with 15 items
- **THEN** the handler rejects the run via the validation step
- **AND** the thread reply names the cap violation

### Requirement: `spec-it` polling-iteration handler translates a scouted item into a `ProposeRequest`
The polling iteration SHALL drain at most one `SpecItAction` per iteration from `pending_spec_it_requests`. For each action, the handler SHALL: load the referenced `ScoutRunState`; look up the item by `item_id`; compute staleness; construct a propose-request-text per a documented shape; submit a `ProposeRequest` using the canonical propose machinery from the existing orchestrator-cli requirements.

The propose-request text SHALL be:

```
[scout-item #<N>] <item.title>

<item.body>

Source: <item.source>
Category: <item.category>
Tractability: <item.tractability>

<operator guidance, if any>
```

Status updates from the resulting propose lifecycle SHALL post into the scout's lifecycle thread (the spec-it action's `thread_ts`), keeping the scout → pick → spec → PR flow in a single visible conversation.

#### Scenario: Spec-it dispatches a ProposeRequest with the expected text shape
- **WHEN** the scout state contains an item with `id: 3, title: "Unauthenticated debug endpoint", body: "..."` AND the operator submits `SpecItAction { item_id: 3, guidance: None }`
- **THEN** a ProposeRequest is submitted with text matching the documented shape (header line, body, metadata lines)
- **AND** the resulting propose lifecycle's status updates post into the scout's thread

#### Scenario: Spec-it concatenates operator guidance
- **WHEN** the operator's `SpecItAction.guidance` is `stick to the OAuth scope, ignore the rate-limit angle`
- **THEN** the constructed propose-request text ends with `\n\nstick to the OAuth scope, ignore the rate-limit angle`

#### Scenario: Missing scout state aborts with thread reply
- **WHEN** the `SpecItAction.scout_request_id` references a state file that no longer exists (deleted by clear-scout between dispatch AND processing)
- **THEN** the handler posts `✗ spec-it: scout state for request <id> not found (was it cleared?). Re-run scout to refresh the list.`
- **AND** no propose-request is submitted

#### Scenario: Item not in scout's list aborts with thread reply
- **WHEN** the `SpecItAction.item_id` does not match any item id in the loaded state
- **THEN** the handler posts `✗ spec-it: item #<id> not present in scout state. The list may have changed; run @<bot> scout <repo> for a fresh list.`
- **AND** no propose-request is submitted

### Requirement: Scout staleness warning when scout is old OR HEAD has drifted
On each `spec-it` invocation, the handler SHALL compute two staleness signals:

1. `now - ScoutRunState.completed_at > features.scout.staleness_warn_days days`.
2. `current_workspace_HEAD_sha != ScoutRunState.head_sha_at_run`.

If either signal is true, the handler SHALL post a single thread reply BEFORE submitting the propose-request:

```
⚠️ Scout from <relative-time> ago; HEAD has <unchanged|moved <N> commits>. Proceeding with the scouted item; consider re-running scout for fresh results.
```

The handler SHALL warn AND PROCEED — staleness is not a blocking condition. Operators who want a fresh scout invoke `@<bot> scout <repo>` themselves.

#### Scenario: Scout older than threshold warns AND proceeds
- **WHEN** `features.scout.staleness_warn_days: 7` AND the scout's `completed_at` is 10 days ago
- **THEN** the handler posts the staleness warning naming `10 days`
- **AND** the propose-request still submits

#### Scenario: HEAD drift warns AND proceeds
- **WHEN** the scout's `head_sha_at_run` is `abc123` AND the workspace's current HEAD is `def456` AND the commit count between them is 5
- **THEN** the staleness warning names `HEAD has moved 5 commits`
- **AND** the propose-request still submits

#### Scenario: Both staleness signals combine in one warning
- **WHEN** both signals are true (scout old AND HEAD moved)
- **THEN** the handler posts ONE warning naming both conditions
- **AND** does NOT post two separate warnings

#### Scenario: Fresh scout produces no warning
- **WHEN** the scout completed less than `staleness_warn_days` days ago AND HEAD is unchanged
- **THEN** the handler does NOT post the staleness warning
- **AND** the propose-request submits without preamble

### Requirement: `features.scout` config schema
The per-repo config schema SHALL accept an optional `features.scout` block:

- `enabled: bool` (default `true`) — when `false`, the `scout`, `spec-it`, AND `clear-scout` verbs are refused at parse time.
- `prompt_path: Option<String>` (default `None`) — per the uniform PromptLoader pattern.
- `max_items: usize` (default `30`, valid range `1..=50`) — cap on the scout's item list.
- `include_issues: bool` (default `true`) — controls whether the handler attempts `gh api` for open issues.
- `staleness_warn_days: u64` (default `7`) — threshold for the staleness warning.

Invalid values (non-bool where bool expected; `max_items` outside `1..=50`) cause config-load to fail-fast with an error naming the offending field.

#### Scenario: Default config enables scout
- **WHEN** a workspace's config omits the `features.scout` block
- **THEN** all five fields take their defaults (`enabled: true, prompt_path: None, max_items: 30, include_issues: true, staleness_warn_days: 7`)

#### Scenario: Explicit disable refuses all three verbs
- **WHEN** a workspace's config sets `features.scout.enabled: false`
- **THEN** the dispatcher refuses `@<bot> scout`, `@<bot> spec-it`, AND `@<bot> clear-scout` for that workspace

#### Scenario: max_items outside valid range fails config load
- **WHEN** a workspace's config sets `features.scout.max_items: 0` OR `features.scout.max_items: 100`
- **THEN** config-load fails with an error naming `features.scout.max_items` AND the valid range `1..=50`
