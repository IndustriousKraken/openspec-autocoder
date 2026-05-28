//! Periodic-audit framework. Audits run on per-audit cadences AFTER the
//! polling loop's `recreate_branch` step AND BEFORE `list_pending`, so an
//! audit that writes new OpenSpec changes feeds the same iteration's
//! queue walk.
//!
//! Structure:
//! - [`Audit`] trait: each concrete audit implements `audit_type`,
//!   `requires_head_change`, `write_policy`, and `run`.
//! - [`AuditOutcome`]: `NoFindings | Reported(Vec<Finding>) | SpecsWritten`.
//! - [`AuditRegistry`]: holds the `Arc<dyn Audit>` list iterated by the
//!   scheduler.
//! - [`AuditLogWriter`]: per-invocation log file under
//!   `<logs_dir>/runs/<basename>/audits/<type>-<timestamp>.log` (the
//!   `logs_dir` here is the daemon's resolved logs root from
//!   `DaemonPaths`).
//! - [`state`]: persistence of `last_run_at` + `last_run_sha` per audit.
//! - [`scheduler`]: cadence + change-guard + write-policy enforcement.

pub mod architecture_consultative;
pub mod brightline;
pub mod drift;
pub mod missing_tests;
pub mod scheduler;
pub mod security_bug;
pub mod specs_writing;
pub mod state;
pub mod threads;
#[cfg(test)]
pub mod test_support;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{RepositoryConfig, ResolvedSandbox};
use crate::polling_loop::ChatOpsContext;

/// Per-attempt log section names for the validation retry loop. Public
/// so tests can grep for them.
pub const VALIDATION_ADDENDUM_PREFIX: &str =
    "Your previous response produced this proposal which failed openspec validation:";
pub const VALIDATION_ADDENDUM_SUFFIX: &str =
    "Please correct the proposal and reply with the full revised content.";

/// Cap on the chatops `❌` notification's quoted validation stderr. The
/// full stderr always lives in the audit-run log; chatops gets a slice.
pub const VALIDATION_ERROR_NOTIFICATION_CAP: usize = 800;

/// Cap on the `error_excerpt` field recorded in the audit-state history
/// for a `ValidationExhausted` outcome. Shorter than the chatops cap to
/// keep the state file bounded.
pub const VALIDATION_ERROR_HISTORY_EXCERPT: usize = 200;

/// What the audit is permitted to do to the workspace. The framework
/// enforces this via a post-hoc `git status --porcelain` check (and, for
/// audits invoking the wrapped Claude CLI, by passing tool restrictions
/// to the sandbox).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WritePolicy {
    /// Report-only. Sandbox blocks `Write`/`Edit`. Post-hoc check
    /// requires an empty diff; any non-empty diff means failure + revert
    /// via `git reset --hard HEAD` + chatops alert.
    None,
    /// Spec-writing audit. Sandbox allows `Write`/`Edit`. Post-hoc
    /// check requires every modified or new path to begin with
    /// `openspec/changes/`. Violations revert the entire diff via
    /// `git reset --hard HEAD` + `git clean -fd` + chatops alert.
    OpenSpecOnly,
    /// Full write access. Reserved for future audits with broader
    /// scope; not used by any audit landing in the foundation.
    Approved,
}

/// Severity of a single finding in a reported outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    /// Glyph used in chatops bullet lists.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Low => "•",
            Self::Medium => "⚠",
            Self::High => "🔴",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub subject: String,
    pub body: String,
    pub anchor: Option<String>,
}

/// Outcome of one audit's `run`. The scheduler dispatches on the variant:
/// `NoFindings` → silent; `Reported` → chatops post unless empty + clean
/// (controlled by `notify_on_clean`); `SpecsWritten` → info log only;
/// `ValidationExhausted` → WARN log + `❌` chatops notification (the
/// proposal was discarded, no commit made);
/// `WorkspaceUnavailable` → INFO log, no chatops, no cadence update
/// (the audit declined to run because the workspace is in a broken
/// state — the iteration-level workspace-init alert is the real signal).
#[derive(Debug, Clone)]
pub enum AuditOutcome {
    NoFindings,
    /// Successful audit run. `retries_used` is `0` when the audit's
    /// generated proposal (if any) validated on first attempt, and
    /// `>0` when it took N validation retries to land. Non-LLM-driven
    /// audits and audits that do not generate proposals always report
    /// `retries_used: 0`.
    Reported {
        findings: Vec<Finding>,
        retries_used: u32,
    },
    /// Spec-writing audit run. `retries_used` is the per-audit retry
    /// count used to land the validated set of change directories.
    SpecsWritten {
        changes: Vec<String>,
        retries_used: u32,
    },
    /// The audit's LLM produced a proposal that failed
    /// `openspec validate --strict` after exhausting the configured
    /// retry budget. The proposal directory was deleted and a chatops
    /// `❌` notification was posted. No commit was made.
    ValidationExhausted {
        audit_type: String,
        retries_attempted: u32,
        final_error: String,
    },
    /// The audit declined to run because the workspace is not in a
    /// valid state (directory missing OR no `.git/` subdirectory). No
    /// file IO, no LLM call, no state mutation: returning this variant
    /// means the audit's `run` exited immediately at the workspace-
    /// validity gate. The scheduler logs at INFO and does NOT update
    /// the cadence-state file — the next iteration's cadence check
    /// will re-evaluate and may try again if the workspace has become
    /// valid.
    WorkspaceUnavailable {
        audit_type: String,
        workspace_path: PathBuf,
        reason: String,
    },
}

impl AuditOutcome {
    /// Convenience constructor for a no-retries successful Reported
    /// outcome — used by the many sites that produced findings before
    /// the retry loop existed.
    pub fn reported(findings: Vec<Finding>) -> Self {
        Self::Reported {
            findings,
            retries_used: 0,
        }
    }

    /// Convenience constructor for a no-retries successful SpecsWritten
    /// outcome.
    #[allow(dead_code)]
    pub fn specs_written(changes: Vec<String>) -> Self {
        Self::SpecsWritten {
            changes,
            retries_used: 0,
        }
    }

    pub fn kind(&self) -> AuditOutcomeKind {
        match self {
            Self::NoFindings => AuditOutcomeKind::NoFindings,
            Self::Reported { .. } => AuditOutcomeKind::Reported,
            Self::SpecsWritten { .. } => AuditOutcomeKind::SpecsWritten,
            Self::ValidationExhausted { .. } => AuditOutcomeKind::ValidationExhausted,
            Self::WorkspaceUnavailable { .. } => AuditOutcomeKind::WorkspaceUnavailable,
        }
    }

    /// Retries used on this run, if any. Returns 0 for outcomes that
    /// have no retry semantics (NoFindings, WorkspaceUnavailable) and
    /// the carried value for the others. For `ValidationExhausted`,
    /// the value returned is `retries_attempted` (the run reached its
    /// budget without landing a valid proposal).
    pub fn retries_used(&self) -> u32 {
        match self {
            Self::NoFindings => 0,
            Self::Reported { retries_used, .. } => *retries_used,
            Self::SpecsWritten { retries_used, .. } => *retries_used,
            Self::ValidationExhausted {
                retries_attempted, ..
            } => *retries_attempted,
            Self::WorkspaceUnavailable { .. } => 0,
        }
    }
}

/// The kind portion of an `AuditOutcome` — what gets persisted in the
/// state file alongside `last_run_at` + `last_run_sha`. Carries no
/// payload so the state file stays compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcomeKind {
    NoFindings,
    Reported,
    SpecsWritten,
    ValidationExhausted,
    WorkspaceUnavailable,
}

impl AuditOutcomeKind {
    /// Human-readable label for log lines and the state file.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoFindings => "NoFindings",
            Self::Reported => "Reported",
            Self::SpecsWritten => "SpecsWritten",
            Self::ValidationExhausted => "ValidationExhausted",
            Self::WorkspaceUnavailable => "WorkspaceUnavailable",
        }
    }
}

/// Context handed to each audit's `run`. Carries the workspace path,
/// the resolved per-repo config, an optional chatops context (so an
/// audit may post directly if it wants to bypass the framework's
/// outcome dispatch, though most audits should let the scheduler post),
/// and the log writer that captures the audit's raw output.
pub struct AuditContext<'a> {
    pub workspace: &'a Path,
    pub repo: &'a RepositoryConfig,
    pub chatops_ctx: Option<&'a ChatOpsContext>,
    pub log_writer: AuditLogWriter,
    /// Number of retry attempts for the post-write
    /// `openspec validate --strict` loop. Resolved from
    /// [`crate::config::AuditsConfig::max_validation_retries`] at
    /// scheduler-dispatch time and clamped to
    /// [`crate::config::MAX_VALIDATION_RETRIES_CEILING`] at config-load.
    /// Audits that produce LLM-generated proposals consult this; audits
    /// that produce advisory findings (`drift`, `architecture_consultative`,
    /// `architecture_brightline`) ignore it.
    pub max_validation_retries: u32,
}

/// Periodic audit interface. Implementations are constructed once at
/// startup, wrapped in `Arc<dyn Audit>`, and registered in
/// [`AuditRegistry`]. The scheduler invokes `run` only when the cadence
/// has elapsed AND (if `requires_head_change()` is true) the recorded
/// `last_run_sha` differs from the current HEAD.
#[async_trait]
pub trait Audit: Send + Sync {
    /// Stable identifier used as the cadence-config key, state-file key,
    /// and log-file name prefix. Use `snake_case`.
    fn audit_type(&self) -> &'static str;

    /// One-line operator-facing description suitable for inline rendering
    /// in the install wizard (≤ 80 chars).
    fn description(&self) -> &'static str;

    /// When `true`, the scheduler skips this audit when the recorded
    /// `last_run_sha` matches the current base-branch HEAD even if the
    /// cadence interval has elapsed. Use `false` for audits whose
    /// inputs are external (package registries, GitHub PRs, etc.).
    fn requires_head_change(&self) -> bool;

    /// Sandbox + post-hoc diff policy. See [`WritePolicy`].
    fn write_policy(&self) -> WritePolicy;

    /// Run the audit. Errors propagate to the scheduler, which logs at
    /// ERROR, does NOT update the state file (so the cadence retriggers
    /// the audit next iteration), and continues to the next audit
    /// without aborting the polling iteration.
    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome>;
}

/// Append-only writer for the per-invocation audit log. Auto-creates
/// the destination directory on first use. Cloning yields a fresh
/// handle to the same underlying file; the inner `Mutex` lets multiple
/// borrows write without contention from the audit's perspective.
#[derive(Clone)]
pub struct AuditLogWriter {
    path: PathBuf,
    inner: Arc<Mutex<std::fs::File>>,
}

impl AuditLogWriter {
    /// Create a new log writer at
    /// `<logs_dir>/runs/<repo-sanitized>/audits/<audit_type>-<UTC-RFC3339-with-Z>.log`.
    /// The directory is created if absent. The per-repo subdir matches
    /// the per-change run-log layout (see
    /// [`crate::executor::claude_cli::run_log_path`]).
    pub fn open(workspace: &Path, audit_type: &str) -> Result<Self> {
        let basename = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let dir = crate::paths::current().audit_logs_dir(basename);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating audit log dir {}", dir.display()))?;
        // Format: type-<RFC3339-with-Z>.log. Replace ':' with '-' so the
        // filename is portable on case-insensitive filesystems.
        let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let safe_ts = timestamp.replace(':', "-");
        let path = dir.join(format!("{audit_type}-{safe_ts}.log"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening audit log {}", path.display()))?;
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(file)),
        })
    }

    /// Path of the on-disk log file. Tests use this; the scheduler reads
    /// it to surface log location info in tracing output.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a section header + body. Convenience wrapper around
    /// `Write::write_all` that prefixes a `## <header>` line.
    pub fn write_section(&self, header: &str, body: &str) -> Result<()> {
        let mut guard = self.inner.lock().expect("audit log mutex poisoned");
        writeln!(guard, "## {header}")?;
        writeln!(guard, "{body}")?;
        writeln!(guard)?;
        guard.flush()?;
        Ok(())
    }

    /// Append a raw block without a header.
    pub fn write_raw(&self, body: &str) -> Result<()> {
        let mut guard = self.inner.lock().expect("audit log mutex poisoned");
        guard.write_all(body.as_bytes())?;
        if !body.ends_with('\n') {
            writeln!(guard)?;
        }
        guard.flush()?;
        Ok(())
    }
}

/// Registry of all audits the daemon knows about. Built once at startup
/// in `cli::run::execute` and shared (via `Arc`) with every polling
/// task. The scheduler iterates `audits.iter()` in declaration order.
#[derive(Clone, Default)]
pub struct AuditRegistry {
    audits: Vec<Arc<dyn Audit>>,
}

impl AuditRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_audits(audits: Vec<Arc<dyn Audit>>) -> Self {
        Self { audits }
    }

    pub fn register(&mut self, audit: Arc<dyn Audit>) {
        self.audits.push(audit);
    }

    /// Iterator over registered audits in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Audit>> {
        self.audits.iter()
    }

    pub fn len(&self) -> usize {
        self.audits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.audits.is_empty()
    }

    /// Slugs of every registered audit type. Used by config validation
    /// to reject typos in `audits.defaults` and `repositories[].audits`.
    pub fn known_type_names(&self) -> Vec<&'static str> {
        self.audits.iter().map(|a| a.audit_type()).collect()
    }
}

/// RAII guard that removes a temp sandbox-settings file when dropped.
/// Returned alongside the on-disk path by [`write_sandbox_settings`].
/// Holding the guard until the spawned CLI has exited keeps the file
/// available; dropping it deletes the file even if the run errored or
/// panicked.
pub struct SandboxSettingsGuard(PathBuf);

impl SandboxSettingsGuard {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SandboxSettingsGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.0.display(),
                "failed to remove sandbox settings temp file: {e}"
            );
        }
    }
}

/// Write a one-shot Claude Code `--settings` file mirroring the same
/// `permissions.deny` structure used by [`crate::executor::claude_cli`].
/// The deny list is built from the sandbox's `disallowed_bash_patterns`
/// and `disallowed_read_paths` plus explicit `Write(*)` and `Edit(*)`
/// entries so audits whose `WritePolicy` is `None` have a defense-in-
/// depth backstop ahead of the post-hoc diff check.
///
/// `settings_dir` selects the directory the file is written to. Pass
/// `None` to use `std::env::temp_dir()`; tests pass a per-test
/// `TempDir` so concurrent runs do not collide on filename probes.
///
/// Returns the path and an RAII guard. Drop the guard AFTER the
/// spawned CLI has exited.
pub fn write_sandbox_settings(
    sandbox: &ResolvedSandbox,
    settings_dir: Option<&Path>,
) -> Result<(PathBuf, SandboxSettingsGuard)> {
    let mut deny: Vec<String> = Vec::new();
    deny.push("Write(*)".to_string());
    deny.push("Edit(*)".to_string());
    for pat in &sandbox.disallowed_bash_patterns {
        deny.push(format!("Bash({pat})"));
    }
    for pat in &sandbox.disallowed_read_paths {
        deny.push(format!("Read({pat})"));
    }
    let json = serde_json::json!({
        "permissions": {
            "allow": Vec::<String>::new(),
            "deny": deny,
        }
    });

    use std::time::{SystemTime, UNIX_EPOCH};
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir: PathBuf = settings_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("autocoder-audit-settings-{pid}-{stamp}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&json)?)
        .with_context(|| format!("writing audit sandbox settings to {}", path.display()))?;
    Ok((path.clone(), SandboxSettingsGuard(path)))
}

/// Returned by [`validate_with_retry`] on success: the proposal
/// validated, possibly after `retries_used` retry attempts.
///
/// The `specs-writing` audits implement their own retry loop inline
/// (because they produce a *set* of change dirs per run, not a single
/// `<slug>`). This struct + the [`validate_with_retry`] helper exist
/// for future single-proposal audits to consume; the API is part of
/// the spec for `a01-audit-proposal-self-validation`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryOutcome {
    pub retries_used: u32,
}

/// Returned by [`validate_with_retry`] when the retry budget was
/// exhausted before a valid proposal could be produced. Carries the
/// number of retry attempts made (i.e. the budget) and the final
/// validation-error string the audit can record / surface.
///
/// Reserved for future single-proposal audits; the `specs-writing`
/// audits build their own `AuditOutcome::ValidationExhausted` directly
/// from the per-attempt failures.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ValidationExhausted {
    pub retries_attempted: u32,
    pub final_error: String,
}

/// Shell out to `openspec validate <slug> --strict` in `workspace`.
/// Returns:
/// - `Ok(())` on exit 0.
/// - `Err(stderr)` on non-zero exit (trimmed stderr text).
/// - `Err("openspec validate spawn failed: ...")` on spawn failure.
///
/// Uses the `openspec` binary on `PATH`. Callers needing a test
/// override should use [`validate_proposal_with_command`].
///
/// Used directly by tests; the in-tree audit modules call this
/// through the inline validation step that runs after each LLM
/// invocation in [`crate::audits::specs_writing`].
#[allow(dead_code)]
pub fn validate_proposal(workspace: &Path, slug: &str) -> std::result::Result<(), String> {
    validate_proposal_with_command("openspec", workspace, slug)
}

/// Same as [`validate_proposal`] but with an injectable `openspec`
/// command path. Tests point at a shell script.
pub fn validate_proposal_with_command(
    openspec_command: &str,
    workspace: &Path,
    slug: &str,
) -> std::result::Result<(), String> {
    // Brief ETXTBSY retry. Linux races a parallel test's `std::fs::write`
    // of one shell script with this thread's `Command::spawn` of a
    // sibling script — see [`spawn_with_etxtbsy_retry`] for the longer
    // analysis. The same mitigation applies here in synchronous form.
    const MAX_ATTEMPTS: u32 = 8;
    let mut attempt: u32 = 0;
    let out = loop {
        let res = std::process::Command::new(openspec_command)
            .arg("validate")
            .arg(slug)
            .arg("--strict")
            .current_dir(workspace)
            .output();
        match res {
            Ok(o) => break Ok(o),
            Err(e)
                if e.raw_os_error() == Some(libc::ETXTBSY)
                    && attempt + 1 < MAX_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(20 * u64::from(attempt)));
                continue;
            }
            Err(e) => break Err(e),
        }
    };
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if stderr.is_empty() {
                // openspec failed but said nothing useful on stderr;
                // include stdout so the caller has something to forward.
                let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
                Err(if stdout.is_empty() {
                    format!("openspec validate {slug} --strict exited {}", o.status)
                } else {
                    stdout
                })
            } else {
                Err(stderr)
            }
        }
        Err(e) => Err(format!("openspec validate spawn failed: {e}")),
    }
}

/// Run the closure `llm_call` to write a proposal under
/// `openspec/changes/<slug>/`, then validate it. On validation failure
/// with retry budget remaining, re-invoke `llm_call` with `Some(<error>)`
/// so the audit can amend its LLM prompt with the validation error, and
/// retry. Returns `Ok(RetryOutcome)` on first or eventual success; on
/// exhaustion returns `Err(ValidationExhausted)`.
///
/// `llm_call`'s `Option<&str>` parameter is `None` on the first attempt
/// and `Some(validation_stderr)` on retries. The closure is responsible
/// for overwriting the change directory; the helper does not delete it
/// between attempts (audits typically delete-and-rewrite or rely on the
/// LLM producing a fresh response that overwrites the prior content).
///
/// Errors returned by `llm_call` propagate as `ValidationExhausted`
/// with `final_error` prefixed `"llm-call failed: "` and
/// `retries_attempted` set to the attempt index at which the call
/// failed (so a first-attempt LLM failure produces `retries_attempted:
/// 0`).
#[allow(dead_code)]
pub async fn validate_with_retry<F, Fut>(
    workspace: &Path,
    slug: &str,
    max_retries: u32,
    llm_call: F,
) -> std::result::Result<RetryOutcome, ValidationExhausted>
where
    F: FnMut(Option<&str>) -> Fut,
    Fut: Future<Output = std::result::Result<(), String>>,
{
    validate_with_retry_with_command("openspec", workspace, slug, max_retries, llm_call).await
}

/// Test/internal variant of [`validate_with_retry`] that takes the
/// `openspec` binary path. Production calls through to this with
/// `"openspec"`.
#[allow(dead_code)]
pub async fn validate_with_retry_with_command<F, Fut>(
    openspec_command: &str,
    workspace: &Path,
    slug: &str,
    max_retries: u32,
    mut llm_call: F,
) -> std::result::Result<RetryOutcome, ValidationExhausted>
where
    F: FnMut(Option<&str>) -> Fut,
    Fut: Future<Output = std::result::Result<(), String>>,
{
    let mut last_error: String = String::new();
    let total_attempts = max_retries.saturating_add(1);
    for attempt in 0..total_attempts {
        let addendum: Option<&str> = if attempt == 0 {
            None
        } else {
            Some(last_error.as_str())
        };
        if let Err(e) = llm_call(addendum).await {
            return Err(ValidationExhausted {
                retries_attempted: attempt,
                final_error: format!("llm-call failed: {e}"),
            });
        }
        match validate_proposal_with_command(openspec_command, workspace, slug) {
            Ok(()) => {
                return Ok(RetryOutcome {
                    retries_used: attempt,
                });
            }
            Err(e) => {
                last_error = e;
            }
        }
    }
    Err(ValidationExhausted {
        retries_attempted: max_retries,
        final_error: last_error,
    })
}

/// Render the validation error in the form callers should hand to the
/// LLM on the retry attempt. The text is shaped to match the
/// `<VALIDATION_ADDENDUM_PREFIX> <error> <VALIDATION_ADDENDUM_SUFFIX>`
/// pattern documented in the spec.
pub fn build_validation_addendum(validation_error: &str) -> String {
    format!(
        "{VALIDATION_ADDENDUM_PREFIX}\n\n{validation_error}\n\n{VALIDATION_ADDENDUM_SUFFIX}"
    )
}

/// Truncate `s` to at most `cap` characters, appending `…` when
/// truncation occurred. Counts unicode characters, not bytes, so the
/// result is always a valid string boundary.
pub fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(cap).collect();
        out.push('…');
        out
    }
}

/// Remove `<workspace>/openspec/changes/<slug>/` recursively (NotFound
/// is silently ignored). When `chatops_ctx` is `Some`, post the
/// documented `❌` failure notification. Notification failures are
/// logged but do not propagate — the discard path's purpose is to
/// clean up; surfacing a downstream chatops error would mask the
/// underlying validation failure.
///
/// `repo_url` is rendered into the notification text so operators can
/// tell which repo's audit fired the alert when one channel is shared.
#[allow(dead_code)]
pub async fn discard_proposal_and_notify(
    workspace: &Path,
    slug: &str,
    audit_type: &str,
    retries_attempted: u32,
    final_error: &str,
    chatops_ctx: Option<&ChatOpsContext>,
    repo_url: &str,
) -> Result<()> {
    let change_dir = workspace.join("openspec/changes").join(slug);
    if let Err(e) = std::fs::remove_dir_all(&change_dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            slug = %slug,
            audit_type = audit_type,
            path = %change_dir.display(),
            "failed to remove invalid proposal directory: {e}"
        );
    }
    if let Some(ctx) = chatops_ctx {
        let post_result = post_validation_exhausted_notification(
            ctx,
            repo_url,
            audit_type,
            retries_attempted,
            final_error,
        )
        .await;
        if let Err(e) = post_result {
            tracing::warn!(
                audit_type = audit_type,
                slug = %slug,
                "validation-exhausted chatops post failed: {e:#}"
            );
        }
    }
    Ok(())
}

/// Post the `❌` validation-exhausted notification via the threaded path
/// when the error is multi-line OR exceeds 300 characters; inline
/// otherwise. The top-line names the repo, audit type, and retry count;
/// the (optional) thread body carries the captured validation-error
/// excerpt + the closing instruction.
pub async fn post_validation_exhausted_notification(
    ctx: &ChatOpsContext,
    repo_url: &str,
    audit_type: &str,
    retries_attempted: u32,
    final_error: &str,
) -> Result<()> {
    let excerpt = truncate_chars(final_error, VALIDATION_ERROR_NOTIFICATION_CAP);
    let top_line = format_validation_exhausted_top_line(
        repo_url,
        audit_type,
        retries_attempted,
    );
    if should_thread_validation_error(&excerpt) {
        let thread_body = format!(
            "Final validation error:\n{excerpt}\nNo commit was made. The audit will retry on its next scheduled cadence."
        );
        ctx.chatops
            .post_notification_with_thread(&ctx.channel, &top_line, &thread_body)
            .await
            .map(|_| ())
    } else {
        let text = format_validation_exhausted_message(
            repo_url,
            audit_type,
            retries_attempted,
            final_error,
        );
        ctx.chatops.post_notification(&ctx.channel, &text).await
    }
}

/// Threading predicate for `ValidationExhausted` notifications: a
/// validation error spanning more than one line OR more than 300
/// characters routes through the threaded path. Single-line short
/// errors continue to use the inline single-message form.
pub fn should_thread_validation_error(excerpt: &str) -> bool {
    excerpt.lines().count() > 1 || excerpt.chars().count() > 300
}

/// Render the `❌` top-line shared by the inline + threaded notification
/// paths. The threaded path uses just this string; the inline path
/// composes it with the validation-error body via
/// [`format_validation_exhausted_message`].
pub fn format_validation_exhausted_top_line(
    repo_url: &str,
    audit_type: &str,
    retries_attempted: u32,
) -> String {
    format!(
        "❌ {repo_url}: {audit_type} produced an invalid proposal that failed openspec validation after {retries_attempted} retries."
    )
}

/// Render the inline single-message form of the `❌` validation-exhausted
/// notification. Used by the inline-path branch of
/// [`post_validation_exhausted_notification`] AND by callers that want
/// the legacy single-message rendering directly.
pub fn format_validation_exhausted_message(
    repo_url: &str,
    audit_type: &str,
    retries_attempted: u32,
    final_error: &str,
) -> String {
    let excerpt = truncate_chars(final_error, VALIDATION_ERROR_NOTIFICATION_CAP);
    let top_line = format_validation_exhausted_top_line(repo_url, audit_type, retries_attempted);
    format!(
        "{top_line}\n\
         Final validation error:\n{excerpt}\nNo commit was made. The audit will retry on its next scheduled cadence."
    )
}

/// Cap on the `why_excerpt` rendered into the `🔍 created proposal`
/// chatops notification. Longer excerpts are truncated to this many
/// characters and have `…` appended.
pub const PROPOSAL_CREATED_WHY_EXCERPT_CAP: usize = 200;

/// Render the documented `🔍` notification text shown when an LLM-
/// driven audit's just-written proposal passes `openspec validate
/// --strict`. The `why_excerpt` is truncated to
/// [`PROPOSAL_CREATED_WHY_EXCERPT_CAP`] characters with an ellipsis
/// when longer. When `retries_used > 0`, a trailing parenthetical
/// names the retry that landed it; on first-attempt success the
/// parenthetical is omitted.
pub fn format_proposal_created_message(
    repo_url: &str,
    audit_type: &str,
    change_slug: &str,
    why_excerpt: &str,
    retries_used: u32,
    max_retries: u32,
) -> String {
    let excerpt = truncate_chars(why_excerpt, PROPOSAL_CREATED_WHY_EXCERPT_CAP);
    let mut out = format!(
        "🔍 {repo_url}: {audit_type} created proposal `{change_slug}` — {excerpt}"
    );
    if retries_used > 0 {
        out.push_str(&format!(
            " (validated on retry {retries_used} of {max_retries})"
        ));
    }
    out
}

/// Post the `🔍 created proposal` chatops notification documented in
/// `a02-audit-proposal-created-notification`. Fires after the audit's
/// just-written proposal validates and BEFORE the proposal is committed
/// to git / returned to the scheduler. When `chatops_ctx` is `None`
/// (chatops not configured), the function returns silently — mirroring
/// every other chatops-optional notification site in the daemon. When
/// `post_notification` itself errors, the failure is logged at WARN
/// and the function returns; the audit's success outcome is unaffected.
pub async fn post_proposal_created_notification(
    chatops_ctx: Option<&ChatOpsContext>,
    repo_url: &str,
    audit_type: &str,
    change_slug: &str,
    why_excerpt: &str,
    retries_used: u32,
    max_retries: u32,
) {
    let Some(ctx) = chatops_ctx else { return };
    let text = format_proposal_created_message(
        repo_url,
        audit_type,
        change_slug,
        why_excerpt,
        retries_used,
        max_retries,
    );
    if let Err(e) = ctx.chatops.post_notification(&ctx.channel, &text).await {
        tracing::warn!(
            audit_type = audit_type,
            slug = %change_slug,
            "proposal-created chatops post failed: {e:#}"
        );
    }
}

/// Read `<workspace>/openspec/changes/<slug>/proposal.md`, extract its
/// `## Why` section, and return the first non-empty line. Returns an
/// empty string when the file is missing, unreadable, or has no
/// non-empty body under the `## Why` heading — callers feed the empty
/// string through to the notification (the formatted text degrades
/// gracefully to "— "). Mirrors the logic the polling loop uses for
/// the start-of-work notification; kept here so the audit framework
/// does not have to depend on a polling-loop-private helper.
pub fn read_proposal_why_first_line(workspace: &Path, slug: &str) -> String {
    let proposal = workspace
        .join("openspec/changes")
        .join(slug)
        .join("proposal.md");
    let raw = match std::fs::read_to_string(&proposal) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    first_line_of_why(&raw).unwrap_or_default()
}

/// Pull the first non-empty line out of the `## Why` section in a
/// `proposal.md` body. Returns `None` when no `## Why` heading is
/// present OR the section has no non-empty body line. Matches the
/// shape of `polling_loop::first_line_of_section(_, "## Why")` so the
/// two helpers stay in lock-step.
fn first_line_of_why(text: &str) -> Option<String> {
    let mut in_section = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.trim_start().starts_with("## ") {
            in_section = line.trim_start() == "## Why";
            continue;
        }
        if in_section {
            let stripped = line.trim();
            if !stripped.is_empty() {
                return Some(stripped.to_string());
            }
        }
    }
    None
}

/// Spawn a child process, retrying briefly on `ETXTBSY`.
///
/// Linux returns `ETXTBSY` when a `Command::spawn` execve targets a file
/// that any process currently holds open for write. With many parallel
/// tests writing short-lived shell scripts and immediately spawning
/// them, this race can fire — one test's `fork()` (inside `spawn`) can
/// inherit another thread's writable fd to its own to-be-exec'd script
/// during the brief window between `std::fs::write` returning and the
/// `File` being dropped. The inherited fd dies on `execve` (Rust opens
/// files with `O_CLOEXEC`), but until `execve` happens, the kernel sees
/// the file as busy and refuses the exec on it from any other process.
///
/// The window is microseconds. A short retry loop closes it without
/// needing to serialize the test suite. Tied to `docs/test-reliability.md`
/// entry "ETXTBSY from concurrent audit-CLI fixtures".
pub async fn spawn_with_etxtbsy_retry<F>(
    mut build: F,
) -> std::io::Result<tokio::process::Child>
where
    F: FnMut() -> tokio::process::Command,
{
    const MAX_ATTEMPTS: u32 = 8;
    let mut attempt: u32 = 0;
    loop {
        match build().spawn() {
            Ok(child) => return Ok(child),
            Err(e)
                if e.raw_os_error() == Some(libc::ETXTBSY)
                    && attempt + 1 < MAX_ATTEMPTS =>
            {
                attempt += 1;
                let backoff = std::time::Duration::from_millis(20 * u64::from(attempt));
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Cheap precondition every audit runs at the top of its `run` method.
/// "Valid" means the workspace directory exists AND it contains a
/// `.git/` subdirectory. The check is a single stat per path; it
/// performs NO file IO beyond `Path::is_dir` and never touches `fs::
/// create_dir_all` (the very call this gate exists to prevent on a
/// broken workspace).
///
/// Known limitation: git-worktree workspaces use a `.git` *file*
/// (containing `gitdir: <path>`) rather than a directory. Autocoder's
/// production workspaces are normal clones, so the directory check is
/// correct for every operator-configured workspace today. If autocoder
/// ever supports worktree-rooted workspaces, this check needs to allow
/// the file form too.
pub fn workspace_is_valid(workspace: &Path) -> bool {
    workspace.is_dir() && workspace.join(".git").is_dir()
}

/// Build the documented `WorkspaceUnavailable` outcome for `workspace`
/// and emit the single INFO log line every audit shares. Returns the
/// outcome variant the caller's `Audit::run` returns immediately.
///
/// The reason string is one of three fixed values, picked by the
/// specific precondition that failed:
/// - `"workspace directory does not exist"` when `workspace.exists()`
///   returns false.
/// - `"workspace exists but has no .git/ subdirectory"` when the
///   directory is present but `<workspace>/.git` is not a directory.
/// - `"workspace failed validity check"` is the catch-all reserved for
///   future checks (e.g. supporting `.git` files for worktrees).
///
/// The variant tag matches the documented strings in the
/// `audits-require-valid-workspace` spec; callers should not invent
/// alternate phrasings.
pub fn workspace_unavailable_outcome(
    audit_type: &str,
    workspace: &Path,
) -> AuditOutcome {
    let reason = if !workspace.exists() {
        "workspace directory does not exist".to_string()
    } else if !workspace.join(".git").is_dir() {
        "workspace exists but has no .git/ subdirectory".to_string()
    } else {
        "workspace failed validity check".to_string()
    };
    tracing::info!(
        audit_type = %audit_type,
        workspace = %workspace.display(),
        reason = %reason,
        "audit skipped: workspace not in a valid state"
    );
    AuditOutcome::WorkspaceUnavailable {
        audit_type: audit_type.to_string(),
        workspace_path: workspace.to_path_buf(),
        reason,
    }
}

// =====================================================================
// Audit notification formatter (chatops-audit-findings-in-threads)
// =====================================================================

/// Threading threshold: notifications whose `thread_body` exceeds either
/// of these dimensions warrant a thread; otherwise the body inlines into
/// the top-line message. Documented in the `chatops-manager` spec's
/// "Audit findings post via the threaded-notification path …" requirement.
pub const AUDIT_THREAD_LINE_THRESHOLD: usize = 3;
pub const AUDIT_THREAD_CHAR_THRESHOLD: usize = 300;

/// Slack's per-message limit is 40,000 characters; cap the thread body
/// at 35,000 to leave a 5,000-char safety margin for any backend-side
/// envelope overhead.
pub const AUDIT_THREAD_BODY_CHAR_CAP: usize = 35_000;

/// Output of [`format_audit_notification`]. The scheduler decides which
/// `ChatOpsBackend` method to invoke based on `should_thread`:
/// `true`  → `post_notification_with_thread(top_line, thread_body)`;
/// `false` → `post_notification(<top_line + body>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditNotification {
    pub top_line: String,
    pub thread_body: String,
    pub should_thread: bool,
}

/// Build the per-audit-type chatops notification (top-line summary,
/// full findings body, and threading decision). `notify_on_clean`
/// distinguishes "audit ran clean, post the ✅ form" from "audit ran
/// clean, post nothing" — the latter case is gated by the scheduler
/// before calling this helper.
///
/// `now` is plumbed in so tests can pin the audit_id used in truncation
/// pointers; production callers pass `chrono::Utc::now()`.
pub fn format_audit_notification(
    audit_type: &str,
    repo_url: &str,
    findings: &[Finding],
    notify_on_clean: bool,
    now: chrono::DateTime<Utc>,
) -> AuditNotification {
    let top_line =
        format_audit_top_line(audit_type, repo_url, findings, notify_on_clean);
    let mut thread_body = if findings.is_empty() {
        String::new()
    } else {
        format_audit_thread_body(findings)
    };

    // Length cap: truncate to 35,000 chars and append a pointer naming
    // the audit_id operators can grep in the daemon log.
    if thread_body.chars().count() > AUDIT_THREAD_BODY_CHAR_CAP {
        let audit_id = make_audit_id(repo_url, audit_type, now);
        let truncated: String = thread_body
            .chars()
            .take(AUDIT_THREAD_BODY_CHAR_CAP)
            .collect();
        thread_body = format!(
            "{truncated}\n\n… [truncated; full findings at journalctl -u autocoder | grep audit_id={audit_id}]"
        );
    }

    let should_thread = thread_body.lines().count() > AUDIT_THREAD_LINE_THRESHOLD
        || thread_body.chars().count() > AUDIT_THREAD_CHAR_THRESHOLD;

    AuditNotification {
        top_line,
        thread_body,
        should_thread,
    }
}

/// Build the per-audit-type top-line string. Documented shapes:
/// - `architecture_brightline`: `📐 architecture_brightline on <repo>: <N> file(s) over line threshold; <M> duplicate signature(s)`
/// - `drift_audit`: `🧭 drift_audit on <repo>: <N> spec/code divergence(s) detected`
/// - other audits: generic `📋 <audit_type> on <repo>: <N> finding(s)`
///
/// Empty findings with `notify_on_clean=true` → uniform
/// `✅ <audit_type> on <repo>: no findings`.
fn format_audit_top_line(
    audit_type: &str,
    repo_url: &str,
    findings: &[Finding],
    notify_on_clean: bool,
) -> String {
    if findings.is_empty() && notify_on_clean {
        return format!("✅ {audit_type} on {repo_url}: no findings");
    }
    match audit_type {
        "architecture_brightline" => {
            let (files, dupes) = count_brightline_findings(findings);
            format!(
                "📐 architecture_brightline on {repo_url}: {files} file(s) over line threshold; {dupes} duplicate signature(s)"
            )
        }
        "drift_audit" => {
            format!(
                "🧭 drift_audit on {repo_url}: {n} spec/code divergence(s) detected",
                n = findings.len()
            )
        }
        _ => {
            format!(
                "📋 {audit_type} on {repo_url}: {n} finding(s)",
                n = findings.len()
            )
        }
    }
}

/// Partition brightline findings by subject shape. Files-over-threshold
/// subjects start with `"file "` and contain `" lines (threshold:"`;
/// duplicate-signature subjects start with `"duplicate signature "`.
/// Any other finding shape falls into neither bucket and is not counted
/// in either total (the per-finding body still appears in the thread).
fn count_brightline_findings(findings: &[Finding]) -> (usize, usize) {
    let mut files = 0usize;
    let mut dupes = 0usize;
    for f in findings {
        let s = f.subject.as_str();
        if s.starts_with("file ") && s.contains(" lines (threshold:") {
            files += 1;
        } else if s.starts_with("duplicate signature ") {
            dupes += 1;
        }
    }
    (files, dupes)
}

/// Render the per-finding body the thread reply carries. Same shape as
/// the prior `format_findings_message` body (severity glyph + subject +
/// optional `(anchor)`) so operators reading the thread see the same
/// content they used to see inline.
fn format_audit_thread_body(findings: &[Finding]) -> String {
    let mut out = String::new();
    for (i, f) in findings.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let glyph = f.severity.glyph();
        out.push_str("  ");
        out.push_str(glyph);
        out.push(' ');
        out.push_str(&f.subject);
        if let Some(anchor) = f.anchor.as_deref() {
            out.push_str(" (");
            out.push_str(anchor);
            out.push(')');
        }
    }
    out
}

/// Deterministic id used in the truncation pointer. Shape:
/// `<repo-sanitized>:<audit-type>:<utc-timestamp>`. The audit-runner
/// stamps this same id into its daemon log entries so operators can
/// grep the daemon log for the full content.
pub fn make_audit_id(
    repo_url: &str,
    audit_type: &str,
    now: chrono::DateTime<Utc>,
) -> String {
    let sanitized = sanitize_for_audit_id(repo_url);
    let timestamp = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!("{sanitized}:{audit_type}:{timestamp}")
}

/// Sanitize a repo URL into a token safe for a shell grep argument:
/// replace any character outside `[A-Za-z0-9._-]` with `_`. Mirrors the
/// workspace-basename sanitisation pattern used elsewhere in the daemon.
fn sanitize_for_audit_id(repo_url: &str) -> String {
    let mut out = String::with_capacity(repo_url.len());
    for c in repo_url.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::test_support::{RecordingBackend, make_recording_ctx};
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn format_proposal_created_message_first_attempt_omits_parenthetical() {
        let msg = format_proposal_created_message(
            "git@github.com:o/r.git",
            "security_bug_audit",
            "secure-bound-arp-step-count",
            "Operator must know which audit generated a queue-bound change",
            0,
            1,
        );
        assert!(msg.starts_with('🔍'));
        assert!(msg.contains("git@github.com:o/r.git"));
        assert!(msg.contains("security_bug_audit"));
        assert!(msg.contains("`secure-bound-arp-step-count`"));
        assert!(msg.contains("Operator must know"));
        assert!(
            !msg.contains("validated on retry"),
            "first-attempt success must omit retry parenthetical: {msg}"
        );
    }

    #[test]
    fn format_proposal_created_message_after_retry_appends_parenthetical() {
        let msg = format_proposal_created_message(
            "u",
            "missing_tests_audit",
            "tests-add-poller-edge-cases",
            "Cover the timeout race",
            2,
            3,
        );
        assert!(
            msg.contains("(validated on retry 2 of 3)"),
            "retry case must include the documented parenthetical: {msg}"
        );
    }

    #[test]
    fn format_proposal_created_message_truncates_long_why_excerpt() {
        let long = "x".repeat(500);
        let msg = format_proposal_created_message(
            "u",
            "security_bug_audit",
            "secure-x",
            &long,
            0,
            1,
        );
        assert!(
            msg.contains('…'),
            "long excerpt must be truncated with an ellipsis: {msg}"
        );
        // Cap is PROPOSAL_CREATED_WHY_EXCERPT_CAP chars + 1 for the
        // ellipsis; the rest of the message header is bounded so the
        // total stays under 500.
        assert!(msg.chars().count() < 500, "truncated msg should fit: {}", msg.chars().count());
    }

    #[tokio::test]
    async fn post_proposal_created_notification_is_no_op_when_chatops_absent() {
        // No panic, no log assertion harness — just confirm the call
        // returns and does not blow up when the daemon has no chatops
        // backend configured.
        post_proposal_created_notification(
            None,
            "u",
            "security_bug_audit",
            "secure-x",
            "why",
            0,
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn post_proposal_created_notification_posts_documented_text_with_chatops() {
        let backend = Arc::new(RecordingBackend::new());
        let ctx = make_recording_ctx(backend.clone());
        post_proposal_created_notification(
            Some(&ctx),
            "git@github.com:o/r.git",
            "security_bug_audit",
            "secure-bound-arp-step-count",
            "Operator must know which audit produced a change",
            0,
            1,
        )
        .await;
        let calls = backend.calls();
        assert_eq!(calls.len(), 1, "exactly one notification per call: {calls:?}");
        assert_eq!(calls[0].channel, "C_AUDIT_TEST", "posts to the resolved channel");
        let text = &calls[0].text;
        assert!(text.starts_with('🔍'));
        assert!(text.contains("security_bug_audit"));
        assert!(text.contains("`secure-bound-arp-step-count`"));
        assert!(text.contains("Operator must know"));
        assert!(!text.contains("validated on retry"));
    }

    #[tokio::test]
    async fn post_proposal_created_notification_retry_clause_appears_in_post() {
        let backend = Arc::new(RecordingBackend::new());
        let ctx = make_recording_ctx(backend.clone());
        post_proposal_created_notification(
            Some(&ctx),
            "u",
            "missing_tests_audit",
            "tests-foo",
            "y",
            1,
            2,
        )
        .await;
        let calls = backend.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].text.contains("(validated on retry 1 of 2)"),
            "retry parenthetical must reach the channel: {}",
            calls[0].text
        );
    }

    #[tokio::test]
    async fn post_proposal_created_notification_swallows_backend_errors() {
        // The chatops post fails; the helper must not propagate. Audit
        // success is unaffected by missed channel signals.
        let backend = Arc::new(RecordingBackend::failing("simulated chatops failure"));
        let ctx = make_recording_ctx(backend);
        post_proposal_created_notification(
            Some(&ctx),
            "u",
            "security_bug_audit",
            "secure-x",
            "y",
            0,
            1,
        )
        .await;
    }

    #[test]
    fn read_proposal_why_first_line_extracts_first_nonblank_line() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let slug = "feature-a";
        let dir = ws.join("openspec/changes").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("proposal.md"),
            "## Why\n\nWhy line one with detail\n\n## What Changes\n",
        )
        .unwrap();
        let got = read_proposal_why_first_line(ws, slug);
        assert_eq!(got, "Why line one with detail");
    }

    #[test]
    fn read_proposal_why_first_line_returns_empty_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(read_proposal_why_first_line(tmp.path(), "no-such-change").is_empty());
    }

    #[test]
    fn read_proposal_why_first_line_returns_empty_when_section_absent() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let slug = "feature-b";
        let dir = ws.join("openspec/changes").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proposal.md"), "## What Changes\n- x\n").unwrap();
        assert!(read_proposal_why_first_line(ws, slug).is_empty());
    }

    #[test]
    fn log_writer_creates_dir_and_writes() {
        let dir = TempDir::new().unwrap();
        // Use a fake workspace path with a unique basename.
        let basename = format!("test-ws-{}", uuid::Uuid::new_v4());
        let workspace = dir.path().join(&basename);
        std::fs::create_dir_all(&workspace).unwrap();
        let writer = AuditLogWriter::open(&workspace, "architecture_brightline")
            .expect("log open succeeds");
        writer.write_section("prompt", "(none)").unwrap();
        writer.write_section("output", "no findings").unwrap();
        let path = writer.path().to_path_buf();
        assert!(path.exists(), "log file must exist: {}", path.display());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("## prompt"));
        assert!(contents.contains("(none)"));
        assert!(contents.contains("## output"));
        assert!(contents.contains("no findings"));
        // Path lives under <logs_dir>/runs/<basename>/audits/...
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("/audits/"),
            "log must live under audits/: {path_str}"
        );
        assert!(
            path_str.contains(&basename),
            "log path must include workspace basename: {path_str}"
        );
        // Cleanup: remove the directory we created under /tmp.
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap());
        }
    }

    #[test]
    fn registry_iterates_in_declaration_order() {
        struct Fake(&'static str);
        #[async_trait]
        impl Audit for Fake {
            fn audit_type(&self) -> &'static str {
                self.0
            }
            fn description(&self) -> &'static str {
                "fake audit for tests"
            }
            fn requires_head_change(&self) -> bool {
                true
            }
            fn write_policy(&self) -> WritePolicy {
                WritePolicy::None
            }
            async fn run(&self, _ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
                Ok(AuditOutcome::NoFindings)
            }
        }
        let mut reg = AuditRegistry::new();
        reg.register(Arc::new(Fake("a")));
        reg.register(Arc::new(Fake("b")));
        reg.register(Arc::new(Fake("c")));
        let names: Vec<_> = reg.iter().map(|a| a.audit_type()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(reg.known_type_names(), vec!["a", "b", "c"]);
        assert_eq!(reg.len(), 3);
    }

    #[test]
    fn outcome_kind_round_trip() {
        assert_eq!(
            AuditOutcome::NoFindings.kind(),
            AuditOutcomeKind::NoFindings
        );
        assert_eq!(
            AuditOutcome::reported(vec![]).kind(),
            AuditOutcomeKind::Reported
        );
        assert_eq!(
            AuditOutcome::specs_written(vec!["x".into()]).kind(),
            AuditOutcomeKind::SpecsWritten
        );
        assert_eq!(
            AuditOutcome::ValidationExhausted {
                audit_type: "a".into(),
                retries_attempted: 1,
                final_error: "e".into(),
            }
            .kind(),
            AuditOutcomeKind::ValidationExhausted
        );
        assert_eq!(
            AuditOutcome::WorkspaceUnavailable {
                audit_type: "a".into(),
                workspace_path: PathBuf::from("/no/such/path"),
                reason: "workspace directory does not exist".into(),
            }
            .kind(),
            AuditOutcomeKind::WorkspaceUnavailable
        );
    }

    #[test]
    fn retries_used_returns_inner_value_for_each_variant() {
        assert_eq!(AuditOutcome::NoFindings.retries_used(), 0);
        assert_eq!(
            AuditOutcome::Reported {
                findings: vec![],
                retries_used: 2
            }
            .retries_used(),
            2
        );
        assert_eq!(
            AuditOutcome::SpecsWritten {
                changes: vec![],
                retries_used: 3
            }
            .retries_used(),
            3
        );
        assert_eq!(
            AuditOutcome::ValidationExhausted {
                audit_type: "x".into(),
                retries_attempted: 4,
                final_error: "boom".into()
            }
            .retries_used(),
            4
        );
        assert_eq!(
            AuditOutcome::WorkspaceUnavailable {
                audit_type: "x".into(),
                workspace_path: PathBuf::from("/no/such/path"),
                reason: "workspace directory does not exist".into(),
            }
            .retries_used(),
            0
        );
    }

    #[test]
    fn workspace_is_valid_returns_false_for_nonexistent_path() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(!workspace_is_valid(&missing));
    }

    #[test]
    fn workspace_is_valid_returns_false_for_file_not_directory() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a-file");
        std::fs::write(&file, "i am a file").unwrap();
        assert!(!workspace_is_valid(&file));
    }

    #[test]
    fn workspace_is_valid_returns_false_for_directory_without_dot_git() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(!workspace_is_valid(&ws));
    }

    #[test]
    fn workspace_is_valid_returns_false_when_dot_git_is_a_file() {
        // git-worktree case: `.git` is a file (e.g. `gitdir: ...`). The
        // autocoder's production workspaces are normal clones so this
        // remains a deliberate false; the limitation is documented on
        // `workspace_is_valid`.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert!(!workspace_is_valid(&ws));
    }

    #[test]
    fn workspace_is_valid_returns_true_for_directory_with_dot_git_subdir() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        assert!(workspace_is_valid(&ws));
    }

    #[test]
    fn workspace_unavailable_outcome_uses_nonexistent_reason_for_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("nope");
        let outcome = workspace_unavailable_outcome("some_audit", &ws);
        match outcome {
            AuditOutcome::WorkspaceUnavailable {
                audit_type,
                workspace_path,
                reason,
            } => {
                assert_eq!(audit_type, "some_audit");
                assert_eq!(workspace_path, ws);
                assert_eq!(reason, "workspace directory does not exist");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn workspace_unavailable_outcome_uses_no_git_reason_for_dir_without_dot_git() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let outcome = workspace_unavailable_outcome("some_audit", &ws);
        match outcome {
            AuditOutcome::WorkspaceUnavailable { reason, .. } => {
                assert_eq!(reason, "workspace exists but has no .git/ subdirectory");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn truncate_chars_caps_and_appends_ellipsis() {
        let s = "x".repeat(500);
        let t = truncate_chars(&s, 100);
        assert!(t.ends_with('…'));
        assert_eq!(t.chars().count(), 101);
        let short = truncate_chars("hi", 100);
        assert_eq!(short, "hi");
    }

    #[test]
    fn build_validation_addendum_contains_prefix_suffix_and_error() {
        let s = build_validation_addendum("missing SHALL in requirement body");
        assert!(s.contains(VALIDATION_ADDENDUM_PREFIX));
        assert!(s.contains(VALIDATION_ADDENDUM_SUFFIX));
        assert!(s.contains("missing SHALL in requirement body"));
    }

    #[test]
    fn format_validation_exhausted_message_shape() {
        let msg = format_validation_exhausted_message(
            "git@github.com:o/r.git",
            "security_bug_audit",
            1,
            "stderr text",
        );
        assert!(msg.starts_with("❌"));
        assert!(msg.contains("git@github.com:o/r.git"));
        assert!(msg.contains("security_bug_audit"));
        assert!(msg.contains("1 retries"));
        assert!(msg.contains("stderr text"));
        assert!(msg.contains("No commit was made"));
    }

    #[test]
    fn format_validation_exhausted_message_truncates_long_stderr() {
        let huge = "z".repeat(2000);
        let msg = format_validation_exhausted_message("u", "t", 1, &huge);
        assert!(msg.contains('…'), "long stderr should be truncated: {msg}");
        // Bounded length: header + truncated stderr (cap+1 for ellipsis) +
        // footer fits well under e.g. 1500 chars.
        assert!(msg.chars().count() < 1500, "msg too long: {}", msg.chars().count());
    }

    #[tokio::test]
    async fn validate_with_retry_returns_ok_when_first_attempt_validates() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/feature-a")).unwrap();
        let validator = ws.join("ok.sh");
        std::fs::write(&validator, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&validator).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&validator, perms).unwrap();
        let calls = Arc::new(Mutex::new(0u32));
        let calls_inner = calls.clone();
        let res = validate_with_retry_with_command(
            &validator.to_string_lossy(),
            ws,
            "feature-a",
            0,
            move |addendum| {
                let calls_inner = calls_inner.clone();
                let captured = addendum.map(|s| s.to_string());
                async move {
                    *calls_inner.lock().unwrap() += 1;
                    assert!(captured.is_none(), "first call must have no addendum");
                    Ok::<_, String>(())
                }
            },
        )
        .await;
        let outcome = res.expect("valid first attempt returns Ok");
        assert_eq!(outcome.retries_used, 0);
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn validate_with_retry_exhausts_with_zero_retries_when_invalid() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/feature-b")).unwrap();
        let validator = ws.join("bad.sh");
        std::fs::write(
            &validator,
            "#!/bin/sh\necho 'MODIFIED header not found' >&2\nexit 2\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&validator).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&validator, perms).unwrap();
        let res = validate_with_retry_with_command(
            &validator.to_string_lossy(),
            ws,
            "feature-b",
            0,
            |_| async { Ok::<_, String>(()) },
        )
        .await;
        let err = res.expect_err("invalid w/ 0 retries → ValidationExhausted");
        assert_eq!(err.retries_attempted, 0);
        assert!(
            err.final_error.contains("MODIFIED header not found"),
            "final_error must carry the validator stderr: {}",
            err.final_error
        );
    }

    #[tokio::test]
    async fn validate_with_retry_passes_addendum_to_retry_call() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/feature-c")).unwrap();
        // Validator: fails first time, succeeds second time. Uses a
        // marker file inside the workspace (deterministic path; tied to
        // this test's TempDir so concurrent tests cannot collide).
        let mark = ws.join(".retry-toggle-mark");
        let validator = ws.join("toggle.sh");
        let body = format!(
            "#!/bin/sh\nMARK='{}'\nif [ ! -f \"$MARK\" ]; then\n  touch \"$MARK\"\n  echo 'missing SHALL keyword' >&2\n  exit 2\nfi\nrm -f \"$MARK\"\nexit 0\n",
            mark.display()
        );
        std::fs::write(&validator, body).unwrap();
        let mut perms = std::fs::metadata(&validator).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&validator, perms).unwrap();

        let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let seen_inner = seen.clone();
        let res = validate_with_retry_with_command(
            &validator.to_string_lossy(),
            ws,
            "feature-c",
            1,
            move |addendum| {
                let seen_inner = seen_inner.clone();
                let captured: Option<String> = addendum.map(|s| s.to_string());
                async move {
                    seen_inner.lock().unwrap().push(captured);
                    Ok::<_, String>(())
                }
            },
        )
        .await;
        let outcome = res.expect("retry should land");
        assert_eq!(outcome.retries_used, 1);
        let log = seen.lock().unwrap();
        assert_eq!(log.len(), 2, "must invoke llm_call twice");
        assert!(log[0].is_none(), "first call: no addendum");
        let addendum = log[1].as_deref().expect("second call: addendum");
        assert!(
            addendum.contains("missing SHALL keyword"),
            "addendum must carry the validator's stderr: {addendum}"
        );
    }

    #[tokio::test]
    async fn validate_with_retry_exhausts_after_max_retries() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/feature-d")).unwrap();
        let validator = ws.join("always-fail.sh");
        std::fs::write(
            &validator,
            "#!/bin/sh\necho 'never valid' >&2\nexit 2\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&validator).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&validator, perms).unwrap();
        let calls = Arc::new(Mutex::new(0u32));
        let calls_inner = calls.clone();
        let res = validate_with_retry_with_command(
            &validator.to_string_lossy(),
            ws,
            "feature-d",
            1,
            move |_| {
                let calls_inner = calls_inner.clone();
                async move {
                    *calls_inner.lock().unwrap() += 1;
                    Ok::<_, String>(())
                }
            },
        )
        .await;
        let err = res.expect_err("exhausted retries");
        assert_eq!(err.retries_attempted, 1);
        assert!(err.final_error.contains("never valid"));
        assert_eq!(*calls.lock().unwrap(), 2, "max_retries=1 → 2 total LLM calls");
    }

    #[tokio::test]
    async fn validate_with_retry_two_retries_valid_on_third() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/feature-e")).unwrap();
        let validator = ws.join("third-wins.sh");
        let counter_path = ws.join(".attempt-counter");
        std::fs::write(&counter_path, "0").unwrap();
        let body = format!(
            "#!/bin/sh\nC=\"$(cat '{}')\"\nN=$((C+1))\necho \"$N\" > '{}'\nif [ \"$N\" -lt 3 ]; then\n  echo \"attempt $N invalid\" >&2\n  exit 2\nfi\nexit 0\n",
            counter_path.display(),
            counter_path.display(),
        );
        std::fs::write(&validator, body).unwrap();
        let mut perms = std::fs::metadata(&validator).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&validator, perms).unwrap();

        let res = validate_with_retry_with_command(
            &validator.to_string_lossy(),
            ws,
            "feature-e",
            2,
            |_| async { Ok::<_, String>(()) },
        )
        .await;
        let outcome = res.expect("valid on third attempt");
        assert_eq!(outcome.retries_used, 2);
    }

    #[tokio::test]
    async fn discard_proposal_and_notify_removes_dir_and_no_panic_without_chatops() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let target = ws.join("openspec/changes/to-discard/proposal.md");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "content").unwrap();
        discard_proposal_and_notify(
            ws,
            "to-discard",
            "security_bug_audit",
            1,
            "validation error",
            None,
            "git@github.com:o/r.git",
        )
        .await
        .expect("discard ok");
        assert!(!ws.join("openspec/changes/to-discard").exists());
    }

    #[tokio::test]
    async fn discard_proposal_and_notify_handles_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        // No directory exists; helper must not panic.
        discard_proposal_and_notify(
            ws,
            "never-existed",
            "missing_tests_audit",
            1,
            "validation error",
            None,
            "u",
        )
        .await
        .expect("ok even when dir absent");
    }

    #[test]
    fn validate_proposal_with_command_spawn_failure_returns_err() {
        let tmp = TempDir::new().unwrap();
        let err = validate_proposal_with_command(
            "/definitely/not/a/real/openspec/binary",
            tmp.path(),
            "x",
        )
        .expect_err("spawn failure must produce Err");
        assert!(
            err.contains("openspec validate spawn failed:"),
            "spawn failure must use the prefix: {err}"
        );
    }

    #[test]
    fn validate_proposal_returns_stderr_on_nonzero_exit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let validator = ws.join("fail.sh");
        std::fs::write(
            &validator,
            "#!/bin/sh\necho 'broken MODIFIED header' >&2\nexit 7\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&validator).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&validator, perms).unwrap();
        let err = validate_proposal_with_command(
            &validator.to_string_lossy(),
            ws,
            "any",
        )
        .expect_err("nonzero exit → Err");
        assert!(err.contains("broken MODIFIED header"), "got: {err}");
    }

    #[test]
    fn severity_glyphs() {
        assert_eq!(Severity::Low.glyph(), "•");
        assert_eq!(Severity::Medium.glyph(), "⚠");
        assert_eq!(Severity::High.glyph(), "🔴");
    }

    // ====================================================================
    // format_audit_notification tests (chatops-audit-findings-in-threads)
    // ====================================================================

    fn brightline_file_finding(rel: &str, n: u64) -> Finding {
        Finding {
            severity: Severity::Medium,
            subject: format!("file {rel} is {n} lines (threshold: 800)"),
            body: format!("path: {rel}\nlines: {n}\nthreshold: 800"),
            anchor: Some(format!("{rel}:1")),
        }
    }

    fn brightline_dup_finding(sig: &str) -> Finding {
        Finding {
            severity: Severity::Low,
            subject: format!("duplicate signature `{sig}` across 2 files"),
            body: "mod_a.rs:1\nmod_b.rs:1".to_string(),
            anchor: Some("mod_a.rs:1".into()),
        }
    }

    fn drift_finding(divergence: &str) -> Finding {
        Finding {
            severity: Severity::Medium,
            subject: "[capX] reqY".to_string(),
            body: divergence.to_string(),
            anchor: Some("src/foo.rs:1".into()),
        }
    }

    fn ts() -> chrono::DateTime<Utc> {
        // Stable timestamp so audit_id assertions are deterministic.
        "2026-05-26T15:30:45Z".parse().unwrap()
    }

    #[test]
    fn format_audit_notification_brightline_counts_files_and_dupes_in_top_line() {
        let mut findings = Vec::new();
        for i in 0..7 {
            findings.push(brightline_file_finding(&format!("src/file{i}.rs"), 1000 + i));
        }
        for i in 0..3 {
            findings.push(brightline_dup_finding(&format!("fn helper{i}")));
        }
        let n = format_audit_notification(
            "architecture_brightline",
            "git@github.com:o/r.git",
            &findings,
            false,
            ts(),
        );
        assert!(
            n.top_line.contains("📐 architecture_brightline on git@github.com:o/r.git"),
            "top_line: {}",
            n.top_line
        );
        assert!(
            n.top_line.contains("7 file(s) over line threshold"),
            "top_line should report 7 files: {}",
            n.top_line
        );
        assert!(
            n.top_line.contains("3 duplicate signature(s)"),
            "top_line should report 3 dupes: {}",
            n.top_line
        );
        assert!(n.thread_body.contains("src/file0.rs"));
        assert!(n.thread_body.contains("duplicate signature `fn helper0`"));
        assert!(
            n.should_thread,
            "10 findings exceed the threshold, must thread"
        );
    }

    #[test]
    fn format_audit_notification_drift_counts_divergences() {
        let findings = vec![
            drift_finding("spec X says A; code says B."),
            drift_finding("spec Y says C; code says D."),
        ];
        // Threshold for threading: 5 lines OR 300 chars. With 2 short
        // findings, the body is 2 lines and a few dozen chars — inline.
        let n = format_audit_notification(
            "drift_audit",
            "git@github.com:o/r.git",
            &findings,
            false,
            ts(),
        );
        assert!(
            n.top_line.starts_with("🧭 drift_audit on git@github.com:o/r.git"),
            "top_line: {}",
            n.top_line
        );
        assert!(
            n.top_line.contains("2 spec/code divergence(s) detected"),
            "top_line must report 2 divergences: {}",
            n.top_line
        );
        assert!(!n.should_thread, "two short divergences inline");
    }

    #[test]
    fn format_audit_notification_drift_long_findings_thread() {
        let findings: Vec<Finding> = (0..5)
            .map(|i| drift_finding(&format!("divergence {i}")))
            .collect();
        let n = format_audit_notification(
            "drift_audit",
            "u",
            &findings,
            false,
            ts(),
        );
        assert!(n.should_thread, "5 findings → >3 lines → thread");
    }

    #[test]
    fn format_audit_notification_empty_findings_with_notify_on_clean_uses_check_form() {
        let n = format_audit_notification(
            "architecture_brightline",
            "git@github.com:o/r.git",
            &[],
            true,
            ts(),
        );
        assert_eq!(
            n.top_line,
            "✅ architecture_brightline on git@github.com:o/r.git: no findings"
        );
        assert!(n.thread_body.is_empty(), "no findings → empty body");
        assert!(!n.should_thread, "empty body → no thread");
    }

    #[test]
    fn format_audit_notification_single_line_below_threshold_inlines() {
        let findings = vec![drift_finding("one short divergence")];
        let n = format_audit_notification(
            "drift_audit",
            "u",
            &findings,
            false,
            ts(),
        );
        assert!(
            !n.should_thread,
            "1 short finding → inline; should_thread must be false"
        );
        assert!(n.top_line.starts_with("🧭 drift_audit on u"));
        assert!(n.thread_body.contains("[capX] reqY"));
    }

    #[test]
    fn format_audit_notification_truncates_thread_body_over_35k() {
        // Construct a body that exceeds 35,000 chars by stuffing one
        // gigantic finding subject in. The exact count must be > the
        // cap so the truncation branch fires.
        let huge_subject = "duplicate signature `fn x` across 2 files: ".to_string()
            + &"y".repeat(40_000);
        let findings = vec![Finding {
            severity: Severity::Low,
            subject: huge_subject,
            body: "details".into(),
            anchor: None,
        }];
        let n = format_audit_notification(
            "architecture_brightline",
            "git@github.com:o/r.git",
            &findings,
            false,
            ts(),
        );
        // The pointer is appended; thread_body chars count must be at
        // most cap + pointer length. Pointer is bounded by audit_id +
        // boilerplate (well under 500 chars in practice).
        let body_chars = n.thread_body.chars().count();
        assert!(
            body_chars > AUDIT_THREAD_BODY_CHAR_CAP,
            "truncated body should still include the pointer (longer than cap by pointer length): {}",
            body_chars
        );
        assert!(
            body_chars < AUDIT_THREAD_BODY_CHAR_CAP + 1_000,
            "truncated body must be within cap + pointer overhead, got {}",
            body_chars
        );
        assert!(
            n.thread_body.contains("[truncated; full findings at journalctl -u autocoder | grep audit_id="),
            "truncated body must end with the documented pointer"
        );
        // The audit_id is derived from the repo + audit_type + timestamp.
        // sanitize_for_audit_id replaces ':', '@', '/' with '_'.
        assert!(
            n.thread_body.contains("git_github.com_o_r.git:architecture_brightline:"),
            "audit_id should sanitize repo url: {}",
            n.thread_body
        );
    }

    #[test]
    fn format_audit_notification_under_cap_has_no_truncation_pointer() {
        let findings = vec![drift_finding("a small divergence")];
        let n = format_audit_notification(
            "drift_audit",
            "u",
            &findings,
            false,
            ts(),
        );
        assert!(
            !n.thread_body.contains("[truncated"),
            "small body must not get a truncation pointer: {}",
            n.thread_body
        );
    }

    #[test]
    fn format_audit_notification_unknown_audit_type_uses_generic_top_line() {
        let findings = vec![drift_finding("x")];
        let n = format_audit_notification(
            "architecture_consultative",
            "u",
            &findings,
            false,
            ts(),
        );
        assert!(
            n.top_line.starts_with("📋 architecture_consultative on u"),
            "unknown audit_type falls back to generic format: {}",
            n.top_line
        );
        assert!(n.top_line.contains("1 finding(s)"));
    }

    #[test]
    fn make_audit_id_sanitizes_repo_and_includes_timestamp() {
        let id = make_audit_id(
            "git@github.com:o/r.git",
            "drift_audit",
            ts(),
        );
        assert_eq!(
            id,
            "git_github.com_o_r.git:drift_audit:2026-05-26T15:30:45Z"
        );
    }

    // ====================================================================
    // ValidationExhausted threaded-notification tests
    // ====================================================================

    #[test]
    fn should_thread_validation_error_single_line_short_inlines() {
        assert!(!should_thread_validation_error("MODIFIED header not found"));
    }

    #[test]
    fn should_thread_validation_error_multi_line_threads() {
        assert!(should_thread_validation_error("line one\nline two"));
    }

    #[test]
    fn should_thread_validation_error_over_300_chars_threads() {
        let long = "x".repeat(400);
        assert!(should_thread_validation_error(&long));
    }

    #[tokio::test]
    async fn post_validation_exhausted_short_error_posts_inline() {
        let backend = Arc::new(RecordingBackend::new());
        let ctx = make_recording_ctx(backend.clone());
        post_validation_exhausted_notification(
            &ctx,
            "git@github.com:o/r.git",
            "security_bug_audit",
            1,
            "short single-line error",
        )
        .await
        .unwrap();
        let calls = backend.calls();
        assert_eq!(calls.len(), 1, "short error → exactly one inline post");
        let text = &calls[0].text;
        assert!(
            text.starts_with("❌ git@github.com:o/r.git: security_bug_audit"),
            "top-line present: {text}"
        );
        assert!(
            text.contains("short single-line error"),
            "inline body contains the validation error: {text}"
        );
        assert!(
            text.contains("Final validation error:"),
            "inline body retains the documented header: {text}"
        );
    }

    #[tokio::test]
    async fn post_validation_exhausted_multi_line_error_uses_thread() {
        // RecordingBackend does not override post_notification_with_thread,
        // so it routes through the default-impl concatenation. We assert
        // the SHAPE (one call, body contains both top-line and the
        // error excerpt with a blank-line separator) which is what the
        // default-impl contract documents.
        let backend = Arc::new(RecordingBackend::new());
        let ctx = make_recording_ctx(backend.clone());
        let err = "line1\nline2\nline3\nline4";
        post_validation_exhausted_notification(
            &ctx,
            "git@github.com:o/r.git",
            "missing_tests_audit",
            2,
            err,
        )
        .await
        .unwrap();
        let calls = backend.calls();
        assert_eq!(calls.len(), 1, "default-impl concat → one underlying call");
        let text = &calls[0].text;
        assert!(
            text.starts_with("❌ git@github.com:o/r.git: missing_tests_audit"),
            "top-line: {text}"
        );
        assert!(text.contains("after 2 retries"));
        assert!(
            text.contains("\n\n"),
            "default-impl separator: {text}"
        );
        assert!(text.contains("line1"));
        assert!(text.contains("line4"));
        assert!(text.contains("No commit was made"));
    }

    #[test]
    fn format_validation_exhausted_top_line_matches_spec() {
        let top = format_validation_exhausted_top_line(
            "git@github.com:o/r.git",
            "security_bug_audit",
            3,
        );
        assert_eq!(
            top,
            "❌ git@github.com:o/r.git: security_bug_audit produced an invalid proposal that failed openspec validation after 3 retries."
        );
    }

    #[test]
    fn all_registered_audits_have_one_line_descriptions() {
        use crate::config::AuditSettings;
        let audit_settings: std::collections::HashMap<String, AuditSettings> =
            std::collections::HashMap::new();
        let executor: crate::config::ExecutorConfig = serde_yml::from_str(
            "kind: claude_cli\ncommand: claude\ntimeout_secs: 600\n",
        )
        .expect("test executor config");
        let audits: Vec<Arc<dyn Audit>> = vec![
            Arc::new(crate::audits::brightline::ArchitectureBrightlineAudit::new(
                &audit_settings,
            )),
            Arc::new(
                crate::audits::architecture_consultative::ArchitectureConsultativeAudit::new(
                    &audit_settings,
                    &executor,
                ),
            ),
            Arc::new(crate::audits::drift::DriftAudit::new(&audit_settings, &executor)),
            Arc::new(crate::audits::missing_tests::MissingTestsAudit::new(
                &audit_settings,
                &executor,
            )),
            Arc::new(crate::audits::security_bug::SecurityBugAudit::new(
                &audit_settings,
                &executor,
            )),
        ];
        for a in &audits {
            let d = a.description();
            assert!(!d.is_empty(), "{}: description must not be empty", a.audit_type());
            assert!(
                d.chars().count() <= 80,
                "{}: description must be ≤ 80 chars, got {}",
                a.audit_type(),
                d.chars().count()
            );
            assert!(
                !d.contains('\n'),
                "{}: description must be a single line",
                a.audit_type()
            );
        }
    }
}
