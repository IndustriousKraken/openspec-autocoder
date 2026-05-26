## 1. GitHub API helpers

- [x] 1.1 Add `pub async fn self_bot_username(api_base: &str, token: &str) -> Result<String>` in `autocoder/src/github.rs`. Calls `GET {api_base}/user` with `Authorization: token <token>`, parses the `login` field. Returns the bot's GitHub username. Errors propagate with the HTTP-status / response-body context the existing helpers use.
- [x] 1.2 Add `pub async fn list_open_prs_for_head(api_base, token, owner, repo, head_branch) -> Result<Vec<PrSummary>>`. Calls `GET {api_base}/repos/{owner}/{repo}/pulls?head={owner}:{head_branch}&state=open&per_page=100`. Returns `PrSummary { number, title, head_branch, base_branch, state, url }`. (Pagination beyond 100 is out of scope — a repo with >100 open PRs from a single agent branch is unrealistic.)
- [x] 1.3 Add `pub async fn list_issue_comments_since(api_base, token, owner, repo, pr_number, since: DateTime<Utc>) -> Result<Vec<IssueComment>>`. Calls `GET {api_base}/repos/{owner}/{repo}/issues/{pr_number}/comments?since=<RFC3339>&per_page=100`. Returns `IssueComment { id, body, user_login, created_at }`. (Pagination beyond 100 is rare for short revision cycles; a follow-up can paginate if real-world usage produces it.)
- [x] 1.4 Add `pub async fn post_issue_comment(api_base, token, owner, repo, pr_number, body) -> Result<()>`. Calls `POST {api_base}/repos/{owner}/{repo}/issues/{pr_number}/comments` with JSON `{"body": "<body>"}`. Returns Err with the HTTP-status / response-body on non-2xx.
- [x] 1.5 Tests (mockito-driven): happy path + 404 + 5xx + auth failure for each helper. The helpers use the existing PAT-routing via `github_token_for_url` / `owner_tokens` map.

## 2. State-file IO

- [x] 2.1 Create `autocoder/src/revisions.rs`. Public types:
  ```rust
  pub struct RevisionState {
      pub pr_number: u64,
      pub agent_branch: String,
      pub last_seen_comment_at: DateTime<Utc>,
      pub revisions_applied: u32,
      pub revision_cap: u32,
      pub cap_decline_posted: bool,
  }
  pub fn state_path(workspace: &Path, pr_number: u64) -> PathBuf;
  pub fn read_state(workspace: &Path, pr_number: u64) -> Result<Option<RevisionState>>;
  pub fn write_state(workspace: &Path, state: &RevisionState) -> Result<()>;
  pub fn remove_state(workspace: &Path, pr_number: u64) -> Result<()>;
  pub fn prune_closed_prs(workspace: &Path, open_pr_numbers: &HashSet<u64>) -> Result<usize>;
  ```
  Path convention: `<workspace>/.autocoder/revisions/<pr_number>.json`. Atomic writes via temp-file-then-rename to match the daemon's existing state-file pattern.
- [x] 2.2 Tests:
  - Read missing state file returns `Ok(None)`.
  - Write then read round-trips every field.
  - Prune removes state files whose PR number is not in the open set.
  - Atomic write tolerates an interrupted write (partial temp file not promoted to the canonical path).

## 3. Trigger-comment parser

- [x] 3.1 In `autocoder/src/revisions.rs`, add `pub fn parse_revision_trigger(comment_body: &str, bot_username: &str) -> Option<String>`. Returns `Some(revision_text)` when:
  - The first non-whitespace token of `comment_body` is `@<bot_username>` (case-insensitive on the username segment because GitHub usernames are case-insensitive at the platform level).
  - The second token (whitespace-separated, case-insensitive) is `revise`.
  - There is at least one non-whitespace character after `revise`.
  The returned `revision_text` is everything after `revise` with leading whitespace trimmed; trailing whitespace also trimmed but newlines internal to the text are preserved.
- [x] 3.2 Filter: comments whose `user_login == bot_username` are ignored at the dispatcher layer (not at the parser); the parser is a pure-text function.
- [x] 3.3 Tests:
  - `@bot revise foo` → `Some("foo")`.
  - `@bot REVISE foo` → `Some("foo")` (case-insensitive verb).
  - `@BOT revise foo` with `bot_username = "bot"` → `Some("foo")` (case-insensitive mention).
  - `@bot foo` → `None` (verb is not `revise`).
  - `@bot revise` → `None` (no text).
  - `@bot revise   ` → `None` (no non-whitespace text).
  - `foo @bot revise bar` → `None` (mention not at start).
  - Multi-line: `@bot revise line1\n\nline2` → `Some("line1\n\nline2")`.

## 4. Config: revision cap

- [x] 4.1 In `autocoder/src/config.rs`, extend `ExecutorConfig` with `pub max_revisions_per_pr: u32` defaulting to `5` via `#[serde(default = "default_max_revisions_per_pr")]`.
- [x] 4.2 Clamp values above `20` to `20` with a WARN log at startup (same shape as the audit-retries clamp from `audit-proposal-self-validation`).
- [x] 4.3 Tests: default → 5; explicit 0 → 0 (no revisions allowed; useful for sites that want to disable the feature); explicit 20 → 20 no WARN; explicit 50 → 20 with WARN.

## 5. Revision-mode prompt template

- [x] 5.1 Add a new template file `prompts/implementer-revision.md` (or extend `prompts/implementer.md` with a templated `## Revision Request` section that the revision-mode path renders). The template instructs the executor to make targeted edits in response to the operator's revision text, using the original change material AND the PR diff as context.
- [x] 5.2 Required placeholders in the revision template:
  - `{{change_body}}` — the original change's `openspec instructions apply` output (same as normal-mode)
  - `{{pr_diff}}` — `git diff <base>..<agent>` against the workspace state
  - `{{revision_request}}` — the operator's revision text verbatim
- [x] 5.3 The executor's prompt-build path branches: normal mode (existing) vs revision mode (new). The branching key is whether a `RevisionContext` is present in the executor call.
- [x] 5.4 Tests: revision-mode prompt build with a sample `RevisionContext` produces a rendered prompt containing all three substitution sections in the documented order.

## 6. Executor-call revision context

- [x] 6.1 Extend `crate::executor::Executor::run`'s signature OR add a new method `Executor::run_revision(workspace, change_name, revision_context) -> Result<ExecutorOutcome>`. The new-method approach avoids changing existing callers. Implementation in `ClaudeCliExecutor::run_revision`: build the revision-mode prompt, spawn the CLI with the same sandbox/timeout config, return the outcome same as `run`.
- [x] 6.2 Tests: stub Executor that returns `Completed` for a revision context produces the same outcome as for a regular run; stub that returns `AskUser` produces the same escalation path; stub that returns `Failed` produces a `Failed` outcome.

## 7. Revision dispatcher

- [x] 7.1 Add `pub async fn process_revision_requests(workspace: &Path, repo: &RepositoryConfig, github_cfg: &GithubConfig, executor: &dyn Executor, chatops_ctx: Option<&ChatOpsContext>, cancel: CancellationToken) -> Result<()>` in `autocoder/src/revisions.rs`. Flow:
  1. Resolve `(owner, repo_name)` from `repo.url` via the existing `github::parse_repo_url`.
  2. Resolve PAT via the existing `github_token_for_url`.
  3. Call `self_bot_username` (cached at startup or per-call — implementer's choice; caching is the long-term right thing).
  4. Call `list_open_prs_for_head(.., repo.agent_branch)`.
  5. Build `open_pr_numbers: HashSet<u64>`. Call `prune_closed_prs(workspace, &open_pr_numbers)`.
  6. For each open PR:
     a. Read existing `RevisionState` (or initialize a fresh one with `last_seen_comment_at = pr.created_at`).
     b. If `cap_decline_posted` is true AND `revisions_applied >= revision_cap`, skip the PR entirely (one-time decline already issued).
     c. Call `list_issue_comments_since(.., since=state.last_seen_comment_at)`.
     d. Filter out comments from `self_bot_username`. Update `state.last_seen_comment_at` to the most recent comment's `created_at` (whether or not any are revision triggers).
     e. For each remaining comment, parse with `parse_revision_trigger(comment.body, bot_username)`.
     f. For each triggering comment, in chronological order:
        - If `state.revisions_applied >= state.revision_cap`: post the one-time cap-decline comment (if not yet posted), set `cap_decline_posted = true`, post the chatops notification, break (silently ignore remaining comments).
        - Otherwise: execute the revision (see step 7).
  7. Revision execution:
     a. Fetch + checkout `agent_branch`.
     b. Capture PR diff via `git diff <base>..<agent>`.
     c. Build `RevisionContext` from original change material (located via existing helpers) + PR diff + revision text.
     d. Call `executor.run_revision(workspace, change_name, revision_context)`.
     e. On `Completed`: `git add -A`, `git commit -m "revise: <change>: <first 60 chars of revision text>"`, `git::push_force_with_lease(workspace, agent_branch, origin)`. Post `✅ Revision applied: <subject>. Revision count: <N> of <cap>.` PR comment. Increment `state.revisions_applied`. Write state.
     f. On `AskUser`: existing chatops escalation path fires; the revision is treated as in-progress (no commit, no count increment, no PR reply yet). The next iteration's revision dispatcher will re-process the same comment once the human answer arrives — handled by NOT advancing `last_seen_comment_at` past this comment. (Alternative: advance the timestamp and rely on the AskUser thread state to drive resumption; choose whichever fits the existing AskUser plumbing better.)
     g. On `Failed`: post `✗ Revision attempt failed: <reason>. The PR is unchanged. ...` PR comment. Increment `state.revisions_applied`. Write state.
- [x] 7.2 Identifying which change to revise: the PR was opened with a body listing changes; for v1, assume one revision request acts on the most-recently-archived change in this PR. (Multi-change revisions are a future enhancement.) Use the existing `extract_change_list_from_pr_body` helper, or recompute via the in-flight change directory. Falling back: the first change name listed in the PR body.

## 8. Wire into the polling iteration

- [x] 8.1 In `autocoder/src/polling_loop.rs::run`, after the existing waiting-changes processing AND before the pending-changes walk, call `process_revision_requests`. On error, log at WARN and continue (a revision-detection failure should not block the rest of the iteration).
- [x] 8.2 Same-repo serial-queue invariant: `process_revision_requests` runs synchronously inside the iteration. Other repos' polling tasks are unaffected.
- [x] 8.3 Cancellation: the function honours `cancel.cancelled()` at every await point, matching the existing waiting-changes and pending-changes flows.
- [x] 8.4 Tests:
  - Iteration with one open PR with no comments: no revision attempted; the rest of the iteration runs normally.
  - Iteration with one open PR with one triggering comment: revision is processed; the pending-changes walk runs after with no interference.
  - Iteration with two open PRs each with triggering comments: both revisions are processed in PR-number order before the pending-changes walk.
  - Iteration where the revision raises `AskUser`: the pending-changes walk does NOT run (same-repo serial invariant from the AskUser-blocks-pending rule).

## 9. README + docs updates

- [x] 9.1 Add a new section in `docs/OPERATIONS.md` titled "Revising an open PR via comment." Documents the `@<bot> revise <text>` trigger pattern, the revision cap, the bot's reply shapes, and the AskUser-escalation path when a revision is ambiguous.
- [x] 9.2 Add a paragraph in `docs/CHATOPS.md` documenting the new `🛑 PR #<num> hit the revision cap` notification.
- [x] 9.3 Add a paragraph in `docs/CONFIG.md` documenting `executor.max_revisions_per_pr`.
- [x] 9.4 Add an entry in `docs/TROUBLESHOOTING.md` for the revision-loop failure modes: revision keeps failing, revision is silently ignored (cap reached), bot didn't reply (network blip or auth failure).

## 10. Spec delta

- [x] 10.1 The ADDED requirement in `openspec/changes/pr-comment-revision-loop/specs/orchestrator-cli/spec.md` codifies: the trigger pattern, the polling-based detection contract, the revision execution flow, the bot-reply shapes (success / failure / cap-decline), the cap enforcement rule, the priority-over-pending rule, the same-repo serial-queue rule, the state-file shape and pruning rule, and the AskUser-during-revision behaviour.

## 11. Verification

- [x] 11.1 `cargo test` passes (new + existing).
- [x] 11.2 `openspec validate pr-comment-revision-loop --strict` passes.
- [x] 11.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
