//! Periodic-audit scheduler. Invoked per polling iteration AFTER
//! `recreate_branch` AND BEFORE `list_pending`, so an audit that writes
//! new OpenSpec changes feeds the same iteration's queue walk.
//!
//! Algorithm per audit:
//!   1. Resolve effective cadence. `Disabled` → skip.
//!   2. Load state. If `last_run_at + interval > now` → skip.
//!   3. If `requires_head_change` && stored sha == current HEAD → skip.
//!   4. Open the per-invocation log writer.
//!   5. Run the audit's `run`.
//!   6. Enforce `WritePolicy` via `git status --porcelain`; violations →
//!      revert (`reset --hard HEAD` or `+ clean -fd`), throttled chatops
//!      alert under `AuditWritePolicyViolation`, do NOT update state.
//!   7. On success: dispatch outcome (chatops post for `Reported`,
//!      info log for `SpecsWritten`), update state.

use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;

use super::state::{AuditRunEntry, AuditState};
use super::{
    Audit, AuditContext, AuditLogWriter, AuditOutcome, AuditRegistry, Finding, Severity,
    WritePolicy,
};
use crate::alert_state::AlertCategory;
use crate::alerts::handle_predictable_failure;
use crate::config::{AuditSettings, AuditsConfig, RepositoryConfig, resolved_cadence};
use crate::polling_loop::ChatOpsContext;
use crate::{git, workspace};

/// Default per-finding excerpt cap for chatops output. The full body
/// always lives in the audit-run log; the chatops post just lists
/// subject lines.
pub const DEFAULT_FINDING_EXCERPT_CHARS: usize = 200;

/// Iterate the registry, run every due audit, and enforce write
/// policies. Failures inside one audit do NOT abort the iteration —
/// the scheduler logs the error, leaves the state file unchanged for
/// that audit, and moves on.
pub async fn run_due_audits(
    registry: &AuditRegistry,
    workspace: &Path,
    repo: &RepositoryConfig,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
    chatops_ctx: Option<&ChatOpsContext>,
) -> Result<()> {
    if registry.is_empty() {
        return Ok(());
    }
    let mut state = AuditState::load_or_default(workspace);
    for audit in registry.iter() {
        if let Err(e) = run_one_audit(
            audit.as_ref(),
            workspace,
            repo,
            audits_cfg,
            audit_settings,
            chatops_ctx,
            &mut state,
        )
        .await
        {
            // Per spec: log and continue. State is not updated for this
            // audit (the inner helper only writes state on success).
            tracing::error!(
                url = %repo.url,
                audit_type = audit.audit_type(),
                "audit `{}` failed (this iteration's other audits continue): {e:#}",
                audit.audit_type()
            );
        }
    }
    Ok(())
}

/// Inner per-audit driver. Returns `Ok(())` for skipped audits and
/// completed-successfully-or-violation-handled audits; returns `Err`
/// only when the audit's `run()` itself errored (the caller logs and
/// moves on).
async fn run_one_audit(
    audit: &dyn Audit,
    workspace: &Path,
    repo: &RepositoryConfig,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
    chatops_ctx: Option<&ChatOpsContext>,
    state: &mut AuditState,
) -> Result<()> {
    let audit_type = audit.audit_type();
    let cadence = resolved_cadence(repo, audits_cfg, audit_type);
    let interval = match cadence.interval() {
        Some(d) => d,
        None => {
            tracing::debug!(audit_type, "audit cadence is Disabled; skipping");
            return Ok(());
        }
    };

    let now = Utc::now();
    let current_head_sha = git::rev_parse(workspace, &repo.base_branch).ok();

    if let Some(prior) = state.runs.get(audit_type) {
        let next_due = prior.last_run_at + interval;
        if now < next_due {
            tracing::debug!(
                audit_type,
                "audit not yet due (last run {}, next due {})",
                prior.last_run_at,
                next_due
            );
            return Ok(());
        }
        if audit.requires_head_change() {
            if let (Some(prior_sha), Some(current_sha)) =
                (prior.last_run_sha.as_ref(), current_head_sha.as_ref())
            {
                if prior_sha == current_sha {
                    tracing::debug!(
                        audit_type,
                        sha = %current_sha,
                        "audit requires HEAD change but HEAD unchanged since last run; skipping"
                    );
                    return Ok(());
                }
            }
        }
    }

    let log_writer = AuditLogWriter::open(workspace, audit_type)?;
    // Record run-prelude metadata so operators reading the log later see
    // exactly when the run started and what cadence/SHA context it had.
    log_writer.write_section(
        "audit_run_preamble",
        &format!(
            "audit_type: {audit_type}\nworkspace: {workspace}\nstart: {start}\nrepo_url: {url}\nbase_branch: {base}\ncadence: {cadence:?}\nrequires_head_change: {rhc}\nwrite_policy: {wp:?}\ncurrent_head_sha: {head}\nlast_run: {last}",
            workspace = workspace.display(),
            start = now.to_rfc3339(),
            url = repo.url,
            base = repo.base_branch,
            rhc = audit.requires_head_change(),
            wp = audit.write_policy(),
            head = current_head_sha.as_deref().unwrap_or("<unresolved>"),
            last = state
                .runs
                .get(audit_type)
                .map(|p| format!(
                    "{} sha={}",
                    p.last_run_at.to_rfc3339(),
                    p.last_run_sha.as_deref().unwrap_or("<none>")
                ))
                .unwrap_or_else(|| "<never>".to_string()),
        ),
    )?;

    let mut ctx = AuditContext {
        workspace,
        repo,
        chatops_ctx,
        log_writer: log_writer.clone(),
    };

    let run_result = audit.run(&mut ctx).await;
    let end_ts = Utc::now();

    let outcome = match run_result {
        Ok(o) => o,
        Err(e) => {
            log_writer.write_section(
                "audit_run_error",
                &format!("end: {}\nerror: {e:#}", end_ts.to_rfc3339()),
            )?;
            return Err(e);
        }
    };

    // Post-hoc write-policy enforcement. Use `-uall` so untracked
    // directories are expanded to per-file paths; otherwise an audit
    // could write `openspec/changes/new-thing/proposal.md` and git
    // would report just `?? openspec/` (when the parent is also new),
    // which would mis-categorize the OpenSpecOnly check.
    let policy = audit.write_policy();
    let porcelain = match git::status_porcelain_untracked_all(workspace) {
        Ok(s) => s,
        Err(e) => {
            // Couldn't probe the workspace — treat as a violation so the
            // operator notices, but skip the revert (we have no usable
            // signal). State stays untouched.
            log_writer.write_section(
                "audit_postcheck_failed",
                &format!("git status --porcelain errored: {e:#}"),
            )?;
            return Err(e);
        }
    };
    if let Some(violation) = detect_write_policy_violation(policy, &porcelain) {
        log_writer.write_section(
            "audit_write_policy_violation",
            &format!(
                "policy: {policy:?}\noffending diff:\n{porcelain}\nreason: {reason}",
                reason = violation.reason,
            ),
        )?;
        // Revert the unexpected diff. None → reset --hard HEAD.
        // OpenSpecOnly → reset --hard HEAD + clean -fd (to also drop
        // untracked files outside the allowed prefix).
        match policy {
            WritePolicy::None => {
                if let Err(e) = git::reset_hard_head(workspace) {
                    tracing::error!(
                        url = %repo.url,
                        audit_type,
                        "failed `git reset --hard HEAD` after WritePolicy::None violation: {e:#}"
                    );
                }
                // `reset --hard HEAD` does not remove untracked files,
                // and an audit that wasn't supposed to write anything
                // might have created some — clean them up so the next
                // iteration's startup dirty check doesn't see them.
                if let Err(e) = git::clean_force(workspace) {
                    tracing::error!(
                        url = %repo.url,
                        audit_type,
                        "failed `git clean -fd` after WritePolicy::None violation: {e:#}"
                    );
                }
            }
            WritePolicy::OpenSpecOnly => {
                if let Err(e) = git::reset_hard_head(workspace) {
                    tracing::error!(
                        url = %repo.url,
                        audit_type,
                        "failed `git reset --hard HEAD` after WritePolicy::OpenSpecOnly violation: {e:#}"
                    );
                }
                if let Err(e) = git::clean_force(workspace) {
                    tracing::error!(
                        url = %repo.url,
                        audit_type,
                        "failed `git clean -fd` after WritePolicy::OpenSpecOnly violation: {e:#}"
                    );
                }
            }
            WritePolicy::CanonicalSpecMerge => {
                if let Err(e) = git::reset_hard_head(workspace) {
                    tracing::error!(
                        url = %repo.url,
                        audit_type,
                        "failed `git reset --hard HEAD` after WritePolicy::CanonicalSpecMerge violation: {e:#}"
                    );
                }
                if let Err(e) = git::clean_force(workspace) {
                    tracing::error!(
                        url = %repo.url,
                        audit_type,
                        "failed `git clean -fd` after WritePolicy::CanonicalSpecMerge violation: {e:#}"
                    );
                }
            }
            WritePolicy::Approved => {
                // No post-hoc enforcement.
            }
        }
        let alert_err = anyhow::anyhow!(
            "audit `{audit_type}` violated WritePolicy::{policy:?}: {}",
            violation.reason
        );
        handle_predictable_failure(
            workspace,
            &repo.url,
            chatops_ctx,
            chatops_ctx
                .map(|c| c.failure_alerts_enabled)
                .unwrap_or(false),
            AlertCategory::AuditWritePolicyViolation,
            &alert_err,
        )
        .await;
        // State NOT updated → cadence re-triggers next iteration.
        return Ok(());
    }

    // Outcome dispatch.
    let outcome_kind = outcome.kind();
    match &outcome {
        AuditOutcome::NoFindings => {
            log_writer.write_section(
                "audit_run_outcome",
                &format!("kind: NoFindings\nend: {}", end_ts.to_rfc3339()),
            )?;
        }
        AuditOutcome::Reported(findings) => {
            log_writer.write_section(
                "audit_run_outcome",
                &format!(
                    "kind: Reported\nend: {}\nfindings_count: {}",
                    end_ts.to_rfc3339(),
                    findings.len()
                ),
            )?;
            // Full findings body is preserved in the log; chatops gets
            // the truncated subjects.
            for (i, f) in findings.iter().enumerate() {
                log_writer.write_section(
                    &format!("finding_{i:03}"),
                    &format!(
                        "severity: {:?}\nsubject: {}\nanchor: {}\nbody:\n{}",
                        f.severity,
                        f.subject,
                        f.anchor.as_deref().unwrap_or("<none>"),
                        f.body
                    ),
                )?;
            }
            let notify_on_clean = audit_settings
                .get(audit_type)
                .map(|s| s.notify_on_clean)
                .unwrap_or(false);
            dispatch_reported_to_chatops(
                chatops_ctx,
                &repo.url,
                audit_type,
                findings,
                notify_on_clean,
            )
            .await;
        }
        AuditOutcome::SpecsWritten(names) => {
            log_writer.write_section(
                "audit_run_outcome",
                &format!(
                    "kind: SpecsWritten\nend: {}\nspecs:\n{}",
                    end_ts.to_rfc3339(),
                    names.join("\n")
                ),
            )?;
            tracing::info!(
                url = %repo.url,
                audit_type,
                count = names.len(),
                "audit wrote {} new spec(s); they will be picked up by this iteration's list_pending: {}",
                names.len(),
                names.join(", "),
            );
        }
    }

    // Success → persist state.
    state.record(
        audit_type,
        AuditRunEntry {
            last_run_at: now,
            last_run_sha: current_head_sha,
            last_outcome: outcome_kind,
        },
    );
    if let Err(e) = state.save(workspace) {
        tracing::warn!(
            url = %repo.url,
            audit_type,
            "failed to persist audit state after successful run: {e:#}"
        );
    }
    // Ensure .audit-state.json is registered in .git/info/exclude. Most
    // callers go through workspace::ensure_initialized which already
    // registers it, but tests that build a workspace by hand may skip
    // that path. The helper is idempotent.
    if workspace.join(".git").is_dir() {
        if let Err(e) = workspace::ensure_git_info_excluded(workspace, ".audit-state.json") {
            tracing::warn!(
                "could not register .audit-state.json in .git/info/exclude: {e:#}"
            );
        }
    }
    Ok(())
}

struct PolicyViolation {
    reason: String,
}

/// Inspect `git status --porcelain` output against the audit's policy.
/// `None` is returned iff the diff is allowed; `Some` describes why.
pub(crate) fn detect_write_policy_violation(
    policy: WritePolicy,
    porcelain: &str,
) -> Option<PolicyViolation> {
    let lines: Vec<&str> = porcelain
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    match policy {
        WritePolicy::None => Some(PolicyViolation {
            reason: format!("workspace dirty after audit (expected clean): {} entry(ies)", lines.len()),
        }),
        WritePolicy::OpenSpecOnly => {
            let mut bad_paths: Vec<String> = Vec::new();
            for line in &lines {
                let path = extract_porcelain_path(line);
                if !path.starts_with("openspec/changes/") {
                    bad_paths.push(path.to_string());
                }
            }
            if bad_paths.is_empty() {
                None
            } else {
                Some(PolicyViolation {
                    reason: format!(
                        "diff includes path(s) outside openspec/changes/: {}",
                        bad_paths.join(", ")
                    ),
                })
            }
        }
        WritePolicy::CanonicalSpecMerge => {
            let mut bad_paths: Vec<String> = Vec::new();
            for line in &lines {
                let path = extract_porcelain_path(line);
                if !path.starts_with("openspec/specs/") {
                    bad_paths.push(path.to_string());
                }
            }
            if bad_paths.is_empty() {
                None
            } else {
                Some(PolicyViolation {
                    reason: format!(
                        "diff includes path(s) outside openspec/specs/: {}",
                        bad_paths.join(", ")
                    ),
                })
            }
        }
        WritePolicy::Approved => None,
    }
}

/// Pull the path out of a `git status --porcelain` line. Lines look like
/// `XY <path>`; for renames the format is `R  <from> -> <to>` but we
/// keep the trailing target which is what callers care about.
fn extract_porcelain_path(line: &str) -> &str {
    // Skip the first two status chars + one space (per `git status
    // --porcelain` man page: `XY <path>`).
    let trimmed = line.get(3..).unwrap_or(line.trim_start());
    if let Some(idx) = trimmed.rfind(" -> ") {
        trimmed[idx + 4..].trim()
    } else {
        trimmed.trim()
    }
}

async fn dispatch_reported_to_chatops(
    chatops_ctx: Option<&ChatOpsContext>,
    repo_url: &str,
    audit_type: &str,
    findings: &[Finding],
    notify_on_clean: bool,
) {
    let Some(ctx) = chatops_ctx else { return };
    let text = if findings.is_empty() {
        if !notify_on_clean {
            return;
        }
        format_clean_message(repo_url, audit_type)
    } else {
        format_findings_message(repo_url, audit_type, findings, DEFAULT_FINDING_EXCERPT_CHARS)
    };
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            url = %repo_url,
            audit_type,
            "audit chatops post failed: {e:#}"
        );
    }
}

/// Render a `Reported(findings)` outcome as the chatops post body. The
/// caller is responsible for not invoking this with an empty `findings`
/// when `notify_on_clean = false`.
pub fn format_findings_message(
    repo_url: &str,
    audit_type: &str,
    findings: &[Finding],
    per_finding_max_chars: usize,
) -> String {
    let mut out = format!(
        "📋 `{repo_url}`: {audit_type} — {n} finding(s)",
        n = findings.len()
    );
    for f in findings {
        let glyph = f.severity.glyph();
        let subject = truncate(&f.subject, per_finding_max_chars);
        if let Some(anchor) = f.anchor.as_deref() {
            out.push_str(&format!("\n  {glyph} {subject} ({anchor})"));
        } else {
            out.push_str(&format!("\n  {glyph} {subject}"));
        }
    }
    out
}

/// Render the "no findings" chatops post used when `notify_on_clean`
/// is true.
pub fn format_clean_message(repo_url: &str, audit_type: &str) -> String {
    format!("✅ `{repo_url}`: {audit_type} — no findings")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audits::{Audit, AuditContext, AuditOutcome, AuditOutcomeKind, WritePolicy};
    use crate::chatops::{ChatOpsBackend, SlackBackend};
    use crate::config::{AuditsConfig, Cadence, RepositoryConfig};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // ---------------- Fixture audits ----------------

    /// Records the number of times its `run` was invoked.
    struct CountingAudit {
        slug: &'static str,
        rhc: bool,
        policy: WritePolicy,
        invocations: Arc<Mutex<u32>>,
        outcome: Mutex<Option<AuditOutcome>>,
        // When true, the audit writes a stray file to the workspace
        // before returning. Used to drive the post-hoc check.
        write_file: Option<&'static str>,
        // When set, the audit returns Err(...).
        fail_with: Option<&'static str>,
    }

    impl CountingAudit {
        fn new(slug: &'static str) -> Self {
            Self {
                slug,
                rhc: true,
                policy: WritePolicy::None,
                invocations: Arc::new(Mutex::new(0)),
                outcome: Mutex::new(Some(AuditOutcome::NoFindings)),
                write_file: None,
                fail_with: None,
            }
        }
        fn with_rhc(mut self, rhc: bool) -> Self {
            self.rhc = rhc;
            self
        }
        fn with_policy(mut self, p: WritePolicy) -> Self {
            self.policy = p;
            self
        }
        fn with_outcome(self, o: AuditOutcome) -> Self {
            *self.outcome.lock().unwrap() = Some(o);
            self
        }
        fn writes_file(mut self, path: &'static str) -> Self {
            self.write_file = Some(path);
            self
        }
        fn fails(mut self, msg: &'static str) -> Self {
            self.fail_with = Some(msg);
            self
        }
        fn invocation_count(&self) -> u32 {
            *self.invocations.lock().unwrap()
        }
    }

    #[async_trait]
    impl Audit for CountingAudit {
        fn audit_type(&self) -> &'static str {
            self.slug
        }
        fn requires_head_change(&self) -> bool {
            self.rhc
        }
        fn write_policy(&self) -> WritePolicy {
            self.policy
        }
        async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
            *self.invocations.lock().unwrap() += 1;
            if let Some(p) = self.write_file {
                let abs = ctx.workspace.join(p);
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&abs, "intruder\n").unwrap();
            }
            if let Some(msg) = self.fail_with {
                return Err(anyhow::anyhow!("{msg}"));
            }
            // Take ownership so we can return a non-cloneable value once.
            let o = self
                .outcome
                .lock()
                .unwrap()
                .take()
                .unwrap_or(AuditOutcome::NoFindings);
            // For tests that need the audit to run more than once, put a
            // default outcome back in.
            *self.outcome.lock().unwrap() = Some(AuditOutcome::NoFindings);
            Ok(o)
        }
    }

    // ---------------- Fixture workspace + repo ----------------

    fn run_git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_workspace() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().to_path_buf();
        run_git(&ws, &["init", "-q", "-b", "main"]);
        run_git(&ws, &["config", "user.email", "t@e.com"]);
        run_git(&ws, &["config", "user.name", "t"]);
        std::fs::write(ws.join("README.md"), "hi\n").unwrap();
        run_git(&ws, &["add", "README.md"]);
        run_git(&ws, &["commit", "-q", "-m", "init"]);
        // Register .audit-state.json so dirty checks ignore it.
        crate::workspace::ensure_git_info_excluded(&ws, ".audit-state.json").unwrap();
        (dir, ws)
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

    fn audits_cfg_daily(slug: &str) -> AuditsConfig {
        let mut defaults = HashMap::new();
        defaults.insert(slug.to_string(), Cadence::Daily);
        AuditsConfig {
            defaults,
            settings: HashMap::new(),
        }
    }

    // ---------------- Tests ----------------

    #[tokio::test]
    async fn audit_due_when_cadence_elapsed() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("a1"));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        // Pre-populate state as if last run was 2 days ago.
        let mut state = AuditState::default();
        state.record(
            "a1",
            AuditRunEntry {
                last_run_at: Utc::now() - chrono::Duration::days(2),
                last_run_sha: Some("definitely-not-current".into()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();
        let cfg = audits_cfg_daily("a1");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        assert_eq!(*counter.lock().unwrap(), 1, "audit should have run");
    }

    #[tokio::test]
    async fn audit_skipped_when_cadence_not_elapsed() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("a2"));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        // Pre-populate state as if last run was 1 hour ago (daily cadence).
        let mut state = AuditState::default();
        state.record(
            "a2",
            AuditRunEntry {
                last_run_at: Utc::now() - chrono::Duration::hours(1),
                last_run_sha: Some("anything".into()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();
        let cfg = audits_cfg_daily("a2");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        assert_eq!(*counter.lock().unwrap(), 0, "audit must NOT run within cadence");
    }

    #[tokio::test]
    async fn audit_skipped_when_requires_head_change_and_sha_matches() {
        let (_t, ws) = init_workspace();
        let head = git::rev_parse(&ws, "main").unwrap();
        let audit = Arc::new(CountingAudit::new("a3").with_rhc(true));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let mut state = AuditState::default();
        state.record(
            "a3",
            AuditRunEntry {
                last_run_at: Utc::now() - chrono::Duration::days(7),
                last_run_sha: Some(head.clone()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();
        let cfg = audits_cfg_daily("a3");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        assert_eq!(*counter.lock().unwrap(), 0, "HEAD unchanged → skip");
    }

    #[tokio::test]
    async fn audit_runs_when_requires_head_change_but_sha_differs() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("a4").with_rhc(true));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let mut state = AuditState::default();
        state.record(
            "a4",
            AuditRunEntry {
                last_run_at: Utc::now() - chrono::Duration::days(7),
                last_run_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();
        let cfg = audits_cfg_daily("a4");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        assert_eq!(*counter.lock().unwrap(), 1, "SHA differs → audit must run");
    }

    #[tokio::test]
    async fn audit_disabled_cadence_never_runs() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("a5"));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        // No AuditsConfig at all → cadence resolves to Disabled.
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, None, &HashMap::new(), None)
            .await
            .unwrap();
        assert_eq!(*counter.lock().unwrap(), 0);
        // Explicit Disabled also never runs.
        let mut defaults = HashMap::new();
        defaults.insert("a5".to_string(), Cadence::Disabled);
        let cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
        };
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        assert_eq!(*counter.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn write_policy_none_post_hoc_diff_triggers_revert_and_alert() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("a6")
                .with_policy(WritePolicy::None)
                .writes_file("INTRUDER.md"),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("a6");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        // After the violation handler, the workspace should be clean
        // again (reset --hard HEAD discards the intruder).
        let porcelain = git::status_porcelain(&ws).unwrap();
        assert!(
            !porcelain.contains("INTRUDER"),
            "violation must be reverted; got porcelain: {porcelain}"
        );
        // State must NOT have been updated.
        let state = AuditState::load_or_default(&ws);
        assert!(
            !state.runs.contains_key("a6"),
            "state must NOT record a violating run (cadence re-triggers next iteration)"
        );
    }

    #[tokio::test]
    async fn write_policy_openspec_only_rejects_diff_outside_changes() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("a7")
                .with_policy(WritePolicy::OpenSpecOnly)
                .writes_file("src/forbidden.rs"),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("a7");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        // After the OpenSpecOnly violation, src/forbidden.rs must be gone
        // (reset + clean -fd removes untracked).
        assert!(
            !ws.join("src/forbidden.rs").exists(),
            "untracked forbidden path must be removed by clean -fd"
        );
        // State must NOT have been updated.
        let state = AuditState::load_or_default(&ws);
        assert!(
            !state.runs.contains_key("a7"),
            "state must NOT record a violating run"
        );
    }

    #[tokio::test]
    async fn write_policy_openspec_only_allows_diff_under_changes() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("a7b")
                .with_policy(WritePolicy::OpenSpecOnly)
                .writes_file("openspec/changes/new-thing/proposal.md")
                .with_outcome(AuditOutcome::SpecsWritten(vec!["new-thing".into()])),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("a7b");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        // The path is inside the allowed prefix → no revert.
        assert!(
            ws.join("openspec/changes/new-thing/proposal.md").exists(),
            "openspec/changes/ path must be preserved"
        );
        // State updated.
        let state = AuditState::load_or_default(&ws);
        assert!(state.runs.contains_key("a7b"));
    }

    #[tokio::test]
    async fn audit_failure_does_not_update_state_and_does_not_abort_iteration() {
        let (_t, ws) = init_workspace();
        let audit_a = Arc::new(CountingAudit::new("af1").fails("intentional"));
        let audit_b = Arc::new(CountingAudit::new("af2"));
        let counter_b = audit_b.invocations.clone();
        let registry =
            AuditRegistry::with_audits(vec![audit_a.clone(), audit_b.clone()]);
        // Both due.
        let mut defaults = HashMap::new();
        defaults.insert("af1".to_string(), Cadence::Daily);
        defaults.insert("af2".to_string(), Cadence::Daily);
        let cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
        };
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .expect("scheduler must not propagate audit errors");
        // af2 (later in registry) MUST have run despite af1 erroring.
        assert_eq!(
            *counter_b.lock().unwrap(),
            1,
            "subsequent audits must still run after one fails"
        );
        // af1's state must NOT have been recorded.
        let state = AuditState::load_or_default(&ws);
        assert!(!state.runs.contains_key("af1"));
        assert!(state.runs.contains_key("af2"));
    }

    async fn fixture_chatops(server: &mut mockito::Server) -> Arc<dyn ChatOpsBackend> {
        let _auth = server
            .mock("POST", "/auth.test")
            .with_status(200)
            .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
            .create_async()
            .await;
        Arc::new(
            SlackBackend::new_at(server.url(), "xoxb-fixture".into())
                .await
                .unwrap(),
        )
    }

    fn make_ctx(chatops: Arc<dyn ChatOpsBackend>) -> ChatOpsContext {
        ChatOpsContext {
            chatops,
            channel: "C_FIXTURE".to_string(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        }
    }

    #[tokio::test]
    async fn reported_findings_post_to_chatops_with_format() {
        let (_t, ws) = init_workspace();
        let findings = vec![
            Finding {
                severity: Severity::Medium,
                subject: "file foo.rs is 1234 lines (threshold: 800)".into(),
                body: "(detail)".into(),
                anchor: Some("foo.rs:1".into()),
            },
            Finding {
                severity: Severity::Low,
                subject: "duplicate signature `fn helper()` across mod_a.rs, mod_b.rs".into(),
                body: "(detail)".into(),
                anchor: None,
            },
        ];
        let audit = Arc::new(
            CountingAudit::new("rep1").with_outcome(AuditOutcome::Reported(findings.clone())),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("rep1");
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex("📋".into()))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);

        run_due_audits(
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            Some(&ctx),
        )
        .await
        .unwrap();
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn reported_no_findings_silent_unless_notify_on_clean() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("clean1").with_outcome(AuditOutcome::Reported(vec![])),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("clean1");
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        // Without notify_on_clean → must NOT post.
        let silent_mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;
        let ctx = make_ctx(chatops.clone());

        run_due_audits(
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            Some(&ctx),
        )
        .await
        .unwrap();
        silent_mock.assert_async().await;

        // With notify_on_clean: should post.
        // Clear state so the audit is due again.
        let _ = std::fs::remove_file(ws.join(".audit-state.json"));
        let audit2 = Arc::new(
            CountingAudit::new("clean2").with_outcome(AuditOutcome::Reported(vec![])),
        );
        let registry2 = AuditRegistry::with_audits(vec![audit2.clone()]);
        let cfg2 = audits_cfg_daily("clean2");
        let mut settings = HashMap::new();
        settings.insert(
            "clean2".to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: true,
                extra: HashMap::new(),
            },
        );
        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex("✅".into()))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        run_due_audits(
            &registry2,
            &ws,
            &repo,
            Some(&cfg2),
            &settings,
            Some(&ctx),
        )
        .await
        .unwrap();
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn specs_written_outcome_logs_info_no_chatops() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("sw1")
                .with_policy(WritePolicy::OpenSpecOnly)
                .writes_file("openspec/changes/new-spec/proposal.md")
                .with_outcome(AuditOutcome::SpecsWritten(vec!["new-spec".into()])),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("sw1");
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let no_post = server
            .mock("POST", "/chat.postMessage")
            .expect(0) // SpecsWritten must NOT post (per spec)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);
        run_due_audits(
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            Some(&ctx),
        )
        .await
        .unwrap();
        no_post.assert_async().await;
        // State updated.
        let state = AuditState::load_or_default(&ws);
        let entry = state.runs.get("sw1").expect("state recorded");
        assert_eq!(entry.last_outcome, AuditOutcomeKind::SpecsWritten);
    }

    #[tokio::test]
    async fn audit_run_log_written_per_invocation() {
        // Use a workspace with a unique basename so the global
        // /tmp/autocoder/logs/<basename>/audits/ path is hermetic.
        let basename = format!("audit-log-test-{}", uuid::Uuid::new_v4());
        let parent = TempDir::new().unwrap();
        let ws = parent.path().join(&basename);
        std::fs::create_dir_all(&ws).unwrap();
        run_git(&ws, &["init", "-q", "-b", "main"]);
        run_git(&ws, &["config", "user.email", "t@e.com"]);
        run_git(&ws, &["config", "user.name", "t"]);
        std::fs::write(ws.join("README.md"), "hi\n").unwrap();
        run_git(&ws, &["add", "README.md"]);
        run_git(&ws, &["commit", "-q", "-m", "init"]);
        crate::workspace::ensure_git_info_excluded(&ws, ".audit-state.json").unwrap();

        let audit = Arc::new(CountingAudit::new("logged1"));
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("logged1");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        let log_dir = PathBuf::from("/tmp/autocoder/logs")
            .join(&basename)
            .join("audits");
        assert!(log_dir.exists(), "audit log dir must be created");
        let entries: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert!(
            entries.iter().any(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with("logged1-") && name.ends_with(".log")
            }),
            "expected logged1-<ts>.log in {}",
            log_dir.display()
        );
        // Clean up the global tmp dir we created.
        let _ = std::fs::remove_dir_all(
            PathBuf::from("/tmp/autocoder/logs").join(&basename),
        );
    }

    #[test]
    fn format_findings_message_renders_header_and_glyphs() {
        let findings = vec![
            Finding {
                severity: Severity::High,
                subject: "critical issue".into(),
                body: "x".into(),
                anchor: Some("file.rs:42".into()),
            },
            Finding {
                severity: Severity::Medium,
                subject: "moderate".into(),
                body: "x".into(),
                anchor: None,
            },
            Finding {
                severity: Severity::Low,
                subject: "minor".into(),
                body: "x".into(),
                anchor: None,
            },
        ];
        let msg = format_findings_message("git@github.com:o/r.git", "type1", &findings, 200);
        assert!(msg.contains("📋"));
        assert!(msg.contains("git@github.com:o/r.git"));
        assert!(msg.contains("type1"));
        assert!(msg.contains("3 finding(s)"));
        assert!(msg.contains("🔴"));
        assert!(msg.contains("⚠"));
        assert!(msg.contains("•"));
        assert!(msg.contains("file.rs:42"));
    }

    #[test]
    fn format_findings_message_truncates_long_subjects() {
        let long = "x".repeat(500);
        let findings = vec![Finding {
            severity: Severity::Low,
            subject: long,
            body: "x".into(),
            anchor: None,
        }];
        let msg = format_findings_message("u", "t", &findings, 50);
        assert!(msg.contains('…'), "long subject should be truncated: {msg}");
    }

    #[test]
    fn format_clean_message_uses_check_glyph() {
        let msg = format_clean_message("git@github.com:o/r.git", "x");
        assert!(msg.starts_with("✅"));
        assert!(msg.contains("git@github.com:o/r.git"));
        assert!(msg.contains("x"));
        assert!(msg.contains("no findings"));
    }

    #[test]
    fn detect_violation_none_with_empty_porcelain_is_ok() {
        assert!(detect_write_policy_violation(WritePolicy::None, "").is_none());
    }

    #[test]
    fn detect_violation_none_with_dirty_workspace_fails() {
        let v = detect_write_policy_violation(WritePolicy::None, "?? new-file.txt");
        assert!(v.is_some());
    }

    #[test]
    fn detect_violation_openspec_only_allows_changes_dir() {
        let porcelain = "?? openspec/changes/new-thing/proposal.md\n M openspec/changes/new-thing/tasks.md";
        assert!(detect_write_policy_violation(WritePolicy::OpenSpecOnly, porcelain).is_none());
    }

    #[test]
    fn detect_violation_openspec_only_rejects_outside_path() {
        let porcelain = "?? openspec/changes/new/proposal.md\n M src/lib.rs";
        let v = detect_write_policy_violation(WritePolicy::OpenSpecOnly, porcelain);
        assert!(v.is_some());
        assert!(v.unwrap().reason.contains("src/lib.rs"));
    }

    #[test]
    fn detect_violation_approved_always_ok() {
        let porcelain = " M anywhere/at/all.rs";
        assert!(detect_write_policy_violation(WritePolicy::Approved, porcelain).is_none());
    }

    #[test]
    fn detect_violation_canonical_spec_merge_allows_specs_dir() {
        let porcelain = " M openspec/specs/cap-a/spec.md\n M openspec/specs/cap-b/spec.md";
        assert!(
            detect_write_policy_violation(WritePolicy::CanonicalSpecMerge, porcelain).is_none()
        );
    }

    #[test]
    fn detect_violation_canonical_spec_merge_rejects_outside_path() {
        let porcelain = " M openspec/specs/cap/spec.md\n M openspec/changes/foo/proposal.md";
        let v = detect_write_policy_violation(WritePolicy::CanonicalSpecMerge, porcelain);
        assert!(v.is_some());
        assert!(v.unwrap().reason.contains("openspec/changes/foo/proposal.md"));
    }

    #[tokio::test]
    async fn write_policy_canonical_spec_merge_rejects_diff_outside_specs() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("csm1")
                .with_policy(WritePolicy::CanonicalSpecMerge)
                .writes_file("src/forbidden.rs"),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("csm1");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        // After the CanonicalSpecMerge violation, the file outside the
        // allowed prefix must be gone (reset + clean -fd removes
        // untracked).
        assert!(
            !ws.join("src/forbidden.rs").exists(),
            "untracked forbidden path must be removed by clean -fd"
        );
        // State must NOT have been updated.
        let state = AuditState::load_or_default(&ws);
        assert!(
            !state.runs.contains_key("csm1"),
            "state must NOT record a violating run"
        );
    }

    #[tokio::test]
    async fn write_policy_canonical_spec_merge_allows_diff_under_specs() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("csm2")
                .with_policy(WritePolicy::CanonicalSpecMerge)
                .writes_file("openspec/specs/cap-a/spec.md")
                .with_outcome(AuditOutcome::Reported(vec![])),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("csm2");
        let repo = fixture_repo();
        run_due_audits(&registry, &ws, &repo, Some(&cfg), &HashMap::new(), None)
            .await
            .unwrap();
        assert!(
            ws.join("openspec/specs/cap-a/spec.md").exists(),
            "openspec/specs/ path must be preserved"
        );
        let state = AuditState::load_or_default(&ws);
        assert!(state.runs.contains_key("csm2"));
    }
}
