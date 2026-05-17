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
//!   `/tmp/autocoder/logs/<basename>/audits/<type>-<timestamp>.log`.
//! - [`state`]: persistence of `last_run_at` + `last_run_sha` per audit.
//! - [`scheduler`]: cadence + change-guard + write-policy enforcement.

pub mod architecture_consultative;
pub mod brightline;
pub mod dependency_update;
pub mod drift;
pub mod missing_tests;
pub mod scheduler;
pub mod security_bug;
pub mod specs_writing;
pub mod state;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{RepositoryConfig, ResolvedSandbox};
use crate::polling_loop::ChatOpsContext;

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
/// (controlled by `notify_on_clean`); `SpecsWritten` → info log only.
#[derive(Debug, Clone)]
pub enum AuditOutcome {
    NoFindings,
    Reported(Vec<Finding>),
    SpecsWritten(Vec<String>),
}

impl AuditOutcome {
    pub fn kind(&self) -> AuditOutcomeKind {
        match self {
            Self::NoFindings => AuditOutcomeKind::NoFindings,
            Self::Reported(_) => AuditOutcomeKind::Reported,
            Self::SpecsWritten(_) => AuditOutcomeKind::SpecsWritten,
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
    /// `/tmp/autocoder/logs/<workspace-basename>/audits/<audit_type>-<UTC-RFC3339-with-Z>.log`.
    /// The directory is created if absent.
    pub fn open(workspace: &Path, audit_type: &str) -> Result<Self> {
        let basename = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let dir = PathBuf::from("/tmp/autocoder/logs")
            .join(basename)
            .join("audits");
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        // Path lives under /tmp/autocoder/logs/<basename>/audits/...
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
            AuditOutcome::Reported(vec![]).kind(),
            AuditOutcomeKind::Reported
        );
        assert_eq!(
            AuditOutcome::SpecsWritten(vec!["x".into()]).kind(),
            AuditOutcomeKind::SpecsWritten
        );
    }

    #[test]
    fn severity_glyphs() {
        assert_eq!(Severity::Low.glyph(), "•");
        assert_eq!(Severity::Medium.glyph(), "⚠");
        assert_eq!(Severity::High.glyph(), "🔴");
    }
}
