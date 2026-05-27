# code-reviewer Specification

## Purpose
TBD - created by archiving change reviewer-integration. Update Purpose after archive.
## Requirements
### Requirement: AI-driven code-quality review
The code-reviewer SHALL accept a structured `ReviewContext` containing the archived-change briefs, full contents of every file modified by the pass, and the unified diff, then send a rendered prompt to a configured LLM API and return a `ReviewReport { verdict, markdown }`. The review SHALL focus on code quality (security, error handling, naming, style, language idioms, obvious bugs) and SHALL NOT assess whether the diff correctly implements any spec — that is a separate verification concern handled in its own change. The reviewer's prompt-budget cap (the threshold past which touched-file context is truncated with a `## Skipped (budget exhausted): ...` footer) SHALL read from `reviewer.prompt_budget_chars` in `config.yaml`. The default value SHALL be `2_000_000` characters, preserving today's behavior verbatim for operators who do not set the field. There is no hard upper bound — the operator is responsible for matching the value to their LLM provider's actual context window.

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
- **WHEN** the operator sets `reviewer.prompt_budget_chars: 4_000_000`
- **AND** the reviewer is invoked against a pass whose touched-file content is 3,000,000 characters total
- **THEN** the reviewer's prompt fits the full context (no truncation)
- **AND** no `## Skipped (budget exhausted): ...` footer fires

#### Scenario: Cap is hot-applicable via `autocoder reload`
- **WHEN** the operator changes `reviewer.prompt_budget_chars` in `config.yaml` AND runs `autocoder reload`
- **THEN** the daemon's reload handler applies the new value at the next iteration's reviewer invocation
- **AND** the existing `reviewer:` hot-reload path picks up the change without a daemon restart

### Requirement: Default prompt template enforces code-quality scope
The code-reviewer SHALL ship a default prompt template that explicitly limits the review to code-quality concerns and instructs the LLM not to assess spec compliance. The template SHALL use the `{{change_context}}`, `{{changed_files}}`, and `{{diff}}` placeholders.

#### Scenario: Default template is shipped with the binary
- **WHEN** autocoder binary is built
- **THEN** a file named `prompts/code-review-default.md` is included
  in the project repository at the relative path
  `prompts/code-review-default.md`
- **AND** the template's text contains the literal scope statement:
  `"You are reviewing code quality only. Do NOT assess whether the diff implements the spec; that is handled separately by the verifier step."`
- **AND** the template specifies the required response format: a
  verdict line followed by markdown bullets
- **AND** the template references all three placeholders
  (`{{change_context}}`, `{{changed_files}}`, `{{diff}}`) at least
  once

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

### Requirement: Reviewer-initiated revision comments on Block verdicts
When `reviewer.auto_revise_on_block` is `true` AND the reviewer returns a `Block` verdict, the daemon SHALL post one PR issue comment per concern where the reviewer marked `should_request_revision: true`, subject to the per-PR revision-cap budget. Each comment's body SHALL begin with the marker line `<!-- reviewer-revision -->` followed by a newline, then the trigger pattern `@<bot-username> revise <actionable_request>`. The marker enables the revision dispatcher's self-author-filter bypass; without it the dispatcher would (correctly) filter the comment as bot-authored noise. The feature is off by default; the config flag must be explicitly enabled.

#### Scenario: Off-by-default has no behavioural change
- **WHEN** `reviewer.auto_revise_on_block` is absent OR `false` AND the reviewer returns any verdict
- **THEN** no reviewer-revision comments are posted
- **AND** the existing PR-body `## Code Review` section is the only reviewer output channel

#### Scenario: Block verdict with should-revise concerns posts comments
- **WHEN** `auto_revise_on_block: true` AND the reviewer returns `Block` AND the response contains two concerns with `should_request_revision: true` and non-empty `actionable_request` AND the per-PR remaining cap budget is at least 2
- **THEN** exactly two PR issue comments are posted
- **AND** each comment's body starts with `<!-- reviewer-revision -->\n`
- **AND** each comment's body's second non-whitespace line matches `@<bot-username> revise <actionable_request>` for that concern

#### Scenario: Pass and Concerns verdicts post no revision comments
- **WHEN** `auto_revise_on_block: true` AND the reviewer returns `Pass` OR `Concerns`
- **THEN** no reviewer-revision comments are posted
- **AND** the existing PR-body section behaviour is unchanged

#### Scenario: Concerns without should_request_revision post no comments
- **WHEN** `auto_revise_on_block: true` AND the reviewer returns `Block` AND every concern has `should_request_revision: false`
- **THEN** no reviewer-revision comments are posted
- **AND** the daemon logs a WARN noting that auto-revise is enabled but the reviewer produced no actionable-revision concerns (signals operator that the reviewer template may need updating)

### Requirement: Cap-budget interaction with reviewer-posted comments
The reviewer-posting step SHALL respect the per-PR `executor.max_revisions_per_pr` cap. When the reviewer would generate more should-revise concerns than the remaining cap budget allows, the daemon SHALL post only the first N concerns (where N = remaining budget; concerns are taken in the reviewer's output order, which the reviewer's prompt template instructs to be most-critical-first) AND SHALL annotate the dropped concerns in the PR-body `## Code Review` section so the human sees what was skipped.

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
Operators with customized reviewer templates that have NOT been updated to emit the new `actionable_request` and `should_request_revision` per-concern fields SHALL see no behavioural change: the response parser defaults missing fields to `actionable_request: None` and `should_request_revision: false`, the posting step finds zero should-revise concerns, and posts zero reviewer-revision comments. The daemon SHALL log a one-shot WARN on the first reviewer-pass in such a session when `auto_revise_on_block` is enabled, naming the gap so operators see the actionable diagnostic.

#### Scenario: Customized template missing the new fields produces no comments
- **WHEN** the reviewer's response (from an operator-customized template that pre-dates this change) contains concerns without `should_request_revision` fields AND `auto_revise_on_block: true` AND the verdict is Block
- **THEN** the parser defaults `should_request_revision: false` for every concern
- **AND** zero reviewer-revision comments are posted
- **AND** the daemon logs a WARN naming the gap and pointing at the prompt-template documentation

### Requirement: No reviewer re-run after a reviewer-initiated revision lands
The reviewer SHALL run exactly once per polling iteration's executor pass, as today. A reviewer-initiated revision committed in a subsequent iteration SHALL NOT trigger a re-evaluation by the reviewer; the verdict from the original pass is "frozen" for the life of the PR. Operators wanting iterative reviewer evaluation can manually re-issue the iteration (e.g. via `autocoder rewind` or by closing + re-opening the PR), or wait for a separate change that adds reviewer re-evaluation as an explicit feature.

#### Scenario: Reviewer does not re-run when a revision lands
- **WHEN** a reviewer-initiated revision is committed and force-pushed in iteration N+1
- **THEN** the reviewer is NOT invoked again in iteration N+1
- **AND** the existing `## Code Review` section in the PR body is not updated
- **AND** the PR's draft status (set by the original Block verdict) is preserved

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

