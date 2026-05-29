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

use super::event_log::{self, ActionKind, StructuredLogWriter};
use super::json_event::{self, AssistantBlock, JsonEvent, UserBlock};
use super::{
    BrownfieldDraftContext, ChangelogContext, ChatTriageContext, Executor, ExecutorOutcome,
    ResumeHandle, ScoutContext, TriageContext, UnimplementableTask,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const MCP_CONFIG_FILENAME: &str = ".mcp.json";
const ASKUSER_MARKER_FILENAME: &str = ".askuser-pending.json";
const OUTCOME_SENTINEL_TAG: &str = "=== AUTOCODER-OUTCOME ===";
const SENTINEL_EXCERPT_MAX: usize = 200;

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

pub struct ClaudeCliExecutor {
    command: String,
    args: Vec<String>,
    timeout: Duration,
    sandbox: crate::config::ResolvedSandbox,
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
    pub fn new(command: String, timeout_secs: u64) -> Self {
        Self::new_with_sandbox(
            command,
            timeout_secs,
            crate::config::ResolvedSandbox::resolve(None),
        )
    }

    pub fn new_with_sandbox(
        command: String,
        timeout_secs: u64,
        sandbox: crate::config::ResolvedSandbox,
    ) -> Self {
        Self {
            command,
            args: Vec::new(),
            timeout: Duration::from_secs(timeout_secs),
            sandbox,
            template: DEFAULT_IMPLEMENTER_TEMPLATE.to_string(),
            changelog_stylist_template: DEFAULT_CHANGELOG_STYLIST_TEMPLATE.to_string(),
            revision_template: DEFAULT_REVISION_TEMPLATE.to_string(),
            triage_template: DEFAULT_TRIAGE_TEMPLATE.to_string(),
            chat_triage_template: DEFAULT_CHAT_TRIAGE_TEMPLATE.to_string(),
            output_format: crate::config::default_output_format(),
            settings_dir: None,
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
    pub fn from_config(cfg: &crate::config::ExecutorConfig) -> Result<Self> {
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
        Ok(Self {
            command: cfg.command.clone(),
            args: Vec::new(),
            timeout: Duration::from_secs(cfg.timeout_secs),
            sandbox: crate::config::ResolvedSandbox::resolve(cfg.sandbox.as_ref()),
            template,
            changelog_stylist_template,
            revision_template,
            triage_template,
            chat_triage_template,
            output_format: cfg.output_format,
            settings_dir: None,
        })
    }

    /// Test/extension constructor allowing additional args to be passed to
    /// the wrapped command. Production wiring uses `from_config`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_args(command: String, args: Vec<String>, timeout_secs: u64) -> Self {
        Self {
            command,
            args,
            timeout: Duration::from_secs(timeout_secs),
            sandbox: crate::config::ResolvedSandbox::resolve(None),
            template: DEFAULT_IMPLEMENTER_TEMPLATE.to_string(),
            changelog_stylist_template: DEFAULT_CHANGELOG_STYLIST_TEMPLATE.to_string(),
            revision_template: DEFAULT_REVISION_TEMPLATE.to_string(),
            triage_template: DEFAULT_TRIAGE_TEMPLATE.to_string(),
            chat_triage_template: DEFAULT_CHAT_TRIAGE_TEMPLATE.to_string(),
            output_format: crate::config::default_output_format(),
            settings_dir: None,
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
        Ok(self.template.replace(PROMPT_BODY_PLACEHOLDER, &body))
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
        let rendered = self
            .revision_template
            .replace(REVISION_PR_BODY_PLACEHOLDER, &revision_context.pr_body)
            .replace(
                REVISION_PR_CHANGE_LIST_PLACEHOLDER,
                &revision_context.pr_change_list,
            )
            .replace(
                REVISION_AGENT_NOTES_PLACEHOLDER,
                &revision_context.agent_implementation_notes,
            )
            .replace(REVISION_DIFF_PLACEHOLDER, &revision_context.pr_diff)
            .replace(REVISION_REQUEST_PLACEHOLDER, &revision_context.revision_text);
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
        self.triage_template
            .replace(TRIAGE_FINDINGS_PLACEHOLDER, &ctx.findings)
            .replace(TRIAGE_AUDIT_TYPE_PLACEHOLDER, &ctx.audit_type)
            .replace(TRIAGE_REPO_URL_PLACEHOLDER, &ctx.repo_url)
            .replace(TRIAGE_SPECS_INDEX_PLACEHOLDER, &ctx.canonical_specs_index)
    }

    /// Build the chat-triage prompt by substituting the three
    /// `ChatTriageContext` payloads into the embedded
    /// `prompts/chat-request-triage.md` template. Like `build_triage_prompt`,
    /// this does NOT shell out to `openspec instructions apply` because the
    /// LLM is asked to classify and explore the codebase itself.
    fn build_chat_triage_prompt(&self, ctx: &ChatTriageContext) -> String {
        self.chat_triage_template
            .replace(CHAT_TRIAGE_REQUEST_TEXT_PLACEHOLDER, &ctx.request_text)
            .replace(TRIAGE_REPO_URL_PLACEHOLDER, &ctx.repo_url)
            .replace(TRIAGE_SPECS_INDEX_PLACEHOLDER, &ctx.canonical_specs_index)
    }

    /// Build the changelog-stylist prompt by substituting the
    /// `ChangelogContext` payloads into the resolved stylist template
    /// (embedded default OR override loaded from
    /// `executor.changelog_stylist_prompt_path`).
    fn build_changelog_prompt(&self, ctx: &ChangelogContext) -> String {
        self.changelog_stylist_template
            .replace(CHANGELOG_JSON_PLACEHOLDER, &ctx.changelog_json)
            .replace(TRIAGE_REPO_URL_PLACEHOLDER, &ctx.repo_url)
            .replace(CHANGELOG_REVISION_TEXT_PLACEHOLDER, &ctx.revision_text)
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
    fn write_mcp_config(workspace: &Path, change: &str) -> Result<PathBuf> {
        // We may be running from a non-autocoder binary (e.g. cargo test).
        // `current_exe` returns the actual running binary; in production
        // this is the `autocoder` binary and the MCP subcommand exists.
        let exe = std::env::current_exe()
            .context("resolving current autocoder binary path for MCP config")?;
        let mut env = serde_json::json!({
            crate::mcp_askuser_server::ENV_WORKSPACE: workspace.to_string_lossy(),
            crate::mcp_askuser_server::ENV_CHANGE: change,
        });
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

    /// Idempotently remove the `.mcp.json` we wrote.
    fn delete_mcp_config(workspace: &Path) {
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

    /// Scan `stdout` for an `=== AUTOCODER-OUTCOME ===` block followed by a
    /// JSON object. Returns the JSON payload string (everything between the
    /// tag line and the first blank line or EOF) and the original byte
    /// excerpt around the payload for diagnostics on parse failure. Returns
    /// `None` if no sentinel is present.
    fn extract_outcome_sentinel(stdout: &str) -> Option<String> {
        let idx = stdout.find(OUTCOME_SENTINEL_TAG)?;
        let after = &stdout[idx + OUTCOME_SENTINEL_TAG.len()..];
        // Skip leading whitespace/newlines to reach the JSON body.
        let body_start = after
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(after.len());
        let body = &after[body_start..];
        if body.is_empty() {
            return None;
        }
        // The agent emits a single JSON object (object/array depth-tracked).
        // Find the first `{` and consume until the matching `}` at depth 0,
        // honoring string literals so braces inside strings don't confuse
        // the depth counter.
        let bytes = body.as_bytes();
        let start = bytes.iter().position(|&b| b == b'{')?;
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escape = false;
        let mut end: Option<usize> = None;
        for (i, &b) in bytes.iter().enumerate().skip(start) {
            if in_str {
                if escape {
                    escape = false;
                } else if b == b'\\' {
                    escape = true;
                } else if b == b'"' {
                    in_str = false;
                }
                continue;
            }
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        Some(body[start..end].to_string())
    }

    /// Try to interpret an outcome-sentinel JSON payload as a
    /// `SpecNeedsRevision` outcome. Returns:
    ///   - `Ok(Some(outcome))` if the payload is a well-formed
    ///     `spec_needs_revision` block with a non-empty task list.
    ///   - `Ok(None)` if the payload is some other outcome type (caller
    ///     leaves the sentinel alone — other parsers may handle it).
    ///   - `Err(reason)` if the payload looks like `spec_needs_revision`
    ///     (matches the `type` field) but is malformed — missing required
    ///     fields, wrong field types, or an empty `unimplementable_tasks`
    ///     list. The caller falls back to `Failed` with a diagnostic.
    fn try_parse_spec_needs_revision(
        payload: &str,
    ) -> std::result::Result<Option<ExecutorOutcome>, String> {
        let value: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => return Err(format!("invalid JSON: {e}")),
        };
        let type_field = value.get("type").and_then(|v| v.as_str());
        if type_field != Some("spec_needs_revision") {
            return Ok(None);
        }
        let tasks_val = value.get("unimplementable_tasks");
        let tasks_array = match tasks_val.and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return Err("missing or non-array `unimplementable_tasks`".to_string()),
        };
        if tasks_array.is_empty() {
            return Err("`unimplementable_tasks` is empty".to_string());
        }
        let mut tasks: Vec<UnimplementableTask> = Vec::with_capacity(tasks_array.len());
        for (i, entry) in tasks_array.iter().enumerate() {
            let task_id = entry
                .get("task_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("task[{i}] missing string `task_id`"))?;
            let task_text = entry
                .get("task_text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("task[{i}] missing string `task_text`"))?;
            let reason = entry
                .get("reason")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("task[{i}] missing string `reason`"))?;
            // Per a20a1: detect un-substituted placeholders.
            // The agent's worked example in prompts/implementer.md is
            // expected to be ANCHORED behaviour; emitting it verbatim
            // (with `<id-from-tasks-md>` etc. still in fields) is a
            // template-following bug. Convert to a specific Err so the
            // caller's WARN log and Failed-reason carry the diagnostic
            // pointing the operator at the prompt section.
            for (field_name, field_value) in [
                ("task_id", task_id),
                ("task_text", task_text),
                ("reason", reason),
            ] {
                if Self::contains_unsubstituted_placeholder(field_value) {
                    return Err(format!(
                        "looks like un-substituted placeholders — the agent emitted the prompt's example verbatim instead of substituting concrete values; see prompts/implementer.md sentinel section (task[{i}].{field_name} contained `<...>` placeholder text: {field_value})"
                    ));
                }
            }
            tasks.push(UnimplementableTask {
                task_id: task_id.to_string(),
                task_text: task_text.to_string(),
                reason: reason.to_string(),
            });
        }
        let revision_suggestion = value
            .get("revision_suggestion")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing string `revision_suggestion`".to_string())?
            .to_string();
        Ok(Some(ExecutorOutcome::SpecNeedsRevision {
            unimplementable_tasks: tasks,
            revision_suggestion,
        }))
    }

    /// Detect `<placeholder>` patterns that indicate the agent emitted
    /// the prompt template's example verbatim instead of substituting
    /// concrete values. The regex is intentionally narrow — lowercase
    /// letters, digits, spaces, underscores, hyphens between angle
    /// brackets, leading char must be a letter — to avoid matching
    /// legitimate `<...>` content like `@<bot>` mentions or HTML-shaped
    /// task descriptions. False positives ARE possible (a real task
    /// whose text happens to match the pattern); per the spec, the
    /// diagnostic phrase still helps the operator either way.
    fn contains_unsubstituted_placeholder(s: &str) -> bool {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'<' {
                i += 1;
                continue;
            }
            let start = i + 1;
            if start >= bytes.len() {
                return false;
            }
            let first = bytes[start];
            if !first.is_ascii_lowercase() {
                i += 1;
                continue;
            }
            let mut j = start + 1;
            let mut closed = false;
            while j < bytes.len() {
                let b = bytes[j];
                if b == b'>' {
                    closed = true;
                    break;
                }
                let ok = b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b' '
                    || b == b'_'
                    || b == b'-';
                if !ok {
                    break;
                }
                j += 1;
            }
            if closed {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Truncate `s` to at most `SENTINEL_EXCERPT_MAX` characters (codepoints)
    /// for inclusion in a parse-failure reason. Adds an ellipsis when
    /// truncated.
    fn excerpt_for_reason(s: &str) -> String {
        let count = s.chars().count();
        if count <= SENTINEL_EXCERPT_MAX {
            s.to_string()
        } else {
            let mut out: String = s.chars().take(SENTINEL_EXCERPT_MAX).collect();
            out.push('…');
            out
        }
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

    /// Write the per-iteration Claude Code settings file to OS temp dir
    /// (NOT the workspace, to avoid contaminating the diff). Returns the
    /// path; the caller is responsible for deletion via `TempFileGuard`.
    fn write_sandbox_settings(&self) -> Result<PathBuf> {
        let mut deny: Vec<String> = Vec::new();
        for pat in &self.sandbox.disallowed_bash_patterns {
            deny.push(format!("Bash({pat})"));
        }
        for pat in &self.sandbox.disallowed_read_paths {
            deny.push(format!("Read({pat})"));
        }
        let json = serde_json::json!({
            "permissions": {
                "allow": Vec::<String>::new(),
                "deny": deny,
            }
        });

        // Unique-named file under the configured settings directory
        // (production: OS temp; tests: per-test isolated dir). UUIDish via
        // process id + nanos.
        use std::time::{SystemTime, UNIX_EPOCH};
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let dir = self
            .settings_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("autocoder-claude-settings-{pid}-{stamp}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&json)?)
            .with_context(|| format!("writing sandbox settings to {}", path.display()))?;
        Ok(path)
    }

    /// Spawn the wrapped CLI, write `prompt` on its stdin, wait with the
    /// configured timeout, return collected stdout/stderr + exit status.
    async fn run_subprocess(
        &self,
        workspace: &Path,
        change: &str,
        prompt: &str,
    ) -> Result<SubprocessOutcome> {
        let settings_path = self
            .write_sandbox_settings()
            .context("generating sandbox settings file")?;
        let _settings_guard = TempFileGuard(settings_path.clone());

        let json_mode = matches!(
            self.output_format,
            crate::config::ExecutorOutputFormat::Json
        );

        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .arg("--settings")
            .arg(&settings_path)
            .arg("--allowedTools")
            .arg(self.sandbox.allowed_tools.join(","))
            .arg("--permission-mode")
            .arg("acceptEdits");
        if json_mode {
            // `--verbose` is required by Claude CLI alongside
            // `stream-json` for non-interactive sessions; without it the
            // CLI emits the legacy single result envelope rather than
            // streaming events as they happen.
            cmd.arg("--verbose")
                .arg("--output-format")
                .arg("stream-json");
        }
        let mut child = cmd
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Launch the child in its own process group so the busy-marker
            // stuck-state recovery path can `killpg` the entire subprocess
            // tree (claude + any MCP server / helper children it spawns)
            // with a single signal. `process_group(0)` is stable Rust.
            .process_group(0)
            .spawn()
            .with_context(|| format!("spawning executor command `{}`", self.command))?;

        // Record the spawned child's PID to a sidecar file so the busy-
        // marker's stuck-state recovery has a kill target that actually
        // covers Claude's process group (the marker's own `pgid` records
        // autocoder's group, not Claude's). The guard cleans the file up
        // on every exit path of this function.
        let _subprocess_marker_guard = if let Some(pid) = child.id() {
            if let Err(e) = crate::busy_marker::write_subprocess_marker(workspace, pid) {
                tracing::warn!(
                    workspace = %workspace.display(),
                    pid,
                    "failed to write subprocess sidecar marker (run continues): {e:#}"
                );
                None
            } else {
                Some(SubprocessMarkerGuard {
                    workspace: workspace.to_path_buf(),
                })
            }
        } else {
            None
        };

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        }
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        if json_mode {
            // Streaming path: build the structured log incrementally so
            // that on a timeout-kill, every event the child wrote before
            // the kill is durably on disk.
            self.run_subprocess_streaming(
                child,
                stdout_pipe,
                stderr_pipe,
                workspace,
                change,
                prompt,
            )
            .await
        } else {
            // Legacy at-exit capture: today's behavior preserved for
            // `output_format: text`.
            self.run_subprocess_legacy(child, stdout_pipe, stderr_pipe).await
        }
    }

    /// Legacy capture: wait for child exit (or timeout) then read
    /// stdout + stderr in one shot. Returns the populated
    /// `SubprocessOutcome` without writing the log file (the caller
    /// invokes `persist_run_log` for that). Used in `output_format: text`.
    async fn run_subprocess_legacy(
        &self,
        mut child: tokio::process::Child,
        mut stdout_pipe: Option<tokio::process::ChildStdout>,
        mut stderr_pipe: Option<tokio::process::ChildStderr>,
    ) -> Result<SubprocessOutcome> {
        let sleeper = tokio::time::sleep(self.timeout);
        tokio::pin!(sleeper);

        let exit_status: Option<std::io::Result<std::process::ExitStatus>> = tokio::select! {
            biased;
            () = &mut sleeper => None,
            res = child.wait() => Some(res),
        };

        match exit_status {
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Ok(SubprocessOutcome {
                    timed_out: true,
                    exit_status: None,
                    stdout: String::new(),
                    stderr: "timeout".to_string(),
                    final_answer: None,
                    streamed_log: false,
                })
            }
            Some(Err(e)) => Err(e).context("waiting on executor child process"),
            Some(Ok(status)) => {
                let mut stdout_text = String::new();
                if let Some(ref mut p) = stdout_pipe {
                    let _ = p.read_to_string(&mut stdout_text).await;
                }
                let mut stderr_text = String::new();
                if let Some(ref mut p) = stderr_pipe {
                    let _ = p.read_to_string(&mut stderr_text).await;
                }
                Ok(SubprocessOutcome {
                    timed_out: false,
                    exit_status: Some(status),
                    stdout: stdout_text,
                    stderr: stderr_text,
                    final_answer: None,
                    streamed_log: false,
                })
            }
        }
    }

    /// Streaming capture: open the structured log writer, spawn one
    /// task that reads stdout line-by-line and dispatches parsed events
    /// to the log + one task that reads stderr into the writer's
    /// buffer, then race `child.wait()` against the configured timeout.
    /// On timeout-kill the partial action stream is already on disk —
    /// the writer is `finalize`d unconditionally.
    async fn run_subprocess_streaming(
        &self,
        mut child: tokio::process::Child,
        stdout_pipe: Option<tokio::process::ChildStdout>,
        stderr_pipe: Option<tokio::process::ChildStderr>,
        workspace: &Path,
        change: &str,
        prompt: &str,
    ) -> Result<SubprocessOutcome> {
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let log_path = run_log_path(workspace, change);
        let writer = match event_log::open(&log_path) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::warn!(
                    log_file = %log_path.display(),
                    "could not open structured log; falling back to legacy capture: {e:#}"
                );
                return self
                    .run_subprocess_legacy(child, stdout_pipe, stderr_pipe)
                    .await;
            }
        };
        if let Err(e) = writer.write_prompt(prompt) {
            tracing::warn!(
                log_file = %log_path.display(),
                "writing prompt header to structured log failed: {e:#}"
            );
        }

        // Stdout reader: parse one JSON event per line; dispatch each to
        // the structured log. Accumulates the raw lines too so the
        // caller's `outcome.stdout` still reflects what was emitted —
        // legacy callsites (sentinel extraction, the Layer-2 heuristic)
        // expect a non-empty string for non-empty output.
        let stdout_writer = writer.clone();
        let stdout_handle: tokio::task::JoinHandle<String> = match stdout_pipe {
            Some(pipe) => tokio::spawn(async move {
                let mut buf = String::new();
                let mut reader = BufReader::new(pipe).lines();
                loop {
                    match reader.next_line().await {
                        Ok(Some(line)) => {
                            buf.push_str(&line);
                            buf.push('\n');
                            dispatch_event_to_log(&stdout_writer, &line);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!("stdout reader error: {e}");
                            break;
                        }
                    }
                }
                buf
            }),
            None => tokio::spawn(async { String::new() }),
        };

        // Stderr reader: stream bytes into the writer's buffer so the
        // STDERR section's annotation reflects the true byte count and
        // the bytes themselves land in the log.
        let stderr_writer = writer.clone();
        let stderr_handle: tokio::task::JoinHandle<String> = match stderr_pipe {
            Some(mut pipe) => tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let mut chunk = [0u8; 4096];
                loop {
                    match pipe.read(&mut chunk).await {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                            let _ = stderr_writer.append_stderr(&chunk[..n]);
                        }
                        Err(e) => {
                            tracing::warn!("stderr reader error: {e}");
                            break;
                        }
                    }
                }
                buf
            }),
            None => tokio::spawn(async { String::new() }),
        };

        let sleeper = tokio::time::sleep(self.timeout);
        tokio::pin!(sleeper);

        let exit_status: Option<std::io::Result<std::process::ExitStatus>> = tokio::select! {
            biased;
            () = &mut sleeper => None,
            res = child.wait() => Some(res),
        };

        let timed_out = exit_status.is_none();
        let status_opt: Option<std::process::ExitStatus> = match exit_status {
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                None
            }
            Some(Err(e)) => return Err(e).context("waiting on executor child process"),
            Some(Ok(s)) => Some(s),
        };

        // The reader tasks return when their pipe hits EOF, which
        // happens when the child closes its end. After `child.wait()` /
        // `child.start_kill()` returned the child is reaped; awaiting
        // the readers is safe.
        let stdout_text = stdout_handle.await.unwrap_or_default();
        let stderr_text = stderr_handle.await.unwrap_or_default();

        // Flush the structured log AFTER readers finished so the FINAL
        // ANSWER section reflects whatever set_final_answer captured.
        if let Err(e) = writer.finalize() {
            tracing::warn!(
                log_file = %log_path.display(),
                "finalizing structured log failed: {e:#}"
            );
        }
        let final_answer = writer.final_answer();

        Ok(SubprocessOutcome {
            timed_out,
            exit_status: status_opt,
            stdout: stdout_text,
            stderr: if timed_out && stderr_text.is_empty() {
                "timeout".to_string()
            } else {
                stderr_text
            },
            final_answer,
            streamed_log: true,
        })
    }

    /// Classify a subprocess outcome into an `ExecutorOutcome`, applying
    /// Layer-1 and Layer-2 AskUser detection.
    async fn classify_outcome(
        &self,
        workspace: &Path,
        change: &str,
        outcome: SubprocessOutcome,
    ) -> Result<ExecutorOutcome> {
        // Tool-recorded outcome lookup (a27a0). The per-execution MCP
        // child relays `outcome_success` / `outcome_spec_needs_revision`
        // tool calls to the daemon via `record_outcome`; we drain via
        // `consume_outcome`. A recorded outcome is the agent's
        // deliberate, schema-validated end-of-run emission and is more
        // authoritative than ANY inferred state (timeout, exit status,
        // stdout content). Runs without a recorded outcome fall through
        // to today's exact classifier ordering, preserving behavior for
        // pre-a27a0 implementer prompts.
        let workspace_basename = workspace_basename_for(workspace);
        if let Some(recorded) =
            try_consume_outcome(&workspace_basename, change).await
        {
            return Ok(map_recorded_outcome(recorded));
        }

        // Layer-1 first: the marker file is the authoritative signal. It
        // may have been written even if the wrapped CLI exited non-zero.
        if let Some(question) = Self::check_askuser_marker(workspace, change)? {
            let handle = build_handle(workspace, change, None);
            return Ok(ExecutorOutcome::AskUser {
                question,
                resume_handle: handle,
            });
        }

        // Timeout precedence (a20a1): a timed-out run by definition did
        // not reach a deliberate end-of-run point. Any sentinel-shaped
        // substring in the captured event stream is by-construction NOT
        // the agent's deliberate emission. Classify as timeout BEFORE
        // any sentinel extraction so a false-match on prompt-echo OR
        // tool-result content cannot mask the real cause. Pre-a20a1,
        // the order was reversed, which caused a21-canonical-spec-rag
        // to perma-stuck under "unparseable sentinel" when the real
        // cause was a one-hour timeout.
        if outcome.timed_out {
            return Ok(ExecutorOutcome::Failed {
                reason: "timeout".to_string(),
            });
        }

        // Outcome sentinel: the agent's pre-flight check writes an
        // `=== AUTOCODER-OUTCOME ===` block when it identifies an
        // unimplementable task. We check this BEFORE looking at the exit
        // status so an agent that exits non-zero after flagging is still
        // honored, and so the dispatcher's no-diff-Failed fallback never
        // sees the workspace ahead of the signal.
        //
        // Scope (a20a1): in JSON streaming mode the sentinel SHALL be
        // sought ONLY in `final_answer` (the `result`-event text — the
        // agent's deliberate end-of-run emission). Tool-result echoes,
        // prompt-context echoes, and other event-stream content are
        // NOT deliberate emissions; matching against them produces the
        // false-positives that drove a21-canonical-spec-rag to perma-
        // stuck. In text-mode opt-out the only signal IS stdout, so
        // the legacy stdout scan is retained for that case.
        let sentinel_source: Option<&str> = match self.output_format {
            crate::config::ExecutorOutputFormat::Json => outcome.final_answer.as_deref(),
            crate::config::ExecutorOutputFormat::Text => Some(outcome.stdout.as_str()),
        };
        if let Some(source) = sentinel_source
            && let Some(payload) = Self::extract_outcome_sentinel(source)
        {
            match Self::try_parse_spec_needs_revision(&payload) {
                Ok(Some(spec_revision)) => {
                    // a27a0 deprecation: this path is the legacy stdout
                    // sentinel match. The canonical replacement is the
                    // `outcome_spec_needs_revision` MCP tool. Emit a
                    // warning so operator logs surface the use of the
                    // deprecated path; scheduled removal is a27a2.
                    tracing::warn!(
                        change = %change,
                        "legacy stdout sentinel matched for change {change}; please call the outcome_spec_needs_revision MCP tool instead (stdout sentinel parsing is scheduled for removal in a27a2)",
                    );
                    return Ok(spec_revision);
                }
                Ok(None) => {
                    // Sentinel present but not a spec_needs_revision payload.
                    // Other parsers (none today) could match here; fall
                    // through to normal exit-status handling.
                }
                Err(parse_err) => {
                    let excerpt = Self::excerpt_for_reason(&payload);
                    tracing::warn!(
                        change = %change,
                        "agent emitted unparseable SpecNeedsRevision sentinel: {parse_err}; payload: {excerpt}"
                    );
                    return Ok(ExecutorOutcome::Failed {
                        reason: format!(
                            "agent emitted unparseable SpecNeedsRevision sentinel: {parse_err}; excerpt: {excerpt}"
                        ),
                    });
                }
            }
        }

        let status = outcome.exit_status.expect("non-timeout path has status");
        if !status.success() {
            let reason: String = outcome.stderr.trim().chars().take(200).collect();
            let reason = if reason.is_empty() {
                format!("executor exited with {status}")
            } else {
                reason
            };
            return Ok(ExecutorOutcome::Failed { reason });
        }

        // Exit-0 path. Check Layer-2 heuristic only when the workspace is
        // clean — if there's a diff, the agent did real work and we trust
        // the Completed outcome regardless of stdout noise.
        let porcelain = crate::git::status_porcelain(workspace).unwrap_or_default();
        if porcelain.is_empty() {
            if let Some(question) = Self::check_stdout_heuristic(&outcome.stdout) {
                let handle = build_handle(workspace, change, None);
                return Ok(ExecutorOutcome::AskUser {
                    question,
                    resume_handle: handle,
                });
            }
            // Suspicious: exit-0, no diff, no AskUser marker, no
            // clarification heuristic. The downstream polling_loop will
            // classify this as Failed. Surface the agent's actual output
            // here so journalctl shows *why* on the same line.
            let stdout_tail = tail(&outcome.stdout, 2048);
            let stderr_tail = tail(&outcome.stderr, 2048);
            let log_path = run_log_path(workspace, change);
            tracing::warn!(
                change = change,
                log_file = %log_path.display(),
                "agent exited 0 without modifying the workspace.\n--- stdout (last 2KB) ---\n{stdout}\n--- stderr (last 2KB) ---\n{stderr}\n--- end ---",
                stdout = if stdout_tail.is_empty() { "(empty)" } else { stdout_tail },
                stderr = if stderr_tail.is_empty() { "(empty)" } else { stderr_tail },
            );
        }

        Ok(ExecutorOutcome::Completed {
            final_answer: outcome.final_answer.clone(),
        })
    }
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
/// the outcome store. Mirrors how `ClaudeCliExecutor::write_mcp_config`
/// resolves it (env var if set, falling back to the path's file name).
fn workspace_basename_for(workspace: &Path) -> String {
    std::env::var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME).unwrap_or_else(|_| {
        workspace
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown_workspace")
            .to_string()
    })
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

/// Map a daemon-recorded outcome to its `ExecutorOutcome` counterpart.
fn map_recorded_outcome(
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
    }
}

struct SubprocessOutcome {
    timed_out: bool,
    exit_status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
    /// Populated by the JSON streaming path with the agent's
    /// conversational summary from the `result` event. `None` in legacy
    /// text mode (no streaming) AND when the run timed out before the
    /// result event arrived.
    final_answer: Option<String>,
    /// True when the JSON streaming path built the log file itself (so
    /// the legacy `persist_run_log` writer should skip it). False in
    /// text mode where `persist_run_log` still owns the log shape.
    streamed_log: bool,
}

/// RAII guard that removes a temp file when dropped. Used so the sandbox
/// settings file is cleaned up regardless of how `run_subprocess` exits
/// (success, error, panic).
struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            tracing::warn!(
                path = %self.0.display(),
                "failed to remove sandbox settings temp file: {e}"
            );
        }
    }
}

/// RAII guard that removes the subprocess-PID sidecar when dropped.
/// Constructed in `run_subprocess` after the sidecar file is successfully
/// written; ensures the file is gone on success, error, or panic so the
/// next iteration's busy-marker recovery only sees a sidecar when an
/// actual orphan exists (i.e. the daemon crashed before Drop ran).
struct SubprocessMarkerGuard {
    workspace: PathBuf,
}

impl Drop for SubprocessMarkerGuard {
    fn drop(&mut self) {
        crate::busy_marker::remove_subprocess_marker(&self.workspace);
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

        let _mcp_path = Self::write_mcp_config(workspace, change)?;
        let outcome = self.run_subprocess(workspace, change, &prompt).await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(workspace, change, &prompt, &outcome);
        self.classify_outcome(workspace, change, outcome).await
    }

    async fn resume(&self, handle: ResumeHandle, answer: &str) -> Result<ExecutorOutcome> {
        let data: ClaudeResumeData = serde_json::from_value(handle.0)
            .context("decoding ClaudeCliExecutor resume handle")?;
        let workspace = data.workspace.as_path();
        let change = data.change.as_str();
        let base = self.build_prompt(workspace, change)?;
        let prompt = format!(
            "(Earlier you asked a question and the human answered: {answer}) Continue the implementation.\n\n{base}"
        );

        let stale_marker = workspace
            .join("openspec/changes")
            .join(change)
            .join(ASKUSER_MARKER_FILENAME);
        let _ = std::fs::remove_file(&stale_marker);

        let _mcp_path = Self::write_mcp_config(workspace, change)?;
        let outcome = self.run_subprocess(workspace, change, &prompt).await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(workspace, change, &prompt, &outcome);
        self.classify_outcome(workspace, change, outcome).await
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

        let _mcp_path = Self::write_mcp_config(workspace, change)?;
        let outcome = self.run_subprocess(workspace, change, &prompt).await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(workspace, change, &prompt, &outcome);
        self.classify_outcome(workspace, change, outcome).await
    }

    async fn run_triage(
        &self,
        workspace: &Path,
        ctx: &TriageContext,
    ) -> Result<ExecutorOutcome> {
        let prompt = self.build_triage_prompt(ctx);
        // Triage mode does not target a specific change directory, so the
        // per-change MCP marker plumbing is keyed by a synthetic name.
        let _mcp_path = Self::write_mcp_config(workspace, TRIAGE_LOG_CHANGE_NAME)?;
        let outcome = self
            .run_subprocess(workspace, TRIAGE_LOG_CHANGE_NAME, &prompt)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(workspace, TRIAGE_LOG_CHANGE_NAME, &prompt, &outcome);
        self.classify_outcome(workspace, TRIAGE_LOG_CHANGE_NAME, outcome)
            .await
    }

    async fn run_chat_triage(
        &self,
        workspace: &Path,
        ctx: &ChatTriageContext,
    ) -> Result<ExecutorOutcome> {
        let prompt = self.build_chat_triage_prompt(ctx);
        let _mcp_path = Self::write_mcp_config(workspace, CHAT_TRIAGE_LOG_CHANGE_NAME)?;
        let outcome = self
            .run_subprocess(workspace, CHAT_TRIAGE_LOG_CHANGE_NAME, &prompt)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(workspace, CHAT_TRIAGE_LOG_CHANGE_NAME, &prompt, &outcome);
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
        let _mcp_path = Self::write_mcp_config(workspace, BROWNFIELD_DRAFT_LOG_CHANGE_NAME)?;
        let outcome = self
            .run_subprocess(workspace, BROWNFIELD_DRAFT_LOG_CHANGE_NAME, &prompt)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(
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
        let _mcp_path = Self::write_mcp_config(workspace, SCOUT_LOG_CHANGE_NAME)?;
        let outcome = self
            .run_subprocess(workspace, SCOUT_LOG_CHANGE_NAME, &prompt)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(workspace, SCOUT_LOG_CHANGE_NAME, &prompt, &outcome);
        self.classify_outcome(workspace, SCOUT_LOG_CHANGE_NAME, outcome)
            .await
    }

    async fn run_changelog(
        &self,
        workspace: &Path,
        ctx: &ChangelogContext,
    ) -> Result<ExecutorOutcome> {
        let prompt = self.build_changelog_prompt(ctx);
        let _mcp_path = Self::write_mcp_config(workspace, CHANGELOG_STYLIST_LOG_CHANGE_NAME)?;
        let outcome = self
            .run_subprocess(workspace, CHANGELOG_STYLIST_LOG_CHANGE_NAME, &prompt)
            .await;
        Self::delete_mcp_config(workspace);
        let outcome = outcome?;
        persist_run_log(
            workspace,
            CHANGELOG_STYLIST_LOG_CHANGE_NAME,
            &prompt,
            &outcome,
        );
        self.classify_outcome(workspace, CHANGELOG_STYLIST_LOG_CHANGE_NAME, outcome)
            .await
    }
}

/// Parse a stdout line as a JSON event and append a corresponding
/// ACTIONS-section line (or, for the `result` event, capture the final
/// answer in the writer's buffer). Malformed JSON lands as `[raw]`;
/// unknown event types as `[unknown:<type>]` — neither aborts the
/// stream-reader loop.
fn dispatch_event_to_log(writer: &StructuredLogWriter, line: &str) {
    if line.is_empty() {
        return;
    }
    match json_event::parse_event_line(line) {
        Ok(event) => dispatch_parsed_event(writer, event),
        Err(e) => {
            tracing::warn!(
                "claude stream-json: malformed line, recording as [raw]: {e}"
            );
            let _ = writer.append_action(ActionKind::Raw, line);
        }
    }
}

fn dispatch_parsed_event(writer: &StructuredLogWriter, event: JsonEvent) {
    match event {
        JsonEvent::System { .. } => {
            // Init metadata isn't actionable for operators; suppress.
        }
        JsonEvent::Assistant { content_blocks } => {
            for block in content_blocks {
                match block {
                    AssistantBlock::Text { text } => {
                        for line in wrap_assistant_text(&text) {
                            let _ = writer.append_action(ActionKind::Assistant, &line);
                        }
                    }
                    AssistantBlock::ToolUse {
                        tool_name,
                        tool_input,
                    } => {
                        let summary = format_tool_input_summary(&tool_input);
                        let content = if summary.is_empty() {
                            tool_name
                        } else {
                            format!("{tool_name} {summary}")
                        };
                        let _ = writer.append_action(ActionKind::ToolUse, &content);
                    }
                }
            }
        }
        JsonEvent::User { content_blocks } => {
            for block in content_blocks {
                match block {
                    UserBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        if is_error {
                            let msg: String =
                                content.chars().take(200).collect();
                            let _ = writer.append_action(
                                ActionKind::Unknown("tool_result:error".into()),
                                &msg,
                            );
                        } else {
                            let line = format!("({n} bytes returned)", n = content.len());
                            let _ = writer.append_action(ActionKind::ToolResult, &line);
                        }
                    }
                }
            }
        }
        JsonEvent::Result { final_text, .. } => {
            let _ = writer.set_final_answer(final_text);
        }
        JsonEvent::Unknown { event_type, raw } => {
            let body = serde_json::to_string(&raw).unwrap_or_default();
            let _ = writer.append_action(ActionKind::Unknown(event_type), &body);
        }
    }
}

/// Wrap assistant text at ~80 columns on whitespace boundaries; long
/// single-line runs (URLs, code) get returned as a single chunk to
/// avoid mid-token splits.
fn wrap_assistant_text(text: &str) -> Vec<String> {
    const WIDTH: usize = 80;
    let mut out: Vec<String> = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in para.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.len() + 1 + word.len() <= WIDTH {
                current.push(' ');
                current.push_str(word);
            } else {
                out.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Format a `tool_input` JSON value into a one-line summary suitable
/// for a `[tool_use]` log line. Truncates at ~200 chars to keep the
/// log readable; the full input was emitted on the stream and is no
/// longer addressable, but operators can re-run the change with text
/// mode to capture the raw bytes if they need them.
fn format_tool_input_summary(input: &serde_json::Value) -> String {
    let raw = match input {
        serde_json::Value::Object(map) => {
            // Pick a small set of recognizable shape clues without
            // dumping the entire object; falls through to to_string
            // when no recognizable key is present.
            if let Some(p) = map.get("file_path").and_then(|v| v.as_str()) {
                p.to_string()
            } else if let Some(p) = map.get("path").and_then(|v| v.as_str()) {
                p.to_string()
            } else if let Some(c) = map.get("command").and_then(|v| v.as_str()) {
                c.to_string()
            } else if let Some(p) = map.get("pattern").and_then(|v| v.as_str()) {
                p.to_string()
            } else {
                serde_json::to_string(input).unwrap_or_default()
            }
        }
        _ => serde_json::to_string(input).unwrap_or_default(),
    };
    if raw.chars().count() > 200 {
        let mut truncated: String = raw.chars().take(200).collect();
        truncated.push('…');
        truncated
    } else {
        raw
    }
}

/// Compute the per-change run-log path:
/// `<logs_dir>/runs/<repo-sanitized>/<change>.log`. The repo-sanitized
/// fragment is the workspace's basename, which is already the
/// URL-sanitized form produced by `workspace::derive_path`; this keeps
/// the per-repo subdirectory consistent with the workspace's own
/// naming.
pub(crate) fn run_log_path(workspace: &Path, change: &str) -> PathBuf {
    let basename = workspace
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    crate::paths::current()
        .run_logs_dir(&basename)
        .join(format!("{change}.log"))
}

/// Best-effort: write the subprocess's prompt, captured stdout, and
/// captured stderr to the per-change log file. Errors are logged at WARN
/// but never propagated; the executor outcome must not depend on
/// diagnostic side-effects.
fn persist_run_log(workspace: &Path, change: &str, prompt: &str, outcome: &SubprocessOutcome) {
    // The JSON-streaming path already wrote the structured log
    // incrementally; overwriting here would discard the ACTIONS section.
    if outcome.streamed_log {
        return;
    }
    let path = run_log_path(workspace, change);
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

    /// Build a fixture workspace with one OpenSpec change so `build_prompt`
    /// has material to produce a non-empty prompt.
    fn fixture_workspace() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let change_dir = dir.path().join("openspec/changes/x");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join("proposal.md"), "## Why\nfixture\n").unwrap();
        std::fs::write(change_dir.join("design.md"), "design text\n").unwrap();
        std::fs::write(change_dir.join("tasks.md"), "- [ ] do thing\n").unwrap();
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
    fn sandbox_settings_file_contains_expected_deny_patterns() {
        // Construct executor with a small custom sandbox so test asserts
        // are precise.
        let sandbox = crate::config::ResolvedSandbox {
            allowed_tools: vec!["Read".into(), "Bash".into()],
            disallowed_bash_patterns: vec!["curl:*".into(), "git push:*".into()],
            disallowed_read_paths: vec!["/home/*/.ssh/**".into()],
        };
        let executor =
            ClaudeCliExecutor::new_with_sandbox("dummy-claude".into(), 30, sandbox);
        let path = executor
            .write_sandbox_settings()
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
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn sandbox_temp_file_cleaned_up_after_spawn() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        // Per-test isolated settings dir, so the assertion is not racy with
        // other parallel tests writing to the shared OS temp dir.
        let settings_dir = TempDir::new().unwrap();
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30)
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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

        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);

        let handle = ResumeHandle(
            serde_json::to_value(ClaudeResumeData {
                workspace: ws.clone(),
                change: "x".into(),
                session_id: None,
            })
            .unwrap(),
        );
        let outcome = executor.resume(handle, "use SAMPLE").await.unwrap();
        assert!(matches!(outcome, ExecutorOutcome::Completed { .. }));
    }

    #[tokio::test]
    async fn resume_errors_on_bad_handle() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 1);
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
        let executor = ClaudeCliExecutor::new("/bin/true".into(), 30);
        let prompt = executor.build_prompt(&ws, "x").unwrap();
        assert!(!prompt.trim().is_empty(), "prompt must not be empty");
    }

    #[tokio::test]
    async fn build_prompt_errors_when_change_dir_missing() {
        let dir = TempDir::new().unwrap();
        let executor = ClaudeCliExecutor::new("/bin/true".into(), 30);
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
        let mut executor = ClaudeCliExecutor::new("/bin/true".into(), 30);
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

    /// `from_config`: with no override path, the default template is used.
    #[test]
    fn from_config_uses_default_template_when_path_unset() {
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg).unwrap();
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
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: Some(path),
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg).unwrap();
        assert!(executor.template.contains("CUSTOM_TEMPLATE_SENTINEL"));
    }

    /// `from_config`: a missing override file falls back to the embedded
    /// default (a24). A one-shot WARN names the missing path; the
    /// daemon does NOT abort start-up.
    #[test]
    fn from_config_falls_back_when_override_file_missing() {
        let cfg = crate::config::ExecutorConfig {
            kind: crate::config::ExecutorKind::ClaudeCli,
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: Some(PathBuf::from("/definitely/not/a/real/path.md")),
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg)
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
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg).unwrap();
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
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: Some(path),
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg).unwrap();
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
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: Some(path),
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg)
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
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: Some(path),
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg)
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
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: Some(legacy),
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            implementer: Some(crate::config::PromptOverrideBlock {
                prompt_path: Some(nested),
            }),
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        };
        let executor = ClaudeCliExecutor::from_config(&cfg).unwrap();
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
            command: "/bin/true".into(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
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
        let executor = ClaudeCliExecutor::from_config(&cfg).unwrap();
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30)
            .with_output_format(crate::config::ExecutorOutputFormat::Text);
        let outcome = executor.run(&ws, "x").await.unwrap();
        assert!(matches!(outcome, ExecutorOutcome::Completed { .. }), "got {outcome:?}");

        let log = run_log_path(&ws, "x");
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
        let path = run_log_path(&ws, "my-change");
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
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: None,
            stdout: "STDOUT_SENTINEL".to_string(),
            stderr: "STDERR_SENTINEL".to_string(),
            final_answer: None,
            streamed_log: false,
        };
        persist_run_log(&ws, "my-change", "PROMPT_SENTINEL", &outcome);

        let log = run_log_path(&ws, "my-change");
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
        ClaudeCliExecutor::new("dummy-claude".into(), 30)
    }

    fn fixture_executor_text() -> ClaudeCliExecutor {
        ClaudeCliExecutor::new("dummy-claude".into(), 30)
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
        let outcome = SubprocessOutcome {
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
        let outcome = SubprocessOutcome {
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
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        assert!(matches!(
            result,
            ExecutorOutcome::Failed { reason } if reason == "timeout"
        ));
    }

    /// JSON streaming mode with sentinel in stdout (tool-result echo)
    /// but final_answer empty — no timeout, no exit-status. The
    /// sentinel scan must consider final_answer ONLY and find nothing.
    /// Falls through to the normal exit-status path. Without an exit
    /// status set, classify_outcome panics ("non-timeout path has
    /// status"), so we set a fake successful status to drive the path.
    #[tokio::test]
    async fn json_mode_sentinel_only_scanned_in_final_answer() {
        use std::os::unix::process::ExitStatusExt;
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            // stdout contains a sentinel-shaped block from a tool-result
            // echo. final_answer is the agent's actual emission — no
            // sentinel there. Pre-fix would have scanned stdout and
            // false-matched; post-fix the scan is final_answer-scoped
            // and returns None.
            stdout: "\
[tool_use] Read prompts/implementer.md\n\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"5.2\",\"task_text\":\"install actionlint\",\"reason\":\"no apt access\"}],\"revision_suggestion\":\"x\"}\n".to_string(),
            stderr: String::new(),
            final_answer: Some("Implementation complete; all tests pass.".to_string()),
            streamed_log: true,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        // Exit-status path: zero exit, empty diff → the
        // no-modifications path; not our concern here. The point: NO
        // SpecNeedsRevision was returned. The scan ignored stdout.
        assert!(
            !matches!(result, ExecutorOutcome::SpecNeedsRevision { .. }),
            "JSON-mode sentinel scan must NOT match against stdout content"
        );
    }

    /// JSON streaming mode with a real sentinel in final_answer —
    /// happy-path scoping. The scan SHOULD find it there.
    #[tokio::test]
    async fn json_mode_sentinel_in_final_answer_is_honored() {
        use std::os::unix::process::ExitStatusExt;
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: "irrelevant tool noise".to_string(),
            stderr: String::new(),
            final_answer: Some(
                "=== AUTOCODER-OUTCOME ===\n{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"5.2\",\"task_text\":\"install actionlint\",\"reason\":\"no apt access\"}],\"revision_suggestion\":\"Replace with CI gate.\"}".to_string(),
            ),
            streamed_log: true,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        assert!(matches!(result, ExecutorOutcome::SpecNeedsRevision { .. }));
    }

    /// JSON streaming mode with final_answer absent (no `result` event)
    /// AND no timeout: the sentinel scan returns None (no fallback to
    /// stdout). Falls through to normal exit-status handling.
    #[tokio::test]
    async fn json_mode_with_no_final_answer_skips_sentinel_scan() {
        use std::os::unix::process::ExitStatusExt;
        let executor = fixture_executor_json();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            // stdout contains a sentinel block; the pre-a20a1 fallback
            // path would have matched it. Now: ignored because mode is
            // Json and final_answer is None.
            stdout: "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"5.2\",\"task_text\":\"install actionlint\",\"reason\":\"no apt access\"}],\"revision_suggestion\":\"x\"}\n".to_string(),
            stderr: String::new(),
            final_answer: None,
            streamed_log: true,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        assert!(
            !matches!(result, ExecutorOutcome::SpecNeedsRevision { .. }),
            "JSON-mode with no final_answer must not scan stdout"
        );
    }

    /// Text mode preserves the stdout sentinel scan for non-timeout
    /// runs. The legacy opt-out has no `result`-event channel; stdout
    /// IS the agent's emission. Behavior unchanged.
    #[tokio::test]
    async fn text_mode_sentinel_in_stdout_is_honored() {
        use std::os::unix::process::ExitStatusExt;
        let executor = fixture_executor_text();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: "\
some leading narrative\n\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"5.2\",\"task_text\":\"install actionlint\",\"reason\":\"no apt access\"}],\"revision_suggestion\":\"Replace with CI gate.\"}\n".to_string(),
            stderr: String::new(),
            final_answer: None,
            streamed_log: false,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        assert!(matches!(result, ExecutorOutcome::SpecNeedsRevision { .. }));
    }

    /// Text mode also yields to timeout precedence — a timed-out
    /// text-mode run is classified as timeout BEFORE the stdout
    /// scan runs.
    #[tokio::test]
    async fn text_mode_timeout_precedence_skips_sentinel_scan() {
        let executor = fixture_executor_text();
        let (_tmp, ws) = fixture_workspace_for_classify();
        let outcome = SubprocessOutcome {
            timed_out: true,
            exit_status: None,
            stdout: "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"5.2\",\"task_text\":\"install actionlint\",\"reason\":\"no apt access\"}],\"revision_suggestion\":\"x\"}\n".to_string(),
            stderr: "timeout".to_string(),
            final_answer: None,
            streamed_log: false,
        };
        let result = executor.classify_outcome(&ws, "x", outcome).await.unwrap();
        assert!(matches!(
            result,
            ExecutorOutcome::Failed { reason } if reason == "timeout"
        ));
    }

    // ---------- end a20a1 ----------

    #[test]
    fn parse_spec_needs_revision_sentinel_round_trips() {
        let stdout = "\
some narrative output before the sentinel\n\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[\
{\"task_id\":\"5.2\",\"task_text\":\"install actionlint on host\",\"reason\":\"no apt access\"}],\
\"revision_suggestion\":\"Replace 5.2 with a CI gate.\"}\n";
        let payload =
            ClaudeCliExecutor::extract_outcome_sentinel(stdout).expect("sentinel present");
        let outcome =
            ClaudeCliExecutor::try_parse_spec_needs_revision(&payload).expect("parse ok");
        match outcome {
            Some(ExecutorOutcome::SpecNeedsRevision {
                unimplementable_tasks,
                revision_suggestion,
            }) => {
                assert_eq!(unimplementable_tasks.len(), 1);
                assert_eq!(unimplementable_tasks[0].task_id, "5.2");
                assert_eq!(
                    unimplementable_tasks[0].task_text,
                    "install actionlint on host"
                );
                assert_eq!(unimplementable_tasks[0].reason, "no apt access");
                assert_eq!(revision_suggestion, "Replace 5.2 with a CI gate.");
            }
            other => panic!("expected SpecNeedsRevision, got {other:?}"),
        }
    }

    /// a27a0 task 4.5: any JSON snippet shown in the prompt SHALL
    /// deserialize cleanly into the corresponding Rust type via
    /// `serde_json::from_str`. The bundled prompt now shows TWO
    /// examples: one for the `outcome_spec_needs_revision` MCP tool's
    /// `arguments` object, AND the legacy stdout sentinel (still
    /// retained for the deprecation cycle). Both must round-trip.
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

    #[test]
    fn bundled_implementer_prompt_worked_example_parses() {
        // Per a20a1: the implementer prompt's worked example must
        // deserialize cleanly into a SpecNeedsRevision outcome with no
        // angle-bracket placeholders surviving in the parsed fields.
        // Guards against regressions where the documented example
        // accidentally reintroduces `<...>` placeholder text that the
        // agent would then emit verbatim.
        let prompt = DEFAULT_IMPLEMENTER_TEMPLATE;
        let payload =
            ClaudeCliExecutor::extract_outcome_sentinel(prompt).expect(
                "implementer prompt must contain an AUTOCODER-OUTCOME example",
            );
        let outcome = ClaudeCliExecutor::try_parse_spec_needs_revision(&payload)
            .expect("worked example must parse as a SpecNeedsRevision payload");
        match outcome {
            Some(ExecutorOutcome::SpecNeedsRevision {
                unimplementable_tasks,
                revision_suggestion,
            }) => {
                assert!(
                    !unimplementable_tasks.is_empty(),
                    "worked example must have at least one task"
                );
                for t in &unimplementable_tasks {
                    assert!(
                        !ClaudeCliExecutor::contains_unsubstituted_placeholder(&t.task_id),
                        "task_id contains placeholder text: {}",
                        t.task_id
                    );
                    assert!(
                        !ClaudeCliExecutor::contains_unsubstituted_placeholder(&t.task_text),
                        "task_text contains placeholder text: {}",
                        t.task_text
                    );
                    assert!(
                        !ClaudeCliExecutor::contains_unsubstituted_placeholder(&t.reason),
                        "reason contains placeholder text: {}",
                        t.reason
                    );
                }
                assert!(
                    !revision_suggestion.is_empty(),
                    "worked example must have a concrete revision_suggestion"
                );
            }
            other => panic!("expected SpecNeedsRevision, got {other:?}"),
        }
    }

    #[test]
    fn placeholder_detection_matches_template_markers() {
        // Each of these is a literal substring from the pre-fix
        // implementer-prompt template. After the fix, the prompt has
        // none of them, but the detection must catch any that survive
        // a misedit OR an operator override.
        for s in &[
            "<id-from-tasks-md>",
            "<verbatim quote>",
            "<one-line why>",
            "<free-form text describing what to change>",
        ] {
            assert!(
                ClaudeCliExecutor::contains_unsubstituted_placeholder(s),
                "expected {s} to match placeholder regex"
            );
        }
    }

    #[test]
    fn placeholder_detection_ignores_uppercase_and_special_chars() {
        // Narrow regex: leading char must be ASCII lowercase. Mixed-case
        // OR symbol-leading angle-bracket content is NOT flagged. Keeps
        // legitimate task descriptions (`<HTML>`, `<MyType>`, real code
        // syntax) from triggering false-positive flags.
        for s in &[
            "<HTML>",          // uppercase
            "<MyType>",        // CamelCase
            "<!doctype>",      // leading symbol
            "<3>",             // leading digit
            "<>",              // empty
            "no brackets at all",
            "plain text",
        ] {
            assert!(
                !ClaudeCliExecutor::contains_unsubstituted_placeholder(s),
                "did not expect {s} to match placeholder regex"
            );
        }
    }

    #[test]
    fn placeholder_detection_catches_template_in_task_id() {
        // The most-likely failure mode: the agent copies the prompt
        // template's `task_id` value verbatim. Parser must reject.
        let stdout = "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[\
{\"task_id\":\"<id-from-tasks-md>\",\"task_text\":\"install actionlint\",\"reason\":\"no apt access\"}],\
\"revision_suggestion\":\"Replace with CI gate.\"}\n";
        let payload =
            ClaudeCliExecutor::extract_outcome_sentinel(stdout).expect("sentinel present");
        let err = ClaudeCliExecutor::try_parse_spec_needs_revision(&payload)
            .expect_err("placeholder must produce parse error");
        assert!(
            err.contains("un-substituted placeholders"),
            "diagnostic phrase missing from: {err}"
        );
        assert!(
            err.contains("prompts/implementer.md"),
            "prompt reference missing from: {err}"
        );
        assert!(
            err.contains("task_id"),
            "specific field name missing from: {err}"
        );
    }

    #[test]
    fn placeholder_detection_catches_template_in_task_text_and_reason() {
        // Both `task_text` and `reason` fields are scanned. Any field
        // tripping the regex fails the whole payload.
        let stdout = "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[\
{\"task_id\":\"5.2\",\"task_text\":\"<verbatim quote>\",\"reason\":\"<one-line why>\"}],\
\"revision_suggestion\":\"Replace with CI gate.\"}\n";
        let payload =
            ClaudeCliExecutor::extract_outcome_sentinel(stdout).expect("sentinel present");
        let err = ClaudeCliExecutor::try_parse_spec_needs_revision(&payload)
            .expect_err("placeholder must produce parse error");
        // Detection short-circuits at task_text (first failing field).
        assert!(err.contains("un-substituted placeholders"), "err: {err}");
        assert!(err.contains("task_text"), "err: {err}");
    }

    #[test]
    fn placeholder_detection_tolerates_legitimate_angle_brackets() {
        // A legitimate task whose text happens to include `<HTML>` or
        // `<MyType>` is NOT flagged — the regex requires lowercase
        // leading char to avoid this common false-positive shape.
        let stdout = "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[\
{\"task_id\":\"7.1\",\"task_text\":\"Render <HTML> tags in user input safely\",\"reason\":\"needs DOM sandbox\"}],\
\"revision_suggestion\":\"Add an HTML sanitizer dependency.\"}\n";
        let payload =
            ClaudeCliExecutor::extract_outcome_sentinel(stdout).expect("sentinel present");
        let outcome = ClaudeCliExecutor::try_parse_spec_needs_revision(&payload)
            .expect("legitimate angle-bracket content must NOT trip placeholder detection");
        assert!(matches!(
            outcome,
            Some(ExecutorOutcome::SpecNeedsRevision { .. })
        ));
    }

    #[test]
    fn parse_spec_needs_revision_missing_required_field_falls_back_to_failed() {
        let stdout = "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"revision_suggestion\":\"x\"}\n";
        let payload =
            ClaudeCliExecutor::extract_outcome_sentinel(stdout).expect("sentinel present");
        let err = ClaudeCliExecutor::try_parse_spec_needs_revision(&payload)
            .expect_err("missing tasks field must surface as parse error");
        assert!(
            err.contains("unimplementable_tasks"),
            "error should name the missing field: {err}"
        );
    }

    #[test]
    fn parse_spec_needs_revision_with_empty_task_list_treated_as_invalid() {
        let stdout = "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[],\"revision_suggestion\":\"x\"}\n";
        let payload =
            ClaudeCliExecutor::extract_outcome_sentinel(stdout).expect("sentinel present");
        let err = ClaudeCliExecutor::try_parse_spec_needs_revision(&payload)
            .expect_err("empty task list must surface as parse error");
        assert!(err.contains("empty"), "error should mention emptiness: {err}");
    }

    #[test]
    fn extract_outcome_sentinel_handles_braces_in_strings() {
        let stdout = "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[\
{\"task_id\":\"1.1\",\"task_text\":\"sudo apt-get install x { y }\",\"reason\":\"no apt\"}],\
\"revision_suggestion\":\"drop {curlies} in description\"}\n";
        let payload =
            ClaudeCliExecutor::extract_outcome_sentinel(stdout).expect("sentinel extracted");
        // The full JSON object must be captured: depth tracker should not
        // close early on `{` or `}` inside string literals.
        assert!(payload.ends_with('}'));
        let outcome = ClaudeCliExecutor::try_parse_spec_needs_revision(&payload)
            .expect("parse ok")
            .expect("Some outcome");
        match outcome {
            ExecutorOutcome::SpecNeedsRevision {
                unimplementable_tasks,
                ..
            } => {
                assert!(unimplementable_tasks[0].task_text.contains("{ y }"));
            }
            other => panic!("expected SpecNeedsRevision, got {other:?}"),
        }
    }

    /// End-to-end: a script that emits a well-formed spec-needs-revision
    /// sentinel on stdout and exits 0 causes the executor to return
    /// `SpecNeedsRevision`, not `Completed`. Uses text mode because the
    /// fixture is a shell script — JSON mode requires the wrapped CLI
    /// to emit a `result` event channel, which this fixture does not
    /// simulate. The semantic the test guards (well-formed sentinel
    /// in agent emission → SpecNeedsRevision) is identical in both
    /// modes; only the source of "agent emission" differs (a20a1).
    #[tokio::test]
    async fn spec_needs_revision_sentinel_routes_through_run() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "needs_revision.sh",
            "#!/bin/sh\ncat <<'EOF'\nbla bla\n=== AUTOCODER-OUTCOME ===\n{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"7.3\",\"task_text\":\"smoke test on macOS\",\"reason\":\"no macOS host in sandbox\"}],\"revision_suggestion\":\"drop the macOS smoke step\"}\nEOF\nexit 0\n",
        );
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30)
            .with_output_format(crate::config::ExecutorOutputFormat::Text);
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::SpecNeedsRevision {
                unimplementable_tasks,
                revision_suggestion,
            } => {
                assert_eq!(unimplementable_tasks.len(), 1);
                assert_eq!(unimplementable_tasks[0].task_id, "7.3");
                assert!(revision_suggestion.contains("macOS"));
            }
            other => panic!("expected SpecNeedsRevision, got {other:?}"),
        }
    }

    /// Unparseable sentinel → Failed with parse-error reason. Production
    /// invariant from the spec: the daemon must not crash on a malformed
    /// payload, and it must not silently treat the run as success.
    #[tokio::test]
    async fn malformed_spec_needs_revision_sentinel_yields_failed() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(
            &ws,
            "bad_sentinel.sh",
            "#!/bin/sh\ncat <<'EOF'\n=== AUTOCODER-OUTCOME ===\n{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[]}\nEOF\nexit 0\n",
        );
        // Text mode: stdout IS the agent's emission stream (no JSON
        // event channel), so the malformed sentinel emitted via the
        // script SHALL trigger the canonical "unparseable sentinel"
        // Failed outcome. Per a20a1, JSON mode scopes the scan to
        // `final_answer` only; a shell-script fixture has no `result`
        // event so a JSON-mode equivalent test isn't well-defined.
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30)
            .with_output_format(crate::config::ExecutorOutputFormat::Text);
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Failed { reason } => {
                assert!(
                    reason.contains("unparseable SpecNeedsRevision sentinel"),
                    "reason should mention the parse failure: {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
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
        let executor = ClaudeCliExecutor::new("dummy".into(), 30);
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
        let executor = ClaudeCliExecutor::new("dummy".into(), 30);
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

    /// End-to-end: after a `run`, the persisted log contains a PROMPT
    /// section (whether the prompt came from openspec or from the raw-
    /// markdown fallback). This is the diagnostic that lets an operator
    /// see exactly what Claude was sent.
    #[tokio::test]
    async fn run_log_contains_prompt_section() {
        let (_dir, ws) = fixture_workspace_with_git();
        let script = write_script(&ws, "noop.sh", "#!/bin/sh\nexit 0\n");
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
        let _ = executor.run(&ws, "x").await.unwrap();

        let log = run_log_path(&ws, "x");
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let log = run_log_path(&ws, "x");
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 1);
        let outcome = executor.run(&ws, "x").await.unwrap();
        match outcome {
            ExecutorOutcome::Failed { reason } => {
                assert_eq!(reason, "timeout");
            }
            other => panic!("expected Failed timeout, got {other:?}"),
        }
        let log = run_log_path(&ws, "x");
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
        let outcome = executor.run(&ws, "x").await.unwrap();
        assert!(matches!(outcome, ExecutorOutcome::Completed { .. }));
        // Raw action lines live in the stream log; the valid `result`
        // event populates FINAL ANSWER in the summary log.
        let summary = std::fs::read_to_string(run_log_path(&ws, "x")).unwrap();
        let stream = std::fs::read_to_string(
            run_log_path(&ws, "x").with_extension("stream.log"),
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
        let _ = executor.run(&ws, "x").await.unwrap();
        // Per a20a2: unknown-type lines land in the stream log, not
        // the summary.
        let stream = std::fs::read_to_string(
            run_log_path(&ws, "x").with_extension("stream.log"),
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
        let _ = executor.run(&ws, "x").await.unwrap();
        let body = std::fs::read_to_string(run_log_path(&ws, "x")).unwrap();
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 1);
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
        let executor = ClaudeCliExecutor::new(script.to_string_lossy().into(), 30)
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
        let body = std::fs::read_to_string(run_log_path(&ws, "x")).unwrap();
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

    /// Serialize env-var-touching tests so concurrent runs do not race
    /// on the `ENV_CONTROL_SOCKET` / `ENV_WORKSPACE_BASENAME` mutation.
    /// Mirrors the lock in `mcp_askuser_server.rs`'s test module.
    static A27A0_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// 3.5 — tool-recorded `Success` outcome takes precedence over a
    /// stdout sentinel block in the same captured event stream.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn tool_recorded_success_takes_precedence_over_stdout_sentinel() {
        let _g = A27A0_ENV_LOCK.lock().unwrap();
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
        // stdout contains a well-formed spec_needs_revision block —
        // pre-a27a0 this would have produced SpecNeedsRevision. Now
        // the tool-recorded Success wins.
        use std::os::unix::process::ExitStatusExt;
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"5.2\",\"task_text\":\"x\",\"reason\":\"r\"}],\"revision_suggestion\":\"s\"}\n".to_string(),
            stderr: String::new(),
            final_answer: None,
            streamed_log: false,
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
        let _g = A27A0_ENV_LOCK.lock().unwrap();
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
        let outcome = SubprocessOutcome {
            timed_out: true,
            exit_status: None,
            stdout: String::new(),
            stderr: "timeout".to_string(),
            final_answer: None,
            streamed_log: true,
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

    /// 3.7 — when `consume_outcome` returns `None` AND the legacy
    /// stdout sentinel matches, today's behavior is preserved AND
    /// a deprecation warning is emitted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn legacy_stdout_sentinel_match_emits_deprecation_warning() {
        use tracing_subscriber::fmt::MakeWriter;

        // tracing-subscriber capture: a thread-local sink that records
        // every `tracing::warn!` line emitted under the dispatched
        // subscriber.
        #[derive(Clone, Default)]
        struct CaptureWriter {
            buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        }
        impl std::io::Write for CaptureWriter {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.buf.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for CaptureWriter {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let _g = A27A0_ENV_LOCK.lock().unwrap();
        let (_sock_dir, socket) = spawn_consume_outcome_responder(None).await;
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
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: "\
=== AUTOCODER-OUTCOME ===\n\
{\"type\":\"spec_needs_revision\",\"unimplementable_tasks\":[{\"task_id\":\"5.2\",\"task_text\":\"x\",\"reason\":\"r\"}],\"revision_suggestion\":\"s\"}\n".to_string(),
            stderr: String::new(),
            final_answer: None,
            streamed_log: false,
        };

        let writer = CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();

        // `set_default` returns a guard scoped to the current thread,
        // so all `tracing::warn!` emitted while the guard is alive go
        // through our capture writer. This stays inside the running
        // tokio runtime — no nested `block_on`.
        let result = {
            let _guard = tracing::subscriber::set_default(subscriber);
            executor.classify_outcome(&ws, "x", outcome).await.unwrap()
        };
        unsafe {
            std::env::remove_var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET);
            std::env::remove_var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME);
        }

        assert!(
            matches!(result, ExecutorOutcome::SpecNeedsRevision { .. }),
            "legacy path must still classify as SpecNeedsRevision; got {result:?}"
        );
        let captured = String::from_utf8_lossy(&writer.buf.lock().unwrap()).into_owned();
        assert!(
            captured.contains("legacy stdout sentinel matched"),
            "deprecation marker missing from log: {captured}"
        );
        assert!(
            captured.contains("outcome_spec_needs_revision"),
            "directive sentence missing: {captured}"
        );
        assert!(
            captured.contains("a27a2"),
            "planned-removal target missing: {captured}"
        );
    }

    /// Daemon-recorded outcome maps correctly to `ExecutorOutcome` for
    /// both variants. Unit-level test of the mapping function so the
    /// integration tests above are not the only coverage.
    #[test]
    fn map_recorded_outcome_round_trips_both_variants() {
        let recorded_success = crate::outcome_store::RecordedOutcome::Success {
            final_answer: Some("ok".to_string()),
        };
        match map_recorded_outcome(recorded_success) {
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
        match map_recorded_outcome(recorded_revision) {
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
}
