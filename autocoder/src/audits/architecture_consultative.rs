//! Architecture consultative audit. Invokes the wrapped agent CLI
//! (typically `claude`) with a read-only sandbox (`Read`, `Glob`, `Grep`,
//! `Bash`) and a consultative architecture prompt. Parses the agent's
//! structured JSON output into 0-5 [`Finding`]s, each phrased as a
//! question anchored to a specific file:line range, and returns
//! `AuditOutcome::Reported`.
//!
//! `requires_head_change = true` — re-asking the same architecture
//! questions about the same SHA wastes CLI invocations.
//! `WritePolicy::None` — strictly advisory; the operator decides which
//! questions (if any) are worth turning into work.
//!
//! Cadence intent: this audit is designed for `monthly` or `quarterly`
//! cadence. Daily/weekly invocations produce noise.

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

/// Tools the consultative agent may call. Excludes `Write` and `Edit`
/// so the sandbox blocks workspace modifications outright; the audit-
/// run log captures the agent's stdout for forensic review.
const ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Bash"];

/// Maximum number of findings the audit will accept. More than this
/// indicates the agent ignored its cap; the audit fails rather than
/// truncating, so operators see the misbehavior in chatops.
const MAX_FINDINGS: usize = 5;

/// Maximum number of characters of stdout to embed in a parse-failure
/// error message. The full stdout always lands in the audit-run log.
const STDOUT_EXCERPT_CHARS: usize = 400;

pub struct ArchitectureConsultativeAudit {
    settings: AuditSettings,
    executor_command: String,
    executor_timeout_secs: u64,
    sandbox: ResolvedSandbox,
    /// Override for the directory the per-invocation sandbox settings
    /// file is written to. `None` (production) means
    /// `std::env::temp_dir()`. Tests pass a per-test TempDir.
    settings_dir: Option<PathBuf>,
}

impl ArchitectureConsultativeAudit {
    pub const TYPE: &'static str = "architecture_consultative";

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

    /// Resolve the consultative prompt via the uniform [`PromptLoader`].
    /// `settings.prompt_path` is the audit's nested override
    /// (`audits.settings.architecture_consultative.prompt_path`);
    /// missing/empty values fall through to the embedded default.
    fn resolve_prompt(&self, workspace: Option<&Path>) -> Result<String> {
        Ok(PromptLoader::load(
            PromptId::AuditArchitectureConsultative,
            self.settings.prompt_path.as_deref(),
            None,
            workspace,
        ))
    }
}

#[async_trait]
impl Audit for ArchitectureConsultativeAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn description(&self) -> &'static str {
        "advisory architecture findings via LLM consultation"
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::None
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        // Workspace-validity gate (see `audits-require-valid-workspace`).
        // MUST run before any other work — particularly before any
        // `fs::create_dir_all` site — so a broken workspace cannot
        // accumulate audit-created partial state.
        if !workspace_is_valid(ctx.workspace) {
            return Ok(workspace_unavailable_outcome(
                Self::TYPE,
                ctx.workspace,
                &ctx.repo.url,
            ));
        }

        let prompt = self.resolve_prompt(Some(ctx.workspace))?;

        let mut sandbox = self.sandbox.clone();
        sandbox.allowed_tools = ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect();

        let (settings_path, _settings_guard) =
            write_sandbox_settings(&sandbox, self.settings_dir.as_deref())
                .context("generating architecture-consultative sandbox settings file")?;

        let _ = ctx.log_writer.write_section(
            "architecture_consultative_preamble",
            &format!(
                "executor_command: {}\ntimeout_secs: {}\nprompt_source: {}\nsettings_file: {}\nallowed_tools: {}",
                self.executor_command,
                self.executor_timeout_secs,
                self.settings
                    .prompt_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<embedded default>".to_string()),
                settings_path.display(),
                sandbox.allowed_tools.join(","),
            ),
        );
        let _ = ctx
            .log_writer
            .write_section("architecture_consultative_prompt", &prompt);

        let outcome = run_subprocess(
            &self.executor_command,
            &settings_path,
            &sandbox.allowed_tools,
            ctx.workspace,
            &prompt,
            Duration::from_secs(self.executor_timeout_secs),
        )
        .await
        .context("spawning architecture-consultative CLI subprocess")?;

        let _ = ctx.log_writer.write_section(
            "architecture_consultative_stdout",
            if outcome.stdout.is_empty() {
                "(empty)"
            } else {
                outcome.stdout.as_str()
            },
        );
        let _ = ctx.log_writer.write_section(
            "architecture_consultative_stderr",
            if outcome.stderr.is_empty() {
                "(empty)"
            } else {
                outcome.stderr.as_str()
            },
        );

        if let Some(err) = outcome_to_terminal_err(
            &outcome,
            &mut ctx.log_writer,
            "architecture_consultative",
            self.executor_timeout_secs,
        ) {
            return Err(err);
        }

        let findings = match parse_findings(&outcome.stdout) {
            Ok(f) => f,
            Err(e) => {
                let _ = ctx.log_writer.write_section(
                    "architecture_consultative_outcome",
                    &format!("kind: Err\nreason: {e:#}"),
                );
                return Err(e);
            }
        };
        let _ = ctx.log_writer.write_section(
            "architecture_consultative_outcome",
            &format!("kind: Reported\nfindings_count: {}", findings.len()),
        );
        // This audit produces advisory findings (`Reported`) — it does NOT
        // write a proposal directory under `openspec/changes/<slug>/`.
        // The post-write `openspec validate --strict` retry loop in
        // `audits::validate_with_retry` is unnecessary here: there is no
        // proposal to validate. `retries_used` is therefore always 0.
        // (See change `a01-audit-proposal-self-validation`.)
        //
        // The `🔍 created proposal` chatops notification documented in
        // `a02-audit-proposal-created-notification` therefore does NOT
        // fire from this audit: there is no proposal-creation event to
        // signal. Operators still see the existing `📋` findings post
        // (or `✅` when `notify_on_clean` is set) through the scheduler's
        // `Reported`-outcome dispatch.
        Ok(AuditOutcome::reported(findings))
    }
}

/// Parse `stdout` as `{ "findings": [...] }`. Rejects more than
/// `MAX_FINDINGS` entries (the prompt's cap; the agent disregarding
/// the cap is treated as audit failure rather than silently truncated).
pub(crate) fn parse_findings(stdout: &str) -> Result<Vec<Finding>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "architecture_consultative: agent produced empty stdout (expected `{{ \"findings\": [...] }}`)"
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
        anyhow!(
            "architecture_consultative: stdout is not valid JSON: {e}; excerpt: {}",
            excerpt(stdout)
        )
    })?;
    let arr = parsed
        .get("findings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            anyhow!(
                "architecture_consultative: stdout JSON missing top-level `findings` array; excerpt: {}",
                excerpt(stdout)
            )
        })?;
    if arr.len() > MAX_FINDINGS {
        return Err(anyhow!(
            "architecture_consultative: agent emitted {} findings; prompt caps at {}; excerpt: {}",
            arr.len(),
            MAX_FINDINGS,
            excerpt(stdout)
        ));
    }
    let mut findings = Vec::with_capacity(arr.len());
    for (idx, raw) in arr.iter().enumerate() {
        let entry: RawFinding = serde_json::from_value(raw.clone()).map_err(|e| {
            anyhow!(
                "architecture_consultative: findings[{idx}] does not match the expected shape: {e}; excerpt: {}",
                excerpt(stdout)
            )
        })?;
        let severity = parse_severity(&entry.severity);
        findings.push(Finding {
            severity,
            subject: entry.subject,
            body: entry.body,
            anchor: Some(entry.anchor),
        });
    }
    Ok(findings)
}

#[derive(Debug, Deserialize)]
struct RawFinding {
    subject: String,
    body: String,
    anchor: String,
    severity: String,
}

/// Consultative findings are `low` or `medium` only — the prompt does
/// not authorize `high`. Unknown / out-of-range severities downgrade
/// to `Low` with a warn log so the audit succeeds rather than failing
/// on a stylistic difference.
fn parse_severity(raw: &str) -> Severity {
    match raw.trim().to_ascii_lowercase().as_str() {
        "medium" => Severity::Medium,
        "low" => Severity::Low,
        other => {
            // no-url: pure severity parser, no AuditContext in scope
            tracing::warn!(
                severity = other,
                "architecture_consultative: unexpected severity `{other}`; defaulting to Low"
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

/// Pure transformation: given a SubprocessOutcome, return Some(error) if
/// the outcome is terminal (timed out OR non-zero exit). Returns None when
/// the caller should continue processing (parse stdout into findings).
///
/// Extracted from `run()` so tests can exercise the timeout/exit error
/// shapes by constructing synthetic SubprocessOutcome values directly,
/// avoiding real subprocesses, timers, and the race condition that
/// comes with them.
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

/// Spawn the wrapped CLI, write `prompt` on its stdin, wait with the
/// configured timeout, return captured stdout + stderr. Mirrors the
/// drift audit's run_subprocess.
async fn run_subprocess(
    command: &str,
    settings_path: &std::path::Path,
    allowed_tools: &[String],
    workspace: &std::path::Path,
    prompt: &str,
    timeout: Duration,
) -> Result<SubprocessOutcome> {
    // ETXTBSY retry: see docs/test-reliability.md
    // "ETXTBSY from concurrent audit-CLI fixtures".
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
    .with_context(|| {
        format!("spawning architecture-consultative command `{command}`")
    })?;

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
        Some(Err(e)) => {
            Err(e).context("waiting on architecture-consultative child process")
        }
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
        AuditLogWriter::open(&paths, workspace, ArchitectureConsultativeAudit::TYPE)
            .expect("log writer opens")
    }

    #[test]
    fn parses_well_formed_findings_json() {
        let stdout = r#"{
            "findings": [
                {
                    "subject": "Should the parser move into its own module?",
                    "body": "The parser at parser.rs has accumulated imports from four unrelated subsystems.",
                    "anchor": "src/parser.rs:120-300",
                    "severity": "medium"
                },
                {
                    "subject": "Is the boundary between state and cache still meaningful?",
                    "body": "Each calls the other's pub(crate) helpers repeatedly.",
                    "anchor": "src/state.rs:1-200",
                    "severity": "low"
                }
            ]
        }"#;
        let findings = parse_findings(stdout).expect("parses");
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].severity, Severity::Medium);
        assert!(findings[0].subject.starts_with("Should"));
        assert_eq!(
            findings[0].anchor.as_deref(),
            Some("src/parser.rs:120-300")
        );
        assert!(findings[0].body.contains("parser.rs"));
        assert_eq!(findings[1].severity, Severity::Low);
    }

    #[test]
    fn parses_zero_findings_as_no_findings_outcome() {
        let stdout = r#"{"findings": []}"#;
        let findings = parse_findings(stdout).expect("parses empty array");
        assert!(findings.is_empty());
    }

    #[test]
    fn rejects_runs_with_more_than_5_findings() {
        let stdout = r#"{
            "findings": [
                {"subject":"q1?","body":"b","anchor":"a:1","severity":"low"},
                {"subject":"q2?","body":"b","anchor":"a:1","severity":"low"},
                {"subject":"q3?","body":"b","anchor":"a:1","severity":"low"},
                {"subject":"q4?","body":"b","anchor":"a:1","severity":"low"},
                {"subject":"q5?","body":"b","anchor":"a:1","severity":"low"},
                {"subject":"q6?","body":"b","anchor":"a:1","severity":"low"}
            ]
        }"#;
        let err = parse_findings(stdout).expect_err("six findings must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("6 findings"), "got: {msg}");
        assert!(msg.contains("caps at 5"), "got: {msg}");
    }

    #[test]
    fn malformed_json_returns_err_with_excerpt() {
        let stdout = "this is not JSON at all, just some prose the agent wrote";
        let err = parse_findings(stdout).expect_err("non-JSON must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("not valid JSON"), "got: {msg}");
        assert!(msg.contains("just some prose"), "excerpt missing: {msg}");
    }

    /// Anti-prompt-drift assertion: the anti-microservices clause must
    /// survive every prompt edit. If you are removing it deliberately,
    /// also revisit the audit's framing — the clause exists because
    /// the bare prompt regularly produces "split into microservices"
    /// suggestions that suit no real project at this scale.
    #[test]
    fn prompt_contains_anti_microservices_clause() {
        let cfg = executor_cfg("/bin/true");
        let audit = ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg);
        let prompt = audit.resolve_prompt(None).expect("default prompt resolves");
        assert!(
            prompt.contains("microservices"),
            "prompt must mention microservices in its anti-pattern list: {prompt}"
        );
        assert!(
            prompt.to_lowercase().contains("do not suggest splitting"),
            "prompt must forbid splitting the codebase: {prompt}"
        );
    }

    /// Anti-prompt-drift assertion: the language-agnostic framing must
    /// survive. The audit is meant to operate on observable structure,
    /// not language-specific idioms.
    #[test]
    fn prompt_contains_language_agnostic_clause() {
        let cfg = executor_cfg("/bin/true");
        let audit = ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg);
        let prompt = audit.resolve_prompt(None).expect("default prompt resolves");
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("language-agnostic")
                || lower.contains("language agnostic"),
            "prompt must declare itself language-agnostic: {prompt}"
        );
        assert!(
            lower.contains("polyglot"),
            "prompt must permit polyglot codebases: {prompt}"
        );
    }

    #[test]
    fn missing_top_level_findings_key_returns_err() {
        let stdout = r#"{"results": []}"#;
        let err = parse_findings(stdout).expect_err("missing key must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("findings"), "got: {msg}");
    }

    #[test]
    fn empty_stdout_returns_err() {
        let err = parse_findings("   \n").expect_err("empty stdout must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "got: {msg}");
    }

    #[test]
    fn severity_parser_accepts_canonical_strings() {
        assert_eq!(parse_severity("low"), Severity::Low);
        assert_eq!(parse_severity("LOW"), Severity::Low);
        assert_eq!(parse_severity("medium"), Severity::Medium);
        assert_eq!(parse_severity("MEDIUM"), Severity::Medium);
        // High is not authorized by the consultative prompt; it
        // downgrades to Low rather than escalating.
        assert_eq!(parse_severity("high"), Severity::Low);
        assert_eq!(parse_severity("bogus"), Severity::Low);
    }

    #[test]
    fn new_reads_prompt_path_and_notify_on_clean_from_settings() {
        let mut settings_map = HashMap::new();
        settings_map.insert(
            ArchitectureConsultativeAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: Some(PathBuf::from("/tmp/example.md")),
                notify_on_clean: true,
                extra: HashMap::new(),
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = ArchitectureConsultativeAudit::new(&settings_map, &cfg);
        assert_eq!(
            audit.settings.prompt_path.as_deref(),
            Some(std::path::Path::new("/tmp/example.md"))
        );
        assert!(audit.settings.notify_on_clean);
        assert_eq!(audit.executor_command, "/bin/true");
        assert_eq!(audit.executor_timeout_secs, 30);
    }

    #[test]
    fn new_falls_back_to_defaults_when_settings_absent() {
        let cfg = executor_cfg("claude");
        let audit =
            ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg);
        assert!(audit.settings.prompt_path.is_none());
        assert!(!audit.settings.notify_on_clean);
        assert_eq!(audit.executor_command, "claude");
    }

    #[test]
    fn resolve_prompt_uses_embedded_default_when_unset() {
        let cfg = executor_cfg("/bin/true");
        let audit =
            ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg);
        let prompt = audit.resolve_prompt(None).expect("default prompt resolves");
        assert!(prompt.contains("findings"), "expected default prompt body");
        assert!(
            prompt.contains("consultative") || prompt.contains("question"),
            "expected default prompt to mention its consultative framing"
        );
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
            ArchitectureConsultativeAudit::TYPE.into(),
            AuditSettings {
                prompt_path: Some(p),
                notify_on_clean: false,
                extra: HashMap::new(),
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = ArchitectureConsultativeAudit::new(&map, &cfg);
        let prompt = audit
            .resolve_prompt(None)
            .expect("empty override falls back to embedded");
        assert!(
            prompt.contains("findings"),
            "fallback must use embedded default"
        );
    }

    #[test]
    fn audit_type_and_policy_are_fixed() {
        let cfg = executor_cfg("/bin/true");
        let audit =
            ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg);
        assert_eq!(audit.audit_type(), "architecture_consultative");
        assert!(audit.requires_head_change());
        assert!(matches!(audit.write_policy(), WritePolicy::None));
    }

    #[tokio::test]
    async fn run_writes_full_stdout_to_audit_log() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        // Satisfy the workspace-validity gate
        // (see `audits-require-valid-workspace`).
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "fake-claude.sh",
            "#!/bin/sh\ncat <<'EOF'\n{\"findings\":[{\"subject\":\"Should foo move?\",\"body\":\"detail\",\"anchor\":\"src/foo.rs:1\",\"severity\":\"medium\"}]}\nEOF\nexit 0\n",
        );

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg)
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
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].severity, Severity::Medium);
                assert!(findings[0].subject.starts_with("Should"));
                assert_eq!(
                    findings[0].anchor.as_deref(),
                    Some("src/foo.rs:1")
                );
                assert_eq!(retries_used, 0, "architecture_consultative does not validate proposals");
            }
            other => panic!("expected Reported, got {other:?}"),
        }
        let log = std::fs::read_to_string(&log_path).expect("log readable");
        assert!(
            log.contains("architecture_consultative_stdout"),
            "log missing stdout section: {log}"
        );
        assert!(
            log.contains("\"findings\""),
            "log missing canned JSON: {log}"
        );
        assert!(
            log.contains("architecture_consultative_prompt"),
            "log missing prompt section: {log}"
        );
        assert!(
            log.contains("architecture_consultative_preamble"),
            "log missing preamble section: {log}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_returns_err_on_malformed_stdout() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        // Satisfy the workspace-validity gate
        // (see `audits-require-valid-workspace`).
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "bad.sh",
            "#!/bin/sh\necho 'this is not the JSON you are looking for'\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg)
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
        let err = audit
            .run(&mut ctx)
            .await
            .expect_err("malformed stdout errors");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not valid JSON") || msg.contains("findings"),
            "error must describe parse failure: {msg}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Pure-data test: feed a synthesized `SubprocessOutcome` with
    /// `timed_out: true` directly into `outcome_to_terminal_err` and
    /// assert the resulting error + log entries. No subprocess, no
    /// timer, no race — verifies the audit's translation logic, which
    /// is what we actually care about. The race-condition version
    /// (real subprocess + real timer) was deterministic locally on
    /// some platforms and flaky on others; the pure-data version is
    /// deterministic everywhere.
    #[test]
    fn outcome_to_terminal_err_translates_timed_out_to_error() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let mut log_writer = make_log_writer(workspace);
        let log_path = log_writer.path().to_path_buf();
        let outcome = SubprocessOutcome {
            timed_out: true,
            exit_status: None,
            stdout: String::new(),
            stderr: "timeout".into(),
        };
        let err = outcome_to_terminal_err(
            &outcome,
            &mut log_writer,
            "architecture_consultative",
            1,
        )
        .expect("timed_out outcome must produce Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("architecture_consultative"),
            "error must name the audit type: {msg}"
        );
        assert!(
            msg.contains("timeout"),
            "error must mention timeout: {msg}"
        );
        let log = std::fs::read_to_string(&log_path).expect("log readable");
        assert!(
            log.contains("kind: Err"),
            "log must record Err outcome: {log}"
        );
        assert!(
            log.contains("reason: timeout"),
            "log must record timeout reason: {log}"
        );
    }

    /// Companion: synthesized non-zero exit produces the exit-error variant.
    #[test]
    fn outcome_to_terminal_err_translates_nonzero_exit_to_error() {
        use std::os::unix::process::ExitStatusExt;
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let mut log_writer = make_log_writer(workspace);
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(7 << 8)),
            stdout: String::new(),
            stderr: "boom".into(),
        };
        let err = outcome_to_terminal_err(
            &outcome,
            &mut log_writer,
            "architecture_consultative",
            30,
        )
        .expect("nonzero exit must produce Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("exit"), "error must mention exit: {msg}");
        assert!(msg.contains("boom"), "error must include stderr excerpt: {msg}");
    }

    /// Companion: a clean outcome (no timeout, exit 0) returns None — the
    /// caller should continue to parse stdout.
    #[test]
    fn outcome_to_terminal_err_returns_none_for_clean_outcome() {
        use std::os::unix::process::ExitStatusExt;
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let mut log_writer = make_log_writer(workspace);
        let outcome = SubprocessOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: r#"{"findings":[]}"#.into(),
            stderr: String::new(),
        };
        assert!(
            outcome_to_terminal_err(&outcome, &mut log_writer, "architecture_consultative", 30)
                .is_none(),
            "clean outcome must return None so caller proceeds to parse"
        );
    }

    /// Workspace-validity gate (see `audits-require-valid-workspace`):
    /// invoking the audit against a nonexistent workspace must return
    /// `Ok(WorkspaceUnavailable { reason: "workspace directory does not exist" })`
    /// immediately without creating the path as a side effect.
    #[tokio::test]
    async fn workspace_unavailable_when_path_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("never-existed");
        assert!(!workspace.exists(), "fixture must start absent");

        let cfg = executor_cfg("/bin/true");
        let settings_dir = TempDir::new().unwrap();
        let audit = ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        // Open the log writer against a temp dir (not the missing
        // workspace) so the test doesn't need to materialize the path.
        let log_workspace = tmp.path();
        let mut ctx = AuditContext {
            workspace: &workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(log_workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("gate returns Ok");
        match outcome {
            AuditOutcome::WorkspaceUnavailable {
                audit_type,
                workspace_path,
                reason,
            } => {
                assert_eq!(audit_type, ArchitectureConsultativeAudit::TYPE);
                assert_eq!(workspace_path, workspace);
                assert_eq!(reason, "workspace directory does not exist");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
        assert!(
            !workspace.exists(),
            "audit must not create the workspace path as a side effect"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Workspace-validity gate (see `audits-require-valid-workspace`):
    /// invoking the audit against a directory that has no `.git/`
    /// subdirectory must return `Ok(WorkspaceUnavailable { reason:
    /// "workspace exists but has no .git/ subdirectory" })` without
    /// creating any new file or subdirectory in the workspace.
    #[tokio::test]
    async fn workspace_unavailable_when_dot_git_missing() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws-no-git");
        std::fs::create_dir_all(&workspace).unwrap();
        let before: Vec<std::ffi::OsString> = std::fs::read_dir(&workspace)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();

        let cfg = executor_cfg("/bin/true");
        let settings_dir = TempDir::new().unwrap();
        let audit = ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let log_workspace = tmp.path();
        let mut ctx = AuditContext {
            workspace: &workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(log_workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("gate returns Ok");
        match outcome {
            AuditOutcome::WorkspaceUnavailable { reason, .. } => {
                assert_eq!(reason, "workspace exists but has no .git/ subdirectory");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
        let after: Vec<std::ffi::OsString> = std::fs::read_dir(&workspace)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after, "audit must not create any new entries");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_returns_err_on_nonzero_exit() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        // Satisfy the workspace-validity gate
        // (see `audits-require-valid-workspace`).
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "fail.sh",
            "#!/bin/sh\necho 'partial' \necho 'boom' >&2\nexit 7\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = ArchitectureConsultativeAudit::new(&HashMap::new(), &cfg)
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
        let err = audit
            .run(&mut ctx)
            .await
            .expect_err("nonzero exit errors");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exit"),
            "error must mention exit code: {msg}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }
}
