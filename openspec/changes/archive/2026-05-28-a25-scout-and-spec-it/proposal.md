## Why

autocoder's existing verbs all assume the operator has already decided what to work on — `propose` takes a directive, `audit` runs a known check, `send it` acts on a specific audit's findings. None of them help the operator answer the upstream question: "I have a few minutes and an unfamiliar codebase, what's worth looking at?"

That question matters in two scenarios:

1. **OSS-contribution workspaces** where the operator wants to land small targeted PRs (security fixes, swallowed errors, type tightening) on an unfamiliar upstream project. A scouting pass that surveys the code, issue tracker, and TODO comments and produces a curated triage list is a high-leverage way to find the next thing to do.
2. **Long-running owned projects** where the operator periodically wants a "what would a fresh pair of eyes notice" pass without committing to a fix yet. Same shape — a curated list of opportunities, no mandate to act on any of them.

The existing audit framework doesn't fit: audits are HEAD-change-triggered defect detectors, not on-demand opportunity surveys. A new operator-triggered verb with a distinct lifecycle (produce a triage list, persist it, let the operator pick later) is the right shape.

Pairing scout with a follow-up verb that promotes one scouted item into the standard spec/PR pipeline closes the loop. `spec-it` is that verb — it's `propose` with the request text auto-filled from a scouted item the operator picked.

## What Changes

**New `scout` chatops verb.** Syntax:

```
@<bot> scout <repo-substring> [optional guidance text]
```

The repo-substring follows the established case-insensitive substring-match rule. Optional guidance is everything after the substring (trimmed, line breaks preserved, capped at 10,000 characters). The guidance steers the scout's focus (e.g., `focus on security fixes and helpful error messages`); it is passed verbatim to the scout prompt.

The dispatcher SHALL submit a `ScoutAction { repo_url, guidance: Option<String>, channel, thread_ts, request_id }` over the control socket. The bot's ack is a top-level channel message whose `ts` becomes the lifecycle thread.

**Scout polling-iteration handler.** A new per-iteration step drains at most one pending scout request per iteration (parallel to `pending_proposal_requests` AND `pending_brownfield_requests`). The handler invokes the executor in scout mode with:

- `WritePolicy::None` — scout produces a report, not files.
- Sandbox: `Read`, `Glob`, `Grep`, `Bash` (read-only). `gh` CLI access permitted for issue-tracker reads.
- Inputs: the scout prompt template (default `prompts/scout.md`, override via `features.scout.prompt_path`), the workspace's README + docs index, a code-symbol overview, the output of `git log --since=<N>` (recent activity), the open-issues list from `gh api repos/<owner>/<repo>/issues?state=open` (best-effort; graceful fall-through if the call fails), AND the operator's optional guidance.

The executor returns a JSON array of opportunity items. Each item SHALL have:

- `id: usize` — 1-indexed sequential identifier within the scout run.
- `category: String` — one of `security`, `bug`, `error_handling`, `type_tightening`, `code_smell`, `perf`, `documentation`, `test_coverage`, `issue`, `todo_fixme`, `research`.
- `title: String` — one-line summary.
- `body: String` — one-paragraph description naming what the candidate is AND why it might be worth pursuing.
- `source: String` — a pointer to where the item originated: `<file>:<line>` for code-derived items, an issue URL for issue-derived items, OR a commit-range/branch-name for git-log-derived items.
- `tractability: String` — one of `small` (clear single-PR fix), `medium` (needs scoping), `large` (likely multi-PR or research).

The list is capped at `features.scout.max_items` (default `30`). The handler persists the list in a `ScoutRunState` file at `<workspace>/.state/scout_runs/<request_id>.json` AND posts a rendered list to the request's thread (compact one-line-per-item format AND grouped by category).

**Tone.** The scout prompt explicitly instructs the LLM to phrase items as "things you might consider" rather than ranked recommendations. The LLM SHALL NOT use value statements like "this is critical," "you must," "this is the highest impact" — the operator does ranking. Items are surfaced for consideration, not advocated for.

**New `spec-it` chatops verb.** Syntax (replied inside a scout thread):

```
@<bot> spec-it <item-number> [optional guidance text]
```

The verb SHALL be recognized ONLY when posted as a reply within a scout request's lifecycle thread (identified via `thread_ts`). The `<item-number>` SHALL be a positive integer matching an `id` field in the most recent scout run for the resolved repo. Optional guidance refines scope; it is concatenated with the item's `body` AND any operator-supplied refinement before being submitted to the standard propose triage flow.

`spec-it` submits a `ProposeRequest` (reusing the existing propose machinery from canonical `orchestrator-cli` spec) with the request text built as:

```
[scout-item #<N>] <item.title>

<item.body>

Source: <item.source>
Category: <item.category>
Tractability: <item.tractability>

<operator guidance, if any>
```

The standard propose lifecycle takes over from there: triage classifies as DIRECTIVE/QUESTION/AMBIGUOUS, the executor runs, the iteration produces a fixes PR AND/OR spec PR per the existing two-PR mechanic, AND `@<bot> revise <text>` works on the resulting PR(s).

**Staleness handling.** When `spec-it` references a scout run older than `features.scout.staleness_warn_days` days OR when the workspace HEAD differs from HEAD-at-scout-time, the bot SHALL post a thread reply noting the staleness AND proceed (warn, do not block). Operators who want fresh results re-run `scout` first.

**Configuration:**

```yaml
features:
  scout:
    enabled: true                  # disable per-workspace
    prompt_path: null              # uniform a24 override pattern
    max_items: 30
    include_issues: true           # gh api opt-out for repos where issues are noise
    staleness_warn_days: 7
```

The `spec-it` verb has no separate enable flag — when scout is enabled AND a scout state file exists, spec-it is implicitly available.

**`gh api` access.** Scout SHALL attempt `gh api repos/<owner>/<repo>/issues?state=open` using the workspace's existing GITHUB_TOKEN. On failure (auth, rate limit, network), scout logs the failure AND continues without issue input. The resulting list omits issue-derived items but still includes code-derived items. The scout's thread post SHALL note when issue input was unavailable.

**Storage AND lifecycle.** `ScoutRunState` files live at `<workspace>/.state/scout_runs/<request_id>.json`. The "current" scout for a repo is the most-recent file by mtime. Older runs remain on disk for audit purposes; a new chatop verb `@<bot> clear-scout <repo>` deletes scout state files for that repo (advanced operator recovery; documented alongside the other `clear-*` verbs).

**No new audit type.** Scout is NOT registered in the periodic-audit framework — it's an on-demand verb, not a cadence-driven check. This keeps the audit registry focused on defect detection.

## Impact

- **Affected specs:**
  - `chatops-manager` — ADDED: `Inbound listener recognizes the scout verb AND submits a ScoutAction`. ADDED: `Inbound listener recognizes the spec-it verb when posted in a scout lifecycle thread`. ADDED: `Inbound listener recognizes the clear-scout verb`.
  - `orchestrator-cli` — ADDED: `scout polling-iteration handler produces a triage list AND persists ScoutRunState`. ADDED: `spec-it dispatch translates a scouted item into a ProposeRequest`. ADDED: `features.scout config schema`. ADDED: `Scout staleness warning when HEAD has drifted`.
  - `project-documentation` — ADDED: `docs/CHATOPS.md, docs/OPERATIONS.md, AND docs/CONFIG.md document the scout, spec-it, AND clear-scout verbs AND the features.scout config block`.
- **Affected code:**
  - `autocoder/src/chatops/listener.rs` — recognize `scout`, `spec-it`, AND `clear-scout` verbs; parse arguments; emit corresponding actions.
  - `autocoder/src/control_socket/actions.rs` — `ScoutAction`, `SpecItAction`, `ClearScoutAction` enum variants.
  - `autocoder/src/state/scout_run.rs` (new) — `ScoutRunState` with item list, HEAD-at-scout-time, AND timestamps.
  - `autocoder/src/polling/scout.rs` (new) — drains pending scout requests, invokes executor in scout mode, parses JSON output, persists state, posts thread reply.
  - `autocoder/src/polling/spec_it.rs` (new) — handles `SpecItAction` by translating the item into a `ProposeRequest` AND submitting it.
  - `autocoder/src/config.rs` — extend with `features.scout.{enabled, prompt_path, max_items, include_issues, staleness_warn_days}`.
  - `prompts/scout.md` (new) — embedded default scout prompt template.
  - `docs/CHATOPS.md`, `docs/OPERATIONS.md`, `docs/CONFIG.md` — verb + config documentation.
  - `PromptId` enum (added in `a24`) — gains a `Scout` variant.
- **Operator-visible behavior:**
  - `@<bot> help` lists `scout`, `spec-it`, AND `clear-scout` alongside the existing verbs.
  - `@<bot> scout <repo> [guidance]` produces a curated triage list posted to a lifecycle thread.
  - `@<bot> spec-it <N> [guidance]` (replied in a scout thread) initiates a propose-equivalent flow for the picked item.
  - `@<bot> clear-scout <repo>` deletes scout state files for that repo.
- **Breaking:** no. New verbs; opt-in via `features.scout.enabled` (defaults true). Existing behavior unchanged when scout is disabled.
- **Acceptance:** `cargo test` passes; `openspec validate a25-scout-and-spec-it --strict` passes. New tests:
  - Listener parses `@<bot> scout <repo> [guidance]` with AND without guidance.
  - Listener parses `@<bot> spec-it <N>` ONLY within a scout lifecycle thread; outside one, rejects with usage hint.
  - Mocked scout-mode executor return parses into `ScoutRunState` correctly.
  - Mocked failed `gh api` call falls through gracefully; scout completes with code-derived items only.
  - `spec-it` with an invalid item number produces the expected rejection.
  - Staleness warning fires when scout is older than `staleness_warn_days` OR HEAD has drifted.
  - `clear-scout` removes the per-repo state files AND replies with the count removed.
