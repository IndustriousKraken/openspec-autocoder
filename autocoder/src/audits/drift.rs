//! Drift audit. Invokes the wrapped agent CLI (typically `claude`) with
//! a read-only sandbox (`Read`, `Glob`, `Grep`, `Bash`) plus the
//! `submit_findings` MCP tool (a57) and a drift-detection prompt. The
//! agent returns its findings by calling `submit_findings`; after the
//! subprocess exits the daemon consumes the stored submission, deserializes
//! it into [`Finding`]s, and returns `AuditOutcome::Reported`. A run that
//! ends with no stored submission is an audit failure.
//!
//! `requires_head_change = true` — drift can only emerge with code or
//! spec changes; rerunning without a HEAD shift wastes CLI invocations.
//! `WritePolicy::None` — the audit is strictly advisory; the operator
//! decides whether each finding becomes a code fix, a spec fix, or is
//! dismissed.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    Audit, AuditContext, AuditLogWriter, AuditOutcome, Finding, Severity, WritePolicy,
    workspace_is_valid, workspace_unavailable_outcome,
};
use crate::config::{AuditSettings, ExecutorConfig, ResolvedSandbox};
use crate::prompts::{PromptId, PromptLoader};

/// Tools the drift agent may call. Excludes `Write` and `Edit` so the
/// sandbox blocks workspace modifications outright; the audit-run log
/// captures the agent's stdout for forensic review.
const ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Bash"];

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
    /// Test-only injected `submit_findings` submission (a57). `Some(Some(p))`
    /// stands in for a recorded payload; `Some(None)` simulates "agent
    /// never submitted"; `None` (default) uses the real control-socket
    /// `consume_submission` path.
    #[cfg(test)]
    test_submission: Option<Option<serde_json::Value>>,
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
            #[cfg(test)]
            test_submission: None,
        }
    }

    /// Test-only override: write the sandbox settings file to `dir`
    /// instead of the OS temp dir.
    #[cfg(test)]
    pub(crate) fn with_settings_dir(mut self, dir: PathBuf) -> Self {
        self.settings_dir = Some(dir);
        self
    }

    /// Test-only override: stand in for the `submit_findings` submission
    /// the agent would have recorded (a57), bypassing the control socket.
    /// `Some(payload)` → the audit consumes that payload; `None` → the
    /// audit observes "no submission" (the failure path).
    #[cfg(test)]
    pub(crate) fn with_submission(mut self, submission: Option<serde_json::Value>) -> Self {
        self.test_submission = Some(submission);
        self
    }

    /// Drain the agent's `submit_findings` submission (a57). In tests an
    /// injected override short-circuits the control socket; in production
    /// this relays `consume_submission` to the daemon.
    async fn consume_submission(&self, workspace: &Path) -> Option<serde_json::Value> {
        #[cfg(test)]
        if let Some(over) = &self.test_submission {
            return over.clone();
        }
        super::try_consume_submission(workspace, Self::TYPE).await
    }

    /// Test-only override: replace the wrapped CLI command (e.g. point
    /// at a fixture shell script that produces canned stdout).
    #[cfg(test)]
    pub(crate) fn with_command(mut self, command: String) -> Self {
        self.executor_command = command;
        self
    }

    /// Resolve the drift prompt via the uniform [`PromptLoader`]. The
    /// `settings.prompt_path` field is the audit's per-workspace nested
    /// override (`audits.settings.drift_audit.prompt_path`). When set
    /// AND the file exists, that content wins; otherwise the embedded
    /// default applies. A missing/empty override path produces a
    /// one-shot WARN AND falls through to the embedded default.
    fn resolve_prompt(&self, workspace: Option<&Path>) -> Result<String> {
        Ok(PromptLoader::load(
            PromptId::AuditDrift,
            self.settings.prompt_path.as_deref(),
            None,
            workspace,
        ))
    }
}

#[async_trait]
impl Audit for DriftAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn description(&self) -> &'static str {
        "spec ↔ code drift detection (warns when reality outgrows the spec)"
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::None
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        // Workspace-validity gate (see `audits-require-valid-workspace`).
        // Skip immediately if the workspace is missing or has no
        // `.git/` — no LLM call, no file IO, no `create_dir_all`.
        if !workspace_is_valid(ctx.workspace) {
            return Ok(workspace_unavailable_outcome(
                Self::TYPE,
                ctx.workspace,
                &ctx.repo.url,
            ));
        }

        let prompt = self.resolve_prompt(Some(ctx.workspace))?;

        // Force the allowed_tools list per the spec; everything else
        // (deny patterns) comes from the executor's resolved sandbox so
        // operators retain a single place to tune Bash/Read denies.
        let mut sandbox = self.sandbox.clone();
        sandbox.allowed_tools = ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect();

        let _ = ctx.log_writer.write_section(
            "drift_audit_preamble",
            &format!(
                "executor_command: {}\ntimeout_secs: {}\nprompt_source: {}\nallowed_tools: {}",
                self.executor_command,
                self.executor_timeout_secs,
                self.settings
                    .prompt_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<embedded default>".to_string()),
                sandbox.allowed_tools.join(","),
            ),
        );
        let _ = ctx.log_writer.write_section("drift_audit_prompt", &prompt);

        // a57: run WITH MCP enabled so the agent returns findings via the
        // `submit_findings` tool. The findings come from the consumed
        // submission below — NOT from stdout (the stdout-JSON path is
        // retired for this audit).
        let outcome = super::run_audit_cli_with_submit(
            &self.executor_command,
            &sandbox,
            ctx.workspace,
            &prompt,
            Duration::from_secs(self.executor_timeout_secs),
            self.settings_dir.as_deref(),
            Self::TYPE,
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

        if let Some(err) = outcome_to_terminal_err(
            &outcome,
            &mut ctx.log_writer,
            "drift_audit",
            self.executor_timeout_secs,
        ) {
            return Err(err);
        }

        // Drain the agent's `submit_findings` submission. No stored
        // submission is an audit failure (the new transport's restatement
        // of the old "malformed stdout" failure): state is not updated, a
        // chatops audit-failure alert posts, the next iteration retries.
        let Some(payload) = self.consume_submission(ctx.workspace).await else {
            let _ = ctx.log_writer.write_section(
                "drift_audit_outcome",
                "kind: Err\nreason: no submit_findings submission recorded",
            );
            return Err(anyhow!(
                "drift_audit: agent exited with no submit_findings submission; stderr excerpt: {}",
                excerpt(&outcome.stderr)
            ));
        };
        let findings = match payload_to_findings(&payload) {
            Ok(f) => f,
            Err(e) => {
                let _ = ctx
                    .log_writer
                    .write_section("drift_audit_outcome", &format!("kind: Err\nreason: {e}"));
                return Err(anyhow!("drift_audit: {e}"));
            }
        };
        let _ = ctx.log_writer.write_section(
            "drift_audit_outcome",
            &format!("kind: Reported\nfindings_count: {}", findings.len()),
        );
        // This audit produces advisory `Reported` findings — it does NOT
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

/// Deserialize a `submit_findings` payload (`{ "findings": [...] }`) into
/// [`Finding`]s (a57). This is the daemon-side consumer of the agent's
/// validated submission — the same shape `parse_findings` produced from
/// stdout before the transport moved to MCP. Returns `Err(reason)` (a
/// correction-suitable string) on a malformed payload; the reason is
/// surfaced to the agent by `record_submission`'s registered validator,
/// which is exactly this function with its `Ok` value discarded.
pub(crate) fn payload_to_findings(
    payload: &serde_json::Value,
) -> std::result::Result<Vec<Finding>, String> {
    let arr = payload
        .get("findings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "drift_audit: submission missing top-level `findings` array".to_string()
        })?;
    let mut findings = Vec::with_capacity(arr.len());
    for (idx, raw) in arr.iter().enumerate() {
        let entry: RawFinding = serde_json::from_value(raw.clone()).map_err(|e| {
            format!("drift_audit: findings[{idx}] does not match the expected shape: {e}")
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
            // no-url: pure severity parser, no AuditContext in scope
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

/// Pure transformation: given an [`crate::agentic_run::AgenticRunOutcome`],
/// return Some(error) if the outcome is terminal (timed out OR non-zero
/// exit). Returns None when the caller should continue processing (parse
/// stdout into findings). Mirrors
/// `architecture_consultative::outcome_to_terminal_err`.
fn outcome_to_terminal_err(
    outcome: &crate::agentic_run::AgenticRunOutcome,
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
            max_revise_triggers_per_pr: 10,
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
        AuditLogWriter::open(&paths, workspace, "drift_audit").expect("log writer opens")
    }

    #[test]
    fn payload_round_trips_to_findings() {
        let payload = serde_json::json!({
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
        });
        let findings = payload_to_findings(&payload).expect("deserializes");
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
    fn empty_findings_array_deserializes_to_no_findings() {
        let payload = serde_json::json!({"findings": []});
        let findings = payload_to_findings(&payload).expect("deserializes empty array");
        assert!(findings.is_empty());
    }

    #[test]
    fn missing_top_level_findings_key_returns_err() {
        let payload = serde_json::json!({"results": []});
        let err = payload_to_findings(&payload).expect_err("missing key must error");
        assert!(err.contains("findings"), "got: {err}");
    }

    #[test]
    fn findings_non_array_returns_err() {
        let payload = serde_json::json!({"findings": "not-an-array"});
        let err = payload_to_findings(&payload).expect_err("non-array must error");
        assert!(err.contains("findings"), "got: {err}");
    }

    #[test]
    fn finding_missing_required_field_returns_err() {
        // `divergence` omitted — the registered schema validator (which is
        // this deserializer) rejects it as a correctable tool error.
        let payload = serde_json::json!({
            "findings": [
                {"capability": "cap", "requirement": "req", "severity": "high"}
            ]
        });
        let err = payload_to_findings(&payload).expect_err("missing field must error");
        assert!(err.contains("findings[0]"), "got: {err}");
    }

    #[test]
    fn unknown_severity_string_maps_to_low() {
        let payload = serde_json::json!({
            "findings": [
                {
                    "capability": "cap",
                    "requirement": "req",
                    "severity": "catastrophic",
                    "code_anchors": [],
                    "divergence": "details"
                }
            ]
        });
        let findings = payload_to_findings(&payload).expect("parses unknown severity");
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
            serde_yml::Value::String("for-future-knobs".into()),
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
        let prompt = audit
            .resolve_prompt(None)
            .expect("default prompt resolves");
        assert!(prompt.contains("findings"), "expected default prompt body");
        assert!(prompt.contains("openspec/specs"), "expected default prompt body");
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
            DriftAudit::TYPE.into(),
            AuditSettings {
                prompt_path: Some(p),
                notify_on_clean: false,
                extra: HashMap::new(),
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = DriftAudit::new(&map, &cfg);
        let prompt = audit
            .resolve_prompt(None)
            .expect("empty override falls back");
        assert!(
            prompt.contains("findings"),
            "fallback must use embedded default"
        );
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
        let prompt = audit
            .resolve_prompt(None)
            .expect("override resolves");
        assert!(prompt.contains("CUSTOM DRIFT PROMPT SENTINEL"));
    }

    #[tokio::test]
    async fn run_writes_full_stdout_to_audit_log() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        // Satisfy the workspace-validity gate
        // (see `audits-require-valid-workspace`).
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        // Fake CLI: echoes a canned findings JSON document to stdout
        // and exits 0.
        let script = write_script(
            ws_dir.path(),
            "fake-claude.sh",
            "#!/bin/sh\ncat <<'EOF'\n{\"findings\":[{\"capability\":\"cap1\",\"requirement\":\"req1\",\"severity\":\"high\",\"code_anchors\":[\"src/foo.rs:1\"],\"divergence\":\"detail\"}]}\nEOF\nexit 0\n",
        );

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        // a57: findings arrive via the consumed `submit_findings` submission,
        // not stdout. The fake CLI still echoes a JSON blob so the stdout
        // log section is populated; the injected submission is the result.
        let submission = serde_json::json!({
            "findings": [
                {"capability": "cap1", "requirement": "req1", "severity": "high",
                 "code_anchors": ["src/foo.rs:1"], "divergence": "detail"}
            ]
        });
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_submission(Some(submission));
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
                assert_eq!(findings[0].severity, Severity::High);
                assert!(findings[0].subject.contains("cap1"));
                assert_eq!(retries_used, 0, "drift audit does not validate proposals");
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
        // Satisfy the workspace-validity gate
        // (see `audits-require-valid-workspace`).
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "fake-claude.sh",
            "#!/bin/sh\necho '{\"findings\":[]}'\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_submission(Some(serde_json::json!({"findings": []})));
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
        // a57 (MCP-enabled audit path): the audit writes a `.mcp.json`
        // advertising `submit_findings` DURING the run AND deletes it on
        // exit, so the working tree is clean afterward (the read-only
        // WritePolicy::None post-hoc diff check must see no stray file).
        assert!(
            !workspace.join(".mcp.json").exists(),
            "audit run must clean up the .mcp.json it wrote"
        );
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
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
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
        let err = audit.run(&mut ctx).await.expect_err("nonzero exit errors");
        let msg = format!("{err:#}");
        assert!(msg.contains("exit"), "error must mention exit code: {msg}");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Pure-data test: feed a synthesized `AgenticRunOutcome` with
    /// `timed_out: true` directly into `outcome_to_terminal_err` and
    /// assert the resulting error + log entries. No subprocess, no
    /// timer, no race — see architecture_consultative's equivalent
    /// test for the architectural rationale.
    #[test]
    fn outcome_to_terminal_err_translates_timed_out_to_error() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let mut log_writer = make_log_writer(workspace);
        let log_path = log_writer.path().to_path_buf();
        let outcome = crate::agentic_run::AgenticRunOutcome {
            timed_out: true,
            exit_status: None,
            stdout: String::new(),
            stderr: "timeout".into(),
            ..Default::default()
        };
        let err = outcome_to_terminal_err(&outcome, &mut log_writer, "drift_audit", 1)
            .expect("timed_out outcome must produce Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("drift_audit"), "error must name the audit type: {msg}");
        assert!(msg.contains("timeout"), "error must mention timeout: {msg}");
        let log = std::fs::read_to_string(&log_path).expect("log readable");
        assert!(log.contains("kind: Err"), "log must record Err outcome: {log}");
        assert!(log.contains("reason: timeout"), "log must record timeout reason: {log}");
    }

    /// a57 (task 3.4): a clean exit with NO stored submission is an audit
    /// failure (`Err`); the audit-run log records the Err outcome.
    #[tokio::test]
    async fn run_returns_err_when_no_submission() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "silent.sh",
            "#!/bin/sh\necho 'I forgot to call submit_findings'\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        // `with_submission(None)` simulates "agent never submitted".
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_submission(None);
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
            .expect_err("no submission must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no submit_findings submission"),
            "error must name the missing submission: {msg}"
        );
        let log = std::fs::read_to_string(&log_path).expect("log readable");
        assert!(log.contains("kind: Err"), "log must record Err outcome: {log}");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// a57 (task 3.5): an empty `findings` submission yields a silent
    /// `Reported(vec![])` (the framework suppresses chatops unless
    /// `notify_on_clean`).
    #[tokio::test]
    async fn run_returns_reported_empty_for_empty_submission() {
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let script = write_script(
            ws_dir.path(),
            "clean.sh",
            "#!/bin/sh\nexit 0\n",
        );
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_submission(Some(serde_json::json!({"findings": []})));
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit.run(&mut ctx).await.expect("empty submission succeeds");
        match outcome {
            AuditOutcome::Reported { findings, .. } => {
                assert!(findings.is_empty(), "empty submission → no findings");
            }
            other => panic!("expected Reported(empty), got {other:?}"),
        }
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

    /// Workspace-validity gate: missing workspace → WorkspaceUnavailable
    /// with `"workspace directory does not exist"`, and the workspace
    /// path is NOT created as a side effect (no `create_dir_all`).
    #[tokio::test]
    async fn workspace_unavailable_when_path_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("never-existed");
        assert!(!workspace.exists());

        let cfg = executor_cfg("/bin/true");
        let settings_dir = TempDir::new().unwrap();
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(tmp.path()),
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
                assert_eq!(audit_type, DriftAudit::TYPE);
                assert_eq!(workspace_path, workspace);
                assert_eq!(reason, "workspace directory does not exist");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
        assert!(!workspace.exists(), "audit must not create the workspace");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Workspace-validity gate: existing workspace without `.git/` →
    /// WorkspaceUnavailable with `"workspace exists but has no .git/ subdirectory"`,
    /// and no new files or subdirectories were created.
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
        let audit = DriftAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(tmp.path()),
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
        assert_eq!(before, after);
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }
}
