## Why

The `a01-pr-comment-revision-loop` change builds a revision dispatcher that processes `@<bot> revise <text>` PR comments — initially from human operators. The dispatcher does not care who authored the comment; it parses the body, executes the revision, posts a reply. That author-agnostic design was deliberate: it leaves room for bot-initiated revision requests to flow through the same plumbing.

The natural first bot to wire in is the code reviewer. Today the reviewer evaluates the diff between base and agent branches after the executor completes, returns a `Verdict { Pass | Concerns | Block }`, and writes a `## Code Review` section into the PR body. A `Block` verdict additionally makes the PR a draft. The reviewer's concerns are read-only — they sit in the PR body waiting for a human to either edit the code manually or close the PR.

A `Block` verdict identifies issues serious enough that the reviewer believes merging would cause real harm. Those are exactly the issues that warrant another revision cycle. If the reviewer can articulate the concern in actionable terms ("the `find_user` helper drops the error context the caller needs"), the revision dispatcher can act on it: re-invoke the executor with the concern as the revision request, commit the fix, force-push to the agent branch. The PR moves from "draft + Block + waiting for human" to "draft + revised + ready for human re-review" without the operator typing anything.

`Concerns` and `Pass` verdicts deliberately do not auto-trigger revisions. `Concerns` flags issues that warrant discussion but are mergeable as-is — auto-revising every one of those would generate constant churn for cosmetic preferences. `Pass` has nothing to revise. The operator can still manually trigger revisions on any verdict by commenting on the PR.

A second motivation: the reviewer-driven revision loop creates a tight feedback cycle between the reviewer's quality bar and the executor's revision capability. If the reviewer's prompt produces poor revision requests, operators see noisy reviewer-initiated comments and can tune the reviewer template. If the executor consistently fails to address valid reviewer concerns, operators see the loop hitting its cap and can investigate why. The signal is operator-visible in a way the current opaque `Block`-creates-draft semantics does not provide.

## What Changes

**Opt-in, off by default.** A new config field `reviewer.auto_revise_on_block: bool` (default `false`) gates the entire mechanism. Sites that have the reviewer enabled today see no behaviour change unless they explicitly flip the flag. Once flipped, every reviewer `Block` verdict posts revision-request comments instead of just leaving the verdict in the PR body. The verdict text in the PR body remains for human-readable summary.

**Reviewer output schema extension.** The reviewer's LLM prompt is updated to produce a richer per-concern shape:

```
- summary: short text describing the concern (already present today)
- actionable_request: optional text suitable for use as a revision-request body
- should_request_revision: bool (the reviewer decides per concern whether it has a clear actionable fix)
```

The reviewer's prompt template instructs: only set `should_request_revision: true` when the concern has a concrete, executable fix the implementer agent can apply without further clarification. Style preferences, philosophical disagreements, and "consider whether…" suggestions stay `false` — they're commentary, not revision requests. The reviewer's existing `## Code Review` PR-body section continues to list ALL concerns; only the `should_request_revision: true` ones additionally produce comments.

**Posting protocol.** For each Block-verdict concern where `should_request_revision: true`, the daemon posts a PR issue comment with body:

```
<!-- reviewer-revision -->
@<bot-username> revise <actionable_request>
```

The `<!-- reviewer-revision -->` HTML comment is a marker the revision dispatcher uses to bypass its self-author filter (without the marker, the dispatcher ignores comments authored by the bot itself, which is correct for the bot's own reply comments but wrong here).

**Self-author filter exception.** The revision dispatcher from `a01-pr-comment-revision-loop` filters bot-authored comments (`user.login == self.bot_username`) before parsing the trigger. This change adds an exception: bot-authored comments whose body starts with the literal string `<!-- reviewer-revision -->` skip the self-author filter and are parsed normally. This is the only sanctioned bypass; any other bot-authored comment continues to be filtered.

**Cap interaction.** Reviewer-initiated revisions count toward the same `executor.max_revisions_per_pr` cap as human-initiated ones. If the reviewer would generate more revision comments than the remaining cap allows, the daemon posts only the top-priority concerns up to the remaining budget (the LLM's output order is treated as priority order — the reviewer's prompt instructs it to list concerns most-critical-first). Concerns that would have been posted but were dropped for budget reasons are listed in the `## Code Review` PR body section with a `(not auto-revised; cap budget exhausted)` annotation so the human sees them.

**Order of operations within an iteration.**

1. Executor completes its change(s).
2. Commit + push happen as today.
3. Reviewer runs against the diff. Verdict + concerns returned.
4. PR is created (or updated) with the `## Code Review` body section as today. Draft state is set per the existing Block-makes-draft rule.
5. **New**: if `reviewer.auto_revise_on_block` is true AND the verdict is `Block`, for each `should_request_revision: true` concern (capped by remaining revision budget), post a `<!-- reviewer-revision -->` comment with the actionable request.
6. Iteration ends.
7. **Next iteration**: the revision dispatcher from `a01-pr-comment-revision-loop` picks up the reviewer-posted comments, executes each revision, commits + force-pushes, posts the standard `✅ Revision applied:` reply comments.
8. If subsequent reviewer passes are configured (out of scope for this change — the reviewer currently runs only at change-completion time), the loop continues until the cap is reached or the reviewer stops producing Block verdicts.

**Reviewer-failure handling.** If the reviewer LLM call fails (network, auth, parse failure), no revision comments are posted. The existing `(reviewer failed: <reason>)` text in the `## Code Review` section continues to be the only operator-facing signal. PR creation is unaffected.

**PR draft status under auto-revise.** A `Block` verdict still makes the PR a draft, even when auto-revise is on. The auto-revisions are a path TOWARD addressable, not toward auto-mergeable. The human still re-reviews the post-revision PR and decides to promote it from draft.

**No reviewer re-run after revision.** This change does NOT add re-running the reviewer after a reviewer-initiated revision lands. The reviewer's verdict is "frozen" at first evaluation. A future change could add re-evaluation if operators want it; for v1, one reviewer pass + revisions + human approval is the loop.

## Impact

- **Affected specs:** `code-reviewer` — one ADDED requirement covering the opt-in flag, the per-concern revision-request shape, the posting protocol with the `<!-- reviewer-revision -->` marker, the self-author filter exception, the cap-budget interaction, and the verdict-gating rule (`Block` only).
- **Affected code:**
  - `autocoder/src/config.rs` — add `reviewer.auto_revise_on_block: bool` (default `false`) to `ReviewerConfig`.
  - `autocoder/src/code_reviewer.rs` — extend the parsed-concern type with `actionable_request: Option<String>` and `should_request_revision: bool`. Update the default prompt template (`prompts/code-review-default.md`) with instructions that produce these fields in the reviewer's structured output. Update the response parser to populate them.
  - `autocoder/src/polling_loop.rs` — after the PR is created (or updated) and the reviewer has run, if `auto_revise_on_block && verdict == Block`, iterate the concerns and post `<!-- reviewer-revision -->` comments via the `post_issue_comment` helper from `a01-pr-comment-revision-loop`. Apply the cap-budget rule.
  - `autocoder/src/revisions.rs` (from `a01-pr-comment-revision-loop`) — extend `parse_revision_trigger` (or the dispatcher's self-author filter) to permit comments whose body begins with `<!-- reviewer-revision -->` to bypass the filter.
  - `prompts/code-review-default.md` — append a section instructing the LLM how to produce `actionable_request` and `should_request_revision` per concern.
  - Tests:
    - Parser tests for the extended response schema: well-formed concerns with both new fields parse correctly; concerns missing the new fields default to `actionable_request: None`, `should_request_revision: false` (backwards-compatible with reviewer templates that haven't been updated yet).
    - Cap-budget test: with 3 should-revise concerns and a remaining cap of 2, exactly the first 2 are posted and the third gets the `(not auto-revised; cap budget exhausted)` annotation in the PR body.
    - Self-author bypass test: a comment authored by the bot with body starting `<!-- reviewer-revision -->\n@<bot> revise foo` parses as a trigger with revision text `foo`; a comment authored by the bot WITHOUT the marker continues to be filtered.
    - Off-by-default test: with `auto_revise_on_block: false`, a `Block` verdict produces the existing behaviour (section in PR body, PR is draft) and posts ZERO revision comments.
    - Verdict-gating tests: `Pass` verdict posts no revision comments; `Concerns` verdict posts no revision comments; only `Block` triggers the loop.
    - End-to-end test using a stub reviewer that returns Block + two should-revise concerns: assert two `<!-- reviewer-revision -->` comments are posted to the PR with the correct bodies; assert the dispatcher in a subsequent iteration picks them up and executes revisions normally.

- **Operator-visible behavior:** sites with `auto_revise_on_block: false` see no change. Sites that flip it on see Block-verdict PRs gain reviewer-initiated revision comments → next iteration processes them → PR updates with revisions applied. The PR remains draft throughout; the human re-reviews and promotes.
- **Breaking:** no. The new config field defaults to `false`. Reviewer templates that haven't been updated to produce `actionable_request` / `should_request_revision` simply produce no revision requests (the defaults are `None` / `false`); operators who flip the auto-revise flag without updating the template see the existing behaviour and a daemon WARN at first reviewer run noting "no actionable_request fields found in reviewer response; auto-revise will not fire until the reviewer template is updated."
- **Acceptance:** `cargo test` passes (new + existing). A reviewer pass returning `Block` with two `should_request_revision: true` concerns posts exactly two `<!-- reviewer-revision -->` comments to the PR. The next polling iteration's revision dispatcher processes those comments and executes two revisions on the agent branch. The PR's commit history shows the original change commits + two revision commits.
