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
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::{AuditContext, AuditOutcome, write_sandbox_settings};
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
pub(crate) async fn run_specs_writing_audit(
    params: SpecsWritingAuditParams<'_>,
    ctx: &mut AuditContext<'_>,
) -> Result<AuditOutcome> {
    let audit_type = params.audit_type;

    let before: HashSet<String> = snapshot_change_dirs(ctx.workspace);

    let mut sandbox = params.sandbox.clone();
    sandbox.allowed_tools = ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect();

    let (settings_path, _settings_guard) =
        write_sandbox_settings(&sandbox, params.settings_dir)
            .with_context(|| format!("generating {audit_type} sandbox settings file"))?;

    let _ = ctx.log_writer.write_section(
        &format!("{audit_type}_preamble"),
        &format!(
            "executor_command: {}\ntimeout_secs: {}\nprompt_source: {}\nmax_proposals_per_run: {}\nsettings_file: {}\nallowed_tools: {}\npre_run_change_dirs: {}",
            params.executor_command,
            params.executor_timeout_secs,
            params.prompt_source,
            params.max_proposals,
            settings_path.display(),
            sandbox.allowed_tools.join(","),
            before.len(),
        ),
    );
    let _ = ctx
        .log_writer
        .write_section(&format!("{audit_type}_prompt"), params.prompt);

    let outcome = run_subprocess(
        audit_type,
        params.executor_command,
        &settings_path,
        &sandbox.allowed_tools,
        ctx.workspace,
        params.prompt,
        Duration::from_secs(params.executor_timeout_secs),
    )
    .await
    .with_context(|| format!("spawning {audit_type} CLI subprocess"))?;

    let _ = ctx.log_writer.write_section(
        &format!("{audit_type}_stdout"),
        if outcome.stdout.is_empty() {
            "(empty)"
        } else {
            outcome.stdout.as_str()
        },
    );
    let _ = ctx.log_writer.write_section(
        &format!("{audit_type}_stderr"),
        if outcome.stderr.is_empty() {
            "(empty)"
        } else {
            outcome.stderr.as_str()
        },
    );

    if outcome.timed_out {
        let _ = ctx.log_writer.write_section(
            &format!("{audit_type}_outcome"),
            "kind: Err\nreason: timeout",
        );
        return Err(anyhow!(
            "{audit_type}: CLI exceeded the {}s timeout",
            params.executor_timeout_secs
        ));
    }

    if let Some(status) = outcome.exit_status {
        if !status.success() {
            let _ = ctx.log_writer.write_section(
                &format!("{audit_type}_outcome"),
                &format!("kind: Err\nreason: exit {status}"),
            );
            return Err(anyhow!("{audit_type}: CLI exited {status}"));
        }
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

    let mut validated: Vec<String> = Vec::new();
    for name in &new_dirs {
        match validate_change(params.openspec_command, ctx.workspace, name).await {
            Ok(()) => validated.push(name.clone()),
            Err(e) => {
                let path = ctx.workspace.join("openspec/changes").join(name);
                if let Err(rm_err) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(
                        audit_type = audit_type,
                        path = %path.display(),
                        "failed to remove invalid change dir: {rm_err}"
                    );
                }
                let _ = ctx.log_writer.write_section(
                    &format!("{audit_type}_validation_failure_{name}"),
                    &format!("change: {name}\nerror: {e:#}"),
                );
                tracing::warn!(
                    audit_type = audit_type,
                    change = %name,
                    "rejecting agent-produced change that failed `openspec validate --strict`: {e:#}"
                );
            }
        }
    }

    if validated.is_empty() {
        let _ = ctx.log_writer.write_section(
            &format!("{audit_type}_outcome"),
            "kind: SpecsWritten\nvalidated_count: 0",
        );
        return Ok(AuditOutcome::SpecsWritten(Vec::new()));
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
            "kind: SpecsWritten\nvalidated_count: {}\nchanges:\n{}",
            validated.len(),
            validated.join("\n")
        ),
    );

    Ok(AuditOutcome::SpecsWritten(validated))
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

struct SubprocessOutcome {
    timed_out: bool,
    exit_status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
}

async fn run_subprocess(
    audit_type: &str,
    command: &str,
    settings_path: &Path,
    allowed_tools: &[String],
    workspace: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<SubprocessOutcome> {
    // ETXTBSY retry: see docs/test-reliability.md
    // "ETXTBSY from concurrent audit-CLI fixtures".
    let mut child = super::spawn_with_etxtbsy_retry(|| {
        let mut cmd = Command::new(command);
        cmd.arg("--settings")
            .arg(settings_path)
            .arg("--allowedTools")
            .arg(allowed_tools.join(","))
            .arg("--permission-mode")
            .arg("acceptEdits")
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        cmd
    })
    .await
    .with_context(|| format!("spawning {audit_type} command `{command}`"))?;

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
        Some(Err(e)) => {
            Err(e).with_context(|| format!("waiting on {audit_type} child process"))
        }
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

