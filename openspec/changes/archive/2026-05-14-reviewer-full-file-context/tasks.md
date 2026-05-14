## 1. Reviewer types and budget

- [x] 1.1 In `code_reviewer.rs`, replace `const DIFF_SIZE_BUDGET: usize = 100_000;` with `const PROMPT_BUDGET: usize = 2_000_000;`.
- [x] 1.2 Add new public structs:
    ```rust
    pub struct ReviewContext {
        pub archived_changes: Vec<ChangeBrief>,
        pub changed_files: Vec<ChangedFile>,
        pub diff: String,
    }
    pub struct ChangeBrief {
        pub name: String,
        pub proposal: String,
        pub design: Option<String>,
        pub tasks: String,
    }
    pub struct ChangedFile {
        pub path: String,
        pub contents: String,
    }
    ```

## 2. Reviewer rendering

- [x] 2.1 Change `CodeReviewer::review` signature from `(diff: &str, change_summary: &str)` to `(context: &ReviewContext)`. The function builds three rendered strings — `change_context`, `changed_files`, `diff_or_explanation` — honoring the 2,000,000-char budget in priority order:
    1. Render `change_context` first: for each `ChangeBrief`, append `## Change: <name>\n\n` then proposal, then design (if Some), then tasks, separated by `\n\n`.
    2. Render `changed_files` second: for each `ChangedFile`, append `## File: <path>\n\n` plus the file contents, separated by `\n\n`. If adding the next file would exceed the budget, stop and append `## Skipped (budget exhausted): <comma-separated paths>` with the omitted paths.
    3. Compute remaining budget after the first two sections. If `diff.len()` fits in the remainder, set `diff_or_explanation = diff.to_string()`. Otherwise set it to `"(diff omitted: budget exhausted by change context and changed files)"`.
- [x] 2.2 Substitute via three `.replace()` calls into `self.template`: `{{change_context}}` → rendered change context, `{{changed_files}}` → rendered files (incl. skip footer if any), `{{diff}}` → rendered diff or explanation.
- [x] 2.3 The retired `{{change_summary}}` placeholder is intentionally NOT substituted; any custom template still referencing it sees the literal text. The default template does not use it.

## 3. Default template rewrite

- [x] 3.1 Rewrite `prompts/code-review-default.md` to match the new variables. Structure:
    - The existing scope/format preamble stays verbatim.
    - "# Change context" section uses `{{change_context}}`.
    - "# Changed files (full contents)" section uses `{{changed_files}}`.
    - "# Diff" section (last) uses `{{diff}}`.
    - The truncation-acknowledgment instruction is rephrased: "If you see `## Skipped (budget exhausted)` or `(diff omitted: ...)` in the prompt, acknowledge missing context in your first bullet under `## Possible bugs` and bias toward `Concerns` over `Pass`."

## 4. Caller (polling_loop) builds the context

- [x] 4.1 In `polling_loop::execute_one_pass` (or wherever the reviewer step lives — line ~108 today), replace `git::diff_three_dot(...) + build_change_summary(...)` with a `ReviewContext` builder. Steps:
    1. Compute the unified diff via `git::diff_three_dot(workspace, &repo.base_branch, &repo.agent_branch)`.
    2. Compute the name-only file list via a new `git::diff_files_changed(workspace, base, head)` helper that runs `git diff --name-only base...head`.
    3. For each file path, read its current contents from disk (the agent branch is checked out at this point). Skip files that no longer exist (deleted files — they appear in the diff but have no current content; the diff itself captures their removal).
    4. For each archived change name in `processed`, locate the archive entry under `openspec/changes/archive/<*>-<name>/` and read `proposal.md`, optional `design.md`, and `tasks.md`. The archive directory is the date-prefixed form (`YYYY-MM-DD-<name>`); glob via `std::fs::read_dir` and match the suffix.
    5. Assemble the `ReviewContext` and call `reviewer.review(&ctx)`.
- [x] 4.2 Drop `build_change_summary` (now unused).

## 5. Tests

- [x] 5.1 `code_reviewer::tests::review_renders_change_context_first` — `ReviewContext` with one ChangeBrief and one ChangedFile. Assert the rendered prompt sent to the (fixture) LLM contains the change-context block BEFORE the changed-files block, and BEFORE the diff.
- [x] 5.2 `code_reviewer::tests::review_skips_files_when_budget_exhausts` — `ReviewContext` with two 1.5MB-string-contents files. Assert the second file is in the skipped list, the diff is the budget-exhausted explanation, and the rendered prompt is ≤ 2,000,000 chars.
- [x] 5.3 `code_reviewer::tests::review_includes_diff_when_room` — small context, small files, modest diff. Assert all three sections present and the diff is verbatim.
- [x] 5.4 `code_reviewer::tests::review_never_truncates_individual_file` — a single 2.5MB file. Assert it's either fully present OR fully skipped (no partial slice).
- [x] 5.5 Existing reviewer tests that call `review("...", "...")` are updated to construct a minimal `ReviewContext` (most can use empty `archived_changes` and `changed_files`, just to exercise the LLM call).
- [x] 5.6 `polling_loop::tests` — update or add an integration test that exercises the build-context path. Acceptable: existing `reviewer_pass_attaches_report_to_pr` (or whichever exists) is updated to use the new signature; no new test required if coverage already touches the path.

## 6. Documentation

- [x] 6.1 README's Code Review section: update to describe the priority order, the 2M-char budget, and the new template variables.
- [x] 6.2 README: note that custom templates referencing `{{change_summary}}` need to migrate to `{{change_context}}`. No code-side compat shim.

## 7. Verification

- [x] 7.1 `cargo test` passes.
- [x] 7.2 `openspec validate reviewer-full-file-context --strict` passes.
