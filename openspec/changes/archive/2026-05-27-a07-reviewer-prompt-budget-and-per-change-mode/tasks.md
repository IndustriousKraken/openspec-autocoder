## 1. Config schema

- [x] 1.1 Extend `ReviewerConfig` in `autocoder/src/config.rs`:
  ```rust
  #[serde(default = "default_prompt_budget_chars")]
  pub prompt_budget_chars: usize,
  #[serde(default)]
  pub mode: ReviewerMode,
  ```
- [x] 1.2 Define `ReviewerMode`:
  ```rust
  #[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
  #[serde(rename_all = "snake_case")]
  pub enum ReviewerMode { #[default] Bundled, PerChange }
  ```
- [x] 1.3 Add `fn default_prompt_budget_chars() -> usize { 2_000_000 }`. No clamping — operators are responsible for matching their provider's context window.
- [x] 1.4 Update the `project-documentation` config-example-coverage test list to include the two new fields. Update `config.example.yaml` with both fields under the commented-out `reviewer:` block, each with an explanatory comment.
- [x] 1.5 Tests: default config parses with `prompt_budget_chars: 2_000_000` and `mode: Bundled`. Explicit values parse correctly. Unknown enum values surface a descriptive parse error.

## 2. Budget-driven truncation refactor

- [x] 2.1 Locate the existing prompt-budget cap in the reviewer code path (today hard-coded as `2_000_000`). Replace every read with `config.reviewer.prompt_budget_chars`.
- [x] 2.2 The truncation logic (skip files over budget, drop diff if files were skipped, etc.) stays identical. Only the threshold becomes data-driven.
- [x] 2.3 The "## Skipped (budget exhausted): ..." footer continues to fire when truncation kicks in.
- [x] 2.4 Tests: existing truncation tests pass against the configurable cap. A new test confirms a higher cap (e.g. `4_000_000`) lets a 3_000_000-char file through that the default cap would have skipped.

## 3. Per-change reviewer dispatch

- [x] 3.1 In the reviewer dispatch site, branch on `config.reviewer.mode`:
  ```rust
  match config.reviewer.mode {
      ReviewerMode::Bundled => run_bundled_review(pass, config, llm).await,
      ReviewerMode::PerChange => run_per_change_review(pass, config, llm).await,
  }
  ```
- [x] 3.2 New function `run_per_change_review(pass: &Pass, config: &Config, llm: &dyn LlmClient) -> Result<Vec<PerChangeReview>>`:
  - Iterate `pass.changes`. For each change:
    - Compute the per-change diff (the change's commit alone, not the union).
    - Identify files touched by the change's commit (`git show --name-only <sha>`).
    - Read each touched file's full contents (subject to `prompt_budget_chars`).
    - Build the cross-change preamble: one line per OTHER change in the pass, format `<slug>: <first-paragraph-of-Why truncated to 200 chars>`.
    - Build the prompt: existing template + preamble + per-change diff + per-change files.
    - Invoke the LLM. Parse the response (same parser as bundled mode).
    - Return one `PerChangeReview { change_slug, verdict, concerns, revision_requests }`.
  - Each per-change review independently respects `prompt_budget_chars` for its own touched-files subset.
- [x] 3.3 The cross-change preamble template (embed in the prompt; static text):
  ```
  This PR contains <N> changes. You are reviewing only `<this-change-slug>`.
  Other changes in the same PR (for cross-reference context only — do not review them):
  - <other-slug-1>: <other-1-summary>
  - <other-slug-2>: <other-2-summary>
  Your verdict applies ONLY to `<this-change-slug>`. The reviewer for each
  other change runs independently.
  ```
- [x] 3.4 Tests:
  - Single-change pass → per_change mode emits one review (no preamble; the "other changes" list is empty).
  - Three-change pass → per_change emits three reviews; each preamble names the other two changes.
  - Per-change reviews independently respect budget — a change touching a huge file has its OWN truncation footer, not affecting the other changes' reviews.

## 4. PR body composition

- [x] 4.1 In the PR-body composer, branch on the result type:
  - `Vec<PerChangeReview>` (per-change mode): emit one `## Code Review: <change-slug>` section per change. Each section contains the same verdict / concerns / format the existing `## Code Review` block uses for bundled mode.
  - Existing bundled result type: unchanged — one `## Code Review` block as today.
- [x] 4.2 The `## Agent implementation notes` and `## Skipped (budget exhausted)` sections are independent and stay unchanged.
- [x] 4.3 Tests: PR body for a 3-change pass under per_change mode contains exactly 3 `## Code Review: <slug>` headings in change order. The combined body stays under GitHub's 65,535-char PR-body cap (truncate the LAST per-change section with a "see daemon log" pointer if needed, same shape as the existing `## Agent implementation notes` truncation).

## 5. Reviewer-initiated revisions aggregation

- [x] 5.1 The `auto_revise_on_block` path collects revision requests from EVERY per-change review (when in per_change mode) and posts them as a flat list of `<!-- reviewer-revision -->`-marked PR comments. The existing comment-posting code doesn't change; only the source becomes "concatenated revision_requests from N per-change reviews" instead of "revision_requests from the one bundled review."
- [x] 5.2 The `executor.max_revisions_per_pr` cap applies across the union of revision requests (not per-change). When the cap would be exceeded, the existing "drop oldest after the cap" + annotated-in-PR-body logic applies.
- [x] 5.3 Tests: 3-change per-change pass with each change producing 2 revision requests AND `max_revisions_per_pr: 5` → 5 comments posted, 1 annotated as "(not auto-revised; cap budget exhausted)".

## 6. Hot-reload integration

- [x] 6.1 The existing `reviewer:` hot-reload path (per the `runtime control: live config reload` requirement) automatically picks up the new fields since they're inside the `reviewer:` block.
- [x] 6.2 Verify in the reload tests that changing `reviewer.mode` from `bundled` to `per_change` between iterations causes the next PR to use the new mode.

## 7. Embedded prompt template update

- [x] 7.1 Update `prompts/code-review-default.md`:
  - Remove the literal `2,000,000 characters` mention (replace with `the configured `reviewer.prompt_budget_chars`).
  - Add a placeholder block near the top that's empty under bundled mode and populated under per-change mode with the cross-change preamble.
  - The template's existing scope statement, verdict format, revision-requests format all stay unchanged.

## 8. Docs

- [x] 8.1 In `docs/CODE-REVIEW.md`, add a `## Prompt budget` subsection describing `reviewer.prompt_budget_chars` (default 2M, no hard ceiling, operator matches it to the provider's window) AND a `## Per-change reviewer mode` subsection describing `reviewer.mode: per_change` (one review per change, N× LLM cost, separate `## Code Review: <slug>` sections in the PR body).
- [x] 8.2 In `docs/CONFIG.md`'s `reviewer:` table, add rows for `prompt_budget_chars` and `mode`.

## 9. Spec deltas

- [x] 9.1 `openspec/changes/a07-reviewer-prompt-budget-and-per-change-mode/specs/code-reviewer/spec.md` covers the two new requirements (config-driven budget + per-change mode dispatch).
- [x] 9.2 `openspec/changes/a07-reviewer-prompt-budget-and-per-change-mode/specs/project-documentation/spec.md` covers the docs surface (CODE-REVIEW.md subsections + CONFIG.md table).

## 10. Verification

- [x] 10.1 `cargo test` passes (new + existing).
- [x] 10.2 `openspec validate a07-reviewer-prompt-budget-and-per-change-mode --strict` passes.
- [x] 10.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
