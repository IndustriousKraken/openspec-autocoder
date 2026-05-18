## 1. Title generator

- [x] 1.1 Add `fn build_pr_title(changes: &[String]) -> String` to `polling_loop.rs`. Behavior:
  - 0 changes (defensive): return `"agent: empty pass"` (the caller already short-circuits on 0 commits, so this is unreachable in production; keep the branch defensive anyway).
  - 1 change: humanize the slug. Strategy: if the slug matches `^([a-z]+\d+)-(.+)$` (the stacked `aNN-` prefix convention), format as `"<prefix>: <rest with hyphens → spaces>"`. Otherwise format as `"<entire slug with hyphens → spaces>"`. Examples: `a06-refactor-portal-handlers-to-fromref` → `a06: refactor portal handlers to fromref`; `foo-bar-baz` → `foo bar baz`.
  - 2+ changes: humanize the first slug per the single-change rule, then append `" (+N more)"` where N is `changes.len() - 1`.
  - Length cap: if the resulting title exceeds 80 chars, truncate the rest-portion (after the colon for prefixed slugs; the whole title for unprefixed) and append `"…"`.
- [x] 1.2 Helper `humanize_slug(slug: &str) -> String` extracted so the title generator can call it cleanly. Uses `regex` (already a dep) to detect the `aNN-` prefix; falls back to plain hyphen→space when no match.

## 2. Body generator

- [x] 2.1 Replace `build_pr_body` with a signature that takes the workspace path so it can read proposal.md files: `fn build_pr_body(workspace: &Path, changes: &[String], includes_self_heal: bool) -> String`.
- [x] 2.2 For each change in order:
  - Glob `<workspace>/openspec/changes/archive/*-<change>/proposal.md` (the change was archived in one of this iteration's commits, so the archive entry exists). On multiple matches (shouldn't happen, but possible if archive dates collide), pick the lexicographically last so the most-recent date wins.
  - If the file exists and contains a `## Why` heading, extract the section: everything from the line after `## Why` up to but not including the next `## ` heading (or EOF). Trim leading/trailing whitespace.
  - Emit a markdown section: `## <change-slug>\n\n<why-text>\n\n` (no humanization here — the slug is the canonical identifier, useful for grep). If `## Why` is absent or proposal.md is unreadable, emit just `## <change-slug>\n\n_(no proposal.md available)_\n\n`.
- [x] 2.3 After all per-change sections, append the existing "Changes implemented in this pass:" list as a compact reference. Preserve the existing self-heal disclaimer at the top of the body when `includes_self_heal` is true.
- [x] 2.4 Update the call site in `open_pull_request` to pass `workspace` through.

## 3. Tests for title generator

- [x] 3.1 `build_pr_title_single_change_humanizes_aNN_prefix` — input `["a06-refactor-portal-handlers-to-fromref"]`, expected `"a06: refactor portal handlers to fromref"`.
- [x] 3.2 `build_pr_title_single_change_without_prefix` — input `["fix-bug-in-thing"]`, expected `"fix bug in thing"`.
- [x] 3.3 `build_pr_title_multi_change_uses_first_and_count` — input `["a04-foo-thing", "a05-bar-thing", "a06-baz-thing"]`, expected `"a04: foo thing (+2 more)"`.
- [x] 3.4 `build_pr_title_caps_overlong` — input a single slug with 200+ chars; expected length ≤ 80 chars AND ends with `"…"`.
- [x] 3.5 `humanize_slug_strips_aNN_prefix_into_label` — unit-tests the helper directly: `a06-x-y` → `"a06: x y"`; `b13-foo-bar` → `"b13: foo bar"`; `foo-bar` → `"foo bar"`.

## 4. Tests for body generator

- [x] 4.1 `build_pr_body_inlines_why_from_archived_proposal` — fixture: a workspace with `openspec/changes/archive/2026-05-18-fix-thing/proposal.md` containing `## Why\n\nThing was broken because of reasons.\n\n## What Changes\n\n...`. Call `build_pr_body(workspace, &["fix-thing"], false)`. Assert the body contains `"## fix-thing"`, `"Thing was broken because of reasons."`, AND `"Changes implemented in this pass"` (the list reference at the bottom).
- [x] 4.2 `build_pr_body_falls_back_when_proposal_missing` — fixture with no archive dir at all. Body contains `"## fix-thing"` AND `"_(no proposal.md available)_"` AND does NOT panic.
- [x] 4.3 `build_pr_body_handles_multiple_changes` — fixture archives three changes with distinct `## Why` texts. Body has three `## <slug>` sections in input order, each containing its own Why text.
- [x] 4.4 `build_pr_body_preserves_self_heal_disclaimer` — `includes_self_heal=true`; first line of body is the existing self-heal disclaimer paragraph, followed by per-change sections.
- [x] 4.5 `build_pr_body_extracts_only_why_section` — proposal.md with `## Why\nWhy text.\n## What Changes\nDifferent text.\n## Impact\nMore text.\n`. Assert body contains "Why text." but NOT "Different text." or "More text." — only the `## Why` section is extracted.

## 5. Update existing tests that asserted on the old format

- [x] 5.1 Find every test in `polling_loop::tests` that asserts on the old title (`"agent: N change(s)"`) or old body (`"Changes implemented in this pass"`). Update to the new format. Specifically check the self-heal disclaimer test at line ~4962 and any title-asserting tests.

## 6. Spec delta

- [x] 6.1 Add an ADDED requirement to `orchestrator-cli`: "PR title and body describe what landed". Three scenarios: single-change PR, multi-change PR, missing-proposal fallback.

## 7. Verification

- [x] 7.1 `cargo test` passes.
- [x] 7.2 `openspec validate informative-pr-title-and-body --strict` passes.
