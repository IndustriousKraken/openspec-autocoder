## Why

PRs autocoder opens currently use this title format:

> agent: 3 change(s) in pass

And this body format:

> Changes implemented in this pass:
>
> - a04-foo
> - a05-bar
> - a06-baz

That tells the reviewer (and future readers of `main`'s git log, once a PR is squash-merged) almost nothing about what the PR actually does. The information is already on hand — each change has a `proposal.md` with a `## Why` section explaining the rationale, and the change slug itself is reasonably descriptive once dehyphenated. The current generator throws all of that away in favor of a placeholder.

Three reasons to fix this:

1. **Git log readability.** GitHub's default squash-merge commit message uses the PR title verbatim. `"agent: 3 change(s) in pass"` is useless when scrolling `git log --oneline` six months later trying to find when a particular refactor landed. A title like `"a06: refactor portal handlers to from-ref (+2 more)"` is searchable.
2. **Review priming.** A reviewer who opens a PR with the current body sees a list of slugs and has to click into each `proposal.md` to learn what's going on. Inlining the `## Why` text into the body gives them the rationale up front.
3. **No new data.** Both improvements draw on data autocoder already has at PR-creation time. The change directories have just been archived in this iteration's commits, so the proposals live at `openspec/changes/archive/<date>-<slug>/proposal.md` and are readable via the workspace path the function already holds.

## What Changes

- **MODIFIED capability requirement** under `orchestrator-cli`: a new requirement "PR title and body describe what landed" pins the title and body shape. The title uses the change name (humanized) for a single-change PR, or `<first> (+N more)` for multi-change PRs. The body includes each change's `## Why` text under a per-change heading, followed by the existing slug list as a quick reference.
- **Code:**
  - New `build_pr_title(changes: &[String]) -> String` in `polling_loop.rs` that humanizes a change slug (replace hyphens after the first dash-bounded prefix with spaces; preserve the `aNN-` prefix or initial segment as a label, then a colon and the rest). Multi-change titles use `<first-humanized> (+N more)`. Total title length is capped at ~80 chars to stay readable in GitHub's PR list.
  - Replace `build_pr_body` with a version that, for each change in order, reads `openspec/changes/archive/*-<change>/proposal.md` (the change has just been archived in this iteration's commits), extracts the `## Why` section (everything between `## Why` and the next `## `), and writes a per-change markdown section. Falls back to the bare change name if the proposal can't be read or has no `## Why` section. The existing slug-list and self-heal disclaimer are preserved at the bottom.
  - `open_pull_request` calls `build_pr_title` instead of the hardcoded format.
- **Tests:**
  - `build_pr_title_single_change_humanizes_slug` — `["a06-refactor-portal-handlers-to-fromref"]` produces `"a06: refactor portal handlers to fromref"` (or similar).
  - `build_pr_title_multi_change_lists_first_and_count` — `["a04-foo", "a05-bar", "a06-baz"]` produces `"a04: foo (+2 more)"`.
  - `build_pr_title_caps_at_eighty_chars` — extra-long single-change slug is truncated with an ellipsis rather than overflowing.
  - `build_pr_body_inlines_why_from_proposal` — fixture archives a change with a known `## Why` text; body contains that text under the per-change heading.
  - `build_pr_body_falls_back_when_proposal_missing` — change with no proposal.md still produces a body (just the slug + list).

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/polling_loop.rs` (title + body generators, tests). The PR-creation call site is the only consumer.
- Operator-visible behavior: PR titles and bodies opened from this commit forward look different. PRs already open are unchanged. The chatops `:tada:` notification continues to embed the URL only and is unaffected. Branch-protection rules keyed on the PR title (unlikely but possible) would need adjustment.
- Breaking: no API change. The visible-output change is intentional and the entire point of the spec.
- Acceptance: new tests pass; existing PR-creation flow tests still pass with their assertions updated to the new format where they currently assert on the old; `cargo test` passes; `openspec validate informative-pr-title-and-body --strict` passes.
