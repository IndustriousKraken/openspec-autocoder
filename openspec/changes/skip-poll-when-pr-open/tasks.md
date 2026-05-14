## 1. GitHub API: list open PRs by head/base

- [ ] 1.1 Add `pub async fn list_open_prs(api_base: &str, owner: &str, repo: &str, head: &str, base: &str, token: &str) -> Result<Vec<OpenPr>>` to `github.rs`. Calls `GET {api_base}/repos/{owner}/{repo}/pulls?state=open&head={head}&base={base}`. Returns `Vec<OpenPr { number: u64, html_url: String }>` on 2xx. Returns Err on non-2xx with status code + 500-char body snippet, matching the style of `create_pull_request`.
- [ ] 1.2 Add a convenience `pub async fn list_open_prs_default(...)` that targets `DEFAULT_API_BASE` for production callers; the explicit-base form stays for tests.
- [ ] 1.3 **Verify:** `github::tests::list_open_prs_parses_response` — mockito returns `[{"number":42,"html_url":"https://..."}]`, assert one PR with number 42. Also a test with empty array returning Vec::new(). Also a test with non-2xx returning Err.

## 2. Polling-loop integration

- [ ] 2.1 In `polling_loop::execute_one_pass` (or wherever `pull --ff-only` lives — confirm before editing), insert the open-PR check immediately after `pull_ff_only` succeeds and BEFORE `recreate_branch`. Compute the head qualifier:
    - If `github.fork_owner.is_some()`: `head = format!("{}:{}", fork_owner, repo.agent_branch)`
    - Else: parse `repo.url` via `github::parse_repo_url` to extract `<owner>`, then `head = format!("{}:{}", owner, repo.agent_branch)`
- [ ] 2.2 Resolve the API token using the existing `github_credentials::resolve_token_with_source` helper (same as PR creation uses).
- [ ] 2.3 Call `list_open_prs_default(<upstream-owner>, <upstream-repo>, &head, &repo.base_branch, &token)`. The `<upstream-owner>` and `<upstream-repo>` are always parsed from `repo.url` regardless of fork mode (queries always target upstream — that's where PRs land).
- [ ] 2.4 Branch on result:
    - `Ok(vec)` non-empty → log `tracing::info!(url = %repo.url, pr_count = vec.len(), prs = ?vec.iter().map(|p| p.number).collect::<Vec<_>>(), "open PR exists; skipping iteration")`, then return `Ok(())` from `execute_one_pass`.
    - `Ok(vec)` empty → proceed with `recreate_branch` and the rest of the pass.
    - `Err(e)` → log `tracing::warn!(url = %repo.url, "open-PR check failed: {e:#}; proceeding with iteration")` and proceed (best-effort).

## 3. Tests

- [ ] 3.1 `polling_loop::tests::skips_iteration_when_open_pr_exists` — fixture workspace with a pending change AND a mockito GitHub endpoint that returns one open PR. Use a `MustNotRunExecutor` (already exists in tests for the same-repo-block test) to assert `executor.run` is NEVER called. Confirm the change is still pending after the pass.
- [ ] 3.2 `polling_loop::tests::proceeds_when_no_open_pr` — same fixture but mockito returns `[]`. Use the `CompletingExecutorWithDiff` fixture; assert the executor ran and a commit was produced.
- [ ] 3.3 `polling_loop::tests::proceeds_when_pr_query_errors` — mockito returns 500 status. Assert the executor IS invoked despite the query failure (best-effort fallback).
- [ ] 3.4 **Verify:** existing tests under `polling_loop::tests` continue to pass. Any test that uses a mockito GitHub endpoint may need to add a `GET /pulls?...` mock returning `[]` so the new pre-flight doesn't fail their fixtures. Update each affected test.

## 4. Documentation

- [ ] 4.1 README: in "Operating Notes", add a brief paragraph explaining that polling skips iterations while an open PR exists on the agent branch, and that to re-trigger implementation the operator must close or merge the PR. Reference this behavior near the existing "Recovering from a bad run" subsection.

## 5. Verification

- [ ] 5.1 `cargo test` passes.
- [ ] 5.2 `openspec validate skip-poll-when-pr-open --strict` passes.
