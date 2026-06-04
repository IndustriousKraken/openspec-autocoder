//! AI-driven code-quality reviewer. Sends a structured `ReviewContext`
//! (changed-file contents + change-spec context + diff) to a configured LLM
//! and parses the response into a `ReviewReport`. Scope is deliberately
//! code-quality only; spec compliance is a separate verification concern.

use crate::config::ReviewerConfig;
use crate::llm::{self, LlmClient};
use crate::prompts::{PromptId, PromptLoader};
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Built-in default prompt template, embedded at compile time so the
/// binary runs without requiring `prompts/` on the filesystem. The
/// [`PromptLoader`] also references the same file via `include_str!`;
/// this alias remains here so existing anti-drift tests can compare
/// the reviewer's resolved template against the embedded source of
/// truth.
#[cfg(test)]
const DEFAULT_TEMPLATE: &str = include_str!("../../prompts/code-review-default.md");

/// Backwards-compatible default for the reviewer's prompt-body cap. The
/// real cap is read from `ReviewerConfig::prompt_budget_chars`; this
/// constant exists as the resolution target for `serde(default = ...)`
/// and the documented baseline. Operators override via `config.yaml`.
const DEFAULT_PROMPT_BUDGET: usize = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Pass,
    Concerns,
    Block,
}

#[derive(Debug, Clone)]
pub struct ReviewReport {
    pub verdict: ReviewVerdict,
    pub markdown: String,
    /// Structured per-concern records the reviewer-initiated revision
    /// pipeline reads. Populated from a trailing fenced YAML block in the
    /// LLM response (info string `revision-requests`). Older templates that
    /// don't emit the block produce an empty vec, which keeps the
    /// reviewer-initiated revision flow off for that operator's setup.
    pub concerns: Vec<ReviewConcern>,
    /// When populated, the report represents a per-change reviewer pass:
    /// each element is one `(change-slug, per-change markdown)` pair and
    /// the PR-body composer emits one `## Code Review: <slug>` section
    /// per element INSTEAD OF a single combined `## Code Review` block.
    /// Empty for bundled-mode reports.
    pub per_change_sections: Vec<PerChangeSection>,
}

/// One per-change reviewer section, surfaced into the PR body under a
/// `## Code Review: <change_slug>` heading. The `markdown` body includes
/// the per-change verdict + concerns + revision-requests in the same
/// format the bundled-mode `## Code Review` block uses.
#[derive(Debug, Clone)]
pub struct PerChangeSection {
    pub change_slug: String,
    pub markdown: String,
}

/// One concern parsed from the reviewer's structured `revision-requests`
/// block. The `summary` mirrors the bullet text in the existing markdown
/// section; `actionable_request` + `should_request_revision` are the
/// per-concern signals the reviewer-initiated revision pipeline reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewConcern {
    pub summary: String,
    #[serde(default)]
    pub actionable_request: Option<String>,
    #[serde(default)]
    pub should_request_revision: bool,
    /// Per-change attribution: in per_change reviewer mode, set to the
    /// change slug whose review surfaced this concern. Used by the
    /// dropped-cap annotator to write the "(not auto-revised; cap
    /// budget exhausted)" footer into the correct
    /// `## Code Review: <slug>` PR-body section. `None` in bundled
    /// mode (annotations land in the single `## Code Review` block).
    #[serde(default)]
    pub change_slug: Option<String>,
}

/// One archived OpenSpec change's source material. Used to give the
/// reviewer the *intent* of the change, not just the mechanical diff.
#[derive(Debug, Clone)]
pub struct ChangeBrief {
    pub name: String,
    pub proposal: String,
    pub design: Option<String>,
    pub tasks: String,
}

/// One file modified by the pass, captured at the agent-branch state.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    pub contents: String,
}

/// All the material the reviewer sees: the change(s) that shipped, the
/// resulting file state, and the unified diff. Rendering into a prompt
/// honors `ReviewerConfig::prompt_budget_chars` in priority order
/// (context > files > diff).
#[derive(Debug, Clone, Default)]
pub struct ReviewContext {
    pub archived_changes: Vec<ChangeBrief>,
    pub changed_files: Vec<ChangedFile>,
    pub diff: String,
}

/// Per-change reviewer call: the change being reviewed (own brief, own
/// diff, own touched files) plus the cross-change preamble naming the
/// other changes in the same pass. Used only when
/// `ReviewerConfig::mode == PerChange`.
#[derive(Debug, Clone)]
pub struct PerChangeContext {
    /// The change being reviewed in this call.
    pub change_slug: String,
    /// The single-change review context (briefs/files/diff scoped to
    /// this change alone).
    pub context: ReviewContext,
    /// Fixed-size cross-change preamble inserted at the top of the
    /// rendered prompt. Format: human-readable lines describing the
    /// OTHER changes in the same PR (slug + first-paragraph-of-Why),
    /// each truncated to 200 chars. Empty when the pass is single-
    /// change (no other changes to reference).
    pub cross_change_preamble: String,
}

/// One per-change reviewer result. Returned by `run_per_change_review`
/// for each change in the pass; the PR-body composer turns each one
/// into a `## Code Review: <change-slug>` section.
#[derive(Debug, Clone)]
pub struct PerChangeReview {
    pub change_slug: String,
    pub report: ReviewReport,
}

pub struct CodeReviewer {
    client: Box<dyn LlmClient>,
    template: String,
    auto_revise: bool,
    prompt_budget: usize,
    mode: crate::config::ReviewerMode,
    /// Per-PR cap on operator-initiated re-reviews. `None` means UNLIMITED
    /// (the default) — re-reviews are deliberate operator actions with no
    /// runaway path, so the cap is opt-in only.
    max_code_reviews_per_pr: Option<u32>,
    suggest_rereview_threshold: Option<f32>,
    /// a34 §6: cost-optimization knob. When `true`, the polling
    /// iteration skips the reviewer call for any PR whose diff lives
    /// entirely under `openspec/`. Defaults to `false`.
    skip_spec_only_prs: bool,
}

impl CodeReviewer {
    pub fn new(client: Box<dyn LlmClient>, template: String) -> Self {
        Self {
            client,
            template,
            auto_revise: false,
            prompt_budget: DEFAULT_PROMPT_BUDGET,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: None,
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
        }
    }

    /// Builder-style setter for the per-PR re-review cap. `None` means
    /// unlimited (the default).
    pub fn with_max_code_reviews_per_pr(mut self, cap: Option<u32>) -> Self {
        self.max_code_reviews_per_pr = cap;
        self
    }

    /// Builder-style setter for the diff-overlap re-review suggestion threshold.
    pub fn with_suggest_rereview_threshold(mut self, t: Option<f32>) -> Self {
        self.suggest_rereview_threshold = t;
        self
    }

    /// Per-PR cap on operator-initiated re-reviews (a33/a47). `None` means
    /// unlimited (the default).
    pub fn max_code_reviews_per_pr(&self) -> Option<u32> {
        self.max_code_reviews_per_pr
    }

    /// Optional diff-overlap threshold for the post-revision re-review
    /// suggestion (a33). `None` disables the suggestion.
    pub fn with_skip_spec_only_prs(mut self, b: bool) -> Self {
        self.skip_spec_only_prs = b;
        self
    }

    pub fn skip_spec_only_prs(&self) -> bool {
        self.skip_spec_only_prs
    }

    pub fn suggest_rereview_threshold(&self) -> Option<f32> {
        self.suggest_rereview_threshold
    }

    /// Builder-style setter for the prompt-budget cap. Default is
    /// `DEFAULT_PROMPT_BUDGET` (2,000,000 chars); production callers
    /// always thread the value from `ReviewerConfig::prompt_budget_chars`.
    pub fn with_prompt_budget(mut self, budget: usize) -> Self {
        self.prompt_budget = budget;
        self
    }

    /// Read the resolved prompt-budget cap (in chars). Used by the
    /// hot-reload tests to verify a config-driven update reached the
    /// live reviewer slot.
    #[allow(dead_code)]
    pub fn prompt_budget(&self) -> usize {
        self.prompt_budget
    }

    /// Builder-style setter for the reviewer dispatch mode.
    pub fn with_mode(mut self, mode: crate::config::ReviewerMode) -> Self {
        self.mode = mode;
        self
    }

    /// Read the configured dispatch mode (`Bundled` vs `PerChange`).
    pub fn mode(&self) -> crate::config::ReviewerMode {
        self.mode
    }

    /// Builder-style setter mirroring the config flag of the same name.
    /// The flag controls whether concerns marked `should_request_revision`
    /// (with a non-empty `actionable_request`) get forwarded to the
    /// revision dispatcher as `<!-- reviewer-revision -->` PR comments,
    /// regardless of the review's verdict. Default `false` (no behavioural
    /// change). Used by `from_config` to propagate
    /// `ReviewerConfig::auto_revise` onto the constructed reviewer; tests
    /// use it directly when they need the flag flipped without
    /// round-tripping a full config.
    pub fn with_auto_revise(mut self, enabled: bool) -> Self {
        self.auto_revise = enabled;
        self
    }

    /// Whether reviewer-initiated revisions are enabled for this
    /// reviewer instance. Read by the polling-loop posting step that
    /// turns actionable concerns into `<!-- reviewer-revision -->` PR
    /// comments (regardless of verdict).
    pub fn auto_revise(&self) -> bool {
        self.auto_revise
    }

    /// Wire a reviewer from config: build the LLM client, load the
    /// prompt template via the uniform [`PromptLoader`] (a24). The
    /// loader walks `reviewer.code_review.prompt_path` (nested form)
    /// → `reviewer.prompt_template_path` (legacy flat) → embedded
    /// default; missing/empty configured paths emit a one-shot WARN
    /// AND fall back to the next level.
    pub fn from_config(cfg: &ReviewerConfig) -> Result<Self> {
        let client = llm::build_from_config(cfg)?;
        let template = PromptLoader::load(
            PromptId::CodeReview,
            cfg.code_review.as_ref().and_then(|b| b.prompt_path.as_deref()),
            cfg.prompt_template_path.as_deref(),
            None,
        );
        Ok(Self::new(client, template)
            .with_auto_revise(cfg.auto_revise)
            .with_prompt_budget(cfg.prompt_budget_chars)
            .with_mode(cfg.mode)
            .with_max_code_reviews_per_pr(cfg.max_code_reviews_per_pr)
            .with_suggest_rereview_threshold(cfg.suggest_rereview_threshold)
            .with_skip_spec_only_prs(cfg.skip_spec_only_prs))
    }

    pub async fn review(&self, context: &ReviewContext) -> Result<ReviewReport> {
        self.review_with_preamble(context, "").await
    }

    /// Per-change dispatch: invokes the LLM once per `PerChangeContext`,
    /// each with that change's diff + touched files + the cross-change
    /// preamble. Each call respects `prompt_budget_chars` independently,
    /// so one change's huge file does NOT affect the other changes'
    /// reviews. Returns one `PerChangeReview` per input context, in
    /// input order; transient failures surface as `Err` for the whole
    /// pass (the polling-loop synthesizes a Concerns-verdict report).
    pub async fn review_per_change(
        &self,
        contexts: &[PerChangeContext],
    ) -> Result<Vec<PerChangeReview>> {
        let mut out = Vec::with_capacity(contexts.len());
        for pcc in contexts {
            let report = self
                .review_with_preamble(&pcc.context, &pcc.cross_change_preamble)
                .await
                .with_context(|| format!("per-change review for `{}`", pcc.change_slug))?;
            out.push(PerChangeReview {
                change_slug: pcc.change_slug.clone(),
                report,
            });
        }
        Ok(out)
    }

    /// Run the reviewer against `context`, optionally prepending
    /// `preamble` to the rendered prompt (used by per-change mode to
    /// carry the cross-change context block). An empty preamble is the
    /// bundled-mode behavior.
    pub async fn review_with_preamble(
        &self,
        context: &ReviewContext,
        preamble: &str,
    ) -> Result<ReviewReport> {
        let rendered = render_sections(context, self.prompt_budget);
        let body = self
            .template
            .replace("{{cross_change_preamble}}", preamble)
            .replace("{{change_context}}", &rendered.change_context)
            .replace("{{changed_files}}", &rendered.changed_files)
            .replace("{{diff}}", &rendered.diff_or_explanation);
        log_prompt_stats(context, &rendered, body.len(), self.prompt_budget);
        let raw = self.client.complete(&body).await?;
        Ok(parse_response(&raw))
    }
}

/// Operator-facing verdict for the re-review entry point (a33). The
/// existing `ReviewVerdict::{Pass, Concerns}` both map to `Approve`;
/// `Block` stays `Block`. The two-state surface matches the spec's
/// `Verdict (Approve | Block)` contract for operator-initiated re-reviews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Block,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Approve => "Approve",
            Verdict::Block => "Block",
        }
    }
}

impl From<ReviewVerdict> for Verdict {
    fn from(v: ReviewVerdict) -> Self {
        match v {
            ReviewVerdict::Block => Verdict::Block,
            ReviewVerdict::Pass | ReviewVerdict::Concerns => Verdict::Approve,
        }
    }
}

/// Operator-facing per-concern record (a33). Mirrors [`ReviewConcern`] but
/// kept as a separate type so the operator-trigger entry point's public
/// surface does not bind to the LLM-output parsing struct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConcernEntry {
    pub summary: String,
    pub actionable_request: Option<String>,
    pub should_request_revision: bool,
    pub change_slug: Option<String>,
}

impl From<&ReviewConcern> for ConcernEntry {
    fn from(c: &ReviewConcern) -> Self {
        Self {
            summary: c.summary.clone(),
            actionable_request: c.actionable_request.clone(),
            should_request_revision: c.should_request_revision,
            change_slug: c.change_slug.clone(),
        }
    }
}

/// Operator-facing review result (a33). Returned by
/// [`review_pr_at_state`]. Carries the verdict, per-concern records, the
/// rendered markdown body, AND the per-change sections (empty in
/// bundled mode).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ReviewResult {
    pub verdict: Verdict,
    pub per_concern: Vec<ConcernEntry>,
    pub raw_output: String,
    pub markdown: String,
    pub per_change_sections: Vec<PerChangeSection>,
    pub concerns: Vec<ReviewConcern>,
}

/// Reusable reviewer entry point (a33 task 5). The polling-loop AND the
/// operator-trigger dispatcher both invoke this function; the caller
/// decides what to do with the returned `ReviewResult` (write into PR
/// body OR post as fresh PR comment).
///
/// The function:
/// - Builds a [`CodeReviewer`] from `cfg`.
/// - Dispatches via [`CodeReviewer::review`] (bundled mode).
/// - Wraps the resulting report in a [`ReviewResult`].
///
/// Per-change mode requires the caller to pre-build per-change
/// [`PerChangeContext`] objects AND invoke [`CodeReviewer::review_per_change`]
/// directly. The single-`ReviewContext` entry point always dispatches
/// `bundled`-style.
#[allow(dead_code)]
pub async fn review_pr_at_state(
    cfg: &ReviewerConfig,
    ctx: &ReviewContext,
) -> Result<ReviewResult> {
    let reviewer = CodeReviewer::from_config(cfg)?;
    review_pr_at_state_with(&reviewer, ctx).await
}

/// Test-friendly variant: dispatches against a caller-supplied
/// [`CodeReviewer`] so unit tests can stub the LLM client. The polling-
/// loop AND operator-trigger callers use [`review_pr_at_state`] which
/// builds the reviewer from config.
pub async fn review_pr_at_state_with(
    reviewer: &CodeReviewer,
    ctx: &ReviewContext,
) -> Result<ReviewResult> {
    let report = reviewer.review(ctx).await?;
    Ok(ReviewResult {
        verdict: Verdict::from(report.verdict),
        per_concern: report.concerns.iter().map(ConcernEntry::from).collect(),
        raw_output: report.markdown.clone(),
        markdown: report.markdown.clone(),
        per_change_sections: report.per_change_sections.clone(),
        concerns: report.concerns,
    })
}

/// Emit a single INFO log line describing the rendered prompt's shape:
/// per-section bytes, per-file bytes, total vs. budget, and any files
/// dropped due to budget exhaustion. Operators rely on this to tell at a
/// glance whether a review approached the prompt-budget cap.
fn log_prompt_stats(
    ctx: &ReviewContext,
    rendered: &RenderedSections,
    prompt_bytes: usize,
    budget: usize,
) {
    let file_sizes: String = ctx
        .changed_files
        .iter()
        .map(|f| format!("{}:{}", f.path, f.contents.len()))
        .collect::<Vec<_>>()
        .join(",");
    let file_bytes_total: usize = ctx.changed_files.iter().map(|f| f.contents.len()).sum();
    let pct = prompt_bytes
        .saturating_mul(100)
        .checked_div(budget)
        .map(|p| p.min(999))
        .unwrap_or(0);
    tracing::info!(
        prompt_bytes = prompt_bytes,
        budget = budget,
        pct_of_budget = pct,
        change_context_bytes = rendered.change_context.len(),
        changed_files_bytes = rendered.changed_files.len(),
        diff_section_bytes = rendered.diff_or_explanation.len(),
        files_included = ctx.changed_files.len().saturating_sub(rendered.skipped_files.len()),
        files_skipped = rendered.skipped_files.len(),
        diff_input_bytes = ctx.diff.len(),
        file_count = ctx.changed_files.len(),
        file_content_total = file_bytes_total,
        skipped = %rendered.skipped_files.join(","),
        files = %file_sizes,
        "reviewer prompt built"
    );
}

/// Build the cross-change preamble for a per-change reviewer call:
/// names the OTHER changes in the same pass so the reviewer has cross-
/// reference context, while making clear the verdict applies only to
/// `this_change`.
///
/// Format (matches the spec's task 3.3 template):
/// ```text
/// This PR contains <N> changes. You are reviewing only `<slug>`.
/// Other changes in the same PR (for cross-reference context only — do not review them):
/// - <other-slug-1>: <other-1-summary>
/// - <other-slug-2>: <other-2-summary>
/// Your verdict applies ONLY to `<slug>`. The reviewer for each other change runs independently.
/// ```
///
/// Each `<other-summary>` is the first paragraph of the other change's
/// proposal `## Why` section, truncated to 200 chars. When the pass
/// contains a single change, the preamble is an empty string (no other
/// changes to reference).
pub fn build_cross_change_preamble(
    this_change: &str,
    all_changes: &[ChangeBrief],
) -> String {
    if all_changes.len() <= 1 {
        return String::new();
    }
    let n = all_changes.len();
    let mut out = format!(
        "This PR contains {n} changes. You are reviewing only `{this_change}`.\n\
         Other changes in the same PR (for cross-reference context only — do not review them):\n"
    );
    for brief in all_changes {
        if brief.name == this_change {
            continue;
        }
        let summary = first_paragraph_of_why(&brief.proposal);
        let truncated: String = summary.chars().take(200).collect();
        out.push_str(&format!("- {}: {}\n", brief.name, truncated));
    }
    out.push_str(&format!(
        "Your verdict applies ONLY to `{this_change}`. The reviewer for each other change runs independently.\n"
    ));
    out
}

/// Extract the first non-empty paragraph from a proposal's `## Why`
/// section. Returns an empty string when the section is absent or empty.
/// "Paragraph" = consecutive non-empty lines (joined with single spaces),
/// stopping at the first blank line or the next `## ` header.
fn first_paragraph_of_why(proposal: &str) -> String {
    let mut in_why = false;
    let mut paragraph_lines: Vec<&str> = Vec::new();
    for raw_line in proposal.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with("## ") {
            if in_why {
                break;
            }
            in_why = trimmed == "## Why";
            continue;
        }
        if !in_why {
            continue;
        }
        if line.trim().is_empty() {
            if !paragraph_lines.is_empty() {
                break;
            }
            continue;
        }
        paragraph_lines.push(line.trim());
    }
    paragraph_lines.join(" ")
}

/// Rendered substitution values for the three template placeholders, sized
/// against the configured `budget` in priority order. Pure function for
/// testability.
struct RenderedSections {
    change_context: String,
    changed_files: String,
    diff_or_explanation: String,
    /// Files whose contents were dropped to fit the budget. Empty when all
    /// files fit. Used by `review` to log a structured warning.
    skipped_files: Vec<String>,
}

fn render_sections(ctx: &ReviewContext, budget: usize) -> RenderedSections {
    // 1. Change context — always included in full. Change briefs are
    //    small (proposal/design/tasks of OpenSpec changes), so the
    //    worst-case overflow here would be a misuse anyway.
    let mut change_context = String::new();
    for brief in &ctx.archived_changes {
        if !change_context.is_empty() {
            change_context.push_str("\n\n");
        }
        change_context.push_str(&format!("## Change: {}\n\n", brief.name));
        change_context.push_str(brief.proposal.trim_end());
        if let Some(design) = brief.design.as_deref() {
            change_context.push_str("\n\n");
            change_context.push_str(design.trim_end());
        }
        change_context.push_str("\n\n");
        change_context.push_str(brief.tasks.trim_end());
    }

    // 2. Changed files — whole-file-or-skip against remaining budget.
    let mut changed_files = String::new();
    let mut skipped: Vec<String> = Vec::new();
    for file in &ctx.changed_files {
        // Approximate next-segment size: header + blank + body + trailing
        // separators. We don't need exact accounting; under-counting risks
        // pushing slightly past budget, over-counting drops files that
        // would have fit. Use a conservative additive estimate.
        let segment_len = file.path.len() + file.contents.len() + 64;
        let projected = change_context.len() + changed_files.len() + segment_len;
        if projected > budget {
            skipped.push(file.path.clone());
            continue;
        }
        if !changed_files.is_empty() {
            changed_files.push_str("\n\n");
        }
        changed_files.push_str(&format!("## File: {}\n\n", file.path));
        changed_files.push_str(&file.contents);
    }
    if !skipped.is_empty() {
        if !changed_files.is_empty() {
            changed_files.push_str("\n\n");
        }
        changed_files.push_str(&format!(
            "## Skipped (budget exhausted): {}",
            skipped.join(", ")
        ));
    }

    // 3. Diff — all-or-explanation. The diff is dropped if any files
    //    were skipped (the spec treats skipped files as the budget-
    //    exhaustion signal), OR if including the diff would push the
    //    rendered prompt past the configured budget.
    let used = change_context.len() + changed_files.len();
    let diff_or_explanation = if ctx.diff.is_empty() {
        String::from("(no diff produced this pass)")
    } else if !skipped.is_empty() || used + ctx.diff.len() > budget {
        String::from("(diff omitted: budget exhausted by change context and changed files)")
    } else {
        ctx.diff.clone()
    };

    RenderedSections {
        change_context,
        changed_files,
        diff_or_explanation,
        skipped_files: skipped,
    }
}

/// Parse the LLM response into a `ReviewReport`. Per spec, the first
/// non-empty line MUST match `(?i)^VERDICT:\s*(Pass|Concerns|Block)\s*$`.
/// If matched, the rest of the response (after that line) is the
/// `markdown`. If unmatched, the verdict defaults to `Concerns` and a
/// parse-failure note is prepended.
///
/// Additionally, a trailing fenced YAML block tagged
/// ```` ```revision-requests ```` is parsed (when present) into
/// `concerns`. The block is OPTIONAL — older reviewer templates that
/// have not been updated to emit it produce an empty `concerns` vec,
/// which keeps the reviewer-initiated revision flow inert.
fn parse_response(raw: &str) -> ReviewReport {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)^VERDICT:\s*(Pass|Concerns|Block)\s*$").unwrap());

    // Find the first non-empty line and try to parse a verdict from it.
    let mut lines = raw.lines();
    let mut found_idx: Option<usize> = None;
    let mut first_nonempty: Option<&str> = None;
    for (i, line) in raw.lines().enumerate() {
        if !line.trim().is_empty() {
            first_nonempty = Some(line.trim());
            found_idx = Some(i);
            break;
        }
    }

    let concerns = extract_revision_requests(raw);

    match (first_nonempty, found_idx) {
        (Some(line), Some(idx)) if re.is_match(line) => {
            let caps = re.captures(line).unwrap();
            let verdict = match caps.get(1).unwrap().as_str().to_ascii_lowercase().as_str() {
                "pass" => ReviewVerdict::Pass,
                "concerns" => ReviewVerdict::Concerns,
                "block" => ReviewVerdict::Block,
                _ => unreachable!("regex group is alternation of three literals"),
            };
            // Skip the verdict line; the remainder is the markdown.
            let _ = lines.nth(idx); // advances past the verdict-line index
            let remainder: Vec<&str> = lines.collect();
            let markdown = remainder.join("\n").trim_start_matches('\n').to_string();
            ReviewReport {
                verdict,
                markdown,
                concerns,
                per_change_sections: Vec::new(),
            }
        }
        _ => ReviewReport {
            verdict: ReviewVerdict::Concerns,
            markdown: format!(
                "[reviewer response did not include a valid verdict line]\n\n{raw}"
            ),
            concerns,
            per_change_sections: Vec::new(),
        },
    }
}

/// Extract the `revision-requests` fenced YAML block from `raw` (if any)
/// and parse it into `Vec<ReviewConcern>`. A missing block, an unparseable
/// block, or one that doesn't deserialize to the expected shape all yield
/// an empty vec — the schema extension is opt-in for operator-customized
/// reviewer templates, so anything other than a well-formed block is
/// treated as "no concerns to act on" rather than an error.
fn extract_revision_requests(raw: &str) -> Vec<ReviewConcern> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Match a fenced block opened with ``` (or ~~~) followed by
    // `revision-requests` (case-insensitive) as the info string, then any
    // body, then a closing fence on its own line. Multiline mode + dotall.
    let re = RE.get_or_init(|| {
        Regex::new(r"(?is)(?:^|\n)\s*```\s*revision-requests\s*\n(.*?)\n\s*```\s*(?:\n|$)")
            .expect("static regex compiles")
    });
    let body = match re.captures(raw).and_then(|c| c.get(1)) {
        Some(m) => m.as_str(),
        None => return Vec::new(),
    };
    match serde_yml::from_str::<Vec<ReviewConcern>>(body) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                "failed to parse reviewer `revision-requests` YAML block: {e}; treating as no concerns"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Test client that returns a pre-canned response and records the prompt
    /// it was asked to complete into a shared captured slot.
    struct StubClient {
        response: String,
        captured: Arc<Mutex<Option<String>>>,
    }
    #[async_trait]
    impl LlmClient for StubClient {
        async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
            *self.captured.lock().unwrap() = Some(prompt.to_string());
            Ok(self.response.clone())
        }
    }

    /// Build a stub client + a handle to its capture slot. The handle stays
    /// valid as long as the test holds it (cloned `Arc`), independent of
    /// whether the client itself has been boxed into a `CodeReviewer`.
    fn stub_with_capture(response: &str) -> (Box<StubClient>, Arc<Mutex<Option<String>>>) {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let client = Box::new(StubClient {
            response: response.to_string(),
            captured: captured.clone(),
        });
        (client, captured)
    }

    #[test]
    fn parses_pass_verdict() {
        let r = parse_response("VERDICT: Pass\n\n## Security\n- None observed.\n");
        assert_eq!(r.verdict, ReviewVerdict::Pass);
        assert!(r.markdown.contains("## Security"));
        assert!(r.markdown.contains("None observed."));
        assert!(!r.markdown.contains("VERDICT:"), "verdict line must be stripped");
    }

    #[test]
    fn parses_block_verdict() {
        let r = parse_response("VERDICT: Block\n\nSQL injection in line 42.\n");
        assert_eq!(r.verdict, ReviewVerdict::Block);
        assert!(r.markdown.contains("SQL injection"));
    }

    #[test]
    fn case_insensitive_verdict() {
        let r = parse_response("verdict: concerns\n\nminor nit.\n");
        assert_eq!(r.verdict, ReviewVerdict::Concerns);
        let r = parse_response("VERDICT:   PASS   \n\nok\n");
        assert_eq!(r.verdict, ReviewVerdict::Pass);
        let r = parse_response("VeRdIcT: BLOCK\nbad\n");
        assert_eq!(r.verdict, ReviewVerdict::Block);
    }

    #[test]
    fn parses_revision_requests_block_with_full_fields() {
        let raw = r#"VERDICT: Block

## Possible bugs
- find_user drops the error context.
- log path is computed with the wrong base directory.

```revision-requests
- summary: "find_user drops the error context"
  actionable_request: "fix find_user to propagate the underlying error via anyhow::Context"
  should_request_revision: true
- summary: "log path uses the wrong base directory"
  actionable_request: "switch the base from workspace_root to log_dir in build_log_path"
  should_request_revision: true
```
"#;
        let r = parse_response(raw);
        assert_eq!(r.verdict, ReviewVerdict::Block);
        assert_eq!(r.concerns.len(), 2);
        assert_eq!(r.concerns[0].summary, "find_user drops the error context");
        assert_eq!(
            r.concerns[0].actionable_request.as_deref(),
            Some("fix find_user to propagate the underlying error via anyhow::Context")
        );
        assert!(r.concerns[0].should_request_revision);
        assert_eq!(r.concerns[1].summary, "log path uses the wrong base directory");
        assert!(r.concerns[1].should_request_revision);
    }

    #[test]
    fn missing_revision_requests_block_yields_empty_concerns() {
        // Older reviewer template that has not been updated to emit the
        // structured block — parse must succeed and produce an empty
        // concerns vec, so the auto-revise step finds nothing actionable.
        let raw = "VERDICT: Block\n\n## Summary\nproblems here.\n";
        let r = parse_response(raw);
        assert_eq!(r.verdict, ReviewVerdict::Block);
        assert!(r.concerns.is_empty());
    }

    #[test]
    fn revision_requests_block_with_missing_fields_uses_defaults() {
        // The block is well-formed YAML but the per-concern records omit
        // `actionable_request` and `should_request_revision`. Those fields
        // must default to None / false respectively.
        let raw = r#"VERDICT: Concerns

```revision-requests
- summary: "consider naming the helper better"
- summary: "another stylistic nit"
```
"#;
        let r = parse_response(raw);
        assert_eq!(r.verdict, ReviewVerdict::Concerns);
        assert_eq!(r.concerns.len(), 2);
        for c in &r.concerns {
            assert!(c.actionable_request.is_none());
            assert!(!c.should_request_revision);
        }
    }

    #[test]
    fn malformed_revision_requests_block_yields_empty_concerns() {
        let raw = r#"VERDICT: Block

```revision-requests
this is not yaml: at all: ::: {{{ broken
```
"#;
        let r = parse_response(raw);
        // Verdict parses cleanly; the broken block is treated as no
        // concerns rather than as a parse error.
        assert_eq!(r.verdict, ReviewVerdict::Block);
        assert!(r.concerns.is_empty());
    }

    #[test]
    fn revision_requests_extracted_even_when_verdict_unparseable() {
        // Unparseable verdict line falls through to the Concerns default
        // path. The concerns extraction is independent and should still
        // surface any well-formed block (so operators can debug their
        // template even when the verdict header is broken).
        let raw = r#"oops bad header

```revision-requests
- summary: "still gets through"
  should_request_revision: true
  actionable_request: "do the thing"
```
"#;
        let r = parse_response(raw);
        assert_eq!(r.verdict, ReviewVerdict::Concerns);
        assert_eq!(r.concerns.len(), 1);
        assert!(r.concerns[0].should_request_revision);
    }

    #[test]
    fn defaults_to_concerns_on_unparseable() {
        let raw = "I think this is fine, but maybe consider X. No verdict line at all.";
        let r = parse_response(raw);
        assert_eq!(r.verdict, ReviewVerdict::Concerns);
        assert!(r.markdown.contains("[reviewer response did not include a valid verdict line]"));
        assert!(r.markdown.contains(raw), "raw response must be preserved");
    }

    #[test]
    fn unparseable_when_verdict_value_invalid() {
        // Right shape but wrong verdict word — should fall through to Concerns default.
        let r = parse_response("VERDICT: LookGoodToMe\n\nfine\n");
        assert_eq!(r.verdict, ReviewVerdict::Concerns);
        assert!(r.markdown.contains("did not include a valid verdict line"));
    }

    #[test]
    fn unparseable_when_first_nonempty_line_is_not_verdict() {
        let r = parse_response("Some preamble.\n\nVERDICT: Pass\n");
        // Spec requires the first NON-EMPTY line to be the verdict line.
        assert_eq!(r.verdict, ReviewVerdict::Concerns);
    }

    fn ctx_with_diff(diff: &str) -> ReviewContext {
        ReviewContext {
            archived_changes: Vec::new(),
            changed_files: Vec::new(),
            diff: diff.to_string(),
        }
    }

    #[tokio::test]
    async fn substitutes_template_variables() {
        let (client, captured) = stub_with_capture("VERDICT: Pass\n");
        let template = "ctx={{change_context}}\nFILES<<<{{changed_files}}>>>\nDIFF<<<{{diff}}>>>"
            .to_string();
        let reviewer = CodeReviewer::new(client, template);
        let ctx = ReviewContext {
            archived_changes: vec![ChangeBrief {
                name: "demo".into(),
                proposal: "## Why\nfor reasons".into(),
                design: None,
                tasks: "- [x] do thing".into(),
            }],
            changed_files: vec![ChangedFile {
                path: "src/foo.rs".into(),
                contents: "fn foo() {}".into(),
            }],
            diff: "the diff content".into(),
        };
        reviewer.review(&ctx).await.unwrap();

        let prompt = captured.lock().unwrap().clone().unwrap();
        assert!(prompt.contains("ctx=## Change: demo"), "got: {prompt}");
        assert!(prompt.contains("FILES<<<## File: src/foo.rs"), "got: {prompt}");
        assert!(prompt.contains("fn foo() {}"));
        assert!(prompt.contains("DIFF<<<the diff content>>>"), "got: {prompt}");
    }

    #[tokio::test]
    async fn small_diff_is_passed_through_verbatim() {
        let small_diff = "x".repeat(100);
        let (client, captured) = stub_with_capture("VERDICT: Pass\n");
        let reviewer = CodeReviewer::new(client, "{{diff}}".to_string());
        reviewer.review(&ctx_with_diff(&small_diff)).await.unwrap();
        let prompt = captured.lock().unwrap().clone().unwrap();
        assert_eq!(prompt.matches('x').count(), 100);
        assert!(!prompt.contains("budget exhausted"));
    }

    /// Priority order: change context appears before changed files, which
    /// appear before the diff.
    #[tokio::test]
    async fn review_renders_change_context_before_files_before_diff() {
        let (client, captured) = stub_with_capture("VERDICT: Pass\n");
        let template = "{{change_context}}|{{changed_files}}|{{diff}}".to_string();
        let reviewer = CodeReviewer::new(client, template);
        let ctx = ReviewContext {
            archived_changes: vec![ChangeBrief {
                name: "alpha".into(),
                proposal: "PROP_SENTINEL".into(),
                design: None,
                tasks: "TASKS_SENTINEL".into(),
            }],
            changed_files: vec![ChangedFile {
                path: "src/a.rs".into(),
                contents: "FILE_SENTINEL".into(),
            }],
            diff: "DIFF_SENTINEL".into(),
        };
        reviewer.review(&ctx).await.unwrap();
        let prompt = captured.lock().unwrap().clone().unwrap();
        let prop_i = prompt.find("PROP_SENTINEL").expect("proposal present");
        let file_i = prompt.find("FILE_SENTINEL").expect("file present");
        let diff_i = prompt.find("DIFF_SENTINEL").expect("diff present");
        assert!(prop_i < file_i, "change context must precede files");
        assert!(file_i < diff_i, "files must precede diff");
    }

    /// Two files large enough to bust the budget together: the second one
    /// is skipped, listed in the skip footer, and the diff is replaced by
    /// the budget-exhausted explanation.
    #[tokio::test]
    async fn skips_files_when_budget_exhausts() {
        let (client, captured) = stub_with_capture("VERDICT: Pass\n");
        let template = "{{change_context}}|{{changed_files}}|{{diff}}".to_string();
        let reviewer = CodeReviewer::new(client, template);
        // Each file ~1.5MB; together they exceed the 2MB budget.
        let big = "y".repeat(1_500_000);
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![
                ChangedFile {
                    path: "first.rs".into(),
                    contents: big.clone(),
                },
                ChangedFile {
                    path: "second.rs".into(),
                    contents: big.clone(),
                },
            ],
            diff: "DIFF_SENTINEL".into(),
        };
        reviewer.review(&ctx).await.unwrap();
        let prompt = captured.lock().unwrap().clone().unwrap();
        assert!(prompt.contains("first.rs"), "first file must be present");
        assert!(
            prompt.contains("## Skipped (budget exhausted): second.rs"),
            "second file must be in skip list; got prompt of {} bytes",
            prompt.len()
        );
        assert!(
            prompt.contains("(diff omitted: budget exhausted by change context and changed files)"),
            "diff must be replaced by the budget-exhausted explanation"
        );
        assert!(
            !prompt.contains("DIFF_SENTINEL"),
            "actual diff must not appear when budget is exhausted"
        );
    }

    /// A single file larger than the whole budget: file is skipped in
    /// full (never partially included).
    #[tokio::test]
    async fn never_truncates_individual_file() {
        let (client, captured) = stub_with_capture("VERDICT: Pass\n");
        let reviewer = CodeReviewer::new(client, "{{changed_files}}".to_string());
        let huge = "z".repeat(DEFAULT_PROMPT_BUDGET + 100_000);
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "huge.rs".into(),
                contents: huge,
            }],
            diff: String::new(),
        };
        reviewer.review(&ctx).await.unwrap();
        let prompt = captured.lock().unwrap().clone().unwrap();
        // Either fully present or fully skipped — no partial slice. With
        // ~2.1MB content vs 2MB budget, we expect "skipped".
        assert!(
            prompt.contains("## Skipped (budget exhausted): huge.rs"),
            "huge file must be wholly skipped"
        );
        // The actual content (`zzz...`) must NOT have leaked into the
        // prompt — if it did, we'd see thousands of 'z' characters.
        let z_count = prompt.matches('z').count();
        assert_eq!(z_count, 0, "no partial file contents should leak into prompt");
    }

    /// Pure-function test for `render_sections`: verifies priority order
    /// and skip-list behavior without needing a stub LLM client.
    #[test]
    fn render_sections_priority_order_pure() {
        let ctx = ReviewContext {
            archived_changes: vec![ChangeBrief {
                name: "x".into(),
                proposal: "P".into(),
                design: Some("D".into()),
                tasks: "T".into(),
            }],
            changed_files: vec![ChangedFile {
                path: "a.rs".into(),
                contents: "BODY".into(),
            }],
            diff: "DELTA".into(),
        };
        let r = render_sections(&ctx, DEFAULT_PROMPT_BUDGET);
        assert!(r.change_context.contains("## Change: x"));
        assert!(r.change_context.contains("P\n\nD\n\nT"));
        assert!(r.changed_files.contains("## File: a.rs"));
        assert!(r.changed_files.contains("BODY"));
        assert_eq!(r.diff_or_explanation, "DELTA");
    }

    /// a34 §6: `skip_spec_only_prs` defaults to `false` AND propagates
    /// from `ReviewerConfig` via `from_config`. This is the gate the
    /// polling iteration consults before invoking the reviewer call.
    #[test]
    fn skip_spec_only_prs_defaults_false_and_propagates() {
        use crate::config::{ReviewerConfig, ReviewerProvider};
        // Default: unset → false.
        unsafe { std::env::set_var("REVIEWER_TEST_SKIP_DEFAULT", "k") };
        let cfg_default = ReviewerConfig {
            enabled: true,
            provider: ReviewerProvider::Anthropic,
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_SKIP_DEFAULT".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
        };
        let r_default = CodeReviewer::from_config(&cfg_default)
            .expect("default-config builds");
        assert!(
            !r_default.skip_spec_only_prs(),
            "default must be false: got {}",
            r_default.skip_spec_only_prs()
        );

        // Explicit true: propagates.
        unsafe { std::env::set_var("REVIEWER_TEST_SKIP_TRUE", "k") };
        let cfg_true = ReviewerConfig {
            enabled: true,
            provider: ReviewerProvider::Anthropic,
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_SKIP_TRUE".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: true,
        };
        let r_true = CodeReviewer::from_config(&cfg_true)
            .expect("skip=true builds");
        assert!(
            r_true.skip_spec_only_prs(),
            "true must propagate: got {}",
            r_true.skip_spec_only_prs()
        );
        unsafe { std::env::remove_var("REVIEWER_TEST_SKIP_DEFAULT") };
        unsafe { std::env::remove_var("REVIEWER_TEST_SKIP_TRUE") };
    }

    /// a34 §6.2: a brownfield iteration's PR has only
    /// `openspec/changes/<change>/...` diff → `diff_is_spec_only`
    /// returns true. With `skip_spec_only_prs: true` the polling
    /// iteration's gate skips the reviewer call (verified via the
    /// predicate the polling code consults).
    #[test]
    fn diff_is_spec_only_classifies_brownfield_pr_correctly() {
        use crate::spec_storage_routing::diff_is_spec_only;
        let brownfield_paths = vec![
            "openspec/changes/a36-brownfield-foo/proposal.md".to_string(),
            "openspec/changes/a36-brownfield-foo/tasks.md".to_string(),
            "openspec/changes/a36-brownfield-foo/specs/foo/spec.md".to_string(),
        ];
        assert!(
            diff_is_spec_only(&brownfield_paths),
            "brownfield PR classifies as spec-only"
        );
    }

    /// a34 §6.3: a dual-tree iteration's code PR has
    /// `autocoder/src/foo.rs` diff → `diff_is_spec_only` returns
    /// false. The polling iteration's gate runs the reviewer normally.
    #[test]
    fn diff_is_spec_only_classifies_dual_tree_code_pr_correctly() {
        use crate::spec_storage_routing::diff_is_spec_only;
        let dual_code_paths = vec![
            "autocoder/src/foo.rs".to_string(),
            "openspec/changes/a36/proposal.md".to_string(),
        ];
        assert!(
            !diff_is_spec_only(&dual_code_paths),
            "dual-tree's code PR is NOT spec-only"
        );
    }

    /// a34 §6.4 (default behavior): with `skip_spec_only_prs: false`,
    /// the reviewer would be invoked even on a spec-only diff. The
    /// accessor returns false → the gate condition evaluates to false →
    /// the reviewer-invocation branch is taken.
    #[test]
    fn skip_spec_only_prs_false_does_not_short_circuit_gate() {
        use crate::config::{ReviewerConfig, ReviewerProvider};
        unsafe { std::env::set_var("REVIEWER_TEST_SKIP_FALSE_GATE", "k") };
        let cfg = ReviewerConfig {
            enabled: true,
            provider: ReviewerProvider::Anthropic,
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_SKIP_FALSE_GATE".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
        };
        let r = CodeReviewer::from_config(&cfg).expect("config builds");
        unsafe { std::env::remove_var("REVIEWER_TEST_SKIP_FALSE_GATE") };
        // The polling-loop gate evaluates `r.skip_spec_only_prs() &&
        // diff_is_spec_only(...)`. When the first conjunct is false,
        // the gate is unconditionally false — the reviewer is invoked.
        assert!(
            !r.skip_spec_only_prs(),
            "default-false config keeps the gate inactive"
        );
    }

    #[test]
    fn from_config_reads_user_provided_template() {
        use crate::config::{ReviewerConfig, ReviewerProvider};
        let dir = tempfile::TempDir::new().unwrap();
        let template_path = dir.path().join("custom.md");
        std::fs::write(&template_path, "CUSTOM TEMPLATE: {{diff}}").unwrap();

        // Set the env var the config will read.
        unsafe { std::env::set_var("REVIEWER_TEST_KEY_OVERRIDE", "k") };
        let cfg = ReviewerConfig {
            enabled: true,
            provider: ReviewerProvider::Anthropic,
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_KEY_OVERRIDE".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: Some(template_path),
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
        };
        let reviewer = CodeReviewer::from_config(&cfg).expect("should load custom template");
        unsafe { std::env::remove_var("REVIEWER_TEST_KEY_OVERRIDE") };

        // Template identity, not wording: the loaded value is exactly the
        // synthetic custom template this test wrote, AND is distinct from
        // the embedded default (symbol comparison, no prose substring).
        assert_eq!(
            reviewer.template, "CUSTOM TEMPLATE: {{diff}}",
            "loaded template must equal the synthetic custom template the test wrote"
        );
        assert_ne!(
            reviewer.template, DEFAULT_TEMPLATE,
            "loaded custom template must not equal the embedded default"
        );
    }

    /// A missing prompt-template override path now falls back to the
    /// embedded default via the uniform `PromptLoader` (a24). The
    /// daemon does NOT abort start-up; instead a one-shot WARN names
    /// the offending path.
    #[test]
    fn from_config_falls_back_when_template_path_missing() {
        use crate::config::{ReviewerConfig, ReviewerProvider};
        unsafe { std::env::set_var("REVIEWER_TEST_KEY_MISSING_TMPL", "k") };
        let bogus = std::path::PathBuf::from("/nonexistent/orchestrator-test-template.md");
        let cfg = ReviewerConfig {
            enabled: true,
            provider: ReviewerProvider::Anthropic,
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_KEY_MISSING_TMPL".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: Some(bogus.clone()),
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
        };
        let reviewer = CodeReviewer::from_config(&cfg)
            .expect("missing template must fall back to embedded default");
        unsafe { std::env::remove_var("REVIEWER_TEST_KEY_MISSING_TMPL") };
        assert_eq!(
            reviewer.template, DEFAULT_TEMPLATE,
            "fallback must use the embedded default template (symbol identity)"
        );
    }

    /// The new `reviewer.code_review.prompt_path` nested form takes
    /// precedence over the legacy flat `reviewer.prompt_template_path`
    /// when both are set AND the nested file exists (a24).
    #[test]
    fn from_config_nested_form_preempts_legacy_for_reviewer() {
        use crate::config::{
            PromptOverrideBlock, ReviewerConfig, ReviewerProvider,
        };
        use tempfile::TempDir;
        unsafe { std::env::set_var("REVIEWER_TEST_KEY_NESTED", "k") };
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested-review.md");
        let legacy = tmp.path().join("legacy-review.md");
        std::fs::write(&nested, "NESTED REVIEW TEMPLATE").unwrap();
        std::fs::write(&legacy, "LEGACY REVIEW TEMPLATE").unwrap();
        let cfg = ReviewerConfig {
            enabled: true,
            provider: ReviewerProvider::Anthropic,
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_KEY_NESTED".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: Some(legacy),
            code_review: Some(PromptOverrideBlock {
                prompt_path: Some(nested),
            }),
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
        };
        let reviewer = CodeReviewer::from_config(&cfg)
            .expect("nested override resolves");
        unsafe { std::env::remove_var("REVIEWER_TEST_KEY_NESTED") };
        assert!(reviewer.template.contains("NESTED REVIEW TEMPLATE"));
        assert!(!reviewer.template.contains("LEGACY REVIEW TEMPLATE"));
    }

    #[test]
    fn from_config_uses_default_template_when_path_omitted() {
        use crate::config::{ReviewerConfig, ReviewerProvider};
        unsafe { std::env::set_var("REVIEWER_TEST_KEY_DEFAULT", "k") };
        let cfg = ReviewerConfig {
            enabled: true,
            provider: ReviewerProvider::Anthropic,
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_KEY_DEFAULT".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: false,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
        };
        let reviewer = CodeReviewer::from_config(&cfg).expect("default template loads");
        unsafe { std::env::remove_var("REVIEWER_TEST_KEY_DEFAULT") };
        assert_eq!(
            reviewer.template, DEFAULT_TEMPLATE,
            "default template must be used when prompt_template_path is None (symbol identity)"
        );
    }

    /// A higher prompt-budget cap lets a file through that the default
    /// cap would have skipped. Demonstrates the field is data-driven.
    #[tokio::test]
    async fn higher_prompt_budget_admits_files_default_would_skip() {
        let (client, captured) = stub_with_capture("VERDICT: Pass\n");
        let reviewer = CodeReviewer::new(client, "{{changed_files}}".to_string())
            .with_prompt_budget(4_000_000);
        // 3MB file: the default 2MB cap would skip it; the 4MB cap admits it.
        let three_mb = "y".repeat(3_000_000);
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "big.rs".into(),
                contents: three_mb,
            }],
            diff: String::new(),
        };
        reviewer.review(&ctx).await.unwrap();
        let prompt = captured.lock().unwrap().clone().unwrap();
        assert!(prompt.contains("## File: big.rs"));
        assert!(
            !prompt.contains("## Skipped (budget exhausted)"),
            "no skip footer expected when cap fits the content"
        );
    }

    /// Default `prompt_budget` from `CodeReviewer::new` matches
    /// `DEFAULT_PROMPT_BUDGET` (the historical hard-coded value).
    #[test]
    fn default_prompt_budget_matches_historical_constant() {
        let (client, _) = stub_with_capture("VERDICT: Pass\n");
        let reviewer = CodeReviewer::new(client, "irrelevant".into());
        assert_eq!(reviewer.prompt_budget(), DEFAULT_PROMPT_BUDGET);
        assert_eq!(reviewer.prompt_budget(), 2_000_000);
    }

    #[test]
    fn build_cross_change_preamble_single_change_is_empty() {
        let briefs = vec![ChangeBrief {
            name: "only-one".into(),
            proposal: "## Why\nfor reasons\n".into(),
            design: None,
            tasks: String::new(),
        }];
        assert_eq!(
            build_cross_change_preamble("only-one", &briefs),
            "",
            "single-change pass produces empty preamble"
        );
    }

    #[test]
    fn build_cross_change_preamble_lists_other_changes() {
        let briefs = vec![
            ChangeBrief {
                name: "a".into(),
                proposal: "## Why\nfix the auth bug\n".into(),
                design: None,
                tasks: String::new(),
            },
            ChangeBrief {
                name: "b".into(),
                proposal: "## Why\nadd metrics emission\n".into(),
                design: None,
                tasks: String::new(),
            },
            ChangeBrief {
                name: "c".into(),
                proposal: "## Why\nrefactor dispatcher\n".into(),
                design: None,
                tasks: String::new(),
            },
        ];
        let p = build_cross_change_preamble("b", &briefs);
        // Must reference the change being reviewed, mention the count,
        // and name the OTHER changes — never itself.
        assert!(p.contains("This PR contains 3 changes"));
        assert!(p.contains("`b`"));
        assert!(p.contains("- a: fix the auth bug"));
        assert!(p.contains("- c: refactor dispatcher"));
        // Must NOT include the reviewed change in the "others" list.
        assert!(!p.contains("- b: add metrics emission"));
        // Must end with the verdict-scope reminder.
        assert!(p.contains("Your verdict applies ONLY to `b`"));
    }

    #[test]
    fn build_cross_change_preamble_truncates_long_why_to_200_chars() {
        // Use a sentinel char that does NOT appear anywhere in the
        // surrounding preamble template, so we can count its occurrences
        // and isolate the truncation behavior.
        let long_why = "Z".repeat(500);
        let briefs = vec![
            ChangeBrief {
                name: "self".into(),
                proposal: "## Why\nshort\n".into(),
                design: None,
                tasks: String::new(),
            },
            ChangeBrief {
                name: "other".into(),
                proposal: format!("## Why\n{long_why}\n"),
                design: None,
                tasks: String::new(),
            },
        ];
        let p = build_cross_change_preamble("self", &briefs);
        // The line is `- other: <truncated to 200 chars>\n`; we expect
        // exactly 200 Z's, not 500.
        let z_count = p.matches('Z').count();
        assert_eq!(z_count, 200);
    }

    #[tokio::test]
    async fn review_per_change_invokes_llm_once_per_change() {
        // We need to track each call. Use the StubClient pattern but
        // record every prompt observed (not just the last).
        use std::sync::Mutex;
        struct CountingClient {
            prompts: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl LlmClient for CountingClient {
            async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
                self.prompts.lock().unwrap().push(prompt.to_string());
                Ok("VERDICT: Pass\n".to_string())
            }
        }
        let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let client = Box::new(CountingClient { prompts: prompts.clone() });
        let template = "PREAMBLE<<{{cross_change_preamble}}>>FILES<<{{changed_files}}>>".to_string();
        let reviewer = CodeReviewer::new(client, template)
            .with_mode(crate::config::ReviewerMode::PerChange);

        let briefs = vec![
            ChangeBrief {
                name: "a".into(),
                proposal: "## Why\nfor a reasons\n".into(),
                design: None,
                tasks: String::new(),
            },
            ChangeBrief {
                name: "b".into(),
                proposal: "## Why\nfor b reasons\n".into(),
                design: None,
                tasks: String::new(),
            },
            ChangeBrief {
                name: "c".into(),
                proposal: "## Why\nfor c reasons\n".into(),
                design: None,
                tasks: String::new(),
            },
        ];
        let contexts: Vec<PerChangeContext> = briefs
            .iter()
            .map(|b| PerChangeContext {
                change_slug: b.name.clone(),
                context: ReviewContext {
                    archived_changes: vec![b.clone()],
                    changed_files: vec![ChangedFile {
                        path: format!("{}.rs", b.name),
                        contents: format!("body of {}", b.name),
                    }],
                    diff: format!("diff of {}", b.name),
                },
                cross_change_preamble: build_cross_change_preamble(&b.name, &briefs),
            })
            .collect();

        let results = reviewer.review_per_change(&contexts).await.unwrap();
        assert_eq!(results.len(), 3);
        let captured = prompts.lock().unwrap();
        assert_eq!(captured.len(), 3, "one LLM call per change");
        // Each prompt must contain ONLY its own file's body and a
        // preamble naming the OTHER two changes.
        for (i, slug) in ["a", "b", "c"].iter().enumerate() {
            let p = &captured[i];
            assert!(p.contains(&format!("body of {slug}")), "prompt {i}: own body");
            for other in ["a", "b", "c"].iter() {
                if other != slug {
                    assert!(
                        p.contains(&format!("- {other}: for {other} reasons")),
                        "prompt {i}: preamble must name other slug {other}"
                    );
                }
            }
            // Must NOT contain the verdict-scope line for any OTHER change.
            assert!(
                p.contains(&format!("`{slug}`")),
                "prompt {i}: self-reference in preamble"
            );
        }
        for r in &results {
            assert_eq!(r.report.verdict, ReviewVerdict::Pass);
        }
    }

    /// Per-change budget enforcement is per-call: a huge file in change
    /// A produces a skip footer in A's section but does NOT affect B's
    /// or C's reviews.
    #[tokio::test]
    async fn review_per_change_budgets_are_independent() {
        use std::sync::Mutex;
        struct EchoClient {
            prompts: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl LlmClient for EchoClient {
            async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
                self.prompts.lock().unwrap().push(prompt.to_string());
                Ok("VERDICT: Pass\n".to_string())
            }
        }
        let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let client = Box::new(EchoClient { prompts: prompts.clone() });
        let reviewer = CodeReviewer::new(client, "{{changed_files}}".to_string())
            .with_prompt_budget(1_000_000)
            .with_mode(crate::config::ReviewerMode::PerChange);
        // Change A: a huge file (way over the 1MB cap).
        let huge = "z".repeat(2_000_000);
        let a_ctx = PerChangeContext {
            change_slug: "a".into(),
            context: ReviewContext {
                archived_changes: Vec::new(),
                changed_files: vec![ChangedFile {
                    path: "huge.rs".into(),
                    contents: huge,
                }],
                diff: String::new(),
            },
            cross_change_preamble: String::new(),
        };
        // Change B: a tiny file (well under cap).
        let b_ctx = PerChangeContext {
            change_slug: "b".into(),
            context: ReviewContext {
                archived_changes: Vec::new(),
                changed_files: vec![ChangedFile {
                    path: "tiny.rs".into(),
                    contents: "fn ok() {}".into(),
                }],
                diff: String::new(),
            },
            cross_change_preamble: String::new(),
        };
        let _ = reviewer
            .review_per_change(&[a_ctx, b_ctx])
            .await
            .unwrap();
        let captured = prompts.lock().unwrap();
        assert_eq!(captured.len(), 2);
        // A's prompt: skipped footer must fire.
        assert!(
            captured[0].contains("## Skipped (budget exhausted): huge.rs"),
            "change A's huge file must trigger its own skip footer"
        );
        // B's prompt: must contain the tiny file in full, and NO skip footer.
        assert!(captured[1].contains("fn ok() {}"));
        assert!(
            !captured[1].contains("## Skipped (budget exhausted)"),
            "change B's review must NOT be affected by change A's truncation"
        );
    }

    /// Task 5.3: `review_pr_at_state_with` against a stub LLM returning
    /// a canned `Pass` verdict produces `ReviewResult { verdict: Approve, ... }`.
    #[tokio::test]
    async fn review_pr_at_state_approves_on_pass_verdict() {
        let (client, _) = stub_with_capture("VERDICT: Pass\n\nAll good.\n");
        let reviewer = CodeReviewer::new(client, "{{diff}}".to_string());
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: Vec::new(),
            diff: "some diff".to_string(),
        };
        let result = review_pr_at_state_with(&reviewer, &ctx)
            .await
            .expect("review succeeds");
        assert_eq!(result.verdict, Verdict::Approve);
        assert!(result.markdown.contains("All good."));
    }

    /// Task 5.3 cont'd: `Concerns` verdict ALSO maps to `Approve` on the
    /// operator-facing surface.
    #[tokio::test]
    async fn review_pr_at_state_approves_on_concerns_verdict() {
        let (client, _) = stub_with_capture("VERDICT: Concerns\n\nminor nits.\n");
        let reviewer = CodeReviewer::new(client, "{{diff}}".to_string());
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: Vec::new(),
            diff: "some diff".to_string(),
        };
        let result = review_pr_at_state_with(&reviewer, &ctx)
            .await
            .expect("review succeeds");
        assert_eq!(result.verdict, Verdict::Approve);
    }

    /// Task 5.4: `Block` verdict surfaces as `Block` AND any concerns
    /// from the trailing `revision-requests` block are preserved.
    #[tokio::test]
    async fn review_pr_at_state_blocks_on_block_verdict() {
        let raw = "VERDICT: Block\n\nSerious issue.\n\n```revision-requests\n- summary: \"fix the broken thing\"\n  should_request_revision: true\n```\n";
        let (client, _) = stub_with_capture(raw);
        let reviewer = CodeReviewer::new(client, "{{diff}}".to_string());
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: Vec::new(),
            diff: "some diff".to_string(),
        };
        let result = review_pr_at_state_with(&reviewer, &ctx)
            .await
            .expect("review succeeds");
        assert_eq!(result.verdict, Verdict::Block);
        assert_eq!(result.per_concern.len(), 1);
        assert!(result.per_concern[0].should_request_revision);
        assert_eq!(result.per_concern[0].summary, "fix the broken thing");
    }

    /// Task 5.5: the extracted entry point's output (markdown body) is
    /// byte-identical to what `CodeReviewer::review`'s `ReviewReport`
    /// would have produced for the same inputs. Confirms the
    /// extraction is refactor-only.
    #[tokio::test]
    async fn review_pr_at_state_byte_identical_to_review_report() {
        let raw = "VERDICT: Pass\n\nNothing of note.\n";
        let (client_a, _) = stub_with_capture(raw);
        let reviewer_a = CodeReviewer::new(client_a, "{{diff}}".to_string());
        let ctx_a = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: Vec::new(),
            diff: "abc".to_string(),
        };
        let report = reviewer_a.review(&ctx_a).await.unwrap();

        let (client_b, _) = stub_with_capture(raw);
        let reviewer_b = CodeReviewer::new(client_b, "{{diff}}".to_string());
        let ctx_b = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: Vec::new(),
            diff: "abc".to_string(),
        };
        let result = review_pr_at_state_with(&reviewer_b, &ctx_b)
            .await
            .unwrap();
        assert_eq!(result.markdown, report.markdown);
        assert_eq!(result.concerns.len(), report.concerns.len());
        assert_eq!(Verdict::from(report.verdict), result.verdict);
    }

    /// Behavior test (a48): the shipped default template must reference
    /// all three substitution placeholders, because the production render
    /// path (`review_with_preamble`) fills them. Rendering the real
    /// default with a distinct sentinel per placeholder and asserting each
    /// sentinel survives proves the references exist — without pinning any
    /// of the template's hand-authored instruction prose (per the
    /// project-documentation requirement "Tests assert behavior or
    /// derivation, never message wording").
    #[test]
    fn default_template_references_all_placeholders() {
        let rendered = DEFAULT_TEMPLATE
            .replace("{{change_context}}", "SENTINEL_CHANGE_CONTEXT_a48")
            .replace("{{changed_files}}", "SENTINEL_CHANGED_FILES_a48")
            .replace("{{diff}}", "SENTINEL_DIFF_a48");
        assert!(
            rendered.contains("SENTINEL_CHANGE_CONTEXT_a48"),
            "default template must reference the {{change_context}} placeholder"
        );
        assert!(
            rendered.contains("SENTINEL_CHANGED_FILES_a48"),
            "default template must reference the {{changed_files}} placeholder"
        );
        assert!(
            rendered.contains("SENTINEL_DIFF_a48"),
            "default template must reference the {{diff}} placeholder"
        );
    }
}
