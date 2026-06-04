# code-reviewer — delta for a58-agentic-reviewer

## MODIFIED Requirements

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

## ADDED Requirements

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
