//! `spec_sync_audit` — merge ADDED/MODIFIED/REMOVED/RENAMED requirements
//! from archived changes' `specs/<capability>/spec.md` files into the
//! canonical `openspec/specs/<capability>/spec.md` files.
//!
//! Exists because OpenSpec 0.18+ split the sync step out of `openspec
//! archive` into a `/opsx:sync` skill that the core profile doesn't
//! install; without this audit, drift accumulates silently every time
//! an archive operation runs. See [`crate::spec_sync`] for the pure-
//! data merge logic; this module just wires it into the audit framework.
//!
//! `requires_head_change = false` — drift can be present at audit
//! registration time on a brand-new workspace, so a HEAD-change gate
//! would mis-skip the first run on a freshly-cloned repo.
//! `WritePolicy::CanonicalSpecMerge` — writes constrained to
//! `openspec/specs/**`.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::path::Path;

use super::{Audit, AuditContext, AuditOutcome, Finding, Severity, WritePolicy};
use crate::spec_sync;

pub struct SpecSyncAudit;

impl SpecSyncAudit {
    pub const TYPE: &'static str = "spec_sync_audit";

    pub fn new() -> Self {
        Self
    }
}

impl Default for SpecSyncAudit {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Audit for SpecSyncAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn requires_head_change(&self) -> bool {
        false
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::CanonicalSpecMerge
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        let plan = spec_sync::compute_sync_plan(ctx.workspace)
            .context("spec_sync_audit: computing sync plan")?;
        let _ = ctx.log_writer.write_section(
            "spec_sync_plan",
            &format!(
                "capabilities_in_plan: {}\ncapabilities:\n{}",
                plan.per_capability.len(),
                plan.per_capability
                    .iter()
                    .map(|(name, cp)| format!(
                        "  - {name}: {} delta(s) -> {}",
                        cp.deltas.len(),
                        cp.canonical_path.display()
                    ))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        );

        let (changed_paths, report) = spec_sync::apply_sync_plan(&plan)
            .context("spec_sync_audit: applying sync plan")?;

        // Log every WARN-level merge note. The chatops Finding gets a
        // condensed summary; the per-invocation log retains the full
        // list.
        for warn in &report.warnings {
            tracing::warn!(audit_type = Self::TYPE, "spec_sync warning: {warn}");
        }
        let _ = ctx.log_writer.write_section(
            "spec_sync_merge_report",
            &format!(
                "added: {a}\nmodified: {m}\nremoved: {r}\nrenamed: {n}\nwarnings ({w}):\n{warns}\nfiles_written ({f}):\n{files}",
                a = report.added,
                m = report.modified,
                r = report.removed,
                n = report.renamed,
                w = report.warnings.len(),
                warns = if report.warnings.is_empty() {
                    "  (none)".to_string()
                } else {
                    report
                        .warnings
                        .iter()
                        .map(|s| format!("  - {s}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
                f = changed_paths.len(),
                files = if changed_paths.is_empty() {
                    "  (none)".to_string()
                } else {
                    changed_paths
                        .iter()
                        .map(|p| format!("  - {}", p.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            ),
        );

        if changed_paths.is_empty() {
            // Idempotent path: no drift detected.
            return Ok(AuditOutcome::Reported(Vec::new()));
        }

        // Distinct archive names contributing to the merge. Used in the
        // commit subject so the operator sees "merge deltas from N
        // archived change(s)" matching the actual archive count, not
        // the per-capability delta count (which double-counts when one
        // archive touches multiple capabilities).
        let archive_names: std::collections::BTreeSet<&str> = plan
            .per_capability
            .values()
            .flat_map(|cp| cp.deltas.iter().map(|(name, _)| name.as_str()))
            .collect();
        let archive_count = archive_names.len();

        // Drift detected — commit the result so the iteration's
        // existing push/PR flow ships it. The post-hoc
        // `CanonicalSpecMerge` policy check sees a clean diff (because
        // we committed) and passes; the policy's defense-in-depth value
        // is for the abnormal case where the merge accidentally wrote
        // outside `openspec/specs/`, which it would have left
        // uncommitted.
        commit_merged_specs(ctx.workspace, archive_count).with_context(|| {
            format!(
                "spec_sync_audit: committing {} merged canonical spec(s)",
                changed_paths.len()
            )
        })?;

        let finding = Finding {
            severity: Severity::Low,
            subject: format!(
                "spec-sync merged {} canonical spec file(s) (added {}, modified {}, removed {}, renamed {})",
                changed_paths.len(),
                report.added,
                report.modified,
                report.removed,
                report.renamed,
            ),
            body: format!(
                "files:\n{}\nwarnings: {}",
                changed_paths
                    .iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                report.warnings.len(),
            ),
            anchor: None,
        };
        Ok(AuditOutcome::Reported(vec![finding]))
    }
}

/// `git add openspec/specs && git commit -m "..."`. Staging only the
/// allowed subtree closes the door on the audit accidentally sweeping
/// up some unrelated dirty file via `git add -A`.
fn commit_merged_specs(workspace: &Path, archive_count: usize) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["add", "openspec/specs"])
        .current_dir(workspace)
        .status()
        .context("spawning `git add openspec/specs`")?;
    if !status.success() {
        return Err(anyhow!("`git add openspec/specs` exited {status}"));
    }
    let subject = format!(
        "audit: spec-sync — merge deltas from {archive_count} archived change(s)"
    );
    crate::git::commit(workspace, &subject)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audits::{AuditLogWriter, AuditOutcome};
    use crate::config::RepositoryConfig;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

    fn run_git(path: &Path, args: &[&str]) {
        let st = Command::new("git").args(args).current_dir(path).status().unwrap();
        assert!(st.success(), "git {args:?} failed");
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

    fn write(p: &Path, contents: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, contents).unwrap();
    }

    fn make_ctx<'a>(workspace: &'a Path, repo: &'a RepositoryConfig) -> AuditContext<'a> {
        AuditContext {
            workspace,
            repo,
            chatops_ctx: None,
            log_writer: AuditLogWriter::open(workspace, SpecSyncAudit::TYPE).unwrap(),
        }
    }

    #[tokio::test]
    async fn audit_no_drift_returns_empty_findings_and_no_commit() {
        let (_t, ws) = init_workspace();
        let repo = fixture_repo();
        // Canonical already has the requirement; archive has the same
        // delta. No drift.
        write(
            &ws.join("openspec/specs/cap/spec.md"),
            "# cap Specification\n\n## Purpose\nP.\n\n## Requirements\n\n### Requirement: One\nbody one.\n",
        );
        write(
            &ws.join("openspec/changes/archive/2026-01-01-x/specs/cap/spec.md"),
            "## ADDED Requirements\n\n### Requirement: One\nbody one.\n",
        );
        run_git(&ws, &["add", "-A"]);
        run_git(&ws, &["commit", "-q", "-m", "fixture"]);
        let head_before = crate::git::rev_parse(&ws, "HEAD").unwrap();

        let audit = SpecSyncAudit::new();
        let mut ctx = make_ctx(&ws, &repo);
        let outcome = audit.run(&mut ctx).await.expect("ok");
        match outcome {
            AuditOutcome::Reported(findings) => assert!(findings.is_empty()),
            other => panic!("expected Reported([]), got {other:?}"),
        }
        let head_after = crate::git::rev_parse(&ws, "HEAD").unwrap();
        assert_eq!(
            head_before, head_after,
            "no-drift run must NOT create a commit"
        );
        // Workspace must be clean too (no untracked or modified files).
        let porcelain = crate::git::status_porcelain(&ws).unwrap();
        assert!(porcelain.is_empty(), "expected clean: {porcelain}");
    }

    #[tokio::test]
    async fn audit_backfills_existing_drift_writes_canonical_and_commits() {
        let (_t, ws) = init_workspace();
        let repo = fixture_repo();
        // Canonical is empty; two archives each add one requirement.
        write(
            &ws.join("openspec/specs/cap/spec.md"),
            "# cap Specification\n\n## Purpose\nP.\n\n## Requirements\n",
        );
        write(
            &ws.join("openspec/changes/archive/2026-01-01-first/specs/cap/spec.md"),
            "## ADDED Requirements\n\n### Requirement: One\nbody one.\n",
        );
        write(
            &ws.join("openspec/changes/archive/2026-02-01-second/specs/cap/spec.md"),
            "## ADDED Requirements\n\n### Requirement: Two\nbody two.\n",
        );
        run_git(&ws, &["add", "-A"]);
        run_git(&ws, &["commit", "-q", "-m", "fixture"]);
        let head_before = crate::git::rev_parse(&ws, "HEAD").unwrap();

        let audit = SpecSyncAudit::new();
        let mut ctx = make_ctx(&ws, &repo);
        let outcome = audit.run(&mut ctx).await.expect("ok");
        match outcome {
            AuditOutcome::Reported(findings) => {
                assert_eq!(findings.len(), 1);
                assert!(findings[0].subject.contains("merged 1"));
            }
            other => panic!("expected single Reported finding, got {other:?}"),
        }

        let head_after = crate::git::rev_parse(&ws, "HEAD").unwrap();
        assert_ne!(head_before, head_after, "drift run must create a commit");

        // Workspace clean after the audit's own commit.
        let porcelain = crate::git::status_porcelain(&ws).unwrap();
        assert!(porcelain.is_empty(), "expected clean: {porcelain}");

        // Canonical now contains both requirements in chronological order.
        let canonical_body =
            std::fs::read_to_string(ws.join("openspec/specs/cap/spec.md")).unwrap();
        let p_one = canonical_body.find("### Requirement: One").expect("One present");
        let p_two = canonical_body.find("### Requirement: Two").expect("Two present");
        assert!(p_one < p_two);

        // Commit subject matches the spec-required prefix.
        let log = Command::new("git")
            .args(["log", "-1", "--pretty=%s"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let subject = String::from_utf8_lossy(&log.stdout).trim().to_string();
        assert!(
            subject.contains("spec-sync") && subject.contains("merge deltas from"),
            "commit subject must follow spec format; got: {subject}"
        );
    }

    #[tokio::test]
    async fn audit_idempotent_across_repeated_invocations() {
        let (_t, ws) = init_workspace();
        let repo = fixture_repo();
        write(
            &ws.join("openspec/specs/cap/spec.md"),
            "# cap Specification\n\n## Purpose\nP.\n\n## Requirements\n",
        );
        write(
            &ws.join("openspec/changes/archive/2026-01-01-only/specs/cap/spec.md"),
            "## ADDED Requirements\n\n### Requirement: One\nbody one.\n",
        );
        run_git(&ws, &["add", "-A"]);
        run_git(&ws, &["commit", "-q", "-m", "fixture"]);

        let audit = SpecSyncAudit::new();

        // First run: drift detected, commit produced.
        {
            let mut ctx = make_ctx(&ws, &repo);
            let outcome = audit.run(&mut ctx).await.expect("ok");
            assert!(matches!(outcome, AuditOutcome::Reported(ref f) if !f.is_empty()));
        }
        let head_after_first = crate::git::rev_parse(&ws, "HEAD").unwrap();

        // Second run: no drift, no commit.
        {
            let mut ctx = make_ctx(&ws, &repo);
            let outcome = audit.run(&mut ctx).await.expect("ok");
            assert!(matches!(outcome, AuditOutcome::Reported(ref f) if f.is_empty()));
        }
        let head_after_second = crate::git::rev_parse(&ws, "HEAD").unwrap();
        assert_eq!(
            head_after_first, head_after_second,
            "second run must not create a commit"
        );

        // Third run: still no drift.
        {
            let mut ctx = make_ctx(&ws, &repo);
            let outcome = audit.run(&mut ctx).await.expect("ok");
            assert!(matches!(outcome, AuditOutcome::Reported(ref f) if f.is_empty()));
        }
        let head_after_third = crate::git::rev_parse(&ws, "HEAD").unwrap();
        assert_eq!(head_after_second, head_after_third);
    }

    #[tokio::test]
    async fn audit_creates_missing_capability_canonical() {
        let (_t, ws) = init_workspace();
        let repo = fixture_repo();
        // No canonical at all; archive references a new capability.
        write(
            &ws.join("openspec/changes/archive/2026-01-01-x/specs/new-cap/spec.md"),
            "## ADDED Requirements\n\n### Requirement: One\nbody.\n",
        );
        run_git(&ws, &["add", "-A"]);
        run_git(&ws, &["commit", "-q", "-m", "fixture"]);

        let audit = SpecSyncAudit::new();
        let mut ctx = make_ctx(&ws, &repo);
        let outcome = audit.run(&mut ctx).await.expect("ok");
        assert!(matches!(outcome, AuditOutcome::Reported(ref f) if f.len() == 1));
        let canonical = ws.join("openspec/specs/new-cap/spec.md");
        assert!(canonical.is_file(), "canonical must be auto-created");
        let body = std::fs::read_to_string(&canonical).unwrap();
        assert!(body.contains("## Requirements"));
        assert!(body.contains("### Requirement: One"));
    }
}
