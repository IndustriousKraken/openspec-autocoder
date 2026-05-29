//! Brownfield-draft polling handler (a23). One pass per iteration: drain
//! ONE pending brownfield request from the per-repo queue, render the
//! brownfield-draft prompt, invoke the executor, verify the resulting
//! artifacts, AND open a spec-only PR.
//!
//! Workspace contract:
//!   - On entry: the polling loop has already loaded the repo snapshot
//!     AND prepared the per-repo workspace (the standard polling-pass
//!     init runs upstream). We do a defensive `ensure_initialized` +
//!     `reset --hard HEAD` + `clean -fd` + `checkout base` so the
//!     brownfield-draft pass sees a known state regardless of what the
//!     prior iteration left behind.
//!   - On exit: success → `Acted`, spec PR open. Failure → `Failed`,
//!     workspace reverted. Late conflict (spec.md already exists) →
//!     `Aborted`, workspace reverted.

use crate::config::{GithubConfig, RepositoryConfig};
use crate::executor::{BrownfieldDraftContext, Executor, ExecutorOutcome};
use crate::polling_loop::ChatOpsContext;
use crate::state::brownfield_request::{
    self, BrownfieldRequestState, BrownfieldRequestStatus,
};
use crate::{git, github};
use anyhow::{Context, Result, anyhow};
use std::path::Path;

/// Built-in default brownfield-draft prompt template, embedded at
/// compile time. The uniform [`crate::prompts::PromptLoader`] holds
/// the canonical reference now; this alias remains for the existing
/// tests that compare against the embedded bytes directly.
#[cfg(test)]
pub(crate) const DEFAULT_BROWNFIELD_TEMPLATE: &str =
    include_str!("../../../prompts/brownfield-draft.md");

/// Process the one drained brownfield request. See module docs for the
/// workspace contract. Returns `Ok(())` on every path (including
/// terminal-Failed); irrecoverable errors propagate as `Err` for the
/// caller to log.
pub async fn process_pending_brownfield(
    workspace: &Path,
    repo: &RepositoryConfig,
    executor: &dyn Executor,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    request: &crate::control_socket::BrownfieldRequest,
) -> Result<()> {
    // ----- Workspace preparation (defensive; mirrors propose path) -----
    let fork_url = match github_cfg.fork_owner.as_deref() {
        Some(owner) => Some(github::derive_fork_url(&repo.url, owner)?),
        None => None,
    };
    let fork_arg = fork_url.as_deref().map(|u| (u, repo.agent_branch.as_str()));
    crate::workspace::ensure_initialized(workspace, &repo.url, fork_arg)
        .with_context(|| "brownfield: workspace ensure_initialized".to_string())?;
    let _ = crate::queue::clear_stale_locks(workspace);
    let _ = git::reset_hard_head(workspace);
    let _ = git::clean_force(workspace);
    git::fetch(workspace).with_context(|| "brownfield: git fetch".to_string())?;
    git::checkout(workspace, &repo.base_branch)
        .with_context(|| format!("brownfield: checkout `{}`", repo.base_branch))?;
    git::pull_ff_only(workspace, &repo.base_branch)
        .with_context(|| format!("brownfield: pull --ff-only `{}`", repo.base_branch))?;

    // ----- Load the request's state file -----
    let mut state = match brownfield_request::read_state(workspace, &request.request_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(
                request_id = %request.request_id,
                "brownfield: no state file (entry pruned between enqueue and processing); skipping"
            );
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                request_id = %request.request_id,
                "brownfield: state read failed: {e:#}"
            );
            return Ok(());
        }
    };

    // ----- Late-conflict check -----
    let spec_path = workspace
        .join("openspec/specs")
        .join(&state.capability_name)
        .join("spec.md");
    if spec_path.is_file() {
        let msg = format!(
            "✗ brownfield: openspec/specs/{}/spec.md now exists (created since the request was queued). Aborting.",
            state.capability_name
        );
        if let Some(ctx) = chatops_ctx {
            let _ = ctx
                .chatops
                .post_threaded_reply(&state.channel, &state.thread_ts, &msg)
                .await;
        }
        state.status = BrownfieldRequestStatus::Aborted;
        state.reason = Some(format!(
            "openspec/specs/{}/spec.md exists at workspace HEAD",
            state.capability_name
        ));
        let _ = brownfield_request::write_state(workspace, &state);
        return Ok(());
    }

    // ----- Flip Pending → InProgress up front -----
    state.status = BrownfieldRequestStatus::InProgress;
    let _ = brownfield_request::write_state(workspace, &state);

    // ----- Prepare the spec branch (brownfield is spec-only; no fixes branch) -----
    let spec_branch = format!("{}-brownfield-{}", repo.agent_branch, state.capability_name);
    if let Err(e) = git::recreate_branch(workspace, &spec_branch) {
        mark_failed(
            workspace,
            &mut state,
            format!("recreate spec branch `{spec_branch}`: {e:#}"),
            chatops_ctx,
        )
        .await;
        let _ = git::reset_hard_head(workspace);
        let _ = git::clean_force(workspace);
        let _ = git::checkout(workspace, &repo.base_branch);
        return Ok(());
    }

    // ----- Resolve the brownfield-draft template -----
    let template = resolve_brownfield_template(workspace);

    // ----- Build the rendered prompt -----
    let readme = read_workspace_readme(workspace);
    let docs_listing = build_docs_listing(workspace);
    let symbols_overview = build_symbols_overview(workspace);
    let rendered_prompt = render_brownfield_prompt(
        &template,
        &state.capability_name,
        state.guidance.as_deref().unwrap_or("(none)"),
        &state.repo_url,
        &readme,
        &docs_listing,
        &symbols_overview,
    );

    let ctx = BrownfieldDraftContext {
        capability_name: state.capability_name.clone(),
        rendered_prompt,
    };

    tracing::info!(
        url = %repo.url,
        request_id = %state.request_id,
        capability = %state.capability_name,
        "brownfield: invoking executor"
    );
    let outcome = executor.run_brownfield_draft(workspace, &ctx).await;

    match outcome {
        Ok(ExecutorOutcome::Completed { .. }) => {
            if let Err(e) = finalize_completed(
                workspace,
                repo,
                github_cfg,
                chatops_ctx,
                &mut state,
                &spec_branch,
            )
            .await
            {
                mark_failed(
                    workspace,
                    &mut state,
                    format!("post-Completed processing: {e:#}"),
                    chatops_ctx,
                )
                .await;
                let _ = git::reset_hard_head(workspace);
                let _ = git::clean_force(workspace);
            }
        }
        Ok(ExecutorOutcome::Failed { reason }) => {
            mark_failed(workspace, &mut state, reason, chatops_ctx).await;
            let _ = git::reset_hard_head(workspace);
            let _ = git::clean_force(workspace);
        }
        Ok(ExecutorOutcome::AskUser { .. }) => {
            // AskUser leaves the state at InProgress; the existing
            // chatops escalation pipeline posts the question. We do
            // NOT post a duplicate thread reply here.
            tracing::info!(
                request_id = %state.request_id,
                "brownfield: executor returned AskUser; state stays InProgress"
            );
        }
        Ok(ExecutorOutcome::SpecNeedsRevision { .. }) => {
            mark_failed(
                workspace,
                &mut state,
                "executor flagged SpecNeedsRevision during brownfield-draft (unexpected)".to_string(),
                chatops_ctx,
            )
            .await;
            let _ = git::reset_hard_head(workspace);
            let _ = git::clean_force(workspace);
        }
        Ok(ExecutorOutcome::IterationRequested { .. }) => {
            mark_failed(
                workspace,
                &mut state,
                "executor returned IterationRequested during brownfield-draft (iteration sequences not applicable)".to_string(),
                chatops_ctx,
            )
            .await;
            let _ = git::reset_hard_head(workspace);
            let _ = git::clean_force(workspace);
        }
        Err(e) => {
            mark_failed(
                workspace,
                &mut state,
                format!("executor task error: {e:#}"),
                chatops_ctx,
            )
            .await;
            let _ = git::reset_hard_head(workspace);
            let _ = git::clean_force(workspace);
        }
    }

    // Always restore base-branch checkout so subsequent iteration
    // phases (proposal-request processing already done above, but the
    // standard change-processing pass follows) start clean.
    let _ = git::checkout(workspace, &repo.base_branch);
    Ok(())
}

/// Verify that the change directory exists AND contains the required
/// brownfield artifacts (`proposal.md`, `tasks.md`, `specs/<cap>/spec.md`).
/// Returns `Ok(())` when every artifact is present; otherwise returns
/// `Err` whose message names the missing artifact(s).
pub(crate) fn verify_change_artifacts(workspace: &Path, capability: &str) -> Result<()> {
    let change_dir = workspace
        .join("openspec/changes")
        .join(format!("brownfield-{capability}"));
    let proposal_path = change_dir.join("proposal.md");
    let tasks_path = change_dir.join("tasks.md");
    let spec_path = change_dir
        .join("specs")
        .join(capability)
        .join("spec.md");
    let mut missing: Vec<&str> = Vec::new();
    if !proposal_path.is_file() {
        missing.push("proposal.md");
    }
    if !tasks_path.is_file() {
        missing.push("tasks.md");
    }
    if !spec_path.is_file() {
        missing.push("specs/<cap>/spec.md");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "missing required change-directory artifacts: {}",
            missing.join(", ")
        ))
    }
}

/// Detect sandbox leaks by partitioning the porcelain output into
/// `openspec/` paths AND everything else. Returns the list of leaked
/// (non-`openspec/`) paths; an empty list means the run stayed within
/// the documented `WritePolicy::OpenSpecOnly` boundary.
pub(crate) fn detect_sandbox_leak(porcelain: &str) -> Vec<String> {
    porcelain
        .lines()
        .filter_map(|l| extract_porcelain_path(l).map(|p| p.to_string()))
        .filter(|p| !p.is_empty() && !p.starts_with("openspec/"))
        .collect()
}

/// Verify artifacts, run the sandbox-leak check, push the spec branch,
/// AND open the spec PR. Returns Err on any verification / push / PR
/// failure so the caller can transition state to Failed.
async fn finalize_completed(
    workspace: &Path,
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    chatops_ctx: Option<&ChatOpsContext>,
    state: &mut BrownfieldRequestState,
    spec_branch: &str,
) -> Result<()> {
    // 1. Verify the change directory contains the required artifacts.
    verify_change_artifacts(workspace, &state.capability_name)?;

    // 2. Sandbox-leak check: every modified path MUST be under
    //    `openspec/`. Anything else means the executor violated the
    //    WritePolicy::OpenSpecOnly contract.
    let porcelain = git::status_porcelain_untracked_all(workspace)
        .with_context(|| "brownfield: reading git status".to_string())?;
    let leaked = detect_sandbox_leak(&porcelain);
    if !leaked.is_empty() {
        tracing::warn!(
            request_id = %state.request_id,
            "brownfield: sandbox leak — paths outside openspec/: {leaked:?}"
        );
        return Err(anyhow!(
            "sandbox violation: paths outside openspec/ were modified: {}",
            leaked.join(", ")
        ));
    }

    // (resume to original control flow)
    let proposal_path = workspace
        .join("openspec/changes")
        .join(format!("brownfield-{}", state.capability_name))
        .join("proposal.md");

    // 3. Read the proposal's "Why" section for the PR body.
    let why_section = extract_why_section(&proposal_path);

    // 4. Stage every change-directory file AND commit.
    let change_rel = format!(
        "openspec/changes/brownfield-{}/",
        state.capability_name
    );
    let _ = std::process::Command::new("git")
        .args(["add", "--", &change_rel])
        .current_dir(workspace)
        .status();
    let subject = format!(
        "Brownfield draft: capability `{}`",
        state.capability_name
    );
    git::commit(workspace, &subject)
        .with_context(|| "brownfield: commit spec branch".to_string())?;

    // 5. Push the spec branch.
    let push_remote = if github_cfg.fork_owner.is_some() {
        "fork"
    } else {
        "origin"
    };
    git::push_force_with_lease(workspace, spec_branch, push_remote)
        .with_context(|| format!("brownfield: pushing spec branch `{spec_branch}`"))?;

    // 6. Open the PR.
    let pr_title = format!("Brownfield: {}", state.capability_name);
    let pr_body = build_pr_body(&state.capability_name, &state.repo_url, &why_section);
    let pr_url =
        open_brownfield_pull_request(repo, github_cfg, spec_branch, &repo.base_branch, &pr_title, &pr_body)
            .await
            .with_context(|| "brownfield: opening PR".to_string())?;

    // 7. Record success + post thread reply.
    state.status = BrownfieldRequestStatus::Acted;
    state.pr_url = Some(pr_url.clone());
    let _ = brownfield_request::write_state(workspace, state);
    if let Some(ctx) = chatops_ctx {
        let body = format_pr_opened_message(&pr_url);
        if let Err(e) = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.thread_ts, &body)
            .await
        {
            tracing::warn!(
                request_id = %state.request_id,
                "brownfield: PR-opened thread reply failed: {e:#}"
            );
        }
    }
    Ok(())
}

/// Build the lifecycle-thread reply body posted when the brownfield
/// PR lands. Matches the documented `✅ Brownfield draft PR opened:
/// <pr_url>` shape (task 6.2).
pub(crate) fn format_pr_opened_message(pr_url: &str) -> String {
    format!("✅ Brownfield draft PR opened: {pr_url}")
}

/// Build the lifecycle-thread reply body posted on terminal failure.
/// Matches the documented `✗ Brownfield draft failed: <reason>` shape
/// (task 6.3) AND appends a daemon-log pointer keyed by request_id so
/// operators can pull the full context without scraping the chat.
pub(crate) fn format_failed_message(reason: &str, request_id: &str) -> String {
    format!(
        "✗ Brownfield draft failed: {reason}\n\n_(See the daemon log for full context: `journalctl -u autocoder | grep request_id={request_id}`)_"
    )
}

/// Resolve the brownfield-draft prompt template via the uniform
/// [`crate::prompts::PromptLoader`] (a24). The polling layer doesn't
/// have direct access to the live `Config` holder on this codepath, so
/// this helper passes `None` for the nested override AND relies on
/// `resolve_brownfield_template_from_path` for explicit override
/// plumbing. The embedded default is the only template returned here
/// when production wiring has not threaded a config-driven path.
fn resolve_brownfield_template(workspace: &Path) -> String {
    use crate::prompts::{PromptId, PromptLoader};
    PromptLoader::load(PromptId::BrownfieldDraft, None, None, Some(workspace))
}

/// Resolve the brownfield-draft template from an explicit override
/// path via the uniform [`crate::prompts::PromptLoader`] (a24).
///
///   - `Some(path)` AND the file exists AND is non-empty → the file's
///     contents are returned.
///   - `Some(path)` AND the file is missing OR empty → a one-shot
///     WARN names the offending path AND the embedded default is
///     returned.
///   - `None` → the embedded default is returned silently.
#[allow(dead_code)]
pub fn resolve_brownfield_template_from_path(
    workspace: &Path,
    override_path: Option<&Path>,
) -> String {
    use crate::prompts::{PromptId, PromptLoader};
    PromptLoader::load(
        PromptId::BrownfieldDraft,
        override_path,
        None,
        Some(workspace),
    )
}

/// Substitute the brownfield-draft context fields into the template's
/// `{{...}}` placeholders. Missing placeholders are simply left out
/// (the template is the source of truth for which substitutions matter).
fn render_brownfield_prompt(
    template: &str,
    capability_name: &str,
    guidance: &str,
    repo_url: &str,
    readme: &str,
    docs_listing: &str,
    symbols_overview: &str,
) -> String {
    template
        .replace("{{capability_name}}", capability_name)
        .replace("{{guidance}}", guidance)
        .replace("{{repo_url}}", repo_url)
        .replace("{{readme}}", readme)
        .replace("{{docs_listing}}", docs_listing)
        .replace("{{symbols_overview}}", symbols_overview)
}

/// Read the workspace's `README.md`. Returns a placeholder when the
/// file is absent or unreadable.
fn read_workspace_readme(workspace: &Path) -> String {
    let path = workspace.join("README.md");
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => "(no README.md at workspace root)".to_string(),
    }
}

/// Build a newline-separated listing of `docs/*.md` filenames in the
/// workspace. Returns a placeholder when no `docs/` directory exists
/// OR when it contains no markdown files.
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

/// Build a code-symbol overview for the workspace. For Rust workspaces
/// (presence of `Cargo.toml`), run `cargo metadata --no-deps --format-version=1`
/// AND extract the package + target names. For other languages, fall back
/// to a ripgrep pass for likely top-level public items. Errors AND empty
/// output produce a placeholder.
fn build_symbols_overview(workspace: &Path) -> String {
    if workspace.join("Cargo.toml").is_file() {
        match cargo_metadata_overview(workspace) {
            Some(s) if !s.is_empty() => return s,
            _ => {}
        }
    }
    // Generic ripgrep fallback: look for likely public-API patterns
    // across common languages. Capped output keeps the prompt small.
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

/// Pull the path out of a `git status --porcelain` line. Local copy of
/// the same parser used by `polling_loop`; kept here to avoid a wider
/// cross-module dependency.
fn extract_porcelain_path(line: &str) -> Option<&str> {
    let trimmed = line.get(3..)?.trim_start();
    let path = if let Some(idx) = trimmed.rfind(" -> ") {
        trimmed[idx + 4..].trim()
    } else {
        trimmed.trim()
    };
    if path.is_empty() { None } else { Some(path) }
}

/// Extract the `## Why` section text from a brownfield proposal.md.
/// Returns the body up to (but excluding) the next top-level header,
/// trimmed. Best-effort: an absent / malformed proposal returns a
/// placeholder so the PR body remains presentable.
fn extract_why_section(proposal_path: &Path) -> String {
    let raw = match std::fs::read_to_string(proposal_path) {
        Ok(s) => s,
        Err(_) => return "(proposal Why section unavailable)".to_string(),
    };
    let mut in_why = false;
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        if line.trim_start().starts_with("## ") {
            if in_why {
                break;
            }
            if line.trim().eq_ignore_ascii_case("## Why") {
                in_why = true;
                continue;
            }
        }
        if in_why {
            out.push(line.to_string());
        }
    }
    let body = out.join("\n").trim().to_string();
    if body.is_empty() {
        "(proposal Why section was empty)".to_string()
    } else {
        body
    }
}

fn build_pr_body(capability: &str, repo_url: &str, why_section: &str) -> String {
    format!(
        "This PR adds the initial canonical spec for capability `{capability}` in `{repo_url}`.\n\n\
         Brownfield drafting captures **existing** behavior under canonical OpenSpec requirements. \
         No code changes are included; review the requirements against the named code modules AND iterate via `@<bot> revise <text>` if the spec misses or misstates any behavior.\n\n\
         ## Why\n\n{why_section}\n"
    )
}

/// Open the spec-only brownfield PR. Mirrors
/// `polling_loop::open_triage_pull_request` but lives in this module so
/// brownfield does not depend on private polling-loop helpers.
async fn open_brownfield_pull_request(
    repo: &RepositoryConfig,
    github_cfg: &GithubConfig,
    head_branch: &str,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let (owner, name) = github::parse_repo_url(&repo.url)
        .with_context(|| "brownfield: parsing repo URL".to_string())?;
    let token = crate::github_credentials::resolve_token(github_cfg, &owner)?;
    let head = if let Some(fork_owner) = github_cfg.fork_owner.as_deref() {
        format!("{fork_owner}:{head_branch}")
    } else {
        head_branch.to_string()
    };
    let pr = github::create_pull_request(
        &owner,
        &name,
        &head,
        base_branch,
        title,
        body,
        &token,
        None,
        false,
    )
    .await?;
    Ok(pr.html_url)
}

/// Flip the brownfield-request state to `Failed` AND post the failure
/// to the lifecycle thread. Best-effort — every step here logs AND
/// continues so the surrounding iteration is unaffected.
async fn mark_failed(
    workspace: &Path,
    state: &mut BrownfieldRequestState,
    reason: String,
    chatops_ctx: Option<&ChatOpsContext>,
) {
    state.status = BrownfieldRequestStatus::Failed;
    state.reason = Some(reason.clone());
    if let Err(e) = brownfield_request::write_state(workspace, state) {
        tracing::warn!(
            request_id = %state.request_id,
            "brownfield: recording Failed state failed: {e:#}"
        );
    }
    if let Some(ctx) = chatops_ctx {
        let body = format_failed_message(&reason, &state.request_id);
        if let Err(e) = ctx
            .chatops
            .post_threaded_reply(&state.channel, &state.thread_ts, &body)
            .await
        {
            tracing::warn!(
                request_id = %state.request_id,
                "brownfield: Failed thread reply failed: {e:#}"
            );
        }
    }
}

/// Test helper: re-exported building block so the tests can construct
/// a `BrownfieldDraftContext` against a known template + inputs.
#[cfg(test)]
pub(crate) fn test_render_prompt(
    template: &str,
    capability_name: &str,
    guidance: &str,
    repo_url: &str,
    readme: &str,
    docs_listing: &str,
    symbols_overview: &str,
) -> String {
    render_brownfield_prompt(
        template,
        capability_name,
        guidance,
        repo_url,
        readme,
        docs_listing,
        symbols_overview,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn render_substitutes_known_placeholders() {
        let template = "cap={{capability_name}} guidance={{guidance}} repo={{repo_url}}";
        let got = test_render_prompt(
            template,
            "scheduler",
            "focus on cron",
            "git@github.com:acme/r.git",
            "ignored",
            "ignored",
            "ignored",
        );
        assert!(got.contains("cap=scheduler"), "{got}");
        assert!(got.contains("guidance=focus on cron"), "{got}");
        assert!(got.contains("repo=git@github.com:acme/r.git"), "{got}");
    }

    #[test]
    fn render_preserves_template_when_no_placeholders() {
        let template = "no placeholders here";
        let got = test_render_prompt(template, "cap", "g", "u", "r", "d", "s");
        assert_eq!(got, "no placeholders here");
    }

    #[test]
    fn resolve_template_falls_back_when_override_missing() {
        let tmp = TempDir::new().unwrap();
        let got = resolve_brownfield_template_from_path(
            tmp.path(),
            Some(Path::new("does-not-exist.md")),
        );
        assert_eq!(got, DEFAULT_BROWNFIELD_TEMPLATE);
    }

    #[test]
    fn resolve_template_uses_override_when_present() {
        let tmp = TempDir::new().unwrap();
        let rel: PathBuf = PathBuf::from("prompts").join("brownfield-custom.md");
        let abs = tmp.path().join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "CUSTOM TEMPLATE BODY").unwrap();
        let got = resolve_brownfield_template_from_path(tmp.path(), Some(&rel));
        assert_eq!(got, "CUSTOM TEMPLATE BODY");
    }

    #[test]
    fn resolve_template_default_when_no_override() {
        let tmp = TempDir::new().unwrap();
        let got = resolve_brownfield_template_from_path(tmp.path(), None);
        assert_eq!(got, DEFAULT_BROWNFIELD_TEMPLATE);
    }

    #[test]
    fn embedded_template_mentions_capability_placeholder() {
        // Sanity: the embedded prompt SHOULD reference the capability
        // placeholder we substitute at runtime. If someone rewrites the
        // template AND drops the placeholder, this fails loudly.
        assert!(
            DEFAULT_BROWNFIELD_TEMPLATE.contains("{{capability_name}}"),
            "embedded template must reference {{{{capability_name}}}}"
        );
    }

    #[test]
    fn extract_why_section_pulls_body_only() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("proposal.md");
        std::fs::write(
            &p,
            "## Why\n\nThis captures existing scheduler behavior.\n\nMore detail.\n\n## What Changes\n\nadded reqs",
        )
        .unwrap();
        let got = extract_why_section(&p);
        assert!(got.contains("captures existing scheduler behavior"), "{got}");
        assert!(!got.contains("What Changes"), "{got}");
    }

    #[test]
    fn extract_why_section_missing_file_is_placeholder() {
        let got = extract_why_section(Path::new("/no/such/path.md"));
        assert!(got.contains("unavailable"), "{got}");
    }

    #[test]
    fn build_docs_listing_returns_placeholder_when_dir_absent() {
        let tmp = TempDir::new().unwrap();
        let got = build_docs_listing(tmp.path());
        assert!(got.contains("no docs/"), "{got}");
    }

    #[test]
    fn build_docs_listing_lists_md_files() {
        let tmp = TempDir::new().unwrap();
        let docs = tmp.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("intro.md"), "x").unwrap();
        std::fs::write(docs.join("config.md"), "y").unwrap();
        std::fs::write(docs.join("not-md.txt"), "z").unwrap();
        let got = build_docs_listing(tmp.path());
        assert!(got.contains("docs/intro.md"), "{got}");
        assert!(got.contains("docs/config.md"), "{got}");
        assert!(!got.contains("not-md.txt"), "{got}");
    }

    #[test]
    fn build_pr_body_includes_why_and_repo() {
        let body = build_pr_body(
            "scheduler",
            "git@github.com:acme/myrepo.git",
            "Capability X exists and we are capturing it.",
        );
        assert!(body.contains("scheduler"), "{body}");
        assert!(body.contains("git@github.com:acme/myrepo.git"), "{body}");
        assert!(body.contains("Capability X exists"), "{body}");
        assert!(body.contains("No code changes"), "{body}");
    }

    #[test]
    fn extract_porcelain_path_handles_simple_and_rename() {
        assert_eq!(extract_porcelain_path(" M src/foo.rs"), Some("src/foo.rs"));
        assert_eq!(
            extract_porcelain_path("R  old.rs -> new.rs"),
            Some("new.rs")
        );
        assert_eq!(extract_porcelain_path(""), None);
    }

    fn write_artifacts(workspace: &Path, capability: &str) {
        let change_dir = workspace
            .join("openspec/changes")
            .join(format!("brownfield-{capability}"));
        let spec_dir = change_dir.join("specs").join(capability);
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::write(
            change_dir.join("proposal.md"),
            "## Why\n\ncaptures existing behavior\n\n## What Changes\n\nADDs reqs",
        )
        .unwrap();
        std::fs::write(change_dir.join("tasks.md"), "## 1. Validate\n\n- [ ] 1.1 x").unwrap();
        std::fs::write(spec_dir.join("spec.md"), "## ADDED Requirements\n\n### Requirement: x").unwrap();
    }

    #[test]
    fn verify_change_artifacts_ok_when_all_present() {
        let tmp = TempDir::new().unwrap();
        write_artifacts(tmp.path(), "scheduler");
        verify_change_artifacts(tmp.path(), "scheduler").expect("all files present must pass");
    }

    #[test]
    fn verify_change_artifacts_err_when_spec_missing() {
        let tmp = TempDir::new().unwrap();
        write_artifacts(tmp.path(), "scheduler");
        // Drop the spec.md file.
        std::fs::remove_file(
            tmp.path()
                .join("openspec/changes/brownfield-scheduler/specs/scheduler/spec.md"),
        )
        .unwrap();
        let err = verify_change_artifacts(tmp.path(), "scheduler")
            .expect_err("missing spec.md must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("specs/<cap>/spec.md"), "{msg}");
    }

    #[test]
    fn verify_change_artifacts_err_when_proposal_missing() {
        let tmp = TempDir::new().unwrap();
        write_artifacts(tmp.path(), "scheduler");
        std::fs::remove_file(
            tmp.path()
                .join("openspec/changes/brownfield-scheduler/proposal.md"),
        )
        .unwrap();
        let err = verify_change_artifacts(tmp.path(), "scheduler")
            .expect_err("missing proposal.md must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("proposal.md"), "{msg}");
    }

    #[test]
    fn verify_change_artifacts_err_when_change_dir_absent() {
        let tmp = TempDir::new().unwrap();
        // No change directory at all.
        let err = verify_change_artifacts(tmp.path(), "scheduler")
            .expect_err("absent change dir must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing"), "{msg}");
    }

    #[test]
    fn detect_sandbox_leak_empty_when_diff_is_pure_openspec() {
        let porcelain = " M openspec/changes/brownfield-scheduler/proposal.md\n A openspec/changes/brownfield-scheduler/tasks.md\n";
        let leaked = detect_sandbox_leak(porcelain);
        assert!(leaked.is_empty(), "expected no leaks; got {leaked:?}");
    }

    #[test]
    fn detect_sandbox_leak_lists_paths_outside_openspec() {
        let porcelain = " M openspec/changes/x/proposal.md\n M src/lib.rs\n A README.md\n";
        let mut leaked = detect_sandbox_leak(porcelain);
        leaked.sort();
        assert_eq!(leaked, vec!["README.md".to_string(), "src/lib.rs".to_string()]);
    }

    #[test]
    fn detect_sandbox_leak_handles_renames_into_source() {
        let porcelain = "R  openspec/changes/x/old.md -> src/leak.rs\n";
        let leaked = detect_sandbox_leak(porcelain);
        assert_eq!(leaked, vec!["src/leak.rs".to_string()]);
    }

    #[test]
    fn detect_sandbox_leak_skips_blank_lines() {
        let porcelain = "\n\n M openspec/changes/x/p.md\n";
        let leaked = detect_sandbox_leak(porcelain);
        assert!(leaked.is_empty(), "{leaked:?}");
    }

    #[test]
    fn format_pr_opened_matches_documented_shape() {
        let msg = format_pr_opened_message("https://github.com/acme/repo/pull/42");
        assert_eq!(
            msg,
            "✅ Brownfield draft PR opened: https://github.com/acme/repo/pull/42"
        );
    }

    #[test]
    fn format_failed_includes_reason_and_log_pointer() {
        let msg = format_failed_message("executor timeout", "req-xyz-123");
        assert!(
            msg.starts_with("✗ Brownfield draft failed: executor timeout"),
            "{msg}"
        );
        assert!(msg.contains("journalctl"), "{msg}");
        assert!(msg.contains("request_id=req-xyz-123"), "{msg}");
    }
}
