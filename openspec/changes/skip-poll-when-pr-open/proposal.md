## Why

Production failure mode (2026-05-14): autocoder opened a PR for two changes. The PR was not merged. On every subsequent poll the daemon re-implemented both changes from scratch — burning Claude tokens, force-pushing new commits onto the open PR's branch (so the PR's diff thrashes under any reviewer), and erroring at `POST /pulls` (since GitHub returns 422 when an open PR already exists for the same head→base).

At a 30-minute poll interval, a month of unmerged-PR vacation = ~1,400 redundant implementations per change. The cost exposure is real.

## What Changes

- **ADDED capability:** `orchestrator-cli` SHALL skip a polling iteration entirely when an open PR already exists for the configured agent branch. The check happens after workspace init succeeds but before `recreate_branch` (so the open PR's branch is not clobbered). Detection uses the GitHub REST API: `GET /repos/{owner}/{repo}/pulls?state=open&head=<head>&base=<base>`.
- **Head qualifier:** in fork-PR mode (`github.fork_owner` set) the `head` parameter is `<fork_owner>:<agent_branch>`. In direct mode the `head` is `<repo_owner>:<agent_branch>`. Both forms are required for the query to be unambiguous.
- **Code:**
  - New `github::list_open_prs(api_base, owner, repo, head, base, token) -> Result<Vec<u64>>` returning the PR numbers of any matching open PRs.
  - `polling_loop::execute_one_pass` calls this between `pull --ff-only` and `recreate_branch`. If any open PR is found, logs INFO with the PR number(s) and returns early.
- **Tests:**
  - `polling_loop::tests::skips_iteration_when_open_pr_exists` — mockito server returns one open PR; assert the executor is never invoked.
  - `polling_loop::tests::proceeds_when_no_open_pr` — mockito returns an empty array; assert the executor IS invoked.
  - `polling_loop::tests::proceeds_when_pr_query_returns_404` — graceful fallback: if the query errors (auth, transport), log WARN and proceed rather than block the iteration on a transient GitHub problem.
  - `github::tests::list_open_prs_parses_response` — exercise the JSON parsing.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/github.rs`, `autocoder/src/polling_loop.rs`.
- One extra HTTP call per polling iteration. Negligible (~50ms). No new dependencies.
- Breaking? No. Daemons that were behaving correctly (merged PRs promptly) see no change.
- Limitation: detection is at the PR-branch level, not per-change. If a PR contains two changes and the operator wants to re-implement only one, they have to close the PR and let the daemon re-implement both. Acceptable given autocoder always opens a single monolithic PR per pass.
