## Why

Pull requests autocoder opens are often close-but-not-quite. A common pattern:

- A test fails because a system library wasn't installed on the runner; the operator installs it on the server but the PR is now slightly stale on a test that didn't pass.
- A function autocoder wrote drops error information the operator needs for debugging; the change is conceptually correct but the implementation has a small mis-judgment.
- A type or naming choice doesn't match the operator's house style; perfectly functional but stylistically off.

Today the operator's options are:
1. **Close the PR + edit the spec + wait for the next iteration** — heavyweight; the spec edit is often the wrong tool because the original spec was fine, just the implementation isn't.
2. **Merge as-is + fix locally** — bypasses the autocoder workflow; the operator ends up doing the very thing autocoder was built to avoid.

Neither matches the way humans collaborate. With another human, the reviewer would leave a PR comment and the author would push a revision. The conversation happens in the natural place — on the PR itself, next to the code in question.

A revision channel built on PR comments closes this gap. The operator comments `@<bot-username> revise <natural-language request>`; the daemon picks up the comment on its next polling pass, re-invokes the executor with the original change context plus the PR diff plus the revision text, commits the revision, force-pushes to the agent branch, and posts a reply comment summarising what changed. The PR diff updates in place. The conversation is preserved in the PR's comment timeline.

A second motivation worth naming, even though it's out of scope for v1: the same revision channel can carry **bot-initiated** revision requests in the future. The code reviewer that currently posts a verdict + concerns into the PR body can, in a future phase, post each concern as a structured revision comment. The revision loop processes both human-authored and reviewer-authored comments uniformly. The plumbing is shared. This change establishes that plumbing without committing to the future phase.

## What Changes

**Polling-based PR-comment detection.** Each polling iteration, before processing pending changes for a repo, fetches every open pull request whose head branch matches `repositories[].agent_branch` (the set of PRs autocoder created and has not yet seen merged). For each, the daemon issues `GET /repos/{owner}/{repo}/issues/{number}/comments?since=<last-seen-timestamp>` and inspects the resulting comments for the trigger pattern. State (last-seen-timestamp + revision-count per PR) is persisted under `<workspace>/.autocoder/revisions/<pr-number>.json`. No webhook server is added; no public-facing surface is needed.

**Trigger pattern.** A comment whose body's first non-whitespace token is `@<bot-username>` AND whose next token (case-insensitive) is `revise` is a revision request. The remainder of the body, after stripping the mention and the verb, is the revision text. Comments whose mention is the bot but whose verb is not `revise` are ignored (`@<bot> looks good` is conversational, not a command). Bot-authored comments (the daemon's own replies) are filtered out by `user.login == self_bot_username`.

The bot's GitHub username is learned at startup via `GET /user` (the GitHub equivalent of Slack's `auth.test`) and cached for the process lifetime, mirroring the existing Slack bot_user_id learning pattern.

**Revision execution.** When a revision request is detected for an open PR:

1. The per-repo polling task acquires the same lock the rest of the iteration uses, so revisions block (and are blocked by) other in-flight work on that repo (serial-per-repo invariant).
2. The workspace is fetched + checked out to the agent branch (already the case after PR creation; this is a no-op refresh).
3. The PR diff is captured (`git diff <base_branch>..<agent_branch>` against the workspace state).
4. The executor is invoked with a revision-mode context bundle: original change material (proposal/tasks/specs, located via the existing helpers), the captured PR diff, the operator's revision text. A new prompt template variant (or a templated revision section appended to the existing implementer template) instructs the executor to make targeted edits.
5. On `Completed`, the daemon commits the revision with subject `revise: <change-slug>: <first 60 chars of revision text>`, force-pushes with `--force-with-lease` to the agent branch (the existing push helper). The PR's diff updates automatically.
6. The daemon posts a reply comment on the PR: `✅ Revision applied: <one-line summary of the executor's commit subject>. Revision count: <N> of <cap>.`
7. On `AskUser`, the existing chatops escalation fires; the PR sits in revision-waiting state until the human answers in the escalation thread, then the next iteration resumes.
8. On `Failed`, the daemon posts: `✗ Revision attempt failed: <reason>. The PR is unchanged. Reply with another \`@<bot> revise ...\` to retry, or close the PR if the request cannot be satisfied.` The revision-count counter still increments (a failed attempt counts toward the cap so a runaway loop of failures doesn't keep firing).

**Revision cap per PR.** Default 5 rounds, configurable via a new `executor.max_revisions_per_pr: u32` field (cap at some upper bound like 20 to prevent runaway operator configurations). When the cap is reached:

1. The daemon posts a one-time decline comment on the PR: `🛑 Revision cap reached (<N> revisions). Further \`@<bot> revise\` requests on this PR will be ignored. Close + re-open or merge as-is.`
2. A chatops notification fires: `🛑 <repo>: PR #<num> hit the revision cap of <N>. Further revision requests ignored.`
3. Subsequent revision comments on the PR are silently ignored — the timestamp is still advanced so they don't get re-processed, but neither a PR comment nor a chatops message fires for each. The one-time decline comment is the operator's only signal.

**Priority over pending changes.** Within a repo's polling iteration, revision requests are processed BEFORE any pending changes from `openspec/changes/`. The reasoning: revisions are operator-initiated time-sensitive feedback on work already in flight; processing them first respects the human's tempo. After all revision requests are processed (or the queue is empty), the iteration proceeds to its normal pending-change walk.

**State file: `<workspace>/.autocoder/revisions/<pr-number>.json`.** Shape:

```json
{
  "pr_number": 42,
  "agent_branch": "agent-q",
  "last_seen_comment_at": "2026-05-25T21:30:00Z",
  "revisions_applied": 2,
  "revision_cap": 5,
  "cap_decline_posted": false
}
```

`last_seen_comment_at` is the `created_at` of the most recent comment the iteration processed (whether it triggered a revision or was ignored). On next iteration, `?since=<last_seen_comment_at>` skips already-processed comments. When the PR is merged or closed, the state file is removed (the polling task notices the PR's `state` field).

**Stale state cleanup.** At iteration start, before fetching comments, prune state files whose corresponding PR is no longer open (merged or closed). This keeps the state directory bounded and avoids stale entries accumulating after every merged PR.

**Trust boundary.** Anyone with GitHub write access to the repo can post PR comments and therefore trigger revisions. This matches the existing chatops channel trust boundary. Sites with stricter governance use GitHub's branch-protection / required-reviewers features; autocoder does not add a per-revision authorization layer.

## Impact

- **Affected specs:** `orchestrator-cli` — one ADDED requirement covering revision detection, execution, the cap, the priority-over-pending rule, the state-file shape, and the bot-reply contract.
- **Affected code:**
  - `autocoder/src/github.rs` — add `pub fn self_bot_username(token: &str) -> Result<String>` (calls `GET /user`); add `pub fn list_open_prs_for_head(owner, repo, head_branch) -> Result<Vec<PrSummary>>`; add `pub fn list_issue_comments_since(owner, repo, pr_number, since) -> Result<Vec<IssueComment>>`; add `pub fn post_issue_comment(owner, repo, pr_number, body) -> Result<()>`. All use the existing PAT-routing logic.
  - New module `autocoder/src/revisions.rs` housing the per-PR state file IO, the trigger-comment parser, and the revision dispatcher.
  - `autocoder/src/polling_loop.rs::run` — before the existing pending-change walk, call a new `process_revision_requests(workspace, repo, github_cfg, executor, chatops_ctx, cancel) -> Result<()>` that does the per-PR comment fetch + revision-or-skip per PR.
  - `autocoder/src/executor/claude_cli.rs` — extend the prompt-build path with a `RevisionContext { original_change_material, pr_diff, revision_text }` variant. New prompt template file (or a `## Revision request` section appended to the existing implementer template). The `{{change_body}}` placeholder gains a sibling `{{revision_request}}` placeholder that the revision-mode path substitutes; the normal-mode path leaves it absent.
  - `autocoder/src/config.rs` — add `executor.max_revisions_per_pr: u32` (default 5, max 20 with WARN-and-clamp at config load).
  - `autocoder/src/git.rs` — add `pub fn diff_branch_against(workspace, base_branch, target_branch) -> Result<String>` for the PR-diff capture. Reuses existing `run_git` helper.
  - Tests:
    - Parser unit tests: comments with `@<bot> revise <text>` parse correctly; comments with `@<bot> something-else` are ignored; comments without the bot mention are ignored; bot-authored comments are filtered; case-insensitivity on the verb; multi-line revision text is captured wholesale.
    - State-file unit tests: create / read / increment-revisions / cap-reached / prune-on-closed-pr.
    - Trigger-detection integration tests (mockito-driven): a fixture PR with a triggering comment is detected; a PR with no triggering comment is skipped; a PR with the bot's own reply comment is not re-triggered; the `since` filter skips already-processed comments.
    - Revision-execution tests with a stub executor: `Completed` → commit + force-push + reply comment; `AskUser` → escalate to chatops; `Failed` → reply comment + count increments.
    - Cap-enforcement test: after `max_revisions_per_pr` revisions on one PR, the next triggering comment is silently ignored AND the one-time decline comment + chatops notification fires.
    - Priority test: a fixture with both a pending change AND a revision request on an open PR processes the revision FIRST, then the pending change.
    - Stale-state cleanup test: a state file whose corresponding PR is closed is removed at iteration start.

- **Operator-visible behavior:** operators can comment `@<bot> revise <free-text>` on autocoder-opened PRs to trigger an in-place revision. The bot replies via PR comment when done (success or failure). The PR's diff updates without the operator closing or re-opening the PR. Per-PR revision count caps at 5 by default.
- **Breaking:** no. PRs that receive no revision comments behave exactly as today. The new behaviour is opt-in by operator action.
- **Acceptance:** `cargo test` passes (new + existing). An operator commenting `@<bot> revise <text>` on an open autocoder-opened PR causes the next polling iteration to execute the revision against the agent branch, force-push the new commit, and post a `✅ Revision applied: ...` reply comment within ~1 polling interval. The PR's diff reflects the revision. A subsequent operator comment triggers a second revision normally. After 5 revisions, a 6th triggers the cap-decline comment and chatops notification.
