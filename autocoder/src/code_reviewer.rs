//! AI-driven code-quality reviewer. Sends a structured `ReviewContext`
//! (changed-file contents + change-spec context + diff) to a configured LLM
//! and parses the response into a `ReviewReport`. Scope is deliberately
//! code-quality only; spec compliance is a separate verification concern.

use crate::config::{LlmProvider, ReviewerConfig, ReviewerKind};
use crate::llm::{self, LlmClient};
use crate::prompts::{PromptId, PromptLoader};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

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
    /// Redaction-safe `<provider>/<model>` model attribution for the
    /// reviewer that produced this report (a49). `Some` when the reviewer
    /// was built from a config carrying a `(provider, model)`; the PR-body
    /// composer renders it as `*Reviewer: <provider>/<model>*`. `None` for
    /// reports built without a configured reviewer (e.g. test fixtures or
    /// the reviewer-failed synthetic report), in which case no attribution
    /// line is emitted.
    pub attribution: Option<String>,
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
    /// The reviewer's own structured security signal (a004). `true` when the
    /// reviewer classified this finding as a credential/secret/key exposure
    /// or an injection vulnerability. The verdict-handling path escalates the
    /// effective verdict to `Block` when any concern carries this flag (see
    /// [`concerns_flag_security_critical`]) — keyed on this signal, NEVER on a
    /// substring scan of `summary`. `#[serde(default)]` keeps older
    /// reviewer templates (which omit the field) parsing as `false`.
    #[serde(default)]
    pub security_critical: bool,
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

impl ReviewConcern {
    /// Whether this concern is an actionable reviewer-initiated revision
    /// request: it carries `should_request_revision: true` AND a non-empty
    /// (whitespace-trimmed) `actionable_request`. The auto-revise aggregation
    /// (a005) collects every revisable concern from one review into a single
    /// revision run. The verdict is NOT consulted here — verdict gating is
    /// the caller's `auto_revise` tri-state decision.
    pub fn is_revisable(&self) -> bool {
        self.should_request_revision
            && self
                .actionable_request
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
    }
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
    auto_revise: crate::config::AutoRevise,
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
    /// Redaction-safe `<provider>/<model>` attribution (a49), stamped onto
    /// every [`ReviewReport`] this reviewer produces. `Some` when built via
    /// [`CodeReviewer::from_config`]; `None` for the test-only
    /// [`CodeReviewer::new`] path (no config, no model to attribute).
    attribution: Option<String>,
    /// a58: reviewer transport. `Oneshot` (default) keeps the existing HTTP
    /// path; `Agentic` routes through the shared `agentic_run` primitive.
    kind: ReviewerKind,
    /// a58: the CLI binary the agentic path wraps (default `claude`).
    command: String,
    /// a58: the reviewer's LLM provider, used to resolve the agentic CLI
    /// strategy via the a55 `provider → CLI` rule. Anthropic for the
    /// test-only [`CodeReviewer::new`] path.
    provider: LlmProvider,
    /// a67: file/function line thresholds for the advisory size flag. The
    /// reviewer appends a `## Size advisory` note when a pass pushes a
    /// changed file or function past these, OR grows one already over.
    /// Default to the same values the `architecture-brightline` audit
    /// applies (file `800`, function `200`).
    file_lines_threshold: u64,
    function_lines_threshold: u64,
}

impl CodeReviewer {
    pub fn new(client: Box<dyn LlmClient>, template: String) -> Self {
        Self {
            client,
            template,
            // Test-only constructor: default to `Off` so a reviewer built via
            // `new()` does not auto-revise unless a test opts in with
            // `with_auto_revise`. (The config-driven default is `Block`; see
            // `AutoRevise::default`.)
            auto_revise: crate::config::AutoRevise::Off,
            prompt_budget: DEFAULT_PROMPT_BUDGET,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: None,
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            attribution: None,
            kind: ReviewerKind::Oneshot,
            command: "claude".to_string(),
            provider: LlmProvider::Anthropic,
            file_lines_threshold: crate::audits::brightline::DEFAULT_FILE_LINES_THRESHOLD,
            function_lines_threshold: crate::audits::brightline::DEFAULT_FUNCTION_LINES_THRESHOLD,
        }
    }

    /// Builder-style setter for the advisory size-flag thresholds (a67).
    /// Defaults match the `architecture-brightline` audit (file `800`,
    /// function `200`); `from_config` leaves the defaults in place since
    /// `ReviewerConfig` carries no per-reviewer override. Exercised by the
    /// size-advisory tests.
    #[allow(dead_code)]
    pub fn with_size_thresholds(mut self, file_lines: u64, function_lines: u64) -> Self {
        self.file_lines_threshold = file_lines;
        self.function_lines_threshold = function_lines;
        self
    }

    /// Builder-style setter for the reviewer transport (`oneshot` vs
    /// `agentic`). `from_config` sets this from `reviewer.kind`.
    pub fn with_kind(mut self, kind: ReviewerKind) -> Self {
        self.kind = kind;
        self
    }

    /// Read the configured reviewer transport.
    pub fn kind(&self) -> ReviewerKind {
        self.kind
    }

    /// Builder-style setter for the agentic CLI command (`reviewer.command`).
    pub fn with_command(mut self, command: String) -> Self {
        self.command = command;
        self
    }

    /// Builder-style setter for the reviewer's LLM provider, used to resolve
    /// the agentic CLI strategy via the a55 `provider → CLI` rule.
    pub fn with_provider(mut self, provider: LlmProvider) -> Self {
        self.provider = provider;
        self
    }

    /// Builder-style setter for the redaction-safe model attribution (a49).
    /// `from_config` sets this from the reviewer config's `(provider,
    /// model)`. The attribution is stamped onto every [`ReviewReport`] this
    /// reviewer produces and carried into [`ReviewResult`], from which the
    /// initial-review and rerun composers render the `*Reviewer: …*` line.
    pub fn with_attribution(mut self, attribution: Option<String>) -> Self {
        self.attribution = attribution;
        self
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

    /// Builder-style setter mirroring the tri-state config field of the
    /// same name (a005). It controls whether — AND under which verdict —
    /// concerns marked `should_request_revision` (with a non-empty
    /// `actionable_request`) get forwarded (aggregated into a single
    /// revision run) to the revision dispatcher. Used by `from_config` to
    /// propagate `ReviewerConfig::auto_revise` onto the constructed
    /// reviewer; tests use it directly when they need a specific mode
    /// without round-tripping a full config.
    pub fn with_auto_revise(mut self, mode: crate::config::AutoRevise) -> Self {
        self.auto_revise = mode;
        self
    }

    /// The reviewer-initiated revision mode for this reviewer instance
    /// (a005 tri-state). Read by the posting step that turns actionable
    /// concerns into the single aggregated `<!-- reviewer-revision -->` PR
    /// comment; the caller combines it with the review's verdict via
    /// [`crate::config::AutoRevise::fires`].
    pub fn auto_revise(&self) -> crate::config::AutoRevise {
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
            .with_skip_spec_only_prs(cfg.skip_spec_only_prs)
            .with_kind(cfg.kind)
            .with_command(cfg.command.clone())
            // After `Config::load_from`, `provider` is always resolved; the
            // unwrap default mirrors the field's documented post-load
            // invariant.
            .with_provider(cfg.provider.unwrap_or(LlmProvider::Anthropic))
            .with_attribution(Some(crate::attribution::AttributionSurface::attribution(cfg))))
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
        // Single-pass substitution (a002): a `{{...}}` token appearing
        // inside a substituted value — most importantly inside the
        // `{{changed_files}}` value when the change under review touches a
        // template, docs, OR the reviewer's own code/specs — is emitted
        // verbatim, never re-expanded. Chained `.replace` re-scanned
        // injected content and could multiply the prompt past the model's
        // context limit.
        let body = crate::prompts::render_template(
            &self.template,
            &[
                ("cross_change_preamble", preamble),
                ("change_context", &rendered.change_context),
                ("changed_files", &rendered.changed_files),
                ("diff", &rendered.diff_or_explanation),
            ],
        );
        log_prompt_stats(context, &rendered, body.len(), self.prompt_budget);
        let raw = self.client.complete(&body).await?;
        let mut report = parse_response(&raw);
        // Stamp the reviewer's redaction-safe attribution (a49) so the
        // PR-body composer can render `*Reviewer: <provider>/<model>*`.
        report.attribution = self.attribution.clone();
        // a67: advisory, non-blocking size flag. Appended to the markdown
        // AFTER the verdict/markdown are assembled; the verdict is never
        // touched (size is a maintainability signal, not a correctness
        // defect).
        append_size_advisory(
            &mut report,
            context,
            self.file_lines_threshold,
            self.function_lines_threshold,
        );
        Ok(report)
    }
}

/// Append the advisory `## Size advisory` section to `report.markdown`
/// when this pass pushes a changed file or function past a size threshold
/// (or grows one already over it). The `verdict` is NOT modified — size
/// is a maintainability signal, not a correctness defect. A no-op when no
/// changed file/function is both over-threshold AND net-grown by the pass.
fn append_size_advisory(
    report: &mut ReviewReport,
    ctx: &ReviewContext,
    file_threshold: u64,
    function_threshold: u64,
) {
    if let Some(section) = size_advisory_section(ctx, file_threshold, function_threshold) {
        if report.markdown.trim().is_empty() {
            report.markdown = section;
        } else {
            report.markdown.push_str("\n\n");
            report.markdown.push_str(&section);
        }
    }
}

/// Net additions/deletions for one file in the unified diff, plus its
/// hunks (used to attribute growth to individual functions).
#[derive(Debug, Default, Clone)]
struct FileDiffStats {
    additions: u64,
    deletions: u64,
    hunks: Vec<DiffHunk>,
}

impl FileDiffStats {
    /// Net lines the pass added to the file (`additions − deletions`).
    fn net(&self) -> i64 {
        self.additions as i64 - self.deletions as i64
    }
}

/// One unified-diff hunk's new-file footprint AND its add/delete counts.
#[derive(Debug, Clone)]
struct DiffHunk {
    /// First new-file line number the hunk covers (1-based).
    new_start: usize,
    /// Last new-file line number the hunk covers (1-based, inclusive).
    /// For a pure-deletion hunk this is `new_start − 1` (no new lines).
    new_end: usize,
    additions: u64,
    deletions: u64,
}

/// Compute the advisory `## Size advisory` markdown section for a review,
/// or `None` when nothing crosses a threshold with net growth. For each
/// changed file the reviewer determines — from the file's full contents
/// AND the unified diff — whether the file (or a function within it)
/// exceeds the file/function threshold AND whether this pass added net
/// lines to it; only files/functions that are BOTH over-threshold AND
/// net-grown are reported. Pure (no I/O) for testability.
fn size_advisory_section(
    ctx: &ReviewContext,
    file_threshold: u64,
    function_threshold: u64,
) -> Option<String> {
    let per_file = parse_unified_diff(&ctx.diff);
    let mut items: Vec<String> = Vec::new();
    for file in &ctx.changed_files {
        let ext = file_extension(&file.path);
        let stats = per_file.get(&file.path);
        let total = file.contents.lines().count() as u64;
        // Whole-file advisory.
        let file_net = stats.map(|s| s.net()).unwrap_or(0);
        if total > file_threshold && file_net > 0 {
            match crate::audits::brightline::production_test_line_split(&file.contents, &ext) {
                Some((prod, test)) => items.push(format!(
                    "- `{}` is now {total} lines (production {prod} / test {test}); this pass added net lines.",
                    file.path
                )),
                None => items.push(format!(
                    "- `{}` is now {total} lines; this pass added net lines.",
                    file.path
                )),
            }
        }
        // Function-level advisories.
        let hunks: &[DiffHunk] = stats.map(|s| s.hunks.as_slice()).unwrap_or(&[]);
        for span in crate::audits::brightline::function_line_spans(&file.contents, &ext) {
            let n = span.line_count();
            if n <= function_threshold {
                continue;
            }
            if function_net_lines(hunks, span.start_line, span.end_line) > 0 {
                items.push(format!(
                    "- function `{}` in `{}` is now {n} lines; this pass added net lines.",
                    span.name, file.path
                ));
            }
        }
    }
    if items.is_empty() {
        return None;
    }
    let mut out = String::from("## Size advisory\n\n");
    out.push_str(&items.join("\n"));
    Some(out)
}

/// Net lines (`additions − deletions`) the pass contributed to a function
/// spanning new-file lines `[fstart, fend]`, attributed by hunk overlap:
/// every hunk whose new-file footprint intersects the span contributes
/// its add/delete counts.
fn function_net_lines(hunks: &[DiffHunk], fstart: usize, fend: usize) -> i64 {
    let mut adds: i64 = 0;
    let mut dels: i64 = 0;
    for h in hunks {
        // A pure-deletion hunk has new_end == new_start - 1; clamp so the
        // overlap test treats it as the single insertion point new_start.
        let hend = h.new_end.max(h.new_start);
        if fstart <= hend && h.new_start <= fend {
            adds += h.additions as i64;
            dels += h.deletions as i64;
        }
    }
    adds - dels
}

/// Parse a unified diff into per-file add/delete totals AND hunk
/// footprints, keyed by the new-file path (the `+++ b/<path>` line with
/// its `a/`/`b/` prefix stripped). Robust to git's extended headers
/// (`diff --git`, `index`, mode/rename lines) AND to hunk headers that
/// omit the optional `,count`.
fn parse_unified_diff(diff: &str) -> std::collections::HashMap<String, FileDiffStats> {
    use std::collections::HashMap;
    static HUNK_RE: OnceLock<Regex> = OnceLock::new();
    let hunk_re = HUNK_RE
        .get_or_init(|| Regex::new(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@").unwrap());
    let mut map: HashMap<String, FileDiffStats> = HashMap::new();
    let mut current: Option<String> = None;
    let mut cur_new: usize = 0;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            current = normalize_diff_path(rest);
            if let Some(p) = &current {
                map.entry(p.clone()).or_default();
            }
            continue;
        }
        if line.starts_with("--- ")
            || line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("rename ")
            || line.starts_with("similarity ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("Binary ")
        {
            continue;
        }
        if let Some(caps) = hunk_re.captures(line) {
            cur_new = caps[1].parse().unwrap_or(1);
            if let Some(cur) = &current {
                let fd = map.entry(cur.clone()).or_default();
                fd.hunks.push(DiffHunk {
                    new_start: cur_new,
                    new_end: cur_new.saturating_sub(1),
                    additions: 0,
                    deletions: 0,
                });
            }
            continue;
        }
        let cur = match &current {
            Some(c) => c,
            None => continue,
        };
        let fd = match map.get_mut(cur) {
            Some(fd) => fd,
            None => continue,
        };
        match line.as_bytes().first().copied() {
            Some(b'+') => {
                fd.additions += 1;
                if let Some(h) = fd.hunks.last_mut() {
                    h.additions += 1;
                    h.new_end = cur_new;
                }
                cur_new += 1;
            }
            Some(b'-') => {
                fd.deletions += 1;
                if let Some(h) = fd.hunks.last_mut() {
                    h.deletions += 1;
                }
            }
            Some(b'\\') => { /* "\ No newline at end of file" — ignore */ }
            _ => {
                // Context line (leading space) or a blank context line.
                if let Some(h) = fd.hunks.last_mut() {
                    h.new_end = cur_new;
                }
                cur_new += 1;
            }
        }
    }
    map
}

/// Strip a unified-diff path's `a/`/`b/` prefix AND any trailing tab
/// metadata, yielding the workspace-relative path. `/dev/null` (an
/// added/deleted side) yields `None`.
fn normalize_diff_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.split('\t').next().unwrap_or(raw);
    if raw == "/dev/null" {
        return None;
    }
    let stripped = raw
        .strip_prefix("b/")
        .or_else(|| raw.strip_prefix("a/"))
        .unwrap_or(raw);
    Some(stripped.to_string())
}

/// File extension (lowercased) for a workspace-relative path, or empty
/// when there is none.
fn file_extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
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
    /// Redaction-safe `<provider>/<model>` attribution (a49), carried from
    /// the underlying [`ReviewReport`]. The rerun composer renders it as
    /// `*Reviewer: <provider>/<model>*` on the `## Code Review (rerun N of
    /// M)` comment. `None` when the reviewer carried no configured model.
    pub attribution: Option<String>,
}

/// Reusable reviewer entry point (a33 task 5). The polling-loop AND the
/// operator-trigger dispatcher both invoke this function; the caller
/// decides what to do with the returned `ReviewResult` (write into PR
/// body OR post as fresh PR comment).
///
/// The function:
/// - Builds a [`CodeReviewer`] from `cfg`.
/// - Performs the per-mode dispatch identically to the polling-loop's
///   initial-review path: one LLM call per change in `per_change` mode
///   (populating `ReviewResult.per_change_sections`), one call per PR in
///   `bundled` mode (leaving `per_change_sections` empty).
/// - Wraps the resulting report in a [`ReviewResult`].
///
/// Both callers therefore observe the configured `reviewer.mode`
/// identically; the function never routes through a bundled-only path
/// that ignores the mode (a53).
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
///
/// Dispatch honors `reviewer.mode()` (a53): in `Bundled` mode the single
/// `ReviewContext` is reviewed in one call (`per_change_sections` empty);
/// in `PerChange` mode the context is split into one per-change context
/// per `archived_changes` entry, each reviewed independently, and the
/// results are synthesized into a report carrying one
/// `per_change_sections` entry per change. The function decides nothing
/// about output disposition — the caller renders the returned
/// `ReviewResult`.
pub async fn review_pr_at_state_with(
    reviewer: &CodeReviewer,
    ctx: &ReviewContext,
) -> Result<ReviewResult> {
    let report = match reviewer.mode() {
        crate::config::ReviewerMode::Bundled => reviewer.review(ctx).await?,
        crate::config::ReviewerMode::PerChange => {
            let contexts = split_per_change_contexts(ctx);
            // a015: an empty split (no archived-change briefs resolved for
            // this PR — e.g. a PR opened under one daemon build and
            // re-reviewed under another) must NEVER synthesize a verdict
            // from zero reviews. `review_per_change(&[])` makes zero
            // reviewer invocations and `synthesize_per_change_report(vec![])`
            // would return a defaulted `Pass` — a blank `Approve` the
            // reviewer never performed. Fall back to a single bundled
            // review so the PR's diff and changed files still reach the
            // reviewer and the verdict reflects an actual invocation.
            if contexts.is_empty() {
                reviewer.review(ctx).await?
            } else {
                let per_change = reviewer.review_per_change(&contexts).await?;
                synthesize_per_change_report(per_change)
            }
        }
    };
    Ok(ReviewResult {
        verdict: Verdict::from(report.verdict),
        per_concern: report.concerns.iter().map(ConcernEntry::from).collect(),
        raw_output: report.markdown.clone(),
        markdown: report.markdown.clone(),
        per_change_sections: report.per_change_sections.clone(),
        attribution: report.attribution.clone(),
        concerns: report.concerns,
    })
}

// =====================================================================
// Agentic reviewer transport (a58)
// =====================================================================

/// The MCP role AND submission routing key the agentic reviewer uses. The
/// per-execution MCP child advertises `submit_review` ONLY when
/// `ORCH_MCP_ROLE` equals this value; the daemon-side schema validator is
/// registered under the same key.
pub const REVIEWER_ROLE: &str = "reviewer";

/// Read-only CLI tool permissions for the agentic reviewer sandbox. NO
/// `Bash`, NO `Write`, NO `Edit` — the reviewer reads files on demand AND
/// returns its verdict through the `submit_review` MCP tool.
pub const AGENTIC_REVIEW_ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep"];

/// Wall-clock cap for one agentic reviewer session. The oneshot path has
/// no analogous timeout (the HTTP client owns it); this bounds the wrapped
/// CLI subprocess the way the audits bound theirs.
const AGENTIC_REVIEW_TIMEOUT: Duration = Duration::from_secs(900);

/// The full `--allowedTools` list the agentic reviewer sandbox grants:
/// the read-only file tools PLUS the qualified `submit_review` MCP tool.
/// Notably absent: `Bash`, `Write`, `Edit`. Exposed so tests can assert
/// the advertised surface (task 4.2).
pub fn agentic_review_allowed_tools() -> Vec<String> {
    let mut tools: Vec<String> = AGENTIC_REVIEW_ALLOWED_TOOLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(t) = crate::mcp_askuser_server::submission_tool_name_for_role(REVIEWER_ROLE) {
        tools.push(crate::mcp_askuser_server::qualified_tool_name(t));
    }
    tools
}

/// One reviewer submission concern, as it arrives in the `submit_review`
/// payload. The daemon-side validator [`payload_to_review_result`]
/// deserializes the payload into this shape; a missing required field is a
/// deserialize error surfaced to the agent as a correctable tool error.
#[derive(Debug, Clone, Deserialize)]
struct RawReviewConcern {
    title: String,
    detail: String,
    anchor: String,
    should_request_revision: bool,
    #[serde(default)]
    actionable_request: Option<String>,
    /// The reviewer's own security signal (a004): `true` when this finding
    /// is a credential/secret/key exposure or an injection vulnerability.
    /// Drives the verdict-escalation safety net the same way the oneshot
    /// `revision-requests` block's `security_critical` does. Defaults to
    /// `false` when the reviewer omits it.
    #[serde(default)]
    security_critical: bool,
}

/// The `submit_review` payload shape.
#[derive(Debug, Clone, Deserialize)]
struct RawReviewSubmission {
    verdict: String,
    summary: String,
    #[serde(default)]
    concerns: Vec<RawReviewConcern>,
}

/// Validate AND map a consumed `submit_review` payload into a
/// [`ReviewResult`] (a58). This is BOTH the daemon-side schema validator
/// (registered via [`register_reviewer_submission_schema`] with its `Ok`
/// value discarded) AND the consume-time mapper — so a payload that
/// records successfully is exactly one that maps, and the two can never
/// drift (mirrors the advisory audits' `payload_to_findings`).
///
/// Returns `Err(reason)` (a correction-suitable string) when the verdict
/// is outside `{Approve, Block}`, when a concern sets
/// `should_request_revision: true` without a non-empty `actionable_request`,
/// OR when the payload does not match the expected shape. `record_submission`
/// surfaces the reason to the agent as a correctable tool error.
///
/// On success the `raw_output` AND `markdown` are the rendered summary +
/// concerns markdown used for the PR-body `## Code Review` block;
/// `attribution` is left `None` for the caller to stamp from the reviewer's
/// configured model.
pub(crate) fn payload_to_review_result(
    payload: &Value,
) -> std::result::Result<ReviewResult, String> {
    let sub: RawReviewSubmission = serde_json::from_value(payload.clone()).map_err(|e| {
        format!("submit_review: payload does not match the expected shape: {e}")
    })?;
    let verdict = match sub.verdict.as_str() {
        "Approve" => Verdict::Approve,
        "Block" => Verdict::Block,
        other => {
            return Err(format!(
                "submit_review: verdict must be one of Approve | Block; got `{other}`"
            ));
        }
    };
    let mut concerns: Vec<ReviewConcern> = Vec::with_capacity(sub.concerns.len());
    for (idx, c) in sub.concerns.iter().enumerate() {
        if c.should_request_revision {
            let has_request = c
                .actionable_request
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !has_request {
                return Err(format!(
                    "submit_review: concerns[{idx}] (`{title}`) sets should_request_revision: true \
                     but has an empty actionable_request; provide the concrete revision instruction",
                    title = c.title
                ));
            }
        }
        concerns.push(ReviewConcern {
            summary: c.title.clone(),
            actionable_request: c.actionable_request.clone(),
            should_request_revision: c.should_request_revision,
            change_slug: None,
            security_critical: c.security_critical,
        });
    }
    // a004 safety net (agentic path): a payload flagging a credential/secret/
    // key exposure or injection via its own `security_critical` finding signal
    // but returning a non-`Block` verdict is escalated to `Block` before the
    // result reaches the PR-draft / auto-revise handling. Keyed on the
    // structured signal, never on the prose of the finding.
    let verdict = if verdict != Verdict::Block && concerns_flag_security_critical(&concerns) {
        Verdict::Block
    } else {
        verdict
    };
    let raw_output = render_review_submission_markdown(&sub.summary, &sub.concerns);
    let per_concern = concerns.iter().map(ConcernEntry::from).collect();
    Ok(ReviewResult {
        verdict,
        per_concern,
        raw_output: raw_output.clone(),
        markdown: raw_output,
        per_change_sections: Vec::new(),
        concerns,
        attribution: None,
    })
}

/// Render a `submit_review` payload's summary + concerns into the markdown
/// body the PR-body `## Code Review` block carries. Wording is not
/// asserted by tests (per the project-documentation requirement "Tests
/// assert behavior or derivation, never message wording").
fn render_review_submission_markdown(summary: &str, concerns: &[RawReviewConcern]) -> String {
    let mut out = String::new();
    if !summary.trim().is_empty() {
        out.push_str(summary.trim_end());
    }
    if !concerns.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("## Concerns");
        for c in concerns {
            out.push_str(&format!("\n\n- **{}**", c.title));
            if !c.anchor.trim().is_empty() {
                out.push_str(&format!(" ({})", c.anchor.trim()));
            }
            if !c.detail.trim().is_empty() {
                out.push_str(&format!("\n  {}", c.detail.trim()));
            }
            if c.should_request_revision
                && let Some(req) = c.actionable_request.as_deref()
                && !req.trim().is_empty()
            {
                out.push_str(&format!("\n  - Requested revision: {}", req.trim()));
            }
        }
    }
    if out.is_empty() {
        out.push_str("(no concerns)");
    }
    out
}

/// Render the agentic reviewer's prompt from a [`ReviewContext`] (a58):
/// the change briefs, the changed-file PATH list (NOT full contents), AND
/// the unified diff. The agent reads whatever files it needs on demand via
/// `Read`, so `reviewer.prompt_budget_chars` is NOT consulted here AND no
/// `## Skipped (budget exhausted)` truncation occurs.
pub fn render_agentic_review_prompt(ctx: &ReviewContext, preamble: &str) -> String {
    let mut out = String::new();
    if !preamble.trim().is_empty() {
        out.push_str(preamble.trim_end());
        out.push_str("\n\n");
    }
    out.push_str(
        "You are reviewing a code change for quality (security, error handling, naming, \
         style, language idioms, obvious bugs). Do NOT assess whether the diff implements \
         any spec — that is a separate concern.\n\n",
    );

    out.push_str("# Change briefs\n\n");
    if ctx.archived_changes.is_empty() {
        out.push_str("(no archived-change briefs for this pass)\n\n");
    } else {
        for brief in &ctx.archived_changes {
            out.push_str(&format!("## Change: {}\n\n", brief.name));
            out.push_str(brief.proposal.trim_end());
            if let Some(design) = brief.design.as_deref() {
                out.push_str("\n\n");
                out.push_str(design.trim_end());
            }
            out.push_str("\n\n");
            out.push_str(brief.tasks.trim_end());
            out.push_str("\n\n");
        }
    }

    out.push_str("# Changed files\n\n");
    out.push_str(
        "These files were modified by this pass. Their full contents are NOT inlined — use \
         the `Read`, `Glob`, AND `Grep` tools to read whatever you need on demand.\n\n",
    );
    if ctx.changed_files.is_empty() {
        out.push_str("(no changed files reported)\n");
    } else {
        for f in &ctx.changed_files {
            out.push_str(&format!("- {}\n", f.path));
        }
    }
    out.push('\n');

    out.push_str("# Unified diff\n\n");
    if ctx.diff.trim().is_empty() {
        out.push_str("(no diff produced this pass)\n\n");
    } else {
        out.push_str("```diff\n");
        out.push_str(&ctx.diff);
        if !ctx.diff.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    out.push_str(
        "Security-critical findings are always Block. Credential or secret leakage (a key, \
         token, or secret written where it could be committed or otherwise exposed), hardcoded \
         secrets, AND injection vulnerabilities (SQL, command, path) are stop-the-line: return \
         `Block`, never a soft verdict, AND set `security_critical: true` on that concern. The \
         daemon escalates the verdict to `Block` from the `security_critical` signal even if you \
         returned `Approve`.\n\n",
    );

    out.push_str(
        "When your analysis is complete, call the `submit_review` MCP tool exactly once with \
         your verdict (Approve | Block), a summary, AND any concerns. Each concern that should \
         drive a revision MUST set `should_request_revision: true` with a non-empty \
         `actionable_request`. Mark any credential/secret/key-exposure or injection finding with \
         `security_critical: true`. Do NOT print the verdict to stdout — the daemon reads it ONLY \
         from `submit_review`.\n",
    );
    out
}

/// Outcome of an agentic review pass (a58). `Reviewed` carries the
/// schema-validated [`ReviewResult`]; `Discarded` means a session ended
/// with no valid `submit_review` submission, so the caller writes NO
/// verdict (it does NOT default to `Approve`) AND posts the reviewer-
/// failure operator alert.
#[derive(Debug, Clone)]
pub enum AgenticReviewOutcome {
    Reviewed(ReviewResult),
    Discarded { reason: String },
}

/// Abstracts "run ONE agentic reviewer session AND drain its submission"
/// so the orchestration ([`run_agentic_review_with_runner`]) is unit-
/// testable without spawning a CLI. Production is
/// [`CliReviewSessionRunner`]; tests inject canned submissions.
#[async_trait]
trait ReviewSessionRunner: Send + Sync {
    /// Run one session against `prompt` AND return the consumed
    /// `submit_review` payload, or `None` when the agent recorded no valid
    /// submission. `slug` labels the session (empty for bundled).
    async fn run_session(&self, slug: &str, prompt: &str) -> Result<Option<Value>>;
}

/// Production session runner: writes the per-execution MCP config
/// (`ORCH_MCP_ROLE = reviewer`), runs the wrapped CLI through
/// [`crate::agentic_run::agentic_run`] in a read-only capture sandbox, AND
/// drains the stored submission via the control socket. Mirrors the
/// advisory audits' `run_audit_cli_with_submit` + `try_consume_submission`.
struct CliReviewSessionRunner<'a> {
    workspace: &'a Path,
    strategy: &'a dyn crate::agentic_run::CliStrategy,
    /// The reviewer's resolved CLI, so the OS sandbox admits THIS CLI's own
    /// credential store (and binds its binary) instead of masking it as a
    /// foreign CLI's. Must match `strategy`'s CLI.
    cli: crate::config::CliKind,
    settings_dir: Option<&'a Path>,
    timeout: Duration,
}

#[async_trait]
impl ReviewSessionRunner for CliReviewSessionRunner<'_> {
    async fn run_session(&self, _slug: &str, prompt: &str) -> Result<Option<Value>> {
        // Write the per-execution MCP config advertising `submit_review`.
        // `change == REVIEWER_ROLE` keys the submission-store entry; this
        // runner consumes the same key after exit.
        crate::executor::claude_cli::ClaudeCliExecutor::write_mcp_config(
            self.workspace,
            REVIEWER_ROLE,
            Some(REVIEWER_ROLE),
        )
        .context("writing reviewer MCP config")?;

        // a70: a single-shot role — prune the session it creates on completion.
        let result = crate::agentic_run::agentic_run_with_session(
            crate::agentic_run::AgenticRunOpts {
            workspace: self.workspace,
            change: REVIEWER_ROLE,
            strategy: self.strategy,
            prompt,
            sandbox: crate::agentic_run::SandboxConfig {
                allowed_tools: agentic_review_allowed_tools(),
                disallowed_bash_patterns: Vec::new(),
                disallowed_read_paths: Vec::new(),
                deny_writes: true,
            },
            model: None,
            output_mode: crate::agentic_run::OutputMode::Capture,
            timeout: self.timeout,
            paths: None,
            settings_dir: self.settings_dir,
            include_autocoder_tools: true,
            emit_stream_json_in_capture: false,
            resume_session_id: None,
            track_subprocess_marker: false,
            etxtbsy_retry_spawn: true,
            // a006: the agentic reviewer is a read-only role — read-only
            // workspace. The OS sandbox MUST match the reviewer's actual CLI so
            // the role's OWN credential store is admitted (not masked as a
            // foreign CLI's) AND its binary is bound. Previously hardcoded to
            // `Claude`, which masked an `opencode`/`agy` reviewer's own store →
            // the CLI could not authenticate → "no valid submit_review submission".
            os_sandbox: crate::sandbox::current_run_sandbox(self.cli, false),
            },
            true,
            None,
        )
        .await;

        // Always remove the config we wrote, regardless of run outcome.
        crate::executor::claude_cli::ClaudeCliExecutor::delete_mcp_config(self.workspace);

        let outcome = result.context("spawning agentic reviewer subprocess")?;
        if outcome.timed_out {
            return Err(anyhow!(
                "agentic reviewer session timed out after {}s",
                self.timeout.as_secs()
            ));
        }
        Ok(crate::audits::try_consume_submission(self.workspace, REVIEWER_ROLE).await)
    }
}

/// Resolve the agentic reviewer's CLI strategy from its provider via the
/// a55/a56 `provider → CLI` rule. Anthropic → the `claude` strategy;
/// non-Anthropic providers → the `opencode` strategy (a60). No session is
/// spawned at resolution time. (A future provider whose CLI has no
/// registered strategy would still return a clear error here.)
fn resolve_reviewer_strategy(
    reviewer: &CodeReviewer,
) -> Result<Box<dyn crate::agentic_run::CliStrategy>> {
    crate::agentic_run::strategy_for_provider(
        reviewer.provider,
        reviewer.command.clone(),
        Vec::new(),
    )
}

/// Whether `cli` resolves to an executable on the daemon host. An absolute
/// or path-qualified command (`/usr/local/bin/claude`, `./claude`) is tested
/// directly; a bare name (`claude`) is searched across the entries in `$PATH`.
/// No subprocess is spawned — the binary is located, not executed — so the
/// startup probe is fast AND has no side effects. Used by
/// [`resolve_startup_reviewer_kind`] for the a64 agentic-CLI fallback.
fn reviewer_binary_on_path(cli: &str) -> bool {
    let candidate = Path::new(cli);
    if candidate.is_absolute() || cli.contains('/') {
        return candidate.is_file();
    }
    match std::env::var_os("PATH") {
        Some(path_var) => std::env::split_paths(&path_var).any(|dir| dir.join(cli).is_file()),
        None => false,
    }
}

/// Pure decision behind the a64 startup CLI-availability fallback. Given the
/// configured reviewer transport, the resolved CLI name, AND whether that CLI
/// is available on the host, return the effective startup transport plus an
/// optional loud WARN message:
///
/// - `Oneshot` configured → `(Oneshot, None)`: the operator opted out of
///   agentic deliberately, so no probe AND no warning.
/// - `Agentic` configured AND CLI available → `(Agentic, None)`: agentic runs.
/// - `Agentic` configured AND CLI unavailable → `(Oneshot, Some(warn))`: the
///   reviewer degrades to the HTTP one-shot path for the boot (review is NOT
///   disabled) AND the caller logs `warn`, which names the missing CLI AND the
///   remedy. The same disposition applies whether `agentic` was the default or
///   set explicitly.
///
/// Separated from the host probe ([`resolve_startup_reviewer_kind`]) so tests
/// assert the decision without depending on what is installed on the host —
/// mirroring [`crate::config::clamp_max_code_reviews_per_pr`]'s observable
/// `Option<String>` warning return.
pub fn startup_reviewer_kind_decision(
    configured: ReviewerKind,
    cli: &str,
    cli_available: bool,
) -> (ReviewerKind, Option<String>) {
    match configured {
        ReviewerKind::Oneshot => (ReviewerKind::Oneshot, None),
        ReviewerKind::Agentic if cli_available => (ReviewerKind::Agentic, None),
        ReviewerKind::Agentic => {
            let warn = format!(
                "reviewer.kind is `agentic` but the resolved reviewer CLI `{cli}` is unavailable \
                 on the daemon host (no registered strategy, OR the binary is not on PATH); \
                 falling back to the `oneshot` HTTP review path for this boot — review is NOT \
                 disabled. Install `{cli}` to enable the agentic reviewer, OR set \
                 `reviewer.kind: oneshot` to silence this warning. A daemon restart or \
                 `autocoder reload` re-evaluates availability."
            );
            (ReviewerKind::Oneshot, Some(warn))
        }
    }
}

/// Resolve the reviewer's effective transport at startup AND on
/// `autocoder reload`, applying the a64 agentic-CLI-availability fallback.
///
/// When the configured kind is `agentic` (defaulted OR explicit) this probes
/// the host: the CLI is "available" only when its strategy is registered
/// (resolved via the a55/a56 `provider → CLI` rule) AND its binary is found on
/// PATH. An unavailable CLI degrades to `oneshot` for the boot, returning the
/// loud WARN for the caller to log exactly once. When the configured kind is
/// `oneshot` no probe runs. The daemon wires this in at the two reviewer
/// construction sites (startup in `cli::run`, reload in `control_socket`), so
/// availability is evaluated once per boot/reload — never per polling
/// iteration. This supersedes a58's "a reviewer CLI with no registered
/// strategy returns a clear error, no session" behavior for the reviewer role:
/// instead of erroring, the reviewer degrades to HTTP review.
pub fn resolve_startup_reviewer_kind(reviewer: &CodeReviewer) -> (ReviewerKind, Option<String>) {
    if reviewer.kind() != ReviewerKind::Agentic {
        return (reviewer.kind(), None);
    }
    // "Available" requires BOTH a registered strategy AND a binary on PATH.
    let cli_available =
        resolve_reviewer_strategy(reviewer).is_ok() && reviewer_binary_on_path(&reviewer.command);
    startup_reviewer_kind_decision(ReviewerKind::Agentic, &reviewer.command, cli_available)
}

/// Apply the a64 startup CLI-availability fallback to a freshly built
/// reviewer. When the effective kind is `agentic` but the resolved reviewer
/// CLI is unavailable, log ONE loud WARN (naming the missing CLI AND the
/// remedy) AND return the reviewer with its kind overridden to `oneshot` for
/// the boot — review continues over HTTP, never disabled. Otherwise the
/// reviewer is returned unchanged. Both reviewer construction sites (startup
/// in `cli::run`, reload in `control_socket::build_reviewer`) call this, so
/// availability is evaluated once per boot/reload — the live polling-loop
/// reviewer slot already carries the resolved kind, so no per-iteration probe
/// (and no re-warn) occurs.
pub fn apply_startup_cli_fallback(reviewer: CodeReviewer) -> CodeReviewer {
    let (effective, warn) = resolve_startup_reviewer_kind(&reviewer);
    if let Some(msg) = warn {
        tracing::warn!("{msg}");
    }
    reviewer.with_kind(effective)
}

/// Run the agentic reviewer against `ctx` (a58). Production entry point for
/// both the polling-loop initial review AND the operator-triggered rerun
/// composer. Resolves the CLI strategy (`claude` for Anthropic, `opencode`
/// for non-Anthropic providers — a60), then dispatches one session per
/// `reviewer.mode()`. This is reached only when the reviewer's effective
/// kind is `agentic`; under a64 the startup CLI-availability check
/// ([`resolve_startup_reviewer_kind`]) has already degraded an
/// unavailable-CLI reviewer to `oneshot` for the boot, so the strategy
/// resolution here succeeds in the common case (a later availability change
/// still surfaces as `Err`, handled by the caller).
pub async fn run_agentic_review(
    reviewer: &CodeReviewer,
    ctx: &ReviewContext,
    workspace: &Path,
) -> Result<AgenticReviewOutcome> {
    let strategy = resolve_reviewer_strategy(reviewer)?;
    let runner = CliReviewSessionRunner {
        workspace,
        strategy: strategy.as_ref(),
        // Match the sandbox to the reviewer's actual CLI (provider → CLI), so an
        // opencode/agy reviewer's own store is admitted, not masked as foreign.
        cli: crate::config::default_cli_for(reviewer.provider),
        settings_dir: None,
        timeout: AGENTIC_REVIEW_TIMEOUT,
    };
    run_agentic_review_with_runner(reviewer, ctx, &runner).await
}

/// Mode-aware orchestration shared by production AND tests. Honors
/// `reviewer.mode()` identically to the one-shot path: `Bundled` → one
/// session for the whole `ReviewContext`; `PerChange` → one session per
/// archived change (split via [`split_per_change_contexts`]), synthesized
/// into a single [`ReviewResult`] with one `per_change_sections` entry per
/// change. A session that records no valid submission discards the WHOLE
/// review (returns `Discarded`) — it never defaults to `Approve`.
async fn run_agentic_review_with_runner(
    reviewer: &CodeReviewer,
    ctx: &ReviewContext,
    runner: &dyn ReviewSessionRunner,
) -> Result<AgenticReviewOutcome> {
    // Build the per-session work list. Bundled is always exactly one
    // session even when the pass has zero archived changes.
    //
    // a015: an empty per-change split (no archived-change briefs resolved
    // for this PR — e.g. a PR opened under one daemon build and re-reviewed
    // under another) must NEVER reach `synthesize_agentic_per_change` with
    // zero reviews: that initializer defaults to `Approve`, so the loop
    // running zero sessions would produce a blank `Approve` the reviewer
    // never performed — the exact silent-approval bug the one-shot
    // `review_pr_at_state_with` path fixes. Mirror that fix here: fall back
    // to a single BUNDLED session so the PR's diff and changed files still
    // reach the reviewer and the verdict comes from an actual invocation.
    // The `bundled` flag then also routes synthesis below through the
    // bundled arm (no empty per-change synthesis).
    let mut bundled = matches!(reviewer.mode(), crate::config::ReviewerMode::Bundled);
    let sessions: Vec<(Option<String>, ReviewContext, String)> = if bundled {
        vec![(None, ctx.clone(), String::new())]
    } else {
        let per_change = split_per_change_contexts(ctx);
        if per_change.is_empty() {
            bundled = true;
            vec![(None, ctx.clone(), String::new())]
        } else {
            per_change
                .into_iter()
                .map(|p| (Some(p.change_slug), p.context, p.cross_change_preamble))
                .collect()
        }
    };

    let mut reviews: Vec<(Option<String>, ReviewResult)> = Vec::with_capacity(sessions.len());
    for (slug, session_ctx, preamble) in &sessions {
        let prompt = render_agentic_review_prompt(session_ctx, preamble);
        let consumed = runner
            .run_session(slug.as_deref().unwrap_or(""), &prompt)
            .await?;
        match consumed {
            None => {
                let reason = match slug {
                    Some(s) => format!(
                        "agentic reviewer session for `{s}` recorded no valid submit_review submission"
                    ),
                    None => "agentic reviewer session recorded no valid submit_review submission"
                        .to_string(),
                };
                return Ok(AgenticReviewOutcome::Discarded { reason });
            }
            Some(payload) => {
                // The payload already passed `record_submission`'s validator,
                // so this re-map cannot drift; a failure here is an internal
                // invariant violation.
                let result = payload_to_review_result(&payload).map_err(|e| {
                    anyhow!("recorded submit_review payload failed re-validation: {e}")
                })?;
                reviews.push((slug.clone(), result));
            }
        }
    }

    // a015: synthesize through the SAME `bundled` flag the session list was
    // built with, so an empty-split fallback (bundled = true above) takes
    // the single-review bundled arm instead of synthesizing per-change from
    // an effectively empty set.
    let outcome = if bundled {
        let mut result = reviews
            .pop()
            .map(|(_, r)| r)
            .expect("bundled mode always runs exactly one session");
        result.attribution = reviewer.attribution.clone();
        result
    } else {
        synthesize_agentic_per_change(reviews, reviewer.attribution.clone())
    };
    Ok(AgenticReviewOutcome::Reviewed(outcome))
}

/// Aggregate per-change agentic [`ReviewResult`]s into one result whose
/// `per_change_sections` drives the composer to emit one
/// `## Code Review: <slug>` section per change — the same disposition the
/// one-shot per-change path produces. The aggregate verdict is `Block` when
/// ANY change blocked, else `Approve`; the flat `concerns` vec is the union
/// of each change's concerns tagged with their `change_slug`.
fn synthesize_agentic_per_change(
    reviews: Vec<(Option<String>, ReviewResult)>,
    attribution: Option<String>,
) -> ReviewResult {
    // a015: a synthesis from zero per-change reviews must NEVER be the
    // source of a defaulted `Approve`. The dispatch in
    // `run_agentic_review_with_runner` now falls back to a bundled session
    // before reaching here with an empty vec, so this guard is defensive —
    // it makes the "never a defaulted Approve" invariant explicit. `Block`
    // is the only fail-safe verdict: an empty synthesis can never become a
    // silent approval. (Mirrors the one-shot `synthesize_per_change_report`
    // guard.)
    if reviews.is_empty() {
        return ReviewResult {
            verdict: Verdict::Block,
            per_concern: Vec::new(),
            raw_output: String::new(),
            markdown: "No per-change reviews were performed; refusing to \
                synthesize a verdict from zero reviews."
                .to_string(),
            per_change_sections: Vec::new(),
            concerns: Vec::new(),
            attribution,
        };
    }
    let mut verdict = Verdict::Approve;
    let mut concerns: Vec<ReviewConcern> = Vec::new();
    let mut sections: Vec<PerChangeSection> = Vec::with_capacity(reviews.len());
    for (slug, result) in reviews {
        let slug = slug.unwrap_or_default();
        if matches!(result.verdict, Verdict::Block) {
            verdict = Verdict::Block;
        }
        for concern in &result.concerns {
            let mut tagged = concern.clone();
            tagged.change_slug = Some(slug.clone());
            concerns.push(tagged);
        }
        let section_body = format!(
            "VERDICT: {}\n\n{}",
            result.verdict.label(),
            result.raw_output
        );
        sections.push(PerChangeSection {
            change_slug: slug,
            markdown: section_body,
        });
    }
    let per_concern = concerns.iter().map(ConcernEntry::from).collect();
    ReviewResult {
        verdict,
        per_concern,
        raw_output: String::new(),
        markdown: String::new(),
        per_change_sections: sections,
        concerns,
        attribution,
    }
}

/// Register the reviewer's `submit_review` payload schema (a58) with the
/// daemon's submission store, under [`REVIEWER_ROLE`]. The validator IS
/// [`payload_to_review_result`] with its `Ok` value discarded, so a
/// payload that records successfully is exactly one that maps. Called once
/// at daemon startup alongside the advisory audits' schema registration.
pub fn register_reviewer_submission_schema(store: &crate::submission_store::SubmissionStore) {
    use std::sync::Arc;
    store.register_schema(
        REVIEWER_ROLE,
        Arc::new(|p: &Value| payload_to_review_result(p).map(|_| ())),
    );
}

impl ReviewResult {
    /// Convert an agentic [`ReviewResult`] into the [`ReviewReport`] the
    /// polling-loop's post-review pipeline consumes (draft decision,
    /// reviewer-revision partitioning, PR-body composition). The two-state
    /// agentic verdict maps `Approve → Pass` AND `Block → Block`.
    pub fn into_review_report(self) -> ReviewReport {
        let verdict = match self.verdict {
            Verdict::Approve => ReviewVerdict::Pass,
            Verdict::Block => ReviewVerdict::Block,
        };
        ReviewReport {
            verdict,
            markdown: self.markdown,
            concerns: self.concerns,
            per_change_sections: self.per_change_sections,
            attribution: self.attribution,
        }
    }
}

/// Split a bundled [`ReviewContext`] into one [`PerChangeContext`] per
/// archived change, for the per-change reviewer dispatch on the reusable
/// entry point. Each per-change context carries that change's brief alone
/// plus a cross-change preamble naming the others; the changed-files AND
/// diff are shared across the per-change contexts. The single-
/// `ReviewContext` entry point has no per-change git scoping (unlike the
/// polling-loop path, which scopes each change's diff via commit-subject
/// prefixes), so the preamble is what confines each reviewer call's
/// verdict to its named change.
fn split_per_change_contexts(ctx: &ReviewContext) -> Vec<PerChangeContext> {
    ctx.archived_changes
        .iter()
        .map(|brief| PerChangeContext {
            change_slug: brief.name.clone(),
            context: ReviewContext {
                archived_changes: vec![brief.clone()],
                changed_files: ctx.changed_files.clone(),
                diff: ctx.diff.clone(),
            },
            cross_change_preamble: build_cross_change_preamble(&brief.name, &ctx.archived_changes),
        })
        .collect()
}

/// Aggregate a `Vec<PerChangeReview>` into one [`ReviewReport`] whose
/// `per_change_sections` drives the composer (PR-body or rerun comment)
/// to emit one `## Code Review: <slug>` section per element. The
/// aggregate `verdict` is the worst across sections (`Block` >
/// `Concerns` > `Pass`). The flat `concerns` vec is the union of each
/// per-change report's concerns (tagged with their `change_slug`), used
/// by the auto-revise pipeline.
pub(crate) fn synthesize_per_change_report(per_change: Vec<PerChangeReview>) -> ReviewReport {
    // a015: a synthesis from zero per-change reviews must NEVER be the
    // source of a defaulted `Pass`/`Approve`. The per_change dispatch arm
    // now falls back to a bundled review before reaching here with an
    // empty vec, so this guard is defensive — it makes that invariant
    // explicit. `Block` is the only verdict that does not map to `Approve`
    // on the operator-facing surface, so it is the fail-safe choice: an
    // empty synthesis can never become a silent approval.
    if per_change.is_empty() {
        return ReviewReport {
            verdict: ReviewVerdict::Block,
            markdown: "No per-change reviews were performed; refusing to \
                synthesize a verdict from zero reviews."
                .to_string(),
            concerns: Vec::new(),
            per_change_sections: Vec::new(),
            attribution: None,
        };
    }
    let mut verdict = ReviewVerdict::Pass;
    let mut concerns: Vec<ReviewConcern> = Vec::new();
    let mut sections: Vec<PerChangeSection> = Vec::with_capacity(per_change.len());
    // Every per-change report comes from the same reviewer, so they share
    // one attribution (a49); carry it onto the synthesized report so the
    // composer can attribute each `## Code Review: <slug>` section.
    let attribution = per_change
        .first()
        .and_then(|pcr| pcr.report.attribution.clone());
    for pcr in per_change {
        verdict = worst_verdict(verdict, pcr.report.verdict);
        for concern in &pcr.report.concerns {
            let mut tagged = concern.clone();
            tagged.change_slug = Some(pcr.change_slug.clone());
            concerns.push(tagged);
        }
        let section_body =
            format!("VERDICT: {}\n\n{}", verdict_label(pcr.report.verdict), pcr.report.markdown);
        sections.push(PerChangeSection {
            change_slug: pcr.change_slug,
            markdown: section_body,
        });
    }
    ReviewReport {
        verdict,
        markdown: String::new(),
        concerns,
        per_change_sections: sections,
        attribution,
    }
}

fn verdict_label(v: ReviewVerdict) -> &'static str {
    match v {
        ReviewVerdict::Pass => "Pass",
        ReviewVerdict::Concerns => "Concerns",
        ReviewVerdict::Block => "Block",
    }
}

fn worst_verdict(a: ReviewVerdict, b: ReviewVerdict) -> ReviewVerdict {
    fn rank(v: ReviewVerdict) -> u8 {
        match v {
            ReviewVerdict::Pass => 0,
            ReviewVerdict::Concerns => 1,
            ReviewVerdict::Block => 2,
        }
    }
    if rank(a) >= rank(b) { a } else { b }
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

/// Whether the reviewer's own structured findings flag a security-critical
/// issue — a credential/secret/key exposure or an injection vulnerability —
/// via the per-concern `security_critical` signal (a004). This drives the
/// verdict-escalation safety net: such a finding forces a `Block` even when
/// the reviewer returned a softer verdict. It keys on the structured signal
/// the reviewer emitted, NEVER on the prose of the finding, so a
/// mis-classifying model cannot downgrade a credential leak to advisory and
/// a finding that merely mentions "credential" in passing does not escalate.
fn concerns_flag_security_critical(concerns: &[ReviewConcern]) -> bool {
    concerns.iter().any(|c| c.security_critical)
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

    let mut report = match (first_nonempty, found_idx) {
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
                attribution: None,
            }
        }
        _ => ReviewReport {
            verdict: ReviewVerdict::Concerns,
            markdown: format!(
                "[reviewer response did not include a valid verdict line]\n\n{raw}"
            ),
            concerns,
            per_change_sections: Vec::new(),
            attribution: None,
        },
    };
    // a004 safety net: a review that flagged a credential/secret/key exposure
    // or injection (via the reviewer's own `security_critical` finding signal)
    // but returned a softer verdict is escalated to `Block` here — before the
    // PR-draft / auto-revise handling runs — so a mis-classifying model cannot
    // ship a security-critical finding through as advisory. Non-security
    // findings are untouched.
    if report.verdict != ReviewVerdict::Block
        && concerns_flag_security_critical(&report.concerns)
    {
        report.verdict = ReviewVerdict::Block;
    }
    report
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

    // =================================================================
    // a004: security-critical findings escalate the verdict to Block.
    // The escalation keys ONLY on the reviewer's own structured
    // `security_critical` signal, never on the prose of the finding.
    // =================================================================

    /// 3.1: a credential/secret-leak finding (`security_critical: true`)
    /// returned with a `Concerns` verdict is escalated to `Block`.
    #[test]
    fn security_finding_escalates_concerns_to_block() {
        let raw = r#"VERDICT: Concerns

## Security
- API key persisted to a committable config file.

```revision-requests
- summary: "API key written to committable opencode.json"
  actionable_request: "read the key from an env var instead of persisting it"
  should_request_revision: true
  security_critical: true
```
"#;
        let r = parse_response(raw);
        assert_eq!(
            r.verdict,
            ReviewVerdict::Block,
            "a security_critical finding must force Block even when the reviewer wrote Concerns"
        );
    }

    /// 3.1: the same escalation applies when the reviewer wrote `Pass`.
    #[test]
    fn security_finding_escalates_pass_to_block() {
        let raw = r#"VERDICT: Pass

## Security
- Token leaked into the workspace.

```revision-requests
- summary: "auth token written to a tracked file"
  security_critical: true
```
"#;
        let r = parse_response(raw);
        assert_eq!(r.verdict, ReviewVerdict::Block);
    }

    /// 3.2: an injection finding (also carried by `security_critical`) with
    /// a non-`Block` verdict escalates to `Block`.
    #[test]
    fn injection_finding_escalates_to_block() {
        let raw = r#"VERDICT: Concerns

## Security
- User input concatenated into a shell command.

```revision-requests
- summary: "command injection in run_hook"
  actionable_request: "pass arguments as a vector instead of building a shell string"
  should_request_revision: true
  security_critical: true
```
"#;
        let r = parse_response(raw);
        assert_eq!(r.verdict, ReviewVerdict::Block);
    }

    /// 3.3: a `Concerns` verdict whose findings are all non-security
    /// (`security_critical` omitted → `false`) stays `Concerns` — no
    /// escalation.
    #[test]
    fn non_security_concerns_are_not_escalated() {
        let raw = r#"VERDICT: Concerns

## Naming, style, idioms
- `tmp` is an unclear name.

```revision-requests
- summary: "rename tmp to something descriptive"
  should_request_revision: false
```
"#;
        let r = parse_response(raw);
        assert_eq!(
            r.verdict,
            ReviewVerdict::Concerns,
            "non-security findings must keep their verdict"
        );
        assert!(
            !r.concerns[0].security_critical,
            "omitted security_critical defaults to false"
        );
    }

    /// 3.4: the escalation is driven by the structured `security_critical`
    /// signal, NOT by message wording. A finding whose prose screams
    /// "credential leak" but is NOT flagged stays `Concerns`; an innocuous-
    /// worded finding that IS flagged escalates to `Block`.
    #[test]
    fn escalation_keys_on_signal_not_wording() {
        // Prose mentions a credential leak, but the structured signal is
        // absent (defaults to false) → no escalation.
        let worded_but_unflagged = r#"VERDICT: Concerns

```revision-requests
- summary: "possible credential leak / secret / api key exposure here"
  should_request_revision: false
```
"#;
        let r = parse_response(worded_but_unflagged);
        assert_eq!(
            r.verdict,
            ReviewVerdict::Concerns,
            "wording alone must NOT escalate — only the structured signal does"
        );

        // Innocuous wording, but the structured signal is set → escalates.
        let flagged_but_innocuous = r#"VERDICT: Concerns

```revision-requests
- summary: "tidy up helper foo"
  security_critical: true
```
"#;
        let r = parse_response(flagged_but_innocuous);
        assert_eq!(
            r.verdict,
            ReviewVerdict::Block,
            "the structured signal escalates regardless of innocuous wording"
        );
    }

    /// A `security_critical` finding that ALSO carries a `Block` verdict is
    /// a no-op for the escalation (already Block) — the verdict is unchanged.
    #[test]
    fn security_finding_already_block_is_unchanged() {
        let raw = r#"VERDICT: Block

```revision-requests
- summary: "hardcoded secret"
  security_critical: true
```
"#;
        let r = parse_response(raw);
        assert_eq!(r.verdict, ReviewVerdict::Block);
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

    // ====================================================================
    // a67: advisory size flag (tasks 7.x / 8.7)
    // ====================================================================

    async fn review_with_size_thresholds(
        ctx: &ReviewContext,
        file_t: u64,
        func_t: u64,
    ) -> ReviewReport {
        let (client, _captured) = stub_with_capture("VERDICT: Pass\n\n## Review\nlooks fine\n");
        let reviewer =
            CodeReviewer::new(client, "{{diff}}".to_string()).with_size_thresholds(file_t, func_t);
        reviewer.review(ctx).await.unwrap()
    }

    /// 8.7a — a pass that grows a changed file past the file threshold
    /// yields a size advisory naming the file, AND leaves the verdict
    /// untouched.
    #[tokio::test]
    async fn size_advisory_flags_file_grown_past_threshold() {
        let contents: String = (0..60).map(|i| format!("// line {i}\n")).collect();
        let mut diff = String::from(
            "diff --git a/src/foo.rs b/src/foo.rs\n--- a/src/foo.rs\n+++ b/src/foo.rs\n@@ -1,1 +1,11 @@\n // line 0\n",
        );
        for i in 0..10 {
            diff.push_str(&format!("+// added {i}\n"));
        }
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "src/foo.rs".into(),
                contents,
            }],
            diff,
        };
        let report = review_with_size_thresholds(&ctx, 50, 20).await;
        assert!(
            report.markdown.contains("## Size advisory") && report.markdown.contains("src/foo.rs"),
            "expected a file size advisory: {}",
            report.markdown
        );
        // Size is advisory only — the parsed verdict is unchanged.
        assert_eq!(report.verdict, ReviewVerdict::Pass);
    }

    /// 8.7b — a pass that only shrinks an over-threshold file is NOT
    /// flagged.
    #[tokio::test]
    async fn size_advisory_skips_file_only_shrunk() {
        let contents: String = (0..60).map(|i| format!("// line {i}\n")).collect();
        let diff = String::from(
            "diff --git a/src/bar.rs b/src/bar.rs\n--- a/src/bar.rs\n+++ b/src/bar.rs\n@@ -1,5 +1,1 @@\n // keep\n-// del 0\n-// del 1\n-// del 2\n-// del 3\n",
        );
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "src/bar.rs".into(),
                contents,
            }],
            diff,
        };
        let report = review_with_size_thresholds(&ctx, 50, 20).await;
        assert!(
            !report.markdown.contains("## Size advisory"),
            "a shrinking pass must not be flagged: {}",
            report.markdown
        );
    }

    /// 8.7c — a pass that grows a single function past the function
    /// threshold yields a function-level advisory.
    #[tokio::test]
    async fn size_advisory_flags_function_grown_past_threshold() {
        // 27-line function (1 signature + 25 body + 1 close).
        let mut contents = String::from("pub fn grower() {\n");
        for i in 0..25 {
            contents.push_str(&format!("    let v{i} = {i};\n"));
        }
        contents.push_str("}\n");
        // Diff that adds the 25 body lines (net +25 within the function).
        let mut diff = String::from(
            "diff --git a/src/grow.rs b/src/grow.rs\n--- a/src/grow.rs\n+++ b/src/grow.rs\n@@ -1,2 +1,27 @@\n pub fn grower() {\n",
        );
        for i in 0..25 {
            diff.push_str(&format!("+    let v{i} = {i};\n"));
        }
        diff.push_str(" }\n");
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "src/grow.rs".into(),
                contents,
            }],
            diff,
        };
        // High file threshold so only the function advisory can fire.
        let report = review_with_size_thresholds(&ctx, 100_000, 20).await;
        assert!(
            report.markdown.contains("## Size advisory") && report.markdown.contains("grower"),
            "expected a function size advisory naming `grower`: {}",
            report.markdown
        );
        assert_eq!(report.verdict, ReviewVerdict::Pass);
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
            provider: Some(ReviewerProvider::Anthropic),
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_SKIP_DEFAULT".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: crate::config::AutoRevise::Off,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
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
            provider: Some(ReviewerProvider::Anthropic),
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_SKIP_TRUE".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: crate::config::AutoRevise::Off,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: true,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
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
            provider: Some(ReviewerProvider::Anthropic),
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_SKIP_FALSE_GATE".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: crate::config::AutoRevise::Off,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
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
            provider: Some(ReviewerProvider::Anthropic),
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_KEY_OVERRIDE".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: Some(template_path),
            code_review: None,
            auto_revise: crate::config::AutoRevise::Off,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
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
            provider: Some(ReviewerProvider::Anthropic),
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_KEY_MISSING_TMPL".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: Some(bogus.clone()),
            code_review: None,
            auto_revise: crate::config::AutoRevise::Off,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
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
            provider: Some(ReviewerProvider::Anthropic),
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_KEY_NESTED".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: Some(legacy),
            code_review: Some(PromptOverrideBlock {
                prompt_path: Some(nested),
            }),
            auto_revise: crate::config::AutoRevise::Off,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
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
            provider: Some(ReviewerProvider::Anthropic),
            model: "x".into(),
            api_key_env: Some("REVIEWER_TEST_KEY_DEFAULT".into()),
            api_key: None,
            api_base_url: None,
            prompt_template_path: None,
            code_review: None,
            auto_revise: crate::config::AutoRevise::Off,
            prompt_budget_chars: 2_000_000,
            mode: crate::config::ReviewerMode::Bundled,
            max_code_reviews_per_pr: Some(5),
            suggest_rereview_threshold: None,
            skip_spec_only_prs: false,
            kind: crate::config::ReviewerKind::Oneshot,
            command: "claude".to_string(),
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

    /// a53 task 3.1: `review_pr_at_state_with` over a synthetic 3-change
    /// `ReviewContext` in per_change mode invokes the reviewer once per
    /// change (3 calls) AND returns a `ReviewResult` carrying 3
    /// `per_change_sections`, one per change, in input order. This is the
    /// regression the change pins: the operator-trigger entry point now
    /// honors `reviewer.mode == per_change` instead of always bundling.
    #[tokio::test]
    async fn review_pr_at_state_per_change_dispatches_once_per_change() {
        use std::sync::Mutex;
        struct CountingClient {
            prompts: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl LlmClient for CountingClient {
            async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
                self.prompts.lock().unwrap().push(prompt.to_string());
                Ok("VERDICT: Pass\n\nlooks fine\n".to_string())
            }
        }
        let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let client = Box::new(CountingClient { prompts: prompts.clone() });
        let reviewer = CodeReviewer::new(
            client,
            "{{cross_change_preamble}}{{changed_files}}{{diff}}".to_string(),
        )
        .with_mode(crate::config::ReviewerMode::PerChange);

        let brief = |name: &str| ChangeBrief {
            name: name.into(),
            proposal: format!("## Why\nreasons for {name}\n"),
            design: None,
            tasks: String::new(),
        };
        let ctx = ReviewContext {
            archived_changes: vec![brief("alpha"), brief("beta"), brief("gamma")],
            changed_files: vec![ChangedFile {
                path: "src/x.rs".into(),
                contents: "fn x() {}".into(),
            }],
            diff: "the union diff".into(),
        };

        let result = review_pr_at_state_with(&reviewer, &ctx)
            .await
            .expect("per-change review succeeds");
        assert_eq!(prompts.lock().unwrap().len(), 3, "one LLM call per change");
        assert_eq!(result.per_change_sections.len(), 3);
        let slugs: Vec<&str> = result
            .per_change_sections
            .iter()
            .map(|s| s.change_slug.as_str())
            .collect();
        assert_eq!(slugs, ["alpha", "beta", "gamma"]);
        for s in &result.per_change_sections {
            assert!(
                s.markdown.starts_with("VERDICT: "),
                "each section body carries its own verdict line"
            );
        }
    }

    /// a53 task 3.2: in bundled mode `review_pr_at_state_with` invokes the
    /// reviewer exactly once, returns empty `per_change_sections`, AND its
    /// markdown is byte-identical to a direct `CodeReviewer::review` of the
    /// same context — no behavior change for the default path.
    #[tokio::test]
    async fn review_pr_at_state_bundled_single_call_empty_sections() {
        use std::sync::Mutex;
        struct CountingClient {
            prompts: Arc<Mutex<Vec<String>>>,
            response: String,
        }
        #[async_trait]
        impl LlmClient for CountingClient {
            async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
                self.prompts.lock().unwrap().push(prompt.to_string());
                Ok(self.response.clone())
            }
        }
        let raw = "VERDICT: Concerns\n\nminor nit.\n";
        let make_ctx = || ReviewContext {
            archived_changes: vec![
                ChangeBrief {
                    name: "alpha".into(),
                    proposal: "## Why\na\n".into(),
                    design: None,
                    tasks: String::new(),
                },
                ChangeBrief {
                    name: "beta".into(),
                    proposal: "## Why\nb\n".into(),
                    design: None,
                    tasks: String::new(),
                },
            ],
            changed_files: vec![ChangedFile {
                path: "src/x.rs".into(),
                contents: "fn x() {}".into(),
            }],
            diff: "the diff".into(),
        };

        // Reference: a direct bundled review of the same context.
        let (ref_client, _) = stub_with_capture(raw);
        let ref_reviewer = CodeReviewer::new(ref_client, "{{changed_files}}{{diff}}".to_string());
        let ref_report = ref_reviewer.review(&make_ctx()).await.unwrap();

        // System under test: the bundled-mode entry point.
        let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let client = Box::new(CountingClient {
            prompts: prompts.clone(),
            response: raw.to_string(),
        });
        let reviewer = CodeReviewer::new(client, "{{changed_files}}{{diff}}".to_string())
            .with_mode(crate::config::ReviewerMode::Bundled);
        let result = review_pr_at_state_with(&reviewer, &make_ctx()).await.unwrap();

        assert_eq!(
            prompts.lock().unwrap().len(),
            1,
            "bundled mode: exactly one LLM call regardless of change count"
        );
        assert!(
            result.per_change_sections.is_empty(),
            "bundled mode leaves per_change_sections empty"
        );
        assert_eq!(
            result.markdown, ref_report.markdown,
            "bundled output byte-identical to a direct review"
        );
        assert_eq!(Verdict::from(ref_report.verdict), result.verdict);
    }

    /// a015 task 2.1: `per_change` mode with an empty `archived_changes`
    /// context (the split yields zero sub-contexts) but a non-empty
    /// diff/changed_files falls back to a single bundled review. Exactly
    /// one reviewer invocation occurs AND the verdict is the one the
    /// stubbed bundled review returns — NOT a defaulted `Pass`/`Approve`
    /// synthesized from zero reviews. The stub returns `Block` precisely
    /// because `Block` is the only verdict that does not map to `Approve`:
    /// if the pre-a015 bug were present (empty synthesis → `Pass` →
    /// `Approve`), this assertion would fail.
    #[tokio::test]
    async fn per_change_empty_split_falls_back_to_bundled_with_real_verdict() {
        use std::sync::Mutex;
        struct CountingClient {
            prompts: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl LlmClient for CountingClient {
            async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
                self.prompts.lock().unwrap().push(prompt.to_string());
                Ok("VERDICT: Block\n\nbundled review found a real problem\n".to_string())
            }
        }
        let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let client = Box::new(CountingClient { prompts: prompts.clone() });
        let reviewer = CodeReviewer::new(client, "{{changed_files}}{{diff}}".to_string())
            .with_mode(crate::config::ReviewerMode::PerChange);

        // Empty archived_changes → split yields zero sub-contexts, but the
        // PR still has a real diff and changed files to review.
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "src/x.rs".into(),
                contents: "fn x() {}".into(),
            }],
            diff: "the union diff".into(),
        };

        let result = review_pr_at_state_with(&reviewer, &ctx)
            .await
            .expect("fallback bundled review succeeds");

        assert_eq!(
            prompts.lock().unwrap().len(),
            1,
            "empty split falls back to exactly one bundled reviewer invocation"
        );
        assert_eq!(
            result.verdict,
            Verdict::Block,
            "verdict comes from the bundled review, not a defaulted Pass/Approve"
        );
        assert!(
            result.per_change_sections.is_empty(),
            "the fallback is a bundled review — no per-change sections"
        );
        assert!(result.markdown.contains("bundled review found a real problem"));
    }

    /// a015 task 2.2: the fallback bundled review is handed the context's
    /// diff and changed files (asserting on what the stub reviewer
    /// received, not on any log/message wording). Proves the reviewer
    /// builds its prompt over the real context rather than skipping the
    /// call.
    #[tokio::test]
    async fn per_change_empty_split_fallback_passes_diff_and_files() {
        let (client, captured) = stub_with_capture("VERDICT: Concerns\n\nnit\n");
        let reviewer = CodeReviewer::new(client, "{{changed_files}}{{diff}}".to_string())
            .with_mode(crate::config::ReviewerMode::PerChange);

        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "src/touched.rs".into(),
                contents: "FILE_BODY_SENTINEL_a015".into(),
            }],
            diff: "DIFF_SENTINEL_a015".into(),
        };

        let _ = review_pr_at_state_with(&reviewer, &ctx)
            .await
            .expect("fallback bundled review succeeds");

        let prompt = captured
            .lock()
            .unwrap()
            .clone()
            .expect("the reviewer built and submitted a prompt");
        assert!(
            prompt.contains("DIFF_SENTINEL_a015"),
            "the fallback review receives the context's diff"
        );
        assert!(
            prompt.contains("FILE_BODY_SENTINEL_a015"),
            "the fallback review receives the context's changed files"
        );
    }

    /// a015 task 2.3 (regression): `per_change` mode with a populated
    /// `archived_changes` (≥1 change) still dispatches one review per
    /// change and synthesizes the results — no bundled fallback fires.
    #[tokio::test]
    async fn per_change_populated_split_still_dispatches_per_change() {
        use std::sync::Mutex;
        struct CountingClient {
            prompts: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl LlmClient for CountingClient {
            async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
                self.prompts.lock().unwrap().push(prompt.to_string());
                Ok("VERDICT: Pass\n\nlooks fine\n".to_string())
            }
        }
        let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let client = Box::new(CountingClient { prompts: prompts.clone() });
        let reviewer = CodeReviewer::new(
            client,
            "{{cross_change_preamble}}{{changed_files}}{{diff}}".to_string(),
        )
        .with_mode(crate::config::ReviewerMode::PerChange);

        let brief = |name: &str| ChangeBrief {
            name: name.into(),
            proposal: format!("## Why\nreasons for {name}\n"),
            design: None,
            tasks: String::new(),
        };
        let ctx = ReviewContext {
            archived_changes: vec![brief("alpha"), brief("beta")],
            changed_files: vec![ChangedFile {
                path: "src/x.rs".into(),
                contents: "fn x() {}".into(),
            }],
            diff: "the union diff".into(),
        };

        let result = review_pr_at_state_with(&reviewer, &ctx)
            .await
            .expect("per-change review succeeds");

        assert_eq!(
            prompts.lock().unwrap().len(),
            2,
            "one reviewer invocation per change — no bundled fallback"
        );
        let slugs: Vec<&str> = result
            .per_change_sections
            .iter()
            .map(|s| s.change_slug.as_str())
            .collect();
        assert_eq!(
            slugs,
            ["alpha", "beta"],
            "results are synthesized per change, in input order"
        );
    }

    /// a015 task 1.2: the empty-input guard on `synthesize_per_change_report`
    /// makes the "never a defaulted Pass" invariant explicit. Called with
    /// an empty vec it returns a non-`Pass` (here `Block`) verdict so a
    /// synthesis from zero reviews can never become a silent approval.
    #[test]
    fn synthesize_per_change_report_empty_input_is_not_pass() {
        let report = synthesize_per_change_report(Vec::new());
        assert_ne!(
            report.verdict,
            ReviewVerdict::Pass,
            "an empty per-change synthesis must never default to Pass"
        );
        assert_ne!(
            Verdict::from(report.verdict),
            Verdict::Approve,
            "an empty per-change synthesis must never map to Approve"
        );
        assert!(report.per_change_sections.is_empty());
        assert!(report.concerns.is_empty());
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

    /// a002 regression (task 3.4): a `ReviewContext` whose changed files
    /// contain the literal `{{diff}}` AND `{{changed_files}}` tokens — the
    /// self-hosting case where the change under review edits the reviewer's
    /// own spec/code/docs — renders a prompt that does NOT re-expand those
    /// literals. Under the old chained `.replace`, the final
    /// `.replace("{{diff}}", …)` stamped the whole diff into every literal
    /// `{{diff}}` carried in the changed files, exploding the prompt.
    #[tokio::test]
    async fn changed_file_placeholder_literals_are_not_re_expanded() {
        let (client, captured) = stub_with_capture("VERDICT: Pass\n");
        // A realistic template wraps each section in delimiters.
        let template =
            "CTX<<<{{change_context}}>>>\nFILES<<<{{changed_files}}>>>\nDIFF<<<{{diff}}>>>"
                .to_string();
        let reviewer = CodeReviewer::new(client, template.clone());

        // The changed file's contents carry MANY literal placeholder tokens
        // (as the reviewer's own spec docs do). The diff itself is large so
        // that re-expansion would be conspicuous.
        let file_contents = "documents {{diff}} and {{changed_files}} tokens\n".repeat(50);
        let diff = "D".repeat(10_000);
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "openspec/specs/code-reviewer/spec.md".into(),
                contents: file_contents.clone(),
            }],
            diff: diff.clone(),
        };
        reviewer.review(&ctx).await.unwrap();
        let prompt = captured.lock().unwrap().clone().unwrap();

        // The literal tokens survive verbatim in the changed-files section.
        assert!(
            prompt.contains("documents {{diff}} and {{changed_files}} tokens"),
            "literal placeholder tokens must survive verbatim in the changed-files section"
        );
        // The big diff is inserted exactly once (at the template's own
        // `{{diff}}`), NOT once per literal carried in the file.
        assert_eq!(
            prompt.matches(&diff).count(),
            1,
            "the diff must be inserted exactly once, not re-stamped into every literal"
        );

        // Size bound: the rendered prompt cannot exceed the sum of the
        // section values plus the template scaffolding. (`render_sections`
        // builds the changed-files section with `## File:` headers, so we
        // bound by file_contents + a small per-file header allowance rather
        // than by the bare contents.)
        let rendered = render_sections(&ctx, reviewer.prompt_budget());
        let bound = rendered.change_context.len()
            + rendered.changed_files.len()
            + rendered.diff_or_explanation.len()
            + template.len();
        assert!(
            prompt.len() <= bound,
            "prompt size {} must be bounded by section sizes + template = {bound} \
             (no multiplicative blowup)",
            prompt.len()
        );
    }

    // =================================================================
    // a58: agentic reviewer transport
    // =================================================================

    use serde_json::json;
    use std::collections::VecDeque;

    fn brief(name: &str) -> ChangeBrief {
        ChangeBrief {
            name: name.into(),
            proposal: "## Why\nbecause reasons".into(),
            design: None,
            tasks: "- [x] do the thing".into(),
        }
    }

    fn valid_review_payload(verdict: &str) -> serde_json::Value {
        json!({ "verdict": verdict, "summary": "looks ok", "concerns": [] })
    }

    /// Test session runner: records the slugs + prompts it saw AND returns
    /// canned submissions (front-of-queue), bypassing any CLI spawn.
    struct CannedRunner {
        submissions: Mutex<VecDeque<Option<serde_json::Value>>>,
        slugs: Mutex<Vec<String>>,
        prompts: Mutex<Vec<String>>,
    }
    impl CannedRunner {
        fn new(subs: Vec<Option<serde_json::Value>>) -> Self {
            Self {
                submissions: Mutex::new(subs.into_iter().collect()),
                slugs: Mutex::new(Vec::new()),
                prompts: Mutex::new(Vec::new()),
            }
        }
        fn session_count(&self) -> usize {
            self.slugs.lock().unwrap().len()
        }
    }
    #[async_trait]
    impl ReviewSessionRunner for CannedRunner {
        async fn run_session(&self, slug: &str, prompt: &str) -> Result<Option<Value>> {
            self.slugs.lock().unwrap().push(slug.to_string());
            self.prompts.lock().unwrap().push(prompt.to_string());
            let next = self.submissions.lock().unwrap().pop_front();
            Ok(next.unwrap_or(None))
        }
    }

    /// The `oneshot` transport's prompt + parsed output are byte-identical
    /// to the pre-change one-shot path (the agentic branch is never taken).
    /// `CodeReviewer::new` is the test-only constructor and keeps `oneshot`
    /// so this surface exercises the HTTP path directly; the operator-facing
    /// `reviewer.kind` config default is `agentic` since a64 (see
    /// `config::ReviewerKind` AND `startup_reviewer_kind_decision`).
    #[tokio::test]
    async fn oneshot_kind_is_byte_identical() {
        let (client, captured) = stub_with_capture("VERDICT: Pass\n\nthe review body");
        let reviewer = CodeReviewer::new(client, "{{diff}}".to_string());
        assert_eq!(
            reviewer.kind(),
            ReviewerKind::Oneshot,
            "the test-only `new` constructor keeps oneshot"
        );
        let ctx = ctx_with_diff("DIFFTEXT");
        let result = review_pr_at_state_with(&reviewer, &ctx).await.unwrap();
        // The one-shot prompt is the unchanged render: the bare diff for a
        // `{{diff}}`-only template — no agentic briefs/file-list framing.
        let prompt = captured.lock().unwrap().clone().unwrap();
        assert_eq!(prompt, "DIFFTEXT");
        assert_eq!(result.verdict, Verdict::Approve);
        assert_eq!(result.markdown, "the review body");
    }

    // =================================================================
    // a64: startup CLI-availability fallback (tasks 3.1–3.4)
    // =================================================================

    /// An absolute path to a file that is guaranteed to exist on the host
    /// (the running test binary). `reviewer_binary_on_path` treats a
    /// path-qualified command as "available" when the file exists, giving the
    /// "CLI present" branch a deterministic input that does not depend on
    /// what bare-name binaries happen to be on the CI `$PATH`.
    fn present_cli() -> String {
        std::env::current_exe()
            .expect("current_exe resolves in tests")
            .to_string_lossy()
            .into_owned()
    }

    /// A bare command name guaranteed NOT to be on any sane `$PATH`, so
    /// `reviewer_binary_on_path` reports it missing.
    const MISSING_CLI: &str = "autocoder-a64-definitely-not-installed-cli";

    /// 3.1: unset `reviewer.kind` resolves to agentic AND, with an available
    /// reviewer CLI, the startup resolver keeps the reviewer agentic with no
    /// fallback WARN.
    #[test]
    fn unset_kind_with_available_cli_stays_agentic() {
        // Unset kind defaults to agentic (the `new` test constructor is
        // oneshot, so model the config default explicitly).
        assert_eq!(ReviewerKind::default(), ReviewerKind::Agentic);

        let (client, _captured) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "{{diff}}".to_string())
            .with_kind(ReviewerKind::Agentic)
            .with_command(present_cli());
        let (effective, warn) = resolve_startup_reviewer_kind(&reviewer);
        assert_eq!(effective, ReviewerKind::Agentic, "available CLI → agentic");
        assert!(warn.is_none(), "no WARN when the CLI is available: {warn:?}");
    }

    /// 3.2 / 3.3: an effective-agentic reviewer (defaulted OR explicit) whose
    /// CLI is unavailable degrades to `oneshot` for the boot AND emits exactly
    /// one WARN naming the CLI + the remedy. Review is NOT disabled — the
    /// effective kind is `oneshot`, not "off".
    #[test]
    fn agentic_with_unavailable_cli_falls_back_to_oneshot_with_warn() {
        let (client, _captured) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "{{diff}}".to_string())
            .with_kind(ReviewerKind::Agentic)
            .with_command(MISSING_CLI.to_string());
        let (effective, warn) = resolve_startup_reviewer_kind(&reviewer);
        assert_eq!(
            effective,
            ReviewerKind::Oneshot,
            "missing CLI → oneshot fallback (review continues, not disabled)"
        );
        let warn = warn.expect("missing CLI must produce a fallback WARN");
        assert!(
            warn.contains(MISSING_CLI),
            "WARN must name the missing CLI: {warn}"
        );
        assert!(
            warn.contains("oneshot"),
            "WARN must name the `reviewer.kind: oneshot` remedy: {warn}"
        );
    }

    /// 3.4: an explicit `oneshot` reviewer is honored with no probe, no
    /// agentic session, AND no fallback WARN — even when the CLI is missing
    /// (the operator opted out deliberately).
    #[test]
    fn explicit_oneshot_is_honored_without_warn() {
        let (client, _captured) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "{{diff}}".to_string())
            .with_kind(ReviewerKind::Oneshot)
            .with_command(MISSING_CLI.to_string());
        let (effective, warn) = resolve_startup_reviewer_kind(&reviewer);
        assert_eq!(effective, ReviewerKind::Oneshot);
        assert!(warn.is_none(), "explicit oneshot never warns: {warn:?}");
    }

    /// The pure decision function covers all four arms independently of any
    /// host probe (tasks 3.1–3.4 condensed): oneshot is always honored
    /// warning-free; agentic + available stays agentic; agentic + unavailable
    /// degrades to oneshot with a CLI-naming, remedy-naming WARN.
    #[test]
    fn startup_kind_decision_truth_table() {
        // Oneshot configured: honored, never warns, regardless of availability.
        for available in [true, false] {
            assert_eq!(
                startup_reviewer_kind_decision(ReviewerKind::Oneshot, "claude", available),
                (ReviewerKind::Oneshot, None)
            );
        }
        // Agentic + available CLI: agentic, no warn.
        assert_eq!(
            startup_reviewer_kind_decision(ReviewerKind::Agentic, "claude", true),
            (ReviewerKind::Agentic, None)
        );
        // Agentic + unavailable CLI: oneshot + WARN naming the CLI and remedy.
        let (kind, warn) =
            startup_reviewer_kind_decision(ReviewerKind::Agentic, "qwen-cli", false);
        assert_eq!(kind, ReviewerKind::Oneshot);
        let warn = warn.expect("unavailable agentic CLI warns");
        assert!(warn.contains("qwen-cli"), "names the CLI: {warn}");
        assert!(warn.contains("oneshot"), "names the remedy: {warn}");
    }

    /// `reviewer_binary_on_path` finds a real file via an absolute path AND
    /// reports a bare name absent from `$PATH` as missing — the primitive the
    /// startup resolver's binary check rests on.
    #[test]
    fn binary_on_path_detects_present_and_missing() {
        assert!(
            reviewer_binary_on_path(&present_cli()),
            "an absolute path to an existing file is available"
        );
        assert!(
            !reviewer_binary_on_path(MISSING_CLI),
            "a bare name not on PATH is unavailable"
        );
    }

    /// 4.2: the agentic sandbox advertises Read/Glob/Grep + `submit_review`
    /// AND does NOT advertise Bash/Write/Edit.
    #[test]
    fn agentic_sandbox_advertises_readonly_tools_plus_submit_review() {
        let tools = agentic_review_allowed_tools();
        for required in ["Read", "Glob", "Grep"] {
            assert!(
                tools.iter().any(|t| t == required),
                "must advertise {required}: {tools:?}"
            );
        }
        assert!(
            tools.iter().any(|t| t.contains("submit_review")),
            "must advertise submit_review: {tools:?}"
        );
        for forbidden in ["Bash", "Write", "Edit"] {
            assert!(
                !tools.iter().any(|t| t == forbidden),
                "must NOT advertise {forbidden}: {tools:?}"
            );
        }
    }

    /// 4.2 (defense in depth): the agentic sandbox settings file denies
    /// `Write`/`Edit` (the read-only `deny_writes` backstop).
    #[test]
    fn agentic_sandbox_settings_deny_writes() {
        let sandbox = crate::config::ResolvedSandbox {
            allowed_tools: AGENTIC_REVIEW_ALLOWED_TOOLS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            disallowed_bash_patterns: Vec::new(),
            disallowed_read_paths: Vec::new(),
        };
        let dir = tempfile::TempDir::new().unwrap();
        let (path, _guard) =
            crate::audits::write_sandbox_settings(&sandbox, Some(dir.path()), true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("Write(*)"), "deny list must contain Write(*): {raw}");
        assert!(raw.contains("Edit(*)"), "deny list must contain Edit(*): {raw}");
    }

    /// The agentic prompt lists changed-file PATHS (not their contents),
    /// includes the diff, AND produces no budget-exhaustion footer — the
    /// agent reads files on demand, so `prompt_budget_chars` does not apply.
    #[test]
    fn agentic_prompt_lists_paths_not_contents() {
        let ctx = ReviewContext {
            archived_changes: vec![brief("demo")],
            changed_files: vec![ChangedFile {
                path: "src/big.rs".into(),
                contents: "SECRET_FILE_BODY".repeat(1000),
            }],
            diff: "DIFFBODY".into(),
        };
        let prompt = render_agentic_review_prompt(&ctx, "");
        assert!(prompt.contains("src/big.rs"), "path must be listed");
        assert!(
            !prompt.contains("SECRET_FILE_BODY"),
            "full file contents must NOT be inlined (read on demand)"
        );
        assert!(prompt.contains("DIFFBODY"), "diff must be included");
        assert!(
            !prompt.contains("Skipped (budget exhausted)"),
            "no budget-exhaustion footer in the agentic prompt"
        );
        assert!(prompt.contains("submit_review"), "must instruct submit_review");
    }

    /// 4.3: a schema-valid `submit_review` payload round-trips
    /// `record_submission` → `consume_submission` → the expected
    /// `ReviewResult` (verdict + concerns + raw_output).
    #[test]
    fn submit_review_payload_round_trips_to_review_result() {
        use crate::submission_store::SubmissionStore;
        let store = SubmissionStore::new();
        register_reviewer_submission_schema(&store);
        let payload = json!({
            "verdict": "Block",
            "summary": "found a real issue",
            "concerns": [{
                "title": "sql injection",
                "detail": "user input is concatenated into the query",
                "anchor": "src/db.rs:42",
                "should_request_revision": true,
                "actionable_request": "use parameterized queries"
            }]
        });
        store
            .record("repo".into(), REVIEWER_ROLE.into(), REVIEWER_ROLE, payload)
            .expect("valid payload records");
        let consumed = store.consume("repo", REVIEWER_ROLE).expect("entry present");
        let result = payload_to_review_result(&consumed).expect("maps to ReviewResult");
        assert_eq!(result.verdict, Verdict::Block);
        assert_eq!(result.concerns.len(), 1);
        assert!(result.concerns[0].should_request_revision);
        assert_eq!(
            result.concerns[0].actionable_request.as_deref(),
            Some("use parameterized queries")
        );
        assert_eq!(result.per_concern.len(), 1);
        assert!(result.raw_output.contains("found a real issue"));
        assert!(result.raw_output.contains("sql injection"));
        // Drained: a second consume returns nothing.
        assert!(store.consume("repo", REVIEWER_ROLE).is_none());
    }

    /// a004 (agentic path, tasks 3.1/3.2): a `submit_review` payload that
    /// flags a finding `security_critical: true` but returns `Approve` is
    /// escalated to `Block` by `payload_to_review_result`, keyed on the
    /// structured signal.
    #[test]
    fn agentic_security_finding_escalates_approve_to_block() {
        let payload = json!({
            "verdict": "Approve",
            "summary": "mostly fine",
            "concerns": [{
                "title": "api key written to opencode.json",
                "detail": "the key lands in a committable workspace file",
                "anchor": "src/config.rs:10",
                "should_request_revision": true,
                "actionable_request": "read the key from an env var at runtime",
                "security_critical": true
            }]
        });
        let result = payload_to_review_result(&payload).expect("maps to ReviewResult");
        assert_eq!(
            result.verdict,
            Verdict::Block,
            "a security_critical concern must escalate Approve to Block"
        );
        assert!(result.concerns[0].security_critical);
    }

    /// a004 (agentic path, task 3.3): a payload with only non-security
    /// concerns (`security_critical` omitted → false) keeps its `Approve`
    /// verdict — no escalation.
    #[test]
    fn agentic_non_security_concern_is_not_escalated() {
        let payload = json!({
            "verdict": "Approve",
            "summary": "minor nits",
            "concerns": [{
                "title": "rename tmp",
                "detail": "unclear name",
                "anchor": "src/x.rs:3",
                "should_request_revision": false
            }]
        });
        let result = payload_to_review_result(&payload).expect("maps to ReviewResult");
        assert_eq!(result.verdict, Verdict::Approve);
        assert!(!result.concerns[0].security_critical);
    }

    /// a004 (agentic path, task 3.4): the escalation keys on the structured
    /// signal, not the wording. A credential-leak-worded but unflagged
    /// concern stays `Approve`; an innocuous-worded but flagged concern
    /// escalates to `Block`.
    #[test]
    fn agentic_escalation_keys_on_signal_not_wording() {
        let worded_but_unflagged = json!({
            "verdict": "Approve",
            "summary": "s",
            "concerns": [{
                "title": "possible credential leak / secret / api key exposure",
                "detail": "d",
                "anchor": "a",
                "should_request_revision": false
            }]
        });
        let r = payload_to_review_result(&worded_but_unflagged).expect("maps");
        assert_eq!(
            r.verdict,
            Verdict::Approve,
            "wording alone must not escalate the agentic verdict"
        );

        let flagged_but_innocuous = json!({
            "verdict": "Approve",
            "summary": "s",
            "concerns": [{
                "title": "tidy up helper foo",
                "detail": "d",
                "anchor": "a",
                "should_request_revision": false,
                "security_critical": true
            }]
        });
        let r = payload_to_review_result(&flagged_but_innocuous).expect("maps");
        assert_eq!(r.verdict, Verdict::Block);
    }

    /// 4.4: a non-enum verdict AND a `should_request_revision` concern with
    /// an empty `actionable_request` are each rejected as a correctable
    /// error; a subsequent valid submission in the same execution succeeds.
    #[test]
    fn submit_review_rejects_bad_verdict_and_missing_request() {
        let bad_verdict = json!({ "verdict": "LookGoodToMe", "summary": "s", "concerns": [] });
        let e = payload_to_review_result(&bad_verdict).expect_err("non-enum verdict rejected");
        assert!(e.contains("verdict"), "reason names the verdict: {e}");

        let bad_concern = json!({
            "verdict": "Block",
            "summary": "s",
            "concerns": [{
                "title": "t", "detail": "d", "anchor": "a",
                "should_request_revision": true,
                "actionable_request": ""
            }]
        });
        let e2 = payload_to_review_result(&bad_concern)
            .expect_err("should_request_revision without actionable_request rejected");
        assert!(e2.contains("actionable_request"), "reason names the field: {e2}");

        // A subsequent valid submission succeeds.
        let good = json!({ "verdict": "Approve", "summary": "s", "concerns": [] });
        assert!(payload_to_review_result(&good).is_ok());
    }

    /// 4.4 (store-level): a rejected `submit_review` payload stores nothing,
    /// AND a subsequent valid submission for the same execution is accepted.
    #[test]
    fn submit_review_rejection_does_not_store_then_valid_accepted() {
        use crate::submission_store::SubmissionStore;
        let store = SubmissionStore::new();
        register_reviewer_submission_schema(&store);
        let bad = json!({ "verdict": "Maybe", "summary": "s", "concerns": [] });
        assert!(
            store
                .record("r".into(), REVIEWER_ROLE.into(), REVIEWER_ROLE, bad)
                .is_err(),
            "schema-invalid payload is rejected"
        );
        assert!(store.consume("r", REVIEWER_ROLE).is_none(), "nothing stored");
        store
            .record(
                "r".into(),
                REVIEWER_ROLE.into(),
                REVIEWER_ROLE,
                valid_review_payload("Approve"),
            )
            .expect("subsequent valid payload accepted");
        assert!(store.consume("r", REVIEWER_ROLE).is_some());
    }

    /// 4.5: an agentic session that ends with no valid submission discards
    /// the review (no verdict written, no auto-approve).
    #[tokio::test]
    async fn agentic_no_submission_discards_review() {
        let (client, _) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "t".to_string());
        let runner = CannedRunner::new(vec![None]);
        let outcome = run_agentic_review_with_runner(&reviewer, &ReviewContext::default(), &runner)
            .await
            .unwrap();
        match outcome {
            AgenticReviewOutcome::Discarded { reason } => {
                assert!(reason.contains("no valid submit_review"), "reason: {reason}");
            }
            AgenticReviewOutcome::Reviewed(_) => {
                panic!("a missing submission must discard, never produce a verdict")
            }
        }
        assert_eq!(runner.session_count(), 1);
    }

    /// A schema-valid submission drives a bundled `Reviewed` outcome whose
    /// verdict + concerns come from the payload, AND the reviewer's
    /// attribution is stamped onto the result.
    #[tokio::test]
    async fn agentic_valid_submission_produces_reviewed_outcome() {
        let (client, _) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "t".to_string())
            .with_attribution(Some("anthropic/claude-opus-4-8".to_string()));
        let payload = json!({
            "verdict": "Approve",
            "summary": "all good",
            "concerns": []
        });
        let runner = CannedRunner::new(vec![Some(payload)]);
        let outcome = run_agentic_review_with_runner(&reviewer, &ReviewContext::default(), &runner)
            .await
            .unwrap();
        match outcome {
            AgenticReviewOutcome::Reviewed(r) => {
                assert_eq!(r.verdict, Verdict::Approve);
                assert!(r.per_change_sections.is_empty(), "bundled has no per-change sections");
                assert_eq!(r.attribution.as_deref(), Some("anthropic/claude-opus-4-8"));
            }
            AgenticReviewOutcome::Discarded { .. } => panic!("expected a reviewed outcome"),
        }
    }

    /// 4.7: `reviewer.mode: per_change` dispatches one agentic session per
    /// change; the per-change results synthesize into one `ReviewResult`
    /// with one section per change AND the worst-of verdict (any Block →
    /// Block), feeding the same disposition the one-shot path produces.
    #[tokio::test]
    async fn agentic_per_change_runs_one_session_per_change() {
        let (client, _) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "t".to_string())
            .with_mode(crate::config::ReviewerMode::PerChange);
        let ctx = ReviewContext {
            archived_changes: vec![brief("a-one"), brief("b-two"), brief("c-three")],
            changed_files: Vec::new(),
            diff: "d".into(),
        };
        let runner = CannedRunner::new(vec![
            Some(valid_review_payload("Approve")),
            Some(valid_review_payload("Block")),
            Some(valid_review_payload("Approve")),
        ]);
        let outcome = run_agentic_review_with_runner(&reviewer, &ctx, &runner)
            .await
            .unwrap();
        assert_eq!(runner.session_count(), 3, "one session per change");
        match outcome {
            AgenticReviewOutcome::Reviewed(r) => {
                assert_eq!(r.per_change_sections.len(), 3);
                assert_eq!(r.verdict, Verdict::Block, "any Block change blocks the PR");
                let slugs: Vec<&str> = r
                    .per_change_sections
                    .iter()
                    .map(|s| s.change_slug.as_str())
                    .collect();
                assert_eq!(slugs, vec!["a-one", "b-two", "c-three"]);
            }
            AgenticReviewOutcome::Discarded { .. } => panic!("expected a reviewed outcome"),
        }
    }

    /// 4.7 (per-change discard): if ANY per-change session records no valid
    /// submission, the whole review is discarded (never partially approved).
    #[tokio::test]
    async fn agentic_per_change_one_missing_submission_discards_all() {
        let (client, _) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "t".to_string())
            .with_mode(crate::config::ReviewerMode::PerChange);
        let ctx = ReviewContext {
            archived_changes: vec![brief("a-one"), brief("b-two")],
            changed_files: Vec::new(),
            diff: "d".into(),
        };
        let runner = CannedRunner::new(vec![Some(valid_review_payload("Approve")), None]);
        let outcome = run_agentic_review_with_runner(&reviewer, &ctx, &runner)
            .await
            .unwrap();
        assert!(matches!(outcome, AgenticReviewOutcome::Discarded { .. }));
    }

    /// a015 (agentic path): `per_change` mode whose split yields ZERO
    /// sub-contexts (empty `archived_changes`) but a real diff falls back to
    /// a single BUNDLED session — exactly one reviewer session runs AND the
    /// verdict is the one that session returned, NOT a defaulted
    /// `Approve`/`Reviewed` synthesized from zero reviews. The canned
    /// submission returns `Block` precisely because `Block` is the only
    /// verdict that does not map to an approval: if the pre-fix bug were
    /// present (empty split → zero sessions → `synthesize_agentic_per_change`
    /// defaulting to `Approve`), this assertion would fail.
    #[tokio::test]
    async fn agentic_per_change_empty_split_falls_back_to_bundled_with_real_verdict() {
        let (client, _) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "t".to_string())
            .with_mode(crate::config::ReviewerMode::PerChange);
        // Empty archived_changes → split yields zero sub-contexts, but the
        // PR still has a real diff and changed files to review.
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "src/x.rs".into(),
                contents: "fn x() {}".into(),
            }],
            diff: "the union diff".into(),
        };
        let runner = CannedRunner::new(vec![Some(valid_review_payload("Block"))]);
        let outcome = run_agentic_review_with_runner(&reviewer, &ctx, &runner)
            .await
            .unwrap();
        assert_eq!(
            runner.session_count(),
            1,
            "empty split falls back to exactly one bundled reviewer session"
        );
        assert_eq!(
            runner.slugs.lock().unwrap().as_slice(),
            [String::new()],
            "the fallback session is bundled (empty slug), not per-change"
        );
        match outcome {
            AgenticReviewOutcome::Reviewed(r) => {
                assert_eq!(
                    r.verdict,
                    Verdict::Block,
                    "verdict comes from the bundled review, not a defaulted Approve"
                );
                assert!(
                    r.per_change_sections.is_empty(),
                    "the fallback is a bundled review — no per-change sections"
                );
            }
            AgenticReviewOutcome::Discarded { .. } => {
                panic!("a valid bundled submission must produce a reviewed outcome")
            }
        }
    }

    /// a015 (agentic path): the fallback bundled session is handed the
    /// context's diff and changed files (asserting on the prompt the stub
    /// runner received, not on any log/message wording). Proves the reviewer
    /// builds its prompt over the real context rather than skipping the call.
    #[tokio::test]
    async fn agentic_per_change_empty_split_fallback_passes_diff_and_files() {
        let (client, _) = stub_with_capture("");
        let reviewer = CodeReviewer::new(client, "t".to_string())
            .with_mode(crate::config::ReviewerMode::PerChange);
        let ctx = ReviewContext {
            archived_changes: Vec::new(),
            changed_files: vec![ChangedFile {
                path: "src/TOUCHED_SENTINEL_a015.rs".into(),
                contents: "fn x() {}".into(),
            }],
            diff: "DIFF_SENTINEL_a015".into(),
        };
        let runner = CannedRunner::new(vec![Some(valid_review_payload("Approve"))]);
        let _ = run_agentic_review_with_runner(&reviewer, &ctx, &runner)
            .await
            .unwrap();
        let prompts = runner.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1, "exactly one bundled session prompt");
        assert!(
            prompts[0].contains("DIFF_SENTINEL_a015"),
            "the fallback review receives the context's diff"
        );
        assert!(
            prompts[0].contains("src/TOUCHED_SENTINEL_a015.rs"),
            "the fallback review receives the context's changed files"
        );
    }

    /// a015 (agentic path): the empty-input guard on
    /// `synthesize_agentic_per_change` makes the "never a defaulted Approve"
    /// invariant explicit. Called with an empty `reviews` vec it returns
    /// `Block` (not `Approve`), so a synthesis from zero reviews can never
    /// become a silent approval even if a future caller reaches it directly.
    #[test]
    fn synthesize_agentic_per_change_empty_input_is_block() {
        let result = synthesize_agentic_per_change(Vec::new(), Some("p/m".to_string()));
        assert_eq!(
            result.verdict,
            Verdict::Block,
            "an empty per-change synthesis must never default to Approve"
        );
        assert!(result.per_change_sections.is_empty());
        assert!(result.concerns.is_empty());
        assert_eq!(
            result.attribution.as_deref(),
            Some("p/m"),
            "attribution is preserved through the guard"
        );
    }

    /// A reviewer whose provider resolves (via the a55 provider→CLI rule) to
    /// the `opencode` CLI now resolves to a working strategy (a60 registered
    /// it); an Anthropic reviewer resolves the `claude` strategy. Neither
    /// spawns a subprocess at resolution time.
    #[test]
    fn agentic_strategy_resolution_resolves_registered_clis() {
        let (c1, _) = stub_with_capture("");
        let opencode_reviewer = CodeReviewer::new(c1, "t".to_string())
            .with_provider(LlmProvider::OpenAiCompatible)
            .with_command("opencode".to_string());
        assert!(
            resolve_reviewer_strategy(&opencode_reviewer).is_ok(),
            "openai_compatible reviewer resolves the opencode strategy (a60)"
        );

        let (c2, _) = stub_with_capture("");
        let claude_reviewer = CodeReviewer::new(c2, "t".to_string());
        assert!(
            resolve_reviewer_strategy(&claude_reviewer).is_ok(),
            "Anthropic reviewer resolves the claude strategy"
        );
    }
}
