# code-reviewer Specification

## Purpose
TBD - created by archiving change reviewer-integration. Update Purpose after archive.
## Requirements
### Requirement: AI-driven code-quality review
This requirement governs the `oneshot` reviewer transport (`reviewer.kind: oneshot`); the `agentic` transport — AND the `reviewer.kind` field's default — is specified by the **Agentic reviewer mode** requirement. The code-reviewer SHALL accept a structured `ReviewContext` containing the archived-change briefs, full contents of every file modified by the pass, and the unified diff, then send a rendered prompt to a configured LLM API and return a `ReviewReport { verdict, markdown }`. The review SHALL focus on code quality (security, error handling, naming, style, language idioms, obvious bugs) and SHALL NOT assess whether the diff correctly implements any spec — that is a separate verification concern handled in its own change. The reviewer's prompt-budget cap (the threshold past which touched-file context is truncated with a `## Skipped (budget exhausted): ...` footer) SHALL read from `reviewer.prompt_budget_chars` in `config.yaml`. The default value SHALL be `2000000` characters, preserving today's behavior verbatim for operators who do not set the field. There is no hard upper bound — the operator is responsible for matching the value to their LLM provider's actual context window.

#### Scenario: Successful review with parseable verdict (env-var key)
- **WHEN** `code_reviewer.review(context)` is called AND the
  configured LLM returns a response whose first non-empty line matches
  `(?i)^VERDICT:\s*(Pass|Concerns|Block)\s*$` AND
  `reviewer.api_key` is unset
- **THEN** the function returns `Ok(ReviewReport { verdict: <parsed value>, markdown: <remainder of response> })`
- **AND** the underlying HTTP call to the LLM API uses the
  `Authorization`/`x-api-key` scheme appropriate to the configured
  provider, with the token sourced from the environment variable named
  in `reviewer.api_key_env`

#### Scenario: Successful review with parseable verdict (inline key)
- **WHEN** `code_reviewer.review(context)` is called AND
  `reviewer.api_key` is set to `{ value: "..." }`
- **THEN** the underlying HTTP call uses the inline value verbatim as
  the token
- **AND** `reviewer.api_key_env`'s named environment variable is NOT
  consulted, regardless of whether it is set

#### Scenario: Both inline and env-var key set
- **WHEN** `reviewer.api_key` is set AND `reviewer.api_key_env` names an
  env var that is also set
- **THEN** the inline value wins
- **AND** autocoder emits exactly one `warn`-level log line at startup
  noting that `reviewer.api_key` takes precedence and the env var named
  by `reviewer.api_key_env` is being ignored

#### Scenario: Unparseable response
- **WHEN** the LLM response does not begin with a valid `VERDICT:` line
- **THEN** the function returns `Ok(ReviewReport { verdict: Concerns, markdown: "[reviewer response did not include a valid verdict line]\n\n<raw response>" })`

#### Scenario: Context assembly priority order
- **WHEN** the reviewer renders the prompt from a `ReviewContext`
- **THEN** the template's `{{change_context}}` placeholder is
  substituted with the concatenated `proposal.md` + `design.md` (if
  present) + `tasks.md` of every archived change in the pass, each
  prefixed by a `## Change: <name>` header
- **AND** the template's `{{changed_files}}` placeholder is
  substituted with the full contents of every file in the diff's
  name-only file list, each prefixed by a `## File: <path>` header
- **AND** the template's `{{diff}}` placeholder is substituted with
  the unified diff, included only if the rendered prompt's running
  total still fits inside the configured `reviewer.prompt_budget_chars`
  budget after the prior two sections; otherwise replaced with the
  literal text `(diff omitted: budget exhausted by change context and
  changed files)`

#### Scenario: Budget exhaustion mid-files
- **WHEN** the cumulative byte size of change context plus changed
  files exceeds `reviewer.prompt_budget_chars`
- **THEN** the reviewer includes whole files in order until the next
  file would push the running total over budget, then stops adding
  files
- **AND** the `{{changed_files}}` substitution ends with a
  `## Skipped (budget exhausted): <comma-separated paths>` footer
  naming every file that was not included
- **AND** the rendered prompt does not include the diff (the diff
  substitution is replaced by an explanatory message naming the
  budget exhaustion)
- **AND** individual files are NEVER truncated mid-content; a file
  either appears in full or appears in the skipped list

#### Scenario: LLM API failure
- **WHEN** the LLM API returns a non-2xx response or the HTTP request
  errors at the transport layer
- **THEN** `code_reviewer.review` returns `Err(_)` whose text contains
  the response status (or transport error description) and, when the
  response body is available, a snippet of it (truncated to 500 chars)

#### Scenario: Default config preserves the 2,000,000-character cap
- **WHEN** the operator's `config.yaml` does NOT set `reviewer.prompt_budget_chars`
- **AND** the reviewer is invoked against a multi-file pass whose touched-file content exceeds the cap
- **THEN** the reviewer's prompt is truncated at 2,000,000 characters (the resolved default)
- **AND** the `## Skipped (budget exhausted): ...` footer fires for the skipped files

#### Scenario: Higher cap permits more touched-file context
- **WHEN** the operator sets `reviewer.prompt_budget_chars: 4000000`
- **AND** the reviewer is invoked against a pass whose touched-file content is 3,000,000 characters total
- **THEN** the reviewer's prompt fits the full context (no truncation)
- **AND** no `## Skipped (budget exhausted): ...` footer fires

#### Scenario: Cap is hot-applicable via `autocoder reload`
- **WHEN** the operator changes `reviewer.prompt_budget_chars` in `config.yaml` AND runs `autocoder reload`
- **THEN** the daemon's reload handler applies the new value at the next iteration's reviewer invocation
- **AND** the existing `reviewer:` hot-reload path picks up the change without a daemon restart

### Requirement: Default prompt template enforces code-quality scope
The code-reviewer SHALL ship a default prompt template that explicitly limits the review to code-quality concerns and instructs the LLM not to assess spec compliance. The template SHALL use the `{{change_context}}`, `{{changed_files}}`, and `{{diff}}` placeholders.

The scope-limiting intent — that the default template confines the review to code quality and instructs the model not to assess spec compliance — is design intent captured by this requirement and verified by the drift audit's semantic judgment. It SHALL NOT be verified by a unit test asserting a verbatim substring of the template's instruction prose (per the project-documentation requirement `Tests assert behavior or derivation, never message wording`). The placeholder references, being behavior-relevant (the substitution code fills them), SHALL be verified by rendering the real default with sentinel inputs and asserting the substituted values appear — never by asserting the surrounding wording.

#### Scenario: Default template is shipped and substitutes every placeholder
- **WHEN** the autocoder binary is built AND the default template is rendered with a distinct sentinel value supplied for each of `{{change_context}}`, `{{changed_files}}`, AND `{{diff}}`
- **THEN** a file named `prompts/code-review-default.md` is included in the project repository at the relative path `prompts/code-review-default.md`
- **AND** the rendered output contains each placeholder's sentinel value, proving the shipped default references all three placeholders at least once
- **AND** the test asserts only the substituted sentinel values, NOT any hand-authored instruction wording of the template (the scope-limiting intent is verified by the drift audit, not a substring check)

#### Scenario: User-provided template overrides default
- **WHEN** `reviewer.prompt_template_path` is set in config
- **THEN** the code-reviewer reads the template from that path at
  startup and uses it instead of the default
- **AND** if the path does not exist or fails to read, startup
  returns a `Err(_)` naming the path
- **AND** no scope enforcement is performed on user-supplied
  templates (custom templates are user-owned)
- **AND** custom templates that still reference the retired
  `{{change_summary}}` placeholder are left with the literal text
  unsubstituted — the operator is responsible for migrating

### Requirement: LLM client surfaces an actionable error when a 2xx response is unusable
The code-reviewer's LLM-client layer (`AnthropicClient`, `OpenAiCompatibleClient`) SHALL distinguish "transport failed / HTTP error" (already covered by the non-2xx scenarios) from "transport succeeded but the response body cannot be turned into review text". For the latter case, the client SHALL return `Err(_)` whose message names the specific shape problem so the operator can tell from logs whether to retry, switch model, or escalate.

#### Scenario: Anthropic returns 2xx with no text content block
- **WHEN** an `AnthropicClient::complete` call gets a `200` response whose `content` array contains only non-text blocks (e.g. only `image` or `tool_use` entries)
- **THEN** the call returns `Err(_)` whose `format!("{err:#}")` contains a substring naming the missing-text-block condition (e.g. `no text block`)
- **AND** the error message does NOT claim the HTTP call failed (preserving the operator's ability to tell shape errors from transport errors in logs)

#### Scenario: Anthropic returns 2xx with unparseable JSON body
- **WHEN** an `AnthropicClient::complete` call gets a `200` whose body is not valid JSON of shape `AnthropicResponse`
- **THEN** the call returns `Err(_)` whose message contains a substring naming the decode failure (e.g. `decode failed`)

#### Scenario: OpenAI-compatible returns 2xx with empty choices array
- **WHEN** an `OpenAiCompatibleClient::complete` call gets a `200` with body `{"choices":[]}`
- **THEN** the call returns `Err(_)` whose message contains a substring naming the empty-choices condition (e.g. `no choices`)

#### Scenario: OpenAI-compatible returns 2xx with unparseable JSON body
- **WHEN** an `OpenAiCompatibleClient::complete` call gets a `200` whose body is not valid JSON of shape `OpenAiResponse`
- **THEN** the call returns `Err(_)` whose message contains a substring naming the decode failure (e.g. `decode failed`)

### Requirement: Cap-budget interaction with reviewer-posted comments
The reviewer-posting step SHALL respect the per-PR `executor.max_auto_revisions_per_pr` cap (legacy alias `executor.max_revisions_per_pr`). Reviewer-revision comments are automatic revisions AND count against this cap. When the reviewer would generate more should-revise concerns than the remaining cap budget allows, the daemon SHALL post only the first N concerns (where N = remaining budget; concerns are taken in the reviewer's output order, which the reviewer's prompt template instructs to be most-critical-first) AND SHALL annotate the dropped concerns in the PR-body `## Code Review` section so the human sees what was skipped.

#### Scenario: Cap budget exhausted truncates posts and annotates drops
- **WHEN** the reviewer returns Block with 3 should-revise concerns AND the per-PR remaining cap budget is 2
- **THEN** exactly 2 reviewer-revision comments are posted (the first 2 in the reviewer's output order)
- **AND** the PR-body `## Code Review` section contains an entry for the third concern annotated `(not auto-revised; cap budget exhausted)`

#### Scenario: Cap budget zero posts nothing
- **WHEN** the reviewer returns Block with should-revise concerns AND the per-PR remaining cap budget is 0
- **THEN** no comments are posted
- **AND** every should-revise concern is annotated in the PR-body section with `(not auto-revised; cap budget exhausted)`

### Requirement: Self-author filter exception for reviewer-revision comments
The revision dispatcher from `a01-pr-comment-revision-loop` SHALL permit bot-authored comments whose body's first non-whitespace text is the literal HTML-comment marker `<!-- reviewer-revision -->` to bypass its self-author filter. All other bot-authored comments — the dispatcher's own `✅ Revision applied:` / `✗ Revision attempt failed:` replies, the cap-decline message, any future bot-posted content — SHALL continue to be filtered as today.

#### Scenario: Reviewer-marked comment bypasses self-author filter
- **WHEN** the dispatcher fetches a comment whose `user_login == self_bot_username` AND whose body starts with `<!-- reviewer-revision -->\n@<bot-username> revise foo`
- **THEN** the dispatcher passes the body to `parse_revision_trigger`
- **AND** the parser returns `Some("foo")`
- **AND** the dispatcher executes the revision normally

#### Scenario: Unmarked bot-authored comment continues to be filtered
- **WHEN** the dispatcher fetches a comment whose `user_login == self_bot_username` AND whose body does NOT start with the marker (e.g. body is `✅ Revision applied: foo`)
- **THEN** the comment is filtered out before parsing
- **AND** no recursive revision is triggered

#### Scenario: Human-authored comment is unaffected by the marker rule
- **WHEN** the dispatcher fetches a comment whose `user_login != self_bot_username`
- **THEN** the self-author filter is irrelevant
- **AND** the comment proceeds to `parse_revision_trigger` regardless of whether the body contains the marker

### Requirement: Backwards compatibility for unaware reviewer templates
Operators with customized reviewer templates that have NOT been updated to emit the new `actionable_request` and `should_request_revision` per-concern fields SHALL see no behavioural change: the response parser defaults missing fields to `actionable_request: None` and `should_request_revision: false`, the posting step finds zero should-revise concerns, and posts zero reviewer-revision comments. The daemon SHALL log a one-shot WARN on the first reviewer-pass in such a session when `reviewer.auto_revise` is enabled, naming the gap so operators see the actionable diagnostic.

#### Scenario: Customized template missing the new fields produces no comments
- **WHEN** the reviewer's response (from an operator-customized template that pre-dates this change) contains concerns without `should_request_revision` fields AND `reviewer.auto_revise: true`
- **THEN** the parser defaults `should_request_revision: false` for every concern
- **AND** zero reviewer-revision comments are posted
- **AND** the daemon logs a WARN naming the gap and pointing at the prompt-template documentation

### Requirement: No reviewer re-run after a reviewer-initiated revision lands

The reviewer SHALL run exactly once per polling iteration's executor pass, as today. A reviewer-initiated revision committed in a subsequent iteration SHALL NOT trigger a re-evaluation by the reviewer; the verdict from the original pass is "frozen" for the life of the PR EXCEPT via the explicit operator-trigger path introduced in this change. Operators wanting iterative reviewer evaluation now have three options:

1. **Operator-initiated re-review via PR comment.** A `@<bot> code-review` PR comment triggers a fresh reviewer pass against the current PR state, bounded by the per-PR cap `reviewer.max_code_reviews_per_pr` (default `5`). See the requirement `Operator-initiated re-review via @<bot> code-review verb` below.
2. **`autocoder rewind`** to re-issue the iteration from scratch (existing).
3. **Close + re-open the PR** to force the polling loop to treat it as a new PR pass (existing).

The original "freeze" semantic remains the default for unattended operation: reviewer cost is bounded because re-runs require explicit operator action. Automatic re-evaluation on revision-lands is NOT introduced by this change.

#### Scenario: Reviewer does not re-run when a revision lands
- **WHEN** a reviewer-initiated revision is committed and force-pushed in iteration N+1
- **THEN** the reviewer is NOT invoked again in iteration N+1
- **AND** the existing `## Code Review` section in the PR body is not updated
- **AND** the PR's draft status (set by the original Block verdict) is preserved

#### Scenario: Operator-initiated re-review IS the explicit exception
- **WHEN** an operator posts `@<bot> code-review` as a PR comment AND the reviewer cap is not exhausted
- **THEN** the reviewer IS invoked against the current PR state (per the `Operator-initiated re-review via @<bot> code-review verb` requirement below)
- **AND** the output is posted as a fresh PR comment with the `## Code Review (rerun N of M)` heading
- **AND** the PR body's original `## Code Review` block is NOT modified

### Requirement: `reviewer.mode: per_change` dispatches one reviewer call per change in the PR
The reviewer SHALL accept a `reviewer.mode` config field with values `bundled` (default) AND `per_change`. Under `bundled`, the existing single-reviewer-call-per-PR behavior SHALL be preserved verbatim. Under `per_change`, the reviewer SHALL dispatch one LLM call per change in the pass, each scoped to that change's diff + the files that specific change touched, AND emit one `## Code Review: <change-slug>` section per change in the PR body (instead of one combined `## Code Review` block).

Each per-change reviewer prompt SHALL include a fixed-size cross-change preamble naming the OTHER changes in the same PR (slug + first-paragraph-of-`## Why`, each truncated to 200 characters). The preamble exists for cross-reference context only; the reviewer's verdict for each change applies strictly to that change.

#### Scenario: Default `bundled` mode is unchanged
- **WHEN** the operator's `config.yaml` does NOT set `reviewer.mode`
- **AND** a 3-change PR pass is reviewed
- **THEN** the reviewer is invoked exactly once
- **AND** the PR body contains one `## Code Review` block (not three)
- **AND** the behavior is byte-identical to pre-spec output for the same inputs

#### Scenario: `per_change` mode invokes the reviewer N times for an N-change pass
- **WHEN** the operator sets `reviewer.mode: per_change`
- **AND** a 3-change PR pass is reviewed
- **THEN** the LLM client receives exactly 3 reviewer invocations
- **AND** each invocation's prompt contains ONLY that change's diff AND the files that change touched
- **AND** each invocation's prompt contains the cross-change preamble naming the OTHER 2 changes (slug + truncated-summary, one line each)
- **AND** the PR body contains 3 `## Code Review: <change-slug>` sections in change order
- **AND** each section follows the same verdict + concerns + format the bundled `## Code Review` block uses

#### Scenario: Per-change reviews independently respect the prompt budget
- **WHEN** `reviewer.mode: per_change` AND one change in a 3-change pass touches a huge file that exceeds the per-call budget
- **THEN** ONLY that change's reviewer section emits a `## Skipped (budget exhausted): ...` footer
- **AND** the other 2 changes' reviews are unaffected
- **AND** each change's verdict is computed independently

#### Scenario: Reviewer-initiated revisions aggregate across per-change reviews
- **WHEN** `reviewer.mode: per_change` AND `reviewer.auto_revise_on_block: true`
- **AND** a 3-change PR pass produces 2 revision-request concerns per change (6 total)
- **AND** `executor.max_revisions_per_pr: 5`
- **THEN** the dispatcher posts the 5 highest-priority revision requests as `<!-- reviewer-revision -->`-marked PR comments
- **AND** the 6th request is annotated in its `## Code Review: <slug>` section as `(not auto-revised; cap budget exhausted)`
- **AND** the cap-budget interaction applies across the union of all per-change reviews, not per-change

#### Scenario: Single-change pass omits the preamble's "other changes" list
- **WHEN** `reviewer.mode: per_change` AND a single-change pass is reviewed
- **THEN** the cross-change preamble is included with an empty "other changes" list (or the preamble is omitted entirely as a formatting choice)
- **AND** the LLM is not confused about the pass containing other changes when it doesn't

### Requirement: Operator-initiated re-review via `@<bot> code-review` verb

The revisions dispatcher (`autocoder/src/revisions.rs::process_one_pr`) SHALL recognize `@<bot> code-review` as a new PR-comment verb. The verb takes no arguments in v1; the dispatcher matches the verb pattern case-insensitively on `code-review` AND ignores any trailing text on the same line.

When the verb matches AND the cap is not exhausted, the daemon SHALL:

1. Verify `reviewer.enabled` is `true` AND a usable `api_key` is present. When NOT enabled, post a PR comment whose body starts with `✗ Code review not available: reviewer is disabled in config` AND advance the seen-marker. No reviewer pipeline is invoked.
2. Construct a `ReviewContext` from PR-sourced material: head SHA, full diff, change list (extracted from PR body per `a20a5`), file list, AND the configured `reviewer.mode` (`bundled` OR `per_change`).
3. Invoke the extracted reviewer entry point (`code_reviewer::review_pr_at_state(cfg, &ctx)`) AND await its `ReviewResult`.
4. Post the reviewer's output as a FRESH PR comment whose body starts with the canonical heading:

   ```
   ## Code Review (rerun {N} of {M})
   ```

   where `N` is `state.code_reviews_applied + 1` AND `M` is `state.code_review_cap`. The verdict, per-concern text, AND raw model output (truncated per the existing canonical reviewer-output discipline) follow.

5. On a `Block` verdict AND `reviewer.auto_revise_on_block: true`: post the per-concern `<!-- reviewer-revision -->`-marked PR comments (the existing canonical "Reviewer-initiated revision comments on Block verdicts" requirement applies AND its behavior is preserved).
6. Increment `state.code_reviews_applied`. Advance the seen-marker past the triggering comment. Write the state file.

The original PR body's `## Code Review` block SHALL NOT be modified by any re-review. History is preserved as a chronological sequence of fresh comments.

The PR's draft status SHALL NOT be auto-toggled by any re-review's verdict:

- A re-review's `Approve` verdict on a previously-Blocked PR leaves the draft status as draft (operator undrafts manually after confirming).
- A re-review's `Block` verdict on a previously-Approved PR leaves the draft status as ready (operator drafts manually if desired).
- The original PR-open review's draft-status semantics (Block → draft, Approve → ready) are unchanged. Only re-reviews are exempt from auto-toggling.

When `reviewer.enabled` is `false` OR no usable `api_key` is present, the verb SHALL produce the "not available" reply AND SHALL NOT invoke the reviewer pipeline OR increment any counter.

#### Scenario: Verb triggers fresh review against current PR state
- **WHEN** an operator posts `@<bot> code-review` as a PR comment on an open PR
- **AND** `reviewer.enabled: true` AND `state.code_reviews_applied < state.code_review_cap`
- **THEN** the dispatcher invokes `review_pr_at_state` with a `ReviewContext` built from the PR's current head SHA, diff, change list, AND files
- **AND** the reviewer's output is posted as a fresh PR comment whose body starts with `## Code Review (rerun 1 of 5)` (for the first rerun)
- **AND** the PR body's original `## Code Review` block is NOT modified
- **AND** `state.code_reviews_applied` increments by 1

#### Scenario: Reviewer disabled produces canonical reply without invocation
- **WHEN** an operator posts `@<bot> code-review` AND `reviewer.enabled: false`
- **THEN** the dispatcher posts a PR comment whose body starts with `✗ Code review not available: reviewer is disabled in config`
- **AND** the reviewer pipeline is NOT invoked
- **AND** `state.code_reviews_applied` is NOT incremented
- **AND** the seen-marker IS advanced (the trigger does not re-fire on subsequent polling cycles)

#### Scenario: Block verdict on re-review fires auto-revise comments when enabled
- **WHEN** an operator-initiated re-review produces a `Block` verdict
- **AND** `reviewer.auto_revise_on_block: true`
- **THEN** the daemon posts the per-concern `<!-- reviewer-revision -->`-marked PR comments (existing canonical behavior unchanged)
- **AND** the revise dispatcher picks them up on the next polling cycle exactly as it does for original-review Block verdicts

#### Scenario: Re-review never auto-toggles draft status
- **WHEN** a previously-Blocked PR receives an operator-initiated re-review with an `Approve` verdict
- **THEN** the PR remains in draft state (the operator must manually undraft)
- **AND** the polling-iteration code does NOT call `gh pr ready` OR any equivalent draft-toggle API

#### Scenario: Re-review under `per_change` mode emits per-change content
- **WHEN** an operator-initiated re-review runs with `reviewer.mode: per_change` AND the PR has 3 changes
- **THEN** the LLM client receives exactly 3 reviewer invocations (matching the original review's behavior)
- **AND** the resulting PR comment(s) contain per-change content under `## Code Review (rerun N of M)` heading (whether emitted as one combined comment OR three separate comments is implementer-discretion)

### Requirement: Re-review cap (`reviewer.max_code_reviews_per_pr`) is independent of revision cap

The `reviewer.max_code_reviews_per_pr` config field SHALL bound operator-initiated re-reviews per PR ONLY when the operator sets it; its default SHALL be UNLIMITED (unset). Re-reviews are uncapped by default because every re-review is a deliberate operator action triggered via `@<bot> code-review`, AND there is no automatic-re-review path (per the canonical "No reviewer re-run after a reviewer-initiated revision lands" requirement), so there is no runaway to bound. When set to a positive integer (ceiling `20`, WARN-and-clamp at startup), it acts as an opt-in ceiling.

The cap is independent of the `executor.max_auto_revisions_per_pr` cap — re-reviews AND automatic revisions consume separate counters in the same per-PR state file. The original automatic review at PR-open time does NOT count against the cap (it is not a re-review).

When the cap is set AND exceeded, the daemon SHALL post a one-time PR decline comment whose body starts with:

```
🛑 Code review cap reached (N reruns). Further @<bot> code-review requests will be ignored. Close + re-open the PR or merge as-is.
```

AND a one-time chatops notification:

```
🛑 <repo>: PR #<num> hit the code-review cap of N. Further @<bot> code-review requests ignored.
```

After posting the decline, the daemon SHALL silently ignore subsequent `code-review` verbs on the same PR (seen-marker still advances; no PR reply; no chatops notification beyond the one-time decline). When the cap is UNSET (the default), no decline is ever posted AND re-reviews always process.

#### Scenario: Default (unset) cap means unlimited re-reviews
- **GIVEN** `reviewer.max_code_reviews_per_pr` is unset (the default)
- **WHEN** an operator posts `@<bot> code-review` for the Nth time on a PR, for any N
- **THEN** the re-review IS dispatched
- **AND** no cap-decline comment is ever posted
- **AND** `state.code_reviews_applied` increments (tracked for display) but is never compared against a ceiling

#### Scenario: First over-cap trigger posts the decline once (cap set)
- **GIVEN** the operator has set `reviewer.max_code_reviews_per_pr`
- **WHEN** an open PR has had `max_code_reviews_per_pr` re-reviews applied AND a new `@<bot> code-review` comment arrives
- **THEN** the daemon posts a PR comment whose body starts with `🛑 Code review cap reached`
- **AND** a chatops notification fires whose text starts with `🛑 <repo>: PR #<num> hit the code-review cap`
- **AND** `state.cap_decline_posted_for_code_review` is set to `true`

#### Scenario: Subsequent over-cap triggers are silently ignored (cap set)
- **GIVEN** the operator has set `reviewer.max_code_reviews_per_pr`
- **WHEN** a PR already has `cap_decline_posted_for_code_review: true` AND a new `@<bot> code-review` comment arrives
- **THEN** the daemon advances `last_seen_comment_at` to the new comment's `created_at`
- **AND** no PR reply is posted
- **AND** no chatops notification fires
- **AND** the reviewer pipeline is NOT invoked

#### Scenario: Revision cap AND re-review cap are independent
- **WHEN** a PR has `auto_revisions_applied: 5` (at the automatic-revision cap) AND `code_reviews_applied: 2`
- **AND** an operator posts `@<bot> code-review`
- **THEN** the re-review IS dispatched (the automatic-revision cap does NOT block re-reviews)
- **AND** `state.code_reviews_applied` increments to 3

### Requirement: Reviewer entry point is reusable across polling-loop AND operator-trigger callers

The reviewer's LLM-invocation logic SHALL be exposed as a reusable function `code_reviewer::review_pr_at_state(cfg: &ReviewerConfig, ctx: &ReviewContext) -> Result<ReviewResult>`.

- `ReviewContext` SHALL carry `head_sha: String`, `diff: String`, `change_list: Vec<String>`, `files: Vec<FileEntry>`, AND `mode: ReviewerMode`.
- `ReviewResult` SHALL carry `verdict: Verdict (Approve | Block)`, `per_concern: Vec<ConcernEntry>`, `raw_output: String`, AND `per_change_sections: Vec<PerChangeSection>`. `per_change_sections` is populated with one entry per change when `ctx.mode` is `per_change`, AND is empty when `ctx.mode` is `bundled`.

The function SHALL NOT decide output disposition; the caller decides whether to write into the PR body's `## Code Review` block (polling-loop caller) OR post as a fresh PR comment with `## Code Review (rerun N of M)` heading (operator-trigger caller).

The function SHALL itself perform the per-mode dispatch per the existing canonical `reviewer.mode: per_change dispatches one reviewer call per change in the PR` requirement: one call per change in `per_change` mode (populating `per_change_sections`), one call per PR in `bundled` mode (leaving `per_change_sections` empty). It SHALL NOT route through a bundled-only entry point that ignores `ctx.mode`; both the polling-loop caller AND the operator-trigger caller observe the configured mode identically.

Because `ReviewResult` carries the per-change sections, the operator-trigger caller (the `@<bot> code-review` rerun composer) SHALL render them: when `per_change_sections` is non-empty it emits one per-change section per entry beneath the `## Code Review (rerun N of M)` heading; when empty it renders the bundled output as before.

The reviewer prompt template, LLM client, output validation, AND retry semantics are unchanged in this change.

#### Scenario: Polling-loop caller produces byte-identical PR body output to pre-spec behavior
- **WHEN** the polling-loop invokes `review_pr_at_state` with a canned PR state
- **AND** the function returns a `ReviewResult`
- **AND** the polling-loop's output-disposition code writes into the PR body's `## Code Review` block
- **THEN** the resulting PR body is byte-identical to pre-spec output for the same inputs (the extraction is refactor-only, no behavior change)

#### Scenario: Operator-trigger caller uses the same function with different disposition
- **WHEN** the operator-trigger dispatcher invokes `review_pr_at_state` with the SAME `ReviewContext` the polling-loop would have built
- **THEN** the function returns the SAME `ReviewResult` (the LLM call AND validation logic are identical)
- **AND** the operator-trigger's output-disposition code posts as a fresh PR comment instead of editing the PR body

#### Scenario: Operator-trigger rerun honors per_change mode end-to-end
- **GIVEN** `reviewer.mode: per_change` AND a PR carrying 3 changes
- **WHEN** an operator-initiated re-review invokes `review_pr_at_state` with a `ReviewContext` whose `mode` is `per_change`
- **THEN** the reviewer is invoked once per change (3 invocations), matching the initial-review path
- **AND** the returned `ReviewResult.per_change_sections` contains 3 entries, one per change
- **AND** the rerun comment composer renders 3 per-change sections (one `## Code Review: <change-slug>` per change) beneath the `## Code Review (rerun N of M)` heading, NOT a single bundled block

#### Scenario: Bundled mode rerun is unchanged
- **GIVEN** `reviewer.mode` is `bundled` (the default)
- **WHEN** an operator-initiated re-review invokes `review_pr_at_state`
- **THEN** the reviewer is invoked once for the PR
- **AND** `ReviewResult.per_change_sections` is empty
- **AND** the rerun comment composer renders the single bundled block exactly as before this change

### Requirement: `reviewer.provider` accepts `ollama` as a first-class provider via a new `OllamaChatClient`

The `reviewer.provider` field SHALL accept `ollama` alongside the existing `anthropic` AND `openai_compatible` values (per the orchestrator-cli canonical `LlmProvider` enum). When `provider: ollama`, the reviewer SHALL invoke a new `OllamaChatClient` that POSTs to `<api_base_url>/api/chat` using Ollama's native chat API.

The `OllamaChatClient` SHALL:

- POST to `<api_base_url>/api/chat` (trailing slashes on `api_base_url` are trimmed). NOT to `<api_base_url>/v1/chat/completions` (Ollama's OpenAI-compat shim — operators using `openai_compatible` to point at Ollama AND including `/v1` in their base URL continue to use that path AS A LEGACY OPTION, but the canonical Ollama path is the native one).
- Send body shape `{"model": <model>, "messages": [{"role": "user", "content": <prompt>}], "stream": false}`. The `stream: false` flag SHALL be explicit so Ollama returns a single-response payload (matching the existing `AnthropicClient` AND `OpenAiCompatibleClient` shapes).
- NOT send an `Authorization` header. Ollama does not authenticate; the per-provider auth-semantics requirement REJECTS `api_key` at config-load when `provider: ollama`, so no key is ever in scope to send.
- Parse the response shape `{"message": {"role": "assistant", "content": "<text>"}, "done": true, ...}` AND return the `message.content` string as the completion.
- On non-2xx HTTP status, return `Err` with the status code AND the first 500 characters of the response body (matching the existing `OpenAiCompatibleClient` error shape).
- On 2xx with a malformed-JSON OR schema-mismatched body, return `Err` with a clear decode-failure message naming `OllamaChatClient`.

The `OllamaChatClient` SHALL implement the same `LlmClient` trait that `AnthropicClient` AND `OpenAiCompatibleClient` implement. Reviewer dispatch (`llm::build_from_config`), contradiction-check dispatch, AND any future LLM-using caller dispatch SHALL match the new `LlmProvider::Ollama` variant AND construct the new client.

Operators using Ollama for the reviewer SHALL configure the bare Ollama host URL (e.g. `api_base_url: http://localhost:11434`) WITHOUT the `/v1` suffix. The new client targets Ollama's native path; the `/v1` suffix is only relevant to the legacy `openai_compatible`-pointed-at-Ollama configuration shape.

#### Scenario: `provider: ollama` for reviewer constructs the new client
- **WHEN** `reviewer.provider: ollama` AND `reviewer.api_base_url: http://10.42.11.10:11434` AND `reviewer.model: qwen2.5-coder:32b`
- **AND** the reviewer is invoked for a code review
- **THEN** the underlying `LlmClient` is an `OllamaChatClient`
- **AND** the HTTP POST target is `http://10.42.11.10:11434/api/chat`
- **AND** the request body contains `"model": "qwen2.5-coder:32b"`, `"messages": [...]`, AND `"stream": false`
- **AND** the request does NOT include an `Authorization` header

#### Scenario: `OllamaChatClient` parses successful response into `LlmClient::complete` result
- **WHEN** `OllamaChatClient::complete("review this diff: ...")` is invoked AND the mock Ollama server returns 200 with `{"message":{"role":"assistant","content":"VERDICT: Pass\n\nLooks good."},"done":true}`
- **THEN** the function returns `Ok("VERDICT: Pass\n\nLooks good.")`

#### Scenario: `OllamaChatClient` surfaces non-2xx as actionable error
- **WHEN** the mock Ollama server returns 404 with body `{"error":"model 'nonexistent' not found"}`
- **THEN** `OllamaChatClient::complete` returns `Err` containing `404` AND the first 500 characters of the body

#### Scenario: `OllamaChatClient` surfaces malformed response as actionable error
- **WHEN** the mock Ollama server returns 200 with body `{"unexpected_shape": true}` (no `message.content`)
- **THEN** `OllamaChatClient::complete` returns `Err` with a message naming `OllamaChatClient` AND the decode failure

#### Scenario: Legacy openai_compatible-pointed-at-Ollama config continues to work
- **WHEN** an existing config has `reviewer.provider: openai_compatible`, `reviewer.api_key.value: "ollama"` (dummy), AND `reviewer.api_base_url: http://10.42.11.10:11434/v1`
- **THEN** config-load succeeds (the `openai_compatible` provider requires `api_key`, which is present; the dummy value is accepted)
- **AND** review invocations POST to `http://10.42.11.10:11434/v1/chat/completions` (Ollama's OpenAI-compat shim)
- **AND** Ollama returns a successful response (the shim is functional)
- **AND** the operator can migrate to `provider: ollama` + bare base URL + no api_key at their discretion without behavioral regression

### Requirement: Reviewer-initiated revision comments on actionable concerns
When `reviewer.auto_revise` is `true`, the daemon SHALL post one PR issue comment per concern where the reviewer marked `should_request_revision: true` AND supplied a non-empty `actionable_request`, REGARDLESS of the review's verdict (`Pass`, `Concerns`, OR `Block`), subject to the per-PR revision-cap budget. Each comment's body SHALL begin with the marker line `<!-- reviewer-revision -->` followed by a newline, then the trigger pattern `@<bot-username> revise <actionable_request>`. The marker enables the revision dispatcher's self-author-filter bypass; without it the dispatcher would (correctly) filter the comment as bot-authored noise. The feature is off by default; the config flag must be explicitly enabled.

The verdict is no longer consulted when deciding whether to post reviewer-revision comments. The `Block` verdict retains its separate effect of marking the PR as draft (per the existing draft-on-Block behavior); it simply no longer gates auto-revise. The actionability signal is the per-concern `should_request_revision` + `actionable_request` pair.

The config flag is `reviewer.auto_revise`. The legacy name `reviewer.auto_revise_on_block` SHALL continue to be accepted as an alias so existing config files load unchanged.

The per-PR revision-cap budget that bounds this posting (currently `executor.max_revisions_per_pr`) is unchanged by this requirement; it bounds all reviewer-revision posts the same as today. (Refining that cap to bound only automatic chains while uncapping human-initiated revisions is a separate change.)

#### Scenario: Off-by-default has no behavioural change
- **WHEN** `reviewer.auto_revise` is absent OR `false` AND the reviewer returns any verdict
- **THEN** no reviewer-revision comments are posted
- **AND** the existing PR-body `## Code Review` section is the only reviewer output channel

#### Scenario: Concerns verdict with actionable concerns posts comments
- **WHEN** `auto_revise: true` AND the reviewer returns `Concerns` AND the response contains two concerns with `should_request_revision: true` and non-empty `actionable_request` AND the per-PR remaining cap budget is at least 2
- **THEN** exactly two PR issue comments are posted
- **AND** each comment's body starts with `<!-- reviewer-revision -->\n`
- **AND** each comment's body's second non-whitespace line matches `@<bot-username> revise <actionable_request>` for that concern

#### Scenario: Pass verdict with an actionable concern posts a comment
- **WHEN** `auto_revise: true` AND the reviewer returns `Pass` AND one concern has `should_request_revision: true` with a non-empty `actionable_request` AND the remaining cap budget is at least 1
- **THEN** exactly one reviewer-revision comment is posted (the verdict does NOT gate posting)

#### Scenario: Block verdict with actionable concerns still posts comments
- **WHEN** `auto_revise: true` AND the reviewer returns `Block` AND the response contains concerns with `should_request_revision: true` and non-empty `actionable_request` within the remaining cap budget
- **THEN** one reviewer-revision comment per such concern is posted (the Block path is preserved, not regressed)
- **AND** the PR is also marked draft per the existing draft-on-Block behavior

#### Scenario: No actionable concerns posts nothing under any verdict
- **WHEN** `auto_revise: true` AND the reviewer returns any verdict AND every concern has `should_request_revision: false` OR an empty `actionable_request`
- **THEN** no reviewer-revision comments are posted
- **AND** the daemon logs a WARN noting that auto-revise is enabled but the reviewer produced no actionable-revision concerns (signals operator that the reviewer template may need updating)

#### Scenario: Legacy `auto_revise_on_block` config key still works
- **WHEN** a config file sets `reviewer.auto_revise_on_block: true` (the legacy key)
- **THEN** it loads identically to `reviewer.auto_revise: true` via the serde alias
- **AND** no deprecation warning is emitted (the alias is a silent compatibility path)

### Requirement: Reviewer renders its prompt with single-pass substitution
The reviewer SHALL assemble its prompt using the single-pass substitution helper (per the orchestrator-cli `Prompt-template substitution is single-pass` requirement), so a `{{cross_change_preamble}}` / `{{change_context}}` / `{{changed_files}}` / `{{diff}}` token appearing inside a substituted value is NOT re-expanded. This matters most for the `{{changed_files}}` value: a changed file's contents are arbitrary, and when the change under review is a template, documentation, OR the reviewer's own code/specs, those contents contain the very placeholder tokens the reviewer substitutes. Re-expanding them corrupts the review AND can multiply the prompt past the model's context limit.

#### Scenario: A `{{diff}}` literal in a changed file is not expanded
- **WHEN** a `ReviewContext` whose changed files include a file whose contents contain the literal `{{diff}}` AND `{{changed_files}}` is rendered (e.g. the change under review edits the reviewer's own spec, which documents those tokens)
- **THEN** those literals appear verbatim in the rendered changed-files section
- **AND** the diff AND the changed-files block are each inserted exactly once, at the template's own placeholders
- **AND** the rendered prompt's size is bounded by `change_context + changed_files + diff + template` — it does NOT grow by the number of placeholder literals present in the changed files

#### Scenario: Ordinary reviews are unchanged
- **WHEN** a `ReviewContext` whose values contain no placeholder tokens is rendered
- **THEN** each of the four placeholders is substituted exactly once
- **AND** the rendered prompt is byte-identical to the prior chained-`.replace` rendering

### Requirement: Agentic reviewer mode
The reviewer SHALL support an `agentic` transport selected by `reviewer.kind: agentic` (the field defaults to `oneshot`, the existing HTTP path governed by the **AI-driven code-quality review** requirement). In agentic mode the reviewer runs through the shared `agentic_run` primitive (a56) as a CLI-wrapped session that reads files on demand and returns its verdict via the `submit_review` MCP tool, instead of pre-dumping every touched file into one prompt and scraping a `VERDICT:` line from the response.

The agentic session SHALL run in a read-only sandbox whose CLI tool permissions are `["Read", "Glob", "Grep"]` ONLY — NO `Bash`, NO `Write`, NO `Edit` — plus the `submit_review` MCP tool, with `ORCH_MCP_ROLE = reviewer`. The rendered prompt SHALL carry the change briefs, the unified diff, AND the list of changed file paths; it SHALL NOT pre-dump full file contents — the agent reads whatever files it needs via `Read`. Because there is no touched-file pre-dump, `reviewer.prompt_budget_chars` does NOT apply in agentic mode AND no `## Skipped (budget exhausted)` truncation occurs.

The agentic path SHALL produce the same `ReviewResult { verdict, per_concern, raw_output }` the one-shot path produces, so per_change dispatch, `auto_revise` revision comments, the operator re-review verb, AND the revision/re-review caps all operate unchanged. The path SHALL honor `reviewer.mode` (per_change → one session per change; bundled → one session per PR) identically to one-shot. `reviewer.command` (default `claude`) selects the CLI; a non-`claude` command resolves its strategy via the a55/a56 `provider → CLI` rule, AND a CLI with no registered strategy SHALL return a clear error naming it. The default `reviewer.kind` is `oneshot` because the `claude` strategy reaches only Anthropic-shaped endpoints; agentic review for other providers becomes available once their CLI strategy is registered.

#### Scenario: Agentic session runs in a read-only, no-Bash sandbox
- **WHEN** `reviewer.kind: agentic` AND a review runs
- **THEN** the session is spawned through `agentic_run` with a sandbox whose CLI tool permissions are exactly `["Read", "Glob", "Grep"]` plus the `submit_review` MCP tool, AND `ORCH_MCP_ROLE = reviewer`
- **AND** `Bash`, `Write`, AND `Edit` are NOT permitted

#### Scenario: Reads files on demand with no budget truncation
- **WHEN** the agentic reviewer renders its prompt from a `ReviewContext`
- **THEN** the prompt contains the change briefs, the unified diff, AND the changed-file path list, but NOT the full contents of those files
- **AND** the agent obtains file context by calling `Read` during the session
- **AND** `reviewer.prompt_budget_chars` is NOT consulted AND no `## Skipped (budget exhausted)` footer is produced

#### Scenario: Verdict and concerns return via submit_review
- **WHEN** the agentic reviewer finishes its analysis
- **THEN** it calls the `submit_review` MCP tool with `{ verdict: Approve | Block, summary, concerns: [...] }`
- **AND** after the session exits the daemon `consume_submission`s the payload (a56) into a `ReviewResult` whose `verdict` AND `per_concern` come from the submission AND whose `raw_output` is the rendered summary + concerns used for the PR-body `## Code Review` block

#### Scenario: No valid submission discards the review and alerts
- **WHEN** the agentic session ends without a schema-valid `submit_review` call (the agent never submits, OR every submission is schema-rejected)
- **THEN** the daemon DISCARDS the review: it writes NO verdict AND does NOT default to `Approve`
- **AND** it posts the reviewer-failure chatops alert so the operator can intervene
- **AND** this supersedes the one-shot rerun composer's verdict-default behavior for the agentic path

#### Scenario: Honors reviewer.mode identically to one-shot
- **WHEN** `reviewer.kind: agentic` AND `reviewer.mode: per_change` AND a PR bundles multiple changes
- **THEN** the reviewer runs one `agentic_run` session per change
- **AND** each session's `ReviewResult` feeds the same per_change disposition code the one-shot path uses
- **WHEN** `reviewer.mode` is the bundled default
- **THEN** the reviewer runs one session for the whole PR

#### Scenario: A reviewer command with no registered strategy returns a clear error
- **WHEN** `reviewer.kind: agentic` AND `reviewer.command` resolves (via the a55/a56 `provider → CLI` rule) to a CLI with no registered strategy
- **THEN** strategy resolution returns an error naming the CLI
- **AND** no review session is spawned

