//! Periodic-audit scheduler. Invoked per polling iteration AFTER the
//! pending queue walk completes AND BEFORE the push+PR step. An audit
//! that writes new OpenSpec changes does NOT feed this iteration's
//! queue walk (it already completed); the new pending changes are
//! picked up by the NEXT iteration's `list_pending`.
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
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::state::{AttemptEntry, AuditRunEntry, AuditState};
use super::threads::{
    self, AuditThreadState, AuditThreadStatus, cap_findings_excerpt, default_state_root,
    write_state,
};
use super::{
    Audit, AuditContext, AuditLogWriter, AuditOutcome, AuditRegistry, Finding, Severity,
    VALIDATION_ERROR_HISTORY_EXCERPT, WritePolicy, format_audit_notification, truncate_chars,
};
use crate::alert_state::AlertCategory;
use crate::alerts::handle_predictable_failure;
use crate::config::{
    AuditSettings, AuditsConfig, RepositoryConfig, default_max_audits_per_iteration,
    default_max_validation_retries, resolved_cadence,
};
use crate::polling_loop::ChatOpsContext;
use crate::{git, workspace};

/// Iterate the registry, run every due audit, and enforce write
/// policies. Failures inside one audit do NOT abort the iteration —
/// the scheduler logs the error, leaves the state file unchanged for
/// that audit, and moves on.
///
/// `queued_audit_types` carries any audit-type names that an operator
/// queued for on-demand execution via the chatops `audit` verb or the
/// CLI `audit run` subcommand. Queued audits run BEFORE the
/// cadence-driven sweep and bypass the cadence check entirely (they run
/// regardless of `last_run` and `requires_head_change`). A queued name
/// that is not registered is logged and skipped. After running a queued
/// audit, the scheduler updates its `last_run` state as if it were a
/// cadence-driven run, then proceeds to the normal cadence sweep —
/// skipping any audit type already run via the queue this iteration so
/// the same audit cannot run twice in one pass.
pub async fn run_due_audits(
    paths: &crate::paths::DaemonPaths,
    registry: &AuditRegistry,
    workspace: &Path,
    repo: &RepositoryConfig,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
    chatops_ctx: Option<&ChatOpsContext>,
    queued_audit_types: &HashSet<String>,
) -> Result<()> {
    if registry.is_empty() {
        return Ok(());
    }
    let mut state = AuditState::load_or_default(workspace);

    // Per-iteration bound. Both queued AND cadence-driven runs count
    // against the same counter so a flood of on-demand audits doesn't
    // bypass the storm-prevention guard. `0` disables audits behaviourally
    // (every iteration skips the audit phase). When the operator did
    // not configure the field (or did not configure an `audits:` block at
    // all), the default lives in `default_max_audits_per_iteration()`.
    let bound = audits_cfg
        .map(|c| c.max_audits_per_iteration)
        .unwrap_or_else(default_max_audits_per_iteration);
    let mut audits_run_this_iteration: usize = 0;

    if bound == 0 {
        // Operator explicitly disabled all audit runs this iteration —
        // skip both the queued drain AND the cadence sweep. Queued
        // entries remain in the queue for a later iteration (drained
        // is owned by the caller).
        return Ok(());
    }

    // 1. Run queued audits first (unconditional — bypass cadence). Each
    //    run increments the counter; once the bound is reached the
    //    remaining queued entries defer to the next iteration.
    let mut ran_via_queue: HashSet<String> = HashSet::new();
    if !queued_audit_types.is_empty() {
        for audit in registry.iter() {
            if audits_run_this_iteration >= bound {
                break;
            }
            let name = audit.audit_type();
            if !queued_audit_types.contains(name) {
                continue;
            }
            match run_one_audit_unconditional(
                paths,
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
                Ok(ran) => {
                    if ran {
                        audits_run_this_iteration += 1;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        url = %repo.url,
                        audit_type = name,
                        "queued audit `{name}` failed (iteration continues): {e:#}"
                    );
                    // A failure inside `run()` still consumed an
                    // attempt; count it against the bound so a flaky
                    // audit can't monopolise the iteration.
                    audits_run_this_iteration += 1;
                }
            }
            ran_via_queue.insert(name.to_string());
        }
        // Log any queued names that didn't match a registered audit so
        // an operator typo doesn't disappear silently. Names that DID
        // match but were deferred because the bound was reached are NOT
        // logged here — they remain in the queue for next iteration.
        for q in queued_audit_types {
            if !ran_via_queue.contains(q)
                && !registry.iter().any(|a| a.audit_type() == q)
            {
                tracing::warn!(
                    url = %repo.url,
                    "queued audit `{q}` is not a registered audit type; skipping"
                );
            }
        }
    }

    // 2. Cadence-driven sweep. Skip anything already run via the queue
    //    AND stop once the bound is reached.
    for audit in registry.iter() {
        if audits_run_this_iteration >= bound {
            break;
        }
        if ran_via_queue.contains(audit.audit_type()) {
            continue;
        }
        match run_one_audit(
            paths,
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
            Ok(ran) => {
                if ran {
                    audits_run_this_iteration += 1;
                }
            }
            Err(e) => {
                // Per spec: log and continue. State is not updated for
                // this audit (the inner helper only writes state on
                // success). The audit DID consume an attempt though,
                // so it counts against the bound.
                tracing::error!(
                    url = %repo.url,
                    audit_type = audit.audit_type(),
                    "audit `{}` failed (this iteration's other audits continue): {e:#}",
                    audit.audit_type()
                );
                audits_run_this_iteration += 1;
            }
        }
    }
    Ok(())
}

/// Wrapper for the cadence-driven path: returns `Ok(true)` if the audit
/// consumed a per-iteration slot (i.e. its `run()` was invoked), `Ok(false)`
/// if it was skipped before invocation (cadence not elapsed, HEAD
/// unchanged, etc.), and propagates errors from the audit's `run()` for
/// the caller to log-and-continue.
async fn run_one_audit(
    paths: &crate::paths::DaemonPaths,
    audit: &dyn Audit,
    workspace: &Path,
    repo: &RepositoryConfig,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
    chatops_ctx: Option<&ChatOpsContext>,
    state: &mut AuditState,
) -> Result<bool> {
    drive_one_audit(
        paths,
        audit,
        workspace,
        repo,
        audits_cfg,
        audit_settings,
        chatops_ctx,
        state,
        /*bypass_cadence=*/ false,
    )
    .await
}

/// Wrapper for the on-demand path (chatops `audit` verb / CLI `audit
/// run`): skips the cadence + `requires_head_change` gates and runs the
/// audit unconditionally. State is still updated on success so the
/// audit's cadence clock moves forward as if it were a cadence-driven
/// run (per the proposal's cadence-interaction rule: an on-demand run
/// shifts the next scheduled fire forward). Returns `Ok(true)` on
/// successful invocation; `Ok(false)` is unreachable on this path
/// because the cadence/HEAD gates are bypassed.
async fn run_one_audit_unconditional(
    paths: &crate::paths::DaemonPaths,
    audit: &dyn Audit,
    workspace: &Path,
    repo: &RepositoryConfig,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
    chatops_ctx: Option<&ChatOpsContext>,
    state: &mut AuditState,
) -> Result<bool> {
    drive_one_audit(
        paths,
        audit,
        workspace,
        repo,
        audits_cfg,
        audit_settings,
        chatops_ctx,
        state,
        /*bypass_cadence=*/ true,
    )
    .await
}

/// Inner per-audit driver. Returns `Ok(true)` once the audit's `run()`
/// has been invoked (regardless of outcome — completed-successfully,
/// violation-handled, validation-exhausted, etc.); returns `Ok(false)`
/// when the audit was skipped before invocation (cadence not elapsed,
/// HEAD unchanged when `requires_head_change` is set). Returns `Err`
/// only when the audit's `run()` itself errored (the caller logs and
/// moves on); the call still consumed an attempt.
///
/// `bypass_cadence` skips the cadence + `requires_head_change` gate at
/// the top of the function (used by the on-demand audit-trigger path).
#[allow(clippy::too_many_arguments)]
async fn drive_one_audit(
    paths: &crate::paths::DaemonPaths,
    audit: &dyn Audit,
    workspace: &Path,
    repo: &RepositoryConfig,
    audits_cfg: Option<&AuditsConfig>,
    audit_settings: &HashMap<String, AuditSettings>,
    chatops_ctx: Option<&ChatOpsContext>,
    state: &mut AuditState,
    bypass_cadence: bool,
) -> Result<bool> {
    let audit_type = audit.audit_type();
    let cadence = resolved_cadence(repo, audits_cfg, audit_type);
    // When bypassing cadence, accept a Disabled config (queued runs are
    // independent of the cadence machinery, per the proposal's
    // "audits configured with `cadence: disabled` can now be triggered
    // on-demand" note). When NOT bypassing, Disabled → skip.
    if !bypass_cadence && cadence.interval().is_none() {
        tracing::debug!(audit_type, "audit cadence is Disabled; skipping");
        return Ok(false);
    }

    let now = Utc::now();
    let current_head_sha = git::rev_parse(workspace, &repo.base_branch).ok();

    if !bypass_cadence
        && let Some(interval) = cadence.interval()
        && let Some(prior) = state.runs.get(audit_type)
    {
        let next_due = prior.last_run_at + interval;
        if now < next_due {
            tracing::debug!(
                audit_type,
                "audit not yet due (last run {}, next due {})",
                prior.last_run_at,
                next_due
            );
            return Ok(false);
        }
        if audit.requires_head_change()
            && let (Some(prior_sha), Some(current_sha)) =
                (prior.last_run_sha.as_ref(), current_head_sha.as_ref())
            && prior_sha == current_sha
        {
            tracing::debug!(
                audit_type,
                sha = %current_sha,
                "audit requires HEAD change but HEAD unchanged since last run; skipping"
            );
            return Ok(false);
        }
    }

    let log_writer = AuditLogWriter::open(paths, workspace, audit_type)?;
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

    let max_validation_retries = audits_cfg
        .map(|c| c.max_validation_retries)
        .unwrap_or_else(default_max_validation_retries);

    let mut ctx = AuditContext {
        workspace,
        repo,
        chatops_ctx,
        log_writer: log_writer.clone(),
        max_validation_retries,
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

    // `WorkspaceUnavailable` is the documented "audit declined to run"
    // outcome (see `audits-require-valid-workspace`). The audit did NO
    // file IO, NO LLM call, and NO state mutation, so there is no
    // post-hoc diff to enforce against the write policy — invoking
    // `git status` against a missing/non-git workspace would fail and
    // mis-report the skip as an audit error. Skip post-hoc enforcement,
    // skip chatops, and do NOT update the cadence-state file so the
    // next iteration's cadence check re-evaluates and may try again
    // if the workspace becomes valid in the meantime.
    if let AuditOutcome::WorkspaceUnavailable {
        audit_type: at,
        workspace_path,
        reason,
    } = &outcome
    {
        log_writer.write_section(
            "audit_run_outcome",
            &format!(
                "kind: WorkspaceUnavailable\nend: {}\naudit_type: {at}\nworkspace_path: {wp}\nreason: {reason}",
                end_ts.to_rfc3339(),
                wp = workspace_path.display(),
            ),
        )?;
        // `WorkspaceUnavailable` is an "audit declined to run" skip — the
        // audit did NO file IO, NO LLM call, and NO state mutation. Treat
        // it like a cadence skip for accounting purposes so a fleet-wide
        // workspace problem doesn't burn the per-iteration bound on every
        // registered audit returning the same skip.
        return Ok(false);
    }

    // Post-hoc write-policy enforcement. Use `-uall` so untracked
    // directories are expanded to per-file paths; otherwise an audit
    // could write `openspec/changes/new-thing/proposal.md` and git
    // would report just `?? openspec/` (when the parent is also new),
    // which would mis-categorize the OpenSpecOnly check.
    let policy = audit.write_policy();
    let entries = match git::status_entries(workspace) {
        Ok(s) => s,
        Err(e) => {
            // Couldn't probe the workspace — treat as a violation so the
            // operator notices, but skip the revert (we have no usable
            // signal). State stays untouched.
            log_writer.write_section(
                "audit_postcheck_failed",
                &format!("git status errored: {e:#}"),
            )?;
            return Err(e);
        }
    };
    if let Some(violation) = detect_write_policy_violation(policy, &entries) {
        // Raw porcelain for the human-readable log section only; the
        // decision above used the structured `status_entries`. Best-effort.
        let porcelain = git::status_porcelain_untracked_all(workspace).unwrap_or_default();
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
            WritePolicy::Approved => {
                // No post-hoc enforcement.
            }
        }
        let alert_err = anyhow::anyhow!(
            "audit `{audit_type}` violated WritePolicy::{policy:?}: {}",
            violation.reason
        );
        handle_predictable_failure(
            paths,
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
        // State NOT updated → cadence re-triggers next iteration. The
        // audit DID consume an attempt though (it ran far enough to
        // violate the write policy), so it counts toward the bound.
        return Ok(true);
    }

    // Outcome dispatch.
    let outcome_kind = outcome.kind();
    let mut history_excerpt: Option<String> = None;
    match &outcome {
        AuditOutcome::NoFindings => {
            log_writer.write_section(
                "audit_run_outcome",
                &format!("kind: NoFindings\nend: {}", end_ts.to_rfc3339()),
            )?;
        }
        AuditOutcome::Reported {
            findings,
            retries_used,
        } => {
            let retry_clause = format_retry_clause(*retries_used, max_validation_retries);
            log_writer.write_section(
                "audit_run_outcome",
                &format!(
                    "kind: Reported{retry_clause}\nend: {}\nfindings_count: {}\nretries_used: {}",
                    end_ts.to_rfc3339(),
                    findings.len(),
                    retries_used,
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
            if *retries_used > 0 {
                tracing::info!(
                    url = %repo.url,
                    audit_type,
                    retries_used = *retries_used,
                    max = max_validation_retries,
                    "audit succeeded (validated on retry {} of {})",
                    retries_used,
                    max_validation_retries,
                );
            }
            let notify_on_clean = audit_settings
                .get(audit_type)
                .map(|s| s.notify_on_clean)
                .unwrap_or(false);
            dispatch_reported_to_chatops(
                paths,
                chatops_ctx,
                &repo.url,
                audit_type,
                findings,
                notify_on_clean,
                *retries_used,
                max_validation_retries,
            )
            .await;
        }
        AuditOutcome::SpecsWritten {
            changes: names,
            retries_used,
        } => {
            let retry_clause = format_retry_clause(*retries_used, max_validation_retries);
            log_writer.write_section(
                "audit_run_outcome",
                &format!(
                    "kind: SpecsWritten{retry_clause}\nend: {}\nretries_used: {}\nspecs:\n{}",
                    end_ts.to_rfc3339(),
                    retries_used,
                    names.join("\n")
                ),
            )?;
            tracing::info!(
                url = %repo.url,
                audit_type,
                count = names.len(),
                retries_used = *retries_used,
                "audit wrote {} new spec(s){}; they will be picked up by the NEXT iteration's list_pending: {}",
                names.len(),
                retry_clause,
                names.join(", "),
            );
        }
        AuditOutcome::ValidationExhausted {
            audit_type: at,
            retries_attempted,
            final_error,
        } => {
            log_writer.write_section(
                "audit_run_outcome",
                &format!(
                    "kind: ValidationExhausted\nend: {}\nretries_attempted: {}\nfinal_error:\n{}",
                    end_ts.to_rfc3339(),
                    retries_attempted,
                    final_error,
                ),
            )?;
            tracing::warn!(
                url = %repo.url,
                audit_type = at.as_str(),
                retries_attempted = *retries_attempted,
                final_error = %final_error,
                "audit `{at}` produced an invalid proposal; discarded after {retries_attempted} retries",
            );
            history_excerpt = Some(truncate_chars(final_error, VALIDATION_ERROR_HISTORY_EXCERPT));
        }
        AuditOutcome::WorkspaceUnavailable { .. } => {
            // Handled by the early-return above. Unreachable here, but
            // the match must remain exhaustive so future additions of
            // outcome variants force a deliberate decision at this site.
            unreachable!(
                "WorkspaceUnavailable is handled by the dedicated early return before outcome dispatch"
            );
        }
    }

    // Persist state. Validation-exhausted runs still update last_run_at
    // so the cadence retriggers naturally on the next due-date rather
    // than burning through retries on every iteration.
    state.record(
        audit_type,
        AuditRunEntry {
            last_run_at: now,
            last_run_sha: current_head_sha,
            last_outcome: outcome_kind,
        },
    );
    state.append_history(
        audit_type,
        AttemptEntry {
            when: now,
            outcome_kind: outcome_kind.as_str().to_string(),
            retries_used: outcome.retries_used(),
            error_excerpt: history_excerpt,
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
                url = %repo.url,
                "could not register .audit-state.json in .git/info/exclude: {e:#}"
            );
        }
    }
    Ok(true)
}

struct PolicyViolation {
    reason: String,
}

/// Inspect the parsed working-tree status entries against the audit's
/// policy. `None` is returned iff the diff is allowed; `Some` describes
/// why. Only each entry's destination `path` is checked (a rename's
/// `orig_path` is not), matching the prior per-line parser.
pub(crate) fn detect_write_policy_violation(
    policy: WritePolicy,
    entries: &[git::StatusEntry],
) -> Option<PolicyViolation> {
    if entries.is_empty() {
        return None;
    }
    match policy {
        WritePolicy::None => Some(PolicyViolation {
            reason: format!(
                "workspace dirty after audit (expected clean): {} entry(ies)",
                entries.len()
            ),
        }),
        WritePolicy::OpenSpecOnly => {
            let bad_paths: Vec<String> = entries
                .iter()
                .map(|e| e.path.clone())
                .filter(|p| !p.starts_with("openspec/changes/"))
                .collect();
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
        WritePolicy::Approved => None,
    }
}

async fn dispatch_reported_to_chatops(
    paths: &crate::paths::DaemonPaths,
    chatops_ctx: Option<&ChatOpsContext>,
    repo_url: &str,
    audit_type: &str,
    findings: &[Finding],
    notify_on_clean: bool,
    retries_used: u32,
    max_validation_retries: u32,
) {
    let Some(ctx) = chatops_ctx else { return };
    if findings.is_empty() && !notify_on_clean {
        // No findings and operator opted out of the clean signal: post
        // nothing. Existing behaviour preserved.
        return;
    }
    let retry_clause = format_retry_clause(retries_used, max_validation_retries);
    let notification =
        format_audit_notification(audit_type, repo_url, findings, notify_on_clean, Utc::now());
    if notification.should_thread {
        let top_line = format!("{}{retry_clause}", notification.top_line);
        match ctx
            .chatops
            .post_notification_with_thread(
                &ctx.channel,
                &top_line,
                &notification.thread_body,
            )
            .await
        {
            Ok(Some(thread_ts)) => {
                // The backend supports threading; record the audit-thread
                // state so the chatops dispatcher can resolve `@<bot> send
                // it` against this thread in the operator's reply path.
                stamp_audit_thread_state(
                    paths,
                    repo_url,
                    audit_type,
                    &ctx.channel,
                    &thread_ts,
                    &notification.thread_body,
                );
            }
            Ok(None) => {
                // Backend without native threading (default impl) —
                // nothing to track. Already logged inline by the
                // backend when the inline-concatenation path runs.
            }
            Err(e) => {
                tracing::warn!(
                    url = %repo_url,
                    audit_type,
                    "audit chatops thread post failed: {e:#}"
                );
            }
        }
    } else {
        // Inline form: top_line + (optional body), then the retry clause
        // appended to the top-line portion so the result keeps the same
        // shape regardless of body presence.
        let text = if notification.thread_body.is_empty() {
            format!("{}{retry_clause}", notification.top_line)
        } else {
            format!(
                "{}{retry_clause}\n{}",
                notification.top_line, notification.thread_body
            )
        };
        if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
            tracing::warn!(
                url = %repo_url,
                audit_type,
                "audit chatops post failed: {e:#}"
            );
        }
    }
}

/// Write the audit-thread state file that the `audit-reply-acts` flow
/// consults when an operator posts `@<bot> send it` in this audit's
/// reply thread. Failures are logged at WARN and never propagated —
/// missing the state is a degradation (the bot's polite-refusal path
/// kicks in) but the audit notification has already landed and the
/// scheduler's job is done.
fn stamp_audit_thread_state(
    paths: &crate::paths::DaemonPaths,
    repo_url: &str,
    audit_type: &str,
    channel: &str,
    thread_ts: &str,
    findings_body: &str,
) {
    let state = AuditThreadState {
        thread_ts: thread_ts.to_string(),
        channel: channel.to_string(),
        repo_url: repo_url.to_string(),
        audit_type: audit_type.to_string(),
        findings_excerpt: cap_findings_excerpt(findings_body),
        posted_at: Utc::now(),
        status: AuditThreadStatus::Open,
        reason: None,
    };
    let root = default_state_root(paths);
    if let Err(e) = write_state(&root, &state) {
        tracing::warn!(
            url = %repo_url,
            audit_type,
            thread_ts = %thread_ts,
            "failed to stamp audit-thread state (audit-reply-acts `send it` will not resolve this thread): {e:#}"
        );
    } else {
        tracing::debug!(
            audit_type,
            thread_ts = %thread_ts,
            "stamped audit-thread state for `send it` resolution"
        );
    }
    // Silence the unused-import warning when `threads` is otherwise
    // unused inside this module — the import is via the helper, but
    // the explicit reference here lets `cargo` notice it.
    let _ = threads::state_dir(&root);
}

/// Render the `" (validated on retry N of M)"` clause appended to the
/// success log line and chatops notification when an audit's generated
/// proposal validated on a retry rather than the first attempt. Returns
/// an empty string when no retries were used (the most common case),
/// so callers can unconditionally append the value.
pub fn format_retry_clause(retries_used: u32, max_validation_retries: u32) -> String {
    if retries_used == 0 {
        String::new()
    } else {
        format!(" (validated on retry {retries_used} of {max_validation_retries})")
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
    use std::sync::{Arc, Mutex, OnceLock};
    use tempfile::TempDir;

    /// One process-wide tempdir-scoped DaemonPaths for tests in this
    /// module. Tests in this file don't share state-dir contents — they
    /// each scope their state files by basename — so a shared paths
    /// object is fine. The tempdir is leaked at process exit.
    fn test_paths() -> &'static crate::paths::DaemonPaths {
        static PATHS: OnceLock<crate::paths::DaemonPaths> = OnceLock::new();
        PATHS.get_or_init(|| {
            let (td, paths) = crate::testing::test_daemon_paths();
            std::mem::forget(td);
            paths
        })
    }

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
        fn description(&self) -> &'static str {
            "counting audit for tests"
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
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
        }
    }

    fn audits_cfg_daily(slug: &str) -> AuditsConfig {
        let mut defaults = HashMap::new();
        defaults.insert(slug.to_string(), Cadence::Daily);
        AuditsConfig {
            defaults,
            settings: HashMap::new(),
            ..AuditsConfig::default()
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
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
        run_due_audits(test_paths(), &registry, &ws, &repo, None, &HashMap::new(), None, &HashSet::new())
            .await
            .unwrap();
        assert_eq!(*counter.lock().unwrap(), 0);
        // Explicit Disabled also never runs.
        let mut defaults = HashMap::new();
        defaults.insert("a5".to_string(), Cadence::Disabled);
        let cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
            ..AuditsConfig::default()
        };
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
                .with_outcome(AuditOutcome::specs_written(vec!["new-thing".into()])),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("a7b");
        let repo = fixture_repo();
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
        // Raise the per-iteration bound above the test's audit count so
        // this test isolates the "failure doesn't abort the loop" property
        // from the storm-prevention bound (see `bound_default_one_*`
        // tests for the default-bound behaviour).
        let cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
            max_audits_per_iteration: 5,
            ..AuditsConfig::default()
        };
        let repo = fixture_repo();
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
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
            CountingAudit::new("rep1").with_outcome(AuditOutcome::reported(findings.clone())),
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
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            Some(&ctx),
            &HashSet::new(),
        )
        .await
        .unwrap();
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn reported_no_findings_silent_unless_notify_on_clean() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("clean1").with_outcome(AuditOutcome::reported(vec![])),
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
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            Some(&ctx),
            &HashSet::new(),
        )
        .await
        .unwrap();
        silent_mock.assert_async().await;

        // With notify_on_clean: should post.
        // Clear state so the audit is due again.
        let _ = std::fs::remove_file(ws.join(".audit-state.json"));
        let audit2 = Arc::new(
            CountingAudit::new("clean2").with_outcome(AuditOutcome::reported(vec![])),
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
            test_paths(),
            &registry2,
            &ws,
            &repo,
            Some(&cfg2),
            &settings,
            Some(&ctx),
            &HashSet::new(),
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
                .with_outcome(AuditOutcome::specs_written(vec!["new-spec".into()])),
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
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            Some(&ctx),
            &HashSet::new(),
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
        // <logs_dir>/runs/<basename>/audits/ path is hermetic.
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
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
            .await
            .unwrap();
        let log_dir = test_paths().audit_logs_dir(&basename);
        assert!(log_dir.exists(), "audit log dir must be created at {}", log_dir.display());
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
        let _ = std::fs::remove_dir_all(test_paths().run_logs_dir(&basename));
    }

    /// When the audit returns `WorkspaceUnavailable`, the scheduler must
    /// NOT update the cadence-state file. The next iteration's cadence
    /// check sees the unchanged timestamp and treats the audit as
    /// still-due (so it retries when the workspace becomes valid).
    #[tokio::test]
    async fn workspace_unavailable_outcome_does_not_update_cadence_state() {
        let (_t, ws) = init_workspace();
        // Seed a state entry from 30 days ago, simulating a long-due
        // audit that has been blocked by a broken workspace state.
        let mut state = AuditState::default();
        let original_last_run = Utc::now() - chrono::Duration::days(30);
        state.record(
            "wsu1",
            AuditRunEntry {
                last_run_at: original_last_run,
                last_run_sha: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();

        let audit = Arc::new(CountingAudit::new("wsu1").with_outcome(
            AuditOutcome::WorkspaceUnavailable {
                audit_type: "wsu1".into(),
                workspace_path: PathBuf::from("/some/missing/path"),
                reason: "workspace directory does not exist".into(),
            },
        ));
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("wsu1");
        let repo = fixture_repo();
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
            .await
            .unwrap();

        // State is unchanged: last_run_at still the original 30-day-old
        // timestamp, last_outcome still `NoFindings` (not overwritten).
        let after = AuditState::load_or_default(&ws);
        let entry = after.runs.get("wsu1").expect("entry must remain");
        assert_eq!(
            entry.last_run_at, original_last_run,
            "skipped runs must NOT advance last_run_at"
        );
        assert_eq!(
            entry.last_outcome,
            AuditOutcomeKind::NoFindings,
            "skipped runs must NOT overwrite the recorded outcome"
        );
        // History must NOT have a new entry either.
        let hist = after.history("wsu1");
        assert!(
            hist.is_empty(),
            "skipped runs must NOT append to attempt history: {hist:?}"
        );
    }

    /// Sibling audits in the same iteration still run when one returns
    /// `WorkspaceUnavailable`. (In practice they'll all hit the same
    /// invalid workspace and all return the same outcome — but the
    /// scheduler loop must not abort the iteration on the first skip.)
    #[tokio::test]
    async fn workspace_unavailable_does_not_abort_iteration() {
        let (_t, ws) = init_workspace();
        let audit_a = Arc::new(CountingAudit::new("wsu_a").with_outcome(
            AuditOutcome::WorkspaceUnavailable {
                audit_type: "wsu_a".into(),
                workspace_path: ws.clone(),
                reason: "workspace directory does not exist".into(),
            },
        ));
        let audit_b = Arc::new(CountingAudit::new("wsu_b"));
        let counter_b = audit_b.invocations.clone();
        let registry =
            AuditRegistry::with_audits(vec![audit_a.clone(), audit_b.clone()]);
        let mut defaults = HashMap::new();
        defaults.insert("wsu_a".to_string(), Cadence::Daily);
        defaults.insert("wsu_b".to_string(), Cadence::Daily);
        let cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
            ..AuditsConfig::default()
        };
        let repo = fixture_repo();
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
            .await
            .expect("scheduler must keep going");
        assert_eq!(
            *counter_b.lock().unwrap(),
            1,
            "subsequent audits must still run after a workspace-unavailable skip"
        );
        // wsu_a's state must NOT have been recorded.
        let state = AuditState::load_or_default(&ws);
        assert!(
            !state.runs.contains_key("wsu_a"),
            "WorkspaceUnavailable must NOT consume cadence"
        );
        // wsu_b's state IS recorded (it ran normally).
        assert!(state.runs.contains_key("wsu_b"));
    }

    /// Skipped audits must NOT fire any chatops notification — the
    /// iteration-level workspace-init alert is the operator-facing
    /// signal of the upstream problem. Per-audit skip notifications
    /// would flood the channel.
    #[tokio::test]
    async fn workspace_unavailable_does_not_post_chatops() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("wsu_quiet")
                .with_outcome(AuditOutcome::WorkspaceUnavailable {
                    audit_type: "wsu_quiet".into(),
                    workspace_path: ws.clone(),
                    reason: "workspace exists but has no .git/ subdirectory".into(),
                }),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("wsu_quiet");
        // notify_on_clean = true to prove the skip path doesn't
        // accidentally route through the "clean findings" notifier.
        let mut settings = HashMap::new();
        settings.insert(
            "wsu_quiet".to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: true,
                extra: HashMap::new(),
            },
        );
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let no_post = server
            .mock("POST", "/chat.postMessage")
            .expect(0) // skipped audits must NEVER post
            .create_async()
            .await;
        let ctx = make_ctx(chatops);
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &settings, Some(&ctx), &HashSet::new())
            .await
            .unwrap();
        no_post.assert_async().await;
    }

    #[tokio::test]
    async fn scheduler_records_validation_exhausted_in_history_and_logs_warn() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("ve1")
                .with_policy(WritePolicy::OpenSpecOnly)
                .with_outcome(AuditOutcome::ValidationExhausted {
                    audit_type: "ve1".into(),
                    retries_attempted: 1,
                    final_error: "MODIFIED header `Old name` not found".repeat(10),
                }),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("ve1");
        let repo = fixture_repo();
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
            .await
            .unwrap();
        // State + history updated.
        let state = AuditState::load_or_default(&ws);
        let entry = state.runs.get("ve1").expect("state recorded");
        assert_eq!(entry.last_outcome, AuditOutcomeKind::ValidationExhausted);
        let hist = state.history("ve1");
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].outcome_kind, "ValidationExhausted");
        assert_eq!(hist[0].retries_used, 1);
        let excerpt = hist[0]
            .error_excerpt
            .as_deref()
            .expect("validation-exhausted entry must carry error_excerpt");
        assert!(
            excerpt.starts_with("MODIFIED header"),
            "excerpt must be a prefix of the validation error: {excerpt}"
        );
        // 200-char cap + ellipsis is the documented bound.
        assert!(
            excerpt.chars().count() <= crate::audits::VALIDATION_ERROR_HISTORY_EXCERPT + 1
        );
    }

    #[tokio::test]
    async fn scheduler_appends_reported_entry_with_retries_used() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("rep_retry")
                .with_outcome(AuditOutcome::Reported {
                    findings: vec![],
                    retries_used: 2,
                }),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("rep_retry");
        let repo = fixture_repo();
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), None, &HashSet::new())
            .await
            .unwrap();
        let state = AuditState::load_or_default(&ws);
        let hist = state.history("rep_retry");
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].outcome_kind, "Reported");
        assert_eq!(hist[0].retries_used, 2);
        assert!(
            hist[0].error_excerpt.is_none(),
            "Reported outcomes must NOT carry an error_excerpt"
        );
    }

    #[tokio::test]
    async fn reported_with_retry_includes_validated_on_retry_clause_in_chatops() {
        let (_t, ws) = init_workspace();
        let findings = vec![Finding {
            severity: Severity::Low,
            subject: "x".into(),
            body: "y".into(),
            anchor: None,
        }];
        let audit = Arc::new(CountingAudit::new("retry-clause").with_outcome(
            AuditOutcome::Reported {
                findings: findings.clone(),
                retries_used: 1,
            },
        ));
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("retry-clause");
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::Regex(
                "validated on retry 1 of 1".into(),
            ))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), Some(&ctx), &HashSet::new())
            .await
            .unwrap();
        post_mock.assert_async().await;
    }

    fn brightline_file_finding(n: usize) -> Finding {
        Finding {
            severity: Severity::Medium,
            subject: format!("file src/file{n}.rs is {lines} lines (threshold: 800)", lines = 1000 + n),
            body: format!("path: src/file{n}.rs\nlines: {lines}\nthreshold: 800", lines = 1000 + n),
            anchor: Some(format!("src/file{n}.rs:1")),
        }
    }

    #[tokio::test]
    async fn brightline_many_findings_post_via_thread() {
        // 5 file findings + 2 dup findings → 7 body lines → above the
        // threading threshold. The Slack backend issues TWO
        // chat.postMessage calls under the hood: the top-line, then the
        // threaded reply. The mockito server expects both.
        let (_t, ws) = init_workspace();
        let mut findings = Vec::new();
        for i in 0..5 {
            findings.push(brightline_file_finding(i));
        }
        for i in 0..2 {
            findings.push(Finding {
                severity: Severity::Low,
                subject: format!("duplicate signature `fn helper{i}` across 2 files"),
                body: "mod_a.rs:1\nmod_b.rs:1".into(),
                anchor: Some("mod_a.rs:1".into()),
            });
        }
        let audit = Arc::new(
            CountingAudit::new("architecture_brightline")
                .with_outcome(AuditOutcome::reported(findings)),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("architecture_brightline");
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        // Top-line POST: matches the brightline emoji + counts.
        let top_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("📐 architecture_brightline".into()),
                mockito::Matcher::Regex("5 file\\(s\\) over line threshold".into()),
                mockito::Matcher::Regex("2 duplicate signature\\(s\\)".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"9999.0001"}"#)
            .expect(1)
            .create_async()
            .await;
        // Threaded-reply POST: carries thread_ts AND the body lines.
        let reply_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("9999.0001".into()),
                mockito::Matcher::Regex("file src/file0.rs is".into()),
                mockito::Matcher::Regex("duplicate signature `fn helper0`".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"9999.0002"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), Some(&ctx), &HashSet::new())
            .await
            .unwrap();
        top_mock.assert_async().await;
        reply_mock.assert_async().await;
    }

    #[tokio::test]
    async fn brightline_no_findings_notify_on_clean_posts_check_inline() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("architecture_brightline")
                .with_outcome(AuditOutcome::reported(vec![])),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("architecture_brightline");
        let repo = fixture_repo();
        let mut settings = HashMap::new();
        settings.insert(
            "architecture_brightline".to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: true,
                extra: HashMap::new(),
            },
        );

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        // Exactly ONE post_notification call carrying the ✅ form. No
        // thread reply (empty body → should_thread=false).
        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("✅ architecture_brightline".into()),
                mockito::Matcher::Regex("no findings".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &settings, Some(&ctx), &HashSet::new())
            .await
            .unwrap();
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn brightline_no_findings_without_notify_on_clean_posts_nothing() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(
            CountingAudit::new("architecture_brightline")
                .with_outcome(AuditOutcome::reported(vec![])),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("architecture_brightline");
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        // No notify_on_clean → no chat.postMessage at all.
        let silent_mock = server
            .mock("POST", "/chat.postMessage")
            .expect(0)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), Some(&ctx), &HashSet::new())
            .await
            .unwrap();
        silent_mock.assert_async().await;
    }

    #[tokio::test]
    async fn drift_short_findings_post_inline() {
        // One short divergence → body is 1 line, well under 300 chars
        // → inline (single post_notification call, no thread).
        let (_t, ws) = init_workspace();
        let findings = vec![Finding {
            severity: Severity::Medium,
            subject: "[orchestrator-cli] timeout".into(),
            body: "spec X says A; code says B.".into(),
            anchor: Some("src/cli.rs:1".into()),
        }];
        let audit = Arc::new(
            CountingAudit::new("drift_audit")
                .with_outcome(AuditOutcome::reported(findings)),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("drift_audit");
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        // One inline POST whose body holds the top-line + the bullet.
        // No thread_ts on the call.
        let post_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("🧭 drift_audit".into()),
                mockito::Matcher::Regex("1 spec/code divergence\\(s\\) detected".into()),
                mockito::Matcher::Regex("\\[orchestrator-cli\\]".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"1.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), Some(&ctx), &HashSet::new())
            .await
            .unwrap();
        post_mock.assert_async().await;
    }

    #[tokio::test]
    async fn drift_long_findings_post_via_thread() {
        // 5 short findings → 5 body lines → above the 3-line threshold
        // → thread. Two chat.postMessage calls expected.
        let (_t, ws) = init_workspace();
        let findings: Vec<Finding> = (0..5)
            .map(|i| Finding {
                severity: Severity::Medium,
                subject: format!("[cap{i}] requirement{i}"),
                body: "divergence detail".into(),
                anchor: Some(format!("src/file{i}.rs:1")),
            })
            .collect();
        let audit = Arc::new(
            CountingAudit::new("drift_audit")
                .with_outcome(AuditOutcome::reported(findings)),
        );
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("drift_audit");
        let repo = fixture_repo();

        let mut server = mockito::Server::new_async().await;
        let chatops = fixture_chatops(&mut server).await;
        let top_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("🧭 drift_audit".into()),
                mockito::Matcher::Regex("5 spec/code divergence\\(s\\) detected".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"42.0"}"#)
            .expect(1)
            .create_async()
            .await;
        let reply_mock = server
            .mock("POST", "/chat.postMessage")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("42.0".into()),
                mockito::Matcher::Regex("\\[cap0\\]".into()),
                mockito::Matcher::Regex("\\[cap4\\]".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"ts":"42.1"}"#)
            .expect(1)
            .create_async()
            .await;
        let ctx = make_ctx(chatops);
        run_due_audits(test_paths(), &registry, &ws, &repo, Some(&cfg), &HashMap::new(), Some(&ctx), &HashSet::new())
            .await
            .unwrap();
        top_mock.assert_async().await;
        reply_mock.assert_async().await;
    }

    #[test]
    fn format_retry_clause_renders_only_when_retries_used() {
        assert_eq!(format_retry_clause(0, 1), "");
        assert_eq!(format_retry_clause(1, 1), " (validated on retry 1 of 1)");
        assert_eq!(format_retry_clause(2, 5), " (validated on retry 2 of 5)");
    }

    fn se(staged: char, worktree: char, path: &str) -> git::StatusEntry {
        git::StatusEntry {
            staged,
            worktree,
            path: path.to_string(),
            orig_path: None,
        }
    }

    #[test]
    fn detect_violation_none_with_empty_porcelain_is_ok() {
        assert!(detect_write_policy_violation(WritePolicy::None, &[]).is_none());
    }

    #[test]
    fn detect_violation_none_with_dirty_workspace_fails() {
        let v = detect_write_policy_violation(WritePolicy::None, &[se('?', '?', "new-file.txt")]);
        assert!(v.is_some());
    }

    #[test]
    fn detect_violation_openspec_only_allows_changes_dir() {
        let entries = [
            se('?', '?', "openspec/changes/new-thing/proposal.md"),
            se(' ', 'M', "openspec/changes/new-thing/tasks.md"),
        ];
        assert!(detect_write_policy_violation(WritePolicy::OpenSpecOnly, &entries).is_none());
    }

    #[test]
    fn detect_violation_openspec_only_rejects_outside_path() {
        let entries = [
            se('?', '?', "openspec/changes/new/proposal.md"),
            se(' ', 'M', "src/lib.rs"),
        ];
        let v = detect_write_policy_violation(WritePolicy::OpenSpecOnly, &entries);
        assert!(v.is_some());
        assert!(v.unwrap().reason.contains("src/lib.rs"));
    }

    #[test]
    fn detect_violation_approved_always_ok() {
        assert!(
            detect_write_policy_violation(
                WritePolicy::Approved,
                &[se(' ', 'M', "anywhere/at/all.rs")]
            )
            .is_none()
        );
    }

    // ---------- queued audit runs (chatops-on-demand-audit-trigger) ----------

    #[tokio::test]
    async fn queued_audit_runs_even_when_cadence_not_due() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("q1"));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        // Cadence says NOT due (just ran 1 hour ago, daily cadence).
        let mut state = AuditState::default();
        state.record(
            "q1",
            AuditRunEntry {
                last_run_at: Utc::now() - chrono::Duration::hours(1),
                last_run_sha: Some("anything".into()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();
        let cfg = audits_cfg_daily("q1");
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("q1".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "queued audit must run regardless of cadence"
        );
        // State updated to the new last_run timestamp.
        let after = AuditState::load_or_default(&ws);
        let entry = after.runs.get("q1").expect("state recorded");
        assert!(
            entry.last_run_at > Utc::now() - chrono::Duration::minutes(1),
            "last_run_at must move forward to ~now"
        );
    }

    #[tokio::test]
    async fn queued_audit_runs_even_when_requires_head_change_and_sha_matches() {
        let (_t, ws) = init_workspace();
        let head = git::rev_parse(&ws, "main").unwrap();
        let audit = Arc::new(CountingAudit::new("q2").with_rhc(true));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let mut state = AuditState::default();
        state.record(
            "q2",
            AuditRunEntry {
                last_run_at: Utc::now() - chrono::Duration::days(7),
                last_run_sha: Some(head.clone()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();
        let cfg = audits_cfg_daily("q2");
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("q2".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "queued audit must run even when HEAD unchanged"
        );
    }

    #[tokio::test]
    async fn queued_audit_runs_even_when_cadence_is_disabled() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("q3"));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        // No AuditsConfig → cadence resolves to Disabled.
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("q3".to_string());
        run_due_audits(test_paths(), &registry, &ws, &repo, None, &HashMap::new(), None, &queued)
            .await
            .unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "queued audit must run even when cadence is Disabled"
        );
    }

    #[tokio::test]
    async fn queued_audit_each_type_runs_at_most_once_per_iteration() {
        // The HashSet collapses duplicates at the polling-loop drain
        // boundary; this test asserts the scheduler also runs each audit
        // exactly once across the queue + cadence sweep.
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("q4"));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("q4");
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("q4".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "queued audit must run exactly once per iteration (not also via cadence sweep)"
        );
    }

    #[tokio::test]
    async fn queued_audit_with_unknown_name_is_skipped_other_audits_proceed() {
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("q5"));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("q5");
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        // The operator queued a name that isn't in the registry. The
        // scheduler logs a warning and the cadence-sweep continues.
        queued.insert("nonexistent_audit".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        // The registered audit `q5` is due (no prior state) so the
        // cadence sweep runs it. The unknown queue entry is a no-op.
        assert_eq!(*counter.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn queued_audit_coexists_with_cadence_due_other_audit() {
        let (_t, ws) = init_workspace();
        let queued_audit = Arc::new(CountingAudit::new("q6-queued"));
        let cadence_audit = Arc::new(CountingAudit::new("q6-cadence"));
        let queued_counter = queued_audit.invocations.clone();
        let cadence_counter = cadence_audit.invocations.clone();
        let registry =
            AuditRegistry::with_audits(vec![queued_audit.clone(), cadence_audit.clone()]);
        // Both have daily cadence. Raise the per-iteration bound above
        // the test's audit count so this test isolates the queue+cadence
        // coexistence property from the storm-prevention bound.
        let mut defaults = HashMap::new();
        defaults.insert("q6-queued".to_string(), Cadence::Daily);
        defaults.insert("q6-cadence".to_string(), Cadence::Daily);
        let cfg = AuditsConfig {
            defaults,
            settings: HashMap::new(),
            max_audits_per_iteration: 5,
            ..AuditsConfig::default()
        };
        // q6-queued has a recent last_run (not due via cadence), but the
        // operator has queued it. q6-cadence has no prior state (due).
        let mut state = AuditState::default();
        state.record(
            "q6-queued",
            AuditRunEntry {
                last_run_at: Utc::now() - chrono::Duration::hours(1),
                last_run_sha: Some("anything".into()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("q6-queued".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        assert_eq!(*queued_counter.lock().unwrap(), 1, "queued audit must run");
        assert_eq!(
            *cadence_counter.lock().unwrap(),
            1,
            "cadence-due audit must also run"
        );
    }

    #[tokio::test]
    async fn second_iteration_without_queue_does_not_rerun() {
        // Iteration 1: queue → audit runs and state recorded.
        // Iteration 2: empty queue → cadence sweep sees recent last_run
        // and skips, so the audit does NOT re-run.
        let (_t, ws) = init_workspace();
        let audit = Arc::new(CountingAudit::new("q7"));
        let counter = audit.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![audit.clone()]);
        let cfg = audits_cfg_daily("q7");
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("q7".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        assert_eq!(*counter.lock().unwrap(), 1);

        // Iteration 2: empty queue. Cadence says NOT due (just ran).
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "second iteration without queue must NOT re-run"
        );
    }

    // ---------------- max_audits_per_iteration bound ----------------

    /// Build an `AuditsConfig` that sets every audit in `slugs` to daily
    /// cadence AND sets `max_audits_per_iteration` to `bound`.
    fn audits_cfg_bounded(slugs: &[&str], bound: usize) -> AuditsConfig {
        let mut defaults = HashMap::new();
        for s in slugs {
            defaults.insert((*s).to_string(), Cadence::Daily);
        }
        AuditsConfig {
            defaults,
            settings: HashMap::new(),
            max_audits_per_iteration: bound,
            ..AuditsConfig::default()
        }
    }

    #[tokio::test]
    async fn bound_default_one_runs_first_eligible_and_defers_rest() {
        let (_t, ws) = init_workspace();
        let a = Arc::new(CountingAudit::new("b1-a").with_rhc(false));
        let b = Arc::new(CountingAudit::new("b1-b").with_rhc(false));
        let c = Arc::new(CountingAudit::new("b1-c").with_rhc(false));
        let ca = a.invocations.clone();
        let cb = b.invocations.clone();
        let cc = c.invocations.clone();
        let registry =
            AuditRegistry::with_audits(vec![a.clone(), b.clone(), c.clone()]);
        // Default bound = 1.
        let cfg = audits_cfg_bounded(&["b1-a", "b1-b", "b1-c"], 1);
        let repo = fixture_repo();
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .await
        .unwrap();
        // First in declaration order runs; the other two defer.
        assert_eq!(*ca.lock().unwrap(), 1, "first audit must run");
        assert_eq!(*cb.lock().unwrap(), 0, "second audit must defer");
        assert_eq!(*cc.lock().unwrap(), 0, "third audit must defer");
        // State for the unrun audits is untouched, so the next iteration
        // can re-evaluate them.
        let state = AuditState::load_or_default(&ws);
        assert!(state.runs.contains_key("b1-a"));
        assert!(!state.runs.contains_key("b1-b"));
        assert!(!state.runs.contains_key("b1-c"));
    }

    #[tokio::test]
    async fn bound_two_runs_first_two_and_defers_third() {
        let (_t, ws) = init_workspace();
        let a = Arc::new(CountingAudit::new("b2-a").with_rhc(false));
        let b = Arc::new(CountingAudit::new("b2-b").with_rhc(false));
        let c = Arc::new(CountingAudit::new("b2-c").with_rhc(false));
        let ca = a.invocations.clone();
        let cb = b.invocations.clone();
        let cc = c.invocations.clone();
        let registry =
            AuditRegistry::with_audits(vec![a.clone(), b.clone(), c.clone()]);
        let cfg = audits_cfg_bounded(&["b2-a", "b2-b", "b2-c"], 2);
        let repo = fixture_repo();
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(*ca.lock().unwrap(), 1);
        assert_eq!(*cb.lock().unwrap(), 1);
        assert_eq!(*cc.lock().unwrap(), 0, "third audit must defer");
    }

    #[tokio::test]
    async fn bound_above_eligibles_runs_them_all() {
        let (_t, ws) = init_workspace();
        let a = Arc::new(CountingAudit::new("b5-a").with_rhc(false));
        let b = Arc::new(CountingAudit::new("b5-b").with_rhc(false));
        let c = Arc::new(CountingAudit::new("b5-c").with_rhc(false));
        let ca = a.invocations.clone();
        let cb = b.invocations.clone();
        let cc = c.invocations.clone();
        let registry =
            AuditRegistry::with_audits(vec![a.clone(), b.clone(), c.clone()]);
        let cfg = audits_cfg_bounded(&["b5-a", "b5-b", "b5-c"], 5);
        let repo = fixture_repo();
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(*ca.lock().unwrap(), 1);
        assert_eq!(*cb.lock().unwrap(), 1);
        assert_eq!(*cc.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn bound_zero_skips_all_audits_even_when_eligible() {
        let (_t, ws) = init_workspace();
        let a = Arc::new(CountingAudit::new("b0-a").with_rhc(false));
        let b = Arc::new(CountingAudit::new("b0-b").with_rhc(false));
        let ca = a.invocations.clone();
        let cb = b.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![a.clone(), b.clone()]);
        let cfg = audits_cfg_bounded(&["b0-a", "b0-b"], 0);
        let repo = fixture_repo();
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(*ca.lock().unwrap(), 0, "bound=0 must skip everything");
        assert_eq!(*cb.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn bound_zero_skips_on_demand_queue_too() {
        let (_t, ws) = init_workspace();
        let a = Arc::new(CountingAudit::new("bz-a").with_rhc(false));
        let ca = a.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![a.clone()]);
        let cfg = audits_cfg_bounded(&["bz-a"], 0);
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("bz-a".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        assert_eq!(
            *ca.lock().unwrap(),
            0,
            "bound=0 must also defer on-demand queued audits"
        );
    }

    #[tokio::test]
    async fn on_demand_queued_runs_count_against_the_bound() {
        // Two queued + 1 cadence-eligible, bound=2 → both queued run
        // first (declaration order), the cadence-eligible defers.
        let (_t, ws) = init_workspace();
        let q1 = Arc::new(CountingAudit::new("bq-q1").with_rhc(false));
        let q2 = Arc::new(CountingAudit::new("bq-q2").with_rhc(false));
        let ca = Arc::new(CountingAudit::new("bq-c").with_rhc(false));
        let cq1 = q1.invocations.clone();
        let cq2 = q2.invocations.clone();
        let cca = ca.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![
            q1.clone(),
            q2.clone(),
            ca.clone(),
        ]);
        let cfg = audits_cfg_bounded(&["bq-q1", "bq-q2", "bq-c"], 2);
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("bq-q1".to_string());
        queued.insert("bq-q2".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        assert_eq!(*cq1.lock().unwrap(), 1, "queued q1 runs (drained first)");
        assert_eq!(*cq2.lock().unwrap(), 1, "queued q2 runs (drained first)");
        assert_eq!(
            *cca.lock().unwrap(),
            0,
            "cadence-eligible audit defers because the bound was consumed by the queue"
        );
    }

    #[tokio::test]
    async fn on_demand_queue_three_with_bound_one_runs_first_only() {
        // Three queued audits + bound=1 → first runs; the other two
        // remain queued for next iteration (queue ownership is the
        // caller's; the scheduler just refuses to drain past the bound).
        let (_t, ws) = init_workspace();
        let q1 = Arc::new(CountingAudit::new("bq1-a").with_rhc(false));
        let q2 = Arc::new(CountingAudit::new("bq1-b").with_rhc(false));
        let q3 = Arc::new(CountingAudit::new("bq1-c").with_rhc(false));
        let cq1 = q1.invocations.clone();
        let cq2 = q2.invocations.clone();
        let cq3 = q3.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![
            q1.clone(),
            q2.clone(),
            q3.clone(),
        ]);
        let cfg = audits_cfg_bounded(&["bq1-a", "bq1-b", "bq1-c"], 1);
        let repo = fixture_repo();
        let mut queued = HashSet::new();
        queued.insert("bq1-a".to_string());
        queued.insert("bq1-b".to_string());
        queued.insert("bq1-c".to_string());
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &queued,
        )
        .await
        .unwrap();
        // Only the first in declaration order runs this iteration.
        assert_eq!(*cq1.lock().unwrap(), 1);
        assert_eq!(*cq2.lock().unwrap(), 0);
        assert_eq!(*cq3.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn cadence_skipped_audits_do_not_consume_a_slot() {
        // a is not cadence-due (recent last_run), b IS due. With bound=1
        // the scheduler must skip `a` (no slot consumed) and run `b`.
        let (_t, ws) = init_workspace();
        let a = Arc::new(CountingAudit::new("bs-a").with_rhc(false));
        let b = Arc::new(CountingAudit::new("bs-b").with_rhc(false));
        let ca = a.invocations.clone();
        let cb = b.invocations.clone();
        let registry = AuditRegistry::with_audits(vec![a.clone(), b.clone()]);
        let cfg = audits_cfg_bounded(&["bs-a", "bs-b"], 1);
        // Seed state so `a` has a recent run; `b` is unseeded (due).
        let mut state = AuditState::default();
        state.record(
            "bs-a",
            AuditRunEntry {
                last_run_at: Utc::now() - chrono::Duration::hours(1),
                last_run_sha: Some("recent".into()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        state.save(&ws).unwrap();
        let repo = fixture_repo();
        run_due_audits(
            test_paths(),
            &registry,
            &ws,
            &repo,
            Some(&cfg),
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(*ca.lock().unwrap(), 0, "not-due audit must be skipped");
        assert_eq!(
            *cb.lock().unwrap(),
            1,
            "skipped audit did not consume the bound's slot, so b runs"
        );
    }
}
