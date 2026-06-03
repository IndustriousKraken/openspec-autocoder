# Code Review

When the optional `reviewer:` config block is present and `enabled: true`, every PR opened by autocoder includes a structured AI-generated code-quality review under a `## Code Review` heading in the PR body. A `Block` verdict additionally causes the PR to be created as a draft.

## Scope

The reviewer's job is **code quality only**: security (injection, auth, secrets), error handling, naming/style/idioms, dead code, obvious bugs. It explicitly does **not** assess whether the diff implements the spec — that is a separate concern handled by the (future) verifier. The default prompt template (`prompts/code-review-default.md`) enforces this scope statement at the top.

## Configuring the reviewer

```yaml
reviewer:
  enabled: true
  provider: anthropic               # or `openai_compatible`
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY    # env var holding the API token
  # OR — inline alternative; when `api_key` is set, `api_key_env` is ignored.
  # api_key:
  #   value: "sk-ant-..."
  api_base_url: https://api.anthropic.com   # optional; provider default if omitted
  prompt_template_path: ./prompts/code-review-default.md  # optional; built-in default if omitted
```

The `openai_compatible` provider works with any endpoint that speaks the OpenAI `/chat/completions` API — Grok, OpenRouter, local Ollama, etc. Point `api_base_url` at the endpoint and provide a matching token via `api_key_env` (or `api_key` inline, see [Secrets in `config.yaml`](SECURITY.md#5-secrets-in-configyaml-inline-vs-env-var)).

## Verdict semantics

| Verdict     | PR state  | Meaning                                                                   |
|-------------|-----------|---------------------------------------------------------------------------|
| `Pass`      | non-draft | No concerns above style nits.                                              |
| `Concerns`  | non-draft | Issues warrant discussion but the diff is mergeable.                       |
| `Block`     | **draft** | At least one issue would cause real harm if merged.                        |

If the LLM's response cannot be parsed for a verdict, the daemon defaults to `Concerns` and prepends a parse-failure note to the report. If the API call itself errors (network, auth, rate limit), the daemon logs the error and still opens the PR with `(reviewer failed: <reason>)` in the `## Code Review` section. **A failed reviewer never blocks PR creation.**

## Block-verdict enforcement (recommended)

autocoder marks Block-verdict PRs as draft. To make this gate merge, configure a branch-protection rule on the PR target branch that **requires PRs not be draft**. Without that rule, anyone with write access can change the draft state and merge.

On hosts that don't support drafts (some private GHE configurations, certain repo types), autocoder falls back automatically: it retries the PR creation with `draft: false` and applies a `do-not-merge` label via the issues-labels endpoint. Configure your branch protection to require the absence of that label as the fallback gate.

## Review context

The reviewer receives a structured bundle, not just a diff. In priority order:

1. **Change context** — the proposal, optional design, and tasks of every OpenSpec change archived in this pass, so the reviewer understands the *intent* of the work.
2. **Changed files (full contents)** — every file touched by the pass, read at the agent-branch state. Whole-file context lets the reviewer evaluate trust boundaries, call sites, and helper definitions — work that a unified diff alone cannot support.
3. **Unified diff** — included last, if the prompt budget allows.

The combined prompt is capped by the configured `reviewer.prompt_budget_chars` (see [Prompt budget](#prompt-budget) below). Files are never partially truncated: if the next file would push the running total over budget, it is skipped in full and named in a `## Skipped (budget exhausted): ...` footer. When files are skipped, the diff is also dropped and replaced by an explanatory message. The default template instructs the model to acknowledge missing context in its first bullet under "Possible bugs" and bias toward `Concerns` over `Pass`.

This is a stopgap until the reviewer is upgraded to an MCP-tool-using model that can `Read`/`Grep` the codebase directly — for now, "send the whole touched surface" gives the reviewer enough information to do a real security review.

## Prompt budget

The reviewer's prompt-body cap is controlled by `reviewer.prompt_budget_chars` (default `2000000`). When the rendered prompt — change context + changed files + diff — would exceed this number of characters, files are skipped whole (in priority order) and the `## Skipped (budget exhausted): ...` footer is emitted.

There is **no hard upper bound** enforced by the daemon. Operators are responsible for matching this value to their LLM provider's actual context window:

- High-context providers (Grok-4, Claude Sonnet 4.6, etc., with 1M+ token windows) tolerate `4000000` chars or more — stop hitting truncation on bundled multi-change PRs touching large files.
- Smaller-window providers (some self-hosted Ollama deployments, older Claude models) need a tighter cap to fit the provider's real limit. Setting `1000000` (or whatever maps to your provider's actual window at the model's chars-per-token rate) avoids API-side rejects.
- Setting too high a value relative to the model's window will cause the LLM to return an error at request time; autocoder does not pre-validate this. Match it to your provider.

YAML integers do NOT accept underscore separators. Write the value as a plain decimal (`4000000`, not `4_000_000`); the latter parses as a string and autocoder rejects the config at load time.

The field is hot-applicable via `autocoder reload` (it lives in the `reviewer:` block, which the existing reload path picks up). Restart-free.

```yaml
reviewer:
  prompt_budget_chars: 4000000   # default 2000000
```

## Per-change reviewer mode

`reviewer.mode` controls how the reviewer dispatches against multi-change PRs:

- **`bundled`** (default) — one reviewer call per PR, with the prompt scoped to the union of every touched file from every change in the PR. This is today's behavior; the reviewer sees the whole pass at once.
- **`per_change`** — one reviewer call per change in the pass. Each call's prompt is scoped to that change's own diff plus the files that specific change touched. The PR body contains one `## Code Review: <change-slug>` section per change instead of a single combined `## Code Review` block.

Trade-offs:

- **LLM cost** scales linearly under `per_change`: an N-change PR costs N× the bundled-mode price. Pick `per_change` only if you specifically want per-change attention and are willing to pay for it.
- **Per-change budget** — each per-change call independently respects `prompt_budget_chars`. One change touching a huge file gets its own truncation footer without affecting the other changes' reviews.
- **Cross-change context preserved** — each per-change prompt includes a short preamble naming the OTHER changes in the same PR (slug + first paragraph of `## Why`, truncated to 200 chars each), so the reviewer sees that change A introduced a symbol change B consumes.
- **Reviewer-initiated revisions still aggregate** — the union of revision requests across all per-change reviews shares the same `executor.max_revisions_per_pr` cap. Dropped requests are annotated inside their own `## Code Review: <change-slug>` section.

```yaml
reviewer:
  mode: per_change                  # default `bundled`
```

The mode is hot-applicable via `autocoder reload`; flipping it between iterations causes the next PR to use the new mode.

## Reviewer-initiated revisions on actionable concerns

When `reviewer.auto_revise: true` is set, autocoder forwards the actionable concerns to the same revision dispatcher that handles operator `@<bot> revise ...` comments. The trigger is the per-concern actionability signal, **not** the verdict: it fires on actionable concerns regardless of whether the review's verdict is `Pass`, `Concerns`, or `Block`. (`Block` retains its separate effect of marking the PR draft; it just no longer gates auto-revise.)

The legacy config key `auto_revise_on_block` is still accepted as a silent alias, so existing config files load unchanged. The flow:

1. Reviewer returns any verdict with one or more per-concern records marked `should_request_revision: true` and a non-empty `actionable_request`.
2. Autocoder posts one PR issue comment per such concern, with body:

   ```
   <!-- reviewer-revision -->
   @<bot-username> revise <actionable_request>
   ```

   The leading HTML-comment marker (`<!-- reviewer-revision -->`) is the dispatcher's self-author-filter bypass — without it, the dispatcher would (correctly) treat the comment as bot-authored noise and drop it.
3. On the next polling iteration, the [PR-comment revision dispatcher](OPERATIONS.md#revising-an-open-pr-via-comment) picks up each comment, runs the executor in revision mode, commits + force-pushes, and posts the standard `✅ Revision applied:` / `✗ Revision attempt failed:` reply.

The feature is **off by default**. A reviewer template that has not been updated to emit the structured `revision-requests` YAML block (see below) silently produces no comments; a daemon `WARN` log surfaces this case on first reviewer run when the flag is enabled but no actionable concerns appear.

### Per-concern revision decision

The reviewer makes the per-concern decision: only concerns with a concrete, executable fix the implementer agent can apply without further clarification should set `should_request_revision: true`. Style preferences, philosophical disagreements, and "consider whether…" suggestions stay `false` — they are commentary, not revision requests. The default prompt template (`prompts/code-review-default.md`) documents this rule in detail.

### Cap-budget interaction

Reviewer-initiated revisions count toward the same per-PR `executor.max_revisions_per_pr` cap as human-initiated ones (default 5; see [CONFIG.md](CONFIG.md#max_revisions_per_pr)). When the reviewer would post more comments than the remaining cap allows, autocoder posts the first N (the reviewer's prompt template instructs it to list concerns most-critical-first) and annotates the dropped concerns in the `## Code Review` PR-body section with `(not auto-revised; cap budget exhausted)` so the human reviewer sees what was skipped.

The cap budget at posting time is a forward-looking estimate; the actual `revisions_applied` counter only increments when the dispatcher processes a comment on a subsequent iteration. Posting failures (transient GitHub errors) are logged at `WARN` per concern and do not abort the iteration — the PR is still created/updated, just without those comments.

### Operator-customized reviewer templates

If you have overridden `reviewer.prompt_template_path` with a custom template that pre-dates this change, the template will need to be updated to emit the structured `revision-requests` block at the end of the response. The block shape is:

````
```revision-requests
- summary: "find_user drops the error context"
  actionable_request: "fix find_user to propagate the underlying error via anyhow::Context"
  should_request_revision: true
- summary: "consider renaming `tmp` to something more descriptive"
  should_request_revision: false
```
````

The fenced block tag is the literal string `revision-requests`; the body is a YAML list with one entry per concern surfaced in the markdown sections above. See the default template `prompts/code-review-default.md` for the full instructions you can copy in.

### Verdict gating

Only `Block` verdicts trigger reviewer-initiated revisions. `Pass` and `Concerns` verdicts deliberately do not auto-revise:

- `Concerns` flags issues that warrant discussion but are mergeable as-is. Auto-revising every one of those would generate constant churn for cosmetic preferences.
- `Pass` has nothing to revise.

The operator can still manually trigger revisions on any verdict by posting `@<bot> revise <text>` as a regular PR comment.

### No reviewer re-run

The reviewer runs exactly once per polling iteration's executor pass. A reviewer-initiated revision committed in a later iteration does NOT trigger a re-evaluation by the reviewer; the verdict from the original pass is "frozen" for the life of the PR. The PR's draft status (set by the original `Block` verdict) is preserved through the revision cycle — the human re-reviews the post-revision PR and decides to promote it from draft.

## Custom prompt templates

Override the default with `reviewer.prompt_template_path`. Custom templates are **user-owned** — autocoder does not enforce scope on overrides, so you can expand the reviewer to additional dimensions (spec compliance, style guide, etc.) by editing the template.

The template must include the three substitution variables `{{change_context}}`, `{{changed_files}}`, and `{{diff}}`, and must instruct the model to begin its response with `VERDICT: Pass`, `VERDICT: Concerns`, or `VERDICT: Block`. A template still using the retired `{{change_summary}}` placeholder (pre-`reviewer-full-file-context`) will not substitute — the literal text appears in the rendered prompt. Migrate by replacing `{{change_summary}}` with `{{change_context}}`.

## PR composition

Every PR autocoder opens carries the change list in its body, plus the optional `## Code Review` section described above. Immediately after creation, autocoder posts a single follow-up issue comment titled `## Agent implementation notes` with one `### <change-name>` subsection per change in the pass. Each subsection contains the implementer agent's captured stdout from that change's run — the agent's own narrative of what it did, deviations from the spec, and any meta-observations.

The comment is best-effort: if the POST fails, the PR still ships and the failure is logged at ERROR. Source for each section is `<system-temp>/autocoder/logs/<workspace-basename>/<change>.log` (the same per-change log file written by the executor); a missing or unreadable log is logged at WARN and that change's section is omitted. If every change's log is missing, no comment is posted.

The total comment body is capped at 60,000 characters (under GitHub's 65,535 limit, with headroom for wrapper text). When truncated, the tail is replaced with a marker pointing back at `/tmp/autocoder/logs/<basename>/<change>.log` so reviewers can fetch the full output server-side.

