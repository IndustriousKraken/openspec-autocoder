# git-workflow-manager Specification

## Purpose
TBD - created by archiving change orchestrator-architecture. Update Purpose after archive.
## Requirements
### Requirement: Per-pass agent branch
The git workflow manager SHALL ensure each polling pass starts from a
clean branch off the configured base branch, recreating the agent
branch each pass. The branch source remains `origin/<base_branch>` in
both direct-push and fork-PR modes — the fork's view of the base
branch is never consulted.

#### Scenario: Branch initialization at start of pass
- **WHEN** a polling pass begins for a repository AND the queue
  contains at least one ready change
- **THEN** the manager runs, in order: `git fetch origin`,
  `git checkout <base_branch>`,
  `git pull --ff-only origin <base_branch>`,
  `git checkout -B <agent_branch>`
- **AND** the resulting `HEAD` of `<agent_branch>` is verifiable as
  identical to the post-pull `HEAD` of `<base_branch>` (`git rev-parse
  <agent_branch>` equals `git rev-parse <base_branch>`)
- **AND** prior local content on `<agent_branch>` is overwritten
  without warning — this is by design
- **AND** in fork-PR mode, the `fork` remote is NEVER consulted
  during branch initialization (it is push-only)

#### Scenario: Pull conflict on base branch
- **WHEN** `git pull --ff-only origin <base_branch>` exits non-zero
  (non-fast-forward, network error, etc.)
- **THEN** the manager aborts the polling pass for this repository
- **AND** the workspace is left in its pre-pull state (no agent
  branch is created or modified for this pass)
- **AND** the captured stderr from the failing git command is logged
  verbatim

### Requirement: Serial commit per change
The git workflow manager SHALL produce one commit per successfully implemented change, on the agent branch, in queue order. A change is "successfully implemented" only when the executor returns `Completed` AND `git status --porcelain` returns a non-empty result. If the workspace is clean after a `Completed` outcome, the manager SHALL NOT commit or archive the change; the iteration SHALL be marked Failed and the change SHALL remain pending for retry. The single commit per change SHALL include both the executor's working-tree modifications AND the archive move of `openspec/changes/<change>/` to `openspec/changes/archive/<YYYY-MM-DD>-<change>/`, so after the commit the working tree is clean and the change's archive move is fully captured in git history.

#### Scenario: Committing a change with modifications
- **WHEN** the executor returns `Completed` for `<change>` AND `git status --porcelain` returns a non-empty result inside the workspace
- **THEN** the manager builds `<change>: <summary>` (where `<summary>` is the first non-empty line of the `## Why` section of the change's `proposal.md`, truncated so the total subject is ≤ 72 characters)
- **AND** the manager moves `openspec/changes/<change>/` to `openspec/changes/archive/<YYYY-MM-DD>-<change>/` before staging
- **AND** the manager runs `git add -A` followed by `git commit -m "<subject>"`
- **AND** the resulting commit contains both the executor's modifications AND the archive rename
- **AND** `git status --porcelain` returns empty immediately after the commit

#### Scenario: Executor reported Completed but produced no diff
- **WHEN** the executor returns `Completed` for `<change>` AND `git status --porcelain` returns empty
- **THEN** the manager logs a warning naming `<change>` ("agent reported Completed without modifying the workspace; marking Failed")
- **AND** the manager does NOT create an empty commit
- **AND** the manager does NOT archive the change
- **AND** the iteration outcome is reported as Failed so the queue engine unlocks `<change>` and the next polling pass retries it

#### Scenario: Working tree clean after every archived change
- **WHEN** the manager has successfully committed any change in the
  current pass
- **THEN** `git status --porcelain` immediately after the commit
  returns empty
- **AND** this invariant holds for every archived change in the pass,
  including the last one, so no archive rename is ever left dangling
  in the working tree

### Requirement: Monolithic PR at end of pass
The git workflow manager SHALL push the agent branch at the end of each polling iteration that produced at least one commit, AND, when `auto_submit_pr` is `true` (the default), SHALL create a single Pull Request via the GitHub REST API AND surface the new PR's number to its caller so a follow-up implementer-summary comment can be posted. When `auto_submit_pr` is `false`, the manager SHALL push the branch but SKIP the PR-creation API call, instead returning a `BranchPushedNoPr { branch_url, suggested_pr_command }` outcome to the caller. The push target and PR `head` format depend on whether fork-PR mode is active:

- **Direct-push mode (`github.fork_owner` unset):** push to `origin`;
  PR `head` is the agent branch name alone.
- **Fork-PR mode (`github.fork_owner` set):** push to `fork`; PR
  `head` is `<fork-owner>:<agent-branch>` (cross-repo PR syntax).

In both modes (when PR creation is not skipped) the PR is posted to the upstream repository's `/pulls` endpoint. **When the code-reviewer is enabled, the PR body SHALL include the reviewer's report under a `## Code Review` heading, and a `Block` verdict SHALL cause the PR to be created as a draft (with a `do-not-merge` label fallback if the host rejects drafts).** **`github::create_pull_request` SHALL return both the `html_url` AND the `number` of the created PR.**

When `auto_submit_pr` is `false`, the `suggested_pr_command` SHALL be templated as `gh pr create --base <base> --head <agent-branch>` where `<base>` is `upstream.branch` if the `upstream` config block is set, OR the workspace's configured base branch otherwise. The reviewer-run AND implementer-summary-capture steps still execute when `auto_submit_pr` is `false`; their outputs are surfaced via the polling iteration's chatops notification rather than via the (skipped) PR body.

#### Scenario: Opening a PR in direct-push mode
- **WHEN** an iteration completes AND the agent branch contains at
  least one commit ahead of base AND `github.fork_owner` is unset
- **THEN** the manager pushes with
  `git push --force-with-lease origin <agent-branch>`
- **AND** POSTs to
  `https://api.github.com/repos/<upstream-owner>/<upstream-repo>/pulls`
  with body containing `"head": "<agent-branch>"` and
  `"base": "<base-branch>"`
- **AND** the response's `number` field is returned to the caller
  alongside the `html_url` so the implementer-summary comment
  step can target this PR

#### Scenario: Opening a PR in fork-PR mode
- **WHEN** an iteration completes AND the agent branch contains at
  least one commit ahead of base AND `github.fork_owner` is set to
  `<fork-owner>`
- **THEN** the manager pushes with
  `git push --force-with-lease fork <agent-branch>`
- **AND** POSTs to
  `https://api.github.com/repos/<upstream-owner>/<upstream-repo>/pulls`
  with body containing `"head": "<fork-owner>:<agent-branch>"` and
  `"base": "<base-branch>"`
- **AND** the API call's authentication token is resolved from
  `github.owner_tokens[<upstream-owner>]` (or the configured
  fallback), per the existing per-owner token routing — the upstream
  owner is the owner of the repository the PR targets, regardless
  of which account owns the fork
- **AND** the response's `number` field is returned to the caller
  alongside the `html_url`

#### Scenario: Opening a PR with a passing review
- **WHEN** an iteration completes AND the agent branch contains at
  least one commit ahead of base AND `reviewer.enabled` is true AND
  `code_reviewer.review` returns `Ok(ReviewReport { verdict: Pass, .. })`
- **THEN** the manager pushes (to `origin` or `fork` per the mode)
  with `--force-with-lease` and POSTs to the GitHub PR API with
  `draft: false` and a body whose final section is `## Code Review`
  followed by the reviewer's `markdown`
- **AND** the `head` parameter formatting follows the mode rules
  above
- **AND** the response's `number` is returned to the caller

#### Scenario: Opening a PR with a Block verdict
- **WHEN** an iteration completes AND the reviewer returns
  `Ok(ReviewReport { verdict: Block, .. })`
- **THEN** the manager pushes the agent branch (to `origin` or
  `fork` per the mode) and POSTs to the GitHub PR API with
  `draft: true`
- **AND** the PR body's final section is `## Code Review` followed
  by the reviewer's `markdown`
- **AND** the response's `number` is returned to the caller

#### Scenario: Reviewer disabled or absent
- **WHEN** the `reviewer` config block is absent OR
  `reviewer.enabled` is false
- **THEN** the manager pushes the agent branch (to `origin` or
  `fork` per the mode) and POSTs to the GitHub PR API with
  `draft: false` and a body that does NOT include a `## Code Review`
  section
- **AND** no LLM API call is made
- **AND** the response's `number` is returned to the caller

#### Scenario: auto_submit_pr false — push without PR creation
- **WHEN** an iteration completes AND the agent branch contains at least one commit ahead of base AND `auto_submit_pr: false` is configured for the repo
- **THEN** the manager pushes the agent branch per the existing direct-push OR fork-PR rule (same push target as it would use with `auto_submit_pr: true`)
- **AND** NO POST to the GitHub PR API is made
- **AND** the manager returns `BranchPushedNoPr { branch_url, suggested_pr_command }` to the caller
- **AND** `branch_url` is `https://github.com/<owner>/<repo>/tree/<agent-branch>` (resolved from the push target's remote URL)
- **AND** `suggested_pr_command` is `gh pr create --base <upstream.branch | base-branch> --head <agent-branch>`

#### Scenario: auto_submit_pr false — reviewer output surfaced without PR body
- **WHEN** `auto_submit_pr: false` AND `reviewer.enabled: true` AND `code_reviewer.review` returns `Ok(ReviewReport { verdict: Block, markdown: ... })`
- **THEN** the manager pushes the branch but does NOT create a PR (the canonical PR-body posting site is skipped because no PR exists)
- **AND** the reviewer's report is surfaced via the polling iteration's chatops notification (per the polling-iteration's existing notification mechanics) so the operator sees the report before manually running `gh pr create`
- **AND** the `BranchPushedNoPr` outcome carries the reviewer report alongside the branch URL so the caller can format the chatops notification accordingly

### Requirement: Implementer-summary PR comment
After a Pull Request is successfully created at the end of a polling iteration, the git workflow manager SHALL post a single follow-up issue comment to that PR containing the implementer agent's captured stdout for each change that shipped in the pass. The comment is best-effort: any failure to post is logged and ignored, and SHALL NOT roll back or affect the PR's existence. The comment exists to surface the agent's own narrative (modules touched, test counts, deviations from the spec it had to make, meta-observations) directly on the PR page so reviewers can read it without inspecting server-local log files.

#### Scenario: Comment posted after successful PR creation
- **WHEN** `open_pull_request` returns `Ok` with a PR number
- **THEN** the manager reads the per-change run-log file
  `<system-temp>/autocoder/logs/<workspace-basename>/<change>.log`
  for each archived change in the pass
- **AND** extracts the `=== STDOUT (n bytes) ===` block (only
  stdout; stderr is operator-facing log noise and is excluded)
- **AND** assembles a markdown comment with the structure:
  - heading `## Agent implementation notes`
  - one `### <change-name>` subsection per change, each carrying
    the extracted stdout
- **AND** POSTs the comment via
  `POST /repos/<upstream-owner>/<upstream-repo>/issues/<pr-number>/comments`
  using the same upstream-owner-routed token as the PR creation
- **AND** logs an INFO line on success with the PR number and
  comment count

#### Scenario: Comment posting fails
- **WHEN** the issue-comment POST returns a non-2xx status or the
  request errors at the transport layer
- **THEN** the manager logs an ERROR naming the failure (status
  code or transport error)
- **AND** the iteration overall is reported as Ok — the PR was
  successfully created; the missing comment is enrichment, not
  contract

#### Scenario: A change's log file is missing or unreadable
- **WHEN** the run-log file for a change cannot be read (file
  absent, permission denied, etc.)
- **THEN** the manager logs WARN naming the change and the path
- **AND** the comment is still posted, omitting that change's
  section
- **AND** if ALL changes' logs are missing, the comment is NOT
  posted (nothing useful to say) and an INFO line records the skip

#### Scenario: Empty stdout for a change
- **WHEN** the extracted stdout for a change is empty
- **THEN** the change's section content is `_(no implementer output captured)_`

#### Scenario: Comment body exceeds GitHub size limit
- **WHEN** the assembled comment body exceeds 60,000 characters
- **THEN** the body is truncated at 60,000 characters
- **AND** the truncation point is followed by `\n\n_[implementer summary truncated to fit GitHub comment limit; full output at /tmp/autocoder/logs/<workspace-basename>/<change>.log]_`
- **AND** the truncated body is posted as a single comment (NOT
  split across multiple comments)

### Requirement: `create_pr` helper accepts an explicit `--repo` argument for cross-repo PR creation

The `autocoder/src/github.rs::create_pr` helper (OR its equivalent shape) SHALL accept an optional `repo: Option<&str>` parameter. The parameter's value SHALL be a `<owner>/<name>` string. When `Some`, the helper's underlying `gh pr create` invocation SHALL receive `--repo <owner>/<name>` as an argument. When `None`, the existing behavior is preserved verbatim: no `--repo` flag is passed, AND `gh` uses the current working tree's origin to determine the target.

This parameter exists to support spec-storage PR creation, where the iteration's commit + push target a DIFFERENT git repo than the code workspace. The `gh` CLI's `--repo` flag natively handles this; the helper just passes it through.

Callers SHALL resolve the `<owner>/<name>` string from the target repo's remote URL (parsed from SSH OR HTTPS form) per the orchestrator-cli's spec-storage push-remote resolution requirement.

#### Scenario: `create_pr` with `repo: Some(...)` passes `--repo` to gh
- **WHEN** `create_pr` is invoked with `repo: Some("speccorp/specs-repo")`, `base: "main"`, `head: "agent-q"`, `title: "[specs] foo"`, AND a body
- **THEN** the underlying `gh pr create` invocation receives `--repo speccorp/specs-repo` in its argv
- **AND** receives `--base main --head agent-q --title "[specs] foo"`

#### Scenario: `create_pr` with `repo: None` omits the `--repo` flag
- **WHEN** `create_pr` is invoked with `repo: None` (the existing code-workspace path)
- **THEN** the underlying `gh pr create` invocation does NOT include `--repo` in its argv
- **AND** `gh` determines the target from the current working tree's origin (existing behavior)

#### Scenario: `create_pr` failure on cross-repo target surfaces a clear error
- **WHEN** `create_pr` is invoked with `repo: Some("speccorp/nonexistent")` AND the `gh` CLI returns non-zero with a "Repository not found" stderr
- **THEN** the helper returns `Err` carrying the captured stderr verbatim
- **AND** the operator-visible error names the target repo AND `gh`'s failure reason

### Requirement: `auto_submit_pr: false` post-push notification for spec-storage PRs uses `--repo`

When `auto_submit_pr: false` AND the iteration's classification is spec-only OR dual-tree's spec half, the post-push notification's `gh pr create` suggestion SHALL include `--repo <spec-owner>/<spec-name>` so the operator's manual invocation targets the correct repo.

Canonical notification body shape:

```
📦 Spec branch pushed to <spec-repo-url>:<branch>. Open a PR with:
  gh pr create --repo <spec-owner>/<spec-name> --base <resolved-base-branch> --head <branch> --title "[specs] <change-list-summary>"
```

The existing code-PR notification format is unchanged (no `--repo` flag, no `[specs] ` prefix).

#### Scenario: Spec-only push suggests cross-repo gh invocation
- **WHEN** a spec-only iteration completes AND `auto_submit_pr: false` AND `spec_storage.path` is configured pointing at `git@github.com:speccorp/specs-repo.git`
- **THEN** the post-push notification body contains `gh pr create --repo speccorp/specs-repo --base main --head agent-q --title "[specs] ..."`
- **AND** does NOT contain a bare `gh pr create` (without `--repo`)

#### Scenario: Code-only push notification is unchanged
- **WHEN** a code-only iteration completes AND `auto_submit_pr: false`
- **THEN** the post-push notification body contains the existing `gh pr create` suggestion WITHOUT `--repo` (the code workspace's origin determines the target)
- **AND** does NOT contain the `[specs] ` title prefix

### Requirement: Reviewer SHALL skip spec-only PRs when `reviewer.skip_spec_only_prs: true`

The `ReviewerConfig` SHALL accept an optional `skip_spec_only_prs: bool` field (default `false`). When `true`, the polling iteration's reviewer-invocation step SHALL skip the reviewer call AND post no `## Code Review` section for any PR whose ENTIRE diff lives under `openspec/`. The detection SHALL use the same diff classification as the iteration's commit + push classification (per the orchestrator-cli "Polling iteration classifies outcome" requirement): a PR opened from a spec-only iteration's classification is a spec-only PR; a PR opened from a code-only iteration's classification is NOT.

When `false` (default), the reviewer runs against spec-only PRs exactly as it runs against code-only PRs (existing canonical behavior preserved).

The toggle is a cost-optimization knob. Operators who want to skip reviewer LLM cost on spec-only PRs (which produce review verdicts that are typically less actionable than code review verdicts) can enable it. Operators who want full review coverage leave it at the default.

#### Scenario: `skip_spec_only_prs: true` skips reviewer on spec-only PR
- **WHEN** `reviewer.skip_spec_only_prs: true` AND a brownfield iteration produces a PR whose diff is entirely under `openspec/`
- **THEN** the reviewer is NOT invoked
- **AND** the PR body contains NO `## Code Review` section
- **AND** the iteration log includes an INFO line `reviewer: skipping spec-only PR per skip_spec_only_prs config`

#### Scenario: `skip_spec_only_prs: true` does NOT skip reviewer on dual-tree code PR
- **WHEN** `reviewer.skip_spec_only_prs: true` AND a dual-tree iteration produces TWO PRs (code + spec)
- **THEN** the spec PR's reviewer step is skipped
- **AND** the code PR's reviewer step runs normally (the diff includes `autocoder/src/...` AND/OR similar non-`openspec/` paths)

#### Scenario: `skip_spec_only_prs: false` (default) runs reviewer on all PRs
- **WHEN** `reviewer.skip_spec_only_prs` is unset OR `false` AND a spec-only iteration produces a PR
- **THEN** the reviewer is invoked exactly as today (existing canonical behavior preserved)
- **AND** the PR body contains the `## Code Review` section

### Requirement: Timeout-bounded remote fetch drains output concurrently
The git workflow manager SHALL drain a timeout-bounded `git fetch` child's stdout AND stderr concurrently with waiting for the process to exit, so that the amount of output the fetch may produce is bounded only by available memory, NOT by the operating system's pipe buffer. A fetch whose combined output exceeds the pipe buffer SHALL complete normally and surface its real outcome; it SHALL NOT be misreported as a timeout caused by an unread pipe. The genuine-timeout behavior is unchanged: when the child does not exit within the configured window, the manager SHALL kill AND reap the child AND return a timeout error.

#### Scenario: Fetch producing more than the pipe buffer of output completes
- **WHEN** `fetch_remote_with_timeout` runs a `git fetch` whose combined stdout + stderr exceeds the OS pipe buffer (e.g. an upstream with thousands of new refs or tags on a first fetch)
- **THEN** the child process writes all of its output without blocking because the manager drains both pipes while the child runs
- **AND** the function returns the fetch's real outcome — `Ok` on a zero exit, or `Err` carrying the captured stderr on a non-zero exit
- **AND** the function does NOT return a timeout error so long as the child exits within the configured window

#### Scenario: Genuine timeout still kills the child and reports timeout
- **WHEN** the child `git fetch` does not exit within the configured timeout window (e.g. an unreachable network host)
- **THEN** the manager kills the child process AND reaps it with a follow-up wait
- **AND** the function returns an `Err` whose message names the timeout (`git fetch <remote> timed out after <timeout_secs>s`)
- **AND** no pipe-reader thread is left running after the function returns

### Requirement: Working-tree status parsing uses a single NUL-delimited porcelain helper
All working-tree status parsing in the daemon SHALL go through one helper, `git::status_entries(workspace) -> Result<Vec<StatusEntry>>`, which runs `git status -z --porcelain --untracked-files=all` AND parses the NUL-delimited output. `StatusEntry` SHALL carry the staged status code, the worktree status code, the path, AND an optional original path for rename/copy records. Per-module hand-sliced parsers (the `extract_porcelain_path` copies AND `triage_status_entries`) SHALL be removed; there is one source of truth for status parsing.

The parser SHALL obey these rules:

- Records are delimited by the NUL byte (`\0`), NOT by newlines. The raw output SHALL NOT be trimmed as a whole — doing so strips the leading staged-status space of the first record (a blank staged column for a worktree-modified file), which would decapitate that record's path.
- Within a record, the first two characters are the staged (X) AND worktree (Y) status codes; the third character is a space; the remainder is the path.
- For a rename or copy record (X or Y is `R` or `C`), the immediately-following NUL-delimited token is the original path, captured as `orig_path`.
- Because `-z` mode emits paths verbatim (no C-style quoting), paths containing spaces or special characters parse correctly without an unquoting step.

The helper exposing the staged AND worktree codes lets callers distinguish staged-new (`A `) and renamed (`R`) entries from untracked (`??`) and worktree-modified (` M`) ones, rather than collapsing them to a single untracked-or-not boolean.

#### Scenario: Worktree-modified first record keeps its full path
- **GIVEN** a working tree whose only change is a worktree-modified tracked file at `openspec/changes/archive/<slug>/proposal.md`
- **WHEN** `status_entries` parses the `git status -z --porcelain` output
- **THEN** it returns one entry whose `path` is exactly `openspec/changes/archive/<slug>/proposal.md` — no leading character dropped
- **AND** the entry's staged code is a space AND its worktree code is `M`

#### Scenario: Path containing spaces parses literally
- **GIVEN** an untracked file at `dir with spaces/note.md`
- **WHEN** `status_entries` parses the output
- **THEN** the entry's `path` is `dir with spaces/note.md` — no surrounding quote characters AND no truncation

#### Scenario: Rename record captures the original path
- **GIVEN** a staged rename from `old.md` to `new.md`
- **WHEN** `status_entries` parses the output
- **THEN** the entry's `path` is `new.md` AND its `orig_path` is `Some("old.md")`

#### Scenario: Staged-new file is distinguishable from untracked
- **GIVEN** a staged new file `src/new.rs` (status `A `)
- **WHEN** `status_entries` parses the output
- **THEN** the entry's `path` is `src/new.rs` AND its staged code is `A`
- **AND** a caller can tell it apart from an untracked (`??`) file

#### Scenario: Changelog scope check accepts a modified archive proposal end-to-end
- **GIVEN** the changelog flow's working tree has a modified `openspec/changes/archive/<slug>/proposal.md` (a legitimate `changelog:` frontmatter edit)
- **WHEN** the out-of-scope check builds its changed-path list from `status_entries`
- **THEN** the path reaches `is_in_scope` intact AND is accepted
- **AND** the diff is NOT refused as out-of-scope

