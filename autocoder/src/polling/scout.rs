//! Scout polling handler (a25). One pass per iteration: drain ONE
//! pending scout request from the per-repo queue, gather workspace
//! context, invoke the executor in scout mode, parse + validate the
//! returned JSON-list response, AND persist the resulting `ScoutRunState`
//! before posting the rendered list to the request's lifecycle thread.
//!
//! Workspace contract:
//!   - On entry: the polling loop has already loaded the repo snapshot
//!     AND prepared the per-repo workspace.
//!   - On exit: success → state file persisted, thread reply posted.
//!     Failure → no state file, thread reply names the failure.

use crate::config::{GithubConfig, RepositoryConfig, ScoutFeatureConfig};
use crate::control_socket::{ClearScoutRequest, ScoutRequest};
use crate::executor::{Executor, ExecutorOutcome, ScoutContext};
use crate::polling_loop::ChatOpsContext;
use crate::state::scout_run::{
    self, ScoutItem, ScoutRunState, validate_items,
};
use crate::{git, github};
use anyhow::{Context, Result};
use std::path::Path;

/// Slack/Mattermost/etc. threaded-notification length budgets are
/// implementation-specific; using ~32k chars keeps the rendered list
/// short enough for every backend's truncation rules without being so
/// tight that it suppresses useful detail.
const THREAD_REPLY_DISPLAY_CAP: usize = 32_000;

/// Process one drained scout request. See module docs for the workspace
/// contract. Returns `Ok(())` on every path (including handled
/// validation failures); only IO-level errors propagate as `Err` so the
/// caller can log without aborting the surrounding iteration.
pub async fn process_pending_scout(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    _github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    request: &ScoutRequest,
    scout_cfg: &ScoutFeatureConfig,
) -> Result<()> {
    tracing::info!(
        url = %repo.url,
        request_id = %request.request_id,
        "scout: invoking executor"
    );

    // Gather workspace context for the prompt.
    let readme = read_workspace_readme(workspace);
    let docs_listing = build_docs_listing(workspace);
    let symbols_overview = build_symbols_overview(workspace);
    let git_log = build_git_log(workspace, scout_cfg.staleness_warn_days * 4);
    let (issues_listing, gh_failed) = if scout_cfg.include_issues {
        fetch_issues_listing(&repo.url)
    } else {
        ("(issue input disabled via features.scout.include_issues=false)".to_string(), false)
    };

    let template = resolve_scout_template(workspace, scout_cfg);
    let rendered_prompt = render_scout_prompt(
        &template,
        &ScoutPromptInputs {
            guidance: request.guidance.as_deref().unwrap_or("(none)"),
            repo_url: &repo.url,
            readme: &readme,
            docs_listing: &docs_listing,
            symbols_overview: &symbols_overview,
            git_log: &git_log,
            issues_listing: &issues_listing,
            max_items: scout_cfg.max_items,
        },
    );

    let ctx = ScoutContext { rendered_prompt };
    let outcome = executor.run_scout(workspace, &ctx).await;
    let response_text = match outcome {
        Ok(ExecutorOutcome::Completed { final_answer }) => {
            match final_answer {
                Some(s) => s,
                None => {
                    post_failure(
                        chatops_ctx,
                        request,
                        "executor returned Completed with no final_answer; cannot parse a list",
                    )
                    .await;
                    return Ok(());
                }
            }
        }
        Ok(ExecutorOutcome::Failed { reason }) => {
            post_failure(chatops_ctx, request, &format!("executor failed: {reason}"))
                .await;
            return Ok(());
        }
        Ok(ExecutorOutcome::AskUser { .. }) => {
            tracing::info!(
                request_id = %request.request_id,
                "scout: executor returned AskUser; no state persisted"
            );
            return Ok(());
        }
        Ok(ExecutorOutcome::SpecNeedsRevision { .. }) => {
            post_failure(
                chatops_ctx,
                request,
                "executor flagged SpecNeedsRevision during scout (unexpected)",
            )
            .await;
            return Ok(());
        }
        Err(e) => {
            post_failure(chatops_ctx, request, &format!("executor task error: {e:#}"))
                .await;
            return Ok(());
        }
    };

    // Parse the JSON list. The prompt instructs the LLM to emit the
    // bare array; defend against a markdown-code-fence wrap.
    let items = match parse_items(&response_text) {
        Ok(items) => items,
        Err(e) => {
            post_failure(
                chatops_ctx,
                request,
                &format!(
                    "could not parse scout response as a JSON array of items: {e}. \
                     See the daemon log (request_id={}) for the raw response.",
                    request.request_id
                ),
            )
            .await;
            return Ok(());
        }
    };

    if let Err(e) = validate_items(&items, scout_cfg.max_items) {
        post_failure(
            chatops_ctx,
            request,
            &format!("scout response failed validation: {e}"),
        )
        .await;
        return Ok(());
    }

    // Capture workspace HEAD AT scout completion for the spec-it staleness
    // check.
    let head_sha = git::rev_parse(workspace, "HEAD").unwrap_or_else(|_| "unknown".to_string());

    let state = ScoutRunState {
        request_id: request.request_id.clone(),
        repo_url: request.repo_url.clone(),
        guidance: request.guidance.clone(),
        head_sha_at_run: head_sha,
        completed_at: chrono::Utc::now(),
        channel: request.channel.clone(),
        thread_ts: request.thread_ts.clone(),
        items: items.clone(),
    };
    if let Err(e) = scout_run::write_state(workspace, &state) {
        tracing::warn!(
            request_id = %request.request_id,
            "scout: write_state failed: {e:#}"
        );
        post_failure(
            chatops_ctx,
            request,
            &format!("could not persist scout state file: {e}"),
        )
        .await;
        return Ok(());
    }

    // Render the list AND post the thread reply.
    let body = render_thread_reply(&items, gh_failed, &request.request_id);
    if let Some(ctx) = chatops_ctx
        && let Err(e) = ctx
            .chatops
            .post_threaded_reply(&request.channel, &request.thread_ts, &body)
            .await
    {
        tracing::warn!(
            request_id = %request.request_id,
            "scout: thread reply failed: {e:#}"
        );
    }
    Ok(())
}

/// Process a drained clear-scout request: remove every scout state file
/// under the workspace AND reply with the count cleared.
pub async fn process_clear_scout(
    workspace: &Path,
    chatops_ctx: Option<&ChatOpsContext>,
    request: &ClearScoutRequest,
) -> Result<()> {
    let removed = scout_run::clear_all(workspace).context("clear-scout: clear_all failed")?;
    let reply = format!(
        "✓ Cleared {removed} scout run(s) for {}.",
        request.repo_url
    );
    if let Some(ctx) = chatops_ctx
        && let Err(e) = ctx
            .chatops
            .post_threaded_reply(&request.channel, &request.thread_ts, &reply)
            .await
    {
        tracing::warn!("clear-scout: thread reply failed: {e:#}");
    }
    Ok(())
}

/// Resolve the scout prompt template via the uniform
/// [`crate::prompts::PromptLoader`].
fn resolve_scout_template(workspace: &Path, scout_cfg: &ScoutFeatureConfig) -> String {
    use crate::prompts::{PromptId, PromptLoader};
    let nested = scout_cfg.prompt_path.as_deref();
    PromptLoader::load(PromptId::Scout, nested, None, Some(workspace))
}

/// Inputs collected from the workspace + operator AND interpolated
/// into the scout prompt template's `{{…}}` placeholders.
struct ScoutPromptInputs<'a> {
    guidance: &'a str,
    repo_url: &'a str,
    readme: &'a str,
    docs_listing: &'a str,
    symbols_overview: &'a str,
    git_log: &'a str,
    issues_listing: &'a str,
    max_items: usize,
}

fn render_scout_prompt(template: &str, inputs: &ScoutPromptInputs<'_>) -> String {
    template
        .replace("{{guidance}}", inputs.guidance)
        .replace("{{repo_url}}", inputs.repo_url)
        .replace("{{readme}}", inputs.readme)
        .replace("{{docs_listing}}", inputs.docs_listing)
        .replace("{{symbols_overview}}", inputs.symbols_overview)
        .replace("{{git_log}}", inputs.git_log)
        .replace("{{issues_listing}}", inputs.issues_listing)
        .replace("{{max_items}}", &inputs.max_items.to_string())
}

fn read_workspace_readme(workspace: &Path) -> String {
    let path = workspace.join("README.md");
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => "(no README.md at workspace root)".to_string(),
    }
}

fn build_docs_listing(workspace: &Path) -> String {
    let docs_dir = workspace.join("docs");
    if !docs_dir.is_dir() {
        return "(no docs/ directory at workspace root)".to_string();
    }
    let mut names: Vec<String> = match std::fs::read_dir(&docs_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => return "(error reading docs/)".to_string(),
    };
    names.sort();
    if names.is_empty() {
        return "(docs/ contains no markdown files)".to_string();
    }
    names
        .iter()
        .map(|n| format!("- docs/{n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_symbols_overview(workspace: &Path) -> String {
    if workspace.join("Cargo.toml").is_file()
        && let Some(s) = cargo_metadata_overview(workspace)
        && !s.is_empty()
    {
        return s;
    }
    rg_public_items_overview(workspace)
        .unwrap_or_else(|| "(could not build code-symbol overview)".to_string())
}

fn cargo_metadata_overview(workspace: &Path) -> Option<String> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version=1"])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let pkgs = parsed.get("packages")?.as_array()?;
    let mut lines: Vec<String> = Vec::new();
    for p in pkgs {
        let pkg_name = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        lines.push(format!("- package `{pkg_name}`"));
        if let Some(targets) = p.get("targets").and_then(|v| v.as_array()) {
            for t in targets {
                let t_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let kinds: Vec<&str> = t
                    .get("kind")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                lines.push(format!(
                    "  - target `{t_name}` (kinds: {})",
                    kinds.join(", ")
                ));
            }
        }
    }
    Some(lines.join("\n"))
}

fn rg_public_items_overview(workspace: &Path) -> Option<String> {
    let out = std::process::Command::new("rg")
        .args([
            "--no-heading",
            "-n",
            "--max-count=200",
            r"^\s*(pub\s+(fn|struct|enum|trait|mod)|def\s+\w+|class\s+\w+|export\s+(function|class)|func\s+[A-Z]\w*)\b",
        ])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = raw.lines().take(150).collect();
    if lines.is_empty() {
        return Some("(ripgrep produced no public-item matches)".to_string());
    }
    lines.sort();
    Some(lines.join("\n"))
}

/// Produce the `git log --since="<N> days ago" --pretty=oneline`
/// output for the prompt. Failures fall through to a placeholder so the
/// scout invocation continues without recent-activity context.
fn build_git_log(workspace: &Path, days: u64) -> String {
    let since = format!("{days} days ago");
    let out = match std::process::Command::new("git")
        .args([
            "log",
            "--since",
            &since,
            "--pretty=oneline",
            "--no-decorate",
        ])
        .current_dir(workspace)
        .output()
    {
        Ok(o) => o,
        Err(_) => return "(git log unavailable)".to_string(),
    };
    if !out.status.success() {
        return "(git log unavailable)".to_string();
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = raw.lines().take(200).collect();
    if lines.is_empty() {
        return "(no commits in the recent-activity window)".to_string();
    }
    lines.join("\n")
}

/// Best-effort `gh api repos/<owner>/<repo>/issues?state=open --paginate`.
/// Returns `(listing, gh_failed)`. On any error, returns
/// `("(gh api unavailable: …)", true)` so the polling layer can note
/// the skip in the thread reply.
fn fetch_issues_listing(repo_url: &str) -> (String, bool) {
    let (owner, name) = match github::parse_repo_url(repo_url) {
        Ok(p) => p,
        Err(e) => return (format!("(could not parse repo URL: {e})"), true),
    };
    let endpoint = format!("repos/{owner}/{name}/issues?state=open");
    let out = match std::process::Command::new("gh")
        .args(["api", "--paginate", &endpoint])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(repo_url, "scout: gh api spawn failed: {e}");
            return (format!("(gh api unavailable: {e})"), true);
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(
            repo_url,
            stderr = %stderr,
            "scout: gh api returned non-success"
        );
        return (format!("(gh api unavailable: {})", stderr.lines().next().unwrap_or("")), true);
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(repo_url, "scout: gh api response not JSON: {e}");
            return (format!("(gh api response not JSON: {e})"), true);
        }
    };
    // The --paginate flag concatenates pages; sometimes that ships as a
    // single flat array, sometimes (legacy gh) as a series of arrays.
    // Handle both by extracting only the visible fields the scout
    // prompt needs (number/title/url).
    let mut lines: Vec<String> = Vec::new();
    fn push_from_array(arr: &serde_json::Value, out: &mut Vec<String>) {
        if let Some(arr) = arr.as_array() {
            for item in arr {
                // Skip pull requests (gh's /issues endpoint mixes them in).
                if item.get("pull_request").is_some() {
                    continue;
                }
                let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no title)");
                let url = item
                    .get("html_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                out.push(format!("- #{number} {title} ({url})"));
            }
        }
    }
    push_from_array(&parsed, &mut lines);
    if lines.is_empty() {
        return ("(no open issues)".to_string(), false);
    }
    // Cap the listing to keep the prompt bounded.
    let cap = 100;
    if lines.len() > cap {
        let extra = lines.len() - cap;
        lines.truncate(cap);
        lines.push(format!("… +{extra} more (truncated)"));
    }
    (lines.join("\n"), false)
}

/// Parse the executor's response text as a JSON array of `ScoutItem`s.
/// Tolerates a leading/trailing markdown code fence; the prompt asks for
/// the bare array, but LLMs sometimes still wrap.
fn parse_items(response: &str) -> Result<Vec<ScoutItem>> {
    let stripped = strip_code_fence(response.trim());
    let items: Vec<ScoutItem> = serde_json::from_str(stripped.trim())
        .context("response is not a valid JSON array of scout items")?;
    Ok(items)
}

fn strip_code_fence(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        return rest.trim_end_matches("```").trim();
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        return rest.trim_end_matches("```").trim();
    }
    trimmed
}

/// Render the grouped-by-category thread reply. Within each category,
/// items are rendered in id order. The full body is truncated to
/// `THREAD_REPLY_DISPLAY_CAP` chars with a pointer to the on-disk state
/// file when over the limit; the state file always contains every item.
fn render_thread_reply(items: &[ScoutItem], gh_failed: bool, request_id: &str) -> String {
    use std::collections::BTreeMap;
    let mut by_cat: BTreeMap<&str, Vec<&ScoutItem>> = BTreeMap::new();
    for item in items {
        by_cat.entry(item.category.as_str()).or_default().push(item);
    }
    let mut body = String::new();
    body.push_str("📋 Scout report — items grouped by category:\n");
    let mut truncated = false;
    'outer: for (cat, items) in &by_cat {
        let header = format!("\n**{cat}**\n");
        if body.len() + header.len() > THREAD_REPLY_DISPLAY_CAP {
            truncated = true;
            break 'outer;
        }
        body.push_str(&header);
        for item in items {
            let line = format!(
                "**{id}. [{cat}] {title}** — {body_first_sentence} _(source: {source}; tractability: {tractability})_\n",
                id = item.id,
                title = item.title,
                body_first_sentence = first_sentence(&item.body),
                source = item.source,
                tractability = item.tractability,
            );
            if body.len() + line.len() > THREAD_REPLY_DISPLAY_CAP {
                truncated = true;
                break 'outer;
            }
            body.push_str(&line);
        }
    }
    if truncated {
        body.push_str(&format!(
            "\n… (truncated; full list in <workspace>/.state/scout_runs/{request_id}.json)\n"
        ));
    }
    body.push_str(
        "\nReply with @<bot> spec-it <N> [optional guidance] to scope work on any item.",
    );
    if gh_failed {
        body.push_str(
            "\n_(Note: issue-derived items were skipped this run — `gh api` was unavailable.)_",
        );
    }
    body
}

fn first_sentence(s: &str) -> String {
    let trimmed = s.trim();
    if let Some(idx) = trimmed.find(['.', '!', '?']) {
        let end = idx + 1;
        if end < trimmed.len() {
            return trimmed[..end].to_string();
        }
    }
    trimmed.to_string()
}

async fn post_failure(
    chatops_ctx: Option<&ChatOpsContext>,
    request: &ScoutRequest,
    body: &str,
) {
    let msg = format!("✗ scout: {body}");
    tracing::warn!(
        request_id = %request.request_id,
        "scout failure: {body}"
    );
    if let Some(ctx) = chatops_ctx
        && let Err(e) = ctx
            .chatops
            .post_threaded_reply(&request.channel, &request.thread_ts, &msg)
            .await
    {
        tracing::warn!(
            request_id = %request.request_id,
            "scout: post_failure thread reply failed: {e:#}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: usize, cat: &str, title: &str) -> ScoutItem {
        ScoutItem {
            id,
            category: cat.to_string(),
            title: title.to_string(),
            body: format!("body for item {id}. extra sentence."),
            source: format!("src/x.rs:{id}"),
            tractability: "small".to_string(),
        }
    }

    #[test]
    fn parse_items_accepts_plain_array() {
        let json = r#"[{"id":1,"category":"bug","title":"t","body":"b","source":"src/x.rs:1","tractability":"small"}]"#;
        let items = parse_items(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "t");
    }

    #[test]
    fn parse_items_accepts_markdown_fenced_array() {
        let json = "```json\n[{\"id\":1,\"category\":\"bug\",\"title\":\"t\",\"body\":\"b\",\"source\":\"src/x.rs:1\",\"tractability\":\"small\"}]\n```";
        let items = parse_items(json).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn parse_items_rejects_invalid_json() {
        let err = parse_items("not json at all").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("valid JSON"), "got: {msg}");
    }

    #[test]
    fn first_sentence_picks_up_to_period() {
        assert_eq!(first_sentence("Hello. world"), "Hello.");
        assert_eq!(first_sentence("no terminator here"), "no terminator here");
    }

    #[test]
    fn render_thread_reply_groups_by_category_and_appends_closing_note() {
        let items = vec![
            item(1, "bug", "first"),
            item(2, "bug", "second"),
            item(3, "security", "third"),
        ];
        let body = render_thread_reply(&items, false, "req-x");
        assert!(body.contains("**bug**"));
        assert!(body.contains("**security**"));
        assert!(body.contains("spec-it <N>"));
        assert!(!body.contains("(truncated"));
    }

    #[test]
    fn render_thread_reply_notes_gh_failure() {
        let items = vec![item(1, "bug", "t")];
        let body = render_thread_reply(&items, true, "req-x");
        assert!(body.contains("issue-derived items were skipped"));
    }

    #[test]
    fn render_thread_reply_truncates_oversize_list() {
        let mut items: Vec<ScoutItem> = Vec::new();
        for i in 1..=600 {
            let mut it = item(i, "bug", "title");
            it.body = "x".repeat(200);
            items.push(it);
        }
        let body = render_thread_reply(&items, false, "req-overflow");
        assert!(body.contains("(truncated; full list in"));
        assert!(body.contains("req-overflow.json"));
    }
}
