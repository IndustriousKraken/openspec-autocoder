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

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::path::PathBuf;

use super::specs_writing::{SpecsWritingAuditParams, run_specs_writing_audit};
use super::{Audit, AuditContext, AuditOutcome, WritePolicy};
use crate::config::{AuditSettings, ExecutorConfig, ResolvedSandbox};

/// Built-in default prompt, embedded at compile time so the binary
/// runs without requiring `prompts/` on disk.
const DEFAULT_PROMPT: &str = include_str!("../../../prompts/security-bug-audit.md");

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

    /// Resolve the prompt (override or embedded default) and substitute
    /// `{{MAX_PROPOSALS}}` with the configured cap so the agent knows
    /// its budget for this run.
    pub(crate) fn resolve_prompt(&self) -> Result<String> {
        let raw = match &self.settings.prompt_path {
            Some(path) => {
                let body = std::fs::read_to_string(path).with_context(|| {
                    format!(
                        "reading security-bug-audit prompt override at {}",
                        path.display()
                    )
                })?;
                if body.trim().is_empty() {
                    return Err(anyhow!(
                        "security-bug-audit prompt override at {} is empty",
                        path.display()
                    ));
                }
                body
            }
            None => DEFAULT_PROMPT.to_string(),
        };
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
        let prompt = self.resolve_prompt()?;
        let prompt_source = self
            .settings
            .prompt_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<embedded default>".to_string());
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
            command: command.to_string(),
            timeout_secs: 30,
            sandbox: None,
            implementer_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_revisions_per_pr: 5,
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
        AuditLogWriter::open(workspace, SecurityBugAudit::TYPE)
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
    }

    #[test]
    fn prompt_substitution_includes_max_proposals() {
        let cfg = executor_cfg("/bin/true");
        let audit =
            SecurityBugAudit::new(&HashMap::new(), &cfg).with_max_proposals(4);
        let prompt = audit.resolve_prompt().expect("default prompt resolves");
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
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = SecurityBugAudit::new(&settings_map, &cfg);
        assert_eq!(audit.max_proposals_per_run, 6);

        let bare = SecurityBugAudit::new(&HashMap::new(), &cfg);
        assert_eq!(bare.max_proposals_per_run, DEFAULT_MAX_PROPOSALS_PER_RUN);
    }

    /// The prompt must contain the confidence-filter and out-of-scope
    /// instructions: without them, the audit's noise floor would be
    /// unacceptable. Asserts on the embedded default so accidental
    /// prompt drift breaks CI rather than the operator's mailbox.
    #[test]
    fn low_confidence_finding_filtering_explicit_in_prompt() {
        let cfg = executor_cfg("/bin/true");
        let audit = SecurityBugAudit::new(&HashMap::new(), &cfg);
        let prompt = audit.resolve_prompt().expect("default prompt resolves");
        assert!(
            prompt.contains("Only emit a change for findings you are highly confident about"),
            "prompt must instruct high-confidence filter: {prompt}"
        );
        assert!(
            prompt.contains("false positive wastes downstream implementer work"),
            "prompt must explain WHY low-confidence findings are harmful: {prompt}"
        );
        assert!(
            prompt.contains("When in doubt, DON'T emit"),
            "prompt must explicitly tell the agent to drop uncertain findings: {prompt}"
        );
        assert!(
            prompt.contains("Do NOT propose stylistic"),
            "prompt must forbid stylistic 'best-practice' suggestions: {prompt}"
        );
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
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten(names) => {
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
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten(names) => {
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
        };
        let log_path = ctx.log_writer.path().to_path_buf();

        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten(names) => {
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
}
