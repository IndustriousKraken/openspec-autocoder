//! Scout-mode polling handler (a25). One pass per iteration: drain ONE
//! pending scout request from the per-repo queue, render the scout
//! prompt template, invoke the executor in scout mode (read-only
//! sandbox), parse the JSON array of opportunity items, AND persist a
//! `ScoutRunState` file.
//!
//! Failure modes:
//!   - Invalid JSON / validation failure → no state file written; the
//!     thread reply names the failure.
//!   - `gh api` failure (when `features.scout.include_issues` is true)
//!     → WARN logged, scout proceeds with code-derived items only AND
//!     the thread reply notes the skip.

use crate::config::RepositoryConfig;
use crate::executor::{Executor, ExecutorOutcome, ScoutContext};
use crate::polling_loop::ChatOpsContext;
use crate::prompts::{PromptId, PromptLoader};
use crate::state::scout_run::{
    self, ALLOWED_CATEGORIES, ALLOWED_TRACTABILITY, ScoutItem, ScoutRunState,
};
use anyhow::Result;
use std::path::Path;

/// Cap on the rendered-list length we post into the thread reply. Past
/// this, the rendered list truncates with a pointer at the on-disk
/// state file. The threshold is conservative — chatops backends each
/// have their own limit (Slack ~40k chars for a message text), but
/// truncating earlier keeps the reply readable.
const RENDERED_LIST_LIMIT: usize = 7000;

/// Process the one drained scout request. Returns `Ok(())` on every
/// path (including validation-failure); irrecoverable errors propagate
/// as `Err` for the caller to log.
pub async fn process_pending_scout(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsContext>,
    request: &crate::control_socket::ScoutRequest,
) -> Result<()> {
    let scout_cfg = load_scout_cfg();

    // Resolve current workspace HEAD for staleness comparison later.
    let head_sha = crate::git::rev_parse(workspace, "HEAD")
        .map(|s| s.chars().take(12).collect::<String>())
        .unwrap_or_else(|_| "unknown".to_string());

    // Build prompt inputs.
    let prompt_template = PromptLoader::load(
        PromptId::Scout,
        scout_cfg.prompt_path.as_deref(),
        None,
        Some(workspace),
    );
    let readme = read_workspace_readme(workspace);
    let docs_listing = build_docs_listing(workspace);
    let symbols_overview = build_symbols_overview(workspace);
    let recent_activity = read_recent_activity(workspace, scout_cfg.staleness_warn_days * 4);
    let (open_issues, issues_failed) = if scout_cfg.include_issues {
        fetch_open_issues(&request.repo_url)
    } else {
        ("(open-issues fetch disabled by features.scout.include_issues=false)".to_string(), false)
    };
    let guidance_text = request
        .guidance
        .as_deref()
        .unwrap_or("(no operator guidance)")
        .to_string();

    let rendered_prompt = render_scout_prompt(
        &prompt_template,
        scout_cfg.max_items,
        &guidance_text,
        &request.repo_url,
        &head_sha,
        &readme,
        &docs_listing,
        &symbols_overview,
        &recent_activity,
        &open_issues,
    );

    tracing::info!(
        url = %repo.url,
        request_id = %request.request_id,
        "scout: invoking executor in scout mode"
    );

    let outcome = executor
        .run_scout(workspace, &ScoutContext { rendered_prompt })
        .await;

    let final_answer = match outcome {
        Ok(ExecutorOutcome::Completed { final_answer: Some(text) }) => text,
        Ok(ExecutorOutcome::Completed { final_answer: None }) => {
            post_thread_failure(
                chatops_ctx,
                request,
                "scout: executor returned no final answer (legacy text mode? streaming-JSON mode is required)",
            )
            .await;
            return Ok(());
        }
        Ok(ExecutorOutcome::Failed { reason }) => {
            post_thread_failure(
                chatops_ctx,
                request,
                &format!("scout: executor returned Failed: {reason}"),
            )
            .await;
            return Ok(());
        }
        Ok(other) => {
            post_thread_failure(
                chatops_ctx,
                request,
                &format!("scout: executor returned an unexpected outcome ({other:?})"),
            )
            .await;
            return Ok(());
        }
        Err(e) => {
            post_thread_failure(
                chatops_ctx,
                request,
                &format!("scout: executor task error: {e:#}"),
            )
            .await;
            return Ok(());
        }
    };

    // Parse the JSON array.
    let items = match parse_and_validate_items(&final_answer, scout_cfg.max_items) {
        Ok(items) => items,
        Err(e) => {
            post_thread_failure(
                chatops_ctx,
                request,
                &format!("scout: invalid response: {e}. See the daemon log for full context."),
            )
            .await;
            return Ok(());
        }
    };

    // Persist the state file BEFORE posting the thread reply, so the
    // spec-it handler always sees a state file matching the rendered
    // list.
    let state = ScoutRunState {
        request_id: request.request_id.clone(),
        repo_url: request.repo_url.clone(),
        guidance: request.guidance.clone(),
        head_sha_at_run: head_sha.clone(),
        completed_at: chrono::Utc::now(),
        thread_ts: request.thread_ts.clone(),
        channel: request.channel.clone(),
        items: items.clone(),
    };
    if let Err(e) = scout_run::write_state(workspace, &state) {
        tracing::warn!(
            request_id = %request.request_id,
            "scout: write_state failed: {e:#}"
        );
        post_thread_failure(
            chatops_ctx,
            request,
            &format!("scout: could not persist state file: {e}"),
        )
        .await;
        return Ok(());
    }

    // Build the rendered list.
    let mut body = render_items(&items);
    if body.chars().count() > RENDERED_LIST_LIMIT {
        body = render_items_truncated(&items, &request.request_id);
    }
    body.push_str(
        "\n\nReply with `@<bot> spec-it <N> [optional guidance]` to scope work on any item.",
    );
    if issues_failed {
        body.push_str(
            "\n\n_Note: open-issues fetch via `gh api` failed; issue-derived items were skipped this run._",
        );
    }
    post_thread(chatops_ctx, request, &body).await;
    Ok(())
}

/// In-memory snapshot of the scout config used by the polling handler.
/// Currently sourced from `ScoutFeatureConfig::default()` since the
/// polling layer does not yet have the per-workspace config holder
/// threaded through; the canonical `features.scout` resolution lives
/// in `Config` AND is enforced at config-load time.
struct ScoutCfg {
    prompt_path: Option<std::path::PathBuf>,
    max_items: usize,
    include_issues: bool,
    staleness_warn_days: u64,
}

fn load_scout_cfg() -> ScoutCfg {
    let defaults = crate::config::ScoutFeatureConfig::default();
    ScoutCfg {
        prompt_path: defaults.prompt_path,
        max_items: defaults.max_items,
        include_issues: defaults.include_issues,
        staleness_warn_days: defaults.staleness_warn_days,
    }
}

/// Substitute the scout context fields into the template's `{{...}}`
/// placeholders. Missing placeholders are simply left out.
#[allow(clippy::too_many_arguments)]
fn render_scout_prompt(
    template: &str,
    max_items: usize,
    guidance: &str,
    repo_url: &str,
    head_sha: &str,
    readme: &str,
    docs_listing: &str,
    symbols_overview: &str,
    recent_activity: &str,
    open_issues: &str,
) -> String {
    // Single-pass substitution (a002): an injected README / docs listing /
    // symbols overview / operator-guidance value that itself contains a
    // `{{...}}` token is emitted verbatim, never re-expanded by a later
    // pass.
    crate::prompts::render_template(
        template,
        &[
            ("max_items", &max_items.to_string()),
            ("guidance", guidance),
            ("repo_url", repo_url),
            ("head_sha", head_sha),
            ("readme", readme),
            ("docs_listing", docs_listing),
            ("symbols_overview", symbols_overview),
            ("recent_activity", recent_activity),
            ("open_issues", open_issues),
        ],
    )
}

/// Validate the executor's JSON response. Returns the parsed item list
/// on success; an error message naming the first invariant violation
/// on failure.
pub fn parse_and_validate_items(raw: &str, max_items: usize) -> std::result::Result<Vec<ScoutItem>, String> {
    let trimmed = extract_json_array(raw);
    let items: Vec<ScoutItem> = serde_json::from_str(trimmed)
        .map_err(|e| format!("not a valid JSON array of scout items: {e}"))?;
    if items.len() > max_items {
        return Err(format!(
            "executor returned {} items but max_items is {max_items}",
            items.len()
        ));
    }
    for (idx, item) in items.iter().enumerate() {
        if item.title.trim().is_empty() {
            return Err(format!("item[{idx}].title is empty"));
        }
        if item.body.trim().is_empty() {
            return Err(format!("item[{idx}].body is empty"));
        }
        if item.source.trim().is_empty() {
            return Err(format!("item[{idx}].source is empty"));
        }
        if !ALLOWED_CATEGORIES.contains(&item.category.as_str()) {
            return Err(format!(
                "item[{idx}].category `{}` is not in the allowed set",
                item.category
            ));
        }
        if !ALLOWED_TRACTABILITY.contains(&item.tractability.as_str()) {
            return Err(format!(
                "item[{idx}].tractability `{}` is not in the allowed set",
                item.tractability
            ));
        }
    }
    Ok(items)
}

/// Best-effort: pull the first `[ ... ]` JSON array out of the
/// executor's reply. The scout prompt asks for "JSON array AND nothing
/// else" but some backends wrap the response in markdown fences; this
/// helper tolerates the wrapping rather than failing the whole run.
fn extract_json_array(raw: &str) -> &str {
    let start = match raw.find('[') {
        Some(i) => i,
        None => return raw,
    };
    let end = match raw.rfind(']') {
        Some(i) => i,
        None => return raw,
    };
    if end < start {
        return raw;
    }
    &raw[start..=end]
}

/// Render the item list grouped by category, compact one-line-per-item
/// shape. The first sentence of each item's body is used; the body's
/// remainder is preserved in the state file.
pub fn render_items(items: &[ScoutItem]) -> String {
    if items.is_empty() {
        return "_(scout returned no items)_".to_string();
    }
    use std::collections::BTreeMap;
    let mut by_category: BTreeMap<&str, Vec<&ScoutItem>> = BTreeMap::new();
    for item in items {
        by_category
            .entry(item.category.as_str())
            .or_default()
            .push(item);
    }
    let mut out = String::new();
    for (category, list) in by_category {
        out.push_str(&format!("\n*{category}*\n"));
        for item in list {
            let first_sentence = first_sentence(&item.body);
            out.push_str(&format!(
                "  *{}.* [{}] {} — {} _(source: {}; tractability: {})_\n",
                item.id,
                item.category,
                item.title,
                first_sentence,
                item.source,
                item.tractability,
            ));
        }
    }
    out
}

/// Render the list, truncating once the rendered length passes the
/// chat-backend limit. Appends a pointer to the on-disk state file.
fn render_items_truncated(items: &[ScoutItem], request_id: &str) -> String {
    let full = render_items(items);
    let mut chars = full.chars();
    let mut out: String = (&mut chars).take(RENDERED_LIST_LIMIT).collect();
    out.push_str(&format!(
        "\n… (truncated; full list in <workspace>/.state/scout_runs/{request_id}.json)"
    ));
    out
}

fn first_sentence(body: &str) -> String {
    let trimmed = body.trim();
    if let Some(idx) = trimmed.find(['.', '!', '?']) {
        let end = idx + 1;
        trimmed.chars().take(end).collect()
    } else {
        trimmed.to_string()
    }
}

/// Post `body` as a threaded reply on the scout's lifecycle thread.
/// Best-effort; failures are logged but the surrounding handler keeps
/// going so the state file persists.
async fn post_thread(
    chatops_ctx: Option<&ChatOpsContext>,
    request: &crate::control_socket::ScoutRequest,
    body: &str,
) {
    let Some(ctx) = chatops_ctx else { return };
    if let Err(e) = ctx
        .chatops
        .post_threaded_reply(&request.channel, &request.thread_ts, body)
        .await
    {
        tracing::warn!(
            request_id = %request.request_id,
            "scout: thread reply failed: {e:#}"
        );
    }
}

async fn post_thread_failure(
    chatops_ctx: Option<&ChatOpsContext>,
    request: &crate::control_socket::ScoutRequest,
    body: &str,
) {
    post_thread(chatops_ctx, request, &format!("✗ {body}")).await;
}

fn read_workspace_readme(workspace: &Path) -> String {
    let path = workspace.join("README.md");
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            // Cap at 6000 chars so the prompt stays bounded; the
            // executor can read the file directly under its sandbox if
            // it needs the full contents.
            let mut out: String = s.chars().take(6000).collect();
            if s.chars().count() > 6000 {
                out.push_str("\n… (README truncated; full file at README.md)");
            }
            out
        }
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
    if workspace.join("Cargo.toml").is_file() {
        match cargo_metadata_overview(workspace) {
            Some(s) if !s.is_empty() => return s,
            _ => {}
        }
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

fn read_recent_activity(workspace: &Path, since_days: u64) -> String {
    let since = format!("{since_days} days ago");
    let out = std::process::Command::new("git")
        .args(["log", "--pretty=oneline", "--since", &since])
        .current_dir(workspace)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).into_owned();
            if s.trim().is_empty() {
                "(no commits in the recent-activity window)".to_string()
            } else {
                // Cap.
                let mut capped: String = s.chars().take(4000).collect();
                if s.chars().count() > 4000 {
                    capped.push_str("\n… (truncated)");
                }
                capped
            }
        }
        _ => "(could not read git log)".to_string(),
    }
}

/// Fetch open issues via `gh api`. Returns `(text, failed)` — when
/// `failed` is true, the caller posts a note saying issue-derived
/// items were skipped.
fn fetch_open_issues(repo_url: &str) -> (String, bool) {
    let parts = match crate::github::parse_repo_url(repo_url) {
        Ok((o, r)) => (o, r),
        Err(_) => {
            return (
                "(could not parse repo URL for `gh api` call)".to_string(),
                true,
            );
        }
    };
    let endpoint = format!("repos/{}/{}/issues?state=open", parts.0, parts.1);
    let out = std::process::Command::new("gh")
        .args(["api", "--paginate", &endpoint])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).into_owned();
            // Cap.
            let mut capped: String = s.chars().take(8000).collect();
            if s.chars().count() > 8000 {
                capped.push_str("\n… (truncated; the executor can re-run `gh api` if needed)");
            }
            (capped, false)
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            tracing::warn!(
                "scout: `gh api` open-issues fetch failed: status={}, stderr={err}",
                o.status
            );
            (
                "(open-issues fetch via `gh api` failed; running with code-derived items only)"
                    .to_string(),
                true,
            )
        }
        Err(e) => {
            tracing::warn!("scout: `gh api` command failed to spawn: {e}");
            (
                "(open-issues fetch via `gh api` could not run)".to_string(),
                true,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: usize, cat: &str, tract: &str) -> ScoutItem {
        ScoutItem {
            id,
            category: cat.into(),
            title: format!("Title {id}"),
            body: "Body sentence one. Body sentence two.".into(),
            source: "src/lib.rs:10".into(),
            tractability: tract.into(),
        }
    }

    #[test]
    fn parse_valid_json_array_round_trips() {
        let json = r#"[
            {"id":1,"category":"bug","title":"x","body":"y","source":"src/a.rs:1","tractability":"small"}
        ]"#;
        let items = parse_and_validate_items(json, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 1);
        assert_eq!(items[0].category, "bug");
    }

    #[test]
    fn parse_rejects_invalid_category() {
        let json = r#"[
            {"id":1,"category":"banana","title":"x","body":"y","source":"s:1","tractability":"small"}
        ]"#;
        let err = parse_and_validate_items(json, 10).unwrap_err();
        assert!(err.contains("category"), "{err}");
        assert!(err.contains("banana"), "{err}");
    }

    #[test]
    fn parse_rejects_invalid_tractability() {
        let json = r#"[
            {"id":1,"category":"bug","title":"x","body":"y","source":"s:1","tractability":"huge"}
        ]"#;
        let err = parse_and_validate_items(json, 10).unwrap_err();
        assert!(err.contains("tractability"), "{err}");
    }

    #[test]
    fn parse_rejects_over_cap() {
        let mut s = String::from("[");
        for i in 1..=5 {
            if i > 1 {
                s.push(',');
            }
            s.push_str(&format!(
                r#"{{"id":{i},"category":"bug","title":"x","body":"y","source":"s:1","tractability":"small"}}"#
            ));
        }
        s.push(']');
        let err = parse_and_validate_items(&s, 3).unwrap_err();
        assert!(err.contains("max_items"), "{err}");
    }

    #[test]
    fn parse_rejects_missing_body() {
        let json = r#"[
            {"id":1,"category":"bug","title":"x","body":"","source":"s:1","tractability":"small"}
        ]"#;
        let err = parse_and_validate_items(json, 10).unwrap_err();
        assert!(err.contains("body"), "{err}");
    }

    #[test]
    fn parse_tolerates_fenced_response() {
        let fenced = "```json\n[\n  {\"id\":1,\"category\":\"bug\",\"title\":\"x\",\"body\":\"y\",\"source\":\"s:1\",\"tractability\":\"small\"}\n]\n```";
        let items = parse_and_validate_items(fenced, 10).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn render_groups_items_by_category() {
        let items = vec![
            item(1, "bug", "small"),
            item(2, "security", "medium"),
            item(3, "bug", "small"),
        ];
        let rendered = render_items(&items);
        assert!(rendered.contains("*bug*"), "{rendered}");
        assert!(rendered.contains("*security*"), "{rendered}");
        // Two bug items both present.
        assert!(rendered.contains("*1.*"));
        assert!(rendered.contains("*3.*"));
        assert!(rendered.contains("Title 1"));
        assert!(rendered.contains("Title 2"));
    }

    #[test]
    fn render_first_sentence_strips_remainder() {
        let items = vec![item(1, "bug", "small")];
        let rendered = render_items(&items);
        assert!(rendered.contains("Body sentence one."), "{rendered}");
        // The second sentence does NOT appear in the rendered list.
        assert!(!rendered.contains("Body sentence two"), "{rendered}");
    }

    #[test]
    fn first_sentence_returns_full_string_when_no_terminator() {
        assert_eq!(first_sentence("hello world"), "hello world".to_string());
        assert_eq!(first_sentence("hello. world."), "hello.".to_string());
    }

    #[test]
    fn render_scout_prompt_substitutes_each_placeholder() {
        let template =
            "max={{max_items}} g={{guidance}} repo={{repo_url}} head={{head_sha}} readme={{readme}} docs={{docs_listing}} sym={{symbols_overview}} act={{recent_activity}} iss={{open_issues}}";
        let got = render_scout_prompt(
            template, 25, "focus on errors", "git@github.com:a/b.git", "abc123", "RM", "D", "S",
            "ACT", "ISS",
        );
        assert!(got.contains("max=25"), "{got}");
        assert!(got.contains("g=focus on errors"), "{got}");
        assert!(got.contains("repo=git@github.com:a/b.git"), "{got}");
        assert!(got.contains("head=abc123"), "{got}");
        assert!(got.contains("readme=RM"), "{got}");
        assert!(got.contains("docs=D"), "{got}");
        assert!(got.contains("sym=S"), "{got}");
        assert!(got.contains("act=ACT"), "{got}");
        assert!(got.contains("iss=ISS"), "{got}");
    }

    #[test]
    fn extract_json_array_handles_fenced_response() {
        let raw = "```json\n[1,2,3]\n```";
        assert_eq!(extract_json_array(raw), "[1,2,3]");
    }

    #[test]
    fn extract_json_array_returns_raw_when_no_brackets() {
        assert_eq!(extract_json_array("nope"), "nope");
    }
}
