//! Shared helper for `OpenSpecOnly` "spec-writing" audits — those that
//! invoke the wrapped agent CLI with a prompt, expect zero or more new
//! `openspec/changes/<name>/` directories to appear, validate each via
//! `openspec validate <name> --strict`, drop ones over the cap, commit
//! the validated set on the agent branch, and return
//! `AuditOutcome::SpecsWritten(validated_names)` so the same iteration's
//! `walk_queue` picks them up.
//!
//! `missing_tests_audit` and `security_bug_audit` differ only in their
//! prompt, their per-run cap source, and their human-readable commit
//! subject — everything else (sandbox shape, snapshot diff, validation,
//! over-cap pruning, commit) is identical. They both delegate to
//! [`run_specs_writing_audit`] so the algorithm lives in one place and
//! cannot drift across audits.

use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

use super::{
    AuditContext, AuditLogWriter, AuditOutcome, build_validation_addendum,
    post_proposal_created_notification, post_validation_exhausted_notification,
    read_proposal_why_first_line, workspace_is_valid, workspace_unavailable_outcome,
};
use crate::config::ResolvedSandbox;

/// Tools every spec-writing audit allows. `Write` and `Edit` are needed
/// because the agent's whole job is to create OpenSpec change files;
/// the framework's post-hoc `OpenSpecOnly` check catches writes outside
/// `openspec/changes/`.
pub(crate) const ALLOWED_TOOLS: &[&str] =
    &["Read", "Glob", "Grep", "Bash", "Write", "Edit"];

/// Parameters for one spec-writing audit invocation. Carried as a
/// struct rather than positional args because the list grew long enough
/// that call-sites became hard to read.
pub(crate) struct SpecsWritingAuditParams<'a> {
    /// Stable audit slug. Used as the prefix for every log section name
    /// and as the "audit_type" label inside error messages.
    pub audit_type: &'static str,
    /// Fully resolved prompt body (override or embedded default, with
    /// any placeholder substitutions already applied).
    pub prompt: &'a str,
    /// Hard cap on the number of new change directories committed this
    /// run. Excess directories are deleted post-hoc.
    pub max_proposals: u32,
    /// Wrapped agent CLI binary (typically `claude`).
    pub executor_command: &'a str,
    /// Wall-clock budget for the agent invocation.
    pub executor_timeout_secs: u64,
    /// Resolved sandbox (the helper overrides `allowed_tools` per
    /// [`ALLOWED_TOOLS`] before writing the settings file).
    pub sandbox: &'a ResolvedSandbox,
    /// Override for the directory the per-invocation sandbox-settings
    /// file is written to. `None` means `std::env::temp_dir()`. Tests
    /// pass a per-test TempDir to avoid concurrent name collisions.
    pub settings_dir: Option<&'a Path>,
    /// Override for the `openspec` validation binary. `None` means
    /// `openspec`. Tests point at a shell script so the audit can be
    /// exercised without the real CLI on PATH.
    pub openspec_command: &'a str,
    /// Optional prompt-source label included in the preamble log line
    /// (e.g. the override path or "<embedded default>"). Cosmetic only.
    pub prompt_source: &'a str,
    /// Human-readable subject inserted into the commit message:
    /// `audit: <commit_subject> (N change(s))`.
    pub commit_subject: &'a str,
}

/// Execute one spec-writing audit run. Returns the outcome the framework
/// dispatches on; never panics on agent misbehavior.
///
/// Validation retry semantics: when EVERY new change directory the LLM
/// produced fails `openspec validate <name> --strict`, the LLM is
/// re-invoked with the validation errors appended to the prompt
/// (per [`build_validation_addendum`]). This repeats until either a
/// valid change dir lands OR the
/// [`AuditContext::max_validation_retries`] budget is exhausted. If
/// the LLM produces a mix of valid and invalid change dirs, the
/// existing per-change drop behavior wins (invalid dirs deleted, valid
/// dirs kept, no retry). When the LLM produces zero change dirs the
/// run is a successful "no findings" with `retries_used: 0` (zero
/// proposals is a legitimate outcome, not a validation failure).
pub(crate) async fn run_specs_writing_audit(
    params: SpecsWritingAuditParams<'_>,
    ctx: &mut AuditContext<'_>,
) -> Result<AuditOutcome> {
    let audit_type = params.audit_type;
    // Workspace-validity gate (see `audits-require-valid-workspace`).
    // The spec-writing helpers are the audits that would otherwise call
    // `fs::create_dir_all(<workspace>/openspec/changes/<slug>)` and
    // recreate workspace + openspec/ on a wiped workspace — the very
    // failure mode the gate exists to prevent.
    if !workspace_is_valid(ctx.workspace) {
        return Ok(workspace_unavailable_outcome(
            audit_type,
            ctx.workspace,
            &ctx.repo.url,
        ));
    }

    let max_retries = ctx.max_validation_retries;
    let total_attempts = max_retries.saturating_add(1);

    let mut sandbox = params.sandbox.clone();
    sandbox.allowed_tools = ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect();

    let initial_before: HashSet<String> = snapshot_change_dirs(ctx.workspace);
    let _ = ctx.log_writer.write_section(
        &format!("{audit_type}_preamble"),
        &format!(
            "executor_command: {}\ntimeout_secs: {}\nprompt_source: {}\nmax_proposals_per_run: {}\nmax_validation_retries: {}\nallowed_tools: {}\npre_run_change_dirs: {}",
            params.executor_command,
            params.executor_timeout_secs,
            params.prompt_source,
            params.max_proposals,
            max_retries,
            sandbox.allowed_tools.join(","),
            initial_before.len(),
        ),
    );

    // Per-attempt state: dirs created on the prior attempt that we
    // need to clear before the next LLM call (we ran into validation
    // failures and are retrying).
    let mut prior_attempt_dirs: Vec<String> = Vec::new();
    // The validation error from the most recent failed attempt, fed
    // to the LLM as a prompt addendum on the next attempt.
    let mut last_addendum_body: Option<String> = None;

    for attempt in 0..total_attempts {
        // Clean up dirs produced by the prior failed attempt so they do
        // not pollute this attempt's diff.
        for name in &prior_attempt_dirs {
            let path = ctx.workspace.join("openspec/changes").join(name);
            let _ = std::fs::remove_dir_all(&path);
        }
        prior_attempt_dirs.clear();

        let before: HashSet<String> = snapshot_change_dirs(ctx.workspace);

        let effective_prompt = match &last_addendum_body {
            None => params.prompt.to_string(),
            Some(err) => format!(
                "{}\n\n{}",
                params.prompt,
                build_validation_addendum(err)
            ),
        };

        let _ = ctx.log_writer.write_section(
            &format!("{audit_type}_prompt_attempt_{attempt}"),
            &effective_prompt,
        );

        let outcome = super::run_audit_cli(
            params.executor_command,
            &sandbox,
            ctx.workspace,
            &effective_prompt,
            Duration::from_secs(params.executor_timeout_secs),
            params.settings_dir,
        )
        .await
        .with_context(|| format!("spawning {audit_type} CLI subprocess"))?;

        let _ = ctx.log_writer.write_section(
            &format!("{audit_type}_stdout_attempt_{attempt}"),
            if outcome.stdout.is_empty() {
                "(empty)"
            } else {
                outcome.stdout.as_str()
            },
        );
        let _ = ctx.log_writer.write_section(
            &format!("{audit_type}_stderr_attempt_{attempt}"),
            if outcome.stderr.is_empty() {
                "(empty)"
            } else {
                outcome.stderr.as_str()
            },
        );

        if let Some(err) = outcome_to_terminal_err(
            &outcome,
            &mut ctx.log_writer,
            audit_type,
            params.executor_timeout_secs,
        ) {
            return Err(err);
        }

        let after = snapshot_change_dirs(ctx.workspace);
        let mut new_dirs: Vec<String> = after.difference(&before).cloned().collect();
        new_dirs.sort();

        let cap = params.max_proposals as usize;
        if new_dirs.len() > cap {
            let dropped: Vec<String> = new_dirs.split_off(cap);
            for d in &dropped {
                let path = ctx.workspace.join("openspec/changes").join(d);
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(
                        url = %ctx.repo.url,
                        audit_type = audit_type,
                        path = %path.display(),
                        "failed to remove over-cap change dir: {e}"
                    );
                }
            }
            let _ = ctx.log_writer.write_section(
                &format!("{audit_type}_dropped_over_cap"),
                &format!(
                    "cap: {}\ndropped:\n{}",
                    params.max_proposals,
                    dropped.join("\n")
                ),
            );
        }

        if new_dirs.is_empty() {
            // Zero proposals — legitimate "no findings", not a
            // validation failure. Exit the retry loop with empty list.
            let _ = ctx.log_writer.write_section(
                &format!("{audit_type}_outcome"),
                &format!("kind: SpecsWritten\nvalidated_count: 0\nretries_used: {attempt}"),
            );
            return Ok(AuditOutcome::SpecsWritten {
                changes: Vec::new(),
                retries_used: attempt,
            });
        }

        let mut validated: Vec<String> = Vec::new();
        let mut failures: Vec<(String, String)> = Vec::new();
        for name in &new_dirs {
            match validate_change(params.openspec_command, ctx.workspace, name).await {
                Ok(()) => validated.push(name.clone()),
                Err(e) => failures.push((name.clone(), format!("{e:#}"))),
            }
        }

        // Log every per-change validation failure so operators can audit
        // exactly what the LLM produced and why.
        for (name, err) in &failures {
            let _ = ctx.log_writer.write_section(
                &format!("{audit_type}_validation_failure_{name}_attempt_{attempt}"),
                &format!("change: {name}\nattempt: {attempt}\nerror: {err}"),
            );
            tracing::warn!(
                url = %ctx.repo.url,
                audit_type = audit_type,
                change = %name,
                attempt = attempt,
                "rejecting agent-produced change that failed `openspec validate --strict`: {err}"
            );
        }

        if !validated.is_empty() {
            // Mixed run: keep valid, drop invalid, commit, return.
            for (name, _) in &failures {
                let path = ctx.workspace.join("openspec/changes").join(name);
                if let Err(rm_err) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(
                        url = %ctx.repo.url,
                        audit_type = audit_type,
                        path = %path.display(),
                        "failed to remove invalid change dir: {rm_err}"
                    );
                }
            }
            // `🔍 created proposal` notification (per
            // `a02-audit-proposal-created-notification`). Fires AFTER
            // per-change validation succeeds AND BEFORE the git commit
            // that ships the proposal, so operators see the audit's
            // signal in the channel ahead of the implementer's
            // `🚀 starting work on …` message on the next iteration.
            // One notification per validated change; failures are
            // logged inside the helper and do not affect the audit's
            // `SpecsWritten` outcome.
            for name in &validated {
                let why_excerpt = read_proposal_why_first_line(ctx.workspace, name);
                post_proposal_created_notification(
                    ctx.chatops_ctx,
                    &ctx.repo.url,
                    audit_type,
                    name,
                    &why_excerpt,
                    attempt,
                    max_retries,
                )
                .await;
            }
            git_add_openspec_changes(ctx.workspace)
                .with_context(|| format!("staging {audit_type}'s openspec/changes/ for commit"))?;
            let commit_msg = format!(
                "audit: {} ({} change(s))",
                params.commit_subject,
                validated.len()
            );
            crate::git::commit(ctx.workspace, &commit_msg).with_context(|| {
                format!(
                    "committing {audit_type}'s {} change(s)",
                    validated.len()
                )
            })?;

            let _ = ctx.log_writer.write_section(
                &format!("{audit_type}_outcome"),
                &format!(
                    "kind: SpecsWritten\nvalidated_count: {}\nretries_used: {attempt}\nchanges:\n{}",
                    validated.len(),
                    validated.join("\n")
                ),
            );

            return Ok(AuditOutcome::SpecsWritten {
                changes: validated,
                retries_used: attempt,
            });
        }

        // All produced dirs failed validation. Drop them and either
        // retry (if budget remains) or return ValidationExhausted.
        prior_attempt_dirs = failures.iter().map(|(n, _)| n.clone()).collect();
        let combined_err = failures
            .iter()
            .map(|(n, e)| format!("{n}: {e}"))
            .collect::<Vec<_>>()
            .join("\n");

        if attempt + 1 < total_attempts {
            // Retry. Stash the combined error as the addendum for the
            // next LLM call. The dirs get deleted at the top of the
            // next iteration.
            last_addendum_body = Some(combined_err);
            continue;
        }

        // Exhausted budget. Clean up and notify.
        for name in &prior_attempt_dirs {
            let path = ctx.workspace.join("openspec/changes").join(name);
            let _ = std::fs::remove_dir_all(&path);
        }
        prior_attempt_dirs.clear();
        let _ = ctx.log_writer.write_section(
            &format!("{audit_type}_outcome"),
            &format!(
                "kind: ValidationExhausted\nretries_attempted: {attempt}\nfinal_error:\n{combined_err}"
            ),
        );

        // Post the chatops `❌` notification directly so the helper's
        // single-slug directory cleanup does not race with our multi-
        // dir cleanup above. Multi-line / long errors route through the
        // threaded notification path; short single-line errors continue
        // to use the inline single-message form.
        if let Some(chat_ctx) = ctx.chatops_ctx
            && let Err(e) = post_validation_exhausted_notification(
                chat_ctx,
                &ctx.repo.url,
                audit_type,
                attempt,
                &combined_err,
            )
            .await
        {
            tracing::warn!(
                url = %ctx.repo.url,
                audit_type = audit_type,
                "validation-exhausted chatops post failed: {e:#}"
            );
        }

        return Ok(AuditOutcome::ValidationExhausted {
            audit_type: audit_type.to_string(),
            retries_attempted: attempt,
            final_error: combined_err,
        });
    }

    // Loop always returns inside; this is unreachable in practice but
    // makes the function total without a panic.
    unreachable!(
        "specs-writing retry loop must return from inside; max_retries was {max_retries}"
    )
}


/// Enumerate the immediate child directory names under
/// `<workspace>/openspec/changes/`. Returns an empty set if the
/// directory is absent (fresh repo with no changes yet). The `archive/`
/// subdirectory is filtered out so archived changes never count as
/// newly created.
pub(crate) fn snapshot_change_dirs(workspace: &Path) -> HashSet<String> {
    let changes = workspace.join("openspec/changes");
    let Ok(entries) = std::fs::read_dir(&changes) else {
        return HashSet::new();
    };
    let mut out = HashSet::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if name == "archive" {
                continue;
            }
            out.insert(name.to_string());
        }
    }
    out
}

async fn validate_change(
    openspec_command: &str,
    workspace: &Path,
    change_name: &str,
) -> Result<()> {
    let output = Command::new(openspec_command)
        .arg("validate")
        .arg(change_name)
        .arg("--strict")
        .current_dir(workspace)
        .output()
        .await
        .with_context(|| {
            format!("spawning `{openspec_command} validate {change_name} --strict`")
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr_tail: String = String::from_utf8_lossy(&output.stderr)
        .chars()
        .take(400)
        .collect();
    Err(anyhow!(
        "`{openspec_command} validate {change_name} --strict` exited {status}; stderr: {stderr_tail}",
        status = output.status,
    ))
}

fn git_add_openspec_changes(workspace: &Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .arg("add")
        .arg("openspec/changes/")
        .current_dir(workspace)
        .status()
        .context("spawning `git add openspec/changes/`")?;
    if !status.success() {
        return Err(anyhow!("`git add openspec/changes/` exited {status}"));
    }
    Ok(())
}

/// Pure transformation: given an [`crate::agentic_run::AgenticRunOutcome`],
/// return Some(error) if the outcome is terminal (timed out OR non-zero
/// exit). Returns None when the caller should continue processing. Mirrors
/// the same-named helpers in the `architecture_consultative` and `drift`
/// audit modules.
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
        return Some(anyhow!("{audit_type}: CLI exited {status}"));
    }
    None
}

#[cfg(test)]
mod outcome_tests {
    use super::*;
    use crate::audits::AuditLogWriter;
    use tempfile::TempDir;

    fn make_log_writer(workspace: &std::path::Path) -> AuditLogWriter {
        let (td, paths) = crate::testing::test_daemon_paths();
        std::mem::forget(td);
        AuditLogWriter::open(&paths, workspace, "test_audit").expect("log writer opens")
    }

    /// Pure-data test: feed a synthesized `AgenticRunOutcome` with
    /// `timed_out: true` directly into `outcome_to_terminal_err` and
    /// assert the resulting error + log entries. No subprocess, no
    /// timer, no race — verifies the audit framework's translation
    /// logic, which is what we actually care about. Replaces the
    /// per-audit "spawn a real subprocess and time it out" tests
    /// that were race-prone across platforms.
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
        let err = outcome_to_terminal_err(&outcome, &mut log_writer, "missing_tests_audit", 1)
            .expect("timed_out outcome must produce Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing_tests_audit"));
        assert!(msg.contains("timeout"));
        let log = std::fs::read_to_string(&log_path).expect("log readable");
        assert!(log.contains("kind: Err"));
        assert!(log.contains("reason: timeout"));
    }

    #[test]
    fn outcome_to_terminal_err_translates_nonzero_exit_to_error() {
        use std::os::unix::process::ExitStatusExt;
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let mut log_writer = make_log_writer(workspace);
        let outcome = crate::agentic_run::AgenticRunOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(7 << 8)),
            stdout: String::new(),
            stderr: "boom".into(),
            ..Default::default()
        };
        let err = outcome_to_terminal_err(&outcome, &mut log_writer, "missing_tests_audit", 30)
            .expect("nonzero exit must produce Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("exit"));
    }

    #[test]
    fn outcome_to_terminal_err_returns_none_for_clean_outcome() {
        use std::os::unix::process::ExitStatusExt;
        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let mut log_writer = make_log_writer(workspace);
        let outcome = crate::agentic_run::AgenticRunOutcome {
            timed_out: false,
            exit_status: Some(std::process::ExitStatus::from_raw(0)),
            stdout: String::new(),
            stderr: String::new(),
            ..Default::default()
        };
        assert!(
            outcome_to_terminal_err(&outcome, &mut log_writer, "missing_tests_audit", 30).is_none()
        );
    }
}

