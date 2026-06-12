//! Security & bug audit. Invokes the wrapped agent CLI with an
//! `OpenSpecOnly` sandbox and a security-and-bug-detection prompt.
//! The agent surveys the source tree for high-confidence security
//! issues and likely bugs, then writes up to `max_proposals_per_run`
//! new OpenSpec change directories under `openspec/changes/` proposing
//! a fix per finding. The shared
//! [`super::specs_writing::run_specs_writing_audit`] helper handles
//! the sandbox, snapshot diff, validation, over-cap pruning, and
//! commit; this module's only responsibilities are reading settings,
//! resolving the prompt, and delegating.
//!
//! `requires_head_change = true` — re-surveying the same SHA finds
//! the same issues. `WritePolicy::OpenSpecOnly` — the agent may write
//! under `openspec/changes/` but nowhere else; the framework reverts
//! anything else.
//!
//! Naming convention: proposed change directories are prefixed with
//! `fix-` for bug fixes and `secure-` for security hardening so
//! operators can recognize audit-produced changes at a glance.

use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use super::specs_writing::{ALLOWED_TOOLS, SpecsWritingAuditParams, run_specs_writing_audit};
use super::{Audit, AuditContext, AuditOutcome, WritePolicy};
use crate::config::{AuditSettings, ExecutorConfig, ResolvedSandbox};
use crate::prompts::{PromptId, PromptLoader};

/// Placeholder substituted into the prompt with the per-run cap.
const MAX_PROPOSALS_PLACEHOLDER: &str = "{{MAX_PROPOSALS}}";

/// Default cap on the number of change directories the audit will
/// commit per run. Operators override via
/// `audits.settings.security_bug_audit.extra.max_proposals_per_run`.
pub const DEFAULT_MAX_PROPOSALS_PER_RUN: u32 = 2;

const SETTINGS_KEY_MAX_PROPOSALS: &str = "max_proposals_per_run";

pub struct SecurityBugAudit {
    pub settings: AuditSettings,
    pub max_proposals_per_run: u32,
    pub executor_command: String,
    pub executor_timeout_secs: u64,
    sandbox: ResolvedSandbox,
    /// Override for the directory the per-invocation sandbox settings
    /// file is written to. `None` (production) means
    /// `std::env::temp_dir()`. Tests pass a per-test TempDir.
    settings_dir: Option<PathBuf>,
    /// Override for the `openspec` validation binary. `None` (prod)
    /// means `openspec`. Tests point at a shell script so the audit
    /// can be exercised without the real CLI on PATH.
    openspec_command: String,
}

impl SecurityBugAudit {
    pub const TYPE: &'static str = "security_bug_audit";

    pub fn new(
        audit_settings: &std::collections::HashMap<String, AuditSettings>,
        executor: &ExecutorConfig,
    ) -> Self {
        let settings = audit_settings
            .get(Self::TYPE)
            .cloned()
            .unwrap_or_default();
        let max_proposals_per_run = settings
            .extra
            .get(SETTINGS_KEY_MAX_PROPOSALS)
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(DEFAULT_MAX_PROPOSALS_PER_RUN);
        let sandbox = ResolvedSandbox::resolve(executor.sandbox.as_ref());
        Self {
            settings,
            max_proposals_per_run,
            executor_command: executor.command.clone(),
            executor_timeout_secs: executor.timeout_secs,
            sandbox,
            settings_dir: None,
            openspec_command: "openspec".to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_settings_dir(mut self, dir: PathBuf) -> Self {
        self.settings_dir = Some(dir);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_openspec_command(mut self, command: String) -> Self {
        self.openspec_command = command;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_max_proposals(mut self, n: u32) -> Self {
        self.max_proposals_per_run = n;
        self
    }

    /// Resolve the prompt via the uniform [`PromptLoader`] (a24) AND
    /// substitute `{{MAX_PROPOSALS}}` with the configured cap so the
    /// agent knows its budget for this run.
    pub(crate) fn resolve_prompt(&self, workspace: Option<&Path>) -> Result<String> {
        let raw = PromptLoader::load(
            PromptId::AuditSecurityBug,
            self.settings.prompt_path.as_deref(),
            None,
            workspace,
        );
        Ok(raw.replace(
            MAX_PROPOSALS_PLACEHOLDER,
            &self.max_proposals_per_run.to_string(),
        ))
    }
}

#[async_trait]
impl Audit for SecurityBugAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn description(&self) -> &'static str {
        "proposes fixes for likely security bugs"
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::OpenSpecOnly
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        let prompt = self.resolve_prompt(Some(ctx.workspace))?;
        let prompt_source = self
            .settings
            .prompt_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<embedded default>".to_string());
        // audit-model-selection: route to the configured model (if any).
        let model = super::audit_resolved_model(&self.settings);
        run_specs_writing_audit(
            SpecsWritingAuditParams {
                audit_type: Self::TYPE,
                prompt: &prompt,
                max_proposals: self.max_proposals_per_run,
                executor_command: &self.executor_command,
                executor_timeout_secs: self.executor_timeout_secs,
                sandbox: &self.sandbox,
                settings_dir: self.settings_dir.as_deref(),
                openspec_command: &self.openspec_command,
                prompt_source: &prompt_source,
                commit_subject: "security-bug proposals",
                allowed_tools: ALLOWED_TOOLS,
                include_autocoder_tools: false,
                model: model.as_ref(),
            },
            ctx,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audits::AuditLogWriter;
    use crate::config::{ExecutorKind, RepositoryConfig};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn executor_cfg(command: &str) -> ExecutorConfig {
        ExecutorConfig {
            kind: ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: command.to_string(),
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
        }
    }

    fn fixture_repo() -> RepositoryConfig {
        RepositoryConfig { forge: None,
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
            sandbox: None,
        }
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn init_workspace_with(existing_changes: &[&str]) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().to_path_buf();
        let st = StdCommand::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success());
        for arg in [
            &["config", "user.email", "t@e.com"],
            &["config", "user.name", "t"],
        ] {
            let st = StdCommand::new("git")
                .args(arg.iter())
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success());
        }
        std::fs::write(ws.join("README.md"), "hi\n").unwrap();
        let st = StdCommand::new("git")
            .args(["add", "README.md"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success());
        let st = StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success());
        for name in existing_changes {
            let p = ws.join("openspec/changes").join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("proposal.md"), "# pre-existing\n").unwrap();
        }
        (dir, ws)
    }

    fn make_log_writer(workspace: &Path) -> AuditLogWriter {
        let (td, paths) = crate::testing::test_daemon_paths();
        std::mem::forget(td);
        AuditLogWriter::open(&paths, workspace, SecurityBugAudit::TYPE)
            .expect("audit log open succeeds")
    }

    // ------------- Settings / prompt resolution -------------

    #[test]
    fn audit_type_and_policy_are_fixed() {
        let cfg = executor_cfg("/bin/true");
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg);
        assert_eq!(audit.audit_type(), "security_bug_audit");
        assert!(audit.requires_head_change());
        assert!(matches!(audit.write_policy(), WritePolicy::OpenSpecOnly));
        // The audit's whole job is to write openspec/changes/ proposals, so it
        // MUST run with a writable workspace. Regression: a read-only mount
        // silently discarded a real finding as "0 proposals".
        assert!(audit.write_policy().workspace_writable());
    }

    #[test]
    fn prompt_substitution_includes_max_proposals() {
        let cfg = executor_cfg("/bin/true");
        let audit =
            SecurityBugAudit::new(&HashMap::new(), &cfg).with_max_proposals(4);
        let prompt = audit.resolve_prompt(None).expect("default prompt resolves");
        assert!(
            !prompt.contains(MAX_PROPOSALS_PLACEHOLDER),
            "placeholder must be substituted: still found `{}`",
            MAX_PROPOSALS_PLACEHOLDER
        );
        assert!(
            prompt.contains("MAX_PROPOSALS: 4"),
            "substituted value must appear in the prompt: {prompt}"
        );
    }

    #[test]
    fn new_reads_max_proposals_from_extra_and_defaults_otherwise() {
        let mut extra = HashMap::new();
        extra.insert(
            SETTINGS_KEY_MAX_PROPOSALS.into(),
            serde_yml::Value::Number(serde_yml::Number::from(6_u64)),
        );
        let mut settings_map = HashMap::new();
        settings_map.insert(
            SecurityBugAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
                ..Default::default()
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = SecurityBugAudit::new(&settings_map, &cfg);
        assert_eq!(audit.max_proposals_per_run, 6);

        let bare = SecurityBugAudit::new(&HashMap::new(), &cfg);
        assert_eq!(bare.max_proposals_per_run, DEFAULT_MAX_PROPOSALS_PER_RUN);
    }

    // ------------- Full-run scenarios -------------

    #[tokio::test]
    async fn change_with_fix_prefix_validates_and_commits() {
        let (_t, ws) = init_workspace_with(&[]);
        let new = ws
            .join("openspec/changes/fix-off-by-one-in-queue-walker")
            .display()
            .to_string();
        let script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new}'\necho '# proposal' > '{new}/proposal.md'\nexit 0\n"
            ),
        );
        let ok_validator = write_script(&ws, "fake-openspec-ok.sh", "#!/bin/sh\nexit 0\n");

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten { changes: names, .. } => {
                assert_eq!(
                    names,
                    vec!["fix-off-by-one-in-queue-walker".to_string()]
                );
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }
        // Commit message names the audit and the count.
        let log = StdCommand::new("git")
            .args(["log", "--oneline", "-n", "5"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&log.stdout);
        assert!(
            log_str.contains("security-bug proposals")
                && log_str.contains("1 change(s)"),
            "commit message must reflect the validated count: {log_str}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn change_with_secure_prefix_validates_and_commits() {
        let (_t, ws) = init_workspace_with(&[]);
        let new = ws
            .join("openspec/changes/secure-sanitize-user-paths")
            .display()
            .to_string();
        let script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new}'\necho '# proposal' > '{new}/proposal.md'\nexit 0\n"
            ),
        );
        let ok_validator = write_script(&ws, "fake-openspec-ok.sh", "#!/bin/sh\nexit 0\n");

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten { changes: names, .. } => {
                assert_eq!(
                    names,
                    vec!["secure-sanitize-user-paths".to_string()]
                );
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }
        let log = StdCommand::new("git")
            .args(["log", "--oneline", "-n", "5"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&log.stdout);
        assert!(
            log_str.contains("security-bug proposals")
                && log_str.contains("1 change(s)"),
            "commit message must reflect the validated count: {log_str}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn oversized_run_truncated_to_cap_with_warn_log() {
        // Defense-in-depth: if the agent ignores its cap and produces
        // more change dirs than `max_proposals_per_run`, the helper
        // must truncate (deterministic by sorted name) and log the
        // dropped names.
        let (_t, ws) = init_workspace_with(&[]);
        let c1 = ws
            .join("openspec/changes/fix-a")
            .display()
            .to_string();
        let c2 = ws
            .join("openspec/changes/secure-b")
            .display()
            .to_string();
        let c3 = ws
            .join("openspec/changes/secure-c")
            .display()
            .to_string();
        let script_body = format!(
            "#!/bin/sh\nmkdir -p '{c1}' '{c2}' '{c3}'\necho '# a' > '{c1}/proposal.md'\necho '# b' > '{c2}/proposal.md'\necho '# c' > '{c3}/proposal.md'\nexit 0\n"
        );
        let script = write_script(&ws, "fake-claude.sh", &script_body);
        let ok_validator = write_script(&ws, "fake-openspec-ok.sh", "#!/bin/sh\nexit 0\n");

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string())
            .with_max_proposals(2);
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten { changes: names, .. } => {
                assert_eq!(names.len(), 2, "cap must hold: got {names:?}");
                // Deterministic: sorted names → fix-a, secure-b kept;
                // secure-c dropped.
                assert_eq!(
                    names,
                    vec!["fix-a".to_string(), "secure-b".to_string()]
                );
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }
        // The dropped change dir must not survive.
        assert!(!ws.join("openspec/changes/secure-c").exists());
        // The audit log captured the WARN-equivalent: a section naming
        // the dropped changes.
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains("security_bug_audit_dropped_over_cap"),
            "log must contain the dropped-over-cap section: {log}"
        );
        assert!(
            log.contains("secure-c"),
            "log must name the dropped change: {log}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    // ------------- Retry-on-validation-failure scenarios -------------

    /// All-invalid run with `max_validation_retries: 0` returns
    /// `ValidationExhausted`, removes the dirs, and (when chatops is
    /// configured) posts the `❌` notification. Mirrors the spec's
    /// "Retry budget exhausted" scenario for the specs-writing case.
    #[tokio::test]
    async fn all_invalid_returns_validation_exhausted_and_discards() {
        let (_t, ws) = init_workspace_with(&[]);
        let new = ws
            .join("openspec/changes/fix-bogus")
            .display()
            .to_string();
        let script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new}'\necho '# bogus' > '{new}/proposal.md'\nexit 0\n"
            ),
        );
        let bad_validator = write_script(
            &ws,
            "fake-openspec-fail.sh",
            "#!/bin/sh\necho 'MODIFIED header not found' >&2\nexit 2\n",
        );

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(bad_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::ValidationExhausted {
                audit_type,
                retries_attempted,
                final_error,
            } => {
                assert_eq!(audit_type, "security_bug_audit");
                assert_eq!(retries_attempted, 0);
                assert!(
                    final_error.contains("fix-bogus"),
                    "final_error names failed change: {final_error}"
                );
            }
            other => panic!("expected ValidationExhausted, got {other:?}"),
        }
        // No commit must have been made and the change dir must be gone.
        assert!(
            !ws.join("openspec/changes/fix-bogus").exists(),
            "invalid change dir must be deleted in the exhausted path"
        );
        let head = StdCommand::new("git")
            .args(["log", "--oneline", "-n", "5"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&head.stdout);
        assert!(
            !log_str.contains("security-bug proposals"),
            "no commit must reference the audit on exhaustion: {log_str}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// `max_validation_retries: 1` with a validator that fails the
    /// first attempt and passes the second → the audit commits the
    /// retried change and returns `SpecsWritten { retries_used: 1 }`.
    /// Mirrors the spec's "Validation passes after one retry"
    /// scenario.
    #[tokio::test]
    async fn invalid_then_valid_with_one_retry_succeeds_with_retries_used_one() {
        let (_t, ws) = init_workspace_with(&[]);
        let new = ws
            .join("openspec/changes/fix-retry-me")
            .display()
            .to_string();
        let script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new}'\necho '# proposal' > '{new}/proposal.md'\nexit 0\n"
            ),
        );
        // Validator: fails first invocation, passes second.
        let toggle = ws.join(".validator-toggle");
        let validator = write_script(
            &ws,
            "fake-openspec-toggle.sh",
            &format!(
                "#!/bin/sh\nMARK='{}'\nif [ ! -f \"$MARK\" ]; then\n  touch \"$MARK\"\n  echo 'missing SHALL keyword' >&2\n  exit 2\nfi\nexit 0\n",
                toggle.display()
            ),
        );

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(&ws),
            max_validation_retries: 1,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten {
                changes,
                retries_used,
            } => {
                assert_eq!(changes, vec!["fix-retry-me".to_string()]);
                assert_eq!(retries_used, 1);
            }
            other => panic!("expected SpecsWritten on retry, got {other:?}"),
        }
        // The validated change must have been committed.
        let head = StdCommand::new("git")
            .args(["log", "--oneline", "-n", "5"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&head.stdout);
        assert!(
            log_str.contains("security-bug proposals") && log_str.contains("1 change(s)"),
            "commit message must reflect the validated count: {log_str}"
        );
        // Audit log contains the addendum-bearing prompt on attempt 1.
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains("security_bug_audit_prompt_attempt_0"),
            "attempt 0 prompt section must exist: {log}"
        );
        assert!(
            log.contains("security_bug_audit_prompt_attempt_1"),
            "attempt 1 prompt section must exist after retry: {log}"
        );
        assert!(
            log.contains("Your previous response produced this proposal which failed openspec validation"),
            "retry prompt must include the documented addendum prefix: {log}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Zero new change dirs is a legitimate "no findings" — even when
    /// `max_validation_retries > 0`, the audit must NOT retry (there is
    /// nothing to validate). Empty `SpecsWritten` with `retries_used: 0`.
    #[tokio::test]
    async fn zero_change_dirs_is_no_findings_and_does_not_retry() {
        let (_t, ws) = init_workspace_with(&[]);
        // CLI produces nothing.
        let script = write_script(&ws, "fake-claude.sh", "#!/bin/sh\nexit 0\n");
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(&ws),
            // Generous retry budget; the audit MUST NOT consume it just
            // because zero change dirs were produced.
            max_validation_retries: 3,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten {
                changes,
                retries_used,
            } => {
                assert!(changes.is_empty());
                assert_eq!(retries_used, 0, "zero proposals must not consume retries");
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    // ------------- 🔍 proposal-created notification scenarios -------------
    //
    // Exercises the `a02-audit-proposal-created-notification` wiring:
    // when an LLM-driven specs-writing audit produces a valid proposal,
    // a `🔍` chatops notification fires AFTER validation passes and
    // BEFORE the proposal is committed to git. The notification's text,
    // its retry-count parenthetical, and the no-fire-on-exhaustion +
    // chatops-absent + chatops-failure paths are all covered here.

    use super::super::test_support::{RecordingBackend, make_recording_ctx};
    use std::sync::Arc;

    fn write_valid_proposal_script(ws: &Path, slug: &str, why: &str) -> PathBuf {
        let new = ws.join("openspec/changes").join(slug).display().to_string();
        write_script(
            ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new}'\ncat > '{new}/proposal.md' <<'EOF'\n## Why\n\n{why}\n\n## What Changes\n- thing\nEOF\nexit 0\n"
            ),
        )
    }

    #[tokio::test]
    async fn proposal_created_notification_fires_on_first_attempt_success() {
        let (_t, ws) = init_workspace_with(&[]);
        let why = "Operator must know that the security audit created this proposal";
        let script = write_valid_proposal_script(&ws, "secure-fire-on-success", why);
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");

        let backend = Arc::new(RecordingBackend::new());
        let chatops = make_recording_ctx(backend.clone());

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: Some(&chatops),
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten { changes, retries_used } => {
                assert_eq!(changes, vec!["secure-fire-on-success".to_string()]);
                assert_eq!(retries_used, 0);
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }

        let calls = backend.calls();
        assert_eq!(
            calls.len(),
            1,
            "exactly one 🔍 notification per validated proposal: {calls:?}"
        );
        let text = &calls[0].text;
        assert!(text.starts_with('🔍'), "documented glyph: {text}");
        assert!(text.contains("security_bug_audit"), "audit type: {text}");
        assert!(text.contains("`secure-fire-on-success`"), "slug: {text}");
        assert!(text.contains(why), "why excerpt: {text}");
        assert!(
            !text.contains("validated on retry"),
            "first-attempt success must omit retry parenthetical: {text}"
        );

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn proposal_created_notification_includes_retry_clause_after_retry() {
        let (_t, ws) = init_workspace_with(&[]);
        let why = "Retried because the first attempt was rejected";
        let script =
            write_valid_proposal_script(&ws, "secure-after-retry", why);
        // Validator: fails first invocation, passes second.
        let toggle = ws.join(".validator-toggle-retry-test");
        let validator = write_script(
            &ws,
            "fake-openspec-toggle-retry.sh",
            &format!(
                "#!/bin/sh\nMARK='{}'\nif [ ! -f \"$MARK\" ]; then\n  touch \"$MARK\"\n  echo 'missing SHALL keyword' >&2\n  exit 2\nfi\nexit 0\n",
                toggle.display()
            ),
        );

        let backend = Arc::new(RecordingBackend::new());
        let chatops = make_recording_ctx(backend.clone());

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: Some(&chatops),
            log_writer: make_log_writer(&ws),
            max_validation_retries: 2,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        assert!(
            matches!(outcome, AuditOutcome::SpecsWritten { retries_used: 1, .. }),
            "expected SpecsWritten with one retry, got {outcome:?}"
        );

        let calls = backend.calls();
        assert_eq!(calls.len(), 1, "one 🔍 per validated change: {calls:?}");
        let text = &calls[0].text;
        assert!(
            text.contains("(validated on retry 1 of 2)"),
            "retry parenthetical must reach the channel verbatim: {text}"
        );

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn validation_exhausted_does_not_fire_proposal_created_notification() {
        let (_t, ws) = init_workspace_with(&[]);
        let script =
            write_valid_proposal_script(&ws, "secure-never-valid", "ignored");
        let bad_validator = write_script(
            &ws,
            "always-fail.sh",
            "#!/bin/sh\necho 'MODIFIED header not found' >&2\nexit 2\n",
        );

        let backend = Arc::new(RecordingBackend::new());
        let chatops = make_recording_ctx(backend.clone());

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(bad_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: Some(&chatops),
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        assert!(
            matches!(outcome, AuditOutcome::ValidationExhausted { .. }),
            "expected ValidationExhausted, got {outcome:?}"
        );

        let calls = backend.calls();
        // The `❌ validation-exhausted` notification still fires, but
        // the `🔍 created proposal` notification must NOT — it is
        // strictly the success-path counterpart.
        let any_proposal_created =
            calls.iter().any(|c| c.text.starts_with('🔍'));
        assert!(
            !any_proposal_created,
            "🔍 created-proposal notification must NOT fire on exhaustion; calls: {calls:?}"
        );
        let exhausted_fired = calls.iter().any(|c| c.text.starts_with('❌'));
        assert!(
            exhausted_fired,
            "❌ validation-exhausted notification SHOULD still fire: {calls:?}"
        );

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn proposal_created_notification_fires_before_audit_commit() {
        // Order verification: the chatops backend snapshots the
        // workspace HEAD at the moment `post_notification` is called.
        // If the snapshot matches the pre-audit HEAD, the notification
        // fired before the audit's `git commit` ran — which is the
        // ordering the spec mandates.
        let (_t, ws) = init_workspace_with(&[]);
        let pre_audit_head = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let pre_audit_head = String::from_utf8_lossy(&pre_audit_head.stdout)
            .trim()
            .to_string();

        let why = "Ordering matters: 🔍 must precede the audit commit";
        let script = write_valid_proposal_script(&ws, "secure-ordering", why);
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");

        let backend =
            Arc::new(RecordingBackend::new().with_workspace(ws.clone()));
        let chatops = make_recording_ctx(backend.clone());

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: Some(&chatops),
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        assert!(matches!(outcome, AuditOutcome::SpecsWritten { .. }));

        // After the audit completes, HEAD has moved — the audit committed.
        let post_audit_head = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let post_audit_head = String::from_utf8_lossy(&post_audit_head.stdout)
            .trim()
            .to_string();
        assert_ne!(
            pre_audit_head, post_audit_head,
            "audit must have advanced HEAD via its `git commit` step"
        );

        let calls = backend.calls();
        assert_eq!(calls.len(), 1, "one 🔍 per validated change");
        let snapshot = calls[0]
            .head_at_post
            .as_deref()
            .expect("recording backend captured HEAD at post time");
        assert_eq!(
            snapshot, pre_audit_head,
            "the 🔍 notification fired BEFORE the audit commit (HEAD at post must match pre-audit HEAD)"
        );

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn proposal_created_chatops_error_does_not_break_audit() {
        let (_t, ws) = init_workspace_with(&[]);
        let script = write_valid_proposal_script(
            &ws,
            "secure-chatops-down",
            "Channel down; audit must commit anyway",
        );
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");

        let backend = Arc::new(RecordingBackend::failing("simulated down"));
        let chatops = make_recording_ctx(backend);

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: Some(&chatops),
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        assert!(
            matches!(outcome, AuditOutcome::SpecsWritten { .. }),
            "audit success outcome must be unaffected by a failed chatops post: {outcome:?}"
        );
        // The proposal commit must have landed regardless.
        let head_log = StdCommand::new("git")
            .args(["log", "--oneline", "-n", "5"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&head_log.stdout);
        assert!(
            log_str.contains("security-bug proposals"),
            "proposal commit must land even when chatops post fails: {log_str}"
        );

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn proposal_created_silent_when_chatops_absent() {
        let (_t, ws) = init_workspace_with(&[]);
        let script = write_valid_proposal_script(
            &ws,
            "secure-no-chatops",
            "No chatops configured; audit still commits",
        );
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &ws,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(&ws),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        assert!(
            matches!(outcome, AuditOutcome::SpecsWritten { .. }),
            "audit success outcome is unaffected by absent chatops: {outcome:?}"
        );
        let head_log = StdCommand::new("git")
            .args(["log", "--oneline", "-n", "5"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&head_log.stdout);
        assert!(
            log_str.contains("security-bug proposals"),
            "proposal commit must land without chatops: {log_str}"
        );

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }
}
