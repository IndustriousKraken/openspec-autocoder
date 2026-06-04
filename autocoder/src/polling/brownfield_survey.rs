//! Brownfield-survey polling handler (a29). One pass per iteration:
//! drain ONE pending brownfield-survey request from the per-repo queue,
//! render the survey prompt template, invoke the executor in survey
//! mode (read-only sandbox; reuses `Executor::run_scout` since the
//! capability needs are identical — read-only sandbox + JSON-text
//! response), parse the JSON array of proposed-capability items, AND
//! persist a `BrownfieldSurveyState` file.
//!
//! Failure modes:
//!   - Invalid JSON / validation failure → no state file written; the
//!     thread reply names the failure.
//!   - Executor error → no state file written; thread reply names the
//!     failure.

use crate::config::RepositoryConfig;
use crate::executor::{Executor, ExecutorOutcome, ScoutContext};
use crate::polling_loop::ChatOpsContext;
use crate::prompts::{PromptId, PromptLoader};
use crate::spec_root::SpecRoot;
use crate::state::brownfield_survey::{
    self, BrownfieldSurveyState, ComplexityBand, ItemStatus, SurveyItem, SurveyStatus,
};
use anyhow::Result;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// Cap on the rendered-list length we post into the thread reply.
const RENDERED_LIST_LIMIT: usize = 7000;

fn slug_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[a-z][a-z0-9-]*$").unwrap())
}

/// Process the one drained brownfield-survey request. Returns `Ok(())`
/// on every path (including validation-failure); irrecoverable errors
/// propagate as `Err` for the caller to log.
pub async fn process_pending_brownfield_survey(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    chatops_ctx: Option<&ChatOpsContext>,
    request: &crate::control_socket::BrownfieldSurveyRequest,
) -> Result<()> {
    let survey_cfg = load_survey_cfg();

    let head_sha = crate::git::rev_parse(workspace, "HEAD")
        .map(|s| s.chars().take(12).collect::<String>())
        .unwrap_or_else(|_| "unknown".to_string());

    // Already-specced capabilities live under `<spec-root>/specs/`.
    let spec_root = SpecRoot::for_repo(repo, workspace);
    let already_specced = list_already_specced(&spec_root.canonical_specs_dir());
    let already_specced_text = if already_specced.is_empty() {
        "(none — this is a greenfield-from-OpenSpec project)".to_string()
    } else {
        already_specced
            .iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let prompt_template = PromptLoader::load(
        PromptId::BrownfieldSurvey,
        survey_cfg.prompt_path.as_deref(),
        None,
        Some(workspace),
    );
    let readme = read_workspace_readme(workspace);
    let docs_listing = build_docs_listing(workspace);
    let symbols_overview = build_symbols_overview(workspace);
    let guidance_text = request
        .guidance
        .as_deref()
        .unwrap_or("(no operator guidance)")
        .to_string();

    let rendered_prompt = render_survey_prompt(
        &prompt_template,
        survey_cfg.max_capabilities,
        &guidance_text,
        &request.repo_url,
        &readme,
        &docs_listing,
        &symbols_overview,
        &already_specced_text,
    );

    tracing::info!(
        url = %repo.url,
        request_id = %request.request_id,
        "brownfield-survey: invoking executor in survey mode"
    );

    // Survey mode reuses `run_scout`: same operational shape (read-only
    // sandbox, expect a JSON-text final answer). The prompt governs
    // what the LLM actually does.
    let outcome = executor
        .run_scout(workspace, &ScoutContext { rendered_prompt })
        .await;

    let final_answer = match outcome {
        Ok(ExecutorOutcome::Completed {
            final_answer: Some(text),
        }) => text,
        Ok(ExecutorOutcome::Completed { final_answer: None }) => {
            post_thread_failure(
                chatops_ctx,
                request,
                "brownfield-survey: executor returned no final answer",
            )
            .await;
            return Ok(());
        }
        Ok(ExecutorOutcome::Failed { reason }) => {
            post_thread_failure(
                chatops_ctx,
                request,
                &format!("brownfield-survey: executor returned Failed: {reason}"),
            )
            .await;
            return Ok(());
        }
        Ok(other) => {
            post_thread_failure(
                chatops_ctx,
                request,
                &format!(
                    "brownfield-survey: executor returned an unexpected outcome ({other:?})"
                ),
            )
            .await;
            return Ok(());
        }
        Err(e) => {
            post_thread_failure(
                chatops_ctx,
                request,
                &format!("brownfield-survey: executor task error: {e:#}"),
            )
            .await;
            return Ok(());
        }
    };

    let items = match parse_and_validate_items(
        &final_answer,
        survey_cfg.max_capabilities,
        &already_specced,
    ) {
        Ok(items) => items,
        Err(e) => {
            post_thread_failure(
                chatops_ctx,
                request,
                &format!(
                    "brownfield-survey: invalid response: {e}. See the daemon log for full context."
                ),
            )
            .await;
            return Ok(());
        }
    };

    let state = BrownfieldSurveyState {
        request_id: request.request_id.clone(),
        repo_url: request.repo_url.clone(),
        guidance: request.guidance.clone(),
        head_sha_at_survey: head_sha.clone(),
        completed_at: chrono::Utc::now(),
        thread_ts: request.thread_ts.clone(),
        channel: request.channel.clone(),
        items: items.clone(),
        status: SurveyStatus::Pending,
    };
    if let Err(e) = brownfield_survey::write_state(workspace, &state) {
        tracing::warn!(
            request_id = %request.request_id,
            "brownfield-survey: write_state failed: {e:#}"
        );
        post_thread_failure(
            chatops_ctx,
            request,
            &format!("brownfield-survey: could not persist state file: {e}"),
        )
        .await;
        return Ok(());
    }

    let mut body = render_items(&request.repo_url, &items);
    if body.chars().count() > RENDERED_LIST_LIMIT {
        body = render_items_truncated(&request.repo_url, &items, &request.request_id);
    }
    body.push_str(&format!(
        "\n\nReply with `@<bot> send it` to batch-generate ALL {n} specs (one per iteration).",
        n = items.len()
    ));
    body.push_str(
        "\nOr re-run `@<bot> brownfield-survey <repo> <refined guidance>` to refresh.",
    );
    post_thread(chatops_ctx, request, &body).await;
    Ok(())
}

/// In-memory snapshot of the brownfield-survey config used by the
/// polling handler.
struct SurveyCfg {
    prompt_path: Option<std::path::PathBuf>,
    max_capabilities: usize,
}

fn load_survey_cfg() -> SurveyCfg {
    let defaults = crate::config::BrownfieldSurveyFeatureConfig::default();
    SurveyCfg {
        prompt_path: defaults.prompt_path,
        max_capabilities: defaults.max_capabilities,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_survey_prompt(
    template: &str,
    max_capabilities: usize,
    guidance: &str,
    repo_url: &str,
    readme: &str,
    docs_listing: &str,
    symbols_overview: &str,
    already_specced: &str,
) -> String {
    // Single-pass substitution (a002): an injected README / docs listing /
    // symbols overview / operator-guidance / already-specced value that
    // itself contains a `{{...}}` token is emitted verbatim, never
    // re-expanded.
    crate::prompts::render_template(
        template,
        &[
            ("max_capabilities", &max_capabilities.to_string()),
            ("guidance", guidance),
            ("repo_url", repo_url),
            ("readme", readme),
            ("docs_listing", docs_listing),
            ("symbols_overview", symbols_overview),
            ("already_specced", already_specced),
        ],
    )
}

/// Validate the executor's JSON response.
pub fn parse_and_validate_items(
    raw: &str,
    max_capabilities: usize,
    already_specced: &[String],
) -> std::result::Result<Vec<SurveyItem>, String> {
    let trimmed = extract_json_array(raw);
    let raw_items: Vec<serde_json::Value> = serde_json::from_str(trimmed)
        .map_err(|e| format!("not a valid JSON array of survey items: {e}"))?;
    if raw_items.len() > max_capabilities {
        return Err(format!(
            "executor returned {} items but max_capabilities is {max_capabilities}",
            raw_items.len()
        ));
    }
    let already: std::collections::HashSet<&str> =
        already_specced.iter().map(|s| s.as_str()).collect();
    let mut items: Vec<SurveyItem> = Vec::with_capacity(raw_items.len());
    for (idx, raw) in raw_items.iter().enumerate() {
        let id = raw
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("item[{idx}].id missing or not a positive integer"))?
            as usize;
        let slug = raw
            .get("slug")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("item[{idx}].slug missing or not a string"))?
            .to_string();
        if !slug_re().is_match(&slug) {
            return Err(format!(
                "item[{idx}].slug `{slug}` does not match `^[a-z][a-z0-9-]*$`"
            ));
        }
        if already.contains(slug.as_str()) {
            return Err(format!(
                "item[{idx}].slug `{slug}` collides with an already-specced capability"
            ));
        }
        let summary = required_string_field(raw, "summary", idx)?;
        let scope_in = required_string_field(raw, "scope_in", idx)?;
        let scope_out = required_string_field(raw, "scope_out", idx)?;
        let source_modules: Vec<String> = raw
            .get("source_modules")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                format!("item[{idx}].source_modules missing or not a JSON array")
            })?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if source_modules.is_empty() {
            return Err(format!("item[{idx}].source_modules must be non-empty"));
        }
        let complexity_raw = raw
            .get("estimated_complexity")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("item[{idx}].estimated_complexity missing"))?;
        let estimated_complexity = ComplexityBand::parse(complexity_raw)
            .map_err(|e| format!("item[{idx}]: {e}"))?;
        items.push(SurveyItem {
            id,
            slug,
            summary,
            scope_in,
            scope_out,
            source_modules,
            estimated_complexity,
            status: ItemStatus::Pending,
            pr_url: None,
            failure_reason: None,
        });
    }
    Ok(items)
}

fn required_string_field(
    raw: &serde_json::Value,
    field: &str,
    idx: usize,
) -> std::result::Result<String, String> {
    let val = raw
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("item[{idx}].{field} missing or not a string"))?
        .to_string();
    if val.trim().is_empty() {
        return Err(format!("item[{idx}].{field} is empty"));
    }
    Ok(val)
}

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

/// Render the survey items as a numbered list grouped in the order
/// returned.
pub fn render_items(repo_url: &str, items: &[SurveyItem]) -> String {
    let mut out = format!("📋 Surveyed capabilities for {repo_url}:\n");
    if items.is_empty() {
        out.push_str("\n_(survey returned no items)_");
        return out;
    }
    for item in items {
        out.push_str(&format!(
            "\n{}. `{}` — {} — {}\n   Scope-in:  {}\n   Scope-out: {}\n   Source:    {}\n",
            item.id,
            item.slug,
            item.estimated_complexity.label(),
            item.summary,
            item.scope_in,
            item.scope_out,
            item.source_modules.join(", "),
        ));
    }
    out
}

fn render_items_truncated(repo_url: &str, items: &[SurveyItem], request_id: &str) -> String {
    let full = render_items(repo_url, items);
    let mut chars = full.chars();
    let mut out: String = (&mut chars).take(RENDERED_LIST_LIMIT).collect();
    out.push_str(&format!(
        "\n… (truncated; full list in <workspace>/.state/brownfield_surveys/{request_id}.json)"
    ));
    out
}

async fn post_thread(
    chatops_ctx: Option<&ChatOpsContext>,
    request: &crate::control_socket::BrownfieldSurveyRequest,
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
            "brownfield-survey: thread reply failed: {e:#}"
        );
    }
}

async fn post_thread_failure(
    chatops_ctx: Option<&ChatOpsContext>,
    request: &crate::control_socket::BrownfieldSurveyRequest,
    body: &str,
) {
    post_thread(chatops_ctx, request, &format!("✗ {body}")).await;
}

fn read_workspace_readme(workspace: &Path) -> String {
    let path = workspace.join("README.md");
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
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

/// List capability directories under `<spec-root>/specs/`. Each
/// directory becomes one already-specced slug. Missing directory →
/// empty list.
fn list_already_specced(specs_dir: &Path) -> Vec<String> {
    if !specs_dir.is_dir() {
        return Vec::new();
    }
    let mut names: Vec<String> = match std::fs::read_dir(specs_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn valid_item_json(id: usize, slug: &str) -> String {
        format!(
            r#"{{"id":{id},"slug":"{slug}","summary":"x","scope_in":"in","scope_out":"out","source_modules":["src/{slug}/"],"estimated_complexity":"small"}}"#
        )
    }

    #[test]
    fn parse_valid_json_array_round_trips() {
        let json = format!("[{},{}]", valid_item_json(1, "scheduler"), valid_item_json(2, "auth"));
        let items = parse_and_validate_items(&json, 10, &[]).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].slug, "scheduler");
        assert_eq!(items[1].slug, "auth");
        assert_eq!(items[0].estimated_complexity, ComplexityBand::Small);
    }

    #[test]
    fn parse_rejects_invalid_slug_regex() {
        let bad = r#"[{"id":1,"slug":"Bad_Slug","summary":"x","scope_in":"in","scope_out":"out","source_modules":["src/x/"],"estimated_complexity":"small"}]"#;
        let err = parse_and_validate_items(bad, 10, &[]).unwrap_err();
        assert!(err.contains("slug"), "{err}");
        assert!(err.contains("Bad_Slug"), "{err}");
    }

    #[test]
    fn parse_rejects_already_specced_collision() {
        let json = format!("[{}]", valid_item_json(1, "scheduler"));
        let err = parse_and_validate_items(
            &json,
            10,
            &["scheduler".to_string()],
        )
        .unwrap_err();
        assert!(err.contains("already-specced"), "{err}");
        assert!(err.contains("scheduler"), "{err}");
    }

    #[test]
    fn parse_rejects_invalid_complexity() {
        let bad = r#"[{"id":1,"slug":"x","summary":"x","scope_in":"in","scope_out":"out","source_modules":["src/x/"],"estimated_complexity":"huge"}]"#;
        let err = parse_and_validate_items(bad, 10, &[]).unwrap_err();
        assert!(err.contains("estimated_complexity") || err.contains("huge"), "{err}");
    }

    #[test]
    fn parse_rejects_over_cap() {
        let mut s = String::from("[");
        for i in 1..=5 {
            if i > 1 {
                s.push(',');
            }
            s.push_str(&valid_item_json(i, &format!("c{i}")));
        }
        s.push(']');
        let err = parse_and_validate_items(&s, 3, &[]).unwrap_err();
        assert!(err.contains("max_capabilities"), "{err}");
    }

    #[test]
    fn parse_rejects_empty_source_modules() {
        let bad = r#"[{"id":1,"slug":"x","summary":"x","scope_in":"in","scope_out":"out","source_modules":[],"estimated_complexity":"small"}]"#;
        let err = parse_and_validate_items(bad, 10, &[]).unwrap_err();
        assert!(err.contains("source_modules"), "{err}");
    }

    #[test]
    fn parse_tolerates_fenced_response() {
        let fenced = format!("```json\n[{}]\n```", valid_item_json(1, "scheduler"));
        let items = parse_and_validate_items(&fenced, 10, &[]).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn render_items_lists_each_item_with_scope_and_source() {
        let items = vec![SurveyItem {
            id: 1,
            slug: "scheduler".into(),
            summary: "Cron-based job scheduler".into(),
            scope_in: "Cron expressions; tick loop".into(),
            scope_out: "Job execution; retry logic".into(),
            source_modules: vec!["src/cron/".into(), "src/scheduler/".into()],
            estimated_complexity: ComplexityBand::Medium,
            status: ItemStatus::Pending,
            pr_url: None,
            failure_reason: None,
        }];
        let rendered = render_items("git@github.com:a/b.git", &items);
        assert!(rendered.contains("git@github.com:a/b.git"), "{rendered}");
        assert!(rendered.contains("1. `scheduler`"), "{rendered}");
        assert!(rendered.contains("medium"), "{rendered}");
        assert!(rendered.contains("Cron expressions"), "{rendered}");
        assert!(rendered.contains("src/cron/, src/scheduler/"), "{rendered}");
    }

    #[test]
    fn list_already_specced_returns_directory_names_sorted() {
        let tmp = TempDir::new().unwrap();
        let specs = tmp.path().join("specs");
        std::fs::create_dir_all(specs.join("scheduler")).unwrap();
        std::fs::create_dir_all(specs.join("auth")).unwrap();
        std::fs::create_dir_all(specs.join("zzz")).unwrap();
        let names = list_already_specced(&specs);
        assert_eq!(names, vec!["auth", "scheduler", "zzz"]);
    }

    #[test]
    fn list_already_specced_missing_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let names = list_already_specced(&tmp.path().join("nope/specs"));
        assert!(names.is_empty());
    }

    #[test]
    fn render_survey_prompt_substitutes_each_placeholder() {
        let template = "max={{max_capabilities}} g={{guidance}} repo={{repo_url}} r={{readme}} d={{docs_listing}} sym={{symbols_overview}} alr={{already_specced}}";
        let got = render_survey_prompt(
            template, 20, "focus on storage", "git@github.com:a/b.git", "RM", "D", "S", "ALR",
        );
        assert!(got.contains("max=20"), "{got}");
        assert!(got.contains("g=focus on storage"), "{got}");
        assert!(got.contains("repo=git@github.com:a/b.git"), "{got}");
        assert!(got.contains("r=RM"), "{got}");
        assert!(got.contains("d=D"), "{got}");
        assert!(got.contains("sym=S"), "{got}");
        assert!(got.contains("alr=ALR"), "{got}");
    }

    #[test]
    fn embedded_template_references_max_capabilities_placeholder() {
        let template = PromptId::BrownfieldSurvey.embedded();
        assert!(template.contains("{{max_capabilities}}"), "embedded template missing max_capabilities");
        assert!(template.contains("{{already_specced}}"), "embedded template missing already_specced");
    }
}
