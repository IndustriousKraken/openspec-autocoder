//! Documentation audit. Invokes the wrapped agent CLI (typically
//! `claude`) with a read-only sandbox (`Read`, `Glob`, `Grep`, `Bash`)
//! and a three-check documentation prompt (coverage, stale references,
//! organization). Parses the agent's structured JSON output into
//! [`Finding`]s tagged with category in `subject` and returns
//! `AuditOutcome::Reported`.
//!
//! `requires_head_change = true` — documentation drift only emerges
//! with code or docs changes; rerunning without a HEAD shift wastes
//! CLI invocations.
//! `WritePolicy::None` — strictly advisory; operators react via
//! `@<bot> send it` to produce a docs-fix PR.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::{
    Audit, AuditContext, AuditLogWriter, AuditOutcome, Finding, Severity, WritePolicy,
    workspace_is_valid, workspace_unavailable_outcome, write_sandbox_settings,
};
use crate::config::{AuditSettings, ExecutorConfig, ResolvedSandbox};
use crate::prompts::{PromptId, PromptLoader};

/// Tools the documentation agent may call. Excludes `Write` and `Edit`
/// so the sandbox blocks workspace modifications outright; the audit-
/// run log captures the agent's stdout for forensic review.
const ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Bash"];

/// Embedded default prompt. The [`crate::prompts::PromptLoader`] also
/// holds an identical reference; this local alias remains so the
/// existing anti-prompt-drift tests can compare against the bytes.
#[cfg(test)]
const DEFAULT_PROMPT: &str =
    include_str!("../../../prompts/documentation-audit.md");

/// Default threshold for the "README too long" organization finding.
/// Overridable via `audits.settings.documentation_audit.extra.readme_max_lines`.
pub const DEFAULT_README_MAX_LINES: usize = 200;

/// Default threshold for the "page too long without TOC" organization
/// finding. Overridable via
/// `audits.settings.documentation_audit.extra.page_max_lines_without_toc`.
pub const DEFAULT_PAGE_MAX_LINES_WITHOUT_TOC: usize = 500;

/// Maximum number of characters of stdout to embed in a parse-failure
/// error message. The full stdout always lands in the audit-run log.
const STDOUT_EXCERPT_CHARS: usize = 400;

/// Per-file byte cap on bundled content. Beyond this the file is
/// truncated with a `[truncated]` marker so a single oversized docs
/// page cannot blow the prompt window.
const PER_FILE_BYTE_CAP: usize = 50_000;

/// Total byte cap on the gathered input bundle. The audit emits an
/// `[overflow]` marker when reached and stops gathering further files.
const TOTAL_BUNDLE_BYTE_CAP: usize = 500_000;

/// Subject tag emitted on `coverage` findings. The chatops formatter
/// reads this to group findings by category in the thread body.
pub const COVERAGE_SUBJECT: &str = "coverage";

/// Subject tag emitted on `stale_reference` findings.
pub const STALE_REFERENCE_SUBJECT: &str = "stale_reference";

/// Subject tag emitted on `organization` findings.
pub const ORGANIZATION_SUBJECT: &str = "organization";

pub struct DocumentationAudit {
    settings: AuditSettings,
    executor_command: String,
    executor_timeout_secs: u64,
    sandbox: ResolvedSandbox,
    /// Override for the directory the per-invocation sandbox settings
    /// file is written to. `None` (production) means
    /// `std::env::temp_dir()`. Tests pass a per-test TempDir.
    settings_dir: Option<PathBuf>,
}

impl DocumentationAudit {
    pub const TYPE: &'static str = "documentation_audit";

    pub fn new(
        audit_settings: &std::collections::HashMap<String, AuditSettings>,
        executor: &ExecutorConfig,
    ) -> Self {
        let settings = audit_settings
            .get(Self::TYPE)
            .cloned()
            .unwrap_or_default();
        let sandbox = ResolvedSandbox::resolve(executor.sandbox.as_ref());
        Self {
            settings,
            executor_command: executor.command.clone(),
            executor_timeout_secs: executor.timeout_secs,
            sandbox,
            settings_dir: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_settings_dir(mut self, dir: PathBuf) -> Self {
        self.settings_dir = Some(dir);
        self
    }

    /// Resolve `extra.readme_max_lines` from settings, falling back to
    /// the default if unset OR the value is not a positive integer.
    pub fn readme_max_lines(&self) -> usize {
        extra_usize(&self.settings, "readme_max_lines")
            .unwrap_or(DEFAULT_README_MAX_LINES)
    }

    /// Resolve `extra.page_max_lines_without_toc` from settings,
    /// falling back to the default if unset OR not a positive integer.
    pub fn page_max_lines_without_toc(&self) -> usize {
        extra_usize(&self.settings, "page_max_lines_without_toc")
            .unwrap_or(DEFAULT_PAGE_MAX_LINES_WITHOUT_TOC)
    }

    /// Resolve the documentation prompt via the uniform [`PromptLoader`].
    /// `settings.prompt_path` is the audit's nested override
    /// (`audits.settings.documentation_audit.prompt_path`); missing or
    /// empty values fall through to the embedded default.
    fn resolve_prompt(&self, workspace: Option<&Path>) -> Result<String> {
        Ok(PromptLoader::load(
            PromptId::AuditDocumentation,
            self.settings.prompt_path.as_deref(),
            None,
            workspace,
        ))
    }

    /// Build the full prompt: embedded template + an `extra` YAML block
    /// naming the operator's configured thresholds + a concatenation of
    /// every gathered input file headed by `## File: <path>`.
    fn build_prompt(&self, workspace: &std::path::Path) -> Result<String> {
        let template = self.resolve_prompt(Some(workspace))?;
        let mut out = template;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n## Inputs (bundled by the driver)\n\n");
        out.push_str("```yaml\ndocumentation_audit_extra:\n");
        out.push_str(&format!("  readme_max_lines: {}\n", self.readme_max_lines()));
        out.push_str(&format!(
            "  page_max_lines_without_toc: {}\n",
            self.page_max_lines_without_toc()
        ));
        out.push_str("```\n\n");

        let inputs = gather_inputs(workspace);
        let mut total = 0usize;
        for input in inputs {
            let header = format!("## File: {}\n\n", input.display_path);
            if total + header.len() + input.body.len() > TOTAL_BUNDLE_BYTE_CAP {
                out.push_str(&header);
                out.push_str("[overflow: remaining input files omitted to keep the prompt within budget]\n\n");
                break;
            }
            out.push_str(&header);
            out.push_str(&input.body);
            if !input.body.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
            total += header.len() + input.body.len() + 1;
        }
        Ok(out)
    }
}

#[async_trait]
impl Audit for DocumentationAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn description(&self) -> &'static str {
        "documentation coverage / stale-reference / organization audit (LLM-driven)"
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::None
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        if !workspace_is_valid(ctx.workspace) {
            return Ok(workspace_unavailable_outcome(
                Self::TYPE,
                ctx.workspace,
                &ctx.repo.url,
            ));
        }

        let prompt = self.build_prompt(ctx.workspace)?;

        let mut sandbox = self.sandbox.clone();
        sandbox.allowed_tools = ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect();

        let (settings_path, _settings_guard) =
            write_sandbox_settings(&sandbox, self.settings_dir.as_deref())
                .context("generating documentation-audit sandbox settings file")?;

        let _ = ctx.log_writer.write_section(
            "documentation_audit_preamble",
            &format!(
                "executor_command: {}\ntimeout_secs: {}\nprompt_source: {}\nsettings_file: {}\nallowed_tools: {}\nreadme_max_lines: {}\npage_max_lines_without_toc: {}",
                self.executor_command,
                self.executor_timeout_secs,
                self.settings
                    .prompt_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<embedded default>".to_string()),
                settings_path.display(),
                sandbox.allowed_tools.join(","),
                self.readme_max_lines(),
                self.page_max_lines_without_toc(),
            ),
        );
        let _ = ctx
            .log_writer
            .write_section("documentation_audit_prompt", &prompt);

        let outcome = run_subprocess(
            &self.executor_command,
            &settings_path,
            &sandbox.allowed_tools,
            ctx.workspace,
            &prompt,
            Duration::from_secs(self.executor_timeout_secs),
        )
        .await
        .context("spawning documentation-audit CLI subprocess")?;

        let _ = ctx.log_writer.write_section(
            "documentation_audit_stdout",
            if outcome.stdout.is_empty() {
                "(empty)"
            } else {
                outcome.stdout.as_str()
            },
        );
        let _ = ctx.log_writer.write_section(
            "documentation_audit_stderr",
            if outcome.stderr.is_empty() {
                "(empty)"
            } else {
                outcome.stderr.as_str()
            },
        );

        if let Some(err) = outcome_to_terminal_err(
            &outcome,
            &mut ctx.log_writer,
            "documentation_audit",
            self.executor_timeout_secs,
        ) {
            return Err(err);
        }

        let findings = match parse_findings(&outcome.stdout) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    url = %ctx.repo.url,
                    excerpt = %excerpt(&outcome.stdout),
                    "documentation_audit: response parse failure: {e:#}"
                );
                let _ = ctx.log_writer.write_section(
                    "documentation_audit_outcome",
                    &format!("kind: Err\nreason: {e:#}"),
                );
                return Err(e);
            }
        };
        let _ = ctx.log_writer.write_section(
            "documentation_audit_outcome",
            &format!("kind: Reported\nfindings_count: {}", findings.len()),
        );
        Ok(AuditOutcome::reported(findings))
    }
}

/// Pull a `usize` value out of `settings.extra.<key>`. Accepts YAML
/// integers; returns `None` for any other shape (including
/// negative integers and floats) so the caller falls back to the
/// documented default rather than crashing on a bad knob.
fn extra_usize(settings: &AuditSettings, key: &str) -> Option<usize> {
    let v = settings.extra.get(key)?;
    let i = v.as_i64()?;
    if i < 0 {
        return None;
    }
    usize::try_from(i).ok()
}

/// One bundled input file with the path the driver names in its
/// `## File: <path>` header (workspace-relative) and the file body.
#[derive(Debug)]
struct GatheredInput {
    display_path: String,
    body: String,
}

/// Read every canonical spec under `openspec/specs/<cap>/spec.md`,
/// `<workspace>/README.md`, AND every `<workspace>/docs/*.md` file,
/// truncating any single file to `PER_FILE_BYTE_CAP` bytes. The
/// gather is best-effort: a missing or unreadable file is silently
/// skipped (the audit can still report on what it has). The returned
/// list is in a stable order: specs first (sorted by capability
/// slug), then README, then docs (sorted by filename).
fn gather_inputs(workspace: &std::path::Path) -> Vec<GatheredInput> {
    let mut out: Vec<GatheredInput> = Vec::new();

    let specs_root = workspace.join("openspec/specs");
    if let Ok(entries) = std::fs::read_dir(&specs_root) {
        let mut cap_paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path().join("spec.md"))
            .filter(|p| p.is_file())
            .collect();
        cap_paths.sort();
        for path in cap_paths {
            push_input(&mut out, workspace, &path);
        }
    }

    let readme = workspace.join("README.md");
    if readme.is_file() {
        push_input(&mut out, workspace, &readme);
    }

    let docs_root = workspace.join("docs");
    if let Ok(entries) = std::fs::read_dir(&docs_root) {
        let mut docs_paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("md"))
                        .unwrap_or(false)
            })
            .collect();
        docs_paths.sort();
        for path in docs_paths {
            push_input(&mut out, workspace, &path);
        }
    }

    out
}

fn push_input(out: &mut Vec<GatheredInput>, workspace: &std::path::Path, path: &std::path::Path) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let body = if raw.len() > PER_FILE_BYTE_CAP {
        let mut truncated: String = raw.chars().take(PER_FILE_BYTE_CAP).collect();
        truncated.push_str("\n[truncated: file exceeds per-file cap]\n");
        truncated
    } else {
        raw
    };
    let display_path = path
        .strip_prefix(workspace)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    out.push(GatheredInput { display_path, body });
}

/// Parse `stdout` as `{ "findings": [...] }`. Each finding's
/// `category` becomes the [`Finding`] `subject` so the chatops
/// formatter can group findings by category in the thread body.
/// Severity `high` is demoted to `medium` with a WARN log per the
/// spec's anti-emergency-promotion rule.
pub(crate) fn parse_findings(stdout: &str) -> Result<Vec<Finding>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "documentation_audit: agent produced empty stdout (expected `{{ \"findings\": [...] }}`)"
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
        anyhow!(
            "documentation_audit: stdout is not valid JSON: {e}; excerpt: {}",
            excerpt(stdout)
        )
    })?;
    let arr = parsed
        .get("findings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            anyhow!(
                "documentation_audit: stdout JSON missing top-level `findings` array; excerpt: {}",
                excerpt(stdout)
            )
        })?;
    let mut findings = Vec::with_capacity(arr.len());
    for (idx, raw) in arr.iter().enumerate() {
        let entry: RawFinding = serde_json::from_value(raw.clone()).map_err(|e| {
            anyhow!(
                "documentation_audit: findings[{idx}] does not match the expected shape: {e}; excerpt: {}",
                excerpt(stdout)
            )
        })?;
        let category_subject = normalize_category(&entry.category).ok_or_else(|| {
            anyhow!(
                "documentation_audit: findings[{idx}] has unknown category `{}`; expected one of coverage, stale_reference, organization",
                entry.category
            )
        })?;
        let severity = parse_severity(&entry.severity);
        findings.push(Finding {
            severity,
            subject: category_subject.to_string(),
            body: entry.body,
            anchor: Some(entry.anchor),
        });
    }
    Ok(findings)
}

#[derive(Debug, Deserialize)]
struct RawFinding {
    category: String,
    severity: String,
    anchor: String,
    body: String,
}

fn normalize_category(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "coverage" => Some(COVERAGE_SUBJECT),
        "stale_reference" | "stale-reference" | "stale" => Some(STALE_REFERENCE_SUBJECT),
        "organization" | "organisation" => Some(ORGANIZATION_SUBJECT),
        _ => None,
    }
}

/// Documentation findings are `low` or `medium` only. The prompt
/// explicitly forbids `high`; the parser demotes any `high` it sees
/// to `medium` with a WARN log so an operator notices the LLM's
/// disregard for the constraint.
fn parse_severity(raw: &str) -> Severity {
    match raw.trim().to_ascii_lowercase().as_str() {
        "medium" => Severity::Medium,
        "low" => Severity::Low,
        "high" => {
            // no-url: pure severity parser, no AuditContext in scope
            tracing::warn!(
                "documentation_audit: LLM emitted `high` severity; demoting to `medium` per the audit's no-high rule"
            );
            Severity::Medium
        }
        other => {
            // no-url: pure severity parser, no AuditContext in scope
            tracing::warn!(
                severity = other,
                "documentation_audit: unexpected severity `{other}`; defaulting to Low"
            );
            Severity::Low
        }
    }
}

fn excerpt(s: &str) -> String {
    let mut out: String = s.chars().take(STDOUT_EXCERPT_CHARS).collect();
    if s.chars().count() > STDOUT_EXCERPT_CHARS {
        out.push('…');
    }
    out
}

struct SubprocessOutcome {
    timed_out: bool,
    exit_status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
}

fn outcome_to_terminal_err(
    outcome: &SubprocessOutcome,
    log_writer: &mut AuditLogWriter,
    audit_type: &str,
    timeout_secs: u64,
) -> Option<anyhow::Error> {
    if outcome.timed_out {
        let _ = log_writer.write_section(
            &format!("{audit_type}_outcome"),
            "kind: Err\nreason: timeout",
        );
        return Some(anyhow!(
            "{audit_type}: CLI exceeded the {timeout_secs}s timeout"
        ));
    }
    if let Some(status) = outcome.exit_status
        && !status.success()
    {
        let _ = log_writer.write_section(
            &format!("{audit_type}_outcome"),
            &format!("kind: Err\nreason: exit {status}"),
        );
        return Some(anyhow!(
            "{audit_type}: CLI exited {status}; stderr excerpt: {}",
            excerpt(&outcome.stderr)
        ));
    }
    None
}

async fn run_subprocess(
    command: &str,
    settings_path: &std::path::Path,
    allowed_tools: &[String],
    workspace: &std::path::Path,
    prompt: &str,
    timeout: Duration,
) -> Result<SubprocessOutcome> {
    let mut child = super::spawn_with_etxtbsy_retry(|| {
        let mut cmd = Command::new(command);
        cmd.arg("--settings")
            .arg(settings_path)
            .arg("--allowedTools")
            .arg(allowed_tools.join(","))
            .arg("--permission-mode")
            .arg("acceptEdits")
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        cmd
    })
    .await
    .with_context(|| format!("spawning documentation-audit command `{command}`"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()).await;
    }
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let sleeper = tokio::time::sleep(timeout);
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
            })
        }
        Some(Err(e)) => Err(e).context("waiting on documentation-audit child process"),
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
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audits::AuditLogWriter;
    use crate::config::{ExecutorKind, RepositoryConfig};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn executor_cfg(command: &str) -> ExecutorConfig {
        ExecutorConfig {
            kind: ExecutorKind::ClaudeCli,
            command: command.to_string(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
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
        }
    }

    fn fixture_repo() -> RepositoryConfig {
        RepositoryConfig {
            url: "git@github.com:test/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
        }
    }

    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn make_log_writer(workspace: &std::path::Path) -> AuditLogWriter {
        let (td, paths) = crate::testing::test_daemon_paths();
        std::mem::forget(td);
        AuditLogWriter::open(&paths, workspace, DocumentationAudit::TYPE).expect("log writer opens")
    }

    #[test]
    fn audit_type_and_policy_are_fixed() {
        let cfg = executor_cfg("/bin/true");
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg);
        assert_eq!(audit.audit_type(), "documentation_audit");
        assert!(audit.requires_head_change());
        assert!(matches!(audit.write_policy(), WritePolicy::None));
    }

    #[test]
    fn parses_empty_findings_array() {
        let findings = parse_findings(r#"{"findings": []}"#).expect("parses empty");
        assert!(findings.is_empty());
    }

    #[test]
    fn parses_three_categories() {
        let stdout = r#"{
            "findings": [
                {
                    "category": "coverage",
                    "severity": "medium",
                    "anchor": "docs/CHATOPS.md",
                    "body": "Verb `propose` documented in spec but not in CHATOPS.md."
                },
                {
                    "category": "stale_reference",
                    "severity": "medium",
                    "anchor": "docs/CONFIG.md:184",
                    "body": "`executor.foo_bar_quux` referenced in docs but not present in source."
                },
                {
                    "category": "organization",
                    "severity": "low",
                    "anchor": "README.md",
                    "body": "README.md is 320 lines without a top-of-file TOC."
                }
            ]
        }"#;
        let findings = parse_findings(stdout).expect("parses three categories");
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[0].subject, COVERAGE_SUBJECT);
        assert_eq!(findings[0].severity, Severity::Medium);
        assert_eq!(findings[0].anchor.as_deref(), Some("docs/CHATOPS.md"));
        assert_eq!(findings[1].subject, STALE_REFERENCE_SUBJECT);
        assert_eq!(findings[1].anchor.as_deref(), Some("docs/CONFIG.md:184"));
        assert_eq!(findings[2].subject, ORGANIZATION_SUBJECT);
        assert_eq!(findings[2].severity, Severity::Low);
    }

    #[test]
    fn high_severity_demotes_to_medium() {
        let stdout = r#"{
            "findings": [
                {"category":"coverage","severity":"high","anchor":"docs/X.md","body":"b"}
            ]
        }"#;
        let findings = parse_findings(stdout).expect("parses");
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].severity,
            Severity::Medium,
            "high severities must demote to medium"
        );
    }

    #[test]
    fn unknown_category_returns_err() {
        let stdout = r#"{
            "findings": [
                {"category":"made-up-bucket","severity":"low","anchor":"a","body":"b"}
            ]
        }"#;
        let err = parse_findings(stdout).expect_err("unknown category must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown category"), "got: {msg}");
        assert!(msg.contains("made-up-bucket"), "got: {msg}");
    }

    #[test]
    fn malformed_json_returns_err_with_excerpt() {
        let stdout = "this is not JSON, just some prose";
        let err = parse_findings(stdout).expect_err("non-JSON must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("not valid JSON"), "got: {msg}");
        assert!(msg.contains("just some prose"), "excerpt missing: {msg}");
    }

    #[test]
    fn missing_top_level_findings_key_returns_err() {
        let err = parse_findings(r#"{"results": []}"#).expect_err("missing key must error");
        assert!(format!("{err:#}").contains("findings"));
    }

    #[test]
    fn empty_stdout_returns_err() {
        let err = parse_findings("   \n").expect_err("empty must error");
        assert!(format!("{err:#}").contains("empty"));
    }

    #[test]
    fn category_normalization_accepts_hyphen_and_underscore() {
        assert_eq!(normalize_category("stale_reference"), Some(STALE_REFERENCE_SUBJECT));
        assert_eq!(normalize_category("stale-reference"), Some(STALE_REFERENCE_SUBJECT));
        assert_eq!(normalize_category("organisation"), Some(ORGANIZATION_SUBJECT));
        assert_eq!(normalize_category("Coverage"), Some(COVERAGE_SUBJECT));
        assert_eq!(normalize_category("nonsense"), None);
    }

    #[test]
    fn severity_parser_demotes_high_and_defaults_to_low() {
        assert_eq!(parse_severity("low"), Severity::Low);
        assert_eq!(parse_severity("MEDIUM"), Severity::Medium);
        assert_eq!(parse_severity("high"), Severity::Medium);
        assert_eq!(parse_severity("bogus"), Severity::Low);
    }

    #[test]
    fn extra_knobs_default_when_unset() {
        let cfg = executor_cfg("/bin/true");
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg);
        assert_eq!(audit.readme_max_lines(), DEFAULT_README_MAX_LINES);
        assert_eq!(
            audit.page_max_lines_without_toc(),
            DEFAULT_PAGE_MAX_LINES_WITHOUT_TOC
        );
    }

    #[test]
    fn extra_knobs_read_from_settings() {
        let mut extra = HashMap::new();
        extra.insert(
            "readme_max_lines".to_string(),
            serde_yml::Value::Number(400.into()),
        );
        extra.insert(
            "page_max_lines_without_toc".to_string(),
            serde_yml::Value::Number(1000.into()),
        );
        let mut map = HashMap::new();
        map.insert(
            DocumentationAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = DocumentationAudit::new(&map, &cfg);
        assert_eq!(audit.readme_max_lines(), 400);
        assert_eq!(audit.page_max_lines_without_toc(), 1000);
    }

    #[test]
    fn extra_knobs_with_negative_int_fall_back_to_defaults() {
        let mut extra = HashMap::new();
        extra.insert(
            "readme_max_lines".to_string(),
            serde_yml::Value::Number((-5_i64).into()),
        );
        let mut map = HashMap::new();
        map.insert(
            DocumentationAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = DocumentationAudit::new(&map, &cfg);
        assert_eq!(audit.readme_max_lines(), DEFAULT_README_MAX_LINES);
    }

    #[test]
    fn resolve_prompt_uses_embedded_default_when_unset() {
        let cfg = executor_cfg("/bin/true");
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg);
        let prompt = audit
            .resolve_prompt(None)
            .expect("default prompt resolves");
        assert!(
            prompt.contains("documentation_audit_extra"),
            "default prompt must reference the extras YAML block"
        );
        assert!(prompt.contains("coverage"));
        assert!(prompt.contains("stale_reference"));
        assert!(prompt.contains("organization"));
    }

    /// Empty override files now fall back to the embedded default via
    /// the uniform `PromptLoader` rather than producing a hard error
    /// (a24). A one-shot WARN names the offending path.
    #[test]
    fn resolve_prompt_falls_back_when_override_file_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("empty.md");
        std::fs::write(&p, "   \n").unwrap();
        let mut map = HashMap::new();
        map.insert(
            DocumentationAudit::TYPE.into(),
            AuditSettings {
                prompt_path: Some(p),
                notify_on_clean: false,
                extra: HashMap::new(),
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = DocumentationAudit::new(&map, &cfg);
        let prompt = audit
            .resolve_prompt(None)
            .expect("empty override falls back");
        assert!(
            prompt.contains("documentation_audit_extra"),
            "fallback must use embedded default"
        );
    }

    #[test]
    fn resolve_prompt_reads_override_file_contents() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("override.md");
        std::fs::write(&p, "CUSTOM DOC PROMPT SENTINEL").unwrap();
        let mut map = HashMap::new();
        map.insert(
            DocumentationAudit::TYPE.into(),
            AuditSettings {
                prompt_path: Some(p),
                notify_on_clean: false,
                extra: HashMap::new(),
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = DocumentationAudit::new(&map, &cfg);
        let prompt = audit
            .resolve_prompt(None)
            .expect("override resolves");
        assert!(prompt.contains("CUSTOM DOC PROMPT SENTINEL"));
    }

    #[test]
    fn build_prompt_includes_extras_yaml_and_input_headers() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        std::fs::create_dir_all(workspace.join("openspec/specs/cap-x")).unwrap();
        std::fs::write(
            workspace.join("openspec/specs/cap-x/spec.md"),
            "# Capability X\n",
        )
        .unwrap();
        std::fs::write(workspace.join("README.md"), "# Project\n").unwrap();
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        std::fs::write(workspace.join("docs/OPERATIONS.md"), "# Ops\n").unwrap();

        let cfg = executor_cfg("/bin/true");
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg);
        let prompt = audit.build_prompt(workspace).expect("prompt builds");
        assert!(prompt.contains("documentation_audit_extra"));
        assert!(prompt.contains(&format!("readme_max_lines: {DEFAULT_README_MAX_LINES}")));
        assert!(prompt.contains(&format!(
            "page_max_lines_without_toc: {DEFAULT_PAGE_MAX_LINES_WITHOUT_TOC}"
        )));
        assert!(prompt.contains("## File: openspec/specs/cap-x/spec.md"));
        assert!(prompt.contains("## File: README.md"));
        assert!(prompt.contains("## File: docs/OPERATIONS.md"));
    }

    #[test]
    fn gather_inputs_skips_missing_directories_gracefully() {
        let tmp = TempDir::new().unwrap();
        // No openspec, no docs, no README.
        let inputs = gather_inputs(tmp.path());
        assert!(inputs.is_empty());
    }

    #[tokio::test]
    async fn run_returns_reported_with_three_findings_from_canned_stdout() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let canned = r#"{"findings":[
            {"category":"coverage","severity":"medium","anchor":"docs/CHATOPS.md","body":"verb propose missing"},
            {"category":"stale_reference","severity":"low","anchor":"docs/CONFIG.md:42","body":"dead field"},
            {"category":"organization","severity":"medium","anchor":"README.md","body":"too long"}
        ]}"#;
        let script = write_script(
            ws_dir.path(),
            "fake-claude.sh",
            &format!("#!/bin/sh\ncat <<'EOF'\n{canned}\nEOF\nexit 0\n"),
        );

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::Reported { findings, retries_used } => {
                assert_eq!(findings.len(), 3);
                assert_eq!(retries_used, 0);
                let cats: Vec<&str> = findings.iter().map(|f| f.subject.as_str()).collect();
                assert!(cats.contains(&COVERAGE_SUBJECT));
                assert!(cats.contains(&STALE_REFERENCE_SUBJECT));
                assert!(cats.contains(&ORGANIZATION_SUBJECT));
            }
            other => panic!("expected Reported, got {other:?}"),
        }
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_returns_reported_empty_when_findings_empty() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "fake-claude.sh",
            "#!/bin/sh\necho '{\"findings\":[]}'\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::Reported { findings, .. } => {
                assert!(findings.is_empty(), "empty findings expected");
            }
            other => panic!("expected Reported(empty), got {other:?}"),
        }
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_returns_err_on_malformed_stdout() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "bad.sh",
            "#!/bin/sh\necho 'not the JSON you are looking for'\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let err = audit.run(&mut ctx).await.expect_err("bad stdout errors");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not valid JSON") || msg.contains("findings"),
            "error must describe parse failure: {msg}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn workspace_unavailable_when_path_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("never-existed");
        let cfg = executor_cfg("/bin/true");
        let settings_dir = TempDir::new().unwrap();
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(tmp.path()),
            max_validation_retries: 0,
        };
        let outcome = audit.run(&mut ctx).await.expect("gate returns Ok");
        match outcome {
            AuditOutcome::WorkspaceUnavailable {
                audit_type, reason, ..
            } => {
                assert_eq!(audit_type, DocumentationAudit::TYPE);
                assert_eq!(reason, "workspace directory does not exist");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
        assert!(!workspace.exists(), "audit must not create the workspace");
    }

    #[tokio::test]
    async fn sandbox_settings_file_cleaned_up_after_run() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "fake-claude.sh",
            "#!/bin/sh\necho '{\"findings\":[]}'\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DocumentationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let _ = audit.run(&mut ctx).await.expect("run succeeds");
        let leftover: Vec<_> = std::fs::read_dir(settings_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert!(
            leftover.is_empty(),
            "sandbox settings file must be deleted after run; leftover: {leftover:?}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Anti-prompt-drift assertion: the embedded default prompt must
    /// document the three categories AND the "no high severity" rule.
    /// If you are removing either deliberately, revisit the audit
    /// design — the prompt's check structure AND the no-high rule are
    /// part of the spec.
    #[test]
    fn embedded_prompt_documents_the_three_categories_and_no_high_rule() {
        assert!(DEFAULT_PROMPT.contains("coverage"));
        assert!(DEFAULT_PROMPT.contains("stale_reference"));
        assert!(DEFAULT_PROMPT.contains("organization"));
        let lower = DEFAULT_PROMPT.to_lowercase();
        assert!(
            lower.contains("do not emit `high`") || lower.contains("do not emit high"),
            "embedded prompt must forbid `high` severity explicitly"
        );
    }

    /// Anti-prompt-drift assertion: the prompt must mention the two
    /// `extra` knobs by name so the LLM respects them when emitting
    /// organization findings.
    #[test]
    fn embedded_prompt_names_extras_knobs() {
        assert!(DEFAULT_PROMPT.contains("readme_max_lines"));
        assert!(DEFAULT_PROMPT.contains("page_max_lines_without_toc"));
    }
}
