## Why

The code reviewer's prompt budget is hard-coded at 2,000,000 characters in `prompts/code-review-default.md`, sized for "current 1M-token-class models" when written. The model landscape has moved fast: Grok-4, Claude Sonnet 4.6, and several others now ship windows well past that ceiling, AND operators on those providers want to stop hitting the truncation path. Conversely, operators on smaller-window providers (some self-hosted Ollama deployments, older Claude models) may want a tighter cap to fit their provider's actual limit. The fix is one config field.

The deeper issue surfaces on stacked multi-change PRs against repos with large files. autocoder bundles up to `max_changes_per_pr` (default 3) into a single PR, and the reviewer sees the union of every touched file's full contents. When the same big file (`install.rs`, `orchestrator-cli`'s main loop) is touched by all 3 stacked changes, the reviewer hits the budget cap fast and falls into degraded mode: files dropped, diff dropped, `Concerns` verdict biased. That bundled-but-truncated path actually works (the reviewer's prompt template is designed to handle it), but operators who care about full per-change attention have no escape.

The escape hatch is per-change reviewer runs: one reviewer call per change in the PR, each with a bounded prompt focused on that change's diff + the files that change touched. 3x the LLM cost on multi-change PRs, but for operators who already pay for high-context providers AND prioritize review quality over LLM spend, it's the right trade-off. Default stays the current bundled-with-truncation behavior; per-change is opt-in.

## What Changes

**New config field `reviewer.prompt_budget_chars`** (`usize`, default `2_000_000`, max `unbounded` — there is no hard ceiling the daemon can enforce; the operator is responsible for matching it to the provider's actual context window). The existing prompt-truncation logic reads this value instead of the hard-coded constant. The default preserves today's behavior verbatim. The field is hot-applicable via `autocoder reload` (it's a `reviewer:` block field; the existing reviewer hot-reload path picks it up).

**New config field `reviewer.mode`** (`enum { bundled, per_change }`, default `bundled`). The default `bundled` is today's behavior: one reviewer call per PR, prompt budget split across every touched file from every change in the PR. The `per_change` opt-in changes the reviewer's dispatch: one call per change in the PR, each prompt scoped to that change's diff + the files that change touched. Each per-change review posts as its own `## Code Review: <change-slug>` section in the PR body (instead of one combined `## Code Review` block).

**Per-change mode preserves cross-change context.** Each per-change reviewer prompt includes a short preamble naming the OTHER changes in the same PR (slug + first-paragraph-of-`## Why`), so the reviewer sees that change A introduced a symbol change B consumes. The preamble is fixed-size (one line per other change, capped at 200 chars each) and doesn't compete with the per-change prompt budget meaningfully.

**Reviewer-initiated revisions still aggregate by PR.** The existing `auto_revise_on_block` flow (when enabled) collects revision requests from EVERY per-change review and posts them as `<!-- reviewer-revision -->`-marked PR comments. The revision dispatcher receives them as a flat list — it doesn't care whether they came from one bundled review or N per-change reviews. The cap-budget interaction (`executor.max_revisions_per_pr`) applies across the union.

**No behavior change in `bundled` mode.** Operators who never set `mode` or `prompt_budget_chars` see exactly today's behavior. The change is purely additive.

## Impact

- **Affected specs:**
  - `code-reviewer` — one MODIFIED requirement (existing review-context-and-budget requirement now reads the configured cap instead of the hard-coded value) AND one ADDED requirement (`reviewer.mode: per_change` dispatches per-change reviewer calls with their own scoped prompts).
  - `project-documentation` — one ADDED requirement: `CODE-REVIEW.md and CONFIG.md document the prompt-budget and per-change-mode fields`.
- **Affected code:**
  - `autocoder/src/config.rs` — extend `ReviewerConfig`:
    ```rust
    #[serde(default = "default_prompt_budget_chars")]
    pub prompt_budget_chars: usize,
    #[serde(default)]
    pub mode: ReviewerMode,

    #[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ReviewerMode {
        #[default]
        Bundled,
        PerChange,
    }
    ```
    Plus `fn default_prompt_budget_chars() -> usize { 2_000_000 }`. No clamping (the operator knows their provider).
  - `autocoder/src/reviewer/mod.rs` (or wherever the reviewer dispatch lives) — replace the hard-coded budget constant with `config.reviewer.prompt_budget_chars` lookups. The truncation logic stays identical; only the threshold becomes data-driven.
  - `autocoder/src/reviewer/dispatch.rs` (new, or extension to the existing dispatch) — add a per-change branch:
    ```rust
    match config.reviewer.mode {
        ReviewerMode::Bundled => run_bundled_review(...).await,
        ReviewerMode::PerChange => run_per_change_review(...).await,
    }
    ```
    `run_per_change_review` iterates `pass.changes`, builds a per-change prompt (diff scoped to that change's commit + the files that commit touched + the cross-change preamble), invokes the LLM, collects findings, and assembles a multi-section PR body.
  - `autocoder/src/pr_body.rs` (or wherever the PR body is composed) — when per-change mode is active, emit one `## Code Review: <change-slug>` section per change instead of one combined `## Code Review` block. Reviewer findings from each section feed the same `auto_revise_on_block` aggregation path.
  - `prompts/code-review-default.md` — the embedded template gains a placeholder for the cross-change preamble that's empty under bundled mode and populated under per-change mode. The literal "2,000,000 character" mention in the template is removed (the budget is now data-driven).
  - `docs/CODE-REVIEW.md` — document both new fields with the use-case framing (bundled = default; per_change for operators who want one review per change; prompt_budget_chars for operators on high-context providers).
  - `docs/CONFIG.md` — add the two fields to the `reviewer:` table.
- **Operator-visible behavior:**
  - Operators on Grok / Sonnet 4.6 / etc. with large context windows set `reviewer.prompt_budget_chars: 4_000_000` (or whatever fits) and stop hitting truncation on bundled multi-change PRs.
  - Operators who want per-change review attention set `reviewer.mode: per_change`. Their PRs gain N `## Code Review: <change>` sections instead of one combined block. LLM cost rises ~N× (one call per change instead of one per PR).
  - Operators who change neither field see identical behavior to today.
- **Breaking:** no. Both fields default to today's behavior. Existing configs deserialize unchanged.
- **Acceptance:** `cargo test` passes; `openspec validate a07-reviewer-prompt-budget-and-per-change-mode --strict` passes. New unit tests cover: bundled mode uses configured budget; per-change mode dispatches N calls for an N-change pass; per-change preamble is populated correctly; PR body composition emits N sections under per-change mode.
