## MODIFIED Requirements

### Requirement: AI-driven code-quality review
The code-reviewer SHALL accept a structured `ReviewContext` containing the archived-change briefs, full contents of every file modified by the pass, and the unified diff, then send a rendered prompt to a configured LLM API and return a `ReviewReport { verdict, markdown }`. The review SHALL focus on code quality (security, error handling, naming, style, language idioms, obvious bugs) and SHALL NOT assess whether the diff correctly implements any spec — that is a separate verification concern handled in its own change.

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
  total still fits inside the 2,000,000-character budget after the
  prior two sections; otherwise replaced with the literal text
  `(diff omitted: budget exhausted by change context and changed files)`

#### Scenario: Budget exhaustion mid-files
- **WHEN** the cumulative byte size of change context plus changed
  files exceeds 2,000,000 characters
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

