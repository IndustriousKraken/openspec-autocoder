## Why

The code-reviewer currently sees only a 100,000-char unified diff and a one-line "summary" naming which changes shipped. Grok-4's review of a recent multi-file change explicitly bailed: "Diff is truncated to 100k chars... full review of the new experimental backends, alert_state, and refactored chatops paths is impossible."

Two structural defects with the current design:

1. **Budget too small for modern context windows.** Both Anthropic Opus 4 and xAI Grok-4 take ~1M tokens (≈4M chars). 100k chars per review under-uses the context budget by ~40x.
2. **Diff alone is not enough for security review.** A unified diff hides the surrounding code that determines whether an apparent vulnerability is actually exploitable. Reviewing the full changed files (and the spec they implement) lets the reviewer evaluate trust boundaries, helper calls, and call-graph implications. The current build_change_summary helper just lists change names — no proposal context at all.

This change is an explicit stopgap. The longer-term fix is an MCP-server reviewer with `Read`/`Grep` tools so the reviewer can roam the codebase as needed. autocoder will eventually build that itself.

## What Changes

- **MODIFIED capability:** `code-reviewer` — the reviewer's input shape and budget change. The diff-only path is replaced by a richer context bundle prioritizing source files over diff.
- **Budget:** raise the prompt-content cap from 100,000 to 2,000,000 characters.
- **Context priority order** when assembling the rendered prompt:
  1. Change context — the proposal/design/tasks of every archived change in this pass (so the reviewer understands intent and security implications).
  2. Changed-file contents — every file touched by the diff, read in full from the agent branch's current state.
  3. Unified diff — included last, only if budget remains.
- **Skipped-items disclosure:** if budget exhausts before all files are included, the rendered prompt names which files were omitted and instructs the model to flag missing context.
- **Template variables:** the default review template now uses `{{change_context}}`, `{{changed_files}}`, and `{{diff}}` (which may be empty). The old `{{change_summary}}` variable is retired.
- **API shape:** `CodeReviewer::review` now takes a structured `ReviewContext` that the polling loop assembles, instead of two strings.
- **Code:**
  - `code_reviewer::ReviewContext { archived_changes: Vec<ChangeBrief>, changed_files: Vec<ChangedFile>, diff: String }`.
  - Reviewer renders context honoring the priority order, substitutes into the template, returns the same `ReviewReport`.
  - `polling_loop::run_pass_through_commits` builds the `ReviewContext` from the archived changes (read from `openspec/changes/archive/<date>-<name>/`) plus the diff's name-only file list (read each file from the agent-branch workspace state).
  - The default `prompts/code-review-default.md` is rewritten to match the new variables.

## Impact

- Affected specs: `code-reviewer` (one MODIFIED requirement, one MODIFIED scenario for the budget bump).
- Affected code: `autocoder/src/code_reviewer.rs`, `autocoder/src/polling_loop.rs` (the build-and-call site).
- Affected templates: `prompts/code-review-default.md`.
- Breaking change for any operator who has set `reviewer.prompt_template_path` to a custom template using `{{change_summary}}`: that variable is gone. They must update to `{{change_context}}`. (Documented in upgrade notes.)
- Token cost increase: a typical multi-file change might send 50-500KB of source instead of 100KB of diff — roughly a 1-5x cost bump per review. Acceptable: reviews are infrequent (once per pass, only when a pass produces commits).
