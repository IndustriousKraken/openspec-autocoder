//! `autocoder audit run` — on-demand audit trigger from the command
//! line. Probes for the daemon's control socket: when reachable, sends
//! the `queue_audit` action so the daemon runs the audit on its next
//! polling iteration (same path the chatops `audit` verb uses); when
//! the daemon is NOT running, falls back to a standalone invocation
//! against the named workspace and prints findings to stdout.

use crate::audits::{
    AuditContext, AuditLogWriter, AuditOutcome, AuditRegistry,
    architecture_consultative::ArchitectureConsultativeAudit,
    brightline::ArchitectureBrightlineAudit,
    documentation_audit::DocumentationAudit,
    drift::DriftAudit,
    missing_tests::MissingTestsAudit, security_bug::SecurityBugAudit,
};
use crate::config::{
    AuditSettings, ExecutorConfig, ExecutorKind, RepositoryConfig,
};
use crate::control_socket;
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn execute(workspace: PathBuf, audit_name: String) -> Result<()> {
    let paths = crate::cli::resolve_paths_from_env()?;
    execute_at(&control_socket::socket_path(&paths), &paths, workspace, audit_name).await
}

pub async fn execute_at(
    socket: &Path,
    paths: &crate::paths::DaemonPaths,
    workspace: PathBuf,
    audit_name: String,
) -> Result<()> {
    // Probe-then-submit. A failed connect is the daemon-absent signal
    // (the standalone fallback path); any other connect failure is an
    // immediate error (the daemon IS present but we couldn't talk to
    // it).
    match UnixStream::connect(socket).await {
        Ok(stream) => submit_queue_audit(stream, &workspace, &audit_name).await,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
            // No daemon listening → standalone path.
            run_standalone(paths, &workspace, &audit_name).await
        }
        Err(e) => Err(anyhow!(
            "could not connect to control socket {}: {e}",
            socket.display(),
        )),
    }
}

/// Daemon-present path: send `queue_audit { workspace, audit_type }` and
/// print the daemon's response. Exit non-zero on a non-ok response so
/// the operator's calling script can tell whether the queue succeeded.
async fn submit_queue_audit(
    stream: UnixStream,
    workspace: &Path,
    audit_name: &str,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let request = serde_json::json!({
        "action": "queue_audit",
        "audit_type": audit_name,
        "workspace": workspace.display().to_string(),
    });
    let mut payload = request.to_string();
    payload.push('\n');
    write_half
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| anyhow!("writing to control socket: {e}"))?;
    write_half
        .shutdown()
        .await
        .map_err(|e| anyhow!("shutdown of control socket: {e}"))?;
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| anyhow!("reading control-socket response: {e}"))?;
    if line.is_empty() {
        return Err(anyhow!("control socket closed without responding"));
    }
    let resp: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| anyhow!("parsing control-socket response: {e}\nraw: {line}"))?;
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        let url = resp.get("url").and_then(|v| v.as_str()).unwrap_or("?");
        let canonical_audit = resp
            .get("audit_type")
            .and_then(|v| v.as_str())
            .unwrap_or(audit_name);
        let poll_clause = resp
            .get("poll_interval_sec")
            .and_then(|v| v.as_u64())
            .map(|s| {
                let mins = (s + 30) / 60;
                if mins == 0 {
                    format!(" (~{s}s)")
                } else {
                    format!(" (~{mins}m)")
                }
            })
            .unwrap_or_default();
        println!(
            "✓ Queued {canonical_audit} for {url}. Will run on the next polling iteration{poll_clause}."
        );
        Ok(())
    } else {
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("(no error message)");
        Err(anyhow!("daemon refused queue_audit: {err}"))
    }
}

/// Daemon-absent path: build a minimal audit registry, look up the
/// named audit, construct a fake `RepositoryConfig` whose `local_path`
/// is the operator-supplied workspace, and call the audit's `run`
/// directly. Findings (if any) are printed to stdout.
async fn run_standalone(paths: &crate::paths::DaemonPaths, workspace: &Path, audit_name: &str) -> Result<()> {
    if !workspace.is_dir() {
        return Err(anyhow!(
            "workspace path {} is not a directory",
            workspace.display()
        ));
    }
    let executor_cfg = default_standalone_executor_cfg();
    let audit_settings: HashMap<String, AuditSettings> = HashMap::new();

    let mut registry = AuditRegistry::new();
    registry.register(std::sync::Arc::new(ArchitectureBrightlineAudit::new(
        &audit_settings,
    )));
    registry.register(std::sync::Arc::new(DriftAudit::new(
        &audit_settings,
        &executor_cfg,
    )));
    registry.register(std::sync::Arc::new(MissingTestsAudit::new(
        &audit_settings,
        &executor_cfg,
    )));
    registry.register(std::sync::Arc::new(SecurityBugAudit::new(
        &audit_settings,
        &executor_cfg,
    )));
    registry.register(std::sync::Arc::new(ArchitectureConsultativeAudit::new(
        &audit_settings,
        &executor_cfg,
    )));
    registry.register(std::sync::Arc::new(DocumentationAudit::new(
        &audit_settings,
        &executor_cfg,
    )));

    let audit_arc = registry
        .iter()
        .find(|a| a.audit_type() == audit_name)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = registry
                .known_type_names()
                .into_iter()
                .collect();
            anyhow!(
                "unknown audit `{audit_name}`; registered: {}",
                known.join(", ")
            )
        })?;

    let repo = fake_repo_for_workspace(workspace);
    let log_writer = AuditLogWriter::open(paths, workspace, audit_arc.audit_type())?;
    let mut ctx = AuditContext {
        workspace,
        repo: &repo,
        chatops_ctx: None,
        log_writer,
        max_validation_retries: 1,
    };

    println!(
        "▶ Running {audit} standalone against {ws}",
        audit = audit_arc.audit_type(),
        ws = workspace.display()
    );
    let outcome = audit_arc.run(&mut ctx).await?;
    print_standalone_outcome(audit_arc.audit_type(), &outcome);
    Ok(())
}

fn default_standalone_executor_cfg() -> ExecutorConfig {
    ExecutorConfig {
        kind: ExecutorKind::ClaudeCli,
        command: "claude".to_string(),
        timeout_secs: 600,
        sandbox: None,
        implementer_prompt_path: None,
        changelog_stylist_prompt_path: None,
        perma_stuck_after_failures: None,
        max_changes_per_pr: None,
        startup_jitter_max_secs: None,
        inter_iteration_jitter_pct: None,
        max_auto_revisions_per_pr: 5,
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

fn fake_repo_for_workspace(workspace: &Path) -> RepositoryConfig {
    RepositoryConfig {
        url: "standalone://audit-run".to_string(),
        local_path: Some(workspace.to_path_buf()),
        base_branch: "main".to_string(),
        agent_branch: "agent-q".to_string(),
        poll_interval_sec: 60,
        chatops_channel_id: None,
        max_changes_per_pr: None,
        audits: None,
        spec_storage: None,
        upstream: None,
        auto_submit_pr: true,
    }
}

fn print_standalone_outcome(audit_name: &str, outcome: &AuditOutcome) {
    match outcome {
        AuditOutcome::NoFindings => {
            println!("✅ {audit_name}: no findings");
        }
        AuditOutcome::Reported {
            findings,
            retries_used,
        } => {
            if findings.is_empty() {
                println!("✅ {audit_name}: no findings");
            } else {
                println!(
                    "📋 {audit_name}: {} finding(s){}",
                    findings.len(),
                    if *retries_used > 0 {
                        format!(" (validated on retry {retries_used})")
                    } else {
                        String::new()
                    }
                );
                for (i, f) in findings.iter().enumerate() {
                    println!(
                        "  [{i}] {sev:?}: {subj}",
                        i = i + 1,
                        sev = f.severity,
                        subj = f.subject,
                    );
                    if !f.body.is_empty() {
                        for line in f.body.lines() {
                            println!("      {line}");
                        }
                    }
                }
            }
        }
        AuditOutcome::SpecsWritten {
            changes,
            retries_used,
        } => {
            println!(
                "🔍 {audit_name}: wrote {} spec(s){}",
                changes.len(),
                if *retries_used > 0 {
                    format!(" (validated on retry {retries_used})")
                } else {
                    String::new()
                }
            );
            for c in changes {
                println!("  • {c}");
            }
        }
        AuditOutcome::ValidationExhausted {
            retries_attempted,
            final_error,
            ..
        } => {
            println!(
                "❌ {audit_name}: produced an invalid proposal after {retries_attempted} retries"
            );
            println!("   final error: {final_error}");
        }
        AuditOutcome::WorkspaceUnavailable {
            workspace_path,
            reason,
            ..
        } => {
            println!(
                "⏭ {audit_name}: workspace unavailable ({reason}) at {}",
                workspace_path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::net::UnixListener;

    /// Spawn a one-shot fake daemon that responds with `response` to the
    /// first incoming connection, then drops. Mirrors the helper in
    /// `cli/reload.rs::tests::fake_server`.
    async fn fake_server(response: &'static str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let response_owned = response.to_string();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut buf = String::new();
            let _ = reader.read_line(&mut buf).await;
            let mut bytes = response_owned.into_bytes();
            if !bytes.ends_with(b"\n") {
                bytes.push(b'\n');
            }
            let _ = write_half.write_all(&bytes).await;
            let _ = write_half.shutdown().await;
        });
        (dir, socket)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ok_response_prints_ack_and_returns_ok() {
        let (_dir, socket) = fake_server(
            r#"{"ok":true,"url":"git@github.com:acme/myrepo.git","audit_type":"security_bug_audit","poll_interval_sec":300}"#,
        )
        .await;
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let res = execute_at(
            &socket,
            &paths,
            PathBuf::from("/tmp/some-workspace"),
            "security_bug_audit".to_string(),
        )
        .await;
        assert!(res.is_ok(), "expected Ok, got: {res:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn not_ok_response_returns_err() {
        let (_dir, socket) = fake_server(
            r#"{"ok":false,"error":"no managed repository found for workspace path"}"#,
        )
        .await;
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let res = execute_at(
            &socket,
            &paths,
            PathBuf::from("/tmp/some-workspace"),
            "security_bug_audit".to_string(),
        )
        .await;
        let err = res.expect_err("expected Err on ok=false response");
        let msg = format!("{err:#}");
        assert!(msg.contains("no managed repository"), "error must surface daemon message: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_socket_falls_back_to_standalone() {
        // No daemon listening. The standalone path bootstraps the
        // registry; an unknown audit name produces a clean error message
        // (rather than panicking) so the operator sees the available
        // names.
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("nope.sock");
        let workspace = dir.path().to_path_buf();
        let (_td_paths, paths) = crate::testing::test_daemon_paths();
        let res = execute_at(
            &socket,
            &paths,
            workspace,
            "does_not_exist".to_string(),
        )
        .await;
        let err = res.expect_err("unknown audit must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown audit `does_not_exist`"),
            "error must name unknown audit: {msg}"
        );
        // Hints at the registered names.
        assert!(
            msg.contains("architecture_brightline")
                || msg.contains("security_bug_audit"),
            "error must list registered audits: {msg}"
        );
    }
}
