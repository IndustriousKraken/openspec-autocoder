## 1. Chatops inbound parsing

- [x] 1.1 Add `scout`, `spec-it`, AND `clear-scout` to the recognized-verb list in the chatops inbound dispatcher.
- [x] 1.2 Parse `@<bot> scout <repo-substring> [optional guidance]`:
  - Resolve the repo substring via the existing match rule.
  - Trim AND cap guidance at 10,000 characters.
  - Refusal: missing repo, ambiguous repo, scout disabled (`features.scout.enabled: false`) → reply with usage/disabled hint, no state file written.
  - On success: generate `request_id`, post top-level ack `✓ Queued scout for <repo_url>. The next polling iteration will run it (~Nm). Follow along in this thread.`, capture ack `ts` as `thread_ts`, submit `ScoutAction`.
- [x] 1.3 Parse `@<bot> spec-it <item-number> [optional guidance]`:
  - The verb SHALL be recognized ONLY when posted as a reply inside a known scout lifecycle thread (the inbound parser looks up the parent thread's `ts` in `ScoutRunState` files).
  - The item-number SHALL be a positive integer.
  - Refusal cases (each → reply with reason, no action submitted):
    - Posted outside a scout thread → `✗ spec-it: only valid as a reply in a scout thread. Run @<bot> scout <repo> first.`
    - Non-integer item number → `✗ spec-it: <token> is not a valid item number. Usage: @<bot> spec-it <N> [guidance]`.
    - Item number out of range → `✗ spec-it: item #<N> not in this scout's list (range: 1..<max>).`
  - On success: submit `SpecItAction { repo_url, scout_request_id, item_id, guidance, channel, thread_ts }`.
- [x] 1.4 Parse `@<bot> clear-scout <repo-substring>`:
  - Refusal: missing/ambiguous repo, OR scout disabled.
  - On success: submit `ClearScoutAction { repo_url, channel, thread_ts }`; the polling iteration handles deletion AND replies with the count of cleared runs.
- [x] 1.5 Tests: each parse path, each refusal, AND the cross-thread scope check for spec-it.

## 2. Control-socket + state plumbing

- [x] 2.1 In `autocoder/src/control_socket/actions.rs`, add `ScoutAction`, `SpecItAction`, AND `ClearScoutAction` variants.
- [x] 2.2 New module `autocoder/src/state/scout_run.rs` defining `ScoutRunState { request_id, repo_url, guidance, head_sha_at_run, completed_at, thread_ts, channel, items: Vec<ScoutItem> }` AND `ScoutItem { id, category, title, body, source, tractability }`. Atomic-rename writes parallel to other state files.
- [x] 2.3 Per-workspace path: `<workspace>/.state/scout_runs/<request_id>.json`. The "current" scout for a repo is the most-recent file by mtime.
- [x] 2.4 Per-repo state extension: `pending_scout_requests: VecDeque<RequestId>` AND `pending_spec_it_requests: VecDeque<SpecItRequest>` queues.
- [x] 2.5 Tests: state-file round-trip, queue enqueue/dequeue, atomic-write safety, "current" scout resolution by mtime.

## 3. Scout polling-loop handler

- [x] 3.1 New module `autocoder/src/polling/scout.rs` exposing `process_pending_scout(repo_state, daemon_ctx) -> Result<()>`. Drains at most one request per iteration.
- [x] 3.2 Gather inputs:
  - The scout prompt template via `PromptLoader::load(PromptId::Scout, &workspace_config)`.
  - README + the list of `docs/*.md` filenames.
  - A code-symbol overview built via `cargo metadata` (Rust) OR a ripgrep pass for top-level public items (other languages).
  - `git log --since="<staleness_warn_days * 4> days ago" --pretty=oneline` for recent activity context.
  - Open-issues list via `gh api repos/<owner>/<repo>/issues?state=open --paginate` when `features.scout.include_issues: true`; on failure, log a WARN AND fall through with an empty issue list.
  - The operator's guidance text.
- [x] 3.3 Invoke the executor in scout mode (WritePolicy::None; sandbox: Read, Glob, Grep, Bash including `gh`).
- [x] 3.4 Parse the executor's response (JSON array of items). Validate:
  - Each item has the required fields (`id`, `category`, `title`, `body`, `source`, `tractability`).
  - Categories are in the allowed set; tractability is in the allowed set.
  - Item count <= `features.scout.max_items`.
  - Reject the run AND post a thread reply naming the validation failure if invariants violated.
- [x] 3.5 Persist `ScoutRunState` AND post the rendered list to the request's thread. Rendering:
  - Group by category. Within each category, render items as `**<id>. [<category>] <title>** — <body-first-sentence> _(source: <source>; tractability: <tractability>)_`.
  - If rendered length exceeds the threaded-notification limit, post the first N categories that fit AND end with `… (truncated; full list in <workspace>/.state/scout_runs/<request_id>.json)`. `spec-it` works against ALL items regardless of which are displayed.
- [x] 3.6 Append a closing note to the thread reply: `Reply with @<bot> spec-it <N> [optional guidance] to scope work on any item.`
- [x] 3.7 If `gh api` failed, the closing note also names that issue-derived items were skipped this run.
- [x] 3.8 Tests:
  - Mocked executor returns a valid JSON list → state file persisted, thread reply contains rendered list.
  - Mocked executor returns invalid JSON → state file NOT persisted, thread reply names the parse failure.
  - Mocked `gh api` failure → scout proceeds with code-derived items only, thread reply notes the skip.
  - List exceeding threaded-notification limit → truncation behavior matches spec.

## 4. Spec-it polling-loop handler

- [x] 4.1 New module `autocoder/src/polling/spec_it.rs` draining `pending_spec_it_requests`.
- [x] 4.2 For each request:
  - Load the referenced `ScoutRunState`. If missing (cleared by clear-scout between dispatch AND processing), post a thread reply naming the late deletion AND abort.
  - Look up the item by id; if absent, post a thread reply naming the missing item AND abort.
  - Compute staleness: `now - completed_at` AND `current_head_sha != head_sha_at_run`. If either crosses the threshold (`features.scout.staleness_warn_days` OR HEAD drifted), post a thread reply `⚠️ Scout from <relative-time> ago; HEAD has <unchanged|moved <N> commits>. Proceeding with the scouted item; consider re-running scout for fresh results.` (Warn, do not block.)
  - Construct the propose-request text per the proposal's "Request text shape" section.
  - Submit a `ProposeRequest` reusing the canonical propose machinery (with the constructed text). The resulting ack thread is a child of the scout thread for continuity OR a new top-level thread per existing propose conventions — the polling iteration SHALL chain its status updates back into the scout thread regardless.
- [x] 4.3 Tests:
  - Happy path: spec-it dispatches a ProposeRequest with the expected text shape.
  - Missing scout state → graceful failure with thread reply.
  - Item not in scout's list → graceful failure with thread reply.
  - Stale scout → warning posted, request still proceeds.

## 5. Clear-scout handler

- [x] 5.1 In the polling-iteration's control-socket-action handling, process `ClearScoutAction`:
  - List all files in `<workspace>/.state/scout_runs/`.
  - Delete each. (No filter by request_id — clear-scout is "wipe all scout state for this repo.")
  - Reply in the action's thread with `✓ Cleared <N> scout run(s) for <repo_url>.`
- [x] 5.2 Tests: clear with multiple runs present; clear with no runs (reply uses count `0`); idempotent across re-invocations.

## 6. Scout prompt template

- [x] 6.1 Create `prompts/scout.md`. Required content:
  - Role statement: "You are scouting an unfamiliar codebase for opportunities the operator might consider working on. Your output is a curated list, NOT a ranked recommendation set."
  - Tone rules: "Phrase items as 'things you might consider' rather than 'you should' or 'this is critical.' Do NOT use value statements like 'high impact,' 'must,' OR 'urgent.' The operator does ranking — your job is to surface candidates."
  - Categories AND tractability vocabularies (the allowed sets).
  - Output format: JSON array of items with the documented fields; nothing else in the response.
  - Cap rule: "Produce up to `<max_items>` items. Quality over quantity — better to surface 8 well-grounded items than 30 weak ones."
  - Source-pointer requirements per category (file:line for code; URL for issues; commit range for git-derived).
  - Anti-noise rules: do NOT flag style-only changes; do NOT flag feature requests requiring large work; do NOT flag changes that contradict project conventions visible in CONTRIBUTING.md OR similar; treat the operator's guidance as a focus filter, not just a topic suggestion.
- [x] 6.2 Register `PromptId::Scout` in `a24`'s loader registry.
- [x] 6.3 `features.scout.prompt_path` per the `a24` uniform pattern; loader handles precedence.

## 7. Config integration

- [x] 7.1 In `autocoder/src/config.rs`, extend `features` with `scout: { enabled: bool (default true), prompt_path: Option<String>, max_items: usize (default 30), include_issues: bool (default true), staleness_warn_days: u64 (default 7) }`.
- [x] 7.2 Validate `max_items` is in `1..=50`; reject config with values outside that range.
- [x] 7.3 Tests: defaults apply when block omitted; each field round-trips; invalid `max_items` fails fast.

## 8. Help-verb output

- [x] 8.1 Update the help-verb's output to include `scout`, `spec-it`, AND `clear-scout` with their one-line descriptions. Order: scout under "chat-driven workflow"; spec-it noted as "scout-thread-only"; clear-scout under operator-recovery verbs.

## 9. Docs

- [x] 9.1 `docs/CHATOPS.md`: add a `### scout` section under chat-driven workflow with syntax, output shape (compact description), AND the lifecycle-thread behavior. Add `### spec-it` noting it's scout-thread-only AND describes its translation to propose. Add `### clear-scout` under operator-recovery verbs.
- [x] 9.2 `docs/OPERATIONS.md`: in the existing "Onboarding existing projects" OR a new "Finding things to work on" section, describe the scout → pick → spec-it flow as the recommended discovery loop.
- [x] 9.3 `docs/CONFIG.md`: document the `features.scout.{enabled, prompt_path, max_items, include_issues, staleness_warn_days}` block.
- [x] 9.4 Update the `a24` Prompt overrides table to include `PromptId::Scout` with `prompts/scout.md` AND `features.scout.prompt_path`.
- [x] 9.5 `config.example.yaml`: include the `features.scout` block commented out.

## 10. Spec deltas

- [x] 10.1 `openspec/changes/a25-scout-and-spec-it/specs/chatops-manager/spec.md` ADDs the three verb-parsing requirements.
- [x] 10.2 `openspec/changes/a25-scout-and-spec-it/specs/orchestrator-cli/spec.md` ADDs the scout handler, spec-it handler, AND config-schema requirements.
- [x] 10.3 `openspec/changes/a25-scout-and-spec-it/specs/project-documentation/spec.md` ADDs the docs requirement.

## 11. Verification

- [x] 11.1 `cargo test` passes.
- [x] 11.2 `openspec validate a25-scout-and-spec-it --strict` passes.
- [x] 11.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
- [ ] 11.4 Manual verification on a small public OSS repo (one the operator has forked): `@<bot> scout <fork> focus on error handling`. Inspect the resulting list for quality. Pick one item via `spec-it` AND verify the resulting PR shape matches a normal propose run.
