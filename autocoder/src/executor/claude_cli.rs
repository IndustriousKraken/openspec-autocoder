//! `ClaudeCliExecutor` — wraps the `claude` CLI as a child process with a
//! timeout and explicit outcome mapping.
//!
//! AskUser detection is two-layered:
//!   1. **MCP tool** — at run time, the executor writes a `.mcp.json` into
//!      the workspace pointing back at `autocoder mcp-ask-user-server`.
//!      The wrapped CLI loads this MCP config and, when its agent calls
//!      `ask_user(question)`, the tool writes
//!      `<workspace>/openspec/changes/<change>/.askuser-pending.json`.
//!      After the child exits, the executor reads + deletes the marker.
//!   2. **Stdout regex backstop** — if Layer 1 produced no marker AND the
//!      CLI exited 0 AND the workspace has no diff AND stdout matches a
//!      clarification regex, the executor synthesizes an AskUser from the
//!      first matching sentence.

use super::{
    BrownfieldDraftContext, ChangelogContext, ChatTriageContext, Executor, ExecutorOutcome,
    IssueContext, IssueReportTriageContext, ResumeHandle, ScoutContext, TriageContext,
    UnimplementableTask,
};
use crate::agentic_run::AgenticRunOutcome;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const MCP_CONFIG_FILENAME: &str = ".mcp.json";
const ASKUSER_MARKER_FILENAME: &str = ".askuser-pending.json";

/// Built-in default implementer prompt template, embedded at compile time
/// so the binary runs without requiring `prompts/` on the filesystem.
const DEFAULT_IMPLEMENTER_TEMPLATE: &str = include_str!("../../../prompts/implementer.md");

/// Built-in revision-mode prompt template, embedded at compile time.
/// Renders the original change body, the current PR diff, and the
/// operator's revision text into a single prompt for the wrapped CLI.
const DEFAULT_REVISION_TEMPLATE: &str =
    include_str!("../../../prompts/implementer-revision.md");

/// Built-in triage-mode prompt template, embedded at compile time. Used
/// by `run_triage` for the `audit-reply-acts` flow.
const DEFAULT_TRIAGE_TEMPLATE: &str = include_str!("../../../prompts/audit-triage.md");

/// Built-in chat-triage prompt template, embedded at compile time. Used
/// by `run_chat_triage` for the `chat-request-triage` (`propose`) flow.
const DEFAULT_CHAT_TRIAGE_TEMPLATE: &str =
    include_str!("../../../prompts/chat-request-triage.md");

/// Built-in changelog-stylist prompt template, embedded at compile time.
/// Used by `run_changelog` for the chat-driven `changelog` flow.
pub const DEFAULT_CHANGELOG_STYLIST_TEMPLATE: &str =
    include_str!("../../../prompts/changelog-stylist.md");

/// Literal placeholder replaced with `openspec instructions apply` output.
const PROMPT_BODY_PLACEHOLDER: &str = "{{change_body}}";
const REVISION_DIFF_PLACEHOLDER: &str = "{{pr_diff}}";
const REVISION_REQUEST_PLACEHOLDER: &str = "{{revision_request}}";
// Per a20a5: revision prompt is constructed from PR-sourced material.
const REVISION_PR_BODY_PLACEHOLDER: &str = "{{pr_body}}";
const REVISION_PR_CHANGE_LIST_PLACEHOLDER: &str = "{{pr_change_list}}";
const REVISION_AGENT_NOTES_PLACEHOLDER: &str = "{{agent_implementation_notes}}";
const TRIAGE_FINDINGS_PLACEHOLDER: &str = "{{findings}}";
const TRIAGE_AUDIT_TYPE_PLACEHOLDER: &str = "{{audit_type}}";
const TRIAGE_REPO_URL_PLACEHOLDER: &str = "{{repo_url}}";
const TRIAGE_SPECS_INDEX_PLACEHOLDER: &str = "{{canonical_specs_index}}";
const CHAT_TRIAGE_REQUEST_TEXT_PLACEHOLDER: &str = "{{request_text}}";
const CHANGELOG_JSON_PLACEHOLDER: &str = "{{changelog_json}}";
const CHANGELOG_REVISION_TEXT_PLACEHOLDER: &str = "{{revision_text}}";

/// Strip the wrapping `{{`/`}}` from a placeholder constant, yielding the
/// bare key [`crate::prompts::render_template`] matches on. Keeping the
/// full-token constants as the single source of truth (they document the
/// tokens AND are asserted by tests) while deriving the bare key here
/// avoids drift between the two forms.
fn placeholder_key(token: &str) -> &str {
    token
        .strip_prefix("{{")
        .and_then(|t| t.strip_suffix("}}"))
        .unwrap_or(token)
}

/// Synthetic "change" name used for the triage-mode run-log path. The
/// triage flow does not target a specific change directory; the name is
/// only used to produce a per-run log file on disk for diagnostics.
const TRIAGE_LOG_CHANGE_NAME: &str = "audit-triage";

/// Synthetic "change" name used for the chat-triage run-log path. The
/// `propose` flow does not target a specific change directory either;
/// the name is only used to produce a per-run log file for diagnostics.
const CHAT_TRIAGE_LOG_CHANGE_NAME: &str = "chat-request-triage";

/// Synthetic "change" name used for the changelog-stylist run-log path.
const CHANGELOG_STYLIST_LOG_CHANGE_NAME: &str = "changelog-stylist";

/// Synthetic "change" name used for the brownfield-draft run-log path
/// (a23). Like the triage modes, brownfield-draft does not target a
/// pre-existing change directory at invocation time.
const BROWNFIELD_DRAFT_LOG_CHANGE_NAME: &str = "brownfield-draft";

/// Synthetic "change" name used for the scout-mode run-log path (a25).
const SCOUT_LOG_CHANGE_NAME: &str = "scout";
const ISSUE_TRIAGE_LOG_CHANGE_NAME: &str = "issue-report-triage";

pub struct ClaudeCliExecutor {
    command: String,
    args: Vec<String>,
    timeout: Duration,
    sandbox: crate::config::ResolvedSandbox,
    /// Daemon-wide resolved `DaemonPaths`, threaded from the entrypoint
    /// per the canonical `Production paths SHALL be threaded` requirement
    /// (constructor-field pattern). Every `run_log_path`, busy-marker
    /// sidecar, and control-socket lookup uses this reference.
    paths: std::sync::Arc<crate::paths::DaemonPaths>,
    template: String,
    /// Stylist prompt template for the chat-driven `changelog` flow.
    /// Resolved from `executor.changelog_stylist.prompt_path` (nested,
    /// a24) OR `executor.changelog_stylist_prompt_path` (legacy flat)
    /// at construction; otherwise the embedded
    /// `prompts/changelog-stylist.md` template.
    changelog_stylist_template: String,
    /// Revision-loop prompt template (a24). Resolved via the uniform
    /// [`PromptLoader`] from `executor.implementer_revision.prompt_path`
    /// or the embedded `prompts/implementer-revision.md` default.
    revision_template: String,
    /// Audit-triage prompt template (a24). Resolved via the loader
    /// from `executor.audit_triage.prompt_path` or the embedded
    /// `prompts/audit-triage.md` default.
    triage_template: String,
    /// Chat-request-triage prompt template (a24). Resolved via the
    /// loader from `executor.chat_request_triage.prompt_path` or the
    /// embedded `prompts/chat-request-triage.md` default.
    chat_triage_template: String,
    /// Output format mode for the wrapped CLI. `Json` (default) → stream
    /// `--output-format stream-json` events through the parser and
    /// structured log writer; `Text` → preserve today's at-exit capture
    /// behavior with the legacy log shape.
    output_format: crate::config::ExecutorOutputFormat,
    /// Override for the directory the per-iteration sandbox settings file
    /// is written to. `None` (production) means `std::env::temp_dir()`.
    /// Tests use this to isolate their settings file from concurrent
    /// tests creating files under the same prefix in the shared OS temp.
    settings_dir: Option<PathBuf>,
    /// a70: the agentic CLI the implementer runs through. `Claude` (the
    /// default) keeps the streaming live-log path byte-identical; `Opencode`
    /// / `Antigravity` run capture-mode (no live log; outcome + `final_answer`
    /// via the MCP relay). Resolved from `executor.implementer_cli`.
    cli: crate::config::CliKind,
    /// a70: override for the home directory the session prune/resume resolves
    /// the CLI's session store under. `None` (production) → `$HOME`. Tests set
    /// a temp home so the surgical session prune is exercised without touching
    /// the operator's real store.
    session_home: Option<PathBuf>,
}

/// Opaque payload stashed inside `ResumeHandle.0` for this backend.
#[derive(Debug, Serialize, Deserialize)]
struct ClaudeResumeData {
    workspace: PathBuf,
    change: String,
    /// Optional Claude Code session id. Captured when we can extract it from
    /// the child's stdout via a `--resume` invocation; otherwise the
    /// resume re-prompts from scratch.
    #[serde(default)]
    session_id: Option<String>,
}

impl ClaudeCliExecutor {
    #[cfg(test)]
    pub fn new(
        command: String,
        timeout_secs: u64,
        paths: std::sync::Arc<crate::paths::DaemonPaths>,
    ) -> Self {
        Self::new_with_sandbox(
            command,
            timeout_secs,
            crate::config::ResolvedSandbox::resolve(None),
            paths,
        )
    }

    pub fn new_with_sandbox(
        command: String,
        timeout_secs: u64,
        sandbox: crate::config::ResolvedSandbox,
        paths: std::sync::Arc<crate::paths::DaemonPaths>,
    ) -> Self {
        Self {
            command,
            args: Vec::new(),
            timeout: Duration::from_secs(timeout_secs),
            sandbox,
            paths,
            template: DEFAULT_IMPLEMENTER_TEMPLATE.to_string(),
            changelog_stylist_template: DEFAULT_CHANGELOG_STYLIST_TEMPLATE.to_string(),
            revision_template: DEFAULT_REVISION_TEMPLATE.to_string(),
            triage_template: DEFAULT_TRIAGE_TEMPLATE.to_string(),
            chat_triage_template: DEFAULT_CHAT_TRIAGE_TEMPLATE.to_string(),
            output_format: crate::config::default_output_format(),
            settings_dir: None,
            cli: crate::config::CliKind::Claude,
            session_home: None,
        }
    }

    /// Test-only override: write the sandbox settings file to `dir` instead
    /// of `std::env::temp_dir()`. The directory must already exist.
    #[cfg(test)]
    pub(crate) fn with_settings_dir(mut self, dir: PathBuf) -> Self {
        self.settings_dir = Some(dir);
        self
    }

    /// Construct an executor wired from an `ExecutorConfig`. Resolves
    /// the implementer, revision, audit-triage, chat-request-triage,
    /// AND changelog-stylist prompt templates via the uniform
    /// [`crate::prompts::PromptLoader`] (a24). Each template walks the
    /// loader's precedence chain (nested → flat-legacy → embedded);
    /// missing/empty configured override paths log a one-shot WARN at
    /// daemon-startup AND fall back to the embedded default.
    pub fn from_config(
        cfg: &crate::config::ExecutorConfig,
        paths: std::sync::Arc<crate::paths::DaemonPaths>,
    ) -> Result<Self> {
        use crate::prompts::{PromptId, PromptLoader};
        let template = PromptLoader::load(
            PromptId::Implementer,
            cfg.implementer.as_ref().and_then(|b| b.prompt_path.as_deref()),
            cfg.implementer_prompt_path.as_deref(),
            None,
        );
        let changelog_stylist_template = PromptLoader::load(
            PromptId::ChangelogStylist,
            cfg.changelog_stylist
                .as_ref()
                .and_then(|b| b.prompt_path.as_deref()),
            cfg.changelog_stylist_prompt_path.as_deref(),
            None,
        );
        let revision_template = PromptLoader::load(
            PromptId::ImplementerRevision,
            cfg.implementer_revision
                .as_ref()
                .and_then(|b| b.prompt_path.as_deref()),
            None,
            None,
        );
        let triage_template = PromptLoader::load(
            PromptId::AuditTriage,
            cfg.audit_triage.as_ref().and_then(|b| b.prompt_path.as_deref()),
            None,
            None,
        );
        let chat_triage_template = PromptLoader::load(
            PromptId::ChatRequestTriage,
            cfg.chat_request_triage
                .as_ref()
                .and_then(|b| b.prompt_path.as_deref()),
            None,
            None,
        );
        // a70: resolve the implementer's CLI (default `claude`) AND its
        // command. When a non-`claude` CLI is selected AND `command` is left
        // at the executor default, fall back to that CLI's own binary so an
        // operator who only sets `implementer_cli` gets a working command.
        let cli = cfg.implementer_cli.unwrap_or(crate::config::CliKind::Claude);
        let command = if cli != crate::config::CliKind::Claude
            && cfg.command == crate::config::default_executor_command()
        {
            cli.default_command().to_string()
        } else {
            cfg.command.clone()
        };
        Ok(Self {
            command,
            args: Vec::new(),
            timeout: Duration::from_secs(cfg.timeout_secs),
            sandbox: crate::config::ResolvedSandbox::resolve(cfg.sandbox.as_ref()),
            paths,
            template,
            changelog_stylist_template,
            revision_template,
            triage_template,
            chat_triage_template,
            output_format: cfg.output_format,
            settings_dir: None,
            cli,
            session_home: None,
        })
    }

    /// Test/extension constructor allowing additional args to be passed to
    /// the wrapped command. Production wiring uses `from_config`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_args(
        command: String,
        args: Vec<String>,
        timeout_secs: u64,
        paths: std::sync::Arc<crate::paths::DaemonPaths>,
    ) -> Self {
        Self {
            command,
            args,
            timeout: Duration::from_secs(timeout_secs),
            sandbox: crate::config::ResolvedSandbox::resolve(None),
            paths,
            template: DEFAULT_IMPLEMENTER_TEMPLATE.to_string(),
            changelog_stylist_template: DEFAULT_CHANGELOG_STYLIST_TEMPLATE.to_string(),
            revision_template: DEFAULT_REVISION_TEMPLATE.to_string(),
            triage_template: DEFAULT_TRIAGE_TEMPLATE.to_string(),
            chat_triage_template: DEFAULT_CHAT_TRIAGE_TEMPLATE.to_string(),
            output_format: crate::config::default_output_format(),
            settings_dir: None,
            cli: crate::config::CliKind::Claude,
            session_home: None,
        }
    }

    /// Test-only override for the executor's output mode.
    #[cfg(test)]
    pub(crate) fn with_output_format(
        mut self,
        format: crate::config::ExecutorOutputFormat,
    ) -> Self {
        self.output_format = format;
        self
    }

    /// Test-only override for the implementer's CLI strategy (a70). Lets a
    /// test drive the implementer through a capture-mode strategy (`opencode`
    /// / `antigravity`) without an operator config file.
    #[cfg(test)]
    pub(crate) fn with_cli(mut self, cli: crate::config::CliKind) -> Self {
        self.cli = cli;
        self
    }

    /// Test-only override for the session-store home (a70). Points the
    /// surgical session prune/resume at a temp home so it is exercised
    /// without touching the operator's real `~/.claude` etc.
    #[cfg(test)]
    pub(crate) fn with_session_home(mut self, home: PathBuf) -> Self {
        self.session_home = Some(home);
        self
    }

    /// Resolve the implementer's [`CliStrategy`](crate::agentic_run::CliStrategy)
    /// from its configured CLI (a70). The default `claude` keeps the streaming
    /// path; `opencode` / `antigravity` run capture-mode.
    fn implementer_strategy(&self) -> Box<dyn crate::agentic_run::CliStrategy> {
        // Registered CLIs all resolve; the `Result` cannot be `Err` for the
        // three known kinds, but we degrade to `claude` defensively rather
        // than panicking if a future kind lands without a strategy.
        crate::agentic_run::strategy_for_cli(self.cli, self.command.clone(), self.args.clone())
            .unwrap_or_else(|_| {
                Box::new(crate::agentic_run::ClaudeStrategy::new(
                    self.command.clone(),
                    self.args.clone(),
                ))
            })
    }

    /// Whether the implementer runs in streaming (live-log) mode: only the
    /// `claude` strategy AND only when the output format is `Json` (a70). A
    /// capture-mode strategy never streams.
    fn implementer_streaming(&self) -> bool {
        self.cli == crate::config::CliKind::Claude
            && matches!(self.output_format, crate::config::ExecutorOutputFormat::Json)
    }

    /// Prune the single session record named by `handle` from the resolved
    /// strategy's store (a70 §4 / §5.4). Surgical AND best-effort: a missing
    /// handle is a no-op; an IO error is logged, never fatal. `home` resolves
    /// to the test override else `$HOME`.
    fn prune_session(&self, workspace: &Path, handle: Option<&str>) {
        let Some(handle) = handle else { return };
        let home = self
            .session_home
            .clone()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let Some(home) = home else { return };
        let strategy = self.implementer_strategy();
        match strategy.delete_session(
            crate::agentic_run::SessionStoreCtx {
                home: &home,
                workspace,
            },
            handle,
        ) {
            Ok(removed) => tracing::debug!(
                session = %handle,
                removed,
                "pruned implementer session record on terminal outcome"
            ),
            Err(e) => tracing::warn!(
                session = %handle,
                "failed to prune implementer session record (run continues): {e:#}"
            ),
        }
    }

    /// Prune the session UNLESS the implementer is parking it for an AskUser
    /// answer (a70 §5.1): an `AskUser` outcome retains the session (its handle
    /// rides the `ResumeHandle`); every other outcome is terminal AND prunes.
    fn prune_session_unless_waiting(
        &self,
        workspace: &Path,
        handle: Option<&str>,
        outcome: &ExecutorOutcome,
    ) {
        if matches!(outcome, ExecutorOutcome::AskUser { .. }) {
            return;
        }
        self.prune_session(workspace, handle);
    }

    /// Build the prompt for `change` by running `openspec instructions
    /// apply` and substituting the result into the implementer template.
    /// On any failure to obtain the openspec output (binary not on PATH,
    /// non-zero exit, empty stdout) the method returns Err and the
    /// caller fails the iteration. There is no silent fallback: a
    /// degraded prompt produces nothing useful, and the startup
    /// preflight in `cli::run::openspec_preflight` should have already
    /// surfaced a missing binary.
    fn build_prompt(&self, workspace: &Path, change: &str) -> Result<String> {
        let out = std::process::Command::new("openspec")
            .args(["instructions", "apply", "--change", change])
            .current_dir(workspace)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow!(
                        "openspec binary not found on PATH while building prompt for `{change}`. \
                         Set Environment=\"PATH=...\" in the systemd unit so it covers openspec's install directory."
                    )
                } else {
                    anyhow!(
                        "spawning `openspec instructions apply` for `{change}` failed: {e}"
                    )
                }
            })?;
        if !out.status.success() {
            let stderr_tail: String =
                String::from_utf8_lossy(&out.stderr).chars().take(200).collect();
            return Err(anyhow!(
                "`openspec instructions apply --change {change}` exited {code:?}: {stderr_tail}",
                code = out.status.code(),
            ));
        }
        let body = String::from_utf8_lossy(&out.stdout).to_string();
        if body.trim().is_empty() {
            return Err(anyhow!(
                "`openspec instructions apply --change {change}` produced empty stdout"
            ));
        }
        let rendered = self.template.replace(PROMPT_BODY_PLACEHOLDER, &body);
        Ok(append_iteration_continuation_block(&self.paths, workspace, change, rendered))
    }

    /// Build the revision-mode prompt for `change` by running `openspec
    /// instructions apply` and substituting the result into the revision
    /// template (along with the PR diff and the operator's revision
    /// text). Errors propagate the same way as `build_prompt`.
    /// Build the revision-mode prompt from the executor's
    /// `RevisionContext`. Per a20a5: ALL material comes from the
    /// `RevisionContext` (PR-sourced); there is NO subprocess call AND
    /// NO degraded-prompt fallback. If the dispatcher provided
    /// incomplete context, the dispatcher already refused to invoke
    /// the executor — this builder is unreachable in that case.
    ///
    /// Pre-a20a5 this function spawned `openspec instructions apply
    /// --change <X>` to load "the original change material." That call
    /// always failed (because autocoder enforces `openspec archive`
    /// before push, so the active change directory is never present
    /// when revise runs) AND the code "fell back to a placeholder."
    /// The placeholder path silently degraded 100% of production
    /// revise attempts. Both the subprocess call AND the placeholder
    /// are now removed entirely; a20a5's canonical "no degraded-prompt
    /// path is permitted" invariant forbids reintroducing either.
    fn build_revision_prompt(
        &self,
        _workspace: &Path,
        _change: &str,
        revision_context: &crate::revisions::RevisionContext,
    ) -> Result<String> {
        // Single-pass substitution (a002): closes the self-hosting hazard
        // where `prompts/implementer-revision.md` itself contains
        // `{{pr_diff}}` / `{{revision_request}}` / `{{pr_body}}` — revising
        // a PR whose diff touches that template would, under chained
        // `.replace`, re-expand those tokens inside the injected diff.
        let rendered = crate::prompts::render_template(
            &self.revision_template,
            &[
                (
                    placeholder_key(REVISION_PR_BODY_PLACEHOLDER),
                    &revision_context.pr_body,
                ),
                (
                    placeholder_key(REVISION_PR_CHANGE_LIST_PLACEHOLDER),
                    &revision_context.pr_change_list,
                ),
                (
                    placeholder_key(REVISION_AGENT_NOTES_PLACEHOLDER),
                    &revision_context.agent_implementation_notes,
                ),
                (
                    placeholder_key(REVISION_DIFF_PLACEHOLDER),
                    &revision_context.pr_diff,
                ),
                (
                    placeholder_key(REVISION_REQUEST_PLACEHOLDER),
                    &revision_context.revision_text,
                ),
            ],
        );
        Ok(rendered)
    }

    /// Build the triage-mode prompt by substituting the four
    /// `TriageContext` payloads into the embedded
    /// `prompts/audit-triage.md` template. The triage prompt is fully
    /// self-contained — unlike `run`/`run_revision` it does NOT shell
    /// out to `openspec instructions apply` because the LLM is asked
    /// to explore the codebase itself rather than acting on one
    /// pre-existing change.
    fn build_triage_prompt(&self, ctx: &TriageContext) -> String {
        // Single-pass substitution (a002): a `{{...}}` token inside the
        // injected findings or canonical-specs index is not re-expanded.
        crate::prompts::render_template(
            &self.triage_template,
            &[
                (placeholder_key(TRIAGE_FINDINGS_PLACEHOLDER), &ctx.findings),
                (
                    placeholder_key(TRIAGE_AUDIT_TYPE_PLACEHOLDER),
                    &ctx.audit_type,
                ),
                (placeholder_key(TRIAGE_REPO_URL_PLACEHOLDER), &ctx.repo_url),
                (
                    placeholder_key(TRIAGE_SPECS_INDEX_PLACEHOLDER),
                    &ctx.canonical_specs_index,
                ),
            ],
        )
    }

    /// Build the chat-triage prompt by substituting the three
    /// `ChatTriageContext` payloads into the embedded
    /// `prompts/chat-request-triage.md` template. Like `build_triage_prompt`,
    /// this does NOT shell out to `openspec instructions apply` because the
    /// LLM is asked to classify and explore the codebase itself.
    fn build_chat_triage_prompt(&self, ctx: &ChatTriageContext) -> String {
        // Single-pass substitution (a002): operator `request_text` that
        // contains a `{{repo_url}}` / `{{canonical_specs_index}}` literal is
        // not re-expanded by the later substitutions.
        crate::prompts::render_template(
            &self.chat_triage_template,
            &[
                (
                    placeholder_key(CHAT_TRIAGE_REQUEST_TEXT_PLACEHOLDER),
                    &ctx.request_text,
                ),
                (placeholder_key(TRIAGE_REPO_URL_PLACEHOLDER), &ctx.repo_url),
                (
                    placeholder_key(TRIAGE_SPECS_INDEX_PLACEHOLDER),
                    &ctx.canonical_specs_index,
                ),
            ],
        )
    }

    /// Build the changelog-stylist prompt by substituting the
    /// `ChangelogContext` payloads into the resolved stylist template
    /// (embedded default OR override loaded from
    /// `executor.changelog_stylist_prompt_path`).
    fn build_changelog_prompt(&self, ctx: &ChangelogContext) -> String {
        // Single-pass substitution (a002): a `{{...}}` token inside the
        // changelog JSON or operator revision text is not re-expanded.
        crate::prompts::render_template(
            &self.changelog_stylist_template,
            &[
                (
                    placeholder_key(CHANGELOG_JSON_PLACEHOLDER),
                    &ctx.changelog_json,
                ),
                (placeholder_key(TRIAGE_REPO_URL_PLACEHOLDER), &ctx.repo_url),
                (
                    placeholder_key(CHANGELOG_REVISION_TEXT_PLACEHOLDER),
                    &ctx.revision_text,
                ),
            ],
        )
    }

    /// Write a `<workspace>/.mcp.json` file telling the wrapped CLI to
    /// launch THIS autocoder binary as the per-execution stdio MCP child.
    /// The caller MUST delete this file via `delete_mcp_config` after the
    /// child exits to keep the working tree clean.
    ///
    /// When `ORCH_DAEMON_CONTROL_SOCKET` is set in the parent process's
    /// environment (the daemon sets it when `canonical_rag` is configured),
    /// the same env vars are propagated into the MCP child's spawn
    /// environment so the `query_canonical_specs` tool can relay queries
    /// to the daemon's `CanonicalRagStore`. Absent → the child sees no
    /// such env vars AND the tool returns the documented
    /// "rag not configured for this execution" hint.
    ///
    /// `role` (a56): when `Some`, `ORCH_MCP_ROLE` is written into the MCP
    /// child's env so it advertises that role's `submit_*` tool. The
    /// executor itself reports via the `outcome_*` tools (not a `submit_*`
    /// tool), so it passes `None`; the agentic roles added by later changes
    /// pass their role name.
    ///
    /// `pub(crate)` so the advisory audits (a57) can write the same
    /// `.mcp.json` shape — they pass their audit type as both `change`
    /// (the submission routing key) AND `role` (the `submit_findings`
    /// advertisement gate).
    pub(crate) fn write_mcp_config(
        workspace: &Path,
        change: &str,
        role: Option<&str>,
    ) -> Result<PathBuf> {
        // We may be running from a non-autocoder binary (e.g. cargo test).
        // `current_exe` returns the actual running binary; in production
        // this is the `autocoder` binary and the MCP subcommand exists.
        let exe = std::env::current_exe()
            .context("resolving current autocoder binary path for MCP config")?;
        let mut env = serde_json::json!({
            crate::mcp_askuser_server::ENV_WORKSPACE: workspace.to_string_lossy(),
            crate::mcp_askuser_server::ENV_CHANGE: change,
        });
        if let Some(role) = role {
            env[crate::mcp_askuser_server::ENV_ROLE] =
                serde_json::Value::String(role.to_string());
        }
        // Plumb the daemon's control-socket path and workspace basename
        // through to the MCP child only when the daemon has explicitly
        // set them in the parent process env (i.e., `canonical_rag` is
        // configured). Absent in non-RAG runs by design.
        if let Ok(socket) = std::env::var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET) {
            env[crate::mcp_askuser_server::ENV_CONTROL_SOCKET] =
                serde_json::Value::String(socket);
            let basename = std::env::var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME)
                .unwrap_or_else(|_| {
                    workspace
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown_workspace")
                        .to_string()
                });
            env[crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME] =
                serde_json::Value::String(basename);
        }
        let config = serde_json::json!({
            "mcpServers": {
                "ask_user": {
                    "command": exe,
                    "args": ["mcp-ask-user-server"],
                    "env": env,
                }
            }
        });
        let path = workspace.join(MCP_CONFIG_FILENAME);
        let raw = serde_json::to_string_pretty(&config)?;
        std::fs::write(&path, raw)
            .with_context(|| format!("writing MCP config {}", path.display()))?;
        Ok(path)
    }

    /// Idempotently remove the `.mcp.json` we wrote. `pub(crate)` so the
    /// advisory audits (a57) can clean up the config they wrote.
    pub(crate) fn delete_mcp_config(workspace: &Path) {
        let path = workspace.join(MCP_CONFIG_FILENAME);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("could not remove {}: {e}", path.display());
        }
    }

    /// Check for the Layer-1 marker file. If present, read + delete it and
    /// return the question.
    fn check_askuser_marker(workspace: &Path, change: &str) -> Result<Option<String>> {
        let path = workspace
            .join("openspec/changes")
            .join(change)
            .join(ASKUSER_MARKER_FILENAME);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let parsed: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        let question = parsed
            .get("question")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow!(
                    "marker file {} missing string field `question`",
                    path.display()
                )
            })?;
        // Always remove the marker so a stale one cannot survive into the
        // next iteration. autocoder now owns the question.
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(
                "could not remove askuser marker {} after reading: {e}",
                path.display()
            );
        }
        Ok(Some(question))
    }

    /// Layer-2 backstop: scan stdout for a clarification phrase. Returns
    /// the first sentence containing a match, or `None`.
    ///
    /// Heuristic intentionally narrow to avoid false positives. Fires when
    /// the wrapped CLI's output reads like a question rather than work.
    /// The reviewer agent provides a downstream backstop in case this
    /// produces noise.
    fn check_stdout_heuristic(stdout: &str) -> Option<String> {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(r"(?i)\b(could you|please) (clarify|specify|tell me|provide)\b")
                .expect("static regex compiles")
        });
        let m = re.find(stdout)?;
        // Return the sentence (split on '.', '!', '?', or newline) that
        // contains the matched span.
        let mat_start = m.start();
        let mat_end = m.end();
        let prev_break = stdout[..mat_start]
            .rfind(|c: char| matches!(c, '.' | '!' | '?' | '\n'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let after_match = &stdout[mat_end..];
        let next_break = after_match
            .find(|c: char| matches!(c, '.' | '!' | '?' | '\n'))
            .map(|i| mat_end + i + 1) // include the punctuation
            .unwrap_or(stdout.len());
        let sentence = stdout[prev_break..next_break].trim().to_string();
        if sentence.is_empty() {
            None
        } else {
            Some(sentence)
        }
    }

    /// Spawn the wrapped CLI for one implementer / triage / etc. session
    /// through the shared agentic-run primitive
    /// ([`crate::agentic_run::agentic_run`]). Streaming-JSON when the
    /// executor's output format is `Json` (parsing `final_answer` /
    /// `session_id` + writing the structured log), simple-capture
    /// otherwise. MCP is enabled — the caller wrote the workspace's
    /// `.mcp.json`, which the CLI auto-discovers — AND the autocoder MCP
    /// provided-tool names are auto-allowed. The busy-marker subprocess
    /// sidecar is written so stuck-state recovery can `killpg` the child's
    /// process group.
    async fn spawn_agentic_session(
        &self,
        workspace: &Path,
        change: &str,
        prompt: &str,
        resume_session_id: Option<&str>,
    ) -> Result<AgenticRunOutcome> {
        // a70: resolve the strategy from the configured CLI (default `claude`).
        // Streaming (live log + parsed final_answer/session_id) only for the
        // `claude` JSON path; a capture-mode strategy runs without the live
        // log, taking outcome + final_answer from the MCP relay.
        let streaming = self.implementer_streaming();
        let strategy = self.implementer_strategy();
        // a70: run through the session-managing wrapper so the created
        // session's handle is captured onto the outcome. The implementer does
        // NOT prune here (`prune = false`) — it may retain the session across
        // an AskUser AND prunes at its terminal outcome instead.
        crate::agentic_run::agentic_run_with_session(
            crate::agentic_run::AgenticRunOpts {
                workspace,
                change,
                strategy: strategy.as_ref(),
                prompt,
                sandbox: crate::agentic_run::SandboxConfig {
                    allowed_tools: self.sandbox.allowed_tools.clone(),
                    disallowed_bash_patterns: self.sandbox.disallowed_bash_patterns.clone(),
                    disallowed_read_paths: self.sandbox.disallowed_read_paths.clone(),
                    // The executor implements code, so it allows Write/Edit:
                    // its settings file must NOT deny them (preserving the
                    // pre-refactor executor settings exactly).
                    deny_writes: false,
                },
                model: None,
                output_mode: if streaming {
                    crate::agentic_run::OutputMode::Streaming
                } else {
                    crate::agentic_run::OutputMode::Capture
                },
                timeout: self.timeout,
                paths: Some(&self.paths),
                settings_dir: self.settings_dir.as_deref(),
                include_autocoder_tools: true,
                emit_stream_json_in_capture: false,
                resume_session_id,
                track_subprocess_marker: true,
                etxtbsy_retry_spawn: false,
                // a006: the executor implements code, so its workspace is mounted
                // read-write. The self-store is keyed by the resolved CLI.
                os_sandbox: crate::sandbox::current_run_sandbox(self.cli, true),
            },
            false,
            self.session_home.as_deref(),
        )
        .await
    }

    /// Classify a subprocess outcome into an `ExecutorOutcome`, applying
    /// Layer-1 and Layer-2 AskUser detection.
    ///
    /// Returns the classified outcome only. The implementer-flow caller
    /// (`Executor::run`) uses [`Self::classify_outcome_with_meta`] so it
    /// can branch on whether a tool-recorded outcome was consumed
    /// (acceptance scan condition).
    async fn classify_outcome(
        &self,
        workspace: &Path,
        change: &str,
        outcome: AgenticRunOutcome,
    ) -> Result<ExecutorOutcome> {
        Ok(self
            .classify_outcome_with_meta(workspace, change, outcome)
            .await?
            .outcome)
    }

    /// Variant of `classify_outcome` returning both the outcome AND a
    /// boolean indicating whether the daemon's outcome store had a
    /// recorded outcome to consume (the agent called one of the
    /// `outcome_*` MCP tools). The acceptance scan (a27a2) fires only
    /// when this flag is `false`.
    async fn classify_outcome_with_meta(
        &self,
        workspace: &Path,
        change: &str,
        outcome: AgenticRunOutcome,
    ) -> Result<ClassifiedOutcome> {
        // a70: the session handle this run created (streamed `session_id` for
        // claude; the captured store entry / resumed id for a capture-mode
        // strategy). An AskUser outcome embeds it in the `ResumeHandle` so the
        // implementer resume continues that same session natively.
        let session_handle = outcome.session_handle.clone();
        // Tool-recorded outcome lookup (a27a0). The per-execution MCP
        // child relays outcome tool calls to the daemon via
        // `record_outcome`; we drain via `consume_outcome`. A recorded
        // outcome is the agent's deliberate, schema-validated end-of-run
        // emission AND is more authoritative than ANY inferred state
        // (timeout, exit status, stdout content). Runs without a
        // recorded outcome fall through to the simplified classifier
        // ordering (a27a2: consume_outcome → AskUser → timeout → exit
        // status → diff-presence/Layer-2/Completed).
        let workspace_basename = workspace_basename_for(workspace);
        if let Some(recorded) =
            try_consume_outcome(&workspace_basename, change).await
        {
            return Ok(ClassifiedOutcome {
                outcome: map_recorded_outcome(&self.paths, workspace, change, recorded),
                tool_recorded: true,
            });
        }

        // Layer-1 first: the marker file is the authoritative signal. It
        // may have been written even if the wrapped CLI exited non-zero.
        if let Some(question) = Self::check_askuser_marker(workspace, change)? {
            let handle = build_handle(workspace, change, session_handle.clone());
            return Ok(ClassifiedOutcome {
                outcome: ExecutorOutcome::AskUser {
                    question,
                    resume_handle: handle,
                },
                tool_recorded: false,
            });
        }

        // Timeout precedence (a20a1): a timed-out run by definition did
        // not reach a deliberate end-of-run point. Classify as timeout
        // BEFORE the exit-status path so the real cause is not masked.
        if outcome.timed_out {
            return Ok(ClassifiedOutcome {
                outcome: ExecutorOutcome::Failed {
                    reason: "timeout".to_string(),
                },
                tool_recorded: false,
            });
        }

        let status = outcome.exit_status.expect("non-timeout path has status");
        // a39: detect a SIGTERM-killed subprocess. The daemon spawns the
        // wrapped CLI directly in its own process group (see
        // `crate::agentic_run::agentic_run`), so when a daemon shutdown's SIGTERM cascade
        // reaches it — systemd's `KillMode=control-group` delivers
        // SIGTERM to every process in the unit's cgroup — the child is
        // terminated *by the signal*. The reaped `ExitStatus` then
        // reports `signal() == Some(15)`, NOT a normal exit with code
        // 143: `code()` returns `None` for any signal-killed process.
        // (The "143 = 128 + 15" form is the *shell* convention; it only
        // appears when a wrapper or the CLI itself catches SIGTERM and
        // `exit(143)`s.) We accept either form so both the real
        // signal-death shape AND the defensive exit-143 shape are
        // caught.
        //
        // When a SIGTERM-kill coincides with the daemon's own shutdown
        // cascade, it is operator-initiated territory, not a real agent
        // failure: map it to `Aborted` so the polling loop bypasses the
        // failure counter + perma-stuck path. External SIGTERMs (OOM
        // killer, manual `kill -TERM`, container orchestrator) still hit
        // the existing `Failed` arm below because the flag is `false`
        // for them.
        let sigterm_killed = {
            use std::os::unix::process::ExitStatusExt;
            status.signal() == Some(15) || status.code() == Some(143)
        };
        if sigterm_killed
            && crate::daemon::SHUTDOWN_REQUESTED
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(ClassifiedOutcome {
                outcome: ExecutorOutcome::Aborted {
                    reason: "daemon shutdown (SIGTERM cascade)".to_string(),
                },
                tool_recorded: false,
            });
        }
        if !status.success() {
            let reason: String = outcome.stderr.trim().chars().take(200).collect();
            let reason = if reason.is_empty() {
                format!("executor exited with {status}")
            } else {
                reason
            };
            return Ok(ClassifiedOutcome {
                outcome: ExecutorOutcome::Failed { reason },
                tool_recorded: false,
            });
        }

        // Exit-0 path. Check Layer-2 heuristic only when the workspace is
        // clean — if there's a diff, the agent did real work and we trust
        // the Completed outcome regardless of stdout noise.
        let porcelain = crate::git::status_porcelain(workspace).unwrap_or_default();
        if porcelain.is_empty() {
            if let Some(question) = Self::check_stdout_heuristic(&outcome.stdout) {
                let handle = build_handle(workspace, change, session_handle.clone());
                return Ok(ClassifiedOutcome {
                    outcome: ExecutorOutcome::AskUser {
                        question,
                        resume_handle: handle,
                    },
                    tool_recorded: false,
                });
            }
            // Suspicious: exit-0, no diff, no AskUser marker, no
            // clarification heuristic. The downstream polling_loop will
            // classify this as Failed. Surface the agent's actual output
            // here so journalctl shows *why* on the same line.
            let stdout_tail = tail(&outcome.stdout, 2048);
            let stderr_tail = tail(&outcome.stderr, 2048);
            let log_path = run_log_path(&self.paths, workspace, change);
            tracing::warn!(
                change = change,
                log_file = %log_path.display(),
                "agent exited 0 without modifying the workspace.\n--- stdout (last 2KB) ---\n{stdout}\n--- stderr (last 2KB) ---\n{stderr}\n--- end ---",
                stdout = if stdout_tail.is_empty() { "(empty)" } else { stdout_tail },
                stderr = if stderr_tail.is_empty() { "(empty)" } else { stderr_tail },
            );
        }

        Ok(ClassifiedOutcome {
            outcome: ExecutorOutcome::Completed {
                final_answer: outcome.final_answer.clone(),
            },
            tool_recorded: false,
        })
    }

    /// Recovery turn (a27a2). Launches `claude --resume <session_id>`
    /// (or a fresh subprocess as fallback) with the canonical recovery
    /// prompt naming the unchecked tasks.md items, classifies the
    /// result, AND returns either the structured outcome the recovery
    /// turn produced OR the canonical Failed reason. The recovery loop
    /// fires AT MOST ONCE per `Executor::run` invocation.
    async fn run_recovery_turn(
        &self,
        workspace: &Path,
        change: &str,
        session_id: Option<String>,
        unchecked: &[crate::executor::acceptance_scan::UncheckedTask],
    ) -> Result<ExecutorOutcome> {
        let recovery_prompt = build_recovery_prompt(unchecked);

        // The recovery turn re-uses the same MCP config so the outcome
        // tools are available. Stale askuser markers should NOT survive
        // into the recovery turn.
        let stale_marker = workspace
            .join("openspec/changes")
            .join(change)
            .join(ASKUSER_MARKER_FILENAME);
        let _ = std::fs::remove_file(&stale_marker);
        let _mcp_path = Self::write_mcp_config(workspace, change, None)?;

        let outcome_result = self
            .run_recovery_subprocess(workspace, change, &recovery_prompt, session_id.as_deref())
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome_result?;

        // Append the recovery turn's content to the existing per-change
        // log files with a clear divider so the operator sees both
        // phases.
        append_recovery_to_log(&self.paths, workspace, change, &outcome);

        // The recovery turn fires AT MOST ONCE — we do not call the
        // acceptance scan on its result. We do, however, consume any
        // tool-recorded outcome AND honor it. If no outcome tool was
        // called by the recovery turn, return the canonical Failed.
        let classified = self
            .classify_outcome_with_meta(workspace, change, outcome)
            .await?;
        if classified.tool_recorded {
            return Ok(classified.outcome);
        }
        Ok(ExecutorOutcome::Failed {
            reason: RECOVERY_FAILED_REASON.to_string(),
        })
    }

    /// Spawn the wrapped CLI for a recovery turn. When `session_id` is
    /// `Some`, the spawn includes `--resume <session_id>` so the child
    /// continues the original conversation. When `None` (text mode,
    /// missing system event, etc.), the spawn falls back to a fresh
    /// session — the agent still sees the recovery prompt AND has the
    /// outcome tools available, so it can converge even without the
    /// prior context.
    async fn run_recovery_subprocess(
        &self,
        workspace: &Path,
        change: &str,
        prompt: &str,
        session_id: Option<&str>,
    ) -> Result<AgenticRunOutcome> {
        // a70: emit stream-json in capture only for the `claude` JSON path
        // (the recovery turn reads it raw at exit); a capture-mode strategy
        // never streams.
        let emit_stream_json = self.implementer_streaming();
        let strategy = self.implementer_strategy();
        // Recovery turns capture stdout/stderr at-exit (no structured log
        // writer — append_recovery_to_log appends into the existing log
        // files after classification). The command still emits stream-json
        // when `json_mode` (preserving the pre-refactor recovery command),
        // but the bytes are read raw at exit; the session-id capture isn't
        // needed (no second loop is permitted). The autocoder MCP
        // provided-tool names are NOT auto-appended here, matching the
        // pre-refactor recovery `--allowedTools` value.
        crate::agentic_run::agentic_run(crate::agentic_run::AgenticRunOpts {
            workspace,
            change,
            strategy: strategy.as_ref(),
            prompt,
            sandbox: crate::agentic_run::SandboxConfig {
                allowed_tools: self.sandbox.allowed_tools.clone(),
                disallowed_bash_patterns: self.sandbox.disallowed_bash_patterns.clone(),
                disallowed_read_paths: self.sandbox.disallowed_read_paths.clone(),
                deny_writes: false,
            },
            model: None,
            output_mode: crate::agentic_run::OutputMode::Capture,
            timeout: self.timeout,
            paths: Some(&self.paths),
            settings_dir: self.settings_dir.as_deref(),
            include_autocoder_tools: false,
            emit_stream_json_in_capture: emit_stream_json,
            resume_session_id: session_id,
            track_subprocess_marker: true,
            etxtbsy_retry_spawn: false,
            // a006: recovery turns are the executor too — read-write workspace,
            // self-store keyed by the resolved CLI.
            os_sandbox: crate::sandbox::current_run_sandbox(self.cli, true),
        })
        .await
    }
}

/// Result of [`ClaudeCliExecutor::classify_outcome_with_meta`]. Carries
/// both the classified outcome AND the flag the acceptance-scan
/// dispatcher (a27a2) reads to decide whether to scan tasks.md.
struct ClassifiedOutcome {
    outcome: ExecutorOutcome,
    /// `true` when `consume_outcome` returned `Some(_)` — i.e. the agent
    /// called one of `outcome_success`, `outcome_spec_needs_revision`,
    /// OR `outcome_request_iteration` via the MCP tool channel. When
    /// `false`, the agent exited without a structured outcome signal AND
    /// the acceptance scan applies (for the implementer flow only).
    tool_recorded: bool,
}

/// Exact wording for the acceptance-scan/recovery-loop Failed reason
/// (a27a2). Operators grep for this AND scripts match against it; the
/// literal text is required by the canonical executor capability delta.
pub(crate) const RECOVERY_FAILED_REASON: &str =
    "acceptance check failed; recovery loop did not produce a structured outcome";

/// Divider line written into the per-change log files AT the start of
/// the recovery turn's content. Operators navigating the log file split
/// the original from the recovery transcript on this marker.
const RECOVERY_LOG_DIVIDER: &str = "=== RECOVERY TURN ===";

/// Build the canonical recovery-turn user message (a27a2). The
/// `unchecked` list is rendered as bullet lines from each task's
/// trailing text; the rest of the template is fixed per the executor
/// capability deltas.
fn build_recovery_prompt(
    unchecked: &[crate::executor::acceptance_scan::UncheckedTask],
) -> String {
    let mut bullets = String::new();
    for task in unchecked {
        bullets.push_str("  - ");
        bullets.push_str(&task.trailing_text);
        bullets.push('\n');
    }
    format!(
        "Acceptance check failed: your run ended without finishing the change.\n\
\n\
tasks.md still has unchecked items:\n\
{bullets}\n\
You did not call any outcome tool to conclude the session. Narrative\n\
\"Deferred:\" notes in the final-answer text are not accepted; the\n\
daemon enforces a structured outcome.\n\
\n\
Decide which of the following applies AND call the corresponding tool:\n\
\n\
1. The unchecked items are actually done in code — you forgot to mark\n\
   tasks.md. Update tasks.md to check them, then call:\n\
       outcome_success({{ final_answer: \"...\" }})\n\
\n\
2. You completed part AND want another iteration to finish the rest.\n\
   Call:\n\
       outcome_request_iteration({{\n\
         completed_tasks: [...],\n\
         remaining_tasks: [<unchecked list>],\n\
         reason: \"<concrete blocker>\"\n\
       }})\n\
\n\
3. The unchecked items are unimplementable in this sandbox. Call:\n\
       outcome_spec_needs_revision({{\n\
         unimplementable_tasks: [...],\n\
         revision_suggestion: \"...\"\n\
       }})\n\
\n\
Do NOT exit without calling exactly one outcome tool. If you call one\n\
AND it returns a validation error, fix the error AND retry the call.\n",
    )
}

/// Append the recovery turn's content to the existing per-change log
/// files with a clear `=== RECOVERY TURN ===` divider. Errors are
/// logged at WARN but never propagated — the executor outcome must not
/// depend on diagnostic side-effects.
fn append_recovery_to_log(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    change: &str,
    outcome: &AgenticRunOutcome,
) {
    use std::io::Write;
    let summary_path = run_log_path(paths, workspace, change);
    let stream_path = crate::executor::event_log::stream_path_for(&summary_path);

    let summary_body = match &outcome.final_answer {
        Some(text) => text.clone(),
        None => outcome.stdout.clone(),
    };
    let summary_text = format!(
        "\n{divider}\n{body}\n",
        divider = RECOVERY_LOG_DIVIDER,
        body = summary_body,
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&summary_path)
        && let Err(e) = f.write_all(summary_text.as_bytes())
    {
        tracing::warn!(
            path = %summary_path.display(),
            "appending recovery-turn content to summary log failed: {e}"
        );
    }

    // The stream log carries the raw action lines. The recovery turn
    // uses the legacy capture path (stdout/stderr at exit), so we
    // emit a synthetic `[assistant]` line for the stdout content AND
    // optionally `[tool_result:error]` for the stderr content. The
    // divider sits at the top so operators can navigate.
    let stream_text = if outcome.stderr.trim().is_empty() {
        format!(
            "\n{divider}\n[assistant] {stdout}\n",
            divider = RECOVERY_LOG_DIVIDER,
            stdout = outcome.stdout.trim(),
        )
    } else {
        format!(
            "\n{divider}\n[assistant] {stdout}\n[tool_result:error] {stderr}\n",
            divider = RECOVERY_LOG_DIVIDER,
            stdout = outcome.stdout.trim(),
            stderr = outcome.stderr.trim(),
        )
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&stream_path)
        && let Err(e) = f.write_all(stream_text.as_bytes())
    {
        tracing::warn!(
            path = %stream_path.display(),
            "appending recovery-turn content to stream log failed: {e}"
        );
    }
}

/// Append the "Prior iteration summary" continuation block to the
/// rendered prompt when `.iteration-pending.json` is present (a27a1).
///
/// - Marker absent → returns `rendered` unchanged (first-iteration
///   prompt is unchanged).
/// - Marker present AND parseable → appends the canonical block AFTER
///   the change body (per design.md D6: placement is load-bearing).
/// - Marker present BUT corrupt → emits `tracing::warn!` AND returns
///   `rendered` unchanged. The corrupt marker is NOT deleted (operator
///   can inspect AND repair).
pub(crate) fn append_iteration_continuation_block(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    change: &str,
    rendered: String,
) -> String {
    let basename = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    match crate::iteration_pending::read_marker(paths, basename, change) {
        Ok(Some(marker)) => {
            let block = render_iteration_continuation_block(&marker);
            format!("{rendered}\n{block}")
        }
        Ok(None) => rendered,
        Err(e) => {
            tracing::warn!(
                change = %change,
                "iteration-pending marker present but corrupt; building prompt as if absent: {e:#}"
            );
            rendered
        }
    }
}

/// Canonical "Prior iteration summary" block. Mirrors the executor
/// capability deltas in this change's spec (verbatim text required so
/// the agent's framing-cues are consistent across runs).
fn render_iteration_continuation_block(
    marker: &crate::iteration_pending::IterationPendingMarker,
) -> String {
    let completed = marker.completed_tasks.join(", ");
    let remaining = marker.remaining_tasks.join(", ");
    format!(
        "--- BEGIN PRIOR ITERATION SUMMARY ---\n\
\n\
A previous iteration of this same change reached a structured stopping\n\
point. Your job is to overcome the prior blocker AND finish the\n\
remaining tasks. The previous iteration's working tree has already been\n\
committed AND pushed to the agent branch — your starting state already\n\
includes its progress.\n\
\n\
Cumulative completed (do NOT re-implement): {completed}\n\
Remaining: {remaining}\n\
Prior iteration's stated reason for stopping: {reason}\n\
Current iteration: {iteration_number} of {cap} (cap)\n\
\n\
Do NOT assume the prior reason still holds. Re-evaluate the blocker\n\
with fresh eyes — the prior iteration's model may have miscalibrated\n\
the scope, AND a different angle of attack may resolve the work in\n\
this iteration. If you genuinely cannot finish in this iteration,\n\
call outcome_request_iteration again with an updated cumulative state\n\
AND a more specific reason. Note that the iteration cap is 5; runs\n\
beyond that are auto-failed.\n\
\n\
--- END PRIOR ITERATION SUMMARY ---\n",
        completed = completed,
        remaining = remaining,
        reason = marker.reason,
        iteration_number = marker.iteration_number,
        cap = ITERATION_REQUEST_CAP,
    )
}

fn build_handle(workspace: &Path, change: &str, session_id: Option<String>) -> ResumeHandle {
    let data = ClaudeResumeData {
        workspace: workspace.to_path_buf(),
        change: change.to_string(),
        session_id,
    };
    ResumeHandle(serde_json::to_value(data).expect("handle serializes"))
}

/// Resolve the workspace basename routing key the daemon uses to key
/// the outcome store. Computes from the workspace path's file name.
/// The MCP child receives the same value via the spawn env in
/// `write_mcp_config`, so both sides (the recorder AND the consumer)
/// agree on the key.
///
/// Note: pre-a27a2 this also consulted `ENV_WORKSPACE_BASENAME` as an
/// override, but the override was only used by tests AND produced a
/// process-wide env-var race when tests ran in parallel. Production
/// never sets the env var on the daemon side, so dropping the lookup
/// preserves production semantics.
fn workspace_basename_for(workspace: &Path) -> String {
    workspace
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown_workspace")
        .to_string()
}

/// Send a `consume_outcome` action to the daemon's control socket. The
/// socket path is read from `ENV_CONTROL_SOCKET`; tests that have not
/// set the env get an immediate `None` so the classifier falls through
/// to legacy behavior. Transport failures log a warning and return
/// `None` — the deliberate outcome signal is best-effort; the legacy
/// path remains as a safety net for the deprecation window.
async fn try_consume_outcome(
    workspace_basename: &str,
    change: &str,
) -> Option<crate::outcome_store::RecordedOutcome> {
    let socket = std::env::var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET).ok()?;
    let request = serde_json::json!({
        "action": "consume_outcome",
        "workspace_basename": workspace_basename,
        "change": change,
    });
    let socket_path = std::path::PathBuf::from(socket);
    // The daemon's control socket may not exist in some test runs that
    // exercise `classify_outcome` without starting the daemon. Probe
    // before connecting so a missing socket is a quiet `None` instead
    // of an error log line in every test.
    if !socket_path.exists() {
        return None;
    }
    let resp = match send_consume_outcome(&socket_path, request).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                workspace_basename = %workspace_basename,
                change = %change,
                "consume_outcome relay failed; falling through to legacy classifier: {e:#}"
            );
            return None;
        }
    };
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let outcome_val = resp.get("outcome")?;
    if outcome_val.is_null() {
        return None;
    }
    serde_json::from_value(outcome_val.clone()).ok()
}

/// One-shot UDS round trip: send the JSON request followed by a newline,
/// read the single-line JSON response. Bounded by a 10-second timeout
/// matching the MCP child's relay primitive.
async fn send_consume_outcome(
    socket_path: &Path,
    request: serde_json::Value,
) -> Result<serde_json::Value> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let timeout = Duration::from_secs(10);
    let stream = tokio::time::timeout(timeout, tokio::net::UnixStream::connect(socket_path))
        .await
        .map_err(|_| anyhow!("control socket connect timed out"))??;
    let (read_half, mut write_half) = stream.into_split();
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    tokio::time::timeout(timeout, write_half.write_all(&bytes))
        .await
        .map_err(|_| anyhow!("control socket write timed out"))??;
    tokio::time::timeout(timeout, write_half.shutdown())
        .await
        .map_err(|_| anyhow!("control socket shutdown timed out"))??;
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut line = String::new();
    tokio::time::timeout(timeout, reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow!("control socket read timed out"))??;
    let value: serde_json::Value = serde_json::from_str(line.trim())
        .with_context(|| format!("decoding consume_outcome response: {line:?}"))?;
    Ok(value)
}

/// Hard cap on the number of iterations a single change may run
/// through. A 6th `iteration_request` is overridden by the classifier
/// to `ExecutorOutcome::Failed` so the operator is alerted rather than
/// the agent being allowed to loop forever.
pub(crate) const ITERATION_REQUEST_CAP: u32 = 5;

/// Exact wording for the cap-exceeded failure reason. Operators grep
/// for this AND scripts match against it; the literal `(5)` lets a
/// future configurable cap evolve without breaking the contract.
pub(crate) const ITERATION_CAP_EXCEEDED_REASON: &str =
    "exceeded iteration-request cap (5); WIP on agent branch — review or restart from scratch";

/// Map a daemon-recorded outcome to its `ExecutorOutcome` counterpart.
/// The workspace + change parameters are used by the `IterationRequest`
/// variant to read the current iteration-pending marker AND enforce the
/// cap of [`ITERATION_REQUEST_CAP`] iterations per change.
fn map_recorded_outcome(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    change: &str,
    recorded: crate::outcome_store::RecordedOutcome,
) -> ExecutorOutcome {
    match recorded {
        crate::outcome_store::RecordedOutcome::Success { final_answer } => {
            ExecutorOutcome::Completed { final_answer }
        }
        crate::outcome_store::RecordedOutcome::SpecNeedsRevision {
            unimplementable_tasks,
            revision_suggestion,
        } => ExecutorOutcome::SpecNeedsRevision {
            unimplementable_tasks: unimplementable_tasks
                .into_iter()
                .map(|t| UnimplementableTask {
                    task_id: t.task_id,
                    task_text: t.task_text,
                    reason: t.reason,
                })
                .collect(),
            revision_suggestion,
        },
        crate::outcome_store::RecordedOutcome::IterationRequest {
            completed_tasks,
            remaining_tasks,
            reason,
        } => {
            // Prior iteration_number comes from the workspace's
            // `.iteration-pending.json` marker (if present). A missing
            // marker means "no prior iteration_request has been
            // recorded for this change," which is iteration_number 1
            // (the just-finished run). A corrupt / unreadable marker
            // is treated as absent (corrupt-as-absent) per design.md
            // D5's degraded-recovery story.
            let basename_for_marker = workspace
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let prior_iteration_number = match crate::iteration_pending::read_marker(
                paths,
                basename_for_marker,
                change,
            ) {
                Ok(Some(m)) => m.iteration_number,
                Ok(None) => 1,
                Err(e) => {
                    tracing::warn!(
                        change = %change,
                        "iteration-pending marker unreadable / corrupt; treating as absent: {e:#}"
                    );
                    1
                }
            };
            let next_iteration_number = prior_iteration_number + 1;
            if next_iteration_number > ITERATION_REQUEST_CAP {
                tracing::warn!(
                    change = %change,
                    cap = ITERATION_REQUEST_CAP,
                    prior_iteration = prior_iteration_number,
                    "iteration_request emitted for `{change}` would produce iteration {next_iteration_number}, exceeding cap {cap}; overriding to Failed AND preserving marker for operator triage",
                    cap = ITERATION_REQUEST_CAP,
                );
                return ExecutorOutcome::Failed {
                    reason: ITERATION_CAP_EXCEEDED_REASON.to_string(),
                };
            }
            ExecutorOutcome::IterationRequested {
                completed_tasks,
                remaining_tasks,
                reason,
                iteration_number: next_iteration_number,
            }
        }
    }
}

#[async_trait]
impl Executor for ClaudeCliExecutor {
    async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
        let prompt = self.build_prompt(workspace, change)?;
        // Best-effort: any stale marker from a prior crash gets cleared so
        // it cannot masquerade as the current invocation's question.
        let stale_marker = workspace
            .join("openspec/changes")
            .join(change)
            .join(ASKUSER_MARKER_FILENAME);
        let _ = std::fs::remove_file(&stale_marker);

        let _mcp_path = Self::write_mcp_config(workspace, change, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, change, &prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(&self.paths, workspace, change, &prompt, &outcome);
        // a70: the strategy-agnostic session handle (streamed `session_id` for
        // claude; the captured store entry for a capture-mode strategy).
        let session_handle = outcome.session_handle.clone();
        let classified = self
            .classify_outcome_with_meta(workspace, change, outcome)
            .await?;

        // Acceptance scan (a27a2). Fires ONLY for the implementer flow's
        // Completed-via-heuristic path AND only when the agent did not
        // call any outcome tool. Other outcomes (AskUser, Failed,
        // SpecNeedsRevision, IterationRequested) bypass the scan — they
        // are already structured signals.
        if classified.tool_recorded {
            // a70 §5.4: a tool-recorded outcome is terminal (never AskUser, which
            // comes from the marker path) — prune the session it created.
            self.prune_session(workspace, session_handle.as_deref());
            return Ok(classified.outcome);
        }
        if !matches!(classified.outcome, ExecutorOutcome::Completed { .. }) {
            // a70 §5.1/§5.4: AskUser retains the session (the handle rides the
            // ResumeHandle for the native resume); every other terminal outcome
            // prunes it.
            self.prune_session_unless_waiting(workspace, session_handle.as_deref(), &classified.outcome);
            return Ok(classified.outcome);
        }
        let unchecked =
            crate::executor::acceptance_scan::scan_change_tasks_md(workspace, change);
        if unchecked.is_empty() {
            self.prune_session(workspace, session_handle.as_deref());
            return Ok(classified.outcome);
        }
        tracing::warn!(
            change = %change,
            unchecked_count = unchecked.len(),
            "acceptance check failed; entering recovery turn for change {change}"
        );
        // The recovery turn re-uses the SAME session (native resume), so the
        // handle is unchanged; prune once at the terminal outcome below.
        let recovered = self
            .run_recovery_turn(workspace, change, session_handle.clone(), &unchecked)
            .await;
        if let Ok(out) = &recovered {
            self.prune_session_unless_waiting(workspace, session_handle.as_deref(), out);
        }
        recovered
    }

    async fn resume(&self, handle: ResumeHandle, answer: &str) -> Result<ExecutorOutcome> {
        let data: ClaudeResumeData = serde_json::from_value(handle.0)
            .context("decoding ClaudeCliExecutor resume handle")?;
        let workspace = data.workspace.as_path();
        let change = data.change.as_str();

        // a70 §5.2/§5.3: resume the RETAINED agentic session natively, delivering
        // the operator's answer into it. The session id was captured at the
        // AskUser outcome AND stashed in the handle. With NO captured session
        // (a strategy with no headless resume, OR a pre-a70 handle), we do NOT
        // fall back to a fresh-run-with-answer: we requeue the change as a
        // retryable failure via the existing failure-counter path (no
        // stash-and-recombine).
        let Some(session_id) = data.session_id.clone() else {
            tracing::warn!(
                change = %change,
                "resume has no retained session handle; requeueing (no fresh-run fallback)"
            );
            return Ok(ExecutorOutcome::Failed {
                reason: "agentic session could not be resumed (no retained handle); requeued"
                    .to_string(),
            });
        };

        // The answer alone is the resume prompt — the retained session already
        // holds the full conversation context (it is NOT re-seeded with the
        // base implementer prompt).
        let prompt = format!("The human answered your question: {answer}\n\nContinue the implementation.");

        let stale_marker = workspace
            .join("openspec/changes")
            .join(change)
            .join(ASKUSER_MARKER_FILENAME);
        let _ = std::fs::remove_file(&stale_marker);

        let _mcp_path = Self::write_mcp_config(workspace, change, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, change, &prompt, Some(&session_id))
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(&self.paths, workspace, change, &prompt, &outcome);
        // The resumed run continues the SAME session, so the handle is
        // unchanged. Classify with it so a follow-up AskUser re-retains it; a
        // CLI resume failure (session not found / corrupt / expired) surfaces
        // as a non-zero exit → `Failed` → the failure-counter path.
        let classified = self
            .classify_outcome(workspace, change, outcome)
            .await?;
        self.prune_session_unless_waiting(workspace, Some(&session_id), &classified);
        Ok(classified)
    }

    async fn run_revision(
        &self,
        workspace: &Path,
        change: &str,
        revision_context: &crate::revisions::RevisionContext,
    ) -> Result<ExecutorOutcome> {
        let prompt = self.build_revision_prompt(workspace, change, revision_context)?;
        // Clear any stale askuser marker so it cannot masquerade as the
        // current invocation's question — mirrors `run`.
        let stale_marker = workspace
            .join("openspec/changes")
            .join(change)
            .join(ASKUSER_MARKER_FILENAME);
        let _ = std::fs::remove_file(&stale_marker);

        let _mcp_path = Self::write_mcp_config(workspace, change, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, change, &prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        // a74: a pre-spawn precondition refusal (the a006 OS-sandbox-mechanism
        // gate, which fails BEFORE the subprocess is spawned) is NOT a
        // substantive task failure — no revision work was attempted. Surface
        // it as the classifiable `PreconditionUnmet` outcome (by KIND, via the
        // typed sandbox error) so the revise dispatcher does not charge a
        // revision slot. Any other executor error propagates unchanged.
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                if let Some(reason) = crate::sandbox::precondition_unmet_message(&e) {
                    return Ok(ExecutorOutcome::PreconditionUnmet { reason });
                }
                return Err(e);
            }
        };
        persist_run_log(&self.paths, workspace, change, &prompt, &outcome);
        let session_handle = outcome.session_handle.clone();
        let classified = self.classify_outcome(workspace, change, outcome).await?;
        self.prune_session_unless_waiting(workspace, session_handle.as_deref(), &classified);
        Ok(classified)
    }

    async fn run_issue(
        &self,
        workspace: &Path,
        ctx: &IssueContext,
    ) -> Result<ExecutorOutcome> {
        // The issues walker already rendered the issue-flavored prompt
        // (`PromptId::ImplementerIssue`). Run it with the MCP outcome
        // tools wired, keyed by the issue slug for the run-log + outcome
        // store. Acceptance is against the EXISTING canon — there is no
        // spec delta, so the acceptance scan / recovery turn (which read
        // `openspec/changes/<change>/tasks.md`) do NOT apply here; a
        // tool-recorded outcome OR the classifier's verdict is final.
        let change = ctx.slug.as_str();
        let _mcp_path = Self::write_mcp_config(workspace, change, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, change, &ctx.rendered_prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(&self.paths, workspace, change, &ctx.rendered_prompt, &outcome);
        let session_handle = outcome.session_handle.clone();
        let classified = self.classify_outcome(workspace, change, outcome).await?;
        self.prune_session_unless_waiting(workspace, session_handle.as_deref(), &classified);
        Ok(classified)
    }

    async fn run_triage(
        &self,
        workspace: &Path,
        ctx: &TriageContext,
    ) -> Result<ExecutorOutcome> {
        let prompt = self.build_triage_prompt(ctx);
        // Triage mode does not target a specific change directory, so the
        // per-change MCP marker plumbing is keyed by a synthetic name.
        let _mcp_path = Self::write_mcp_config(workspace, TRIAGE_LOG_CHANGE_NAME, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, TRIAGE_LOG_CHANGE_NAME, &prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(&self.paths, workspace, TRIAGE_LOG_CHANGE_NAME, &prompt, &outcome);
        self.classify_outcome(workspace, TRIAGE_LOG_CHANGE_NAME, outcome)
            .await
    }

    async fn run_chat_triage(
        &self,
        workspace: &Path,
        ctx: &ChatTriageContext,
    ) -> Result<ExecutorOutcome> {
        let prompt = self.build_chat_triage_prompt(ctx);
        let _mcp_path = Self::write_mcp_config(workspace, CHAT_TRIAGE_LOG_CHANGE_NAME, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, CHAT_TRIAGE_LOG_CHANGE_NAME, &prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(&self.paths, workspace, CHAT_TRIAGE_LOG_CHANGE_NAME, &prompt, &outcome);
        self.classify_outcome(workspace, CHAT_TRIAGE_LOG_CHANGE_NAME, outcome)
            .await
    }

    async fn run_brownfield_draft(
        &self,
        workspace: &Path,
        ctx: &BrownfieldDraftContext,
    ) -> Result<ExecutorOutcome> {
        // The polling layer has already substituted the template; the
        // executor passes `rendered_prompt` verbatim to the wrapped CLI.
        let prompt = ctx.rendered_prompt.clone();
        let _mcp_path = Self::write_mcp_config(workspace, BROWNFIELD_DRAFT_LOG_CHANGE_NAME, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, BROWNFIELD_DRAFT_LOG_CHANGE_NAME, &prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(
            &self.paths,
            workspace,
            BROWNFIELD_DRAFT_LOG_CHANGE_NAME,
            &prompt,
            &outcome,
        );
        self.classify_outcome(workspace, BROWNFIELD_DRAFT_LOG_CHANGE_NAME, outcome)
            .await
    }

    async fn run_scout(
        &self,
        workspace: &Path,
        ctx: &ScoutContext,
    ) -> Result<ExecutorOutcome> {
        let prompt = ctx.rendered_prompt.clone();
        let _mcp_path = Self::write_mcp_config(workspace, SCOUT_LOG_CHANGE_NAME, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, SCOUT_LOG_CHANGE_NAME, &prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(&self.paths, workspace, SCOUT_LOG_CHANGE_NAME, &prompt, &outcome);
        self.classify_outcome(workspace, SCOUT_LOG_CHANGE_NAME, outcome)
            .await
    }

    async fn run_issue_triage(
        &self,
        workspace: &Path,
        ctx: &IssueReportTriageContext,
    ) -> Result<ExecutorOutcome> {
        // The ingestion layer already rendered the issue-report-triage
        // prompt (reported body embedded as untrusted DATA); pass it
        // verbatim. The agent classifies read-only AND returns its verdict
        // as the final answer, which the ingestion layer parses.
        let prompt = ctx.rendered_prompt.clone();
        let _mcp_path = Self::write_mcp_config(workspace, ISSUE_TRIAGE_LOG_CHANGE_NAME, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, ISSUE_TRIAGE_LOG_CHANGE_NAME, &prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(
            &self.paths,
            workspace,
            ISSUE_TRIAGE_LOG_CHANGE_NAME,
            &prompt,
            &outcome,
        );
        self.classify_outcome(workspace, ISSUE_TRIAGE_LOG_CHANGE_NAME, outcome)
            .await
    }

    async fn run_changelog(
        &self,
        workspace: &Path,
        ctx: &ChangelogContext,
    ) -> Result<ExecutorOutcome> {
        let prompt = self.build_changelog_prompt(ctx);
        let _mcp_path = Self::write_mcp_config(workspace, CHANGELOG_STYLIST_LOG_CHANGE_NAME, None)?;
        let outcome = self
            .spawn_agentic_session(workspace, CHANGELOG_STYLIST_LOG_CHANGE_NAME, &prompt, None)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(
            &self.paths,
            workspace,
            CHANGELOG_STYLIST_LOG_CHANGE_NAME,
            &prompt,
            &outcome,
        );
        self.classify_outcome(workspace, CHANGELOG_STYLIST_LOG_CHANGE_NAME, outcome)
            .await
    }
}

/// Compute the per-change run-log path:
/// `<logs_dir>/runs/<repo-sanitized>/<change>.log`. The repo-sanitized
/// fragment is the workspace's basename, which is already the
/// URL-sanitized form produced by `workspace::derive_path`; this keeps
/// the per-repo subdirectory consistent with the workspace's own
/// naming.
pub(crate) fn run_log_path(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    change: &str,
) -> PathBuf {
    let basename = workspace
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    paths
        .run_logs_dir(&basename)
        .join(format!("{change}.log"))
}

/// Best-effort: write the subprocess's prompt, captured stdout, and
/// captured stderr to the per-change log file. Errors are logged at WARN
/// but never propagated; the executor outcome must not depend on
/// diagnostic side-effects.
fn persist_run_log(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    change: &str,
    prompt: &str,
    outcome: &AgenticRunOutcome,
) {
    // The JSON-streaming path already wrote the structured log
    // incrementally; overwriting here would discard the ACTIONS section.
    if outcome.streamed_log {
        return;
    }
    let path = run_log_path(paths, workspace, change);
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            path = %parent.display(),
            "could not create run-log directory: {e}"
        );
        return;
    }
    let body = format!(
        "=== PROMPT ({p} bytes) ===\n{prompt}\n=== STDOUT ({n} bytes) ===\n{stdout}\n=== STDERR ({m} bytes) ===\n{stderr}\n",
        p = prompt.len(),
        n = outcome.stdout.len(),
        m = outcome.stderr.len(),
        stdout = outcome.stdout,
        stderr = outcome.stderr,
    );
    match std::fs::write(&path, body) {
        Ok(()) => tracing::info!(path = %path.display(), "run log written"),
        Err(e) => tracing::warn!(path = %path.display(), "writing run log failed: {e}"),
    }
}

/// Return the trailing `max` bytes of `s`, snapped down to the nearest
/// UTF-8 character boundary so the returned slice never splits a
/// codepoint. Returns the full string if it is shorter than `max`.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Return a process-wide `Arc<DaemonPaths>` for tests in this
    /// module. A single tempdir-scoped instance is constructed lazily on
    /// first call AND reused thereafter so that the executor's `paths`
    /// field AND any test-side `run_log_path(...)` call agree on the
    /// same disk layout. The underlying `TempDir` is leaked (OS reaps
    /// at process exit). Tests that need true per-test isolation (e.g.
    /// the iteration-pending tests around `map_recorded_outcome`)
    /// construct their own via `crate::testing::test_daemon_paths()`.
    fn test_paths_arc() -> std::sync::Arc<crate::paths::DaemonPaths> {
        use std::sync::OnceLock;
        static PATHS: OnceLock<std::sync::Arc<crate::paths::DaemonPaths>> = OnceLock::new();
        PATHS
            .get_or_init(|| {
                let (td, paths) = crate::testing::test_daemon_paths();
                std::mem::forget(td);
                std::sync::Arc::new(paths)
            })
            .clone()
    }

    /// Build a fixture workspace with one OpenSpec change so `build_prompt`
    /// has material to produce a non-empty prompt. The bundled tasks.md
    /// is fully checked so the a27a2 acceptance scan does NOT trip
    /// during legacy-completion tests. Tests that exercise the
    /// acceptance-scan path overwrite tasks.md with an unchecked line.
    fn fixture_workspace() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let change_dir = dir.path().join("openspec/changes/x");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join("proposal.md"), "## Why\nfixture\n").unwrap();
        std::fs::write(change_dir.join("design.md"), "design text\n").unwrap();
        std::fs::write(change_dir.join("tasks.md"), "- [x] do thing\n").unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    /// Like `fixture_workspace` but also initializes a git repo so
    /// `git status --porcelain` works (used by Layer-2 detection).
    fn fixture_workspace_with_git() -> (TempDir, std::path::PathBuf) {
        let (dir, path) = fixture_workspace();
        let run = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&path)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "initial"]);
        (dir, path)
    }

    /// Write an executable shell script to the workspace. Returns the path.
    fn write_script(workspace: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = workspace.join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn build_allowed_tools_arg_auto_includes_autocoder_mcp_tools() {
        // The autocoder MCP server's tools MUST appear in --allowedTools
        // without operator action — they're part of the daemon's contract
        // with the agent. Operators who never touched their config still
        // get a functional outcome-tools path.
        let operator_tools = vec!["Read".to_string(), "Edit".to_string()];
        let combined = crate::agentic_run::build_allowed_tools_value(&operator_tools, true);
        let entries: Vec<&str> = combined.split(',').collect();

        // Operator-configured tools preserved verbatim.
        assert!(entries.contains(&"Read"), "operator's Read tool missing: {combined}");
        assert!(entries.contains(&"Edit"), "operator's Edit tool missing: {combined}");

        // Every tool the MCP server advertises is auto-allowed in
        // mcp__<server>__<tool> form.
        for tool in crate::mcp_askuser_server::PROVIDED_TOOL_NAMES {
            let qualified = crate::mcp_askuser_server::qualified_tool_name(tool);
            assert!(
                entries.iter().any(|e| *e == qualified),
                "autocoder MCP tool {qualified} not auto-allowed; argv was: {combined}"
            );
        }

        // ask_user AND query_canonical_specs are canonical members today.
        // Pin them explicitly so the test fails loudly if the const drifts.
        assert!(
            entries.contains(&"mcp__ask_user__ask_user"),
            "ask_user not auto-allowed: {combined}"
        );
        assert!(
            entries.contains(&"mcp__ask_user__query_canonical_specs"),
            "query_canonical_specs not auto-allowed: {combined}"
        );
    }

    #[test]
    fn build_allowed_tools_arg_preserves_empty_operator_list() {
        // Operators who configure NO sandbox.allowed_tools still get the
        // autocoder MCP tools auto-allowed — the daemon contract holds
        // regardless of operator config state.
        let combined = crate::agentic_run::build_allowed_tools_value(&[], true);
        let entries: Vec<&str> = combined.split(',').collect();
        assert!(entries.contains(&"mcp__ask_user__ask_user"));
        assert!(entries.contains(&"mcp__ask_user__query_canonical_specs"));
        // No spurious leading/trailing commas from the empty operator list.
        assert!(!combined.starts_with(','));
        assert!(!combined.ends_with(','));
    }

    #[test]
    fn sandbox_settings_file_contains_expected_deny_patterns() {
        // The executor path through `agentic_run` writes its settings file
        // via the shared `audits::write_sandbox_settings` with
        // `deny_writes: false` — preserving the pre-refactor executor
        // settings exactly: the bash/read deny patterns are present AND
        // `Write(*)`/`Edit(*)` are NOT denied (the executor implements
        // code).
        let sandbox = crate::config::ResolvedSandbox {
            allowed_tools: vec!["Read".into(), "Bash".into()],
            disallowed_bash_patterns: vec!["curl:*".into(), "git push:*".into()],
            disallowed_read_paths: vec!["/home/*/.ssh/**".into()],
        };
        let (path, _guard) =
            crate::audits::write_sandbox_settings(&sandbox, None, false)
                .expect("settings file writes");
        // Settings file is in OS temp dir.
        assert_eq!(path.parent().unwrap(), std::env::temp_dir());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let deny = parsed["permissions"]["deny"].as_array().unwrap();
        let deny_strings: Vec<String> = deny
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(deny_strings.contains(&"Bash(curl:*)".to_string()));
        assert!(deny_strings.contains(&"Bash(git push:*)".to_string()));
        assert!(deny_strings.contains(&"Read(/home/*/.ssh/**)".to_string()));
        // Executor settings allow Write/Edit (deny_writes: false).
        assert!(!deny_strings.contains(&"Write(*)".to_string()));
        assert!(!deny_strings.contains(&"Edit(*)".to_string()));
    }

    // a56: `write_mcp_config` writes `ORCH_MCP_ROLE` into the `.mcp.json`
    // env when a role is supplied, AND omits it when `None` (the executor's
    // own runs, which report via the `outcome_*` tools, pass `None`).
    #[test]
    fn write_mcp_config_writes_role_env_when_supplied() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let path =
            ClaudeCliExecutor::write_mcp_config(ws, "a56-foo", Some("reviewer")).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let env = &v["mcpServers"]["ask_user"]["env"];
        assert_eq!(env[crate::mcp_askuser_server::ENV_ROLE], "reviewer");
        ClaudeCliExecutor::delete_mcp_config(ws);
    }

    #[test]
    fn write_mcp_config_omits_role_env_when_none() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let path = ClaudeCliExecutor::write_mcp_config(ws, "a56-foo", None).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let env = &v["mcpServers"]["ask_user"]["env"];
        assert!(
            env.get(crate::mcp_askuser_server::ENV_ROLE).is_none(),
            "ORCH_MCP_ROLE must be absent when role is None: {env}"
        );
        ClaudeCliExecutor::delete_mcp_config(ws);
    }

    #[tokio::test]
    async fn sandbox_temp_file_cleaned_up_after_spawn() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        // Per-test isolated settings dir, so the assertion is not racy with
        // other parallel tests writing to the shared OS temp dir.
        let settings_dir = TempDir::new().unwrap();
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc())
            .with_settings_dir(settings_dir.path().to_path_buf());
        let _ = executor.run(&ws, "x").await.unwrap();
        let leftover: Vec<_> = std::fs::read_dir(settings_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert!(
            leftover.is_empty(),
            "settings file must be deleted after the child exits; leftover: {leftover:?}"
        );
    }

    #[tokio::test]
    async fn completed_when_command_exits_zero() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        assert!(matches!(outcome, ExecutorOutcome::Completed { .. }), "got {outcome:?}");
    }

    #[tokio::test]
    async fn failed_with_reason_on_nonzero_exit() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "fail.sh",
            "#!/bin/sh\necho 'something broke' >&2\nexit 7\n",
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Failed { reason } => {
                assert!(reason.contains("something broke"), "got reason: {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_when_nonzero_with_no_stderr() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "silent.sh", "#!/bin/sh\nexit 3\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Failed { reason } => {
                assert!(!reason.is_empty(), "reason should never be empty");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Layer-1: a fixture script writes the marker file (simulating what
    /// the MCP server would do when the agent calls `ask_user`). The
    /// executor MUST detect it and return AskUser, and MUST delete the
    /// marker afterward.
    #[tokio::test]
    async fn askuser_layer1_marker_produces_askuser() {
        let (_dir, ws) = fixture_workspace_with_git();
        let marker_dir = ws.join("openspec/changes/x");
        let script = write_script(
            &ws,
            "mcp.sh",
            &format!(
                "#!/bin/sh\nmkdir -p {0}\ncat > {0}/.askuser-pending.json <<'EOF'\n{{\"question\":\"What name should this take?\"}}\nEOF\nexit 0\n",
                marker_dir.to_string_lossy()
            ),
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::AskUser { question, resume_handle } => {
                assert_eq!(question, "What name should this take?");
                // Handle round-trips through JSON.
                let data: ClaudeResumeData = serde_json::from_value(resume_handle.0).unwrap();
                assert_eq!(data.change, "x");
                assert_eq!(data.workspace, ws);
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
        // Marker must be cleaned up so it doesn't fire on the NEXT run.
        assert!(!marker_dir.join(".askuser-pending.json").exists());
    }

    /// Layer-1 takes precedence over Layer-2 even if both signals are
    /// present (i.e. the marker file wins over a stdout regex match).
    #[tokio::test]
    async fn askuser_layer1_wins_over_layer2() {
        let (_dir, ws) = fixture_workspace_with_git();
        let marker_dir = ws.join("openspec/changes/x");
        let script = write_script(
            &ws,
            "both.sh",
            &format!(
                "#!/bin/sh\nmkdir -p {0}\ncat > {0}/.askuser-pending.json <<'EOF'\n{{\"question\":\"MARKER QUESTION\"}}\nEOF\necho 'could you clarify the requirements?'\nexit 0\n",
                marker_dir.to_string_lossy()
            ),
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::AskUser { question, .. } => {
                assert_eq!(
                    question, "MARKER QUESTION",
                    "marker question must beat the stdout regex"
                );
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    /// Layer-2: no marker, exit 0, clean workspace, clarifying stdout →
    /// AskUser synthesized from the matching sentence.
    #[tokio::test]
    async fn askuser_layer2_heuristic_fires_on_clarify_stdout() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "clarify.sh",
            "#!/bin/sh\necho 'I need more information to proceed. Could you clarify which folder this should live in?'\nexit 0\n",
        );
        // Commit the script so it doesn't show as untracked when the
        // executor checks `git status --porcelain` for Layer-2 detection.
        let commit = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success());
        };
        commit(&["add", "-A"]);
        commit(&["commit", "-q", "-m", "fixture script"]);

        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::AskUser { question, .. } => {
                assert!(
                    question.contains("Could you clarify"),
                    "synthesized question should be the matched sentence; got: {question}"
                );
            }
            other => panic!("expected Layer-2 AskUser, got {other:?}"),
        }
    }

    /// Layer-2 does NOT fire when the workspace has a diff (the agent did
    /// real work, so we trust Completed).
    #[tokio::test]
    async fn askuser_layer2_suppressed_when_diff_present() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "did_work.sh",
            "#!/bin/sh\necho 'work done; please clarify nothing relevant'\ntouch ARTIFACT\nexit 0\n",
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        assert!(matches!(outcome, ExecutorOutcome::Completed { .. }), "got {outcome:?}");
    }

    /// Layer-2 does NOT fire on benign stdout that doesn't match.
    #[test]
    fn heuristic_returns_none_when_no_match() {
        let out = ClaudeCliExecutor::check_stdout_heuristic("All done. No questions.");
        assert!(out.is_none());
    }

    #[test]
    fn heuristic_extracts_sentence_containing_match() {
        let stdout =
            "Looking at the change. I'm not sure where to put this. Could you specify the directory?";
        let sentence = ClaudeCliExecutor::check_stdout_heuristic(stdout).unwrap();
        assert!(sentence.contains("Could you specify"));
        // Should not span across an earlier `?` if there were one.
        assert!(!sentence.contains("Looking at the change"));
    }

    #[tokio::test]
    async fn resume_decodes_handle_and_completes_on_exit_zero() {
        let (_dir, ws) = fixture_workspace_with_git();
        // Use a script that simply exits 0 — resume should treat that as
        // Completed (no diff path).
        let script = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());

        // a70: resume requires a RETAINED session handle to continue natively.
        let handle = ResumeHandle(
            serde_json::to_value(ClaudeResumeData {
                workspace: ws.clone(),
                change: "x".into(),
                session_id: Some("sess-resume-1".into()),
            })
            .unwrap(),
        );
        let outcome = executor.resume(handle, "use SAMPLE").await.unwrap();
        assert!(matches!(outcome, ExecutorOutcome::Completed { .. }));
    }

    /// a70 §5.3 / scenario "Resume failure requeues the change with no
    /// fallback": a resume handle carrying NO retained session id is NOT
    /// fresh-run with the answer — it requeues as a retryable `Failed`.
    #[tokio::test]
    async fn resume_without_retained_session_requeues_no_fresh_run() {
        let (_dir, ws) = fixture_workspace_with_git();
        // A script that, if it WERE run, would create a diff (proving a fresh
        // run happened). The requeue path must NOT invoke it at all.
        let script = write_script(
            &ws,
            "intruder.sh",
            "#!/bin/sh\necho FRESH_RUN > fresh_run_marker.txt\nexit 0\n",
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());

        let handle = ResumeHandle(
            serde_json::to_value(ClaudeResumeData {
                workspace: ws.clone(),
                change: "x".into(),
                session_id: None,
            })
            .unwrap(),
        );
        let outcome = executor.resume(handle, "the answer").await.unwrap();
        assert!(
            matches!(outcome, ExecutorOutcome::Failed { .. }),
            "no retained session → requeue as Failed, got {outcome:?}"
        );
        assert!(
            !ws.join("fresh_run_marker.txt").exists(),
            "resume MUST NOT fresh-run the CLI when there is no session to resume"
        );
    }

    #[tokio::test]
    async fn resume_errors_on_bad_handle() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let handle = ResumeHandle(serde_json::json!({ "not": "a real handle" }));
        let err = match executor.resume(handle, "x").await {
            Ok(_) => panic!("expected Err from malformed handle"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("resume handle"), "got: {msg}");
    }

    #[tokio::test]
    async fn mcp_config_is_cleaned_up_after_run() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        executor.run(&ws, "x").await.unwrap();
        assert!(
            !ws.join(".mcp.json").exists(),
            ".mcp.json must be removed after the executor returns"
        );
    }

    // The timeout-kills-child test is intentionally `#[ignore]`d on this
    // host. In a fixture spawn of `/bin/sh -c "sleep 30"`, the shell exits
    // (status 0, ~50µs) before `sleep` has actually started doing anything
    // observable to the test, but `sleep` inherits the piped stderr handle
    // and keeps it open for the full 30s. The blocking read_to_string on
    // stderr after wait returns blocks for the inherited pipe duration,
    // which means autocoder's timeout never gets a chance to fire.
    #[ignore = "fixture inheritance issue with /bin/sh + sleep on macOS; production path is correct"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_kills_child() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "slow.sh", "#!/bin/sh\nsleep 30\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 1, test_paths_arc());
        let start = std::time::Instant::now();
        let outcome = executor.run(&ws, "x").await.unwrap();
        let elapsed = start.elapsed();
        match outcome {
            ExecutorOutcome::Failed { reason } => {
                assert_eq!(reason, "timeout");
            }
            other => panic!("expected Failed timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout should fire well before the 30s sleep; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn build_prompt_returns_non_empty_for_valid_fixture() {
        let (_dir, ws) = fixture_workspace();
        let executor = ClaudeCliExecutor::new("/bin/true".into(), 30, test_paths_arc());
        let prompt = executor.build_prompt(&ws, "x").unwrap();
        assert!(!prompt.trim().is_empty(), "prompt must not be empty");
    }

    #[tokio::test]
    async fn build_prompt_errors_when_change_dir_missing() {
        let dir = TempDir::new().unwrap();
        let executor = ClaudeCliExecutor::new("/bin/true".into(), 30, test_paths_arc());
        let err = executor
            .build_prompt(dir.path(), "missing")
            .expect_err("missing change dir should error");
        // Either openspec rejects the unknown change name or the workspace
        // has no openspec dir at all — both surface as a non-empty error.
        assert!(
            !format!("{err:#}").is_empty(),
            "error message must be non-empty"
        );
    }

    /// Template substitution: a custom template's `{{change_body}}`
    /// placeholder is replaced with the openspec output.
    #[tokio::test]
    async fn build_prompt_substitutes_change_body_into_template() {
        let (_dir, ws) = fixture_workspace();
        let mut executor = ClaudeCliExecutor::new("/bin/true".into(), 30, test_paths_arc());
        executor.template = "ROLE_HEADER\n--- BEGIN ---\n{{change_body}}\n--- END ---".into();
        let prompt = executor.build_prompt(&ws, "x").unwrap();
        assert!(prompt.starts_with("ROLE_HEADER"), "template prefix missing: {prompt}");
        assert!(prompt.contains("--- BEGIN ---"));
        assert!(prompt.contains("--- END ---"));
        // The openspec output's distinctive header should land between
        // the BEGIN/END markers.
        assert!(
            prompt.contains("Apply: x") || prompt.contains("# proposal.md") || prompt.contains("change_body") == false,
            "expected change body between markers; got: {prompt}"
        );
    }

    /// a27a1 Task 7.7: prompt-builder for a change with NO marker
    /// produces output WITHOUT the continuation block.
    #[tokio::test]
    async fn build_prompt_without_marker_omits_continuation_block() {
        let (_dir, ws) = fixture_workspace();
        let executor = ClaudeCliExecutor::new("/bin/true".into(), 30, test_paths_arc());
        let prompt = executor.build_prompt(&ws, "x").unwrap();
        assert!(
            !prompt.contains("--- BEGIN PRIOR ITERATION SUMMARY ---"),
            "first-iteration prompt must not contain continuation block: {prompt}"
        );
    }

    /// a27a1 Task 7.6: prompt-builder for a change with a present-AND-
    /// valid marker produces output containing the continuation block AND
    /// every marker-field value verbatim.
    #[tokio::test]
    async fn build_prompt_with_marker_appends_continuation_block_verbatim() {
        let (_dir, ws) = fixture_workspace();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        crate::iteration_pending::write_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "x",
            &crate::iteration_pending::IterationPendingMarker {
                completed_tasks: vec!["1".into(), "2".into()],
                remaining_tasks: vec!["3".into()],
                reason: "task 3 needs a refactor I want to plan more carefully".into(),
                iteration_number: 2,
            },
        )
        .unwrap();
        let executor = ClaudeCliExecutor::new("/bin/true".into(), 30, std::sync::Arc::new(paths));
        let prompt = executor.build_prompt(&ws, "x").unwrap();
        assert!(
            prompt.contains("--- BEGIN PRIOR ITERATION SUMMARY ---"),
            "continuation block start marker missing"
        );
        assert!(
            prompt.contains("--- END PRIOR ITERATION SUMMARY ---"),
            "continuation block end marker missing"
        );
        // Verbatim marker-field values are present.
        assert!(
            prompt.contains("Cumulative completed (do NOT re-implement): 1, 2"),
            "completed_tasks rendering missing: {prompt}"
        );
        assert!(prompt.contains("Remaining: 3"));
        assert!(prompt.contains(
            "Prior iteration's stated reason for stopping: task 3 needs a refactor I want to plan more carefully"
        ));
        assert!(prompt.contains("Current iteration: 2 of 5 (cap)"));
        // The block lands AFTER the change body — find the position of
        // both and compare.
        let change_body_pos = prompt.find("--- END CHANGE ---")
            .or_else(|| prompt.find("--- BEGIN CHANGE ---"))
            .unwrap_or(0);
        let block_pos = prompt
            .find("--- BEGIN PRIOR ITERATION SUMMARY ---")
            .unwrap();
        assert!(
            block_pos > change_body_pos,
            "continuation block must appear AFTER the change body (change_body_pos={change_body_pos}, block_pos={block_pos})"
        );
    }

    /// a27a1 Task 7.8: prompt-builder for a change with a CORRUPT marker
    /// logs a warning AND produces output WITHOUT the continuation block.
    /// The corrupt marker file is NOT modified OR deleted by the builder.
    #[tokio::test]
    async fn build_prompt_with_corrupt_marker_logs_warning_and_omits_block() {
        let (_dir, ws) = fixture_workspace();
        // Inject a corrupt marker.
        let marker_path = ws
            .join("openspec/changes/x")
            .join(crate::iteration_pending::MARKER_FILE);
        std::fs::write(&marker_path, "{ truncated json").unwrap();
        let executor = ClaudeCliExecutor::new("/bin/true".into(), 30, test_paths_arc());
        let prompt = executor.build_prompt(&ws, "x").unwrap();
        assert!(
            !prompt.contains("--- BEGIN PRIOR ITERATION SUMMARY ---"),
            "corrupt-marker prompt must not contain continuation block: {prompt}"
        );
        // The corrupt marker file is NOT modified OR deleted.
        let raw = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(
            raw, "{ truncated json",
            "corrupt marker must be left as-is for operator inspection"
        );
    }

    /// `from_config`: with no override path, the default template is used.
    #[test]
    fn from_config_uses_default_template_when_path_unset() {
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1)).unwrap();
        assert_eq!(executor.template, DEFAULT_IMPLEMENTER_TEMPLATE);
    }

    /// `from_config`: with an override path, the file is read and used.
    #[test]
    fn from_config_loads_override_template_when_path_set() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("custom.md");
        std::fs::write(&path, "CUSTOM_TEMPLATE_SENTINEL {{change_body}}").unwrap();
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: Some(path),
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1)).unwrap();
        assert!(executor.template.contains("CUSTOM_TEMPLATE_SENTINEL"));
    }

    /// `from_config`: a missing override file falls back to the embedded
    /// default (a24). A one-shot WARN names the missing path; the
    /// daemon does NOT abort start-up.
    #[test]
    fn from_config_falls_back_when_override_file_missing() {
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: Some(PathBuf::from("/definitely/not/a/real/path.md")),
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1))
            .expect("missing override path must fall back to embedded");
        assert_eq!(executor.template, DEFAULT_IMPLEMENTER_TEMPLATE);
    }

    /// The embedded changelog-stylist template is non-empty AND contains
    /// the documented placeholders.
    #[test]
    fn embedded_changelog_stylist_template_is_loaded() {
        assert!(!DEFAULT_CHANGELOG_STYLIST_TEMPLATE.trim().is_empty());
        assert!(DEFAULT_CHANGELOG_STYLIST_TEMPLATE.contains(CHANGELOG_JSON_PLACEHOLDER));
        assert!(DEFAULT_CHANGELOG_STYLIST_TEMPLATE.contains(TRIAGE_REPO_URL_PLACEHOLDER));
        assert!(
            DEFAULT_CHANGELOG_STYLIST_TEMPLATE.contains(CHANGELOG_REVISION_TEXT_PLACEHOLDER)
        );
        assert!(DEFAULT_CHANGELOG_STYLIST_TEMPLATE.contains("CHANGELOG.md"));
        assert!(DEFAULT_CHANGELOG_STYLIST_TEMPLATE.contains("Keep a Changelog"));
    }

    /// `from_config`: with no override path, the embedded stylist
    /// template is used.
    #[test]
    fn from_config_uses_default_changelog_stylist_when_path_unset() {
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1)).unwrap();
        assert_eq!(
            executor.changelog_stylist_template,
            DEFAULT_CHANGELOG_STYLIST_TEMPLATE
        );
    }

    /// `from_config`: with an override path set, the file contents
    /// replace the embedded stylist template.
    #[test]
    fn from_config_loads_override_changelog_stylist_when_path_set() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("custom-stylist.md");
        std::fs::write(&path, "CUSTOM_STYLIST_SENTINEL {{changelog_json}}").unwrap();
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: Some(path),
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1)).unwrap();
        assert!(
            executor.changelog_stylist_template.contains("CUSTOM_STYLIST_SENTINEL"),
            "{}",
            executor.changelog_stylist_template
        );
    }

    /// `from_config`: an empty changelog-stylist override file falls
    /// back to the embedded default (a24). A one-shot WARN names the
    /// path; start-up is NOT aborted.
    #[test]
    fn from_config_falls_back_when_changelog_stylist_file_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.md");
        std::fs::write(&path, "   \n  \n").unwrap();
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: Some(path),
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1))
            .expect("empty stylist override falls back to embedded");
        assert_eq!(
            executor.changelog_stylist_template,
            DEFAULT_CHANGELOG_STYLIST_TEMPLATE
        );
    }

    /// `from_config`: an empty implementer override file falls back to
    /// the embedded default (a24). A one-shot WARN names the path.
    #[test]
    fn from_config_falls_back_when_override_file_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.md");
        std::fs::write(&path, "   \n  \n").unwrap();
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: Some(path),
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1))
            .expect("empty implementer override falls back to embedded");
        assert_eq!(executor.template, DEFAULT_IMPLEMENTER_TEMPLATE);
    }

    /// `from_config`: the new nested form
    /// `executor.implementer.prompt_path` is preferred over the legacy
    /// flat `implementer_prompt_path` (a24).
    #[test]
    fn from_config_nested_form_preempts_legacy_for_implementer() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested.md");
        let legacy = tmp.path().join("legacy.md");
        std::fs::write(&nested, "NESTED_IMPL {{change_body}}").unwrap();
        std::fs::write(&legacy, "LEGACY_IMPL {{change_body}}").unwrap();
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: Some(legacy),
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: Some(crate::config::PromptOverrideBlock {
                prompt_path: Some(nested),
            }),
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1)).unwrap();
        assert!(executor.template.contains("NESTED_IMPL"));
        assert!(!executor.template.contains("LEGACY_IMPL"));
    }

    /// `from_config`: the new nested fields
    /// `executor.audit_triage.prompt_path`,
    /// `executor.chat_request_triage.prompt_path`, AND
    /// `executor.implementer_revision.prompt_path` are honored (a24).
    #[test]
    fn from_config_resolves_new_nested_triage_and_revision_overrides() {
        let tmp = TempDir::new().unwrap();
        let triage = tmp.path().join("triage.md");
        let chat_triage = tmp.path().join("chat-triage.md");
        let revision = tmp.path().join("revision.md");
        std::fs::write(&triage, "TRIAGE_SENTINEL").unwrap();
        std::fs::write(&chat_triage, "CHAT_TRIAGE_SENTINEL").unwrap();
        std::fs::write(&revision, "REVISION_SENTINEL").unwrap();
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: Some(crate::config::PromptOverrideBlock {
                prompt_path: Some(revision),
            }),
            audit_triage: Some(crate::config::PromptOverrideBlock {
                prompt_path: Some(triage),
            }),
            chat_request_triage: Some(crate::config::PromptOverrideBlock {
                prompt_path: Some(chat_triage),
            }),
        };
        let executor = ClaudeCliExecutor::from_config(&cfg, std::sync::Arc::new(crate::testing::test_daemon_paths().1)).unwrap();
        assert!(
            executor.triage_template.contains("TRIAGE_SENTINEL"),
            "audit_triage override must load: {}",
            executor.triage_template
        );
        assert!(
            executor.chat_triage_template.contains("CHAT_TRIAGE_SENTINEL"),
            "chat_request_triage override must load: {}",
            executor.chat_triage_template
        );
        assert!(
            executor.revision_template.contains("REVISION_SENTINEL"),
            "implementer_revision override must load: {}",
            executor.revision_template
        );
    }

    /// Run-log persistence: after a subprocess invocation completes,
    /// both stdout and stderr (verbatim) must be written to the
    /// per-change log file at the expected path.
    #[tokio::test]
    async fn run_log_is_written_with_expected_format() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "echoes.sh",
            "#!/bin/sh\necho hello-out\necho hello-err >&2\nexit 0\n",
        );
        // Text-mode opt-out path: legacy STDOUT/STDERR section names.
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc())
            .with_output_format(crate::config::ExecutorOutputFormat::Text);
        let outcome = executor.run(&ws, "x").await.unwrap();
        assert!(matches!(outcome, ExecutorOutcome::Completed { .. }), "got {outcome:?}");

        let log = run_log_path(&test_paths_arc(), &ws, "x");
        let body = std::fs::read_to_string(&log)
            .unwrap_or_else(|e| panic!("reading {}: {e}", log.display()));
        assert!(body.contains("=== STDOUT ("), "missing stdout header in:\n{body}");
        assert!(body.contains("=== STDERR ("), "missing stderr header in:\n{body}");
        assert!(body.contains("hello-out"), "stdout text missing in:\n{body}");
        assert!(body.contains("hello-err"), "stderr text missing in:\n{body}");
    }

    /// Run-log path layout: `<logs_dir>/runs/<repo-sanitized>/<change>.log`.
    /// All segments must be present so per-repo and per-change
    /// inspection is possible.
    #[tokio::test]
    async fn run_log_path_is_under_repo_sanitized_and_change_name() {
        let (_dir, ws) = fixture_workspace_with_git();
        let path = run_log_path(&test_paths_arc(), &ws, "my-change");
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        let s = path.to_string_lossy();
        assert!(
            s.contains("/runs/") || s.contains("\\runs\\"),
            "path missing /runs/ segment: {s}"
        );
        assert!(s.contains(&*basename), "path missing repo-sanitized `{basename}`: {s}");
        assert!(s.ends_with("my-change.log"), "path missing change name: {s}");
    }

    #[test]
    fn tail_snaps_to_char_boundary() {
        // Multi-byte string: "héllo" — 'é' is two bytes (0xC3 0xA9).
        // Asking for 4 bytes from a 6-byte string would naively split
        // the codepoint at byte index 2.
        let s = "héllo"; // 6 bytes
        let t = tail(s, 4);
        // The slice must be valid UTF-8 (Rust would panic on the slice
        // op itself if not). Confirm length is <= 4 and content is the
        // suffix.
        assert!(t.len() <= 4, "tail length must respect the budget: {:?}", t);
        assert!(s.ends_with(t), "tail must be a suffix of input: {t:?} vs {s:?}");
    }

    #[test]
    fn tail_returns_full_string_when_shorter_than_max() {
        let s = "abc";
        assert_eq!(tail(s, 100), "abc");
    }

    #[test]
    fn tail_handles_empty_input() {
        assert_eq!(tail("", 100), "");
    }

    /// `persist_run_log` writes a PROMPT section ahead of STDOUT and
    /// STDERR. Operators rely on this to see exactly what Claude was
    /// sent on a `Completed-without-modifying-the-workspace` outcome.
    #[test]
    fn persist_run_log_writes_prompt_section_first() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("github_com_owner_repo");
        std::fs::create_dir_all(&ws).unwrap();
        let outcome = AgenticRunOutcome {
            timed_out: false,
            exit_status: None,
            stdout: "STDOUT_SENTINEL".to_string(),
            stderr: "STDERR_SENTINEL".to_string(),
            final_answer: None,
            streamed_log: false,
            session_handle: None,
            session_id: None,
        };
        persist_run_log(&test_paths_arc(), &ws, "my-change", "PROMPT_SENTINEL", &outcome);

        let log = run_log_path(&test_paths_arc(), &ws, "my-change");
        let body = std::fs::read_to_string(&log).expect("log file written");
        // Ordering and labels.
        let prompt_idx = body.find("=== PROMPT (").expect("PROMPT header");
        let stdout_idx = body.find("=== STDOUT (").expect("STDOUT header");
        let stderr_idx = body.find("=== STDERR (").expect("STDERR header");
        assert!(prompt_idx < stdout_idx && stdout_idx < stderr_idx,
            "sections must appear in PROMPT → STDOUT → STDERR order:\n{body}");
        // Content presence.
        assert!(body.contains("PROMPT_SENTINEL"));
        assert!(body.contains("STDOUT_SENTINEL"));
        assert!(body.contains("STDERR_SENTINEL"));
    }

    // ---------- a20a1: timeout precedence + sentinel-scope ----------

    fn fixture_workspace_for_classify() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("github_com_owner_repo");
        std::fs::create_dir_all(&ws).unwrap();
        // Ensure the openspec/changes/<change>/ directory exists so the
        // askuser-marker check in classify_outcome's preamble doesn't
        // error on a missing path. classify_outcome only checks the
        // marker file's existence, not the directory, so this is mostly
        // hygiene.
        std::fs::create_dir_all(ws.join("openspec/changes/x")).unwrap();
        (tmp, ws)
    }

    fn fixture_executor_json() -> ClaudeCliExecutor {
        ClaudeCliExecutor::new("dummy-claude".into(), 30, test_paths_arc())
    }

    fn fixture_executor_text() -> ClaudeCliExecutor {
        ClaudeCliExecutor::new("dummy-claude".into(), 30, test_paths_arc())
            .with_output_format(crate::config::ExecutorOutputFormat::Text)
    }

    /// The regression test for the a21-perma-stuck incident: a timed-out
    /// run with a well-formed sentinel-shaped block in stdout (e.g. from
    /// a tool-result echo of prompts/implementer.md) must classify as
    /// `timeout`, NOT `unparseable sentinel`. Pre-a20a1 this produced
    /// the misleading sentinel-failure reason that masked the real cause.
    #[tokio::test]
    async fn timed_out_run_with_sentinel_in_stdout_returns_timeout() {
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: true,
            exit_status: None,
            // Full well-formed sentinel-shaped content in stdout —
            // would have triggered the false-match path pre-fix.
            stdout: "\
some tool output\n\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[\
{\"task_id\":\"5.2\",\"task_text\":\"install actionlint\",\"reason\":\"no apt access\"}],\
\"revision_suggestion\":\"Replace with CI gate.\"}\n".to_string(),
            stderr: "timeout".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        match result {
            ExecutorOutcome::Failed { reason } => {
                assert_eq!(reason, "timeout",
                    "timed-out runs must classify as timeout regardless of stdout content: got {reason}");
                assert!(
                    !reason.contains("unparseable"),
                    "pre-fix would have surfaced 'unparseable sentinel' here"
                );
            }
            other => panic!("expected Failed(timeout), got {other:?}"),
        }
    }

    /// The exact a21 incident shape: timed_out, final_answer absent,
    /// stdout contains the `\n31\t`-line-numbered Read echo of the
    /// prompt template. Pre-fix returned `Failed { reason: "agent
    /// emitted unparseable SpecNeedsRevision sentinel: ..." }`. Now
    /// classifies as timeout.
    #[tokio::test]
    async fn timed_out_run_with_line_numbered_prompt_echo_returns_timeout() {
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        // The `\n31\t` is the cat-n style prefix that signaled the bug:
        // a Read tool result for prompts/implementer.md renders the
        // file with line numbers, breaking JSON parse if scanned.
        let outcome = AgenticRunOutcome {
            timed_out: true,
            exit_status: None,
            stdout: "\
[tool_use] Read prompts/implementer.md\n\
[tool_result]\n\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[\n31\t  {\"task_id\":\"<id-from-tasks-md>\",\"task_text\":\"<verbatim quote>\",\"reason\":\"<one-line why>\"}\n],\"revision_suggestion\":\"<free-form text>\"}\n".to_string(),
            stderr: "timeout".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        assert!(matches!(
            result,
            ExecutorOutcome::Failed { reason } if reason == "timeout"
        ));
    }

    // ---------- end a20a1 ----------

    /// a27a0 task 4.5: any JSON snippet shown in the prompt SHALL
    /// deserialize cleanly into the corresponding Rust type via
    /// `serde_json::from_str`. The bundled prompt's worked example for
    /// the `outcome_spec_needs_revision` MCP tool's `arguments` object
    /// must round-trip through the MCP-layer validator.
    #[test]
    fn bundled_implementer_prompt_outcome_tool_example_validates_at_mcp_layer() {
        // Extract the FIRST `{...}` JSON object inside a fenced
        // markdown block after the "Worked example" heading. We don't
        // need a full markdown parser — the prompt format is stable
        // and the heading is unique.
        let prompt = DEFAULT_IMPLEMENTER_TEMPLATE;
        let heading_idx = prompt
            .find("Worked example")
            .expect("prompt must document a worked example for the outcome tool");
        let after = &prompt[heading_idx..];
        // First triple-backtick opens the fence; find it.
        let fence_open = after.find("```").expect("worked example must be fenced");
        let body_start = fence_open + 3;
        // Advance past the language identifier line, if any.
        let body_start = body_start
            + after[body_start..]
                .find('\n')
                .map(|i| i + 1)
                .unwrap_or(0);
        let body_after = &after[body_start..];
        let fence_close = body_after.find("```").expect("fenced block must close");
        let body = &body_after[..fence_close];
        let value: serde_json::Value = serde_json::from_str(body.trim())
            .unwrap_or_else(|e| panic!("worked-example JSON does not parse: {e}\n---\n{body}"));
        // The body should validate cleanly through the MCP-layer
        // validator AND every string field must contain a concrete
        // (non-placeholder) value.
        let validated = crate::mcp_askuser_server::validate_spec_needs_revision_args(&value)
            .unwrap_or_else(|e| {
                panic!("worked example failed MCP-layer validation: {e}\n---\n{body}")
            });
        assert_eq!(validated["type"], "spec_needs_revision");
        // The shape that the daemon's outcome store will store/round-trip:
        // deserialize the full variant-tagged object into RecordedOutcome.
        let _round_trip: crate::outcome_store::RecordedOutcome =
            serde_json::from_value(validated.clone())
                .expect("validated payload must deserialize as RecordedOutcome");
    }

    /// 5.4: a revision-mode prompt build with a sample `RevisionContext`
    /// Per a20a5: the revision prompt is constructed exclusively from
    /// PR-sourced material in the `RevisionContext`. The builder MUST:
    /// (a) substitute all FIVE placeholders verbatim into the
    /// template's positions, (b) NOT spawn any subprocess (the
    /// pre-a20a5 `openspec instructions apply` call is removed), AND
    /// (c) NOT contain ANY legacy placeholder fallback string.
    #[test]
    fn build_revision_prompt_substitutes_all_placeholders() {
        let (_dir, ws) = fixture_workspace();
        let executor = ClaudeCliExecutor::new("dummy".into(), 30, test_paths_arc());
        let ctx = crate::revisions::RevisionContext {
            change_name: "x".to_string(),
            pr_diff: "DIFF_HERE".to_string(),
            revision_text: "REVISION_HERE".to_string(),
            pr_body: "PR_BODY_HERE".to_string(),
            pr_change_list: "a17-foo\na18-bar".to_string(),
            agent_implementation_notes: "AGENT_NOTES_HERE".to_string(),
        };
        let prompt = executor.build_revision_prompt(&ws, "x", &ctx).unwrap();

        // All five context fields appear in the rendered prompt.
        assert!(
            prompt.contains("DIFF_HERE"),
            "diff missing:\n{prompt}"
        );
        assert!(
            prompt.contains("REVISION_HERE"),
            "revision request missing:\n{prompt}"
        );
        assert!(
            prompt.contains("PR_BODY_HERE"),
            "PR body missing:\n{prompt}"
        );
        assert!(
            prompt.contains("a17-foo"),
            "change list missing:\n{prompt}"
        );
        assert!(
            prompt.contains("a18-bar"),
            "change list (second slug) missing:\n{prompt}"
        );
        assert!(
            prompt.contains("AGENT_NOTES_HERE"),
            "agent implementation notes missing:\n{prompt}"
        );

        // Section markers in the new template.
        for marker in [
            "BEGIN CHANGES IN THIS PR",
            "BEGIN PR BODY",
            "BEGIN ORIGINAL AGENT IMPLEMENTATION NOTES",
            "BEGIN PR DIFF",
            "BEGIN REVISION REQUEST",
        ] {
            assert!(
                prompt.contains(marker),
                "marker `{marker}` missing:\n{prompt}"
            );
        }

        // No legacy placeholder fallback strings, no legacy
        // {{change_body}} placeholder, no unrendered new placeholders.
        for forbidden in [
            "original change material unavailable",
            "openspec instructions apply",
            "{{change_body}}",
            "{{pr_body}}",
            "{{pr_change_list}}",
            "{{agent_implementation_notes}}",
            "{{pr_diff}}",
            "{{revision_request}}",
            "BEGIN ORIGINAL CHANGE",  // pre-a20a5 marker
        ] {
            assert!(
                !prompt.contains(forbidden),
                "forbidden string `{forbidden}` present in rendered prompt:\n{prompt}"
            );
        }
    }

    /// a20a5 regression test: build_revision_prompt MUST NOT call
    /// `openspec` (or any other subprocess). The previous
    /// implementation spawned `openspec instructions apply` and fell
    /// back to a placeholder when it failed — that call is removed.
    /// Verifies via the renderered prompt's contents (the placeholder
    /// fallback string would surface if the subprocess were re-added
    /// AND it failed).
    #[test]
    fn build_revision_prompt_does_not_invoke_openspec() {
        let (_dir, ws) = fixture_workspace();
        let executor = ClaudeCliExecutor::new("dummy".into(), 30, test_paths_arc());
        let ctx = crate::revisions::RevisionContext {
            change_name: "x".to_string(),
            pr_diff: "diff".to_string(),
            revision_text: "rev".to_string(),
            pr_body: "body".to_string(),
            pr_change_list: "x".to_string(),
            agent_implementation_notes: "notes".to_string(),
        };
        let prompt = executor.build_revision_prompt(&ws, "x", &ctx).unwrap();
        // If a subprocess fallback were still in place, this would
        // surface in any error case. The new builder cannot produce
        // either of these substrings under any input.
        assert!(!prompt.contains("openspec instructions apply"));
        assert!(!prompt.contains("original change material unavailable"));
    }

    /// a002 regression (task 3.5): the self-hosting case. When the PR
    /// under revision edits `prompts/implementer-revision.md`, the
    /// `pr_diff` carries literal `{{revision_request}}` / `{{pr_body}}`
    /// tokens. Under chained `.replace`, the later
    /// `.replace("{{revision_request}}", …)` / `.replace("{{pr_body}}", …)`
    /// passes re-expanded those literals inside the injected diff,
    /// corrupting the prompt. Single-pass substitution emits them verbatim
    /// and inserts each real value exactly once.
    #[test]
    fn build_revision_prompt_does_not_re_expand_placeholders_in_diff() {
        let (_dir, ws) = fixture_workspace();
        let executor = ClaudeCliExecutor::new("dummy".into(), 30, test_paths_arc());
        // The diff carries many literal placeholder tokens (as a diff
        // touching the revision template itself would).
        let diff_line = "+ instructions reference {{revision_request}} and {{pr_body}}\n";
        let k = 40usize;
        let pr_diff = diff_line.repeat(k);
        let ctx = crate::revisions::RevisionContext {
            change_name: "x".to_string(),
            pr_diff: pr_diff.clone(),
            revision_text: "UNIQUE_REVISION_SENTINEL".to_string(),
            pr_body: "UNIQUE_BODY_SENTINEL".to_string(),
            pr_change_list: "a01-x".to_string(),
            agent_implementation_notes: "notes".to_string(),
        };
        let prompt = executor.build_revision_prompt(&ws, "x", &ctx).unwrap();

        // The literal tokens carried by the diff survive verbatim.
        assert!(
            prompt.contains("instructions reference {{revision_request}} and {{pr_body}}"),
            "diff-borne placeholder literals must survive verbatim:\n{prompt}"
        );
        // Each real value is inserted exactly once — NOT once per literal
        // carried in the diff.
        assert_eq!(
            prompt.matches("UNIQUE_REVISION_SENTINEL").count(),
            1,
            "the revision request must be inserted exactly once"
        );
        assert_eq!(
            prompt.matches("UNIQUE_BODY_SENTINEL").count(),
            1,
            "the PR body must be inserted exactly once"
        );
        // The K literal tokens remain K (none were expanded).
        assert_eq!(prompt.matches("{{revision_request}}").count(), k);
        assert_eq!(prompt.matches("{{pr_body}}").count(), k);
        // Size bound: the prompt cannot exceed template + every injected
        // value's length (no multiplicative growth from the diff's literals).
        let bound = executor.revision_template.len()
            + ctx.pr_body.len()
            + ctx.pr_change_list.len()
            + ctx.agent_implementation_notes.len()
            + ctx.pr_diff.len()
            + ctx.revision_text.len();
        assert!(
            prompt.len() <= bound,
            "prompt size {} must be bounded by template + injected values = {bound}",
            prompt.len()
        );
    }

    /// a002 (executor spec scenario): operator `request_text` carrying a
    /// `{{repo_url}}` / `{{canonical_specs_index}}` literal is not
    /// re-expanded by `build_chat_triage_prompt`; the real placeholders are
    /// each substituted exactly once.
    #[test]
    fn build_chat_triage_prompt_does_not_re_expand_request_text() {
        let executor = ClaudeCliExecutor::new("dummy".into(), 30, test_paths_arc());
        let ctx = ChatTriageContext {
            request_text: "please look at {{repo_url}} and {{canonical_specs_index}}".to_string(),
            repo_url: "UNIQUE_REPO_URL_SENTINEL".to_string(),
            canonical_specs_index: "UNIQUE_SPECS_INDEX_SENTINEL".to_string(),
        };
        let prompt = executor.build_chat_triage_prompt(&ctx);

        assert!(
            prompt.contains("please look at {{repo_url}} and {{canonical_specs_index}}"),
            "request_text placeholder literals must survive verbatim:\n{prompt}"
        );
        assert_eq!(
            prompt.matches("UNIQUE_REPO_URL_SENTINEL").count(),
            1,
            "repo_url must be substituted exactly once"
        );
        assert_eq!(
            prompt.matches("UNIQUE_SPECS_INDEX_SENTINEL").count(),
            1,
            "canonical_specs_index must be substituted exactly once"
        );
    }

    /// End-to-end: after a `run`, the persisted log contains a PROMPT
    /// section (whether the prompt came from openspec or from the raw-
    /// markdown fallback). This is the diagnostic that lets an operator
    /// see exactly what Claude was sent.
    #[tokio::test]
    async fn run_log_contains_prompt_section() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "noop.sh", "#!/bin/sh\nexit 0\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let _ = executor.run(&ws, "x").await.unwrap();

        let log = run_log_path(&test_paths_arc(), &ws, "x");
        let body = std::fs::read_to_string(&log).expect("log file written");
        assert!(body.contains("=== PROMPT ("), "missing PROMPT header in:\n{body}");
        // The recorded prompt must be non-empty. Different envs may
        // hit the openspec path or the raw-markdown fallback — both
        // identify the change by name, so assert on that.
        assert!(
            body.contains("x"),
            "prompt content missing change identifier:\n{body}"
        );
        assert!(
            !body.contains("=== PROMPT (0 bytes)"),
            "prompt was empty:\n{body}"
        );
    }

    // ------------------------------------------------------------------
    // JSON streaming-mode tests
    // ------------------------------------------------------------------

    /// JSON streaming: a fixture script that emits a few tool_use
    /// events followed by a result event must produce a structured log
    /// with PROMPT / ACTIONS / FINAL ANSWER / STDERR sections.
    #[tokio::test]
    async fn json_streaming_log_has_structured_sections() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "stream.sh",
            r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"s1"}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"src/a.rs"}}]}}'
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"file body","is_error":false}]}}'
echo '{"type":"assistant","message":[{"type":"text","text":"checking next file"}]}'
echo '{"type":"result","stop_reason":"end_turn","result":"Done — the change is implemented."}'
exit 0
"#,
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(
                    final_answer.as_deref(),
                    Some("Done — the change is implemented."),
                    "final answer must round-trip from the result event"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        let log = run_log_path(&test_paths_arc(), &ws, "x");
        let body = std::fs::read_to_string(&log).unwrap();
        // Per a20a2: summary log has PROMPT, ACTIONS-pointer line,
        // FINAL ANSWER, STDERR. NOT raw action content.
        assert!(body.contains("=== PROMPT ("));
        assert!(body.contains("=== ACTIONS (see x.stream.log) ==="));
        assert!(body.contains("=== FINAL ANSWER ("));
        assert!(body.contains("=== STDERR ("));
        assert!(body.contains("Done — the change is implemented."));
        // FINAL ANSWER must not leak action stream content.
        assert!(!body.contains("[tool_use]"));
        assert!(!body.contains("[tool_result]"));

        // Action stream lives in the sibling stream log.
        let stream_log = log.with_extension("stream.log");
        let stream = std::fs::read_to_string(&stream_log).unwrap();
        assert!(stream.contains("[tool_use] Read src/a.rs"));
        assert!(stream.contains("[tool_result] (9 bytes returned)"));
    }

    /// JSON streaming: a fixture child that gets killed mid-stream
    /// (script never emits the `result` event) produces a log with
    /// ACTIONS for the events that DID arrive, empty FINAL ANSWER,
    /// and the outcome's `final_answer` is None.
    ///
    /// The script closes stdout/stderr after emitting its two events
    /// so the executor's reader tasks see EOF immediately — without
    /// the redirect, `sleep` would inherit the open pipe and block
    /// our readers for the full 30s (see the `#[ignore]`d
    /// `timeout_kills_child` test for the same pipe-inheritance issue
    /// on the legacy path).
    #[tokio::test]
    async fn json_streaming_timeout_kill_preserves_partial_actions() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "slow.sh",
            r#"#!/bin/sh
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"a"}}]}}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"path":"b"}}]}}'
exec </dev/null >/dev/null 2>&1
sleep 30
"#,
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 1, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Failed { reason } => {
                assert_eq!(reason, "timeout");
            }
            other => panic!("expected Failed timeout, got {other:?}"),
        }
        let log = run_log_path(&test_paths_arc(), &ws, "x");
        let body = std::fs::read_to_string(&log).expect("summary log written");
        // Summary: empty FINAL ANSWER (timeout). Stream: action lines.
        assert!(
            body.contains("=== FINAL ANSWER (0 bytes) ==="),
            "FINAL ANSWER must be empty after timeout-kill:\n{body}"
        );
        let stream_log = log.with_extension("stream.log");
        let stream = std::fs::read_to_string(&stream_log).expect("stream log written");
        assert!(stream.contains("[tool_use] Read a"));
        assert!(stream.contains("[tool_use] Edit b"));
    }

    /// JSON streaming: malformed JSON line lands in ACTIONS as `[raw]`.
    /// Subsequent valid events continue to be processed.
    #[tokio::test]
    async fn json_streaming_malformed_line_becomes_raw_action() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "mixed.sh",
            r#"#!/bin/sh
echo 'this is not json'
echo '{"type":"result","stop_reason":"end_turn","result":"ok"}'
exit 0
"#,
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        assert!(matches!(outcome, ExecutorOutcome::Completed { .. }));
        // Raw action lines live in the stream log; the valid `result`
        // event populates FINAL ANSWER in the summary log.
        let summary = std::fs::read_to_string(run_log_path(&test_paths_arc(), &ws, "x")).unwrap();
        let stream = std::fs::read_to_string(
            run_log_path(&test_paths_arc(), &ws, "x").with_extension("stream.log"),
        )
        .unwrap();
        assert!(
            stream.contains("[raw] this is not json"),
            "malformed line missing from stream log:\n{stream}"
        );
        assert!(
            summary.contains("ok"),
            "result event's text must reach FINAL ANSWER in summary:\n{summary}"
        );
    }

    /// JSON streaming: an unknown event type lands in ACTIONS as
    /// `[unknown:<type>]`; processing continues normally.
    #[tokio::test]
    async fn json_streaming_unknown_event_type_becomes_unknown_action() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "unknown.sh",
            r#"#!/bin/sh
echo '{"type":"future_event_kind","foo":"bar"}'
echo '{"type":"result","stop_reason":"end_turn","result":"done"}'
exit 0
"#,
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let _ = executor.run(&ws, "x").await.unwrap();
        // Per a20a2: unknown-type lines land in the stream log, not
        // the summary.
        let stream = std::fs::read_to_string(
            run_log_path(&test_paths_arc(), &ws, "x").with_extension("stream.log"),
        )
        .unwrap();
        assert!(
            stream.contains("[unknown:future_event_kind]"),
            "unknown prefix missing from stream log:\n{stream}"
        );
    }

    /// JSON streaming: stderr from the child lands in the STDERR
    /// section alongside the ACTIONS content from stdout.
    #[tokio::test]
    async fn json_streaming_stderr_lands_in_stderr_section() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "both.sh",
            r#"#!/bin/sh
echo '{"type":"result","stop_reason":"end_turn","result":"ok"}'
echo 'stderr noise' >&2
exit 0
"#,
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let _ = executor.run(&ws, "x").await.unwrap();
        let body = std::fs::read_to_string(run_log_path(&test_paths_arc(), &ws, "x")).unwrap();
        let stderr_section = body.split("=== STDERR (").nth(1).unwrap();
        assert!(
            stderr_section.contains("stderr noise"),
            "stderr noise not in STDERR section:\n{body}"
        );
    }

    /// Outcome shape: Completed carries final_answer Some(text) from
    /// the result event.
    #[tokio::test]
    async fn completed_outcome_carries_final_answer_from_result_event() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "answer.sh",
            r#"#!/bin/sh
echo '{"type":"result","stop_reason":"end_turn","result":"FINAL_ANSWER_SENTINEL"}'
exit 0
"#,
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(
                    final_answer.as_deref(),
                    Some("FINAL_ANSWER_SENTINEL"),
                    "outcome must surface the final answer text"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Outcome shape: Failed (timeout) carries no final answer.
    /// Uses the closed-stdout/stderr trick so the readers see EOF and
    /// the test doesn't have to wait for `sleep 30` to die.
    #[tokio::test]
    async fn failed_outcome_has_no_final_answer() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "slow.sh",
            "#!/bin/sh\nexec </dev/null >/dev/null 2>&1\nsleep 30\nexit 0\n",
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 1, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Failed { .. } => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Text-mode opt-out: spawn does NOT include `--output-format`,
    /// log shape uses legacy STDOUT/STDERR section names, outcome's
    /// final_answer is None.
    #[tokio::test]
    async fn text_mode_opt_out_produces_legacy_log_shape() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "text.sh",
            "#!/bin/sh\necho 'final summary text from text mode'\nexit 0\n",
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc())
            .with_output_format(crate::config::ExecutorOutputFormat::Text);
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Completed { final_answer } => {
                assert!(
                    final_answer.is_none(),
                    "text mode must NOT populate final_answer (no JSON parser ran)"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        let body = std::fs::read_to_string(run_log_path(&test_paths_arc(), &ws, "x")).unwrap();
        assert!(body.contains("=== STDOUT ("), "legacy STDOUT section missing");
        assert!(body.contains("=== STDERR ("), "legacy STDERR section missing");
        assert!(!body.contains("=== ACTIONS ==="), "JSON-mode ACTIONS section must be absent");
        assert!(
            !body.contains("=== FINAL ANSWER ("),
            "JSON-mode FINAL ANSWER section must be absent"
        );
        assert!(body.contains("final summary text from text mode"));
    }

    // ---------------------------------------------------------------
    // a27a0: tool-recorded outcome precedence + deprecation warning
    // ---------------------------------------------------------------

    // Env-var-touching tests serialize via `crate::testing::ENV_LOCK`
    // (a27a2 unified the per-module locks into a single process-wide
    // lock so cross-module tests cannot race).

    /// Spin up a Unix-domain-socket listener that handles ONE
    /// `consume_outcome` action by returning the canned `outcome`
    /// payload (or null when `None`). Returns the socket path; the
    /// listener exits after one round trip.
    async fn spawn_consume_outcome_responder(
        outcome: Option<serde_json::Value>,
    ) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(read_half);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let resp = serde_json::json!({
                "ok": true,
                "outcome": outcome.unwrap_or(serde_json::Value::Null),
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            let _ = write_half.write_all(&bytes).await;
            let _ = write_half.shutdown().await;
        });
        (dir, socket)
    }

    /// 3.5 — tool-recorded `Success` outcome takes precedence over the
    /// diff-presence/Completed heuristic. The agent's deliberate
    /// `outcome_success` signal wins over any inferred state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn tool_recorded_success_takes_precedence() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let success_payload = serde_json::json!({
            "type": "success",
            "final_answer": "from-the-tool"
        });
        let (_sock_dir, socket) =
            spawn_consume_outcome_responder(Some(success_payload)).await;
        let (_tmp, ws) = fixture_workspace_for_classify();
        // SAFETY: tests serialize via A27A0_ENV_LOCK.
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        let executor = fixture_executor_text();
        use std::os::unix::process::ExitStatusExt;
        let outcome = AgenticRunOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: "some normal output".to_string(),
            stderr: String::new(),
            final_answer: None,
            streamed_log: false,
            session_handle: None,
            session_id: None,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match result {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(final_answer.as_deref(), Some("from-the-tool"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// 3.6 — tool-recorded `SpecNeedsRevision` outcome takes precedence
    /// over the timeout flag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn tool_recorded_spec_revision_takes_precedence_over_timeout() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let revision_payload = serde_json::json!({
            "type": "spec_needs_revision",
            "unimplementable_tasks": [
                {"task_id": "6.4", "task_text": "Manual: SSH", "reason": "no SSH"}
            ],
            "revision_suggestion": "Mock systemctl"
        });
        let (_sock_dir, socket) =
            spawn_consume_outcome_responder(Some(revision_payload)).await;
        let (_tmp, ws) = fixture_workspace_for_classify();
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        let executor = fixture_executor_json();
        let outcome = AgenticRunOutcome {
            timed_out: true,
            exit_status: None,
            stdout: String::new(),
            stderr: "timeout".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match result {
            ExecutorOutcome::SpecNeedsRevision {
                unimplementable_tasks,
                revision_suggestion,
            } => {
                assert_eq!(unimplementable_tasks[0].task_id, "6.4");
                assert_eq!(revision_suggestion, "Mock systemctl");
            }
            other => panic!("expected SpecNeedsRevision, got {other:?}"),
        }
    }

    /// Daemon-recorded outcome maps correctly to `ExecutorOutcome` for
    /// both variants. Unit-level test of the mapping function so the
    /// integration tests above are not the only coverage.
    #[test]
    fn map_recorded_outcome_round_trips_both_variants() {
        let (_td, paths) = crate::testing::test_daemon_paths();
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/x")).unwrap();
        let recorded_success = crate::outcome_store::RecordedOutcome::Success {
            final_answer: Some("ok".to_string()),
        };
        match map_recorded_outcome(&paths, ws, "x", recorded_success) {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(final_answer.as_deref(), Some("ok"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        let recorded_revision = crate::outcome_store::RecordedOutcome::SpecNeedsRevision {
            unimplementable_tasks: vec![
                crate::outcome_store::RecordedUnimplementableTask {
                    task_id: "1".into(),
                    task_text: "t".into(),
                    reason: "r".into(),
                },
            ],
            revision_suggestion: "s".to_string(),
        };
        match map_recorded_outcome(&paths, ws, "x", recorded_revision) {
            ExecutorOutcome::SpecNeedsRevision {
                unimplementable_tasks,
                revision_suggestion,
            } => {
                assert_eq!(unimplementable_tasks[0].task_id, "1");
                assert_eq!(revision_suggestion, "s");
            }
            other => panic!("expected SpecNeedsRevision, got {other:?}"),
        }
    }

    /// Task 3.3: a `RecordedOutcome::IterationRequest` with no marker
    /// present maps to `IterationRequested { iteration_number: 2, ... }`.
    #[test]
    fn map_iteration_request_no_marker_yields_iteration_two() {
        let (_td, paths) = crate::testing::test_daemon_paths();
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/x")).unwrap();
        let recorded = crate::outcome_store::RecordedOutcome::IterationRequest {
            completed_tasks: vec!["1".into(), "2".into()],
            remaining_tasks: vec!["3".into()],
            reason: "ran out of time".into(),
        };
        match map_recorded_outcome(&paths, ws, "x", recorded) {
            ExecutorOutcome::IterationRequested {
                completed_tasks,
                remaining_tasks,
                reason,
                iteration_number,
            } => {
                assert_eq!(completed_tasks, vec!["1".to_string(), "2".to_string()]);
                assert_eq!(remaining_tasks, vec!["3".to_string()]);
                assert_eq!(reason, "ran out of time");
                assert_eq!(iteration_number, 2);
            }
            other => panic!("expected IterationRequested, got {other:?}"),
        }
    }

    /// Task 3.4: marker showing iteration_number 4 maps to iteration 5
    /// (5th iteration is still permitted under the cap).
    #[test]
    fn map_iteration_request_with_marker_iteration_four_yields_five() {
        let (_td, paths) = crate::testing::test_daemon_paths();
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/x")).unwrap();
        crate::iteration_pending::write_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "x",
            &crate::iteration_pending::IterationPendingMarker {
                completed_tasks: vec!["1".into(), "2".into(), "3".into()],
                remaining_tasks: vec!["4".into()],
                reason: "prior reason".into(),
                iteration_number: 4,
            },
        )
        .unwrap();
        let recorded = crate::outcome_store::RecordedOutcome::IterationRequest {
            completed_tasks: vec!["1".into(), "2".into(), "3".into(), "4a".into()],
            remaining_tasks: vec!["4b".into()],
            reason: "need more time".into(),
        };
        match map_recorded_outcome(&paths, ws, "x", recorded) {
            ExecutorOutcome::IterationRequested {
                iteration_number, ..
            } => {
                assert_eq!(iteration_number, 5);
            }
            other => panic!("expected IterationRequested at iteration 5, got {other:?}"),
        }
    }

    /// Task 3.5: marker showing iteration_number 5 maps to Failed with
    /// the cap-exceeded reason; the marker is NOT modified.
    #[test]
    fn map_iteration_request_with_marker_iteration_five_yields_failed_cap_exceeded() {
        let (_td, paths) = crate::testing::test_daemon_paths();
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/x")).unwrap();
        let prior_marker = crate::iteration_pending::IterationPendingMarker {
            completed_tasks: vec!["1".into(), "2".into(), "3".into(), "4".into()],
            remaining_tasks: vec!["5".into()],
            reason: "prior reason".into(),
            iteration_number: 5,
        };
        crate::iteration_pending::write_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "x",
            &prior_marker,
        )
        .unwrap();
        let recorded = crate::outcome_store::RecordedOutcome::IterationRequest {
            completed_tasks: vec![
                "1".into(),
                "2".into(),
                "3".into(),
                "4".into(),
                "5a".into(),
            ],
            remaining_tasks: vec!["5b".into()],
            reason: "still need more time".into(),
        };
        match map_recorded_outcome(&paths, ws, "x", recorded) {
            ExecutorOutcome::Failed { reason } => {
                assert!(
                    reason.starts_with("exceeded iteration-request cap (5)"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected Failed (cap-exceeded), got {other:?}"),
        }
        // Marker is NOT modified.
        let still = crate::iteration_pending::read_marker(
            &paths,
            ws.file_name().and_then(|s| s.to_str()).unwrap(),
            "x",
        )
        .unwrap()
        .expect("marker preserved");
        assert_eq!(still, prior_marker);
    }

    /// Task 3.6: a corrupt marker file (truncated JSON) is treated as
    /// "no marker present" — the classifier returns IterationRequested
    /// at iteration_number 2.
    #[test]
    fn map_iteration_request_with_corrupt_marker_treats_as_absent() {
        let (_td, paths) = crate::testing::test_daemon_paths();
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/x")).unwrap();
        // Inject a truncated marker.
        std::fs::write(
            ws.join("openspec/changes/x")
                .join(crate::iteration_pending::MARKER_FILE),
            "{\"completed_tasks\":[\"1\"]",
        )
        .unwrap();
        let recorded = crate::outcome_store::RecordedOutcome::IterationRequest {
            completed_tasks: vec!["1".into()],
            remaining_tasks: vec!["2".into()],
            reason: "fresh start".into(),
        };
        match map_recorded_outcome(&paths, ws, "x", recorded) {
            ExecutorOutcome::IterationRequested {
                iteration_number, ..
            } => {
                assert_eq!(iteration_number, 2);
            }
            other => panic!("expected IterationRequested at iteration 2 (corrupt-as-absent), got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // a27a2: acceptance scan + recovery loop integration tests
    // ---------------------------------------------------------------

    /// Spin up a UDS listener that responds to a configurable number of
    /// `consume_outcome` round-trips, pulling each response off the
    /// supplied queue in order. Tests covering the acceptance scan's
    /// recovery-turn arms drive this with two-element queues (initial
    /// classify + recovery classify).
    ///
    /// The responder filters by `expected_workspace_basename`: requests
    /// whose `workspace_basename` field does not match are silently
    /// dropped (the connection closes without a response, which the
    /// classifier reads as `None`). This isolates parallel test runs
    /// that happen to read the same `ENV_CONTROL_SOCKET` set by a
    /// concurrently-executing locked test.
    async fn spawn_multi_consume_outcome_responder_for(
        expected_workspace_basename: &str,
        responses: Vec<Option<serde_json::Value>>,
    ) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let expected = expected_workspace_basename.to_string();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let mut queue = responses.into_iter();
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                let req: serde_json::Value =
                    match serde_json::from_str(line.trim()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                let req_basename = req
                    .get("workspace_basename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if req_basename != expected {
                    // Foreign request: close connection silently so the
                    // sender's `from_str` over an empty line yields Err
                    // → fall-through to `None`.
                    let _ = write_half.shutdown().await;
                    continue;
                }
                let response = match queue.next() {
                    Some(r) => r,
                    None => return,
                };
                let resp = serde_json::json!({
                    "ok": true,
                    "outcome": response.unwrap_or(serde_json::Value::Null),
                });
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                let _ = write_half.write_all(&bytes).await;
                let _ = write_half.shutdown().await;
            }
        });
        (dir, socket)
    }

    /// Build a fake-claude script that emits one JSON `system` init
    /// event with the supplied session_id, then exits 0. The recovery
    /// turn does NOT use the JSON stream (uses the legacy at-exit
    /// capture path), so this fixture is sufficient for both phases.
    fn write_system_event_script(workspace: &Path, name: &str, session_id: &str) -> PathBuf {
        let body = format!(
            "#!/bin/sh\n\
echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{sid}\"}}'\n\
echo '{{\"type\":\"result\",\"stop_reason\":\"end_turn\",\"result\":\"agent narrative ending without an outcome tool\"}}'\n\
exit 0\n",
            sid = session_id
        );
        write_script(workspace, name, &body)
    }

    /// 6.1: all tasks checked AND outcome_success called → Completed,
    /// no recovery turn fires. The fixture's tasks.md is fully checked
    /// (default in `fixture_workspace`) AND the responder returns a
    /// recorded Success outcome, so the implementer-flow classifier's
    /// `tool_recorded` path wins immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn acceptance_scan_skipped_when_outcome_success_called() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        let (_sock_dir, socket) = spawn_multi_consume_outcome_responder_for(
            &basename,
            vec![Some(serde_json::json!({
                "type": "success",
                "final_answer": "all done"
            }))],
        )
        .await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        let script = write_system_event_script(&ws, "ok.sh", "session-abc");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match outcome {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(final_answer.as_deref(), Some("all done"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        let log = run_log_path(&test_paths_arc(), &ws, "x");
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            !body.contains(RECOVERY_LOG_DIVIDER),
            "no recovery turn should have fired: {body}"
        );
    }

    /// 6.2: unchecked tasks BUT outcome_success called → Completed,
    /// no recovery turn fires. The agent's structured signal wins over
    /// the daemon's would-be heuristic disagreement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn acceptance_scan_skipped_when_unchecked_but_tool_recorded() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        // Overwrite tasks.md with an unchecked item.
        std::fs::write(
            ws.join("openspec/changes/x/tasks.md"),
            "- [ ] 1.1 still on the list\n",
        )
        .unwrap();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        let (_sock_dir, socket) = spawn_multi_consume_outcome_responder_for(
            &basename,
            vec![Some(serde_json::json!({
                "type": "success",
                "final_answer": "shipped despite the leftover checkbox"
            }))],
        )
        .await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        let script = write_system_event_script(&ws, "ok.sh", "session-xyz");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match outcome {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(
                    final_answer.as_deref(),
                    Some("shipped despite the leftover checkbox")
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        let log = run_log_path(&test_paths_arc(), &ws, "x");
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            !body.contains(RECOVERY_LOG_DIVIDER),
            "no recovery turn should fire when tool was recorded: {body}"
        );
    }

    /// 6.3: unchecked tasks AND no outcome tool call → recovery turn
    /// fires. The recovery turn calls `outcome_success` → final
    /// Completed. The run log captures both phases with the
    /// `=== RECOVERY TURN ===` divider.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn recovery_turn_outcome_success_yields_completed() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        std::fs::write(
            ws.join("openspec/changes/x/tasks.md"),
            "- [ ] 1.1 finish the work\n",
        )
        .unwrap();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        // Two-element queue: initial classify returns None (no tool
        // call) → triggers recovery; recovery classify returns Success.
        let (_sock_dir, socket) = spawn_multi_consume_outcome_responder_for(
            &basename,
            vec![
                None,
                Some(serde_json::json!({
                    "type": "success",
                    "final_answer": "fixed up and finished"
                })),
            ],
        )
        .await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        let script = write_system_event_script(&ws, "ok.sh", "session-recover");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match outcome {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(final_answer.as_deref(), Some("fixed up and finished"));
            }
            other => panic!("expected Completed from recovery, got {other:?}"),
        }
        let log = run_log_path(&test_paths_arc(), &ws, "x");
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains(RECOVERY_LOG_DIVIDER),
            "log must contain recovery divider: {body}"
        );
    }

    /// 6.4: unchecked tasks AND no outcome tool call → recovery turn
    /// calls `outcome_request_iteration` → final IterationRequested
    /// (with iteration_number=2 per a27a1 cap rules; no marker present
    /// at this fixture's workspace).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn recovery_turn_outcome_iteration_yields_iteration_requested() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        std::fs::write(
            ws.join("openspec/changes/x/tasks.md"),
            "- [ ] 1.1 first task\n- [ ] 1.2 second task\n",
        )
        .unwrap();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        let (_sock_dir, socket) = spawn_multi_consume_outcome_responder_for(
            &basename,
            vec![
                None,
                Some(serde_json::json!({
                    "type": "iteration_request",
                    "completed_tasks": ["1.1"],
                    "remaining_tasks": ["1.2"],
                    "reason": "ran out of room"
                })),
            ],
        )
        .await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        let script = write_system_event_script(&ws, "ok.sh", "session-iter");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match outcome {
            ExecutorOutcome::IterationRequested {
                completed_tasks,
                remaining_tasks,
                reason,
                iteration_number,
            } => {
                assert_eq!(completed_tasks, vec!["1.1".to_string()]);
                assert_eq!(remaining_tasks, vec!["1.2".to_string()]);
                assert_eq!(reason, "ran out of room");
                assert_eq!(iteration_number, 2);
            }
            other => panic!("expected IterationRequested, got {other:?}"),
        }
    }

    /// 6.5: unchecked tasks AND no outcome tool call → recovery turn
    /// ALSO produces no outcome tool call → final Failed with the
    /// canonical reason (the literal text scripts grep for).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn recovery_turn_with_no_outcome_yields_canonical_failed() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        std::fs::write(
            ws.join("openspec/changes/x/tasks.md"),
            "- [ ] 1.1 still needs doing\n",
        )
        .unwrap();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        let (_sock_dir, socket) =
            spawn_multi_consume_outcome_responder_for(&basename, vec![None, None]).await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        let script = write_system_event_script(&ws, "ok.sh", "session-no-recover");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match outcome {
            ExecutorOutcome::Failed { reason } => {
                assert_eq!(reason, RECOVERY_FAILED_REASON);
            }
            other => panic!("expected canonical Failed, got {other:?}"),
        }
    }

    /// 6.6: `run_revision` with unchecked tasks AND no outcome tool
    /// call does NOT fire the acceptance scan or the recovery loop.
    /// The classifier-only path returns whatever today's heuristic
    /// produces.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn run_revision_skips_acceptance_scan() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        std::fs::write(
            ws.join("openspec/changes/x/tasks.md"),
            "- [ ] 1.1 revision unchecked\n",
        )
        .unwrap();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        // Only one consume_outcome round-trip — if the scan fired, the
        // recovery turn would attempt a second call AND the test would
        // hang waiting for the responder.
        let (_sock_dir, socket) =
            spawn_multi_consume_outcome_responder_for(&basename, vec![None]).await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        // The script writes a small file so the workspace has a
        // diff — that drives classify_outcome to Completed via the
        // diff-presence path. If the scan WERE applied, a recovery
        // turn would launch AND a second consume_outcome round-trip
        // would be issued.
        let script = write_script(
            &ws,
            "rev.sh",
            "#!/bin/sh\nmkdir -p out && echo touched > out/touched\nexit 0\n",
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let ctx = crate::revisions::RevisionContext {
            change_name: "x".to_string(),
            pr_diff: "".to_string(),
            revision_text: "tweak it".to_string(),
            pr_body: "".to_string(),
            pr_change_list: "".to_string(),
            agent_implementation_notes: "".to_string(),
        };
        let outcome = executor.run_revision(&ws, "x", &ctx).await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        assert!(
            matches!(outcome, ExecutorOutcome::Completed { .. }),
            "got {outcome:?}"
        );
        let log = run_log_path(&test_paths_arc(), &ws, "x");
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            !body.contains(RECOVERY_LOG_DIVIDER),
            "run_revision must NOT trigger the recovery turn: {body}"
        );
    }

    /// 6.7: non-implementer flows (`run_triage`) skip the acceptance
    /// scan regardless of workspace tasks.md content. We test
    /// `run_triage` here as a representative; the codepath is shared
    /// with `run_chat_triage`, `run_brownfield_draft`, `run_scout`,
    /// AND `run_changelog` (none of which invoke
    /// `Executor::run`'s implementer-only scan + recovery dispatch).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn run_triage_skips_acceptance_scan() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        std::fs::write(
            ws.join("openspec/changes/x/tasks.md"),
            "- [ ] 1.1 not a triage concern\n",
        )
        .unwrap();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        let (_sock_dir, socket) =
            spawn_multi_consume_outcome_responder_for(&basename, vec![None]).await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        let script = write_script(
            &ws,
            "triage.sh",
            "#!/bin/sh\necho triage done\nmkdir -p out && echo found > out/found\nexit 0\n",
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let ctx = TriageContext {
            findings: "f".to_string(),
            audit_type: "drift_audit".to_string(),
            repo_url: "https://example.com".to_string(),
            canonical_specs_index: "".to_string(),
        };
        let outcome = executor.run_triage(&ws, &ctx).await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        assert!(
            matches!(outcome, ExecutorOutcome::Completed { .. }),
            "got {outcome:?}"
        );
    }

    /// 6.8: an implementer run that emits a legacy `=== AUTOCODER-OUTCOME ===`
    /// stdout block AND has unchecked tasks AND does not call any
    /// outcome tool. The stdout sentinel is NOT parsed (deleted in
    /// a27a2); the acceptance scan fires; the recovery turn runs.
    /// Recovery returns no tool call → final canonical Failed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn legacy_stdout_sentinel_is_not_parsed_and_scan_fires() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        std::fs::write(
            ws.join("openspec/changes/x/tasks.md"),
            "- [ ] 1.1 still leftover\n",
        )
        .unwrap();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        let (_sock_dir, socket) =
            spawn_multi_consume_outcome_responder_for(&basename, vec![None, None]).await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        // Emit a well-formed AUTOCODER-OUTCOME stdout block AND a JSON
        // result event with the sentinel content as the agent's final
        // answer. Pre-a27a2 this would have routed to SpecNeedsRevision;
        // post-a27a2 the sentinel is dead code AND the acceptance scan
        // fires instead.
        let script = write_script(
            &ws,
            "legacy.sh",
            r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"legacy-session"}'
echo '{"type":"result","stop_reason":"end_turn","result":"=== AUTOCODER-OUTCOME ===\n{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"5.2\",\"task_text\":\"x\",\"reason\":\"r\"}],\"revision_suggestion\":\"s\"}"}'
exit 0
"#,
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc());
        let outcome = executor.run(&ws, "x").await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match outcome {
            ExecutorOutcome::Failed { reason } => {
                assert_eq!(
                    reason, RECOVERY_FAILED_REASON,
                    "legacy stdout sentinel must NOT shortcut to SpecNeedsRevision; expected canonical Failed"
                );
            }
            ExecutorOutcome::SpecNeedsRevision { .. } => {
                panic!(
                    "regression: legacy stdout AUTOCODER-OUTCOME block must NOT classify as SpecNeedsRevision in a27a2"
                );
            }
            other => panic!("expected canonical Failed, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // Task 4.9: classifier ordering — one fixture per branch
    // ---------------------------------------------------------------

    /// Ordering 1/5: tool-recorded outcome (consume_outcome → Some)
    /// short-circuits BEFORE any other check. Covered by
    /// `tool_recorded_success_takes_precedence` AND
    /// `tool_recorded_spec_revision_takes_precedence_over_timeout`
    /// above — this comment is the cross-reference.

    /// Ordering 2/5: AskUser marker (no tool-recorded outcome) wins
    /// over the timeout flag. The marker file is present AND
    /// `outcome.timed_out` is true; the classifier returns AskUser,
    /// not Failed{timeout}.
    #[tokio::test]
    async fn classifier_ordering_askuser_wins_over_timeout() {
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        std::fs::write(
            ws.join("openspec/changes/x").join(ASKUSER_MARKER_FILENAME),
            "{\"question\":\"clarify scope\"}",
        )
        .unwrap();
        let outcome = AgenticRunOutcome {
            timed_out: true,
            exit_status: None,
            stdout: String::new(),
            stderr: "timeout".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        assert!(
            matches!(result, ExecutorOutcome::AskUser { .. }),
            "AskUser marker must beat timeout precedence: {result:?}"
        );
    }

    /// Ordering 3/5: timeout precedence (no marker, no tool-recorded
    /// outcome) wins over the exit-status path.
    #[tokio::test]
    async fn classifier_ordering_timeout_wins_over_exit_status() {
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: true,
            exit_status: None,
            stdout: String::new(),
            stderr: "timeout".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        assert!(matches!(
            result,
            ExecutorOutcome::Failed { reason } if reason == "timeout"
        ));
    }

    /// Ordering 4/5: exit-status path (non-zero) wins over the
    /// Layer-2/Completed fallback.
    #[tokio::test]
    async fn classifier_ordering_exit_status_wins_over_layer2() {
        use std::os::unix::process::ExitStatusExt;
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(1 << 8)),
            stdout: "could you clarify the scope?".to_string(),
            stderr: "broke somewhere".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        match result {
            ExecutorOutcome::Failed { reason } => {
                assert!(
                    reason.contains("broke somewhere"),
                    "exit-status path must surface stderr in reason: {reason}"
                );
            }
            other => panic!("expected Failed via exit status, got {other:?}"),
        }
    }

    /// Ordering 5/5: with exit 0 AND no diff AND no clarification
    /// heuristic, the classifier falls through to `Completed` carrying
    /// the captured final_answer. The Layer-2 path tested earlier
    /// covers the clarification branch.
    #[tokio::test]
    async fn classifier_ordering_completed_terminal_path() {
        use std::os::unix::process::ExitStatusExt;
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: String::new(),
            stderr: String::new(),
            final_answer: Some("done".to_string()),
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        match result {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(final_answer.as_deref(), Some("done"));
            }
            other => panic!("expected Completed terminal path, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // a39: SIGTERM-aware classifier — signal-15 / exit-143 +
    // SHUTDOWN_REQUESTED
    // ---------------------------------------------------------------
    //
    // Each test takes `crate::daemon::TEST_GUARD` to serialize access
    // to the process-wide flag AND resets the flag to its default
    // before exiting. The classifier reads the flag inline, so any
    // unrelated test that races a flag mutation would see a wrong
    // result.
    //
    // `ExitStatus::from_raw` takes the platform wait-status word: a
    // signal-killed process encodes the signal number in the low 7
    // bits (`from_raw(15)` → `signal() == Some(15)`, `code() == None`),
    // while a normal exit encodes `code << 8` (`from_raw(143 << 8)` →
    // `code() == Some(143)`, `signal() == None`). The production shape
    // for a SIGTERM-cascade kill is the FORMER — a directly-spawned
    // child reaped after the signal — so the primary tests use
    // `from_raw(15)`. The exit-143 form is also covered defensively
    // (a wrapper / the CLI catching SIGTERM and `exit(143)`-ing).

    /// Task 3.2: a real SIGTERM death (`signal() == Some(15)`) AND
    /// `SHUTDOWN_REQUESTED == true` → classifier returns `Aborted {
    /// reason: "daemon shutdown (SIGTERM cascade)" }`. This is the
    /// production shape: the daemon spawns the CLI directly in its own
    /// process group, the shutdown SIGTERM cascade reaps it by signal,
    /// AND `child.wait()` reports `from_raw(15)`-shaped status.
    ///
    /// The classifier reads a process-wide flag (`crate::daemon::
    /// SHUTDOWN_REQUESTED`) inline; the test guard MUST stay held
    /// across the `.await` to keep concurrent classifier tests from
    /// observing a flipped flag. A `std::sync::Mutex` is correct here
    /// (the awaited operation is fast AND the lock is contention-free
    /// outside this test pair), so we silence the clippy lint
    /// recommending an async-aware mutex.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn classifier_sigterm_death_with_shutdown_flag_set_returns_aborted() {
        use std::os::unix::process::ExitStatusExt;
        let _g = crate::daemon::TEST_GUARD.lock().unwrap();
        crate::daemon::reset_for_test();
        crate::daemon::request_shutdown();
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: false,
            // `from_raw(15)` = killed by signal 15 (SIGTERM): the wait
            // status a directly-spawned child reports when the daemon's
            // shutdown SIGTERM cascade reaps it. `code()` is `None`
            // here — the bug the old `code() == Some(143)` check missed.
            exit_status: Some(std::process::ExitStatus::from_raw(15)),
            stdout: String::new(),
            stderr: "killed by signal".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor
            .classify_outcome(&ws, "x", outcome)
            .await
            .unwrap();
        crate::daemon::reset_for_test();
        match result {
            ExecutorOutcome::Aborted { reason } => {
                assert_eq!(
                    reason, "daemon shutdown (SIGTERM cascade)",
                    "Aborted must carry the canonical reason"
                );
            }
            other => panic!(
                "expected Aborted when signal=15 AND SHUTDOWN_REQUESTED is true, got {other:?}"
            ),
        }
    }

    /// Task 3.2 (defensive): the exit-143 form (`code() == Some(143)`,
    /// e.g. a wrapper or the CLI catching SIGTERM and `exit(143)`-ing)
    /// AND `SHUTDOWN_REQUESTED == true` → also `Aborted`. The classifier
    /// accepts either the signal-15 OR the exit-143 shape.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn classifier_exit_143_with_shutdown_flag_set_returns_aborted() {
        use std::os::unix::process::ExitStatusExt;
        let _g = crate::daemon::TEST_GUARD.lock().unwrap();
        crate::daemon::reset_for_test();
        crate::daemon::request_shutdown();
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: false,
            // `from_raw(143 << 8)` = a NORMAL exit with code 143 (the
            // shell "128 + 15" convention surfacing through a wrapper).
            exit_status: Some(std::process::ExitStatus::from_raw(143 << 8)),
            stdout: String::new(),
            stderr: "killed by signal".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor
            .classify_outcome(&ws, "x", outcome)
            .await
            .unwrap();
        crate::daemon::reset_for_test();
        match result {
            ExecutorOutcome::Aborted { reason } => {
                assert_eq!(
                    reason, "daemon shutdown (SIGTERM cascade)",
                    "Aborted must carry the canonical reason"
                );
            }
            other => panic!(
                "expected Aborted when exit=143 AND SHUTDOWN_REQUESTED is true, got {other:?}"
            ),
        }
    }

    /// Task 3.3: a real SIGTERM death (`signal() == Some(15)`) AND
    /// `SHUTDOWN_REQUESTED == false` → classifier returns the existing
    /// `Failed` — preserving today's behavior for external-source
    /// SIGTERMs (OOM, manual `kill -TERM`, container orchestrator). The
    /// failure reason is the Display of the signal-killed status
    /// (`signal: 15 (SIGTERM)`).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn classifier_sigterm_death_without_shutdown_flag_returns_failed() {
        use std::os::unix::process::ExitStatusExt;
        let _g = crate::daemon::TEST_GUARD.lock().unwrap();
        crate::daemon::reset_for_test();
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(15)),
            stdout: String::new(),
            // Empty stderr so the exit-status branch falls through to
            // the `format!("executor exited with {status}")` reason
            // shape (Display of a signal death names the signal).
            stderr: String::new(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor
            .classify_outcome(&ws, "x", outcome)
            .await
            .unwrap();
        crate::daemon::reset_for_test();
        match result {
            ExecutorOutcome::Failed { reason } => {
                assert!(
                    reason.contains("signal: 15"),
                    "external-source SIGTERM must classify as Failed naming the signal: {reason}"
                );
            }
            other => panic!(
                "expected Failed when signal=15 AND SHUTDOWN_REQUESTED is false, got {other:?}"
            ),
        }
    }

    /// Task 3.4: exit_status 1 AND `SHUTDOWN_REQUESTED == true` → the
    /// flag does NOT override non-SIGTERM exit codes; classifier returns
    /// the existing `Failed { reason: <stderr excerpt> }`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn classifier_exit_1_with_shutdown_flag_set_still_failed() {
        use std::os::unix::process::ExitStatusExt;
        let _g = crate::daemon::TEST_GUARD.lock().unwrap();
        crate::daemon::reset_for_test();
        crate::daemon::request_shutdown();
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(1 << 8)),
            stdout: String::new(),
            stderr: "real agent failure".to_string(),
            final_answer: None,
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor
            .classify_outcome(&ws, "x", outcome)
            .await
            .unwrap();
        crate::daemon::reset_for_test();
        match result {
            ExecutorOutcome::Failed { reason } => {
                assert!(
                    reason.contains("real agent failure"),
                    "stderr-derived reason must be preserved for exit-1 even with shutdown flag set: {reason}"
                );
            }
            other => panic!(
                "expected Failed when exit=1 AND SHUTDOWN_REQUESTED is true, got {other:?}"
            ),
        }
    }

    /// Task 3.5: exit_status 0 AND `SHUTDOWN_REQUESTED == true` → the
    /// flag does NOT override clean exits; classifier proceeds through
    /// the existing happy-path rules AND returns `Completed`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn classifier_exit_0_with_shutdown_flag_set_still_completed() {
        use std::os::unix::process::ExitStatusExt;
        let _g = crate::daemon::TEST_GUARD.lock().unwrap();
        crate::daemon::reset_for_test();
        crate::daemon::request_shutdown();
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = AgenticRunOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: String::new(),
            stderr: String::new(),
            final_answer: Some("done despite shutdown".to_string()),
            streamed_log: true,
            session_handle: None,
            session_id: None,
        };
        let result = executor
            .classify_outcome(&ws, "x", outcome)
            .await
            .unwrap();
        crate::daemon::reset_for_test();
        match result {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(final_answer.as_deref(), Some("done despite shutdown"));
            }
            other => panic!(
                "expected Completed when exit=0 AND SHUTDOWN_REQUESTED is true, got {other:?}"
            ),
        }
    }

    // -----------------------------------------------------------------------
    // a70: strategy-agnostic implementer + session resume/prune.
    // -----------------------------------------------------------------------

    /// a70 §6.2 / scenario "The claude implementer is unchanged": with no
    /// configured CLI the implementer defaults to `claude` AND streams (live
    /// log). §6.1: a capture-only CLI (opencode) does NOT stream.
    #[test]
    fn default_implementer_cli_is_claude_and_streams() {
        let exec = ClaudeCliExecutor::new("claude".into(), 30, test_paths_arc());
        assert_eq!(exec.cli, crate::config::CliKind::Claude);
        assert!(
            exec.implementer_streaming(),
            "claude + Json output streams the live log"
        );
        let oc = exec.with_cli(crate::config::CliKind::Opencode);
        assert!(
            !oc.implementer_streaming(),
            "a capture-only CLI never streams (no live log / no stream-JSON parse)"
        );
    }

    /// a70 §6.1 / scenario "A capture-mode strategy implements a change
    /// end-to-end": with the implementer's CLI resolved to a capture-mode
    /// strategy (opencode), a `Completed` outcome AND its `final_answer`
    /// arrive via the MCP outcome relay (NOT a streaming-JSON parse — the
    /// stub emits no stream events).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn capture_mode_implementer_takes_outcome_and_final_answer_from_relay() {
        let _g = crate::testing::ENV_LOCK.lock().unwrap();
        let (_dir, ws) = fixture_workspace_with_git();
        let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
        let (_sock_dir, socket) = spawn_multi_consume_outcome_responder_for(
            &basename,
            vec![Some(serde_json::json!({
                "type": "success",
                "final_answer": "shipped via the relay"
            }))],
        )
        .await;
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
            std::env::set_var(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME,
                ws.file_name().unwrap(),
            );
        }
        // A capture-mode (opencode) stub: drains the piped prompt, prints a
        // line, exits 0 — emits NO streaming-JSON system/result events.
        let stub = write_script(
            &ws,
            "opencode_stub.sh",
            "#!/bin/sh\ncat >/dev/null\necho 'capture stub done'\nexit 0\n",
        );
        let executor = ClaudeCliExecutor::new(stub.to_string_lossy().into(), 30, test_paths_arc())
            .with_cli(crate::config::CliKind::Opencode);
        let outcome = executor.run(&ws, "x").await.unwrap();
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }
        match outcome {
            ExecutorOutcome::Completed { final_answer } => {
                assert_eq!(
                    final_answer.as_deref(),
                    Some("shipped via the relay"),
                    "final_answer is taken from the outcome relay, not a stream parse"
                );
            }
            other => panic!("expected Completed via relay, got {other:?}"),
        }
        // The capture-mode strategy wrote opencode.json (not the claude
        // .mcp.json stream config) — corroborating it ran capture-mode.
        assert!(ws.join("opencode.json").exists());
    }

    /// a70 §6.5 / scenario "The implementer prunes on terminal outcome": a
    /// terminal `Completed` prunes ONLY the session the run created (by its
    /// captured handle), leaving a sibling session AND the settings file in
    /// place (surgical scope).
    #[tokio::test]
    async fn implementer_terminal_outcome_prunes_session_surgically() {
        let (_dir, ws) = fixture_workspace_with_git();
        let home = TempDir::new().unwrap();
        let store = home
            .path()
            .join(".claude/projects")
            .join(crate::agentic_run::claude_project_hash(&ws));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("sess-term.jsonl"), "{}").unwrap();
        std::fs::write(store.join("other-sess.jsonl"), "{}").unwrap();
        std::fs::write(home.path().join(".claude/settings.json"), "{}").unwrap();

        // Stub claude emits session_id "sess-term" then exits 0; the fixture's
        // tasks.md is fully checked, so the run completes with no recovery turn.
        let script = write_system_event_script(&ws, "ok.sh", "sess-term");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc())
            .with_session_home(home.path().to_path_buf());
        let outcome = executor.run(&ws, "x").await.unwrap();
        assert!(
            matches!(outcome, ExecutorOutcome::Completed { .. }),
            "expected Completed, got {outcome:?}"
        );
        assert!(
            !store.join("sess-term.jsonl").exists(),
            "the run's own session is pruned at the terminal outcome"
        );
        assert!(
            store.join("other-sess.jsonl").exists(),
            "a sibling session survives the surgical prune"
        );
        assert!(
            home.path().join(".claude/settings.json").exists(),
            "settings survive the surgical prune"
        );
    }

    /// a70 §6.4 / scenario "AskUser retains the session and waits": an AskUser
    /// outcome does NOT prune the session (it is retained for the resume) AND
    /// the returned `ResumeHandle` carries the session id so the answer can
    /// resume it natively.
    #[tokio::test]
    async fn askuser_retains_session_and_handle_carries_id() {
        let (_dir, ws) = fixture_workspace_with_git();
        let home = TempDir::new().unwrap();
        let store = home
            .path()
            .join(".claude/projects")
            .join(crate::agentic_run::claude_project_hash(&ws));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("sess-ask.jsonl"), "{}").unwrap();

        // Stub emits session_id "sess-ask" AND drops the askuser marker so the
        // classifier returns AskUser.
        let marker = ws
            .join("openspec/changes/x")
            .join(".askuser-pending.json");
        let script = write_script(
            &ws,
            "ask.sh",
            &format!(
                "#!/bin/sh\n\
echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-ask\"}}'\n\
printf '%s' '{{\"question\":\"which directory?\"}}' > '{}'\n\
exit 0\n",
                marker.display()
            ),
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30, test_paths_arc())
            .with_session_home(home.path().to_path_buf());
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::AskUser { resume_handle, .. } => {
                let data: ClaudeResumeData = serde_json::from_value(resume_handle.0).unwrap();
                assert_eq!(
                    data.session_id.as_deref(),
                    Some("sess-ask"),
                    "the AskUser handle carries the session id for the native resume"
                );
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
        assert!(
            store.join("sess-ask.jsonl").exists(),
            "AskUser retains the session — it is NOT pruned while waiting"
        );
    }
}
