## Why

Chat-driven changelog generation fails: the changelog-stylist legitimately stamps `changelog: skip` frontmatter onto an archived `proposal.md` (an in-scope edit per the canonical changelog requirement), but the scope check rejects it as out-of-scope. The logged path is `penspec/changes/archive/<slug>/proposal.md` — `openspec` with its leading character chopped.

Root cause (reproduced exactly): `git::status_porcelain` returns `String::from_utf8_lossy(stdout).trim()`. Porcelain v1 worktree-modified records start with a leading space (the staged-status column is blank when a file is modified-but-not-staged): ` M openspec/...`. `.trim()` strips that leading space off the FIRST record, collapsing its 3-char `XY␣` prefix to 2 chars. The per-module `extract_porcelain_path` then does `line[3..]` — correct for an intact 3-char prefix — and over-slices by one, decapitating the path. `is_in_scope("penspec/...")` returns false, the whole diff is refused, and changelog generation aborts.

The deeper problem is duplication: there are **four** hand-sliced `extract_porcelain_path` copies (`changelog_triage.rs`, `polling/brownfield.rs`, `polling_loop.rs`, `audits/scheduler.rs`) plus `triage_status_entries`, each parsing `git status --porcelain` by index. They share a class of fragility the a43 code review kept surfacing on PR #84: leading-space handling, C-style path quoting (paths with spaces), and status-code width (staged `A `/rename `R` records). One robust helper retires all of it.

## What Changes

**A single NUL-delimited status helper (git-workflow-manager).** `git.rs` gains `status_entries(workspace) -> Result<Vec<StatusEntry>>` that runs `git status -z --porcelain --untracked-files=all` and parses the NUL-delimited records into `StatusEntry { staged: char, worktree: char, path: String, orig_path: Option<String> }`. Records are split on the NUL byte (never `.trim()`-ed as a whole, so the first record's leading status-space survives), each record's first two chars are the staged/worktree codes, index 2 is a space, and the remainder is the path. Rename/copy records capture the following NUL token as `orig_path`. `-z` emits paths verbatim, so spaces and special characters need no unquoting.

**All status parsing migrates onto it.** The four `extract_porcelain_path` copies AND `triage_status_entries` are removed; their callers use `status_entries`, reading `.path` and the status codes they need. The changelog flow's `is_in_scope` now receives an intact path, so the legitimate frontmatter edit is accepted and changelog generation succeeds. Exposing the staged/worktree codes also gives the triage discard flow what it needs to handle staged-new (`A `) and renamed (`R`) files correctly (the a43-flagged abort), though that flow's policy logic is out of scope here.

**`status_porcelain`'s whole-output `.trim()` is corrected to `.trim_end()`** for any remaining string-returning caller (e.g. dirty-tree checks), so the leading status-space of the first record is never stripped.

## Impact

- **Affected specs:**
  - `git-workflow-manager` — ADDED `Working-tree status parsing uses a single NUL-delimited porcelain helper`.
- **Affected code:**
  - `autocoder/src/git.rs` — add `status_entries` (and `StatusEntry`); change `status_porcelain` / `status_porcelain_untracked_all` `.trim()` → `.trim_end()`.
  - `autocoder/src/changelog_triage.rs` — remove `extract_porcelain_path`; both out-of-scope checks (~257, ~666) use `status_entries(...).path`.
  - `autocoder/src/polling_loop.rs` — remove `extract_porcelain_path` (~6573) AND `triage_status_entries` (~6790); `discard_non_spec_writes` and callers use `status_entries`.
  - `autocoder/src/polling/brownfield.rs` (~577) AND `autocoder/src/audits/scheduler.rs` (~708) — remove their `extract_porcelain_path`; use `status_entries`.
- **Operator-visible behavior:** chat-driven changelog generation works again (the frontmatter edit is no longer falsely rejected). No other behavior change; the migration is parsing-correctness-only.
- **Acceptance:** `cargo test` passes; `openspec validate a001-nul-delimited-git-status-parsing --strict` passes. Tests (behavior, against synthetic git states): a worktree-modified first record keeps its full path (the changelog regression); a path containing spaces parses literally; a rename captures `orig_path`; a staged-new file reports `staged == 'A'`; and the changelog `is_in_scope` accepts a modified archive `proposal.md` end-to-end.
- **Dependencies:** none. Independent of a47–a53. Complements the a43 follow-up (PR #84) by giving its discard flow a correct status parser, but does not change that flow's policy.
