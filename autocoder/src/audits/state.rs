//! Per-workspace state for the periodic-audit framework.
//!
//! Lives at `<workspace>/.audit-state.json`. The file is autocoder
//! bookkeeping and is registered in `.git/info/exclude` at workspace
//! init time (see [`workspace::ensure_initialized`]).
//!
//! Distinct from `.alert-state.json` by design: audits run on N-day
//! cadences while alerts throttle on 24h windows; the two schemas have
//! nothing in common.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::AuditOutcomeKind;

const AUDIT_STATE_FILE: &str = ".audit-state.json";

/// On-disk record of one audit's most-recent run. Keyed by `audit_type`
/// inside [`AuditState::runs`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditRunEntry {
    pub last_run_at: DateTime<Utc>,
    /// HEAD SHA on the base branch at the moment the audit ran. Stored as
    /// `Option<String>` to support the rare audit that runs without a
    /// resolvable HEAD (e.g. brand-new empty repo); `None` is treated as
    /// "always changed" by the `requires_head_change` check.
    pub last_run_sha: Option<String>,
    pub last_outcome: AuditOutcomeKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditState {
    #[serde(default)]
    pub runs: HashMap<String, AuditRunEntry>,
}

fn audit_state_path(workspace: &Path) -> PathBuf {
    workspace.join(AUDIT_STATE_FILE)
}

impl AuditState {
    /// Load the per-workspace audit state. A missing file silently parses
    /// to the empty default ("no audits have ever run"). An unparseable
    /// file logs WARN and parses to the default — never blocks the
    /// iteration on corrupt state.
    pub fn load_or_default(workspace: &Path) -> Self {
        let path = audit_state_path(workspace);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!(
                    "audit-state file at {} is corrupt; starting empty: {e:#}",
                    path.display()
                );
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(
                    "audit-state file at {} unreadable; starting empty: {e:#}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Atomically persist this state under `<workspace>/.audit-state.json`
    /// via tempfile-then-rename in the same directory. Mirrors
    /// `alert_state::save` so a torn write can never be observed by a
    /// concurrent reader. Idempotent.
    pub fn save(&self, workspace: &Path) -> Result<()> {
        let path = audit_state_path(workspace);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("destination path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
        let tmp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("creating tempfile in {}", parent.display()))?;
        serde_json::to_writer_pretty(&tmp, self)
            .with_context(|| format!("serializing audit state for {}", path.display()))?;
        tmp.persist(&path)
            .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
        Ok(())
    }

    /// Record `entry` under `audit_type`, replacing any prior record.
    pub fn record(&mut self, audit_type: &str, entry: AuditRunEntry) {
        self.runs.insert(audit_type.to_string(), entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn state_load_handles_missing_file() {
        let dir = TempDir::new().unwrap();
        let s = AuditState::load_or_default(dir.path());
        assert!(s.runs.is_empty());
    }

    #[test]
    fn state_save_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut s = AuditState::default();
        let now = Utc::now();
        s.record(
            "architecture_brightline",
            AuditRunEntry {
                last_run_at: now,
                last_run_sha: Some("deadbeef".into()),
                last_outcome: AuditOutcomeKind::NoFindings,
            },
        );
        s.save(dir.path()).expect("save ok");
        let reloaded = AuditState::load_or_default(dir.path());
        let e = reloaded
            .runs
            .get("architecture_brightline")
            .expect("entry roundtrips");
        assert_eq!(e.last_run_sha.as_deref(), Some("deadbeef"));
        assert_eq!(e.last_outcome, AuditOutcomeKind::NoFindings);
        let diff = (e.last_run_at - now).num_milliseconds().abs();
        assert!(diff < 5, "timestamps must roundtrip within 5ms; diff = {diff}");
    }

    #[test]
    fn state_load_handles_corrupt_file_with_warning() {
        let dir = TempDir::new().unwrap();
        // Write garbage to the audit-state path.
        std::fs::write(dir.path().join(".audit-state.json"), "{not valid json").unwrap();
        let s = AuditState::load_or_default(dir.path());
        // Corrupt → empty default; the warn log is a side-effect we don't
        // assert on here (alert_state's equivalent test doesn't either).
        assert!(s.runs.is_empty(), "corrupt file must parse to empty state");
    }

    #[test]
    fn state_save_is_atomic_no_tmp_files_leak() {
        let dir = TempDir::new().unwrap();
        let s = AuditState::default();
        s.save(dir.path()).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            !entries.iter().any(|n| n.contains(".tmp")),
            "no `.tmp` files should leak: {entries:?}"
        );
    }
}
