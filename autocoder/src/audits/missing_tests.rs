//! Missing-tests audit. Invokes the wrapped agent CLI with an
//! `OpenSpecOnly` sandbox and a missing-tests prompt. The agent
//! surveys the source tree, identifies uncovered behavior, and writes
//! up to `max_proposals_per_run` new OpenSpec change directories under
//! `openspec/changes/` proposing tests to fill those gaps.
//!
//! The audit itself does NOT decide which gaps matter — that's the
//! agent's job. The shared [`super::specs_writing::run_specs_writing_audit`]
//! helper handles the sandbox, snapshot diff, validation, over-cap
//! pruning, and commit. This module's only responsibilities are
//! reading settings, resolving the prompt, and delegating.
//!
//! `requires_head_change = true` — there is no point re-surveying the
//! same code state for new coverage gaps. `WritePolicy::OpenSpecOnly`
//! — the agent may write under `openspec/changes/` but nowhere else;
//! a write outside that prefix triggers the framework's revert.

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
/// `audits.settings.missing_tests_audit.extra.max_proposals_per_run`.
pub const DEFAULT_MAX_PROPOSALS_PER_RUN: u32 = 2;

const SETTINGS_KEY_MAX_PROPOSALS: &str = "max_proposals_per_run";

pub struct MissingTestsAudit {
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

impl MissingTestsAudit {
    pub const TYPE: &'static str = "missing_tests_audit";

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
            PromptId::AuditMissingTests,
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
impl Audit for MissingTestsAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn description(&self) -> &'static str {
        "proposes test coverage for untested branches"
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
                commit_subject: "missing-tests proposals",
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
    use super::super::specs_writing::snapshot_change_dirs;
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

    /// Initialize a workspace with `openspec/changes/` populated by a
    /// caller-provided list of existing change directory names. Returns
    /// the TempDir handle (drop = cleanup) and the workspace path.
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
        AuditLogWriter::open(&paths, workspace, MissingTestsAudit::TYPE)
            .expect("audit log open succeeds")
    }

    // ------------- Settings / prompt resolution -------------

    #[test]
    fn new_reads_max_proposals_from_extra_and_defaults_otherwise() {
        let mut extra = HashMap::new();
        extra.insert(
            SETTINGS_KEY_MAX_PROPOSALS.into(),
            serde_yml::Value::Number(serde_yml::Number::from(5_u64)),
        );
        let mut settings_map = HashMap::new();
        settings_map.insert(
            MissingTestsAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
                ..Default::default()
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = MissingTestsAudit::new(&settings_map, &cfg);
        assert_eq!(audit.max_proposals_per_run, 5);

        let bare = MissingTestsAudit::new(&HashMap::new(), &cfg);
        assert_eq!(bare.max_proposals_per_run, DEFAULT_MAX_PROPOSALS_PER_RUN);
    }

    #[test]
    fn parses_max_proposals_substitution_into_prompt() {
        let cfg = executor_cfg("/bin/true");
        let audit =
            MissingTestsAudit::new(&HashMap::new(), &cfg).with_max_proposals(7);
        let prompt = audit.resolve_prompt(None).expect("default prompt resolves");
        assert!(
            !prompt.contains(MAX_PROPOSALS_PLACEHOLDER),
            "placeholder must be substituted: still found `{}`",
            MAX_PROPOSALS_PLACEHOLDER
        );
        assert!(
            prompt.contains("MAX_PROPOSALS: 7"),
            "substituted value must appear: {prompt}"
        );
    }

    #[test]
    fn resolve_prompt_reads_override_file_and_substitutes() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("override.md");
        std::fs::write(&p, "CUSTOM PROMPT cap={{MAX_PROPOSALS}}").unwrap();
        let mut map = HashMap::new();
        map.insert(
            MissingTestsAudit::TYPE.into(),
            AuditSettings {
                prompt_path: Some(p),
                notify_on_clean: false,
                extra: HashMap::new(),
                ..Default::default()
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = MissingTestsAudit::new(&map, &cfg).with_max_proposals(3);
        let prompt = audit.resolve_prompt(None).expect("override resolves");
        assert!(prompt.contains("CUSTOM PROMPT"));
        assert!(prompt.contains("cap=3"));
    }

    /// Empty override files now fall back to the embedded default via
    /// the uniform `PromptLoader` rather than producing a hard error
    /// (a24). The `{{MAX_PROPOSALS}}` substitution still runs against
    /// the embedded default.
    #[test]
    fn resolve_prompt_falls_back_when_override_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("empty.md");
        std::fs::write(&p, "  \n").unwrap();
        let mut map = HashMap::new();
        map.insert(
            MissingTestsAudit::TYPE.into(),
            AuditSettings {
                prompt_path: Some(p),
                notify_on_clean: false,
                extra: HashMap::new(),
                ..Default::default()
            },
        );
        let cfg = executor_cfg("/bin/true");
        let audit = MissingTestsAudit::new(&map, &cfg).with_max_proposals(4);
        let prompt = audit
            .resolve_prompt(None)
            .expect("empty override falls back");
        assert!(
            !prompt.contains(MAX_PROPOSALS_PLACEHOLDER),
            "placeholder must still be substituted on fallback"
        );
        assert!(
            prompt.contains("MAX_PROPOSALS: 4"),
            "substitution must apply to embedded fallback: {prompt}"
        );
    }

    #[test]
    fn audit_type_and_policy_are_fixed() {
        let cfg = executor_cfg("/bin/true");
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg);
        assert_eq!(audit.audit_type(), "missing_tests_audit");
        assert!(audit.requires_head_change());
        assert!(matches!(audit.write_policy(), WritePolicy::OpenSpecOnly));
        // Writes openspec/changes/ proposals — MUST run writable (a read-only
        // mount silently yields 0 proposals).
        assert!(audit.write_policy().workspace_writable());
    }

    // ------------- Pre-run snapshot -------------

    #[test]
    fn pre_run_snapshot_captures_existing_change_dirs() {
        let (_t, ws) =
            init_workspace_with(&["existing-one", "existing-two"]);
        // Add an archive dir; it should NOT count toward the snapshot.
        std::fs::create_dir_all(ws.join("openspec/changes/archive/old-thing")).unwrap();
        let snap = snapshot_change_dirs(&ws);
        assert!(snap.contains("existing-one"));
        assert!(snap.contains("existing-two"));
        assert!(
            !snap.contains("archive"),
            "archive/ must be excluded from the snapshot"
        );
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn snapshot_handles_missing_openspec_changes_dir() {
        let tmp = TempDir::new().unwrap();
        let snap = snapshot_change_dirs(tmp.path());
        assert!(snap.is_empty(), "missing dir → empty snapshot, not panic");
    }

    // ------------- Post-run new-dir detection -------------

    #[tokio::test]
    async fn post_run_detects_only_new_change_dirs() {
        let (_t, ws) = init_workspace_with(&["already-here"]);
        // Fake CLI: drop a fresh change directory under openspec/changes/.
        let new_change_dir = ws
            .join("openspec/changes/tests-new-thing")
            .display()
            .to_string();
        let script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new_change_dir}'\necho '# proposal' > '{new_change_dir}/proposal.md'\nexit 0\n"
            ),
        );
        // Fake openspec validator: always passes (exit 0).
        let ok_validator = write_script(&ws, "fake-openspec-ok.sh", "#!/bin/sh\nexit 0\n");

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
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
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten { changes: names, .. } => {
                assert_eq!(names, vec!["tests-new-thing".to_string()]);
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }
        let log_path = ctx.log_writer.path().to_path_buf();
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn validation_failure_rejects_change_and_logs_warning() {
        let (_t, ws) = init_workspace_with(&[]);
        // CLI creates one change dir.
        let new = ws
            .join("openspec/changes/tests-bad-shape")
            .display()
            .to_string();
        let script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new}'\necho '# nope' > '{new}/proposal.md'\nexit 0\n"
            ),
        );
        // Validator always fails (nonzero exit).
        let bad_validator = write_script(
            &ws,
            "fake-openspec-fail.sh",
            "#!/bin/sh\necho 'spec missing scenarios' >&2\nexit 2\n",
        );

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
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
        // With `max_validation_retries: 0`, all-invalid runs return
        // ValidationExhausted (no retry attempted, proposal discarded).
        match outcome {
            AuditOutcome::ValidationExhausted {
                audit_type,
                retries_attempted,
                final_error,
            } => {
                assert_eq!(audit_type, "missing_tests_audit");
                assert_eq!(retries_attempted, 0);
                assert!(
                    final_error.contains("tests-bad-shape"),
                    "final_error must name the failed change: {final_error}"
                );
                assert!(
                    final_error.contains("spec missing scenarios"),
                    "final_error must include the validator stderr: {final_error}"
                );
            }
            other => panic!("expected ValidationExhausted, got {other:?}"),
        }
        // The invalid change directory must have been removed.
        assert!(
            !ws.join("openspec/changes/tests-bad-shape").exists(),
            "invalid change directory must be removed so the framework's \
             post-hoc OpenSpecOnly check sees a clean tree"
        );
        // Audit log captured the validation failure.
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains("missing_tests_audit_validation_failure_tests-bad-shape_attempt_0"),
            "validation failure must be logged with attempt index: {log}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// `a42-audit-logs-carry-repo-url` capture test: the validation-
    /// rejection WARN emitted by the shared `run_specs_writing_audit`
    /// helper (used by both the missing-tests and security-bug audits)
    /// MUST carry the repository URL as a structured `url` field so a
    /// multi-repo operator can attribute the rejection. Drives the
    /// all-invalid path with a unique sentinel repo URL and asserts the
    /// captured WARN's structured fields include `url=<sentinel>`.
    ///
    /// Capture uses `tracing-test`, which installs a process-global
    /// subscriber and scopes each test's log buffer to a span named after
    /// the test fn — this is parallel-safe. (A thread-local `set_default`
    /// subscriber is NOT: a concurrent test that hits the same WARN
    /// callsite first, with no subscriber installed, leaves its `Interest`
    /// cached as disabled process-wide, so this thread's subscriber never
    /// sees the event.) `tracing-test`'s scope filter keeps only physical
    /// log lines bearing the span name, so the WARN's `url=` field — which
    /// the fmt layer renders right after the message — must stay on the
    /// SAME line as the message. The validator below therefore emits its
    /// error with `printf` (no trailing newline) so the rendered event is
    /// a single line.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn validation_failure_warn_carries_repo_url() {
        // Unique, obviously-fake URL so the captured-field assertion is
        // unambiguous (no other log line in the run names this string).
        const SENTINEL_URL: &str = "https://example.invalid/sentinel-repo-a42";

        let (_t, ws) = init_workspace_with(&[]);
        let new = ws
            .join("openspec/changes/tests-sentinel-shape")
            .display()
            .to_string();
        let script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new}'\necho '# nope' > '{new}/proposal.md'\nexit 0\n"
            ),
        );
        // Validator fails (nonzero exit) so the validation-rejection WARN
        // at `specs_writing.rs` fires for the lone change. `printf` (no
        // trailing newline) keeps the rendered event on one line so its
        // `url=` field stays within this test's captured scope.
        let bad_validator = write_script(
            &ws,
            "fake-openspec-fail.sh",
            "#!/bin/sh\nprintf 'spec missing scenarios' >&2\nexit 2\n",
        );

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(bad_validator.to_string_lossy().to_string());
        let mut repo = fixture_repo();
        repo.url = SENTINEL_URL.into();
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
            matches!(outcome, AuditOutcome::ValidationExhausted { .. }),
            "all-invalid run with no retries must exhaust validation: {outcome:?}"
        );

        // The validation-rejection WARN must have been emitted...
        assert!(
            logs_contain("rejecting agent-produced change that failed"),
            "expected the validation-rejection WARN to be captured"
        );
        // ...carrying the repo URL as a `url` structured field (the fmt
        // layer renders structured fields as `key=value`).
        assert!(
            logs_contain(&format!("url={SENTINEL_URL}")),
            "validation-rejection WARN must carry `url={SENTINEL_URL}` as a structured field"
        );

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn validation_success_commits_change_to_agent_branch() {
        let (_t, ws) = init_workspace_with(&[]);
        let new = ws
            .join("openspec/changes/tests-good-thing")
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
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
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
        let _ = audit.run(&mut ctx).await.expect("run succeeds");

        // The validated change must have been committed: the workspace
        // should now be clean (no porcelain).
        let porcelain = StdCommand::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let porcelain_str = String::from_utf8_lossy(&porcelain.stdout);
        // Strip the fake-claude.sh and fake-openspec-ok.sh untracked
        // entries before asserting cleanliness (those are test fixture
        // files, not the audit's writes).
        let interesting: Vec<&str> = porcelain_str
            .lines()
            .filter(|l| !l.contains("fake-"))
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert!(
            interesting.is_empty(),
            "validated change must be committed; leftover porcelain: {interesting:?}"
        );
        // Git log must mention the commit.
        let log = StdCommand::new("git")
            .args(["log", "--oneline", "-n", "5"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&log.stdout);
        assert!(
            log_str.contains("missing-tests proposals") && log_str.contains("1 change(s)"),
            "commit message must reflect the validated count: {log_str}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn empty_findings_no_commit_no_chatops_post() {
        let (_t, ws) = init_workspace_with(&[]);
        // CLI exits cleanly without creating any change directory.
        let script = write_script(&ws, "fake-claude.sh", "#!/bin/sh\nexit 0\n");
        let ok_validator = write_script(&ws, "fake-openspec-ok.sh", "#!/bin/sh\nexit 0\n");

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
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

        // Capture HEAD before run.
        let head_before = crate::git::rev_parse(&ws, "HEAD").unwrap();
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::SpecsWritten { changes: names, .. } => assert!(names.is_empty()),
            other => panic!("expected SpecsWritten(empty), got {other:?}"),
        }
        // HEAD must not have moved (no commit made).
        let head_after = crate::git::rev_parse(&ws, "HEAD").unwrap();
        assert_eq!(head_before, head_after, "empty findings must NOT commit");

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    // The shared timeout-to-error translation is exercised in
    // `specs_writing::outcome_tests::outcome_to_terminal_err_translates_timed_out_to_error`
    // — that pure-data test covers the missing_tests audit's path through
    // `run_specs_writing_audit` without spawning a real subprocess and
    // racing a real timer. The previous version of this test did exactly
    // that and was race-prone (the 1s timer racing the 10s sleep produced
    // platform-dependent outcomes).

    #[tokio::test]
    async fn over_cap_excess_change_dirs_are_dropped_before_commit() {
        // Defense-in-depth: even if the agent creates more change dirs
        // than the cap, the audit must NOT commit or return more than
        // `max_proposals_per_run` of them.
        let (_t, ws) = init_workspace_with(&[]);
        let c1 = ws
            .join("openspec/changes/tests-a")
            .display()
            .to_string();
        let c2 = ws
            .join("openspec/changes/tests-b")
            .display()
            .to_string();
        let c3 = ws
            .join("openspec/changes/tests-c")
            .display()
            .to_string();
        let script_body = format!(
            "#!/bin/sh\nmkdir -p '{c1}' '{c2}' '{c3}'\necho '# a' > '{c1}/proposal.md'\necho '# b' > '{c2}/proposal.md'\necho '# c' > '{c3}/proposal.md'\nexit 0\n"
        );
        let script = write_script(&ws, "fake-claude.sh", &script_body);
        let ok_validator = write_script(&ws, "fake-openspec-ok.sh", "#!/bin/sh\nexit 0\n");

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
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
                // Deterministic: sorted names → tests-a, tests-b kept.
                assert_eq!(names, vec!["tests-a".to_string(), "tests-b".to_string()]);
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }
        // The dropped change dir must not survive.
        assert!(!ws.join("openspec/changes/tests-c").exists());
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Workspace-validity gate (see `audits-require-valid-workspace`):
    /// running the spec-writing helper against a nonexistent workspace
    /// must return `WorkspaceUnavailable` with the documented reason
    /// AND must NOT create the workspace path as a side effect (the
    /// helper otherwise calls `fs::create_dir_all` for change dirs,
    /// which would recursively create the missing workspace + openspec/
    /// — the failure mode the gate exists to prevent).
    #[tokio::test]
    async fn workspace_unavailable_when_workspace_path_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("never-existed");
        assert!(!workspace.exists());

        let cfg = executor_cfg("/bin/true");
        let settings_dir = TempDir::new().unwrap();
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
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
                assert_eq!(audit_type, MissingTestsAudit::TYPE);
                assert_eq!(workspace_path, workspace);
                assert_eq!(reason, "workspace directory does not exist");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
        assert!(
            !workspace.exists(),
            "missing workspace must NOT be created as a side effect"
        );
        assert!(
            !workspace.join("openspec").exists(),
            "openspec/ must NOT be created as a side effect"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Workspace-validity gate: existing directory without `.git/` →
    /// WorkspaceUnavailable; no new files/subdirs are created.
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
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
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
        assert_eq!(before, after, "no new entries must appear in the workspace");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Parity check with `security_bug_audit`: the proposal-created
    /// `🔍` notification fires from the missing-tests audit too,
    /// because both delegate to `run_specs_writing_audit`. The full
    /// matrix (retry parenthetical, ValidationExhausted no-fire,
    /// ordering vs. commit, chatops-down/absent) is exercised in
    /// `security_bug.rs`; this test confirms the wiring is reached
    /// from the missing-tests entry point.
    #[tokio::test]
    async fn proposal_created_notification_fires_from_missing_tests_audit() {
        use super::super::test_support::{RecordingBackend, make_recording_ctx};
        use std::sync::Arc;

        let (_t, ws) = init_workspace_with(&[]);
        let new = ws
            .join("openspec/changes/tests-fire")
            .display()
            .to_string();
        let why = "Missing-tests audit must also signal proposal creation";
        let script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{new}'\ncat > '{new}/proposal.md' <<'EOF'\n## Why\n\n{why}\n\n## What Changes\n- thing\nEOF\nexit 0\n"
            ),
        );
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");

        let backend = Arc::new(RecordingBackend::new());
        let chatops = make_recording_ctx(backend.clone());

        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = MissingTestsAudit::new(&HashMap::new(), &cfg)
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

        let calls = backend.calls();
        assert_eq!(calls.len(), 1);
        let text = &calls[0].text;
        assert!(text.starts_with('🔍'));
        assert!(text.contains("missing_tests_audit"));
        assert!(text.contains("`tests-fire`"));
        assert!(text.contains(why));

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }
}
