//! Canon-consolidation audit (a76). Scans the canonical spec set for
//! requirements that *overlap* — two or more requirements expressing the
//! same invariant under different titles — AND drafts a `consolidate-`
//! change that merges them, so the canon stays compact and each invariant
//! has one home.
//!
//! This completes the canon-quality pair: `canon_contradiction_audit`
//! (a75) finds requirements that *conflict*; this audit finds requirements
//! that are *redundant* and proposes folding them together. Overlap vs
//! conflict differ only in prompt + disposition: a75 REPORTS findings
//! advisorily (`WritePolicy::None`); a76 WRITES a reviewable change
//! (`WritePolicy::OpenSpecOnly`), following the specs-writing pattern of
//! `missing_tests_audit` / `security_bug_audit`.
//!
//! Like a75, it shares the enumerate → retrieve → focused-judgment driver:
//! when a21's canonical-spec RAG is enabled the agent retrieves the nearest
//! requirements per requirement via `query_canonical_specs` and judges each
//! focused bundle for redundancy; when RAG is off the audit degrades to a
//! best-effort direct read AND logs the degradation. Distinct from the
//! other specs-writing audits, the consolidation agent's sandbox enables
//! the autocoder MCP tools (so `query_canonical_specs` is reachable) AND
//! drops `Bash` (the audit never shells out).
//!
//! Consolidation is the one audit that rewrites canon, so its hazard is
//! information loss — the general-vs-specific trap (merging a project-wide
//! prescription with one feature's implementation of it). That guard lives
//! in the prompt as design intent verified by the drift audit's semantic
//! judgment; it is deliberately NOT pinned by a unit test asserting prompt
//! wording (per the canonical `Tests assert behavior or derivation, never
//! message wording` requirement). The audit only ever writes a *change*
//! (never the canon directly); the human is the arbiter in PR review.
//!
//! `requires_head_change = true` — redundancy only changes when the canon
//! changes. Default cadence is heavy (`monthly`); default config leaves it
//! `Disabled`.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::specs_writing::{SpecsWritingAuditParams, run_specs_writing_audit};
use super::{Audit, AuditContext, AuditOutcome, WritePolicy};
use crate::config::{AuditSettings, ExecutorConfig, ResolvedSandbox};
use crate::prompts::{PromptId, PromptLoader};

/// Tools the consolidation agent may call: the read tools plus
/// `Write`/`Edit` so it can draft a `consolidate-` change under
/// `openspec/changes/`. NO `Bash` — the audit never shells out (the
/// framework runs `openspec validate` itself). `query_canonical_specs` /
/// `ask_user` / `outcome_*` are auto-included by the MCP-enabled CLI path
/// (`include_autocoder_tools`). The framework's post-hoc `OpenSpecOnly`
/// check catches any write outside `openspec/changes/`.
const ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Write", "Edit"];

/// Default number of nearest requirements retrieved per requirement when
/// RAG is enabled. Operator-tunable via
/// `audits.settings.canon_consolidation_audit.extra.retrieval_breadth`.
const DEFAULT_RETRIEVAL_BREADTH: u64 = 8;

/// Default cap on consolidation changes drafted per run. Consolidation
/// changes are denser to review than additive ones, so the default is `1`.
/// Operator-tunable via
/// `audits.settings.canon_consolidation_audit.extra.max_proposals_per_run`.
pub const DEFAULT_MAX_PROPOSALS_PER_RUN: u32 = 1;

const SETTINGS_KEY_MAX_PROPOSALS: &str = "max_proposals_per_run";
const SETTINGS_KEY_RETRIEVAL_BREADTH: &str = "retrieval_breadth";

pub struct CanonConsolidationAudit {
    settings: AuditSettings,
    max_proposals_per_run: u32,
    executor_command: String,
    executor_timeout_secs: u64,
    sandbox: ResolvedSandbox,
    /// Override for the directory the per-invocation sandbox settings file
    /// is written to. `None` (production) means `std::env::temp_dir()`.
    settings_dir: Option<PathBuf>,
    /// Override for the `openspec` validation binary. `None` (prod) means
    /// `openspec`. Tests point at a shell script so the audit can run
    /// without the real CLI on PATH.
    openspec_command: String,
    /// Test-only override for the RAG-enabled detection (which otherwise
    /// reads the process-global `crate::rag::shared_config`).
    #[cfg(test)]
    test_rag_enabled: Option<bool>,
}

impl CanonConsolidationAudit {
    pub const TYPE: &'static str = "canon_consolidation_audit";

    pub fn new(
        audit_settings: &HashMap<String, AuditSettings>,
        executor: &ExecutorConfig,
    ) -> Self {
        let settings = audit_settings.get(Self::TYPE).cloned().unwrap_or_default();
        let max_proposals_per_run = settings
            .extra
            .get(SETTINGS_KEY_MAX_PROPOSALS)
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
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
            #[cfg(test)]
            test_rag_enabled: None,
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

    #[cfg(test)]
    pub(crate) fn with_rag_enabled(mut self, enabled: bool) -> Self {
        self.test_rag_enabled = Some(enabled);
        self
    }

    /// Operator-tunable retrieval breadth (top_k) for `query_canonical_specs`.
    fn retrieval_breadth(&self) -> u64 {
        self.settings
            .extra
            .get(SETTINGS_KEY_RETRIEVAL_BREADTH)
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_RETRIEVAL_BREADTH)
    }

    /// Whether a21's canonical-spec RAG is enabled for this daemon. Drives
    /// the prompt's retrieval guidance AND the best-effort degradation log.
    fn rag_enabled(&self) -> bool {
        #[cfg(test)]
        if let Some(over) = self.test_rag_enabled {
            return over;
        }
        crate::rag::shared_config()
            .map(|c| c.is_active())
            .unwrap_or(false)
    }

    fn resolve_prompt(&self, workspace: Option<&Path>) -> Result<String> {
        Ok(PromptLoader::load(
            PromptId::AuditCanonConsolidation,
            self.settings.prompt_path.as_deref(),
            None,
            workspace,
        ))
    }
}

#[async_trait]
impl Audit for CanonConsolidationAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn description(&self) -> &'static str {
        "proposes consolidating redundant canonical requirements (canon-vs-canon)"
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::OpenSpecOnly
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        let rag_on = self.rag_enabled();
        let breadth = self.retrieval_breadth();

        let base_prompt = self.resolve_prompt(Some(ctx.workspace))?;
        let prompt = compose_prompt(&base_prompt, rag_on, breadth, self.max_proposals_per_run);
        let prompt_source = self
            .settings
            .prompt_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<embedded default>".to_string());

        // RAG mode is behavior (not prompt wording): log the decision and,
        // when RAG is off, the best-effort degradation. The specs-writing
        // helper logs the prompt + preamble; this section records the
        // retrieval decision the daemon made for this run.
        let _ = ctx.log_writer.write_section(
            "canon_consolidation_audit_rag",
            &format!(
                "rag_enabled: {rag_on}\nretrieval_breadth: {breadth}\ncoverage: {}",
                if rag_on {
                    "rag-assisted (query_canonical_specs)"
                } else {
                    "best-effort direct read (RAG not configured; subtle cross-capability redundancy may be missed)"
                }
            ),
        );
        if !rag_on {
            tracing::info!(
                url = %ctx.repo.url,
                audit_type = Self::TYPE,
                "canon_consolidation_audit: a21 RAG not configured; coverage is best-effort"
            );
        }

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
                commit_subject: "canon-consolidation proposals",
                allowed_tools: ALLOWED_TOOLS,
                include_autocoder_tools: true,
                model: model.as_ref(),
            },
            ctx,
        )
        .await
    }
}

/// Append the daemon-set retrieval + cap configuration to the embedded
/// prompt. This is operational guidance (which tool to use, how many
/// neighbors, the best-effort fallback, the per-run cap) — distinct from
/// the consolidation design intent (the general-vs-specific guard, scenario
/// preservation, the loss summary) baked into the embedded template, which
/// is deliberately NOT pinned by a test.
fn compose_prompt(base: &str, rag_on: bool, breadth: u64, max_proposals: u32) -> String {
    let mut out = base.trim_end().to_string();
    out.push_str("\n\n---\n\n## Retrieval configuration (set by the daemon for this run)\n\n");
    if rag_on {
        out.push_str(&format!(
            "Canonical-spec RAG is ENABLED. The `query_canonical_specs` MCP tool is backed by an \
             index this run. Enumerate the canonical requirements across `openspec/specs/*/spec.md`, \
             and for each requirement retrieve the {breadth} most semantically-similar requirements \
             via `query_canonical_specs` and judge that focused bundle for redundancy — the same \
             invariant stated under different titles. This bounds each check AND targets related \
             requirements, where redundancy actually lives.\n",
        ));
    } else {
        out.push_str(
            "Canonical-spec RAG is NOT configured this run; `query_canonical_specs` will return \
             empty hits. Fall back to a best-effort direct read of `openspec/specs/*/spec.md`, \
             focusing on requirements that govern the same subject. Coverage is best-effort — \
             subtle cross-capability redundancy may be missed without retrieval.\n",
        );
    }
    out.push_str(&format!(
        "\nDraft at most {max_proposals} consolidation change(s) this run; the daemon discards any \
         beyond that cap.\n",
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audits::AuditLogWriter;
    use crate::config::{ExecutorKind, RepositoryConfig};
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
            change_internal_contradiction_check: crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check: crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check: crate::config::ContradictionCheckMode::Disabled,
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
        RepositoryConfig {
            forge: None,
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

    /// Initialize a real git workspace (enough for `workspace_is_valid`
    /// AND the commit the specs-writing helper makes on success).
    fn init_workspace() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().to_path_buf();
        let git = |args: &[&str]| {
            let st = StdCommand::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@e.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(ws.join("README.md"), "hi\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "init"]);
        (dir, ws)
    }

    fn make_log_writer(workspace: &Path) -> AuditLogWriter {
        let (td, paths) = crate::testing::test_daemon_paths();
        std::mem::forget(td);
        AuditLogWriter::open(&paths, workspace, CanonConsolidationAudit::TYPE)
            .expect("audit log open succeeds")
    }

    /// A fake `claude` that drops a `consolidate-<slug>` change directory
    /// with the given `proposal.md` body, then exits 0.
    fn fake_claude_writes(dir: &Path, slug: &str, proposal_body: &str) -> PathBuf {
        let change = dir.join("openspec/changes").join(slug);
        let change = change.display().to_string();
        write_script(
            dir,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{change}'\ncat > '{change}/proposal.md' <<'EOF'\n{proposal_body}\nEOF\nexit 0\n"
            ),
        )
    }

    // ---- trait fixedness / settings -------------------------------------

    #[test]
    fn audit_type_and_policy_are_fixed() {
        let cfg = executor_cfg("/bin/true");
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg);
        assert_eq!(audit.audit_type(), "canon_consolidation_audit");
        assert!(audit.requires_head_change());
        assert!(matches!(audit.write_policy(), WritePolicy::OpenSpecOnly));
        let d = audit.description();
        assert!(!d.is_empty() && d.chars().count() <= 80, "description ≤ 80 chars: {d:?}");
    }

    #[test]
    fn allowed_tools_are_openspec_write_without_bash() {
        assert_eq!(ALLOWED_TOOLS, &["Read", "Glob", "Grep", "Write", "Edit"]);
        assert!(
            !ALLOWED_TOOLS.contains(&"Bash"),
            "the consolidation audit never shells out — Bash must not be in its sandbox"
        );
        for needed in ["Write", "Edit"] {
            assert!(
                ALLOWED_TOOLS.contains(&needed),
                "{needed} is required to draft a consolidate- change"
            );
        }
    }

    #[test]
    fn new_reads_max_proposals_and_defaults_to_one() {
        let mut extra = HashMap::new();
        extra.insert(
            SETTINGS_KEY_MAX_PROPOSALS.into(),
            serde_yml::Value::from(3u64),
        );
        let mut map = HashMap::new();
        map.insert(
            CanonConsolidationAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
                ..Default::default()
            },
        );
        let cfg = executor_cfg("claude");
        assert_eq!(
            CanonConsolidationAudit::new(&map, &cfg).max_proposals_per_run,
            3
        );
        // Default (consolidation changes are dense to review) is 1.
        assert_eq!(
            CanonConsolidationAudit::new(&HashMap::new(), &cfg).max_proposals_per_run,
            DEFAULT_MAX_PROPOSALS_PER_RUN
        );
        assert_eq!(DEFAULT_MAX_PROPOSALS_PER_RUN, 1);
    }

    #[test]
    fn new_reads_retrieval_breadth_and_defaults() {
        let mut extra = HashMap::new();
        extra.insert(
            SETTINGS_KEY_RETRIEVAL_BREADTH.into(),
            serde_yml::Value::from(12u64),
        );
        let mut map = HashMap::new();
        map.insert(
            CanonConsolidationAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
                ..Default::default()
            },
        );
        let cfg = executor_cfg("claude");
        assert_eq!(CanonConsolidationAudit::new(&map, &cfg).retrieval_breadth(), 12);
        assert_eq!(
            CanonConsolidationAudit::new(&HashMap::new(), &cfg).retrieval_breadth(),
            DEFAULT_RETRIEVAL_BREADTH
        );
    }

    // ---- prompt composition (RAG mode + cap are derivation, not wording) -

    #[test]
    fn compose_prompt_reflects_rag_on_with_breadth() {
        let out = compose_prompt("BASE PROMPT", true, 7, 1);
        assert!(out.contains("BASE PROMPT"));
        assert!(out.contains("query_canonical_specs"));
        assert!(out.contains('7'), "breadth value must be injected");
    }

    #[test]
    fn compose_prompt_reflects_rag_off_best_effort() {
        let out = compose_prompt("BASE PROMPT", false, 8, 1);
        assert!(out.contains("BASE PROMPT"));
        assert!(out.contains("best-effort"));
    }

    #[test]
    fn compose_prompt_states_the_per_run_cap() {
        let out = compose_prompt("BASE", false, 8, 4);
        assert!(out.contains('4'), "the per-run cap must be injected: {out}");
    }

    #[test]
    fn resolve_prompt_uses_embedded_default() {
        let cfg = executor_cfg("/bin/true");
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg);
        let prompt = audit.resolve_prompt(None).expect("default resolves");
        // Derivation checks (what the audit does), not message wording: the
        // embedded prompt is about consolidating canonical requirements via
        // a `consolidate-` change.
        assert!(prompt.contains("consolidate-"));
        assert!(prompt.contains("openspec/specs"));
    }

    // ---- run() integration ----------------------------------------------

    /// 5.3: a near-duplicate cluster yields one `consolidate-` change;
    /// `SpecsWritten` names it AND the validated change is committed.
    #[tokio::test]
    async fn near_duplicate_cluster_yields_one_consolidate_change() {
        let (_t, ws) = init_workspace();
        let _script = fake_claude_writes(
            &ws,
            "consolidate-outbound-retry",
            "## Why\nStripe AND PayPal retry rules are the same invariant.\n\n## What Changes\n- merge\n",
        );
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&ws.join("fake-claude.sh").to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string())
            .with_rag_enabled(true);
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
            AuditOutcome::SpecsWritten { changes, .. } => {
                assert_eq!(changes, vec!["consolidate-outbound-retry".to_string()]);
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }
        // The validated change was committed with the consolidation subject.
        let log = StdCommand::new("git")
            .args(["log", "--oneline", "-n", "3"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&log.stdout);
        assert!(
            log_str.contains("canon-consolidation proposals") && log_str.contains("1 change(s)"),
            "commit must reflect the consolidation subject + count: {log_str}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// 5.2 (RAG on): the run advertises `query_canonical_specs` — the
    /// composed prompt names the tool AND the preamble records that the
    /// autocoder MCP tools are included for this run.
    #[tokio::test]
    async fn run_with_rag_on_advertises_query_canonical_specs() {
        let (_t, ws) = init_workspace();
        let _script = fake_claude_writes(
            &ws,
            "consolidate-x",
            "## Why\nredundant pair.\n\n## What Changes\n- merge\n",
        );
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&ws.join("fake-claude.sh").to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string())
            .with_rag_enabled(true);
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
        let log = std::fs::read_to_string(&log_path).expect("log");
        assert!(log.contains("rag_enabled: true"));
        // The MCP-enabled CLI path is used so query_canonical_specs is
        // reachable; the composed prompt (logged by the helper) names it.
        assert!(
            log.contains("include_autocoder_tools: true"),
            "the run must enable the autocoder MCP tools: {log}"
        );
        assert!(
            log.contains("query_canonical_specs"),
            "the composed prompt must direct the agent at query_canonical_specs: {log}"
        );
        // The read-only-plus-write sandbox is in effect (no Bash).
        assert!(
            log.contains("allowed_tools: Read,Glob,Grep,Write,Edit"),
            "the OpenSpec-only sandbox must omit Bash: {log}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// 5.2 (RAG off): the audit proceeds best-effort AND logs the
    /// degradation.
    #[tokio::test]
    async fn run_logs_best_effort_when_rag_off() {
        let (_t, ws) = init_workspace();
        // Agent finds nothing → no change dir created.
        let _script = write_script(&ws, "fake-claude.sh", "#!/bin/sh\nexit 0\n");
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&ws.join("fake-claude.sh").to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string())
            .with_rag_enabled(false);
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
        assert!(matches!(outcome, AuditOutcome::SpecsWritten { ref changes, .. } if changes.is_empty()));
        let log = std::fs::read_to_string(&log_path).expect("log");
        assert!(log.contains("rag_enabled: false"));
        assert!(log.contains("best-effort"));
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// 5.6 (invalid draft): a draft that fails `openspec validate --strict`
    /// is discarded with no commit (here, no retry budget → ValidationExhausted).
    #[tokio::test]
    async fn invalid_draft_discarded_without_commit() {
        let (_t, ws) = init_workspace();
        let _script = fake_claude_writes(
            &ws,
            "consolidate-bad",
            "## Why\nmalformed delta.\n",
        );
        let bad_validator = write_script(
            &ws,
            "bad.sh",
            "#!/bin/sh\necho 'spec missing scenarios' >&2\nexit 2\n",
        );
        let cfg = executor_cfg(&ws.join("fake-claude.sh").to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(bad_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let head_before = crate::git::rev_parse(&ws, "HEAD").unwrap();
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
            AuditOutcome::ValidationExhausted { audit_type, final_error, .. } => {
                assert_eq!(audit_type, "canon_consolidation_audit");
                assert!(final_error.contains("consolidate-bad"), "names the change: {final_error}");
            }
            other => panic!("expected ValidationExhausted, got {other:?}"),
        }
        assert!(
            !ws.join("openspec/changes/consolidate-bad").exists(),
            "invalid draft must be removed (clean tree for the post-hoc check)"
        );
        let head_after = crate::git::rev_parse(&ws, "HEAD").unwrap();
        assert_eq!(head_before, head_after, "no commit on an invalid draft");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// 5.6 (empty) + 5.4 (general+specific not merged): when the agent
    /// drafts no change — including the case where a general+compatible-
    /// specific pair must NOT be merged per the prompt's general-vs-specific
    /// guard — the outcome is a silent `SpecsWritten(vec![])` with no commit.
    ///
    /// The guard itself is prompt-level design intent verified by the drift
    /// audit's semantic judgment; per the canonical "Tests assert behavior
    /// or derivation, never message wording" requirement it is NOT pinned by
    /// a prompt-substring assertion here. What is assertable is the audit's
    /// behavior when nothing is drafted: silence.
    #[tokio::test]
    async fn empty_result_is_silent_no_commit() {
        let (_t, ws) = init_workspace();
        let _script = write_script(&ws, "fake-claude.sh", "#!/bin/sh\nexit 0\n");
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&ws.join("fake-claude.sh").to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string());
        let repo = fixture_repo();
        let head_before = crate::git::rev_parse(&ws, "HEAD").unwrap();
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
            AuditOutcome::SpecsWritten { changes, .. } => assert!(changes.is_empty()),
            other => panic!("expected SpecsWritten(empty), got {other:?}"),
        }
        let head_after = crate::git::rev_parse(&ws, "HEAD").unwrap();
        assert_eq!(head_before, head_after, "empty result must NOT commit");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// 5.6 (cap): even if the agent drafts more than the cap, the audit
    /// commits at most `max_proposals_per_run` (default 1) change dirs.
    #[tokio::test]
    async fn per_run_cap_is_honored() {
        let (_t, ws) = init_workspace();
        let a = ws.join("openspec/changes/consolidate-a").display().to_string();
        let b = ws.join("openspec/changes/consolidate-b").display().to_string();
        let _script = write_script(
            &ws,
            "fake-claude.sh",
            &format!(
                "#!/bin/sh\nmkdir -p '{a}' '{b}'\necho '# a' > '{a}/proposal.md'\necho '# b' > '{b}/proposal.md'\nexit 0\n"
            ),
        );
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&ws.join("fake-claude.sh").to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_openspec_command(ok_validator.to_string_lossy().to_string())
            .with_max_proposals(1);
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
            AuditOutcome::SpecsWritten { changes, .. } => {
                assert_eq!(changes, vec!["consolidate-a".to_string()], "cap=1 keeps the first sorted dir");
            }
            other => panic!("expected SpecsWritten, got {other:?}"),
        }
        assert!(
            !ws.join("openspec/changes/consolidate-b").exists(),
            "the over-cap dir must be dropped"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// 5.5: the proposal's reviewer-facing loss summary (before/after
    /// scenario count + dropped-as-redundant list) is carried through to
    /// the committed change. This asserts the audit's behavior — it commits
    /// the agent's full proposal, loss summary included — without pinning
    /// the embedded prompt's wording.
    #[tokio::test]
    async fn drafted_proposal_loss_summary_is_preserved_in_commit() {
        let (_t, ws) = init_workspace();
        let body = "## Why\nStripe AND PayPal retry rules are one invariant.\n\n\
                    ## What Changes\nMerge into one requirement.\n\n\
                    Scenario count: before 5 across 2 requirements; after 4 in 1 requirement.\n\
                    Dropped as redundant: \"PayPal call retries\" — duplicate of the merged retry scenario.\n";
        let _script = fake_claude_writes(&ws, "consolidate-retry", body);
        let ok_validator = write_script(&ws, "ok.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&ws.join("fake-claude.sh").to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg)
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
        let committed = std::fs::read_to_string(
            ws.join("openspec/changes/consolidate-retry/proposal.md"),
        )
        .expect("committed proposal readable");
        assert!(committed.contains("before 5"), "before/after count preserved");
        assert!(committed.contains("Dropped as redundant"), "dropped list preserved");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Workspace-validity gate: a directory with no `.git/` →
    /// `WorkspaceUnavailable`, no LLM call, no side effects.
    #[tokio::test]
    async fn workspace_unavailable_when_dot_git_missing() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws-no-git");
        std::fs::create_dir_all(&workspace).unwrap();
        let cfg = executor_cfg("/bin/true");
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonConsolidationAudit::new(&HashMap::new(), &cfg)
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
            AuditOutcome::WorkspaceUnavailable { audit_type, reason, .. } => {
                assert_eq!(audit_type, CanonConsolidationAudit::TYPE);
                assert_eq!(reason, "workspace exists but has no .git/ subdirectory");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }
}
