# code-reviewer — delta for a64-reviewer-agentic-by-default

## MODIFIED Requirements

### Requirement: Agentic reviewer mode
The reviewer SHALL support an `agentic` transport selected by `reviewer.kind: agentic`. The `reviewer.kind` field SHALL default to `agentic`: now that the `opencode` strategy makes the agentic path provider-agnostic (a60), agentic is the preferred default for every provider, not only Anthropic-shaped ones. The `oneshot` HTTP path (governed by the **AI-driven code-quality review** requirement) remains available as an explicit opt-in AND as the automatic startup fallback described below. In agentic mode the reviewer runs through the shared `agentic_run` primitive (a56) as a CLI-wrapped session that reads files on demand and returns its verdict via the `submit_review` MCP tool, instead of pre-dumping every touched file into one prompt and scraping a `VERDICT:` line from the response.

The agentic session SHALL run in a read-only sandbox whose CLI tool permissions are `["Read", "Glob", "Grep"]` ONLY — NO `Bash`, NO `Write`, NO `Edit` — plus the `submit_review` MCP tool, with `ORCH_MCP_ROLE = reviewer`. The rendered prompt SHALL carry the change briefs, the unified diff, AND the list of changed file paths; it SHALL NOT pre-dump full file contents — the agent reads whatever files it needs via `Read`. Because there is no touched-file pre-dump, `reviewer.prompt_budget_chars` does NOT apply in agentic mode AND no `## Skipped (budget exhausted)` truncation occurs.

The agentic path SHALL produce the same `ReviewResult { verdict, per_concern, raw_output }` the one-shot path produces, so per_change dispatch, `auto_revise` revision comments, the operator re-review verb, AND the revision/re-review caps all operate unchanged. The path SHALL honor `reviewer.mode` (per_change → one session per change; bundled → one session per PR) identically to one-shot. `reviewer.command` (default `claude`) selects the CLI; a non-`claude` command resolves its strategy via the a55/a56 `provider → CLI` rule.

When the effective reviewer kind is `agentic` (whether defaulted OR set explicitly) but the resolved reviewer CLI is unavailable at startup — its strategy is not registered OR its binary is not found on the daemon host — the reviewer SHALL fall back to the `oneshot` HTTP path for that boot AND log ONE loud startup WARN naming the missing CLI AND the remedy (install it, OR set `reviewer.kind: oneshot` to silence the warning). The fallback SHALL NOT disable review: every provider has a working `oneshot` HTTP client, so a missing CLI degrades to HTTP review rather than no review. This keeps the default flip upgrade-safe — an operator whose reviewer points at a provider whose CLI is not installed keeps reviewing via HTTP until they install it. A daemon restart OR `autocoder reload` re-evaluates CLI availability.

#### Scenario: `reviewer.kind` defaults to agentic when the CLI is available
- **WHEN** `reviewer.kind` is unset AND the resolved reviewer CLI (default `claude`) is available at startup
- **THEN** the reviewer runs in agentic mode (the default)
- **AND** no fallback WARN fires

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

#### Scenario: Unavailable reviewer CLI falls back to oneshot
- **WHEN** the effective reviewer kind is `agentic` (defaulted OR explicit) AND the resolved reviewer CLI is unavailable at startup (its strategy is not registered OR its binary is not found on the daemon host)
- **THEN** the reviewer logs ONE loud startup WARN naming the CLI AND the remedy (install it, OR set `reviewer.kind: oneshot`)
- **AND** it uses the `oneshot` HTTP path for that boot — review continues AND is NOT disabled
- **AND** a daemon restart OR `autocoder reload` re-evaluates availability

#### Scenario: Explicit oneshot is honored as the opt-out
- **WHEN** `reviewer.kind: oneshot` is set explicitly
- **THEN** the reviewer uses the HTTP one-shot path AND no agentic session is spawned
- **AND** no fallback WARN fires (the operator chose `oneshot` deliberately)
