//! Drift audit. Invokes the wrapped agent CLI (typically `claude`) with
//! a read-only sandbox (`Read`, `Glob`, `Grep`, `Bash`) and a drift-
//! detection prompt, parses the agent's structured JSON output into
//! [`Finding`]s, and returns `AuditOutcome::Reported`.
//!
//! `requires_head_change = true` — drift can only emerge with code or
//! spec changes; rerunning without a HEAD shift wastes CLI invocations.
//! `WritePolicy::None` — the audit is strictly advisory; the operator
//! decides whether each finding becomes a code fix, a spec fix, or is
//! dismissed.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::{
    Audit, AuditContext, AuditOutcome, Finding, Severity, WritePolicy,
    write_sandbox_settings,
};
use crate::config::{AuditSettings, ExecutorConfig, ResolvedSandbox};

/// Tools the drift agent may call. Excludes `Write` and `Edit` so the
/// sandbox blocks workspace modifications outright; the audit-run log
/// captures the agent's stdout for forensic review.
const ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Bash"];

/// Built-in default drift prompt, embedded at compile time so the
/// binary runs without requiring `prompts/` on disk.
const DEFAULT_DRIFT_PROMPT: &str = include_str!("../../../prompts/drift-audit.md");

/// Maximum number of characters of stdout to embed in a parse-failure
/// error message. The full stdout always lands in the audit-run log.
const STDOUT_EXCERPT_CHARS: usize = 400;

pub struct DriftAudit {
    settings: AuditSettings,
    executor_command: String,
    executor_timeout_secs: u64,
    sandbox: ResolvedSandbox,
    /// Override for the directory the per-invocation sandbox settings
    /// file is written to. `None` (production) means
    /// `std::env::temp_dir()`. Tests use this to isolate the settings
    /// file from concurrent tests sharing the same OS temp dir.
    settings_dir: Option<PathBuf>,
}

impl DriftAudit {
    pub const TYPE: &'static str = "drift_audit";

    /// Construct the audit. Pulls the per-audit `AuditSettings` out of
    /// the map (defaults if absent) and snapshots the executor's command
    /// + timeout. Sandbox defaults are derived from the executor's
    /// configured deny lists.
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

    /// Test-only override: write the sandbox settings file to `dir`
    /// instead of the OS temp dir.
    #[cfg(test)]
    pub(crate) fn with_settings_dir(mut self, dir: PathBuf) -> Self {
        self.settings_dir = Some(dir);
        self
    }

    /// Test-only override: replace the wrapped CLI command (e.g. point
    /// at a fixture shell script that produces canned stdout).
    #[cfg(test)]
    pub(crate) fn with_command(mut self, command: String) -> Self {
        self.executor_command = command;
        self
    }

    /// Resolve the drift prompt. When `settings.prompt_path` is set,
    /// read the file (empty content errors so the daemon does not feed
    /// an empty prompt to the wrapped CLI). Otherwise use the embedded
    /// default.
    fn resolve_prompt(&self) -> Result<String> {
        match &self.settings.prompt_path {
            Some(path) => {
                let body = std::fs::read_to_string(path).with_context(|| {
                    format!("reading drift-audit prompt override at {}", path.display())
                })?;
                if body.trim().is_empty() {
                    return Err(anyhow!(
                        "drift-audit prompt override at {} is empty",
                        path.display()
                    ));
                }
                Ok(body)
            }
            None => Ok(DEFAULT_DRIFT_PROMPT.to_string()),
        }
    }
}

#[async_trait]
impl Audit for DriftAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::None
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        let prompt = self.resolve_prompt()?;

        // Force the allowed_tools list per the spec; everything else
        // (deny patterns) comes from the executor's resolved sandbox so
        // operators retain a single place to tune Bash/Read denies.
        let mut sandbox = self.sandbox.clone();
        sandbox.allowed_tools = ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect();

        let (settings_path, _settings_guard) =
            write_sandbox_settings(&sandbox, self.settings_dir.as_deref())
                .context("generating drift-audit sandbox settings file")?;

        let _ = ctx.log_writer.write_section(
            "drift_audit_preamble",
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
        let _ = ctx.log_writer.write_section("drift_audit_prompt", &prompt);

        let outcome = run_subprocess(
            &self.executor_command,
            &settings_path,
            &sandbox.allowed_tools,
            ctx.workspace,
            &prompt,
            Duration::from_secs(self.executor_timeout_secs),
        )
        .await
        .context("spawning drift-audit CLI subprocess")?;

        let _ = ctx.log_writer.write_section(
            "drift_audit_stdout",
            if outcome.stdout.is_empty() {
                "(empty)"
            } else {
                outcome.stdout.as_str()
            },
        );
        let _ = ctx.log_writer.write_section(
            "drift_audit_stderr",
            if outcome.stderr.is_empty() {
                "(empty)"
            } else {
                outcome.stderr.as_str()
            },
        );

        if outcome.timed_out {
            let _ = ctx.log_writer.write_section(
                "drift_audit_outcome",
                "kind: Err\nreason: timeout",
            );
            return Err(anyhow!(
                "drift_audit: CLI exceeded the {}s timeout",
                self.executor_timeout_secs
            ));
        }

        if let Some(status) = outcome.exit_status {
            if !status.success() {
                let _ = ctx.log_writer.write_section(
                    "drift_audit_outcome",
                    &format!("kind: Err\nreason: exit {status}"),
                );
                return Err(anyhow!(
                    "drift_audit: CLI exited {status}; stderr excerpt: {}",
                    excerpt(&outcome.stderr)
                ));
            }
        }

        let findings = parse_findings(&outcome.stdout)?;
        let _ = ctx.log_writer.write_section(
            "drift_audit_outcome",
            &format!("kind: Reported\nfindings_count: {}", findings.len()),
        );
        Ok(AuditOutcome::Reported(findings))
    }
}

/// Parse `stdout` as `{ "findings": [...] }`. On failure, returns an
/// `Err` whose message includes a truncated stdout excerpt so the
/// scheduler's log lines and the audit-run log align.
pub(crate) fn parse_findings(stdout: &str) -> Result<Vec<Finding>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "drift_audit: agent produced empty stdout (expected `{{ \"findings\": [...] }}`)"
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
        anyhow!(
            "drift_audit: stdout is not valid JSON: {e}; excerpt: {}",
            excerpt(stdout)
        )
    })?;
    let arr = parsed
        .get("findings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            anyhow!(
                "drift_audit: stdout JSON missing top-level `findings` array; excerpt: {}",
                excerpt(stdout)
            )
        })?;
    let mut findings = Vec::with_capacity(arr.len());
    for (idx, raw) in arr.iter().enumerate() {
        let entry: RawFinding = serde_json::from_value(raw.clone()).map_err(|e| {
            anyhow!(
                "drift_audit: findings[{idx}] does not match the expected shape: {e}; excerpt: {}",
                excerpt(stdout)
            )
        })?;
        let severity = parse_severity(&entry.severity);
        let subject = format!(
            "[{capability}] {requirement}",
            capability = entry.capability,
            requirement = entry.requirement,
        );
        let anchors = entry.code_anchors.unwrap_or_default();
        let anchor = anchors.first().cloned();
        let mut body = String::new();
        if !anchors.is_empty() {
            body.push_str("code_anchors:\n");
            for a in &anchors {
                body.push_str("  - ");
                body.push_str(a);
                body.push('\n');
            }
            body.push('\n');
        }
        body.push_str(&entry.divergence);
        findings.push(Finding {
            severity,
            subject,
            body,
            anchor,
        });
    }
    Ok(findings)
}

#[derive(Debug, Deserialize)]
struct RawFinding {
    capability: String,
    requirement: String,
    severity: String,
    #[serde(default)]
    code_anchors: Option<Vec<String>>,
    divergence: String,
}

fn parse_severity(raw: &str) -> Severity {
    match raw.trim().to_ascii_lowercase().as_str() {
        "high" => Severity::High,
        "medium" => Severity::Medium,
        "low" => Severity::Low,
        other => {
            tracing::warn!(
                severity = other,
                "drift_audit: unknown severity `{other}`; defaulting to Low"
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

/// Spawn the wrapped CLI, write `prompt` on its stdin, wait with the
/// configured timeout, return captured stdout + stderr. Mirrors the
/// claude_cli executor's run_subprocess shape, scoped down to what
/// the drift audit needs (no MCP server, no busy-marker sidecar).
async fn run_subprocess(
    command: &str,
    settings_path: &std::path::Path,
    allowed_tools: &[String],
    workspace: &std::path::Path,
    prompt: &str,
    timeout: Duration,
) -> Result<SubprocessOutcome> {
    let mut child = Command::new(command)
        .arg("--settings")
        .arg(settings_path)
        .arg("--allowedTools")
        .arg(allowed_tools.join(","))
        .arg("--permission-mode")
        .arg("acceptEdits")
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("spawning drift-audit command `{command}`"))?;

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
        Some(Err(e)) => Err(e).context("waiting on drift-audit child process"),
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
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
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
        AuditLogWriter::open(workspace, "drift_audit").expect("log writer opens")
    }

    #[test]
    fn parses_well_formed_findings_json() {
        let stdout = r#"{
            "findings": [
                {
                    "capability": "orchestrator-cli",
                    "requirement": "Per-repository asynchronous polling loop",
                    "severity": "high",
                    "code_anchors": ["autocoder/src/polling_loop.rs:45-95"],
                    "divergence": "Spec requires X; code does Y."
                },
                {
                    "capability": "executor",
                    "requirement": "Wraps an LLM CLI",
                    "severity": "medium",
                    "code_anchors": ["autocoder/src/executor/claude_cli.rs:1"],
                    "divergence": "Timeout handling differs from spec."
                }
            ]
        }"#;
        let findings = parse_findings(stdout).expect("parses");
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].severity, Severity::High);
        assert!(findings[0].subject.contains("orchestrator-cli"));
        assert!(findings[0].subject.contains("Per-repository"));
        assert_eq!(
            findings[0].anchor.as_deref(),
            Some("autocoder/src/polling_loop.rs:45-95")
        );
        assert!(findings[0].body.contains("Spec requires X"));
        assert_eq!(findings[1].severity, Severity::Medium);
    }

    #[test]
    fn parses_empty_findings_array_to_no_findings_outcome() {
        let stdout = r#"{"findings": []}"#;
        let findings = parse_findings(stdout).expect("parses empty array");
        assert!(findings.is_empty());
    }

    #[test]
    fn malformed_json_returns_err_with_excerpt() {
        let stdout = "this is not JSON at all, just some prose the agent wrote";
        let err = parse_findings(stdout).expect_err("non-JSON must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("not valid JSON"), "got: {msg}");
        assert!(msg.contains("just some prose"), "excerpt missing: {msg}");
    }

    #[test]
    fn missing_top_level_findings_key_returns_err() {
        let stdout = r#"{"results": []}"#;
        let err = parse_findings(stdout).expect_err("missing key must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("findings"), "got: {msg}");
    }

    #[test]
    fn findings_non_array_returns_err() {
        let stdout = r#"{"findings": "not-an-array"}"#;
        let err = parse_findings(stdout).expect_err("non-array must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("findings"), "got: {msg}");
    }

    #[test]
    fn unknown_severity_string_maps_to_low_with_warn_log() {
        let stdout = r#"{
            "findings": [
                {
                    "capability": "cap",
                    "requirement": "req",
                    "severity": "catastrophic",
                    "code_anchors": [],
                    "divergence": "details"
                }
            ]
        }"#;
        let findings = parse_findings(stdout).expect("parses unknown severity");
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].severity,
            Severity::Low,
            "unknown severities must downgrade to Low"
        );
    }

    #[test]
    fn severity_parser_accepts_canonical_strings() {
        assert_eq!(parse_severity("high"), Severity::High);
        assert_eq!(parse_severity("HIGH"), Severity::High);
        assert_eq!(parse_severity("medium"), Severity::Medium);
        assert_eq!(parse_severity("low"), Severity::Low);
        assert_eq!(parse_severity("bogus"), Severity::Low);
    }

    #[test]
    fn new_reads_prompt_path_and_notify_on_clean_from_settings() {
        let mut extra = HashMap::new();
        let mut settings_map = HashMap::new();
        extra.insert(
            "ignored".into(),
            serde_yaml::Value::String("for-future-knobs".into()),
        );
        settings_map.insert(
            DriftAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: Some(PathBuf::from("/tmp/example.md")),
                notify_on_clean: true,
                extra,
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = DriftAudit::new(&settings_map, &cfg);
        assert_eq!(audit.settings.prompt_path.as_deref(), Some(std::path::Path::new("/tmp/example.md")));
        assert!(audit.settings.notify_on_clean);
        assert_eq!(audit.executor_command, "/bin/true");
        assert_eq!(audit.executor_timeout_secs, 30);
    }

    #[test]
    fn new_falls_back_to_defaults_when_settings_absent() {
        let cfg = executor_cfg("claude");
        let audit = DriftAudit::new(&HashMap::new(), &cfg);
        assert!(audit.settings.prompt_path.is_none());
        assert!(!audit.settings.notify_on_clean);
        assert_eq!(audit.executor_command, "claude");
    }

    #[test]
    fn resolve_prompt_uses_embedded_default_when_unset() {
        let cfg = executor_cfg("/bin/true");
        let audit = DriftAudit::new(&HashMap::new(), &cfg);
        let prompt = audit.resolve_prompt().expect("default prompt resolves");
        assert!(prompt.contains("findings"), "expected default prompt body");
        assert!(prompt.contains("openspec/specs"), "expected default prompt body");
    }

    #[test]
    fn resolve_prompt_errors_on_empty_override_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("empty.md");
        std::fs::write(&p, "   \n").unwrap();
        let mut map = HashMap::new();
        map.insert(
            DriftAudit::TYPE.into(),
            AuditSettings {
                prompt_path: Some(p),
                notify_on_clean: false,
                extra: HashMap::new(),
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = DriftAudit::new(&map, &cfg);
        let err = audit.resolve_prompt().expect_err("empty override errors");
        assert!(format!("{err:#}").contains("empty"));
    }

    #[test]
    fn resolve_prompt_reads_override_file_contents() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("override.md");
        std::fs::write(&p, "CUSTOM DRIFT PROMPT SENTINEL").unwrap();
        let mut map = HashMap::new();
        map.insert(
            DriftAudit::TYPE.into(),
            AuditSettings {
                prompt_path: Some(p),
                notify_on_clean: false,
                extra: HashMap::new(),
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = DriftAudit::new(&map, &cfg);
        let prompt = audit.resolve_prompt().expect("override resolves");
        assert!(prompt.contains("CUSTOM DRIFT PROMPT SENTINEL"));
    }

    #[tokio::test]
    async fn run_writes_full_stdout_to_audit_log() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        // Fake CLI: echoes a canned findings JSON document to stdout
        // and exits 0.
        let script = write_script(
            ws_dir.path(),
            "fake-claude.sh",
            "#!/bin/sh\ncat <<'EOF'\n{\"findings\":[{\"capability\":\"cap1\",\"requirement\":\"req1\",\"severity\":\"high\",\"code_anchors\":[\"src/foo.rs:1\"],\"divergence\":\"detail\"}]}\nEOF\nexit 0\n",
        );

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::Reported(findings) => {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].severity, Severity::High);
                assert!(findings[0].subject.contains("cap1"));
            }
            other => panic!("expected Reported, got {other:?}"),
        }
        let log = std::fs::read_to_string(&log_path).expect("log readable");
        assert!(log.contains("drift_audit_stdout"), "log missing stdout section: {log}");
        assert!(log.contains("\"findings\""), "log missing canned JSON: {log}");
        assert!(log.contains("drift_audit_prompt"), "log missing prompt section: {log}");
        assert!(log.contains("drift_audit_preamble"), "log missing preamble section: {log}");
        // Cleanup global audit log dir.
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn sandbox_settings_file_cleaned_up_after_run() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let script = write_script(
            ws_dir.path(),
            "fake-claude.sh",
            "#!/bin/sh\necho '{\"findings\":[]}'\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
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

    #[tokio::test]
    async fn run_returns_err_on_nonzero_exit() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let script = write_script(
            ws_dir.path(),
            "fail.sh",
            "#!/bin/sh\necho 'partial' \necho 'boom' >&2\nexit 7\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let err = audit.run(&mut ctx).await.expect_err("nonzero exit errors");
        let msg = format!("{err:#}");
        assert!(msg.contains("exit"), "error must mention exit code: {msg}");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_returns_err_on_malformed_stdout() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let script = write_script(
            ws_dir.path(),
            "bad.sh",
            "#!/bin/sh\necho 'this is not the JSON you are looking for'\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
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

    #[test]
    fn audit_type_and_policy_are_fixed() {
        let cfg = executor_cfg("/bin/true");
        let audit = DriftAudit::new(&HashMap::new(), &cfg);
        assert_eq!(audit.audit_type(), "drift_audit");
        assert!(audit.requires_head_change());
        assert!(matches!(audit.write_policy(), WritePolicy::None));
    }
}
