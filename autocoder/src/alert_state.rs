//! Per-workspace persistence for predictable-failure alert throttling.
//!
//! Lives at `<workspace>/.alert-state.json` (alongside `.in-progress`,
//! `.question.json`, `.answer.json`). Cleared on the next successful pass so
//! a transient outage does not silence follow-up alerts.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ALERT_STATE_FILE: &str = ".alert-state.json";

/// Categories of predictable infrastructure failure that autocoder alerts on.
/// Other failure surfaces (executor-`Failed`, reviewer-failed, chatops-post-
/// failed) are explicitly out of scope and never produce an alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertCategory {
    WorkspaceInitFailure,
    WorkspaceDirtyMidIteration,
    BranchPushFailure,
    PrCreationFailure,
}

impl AlertCategory {
    /// Short human-readable label used inside the alert text (e.g.
    /// "workspace init keeps failing").
    pub fn label(&self) -> &'static str {
        match self {
            Self::WorkspaceInitFailure => "workspace init keeps failing",
            Self::WorkspaceDirtyMidIteration => "workspace dirty mid-iteration",
            Self::BranchPushFailure => "branch push keeps failing",
            Self::PrCreationFailure => "PR creation keeps failing",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertEntry {
    pub last_alerted_at: DateTime<Utc>,
    pub last_error_excerpt: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlertState {
    #[serde(default)]
    pub alerts: HashMap<AlertCategory, AlertEntry>,
    /// Per-change perma-stuck alert throttle. Keyed by change name. The
    /// 24h throttle ensures that a repeat fix-test-fail cycle on a single
    /// change doesn't spam the alert channel.
    #[serde(default)]
    pub perma_stuck_alerts: HashMap<String, AlertEntry>,
}

fn alert_state_path(workspace: &Path) -> PathBuf {
    workspace.join(ALERT_STATE_FILE)
}

impl AlertState {
    /// Load the per-workspace alert state. A missing file is not an error —
    /// it parses to an empty state (no prior alerts).
    pub fn load_or_default(workspace: &Path) -> Self {
        let path = alert_state_path(workspace);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!(
                    "alert-state file at {} is corrupt; starting empty: {e:#}",
                    path.display()
                );
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(
                    "alert-state file at {} unreadable; starting empty: {e:#}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Atomically persist this state under `<workspace>/.alert-state.json`
    /// via tempfile-then-rename in the same directory so a torn write can
    /// never be observed by a concurrent reader.
    pub fn save(&self, workspace: &Path) -> Result<()> {
        let path = alert_state_path(workspace);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("destination path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
        let tmp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("creating tempfile in {}", parent.display()))?;
        serde_json::to_writer_pretty(&tmp, self)
            .with_context(|| format!("serializing alert state for {}", path.display()))?;
        tmp.persist(&path)
            .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
        Ok(())
    }

    /// Idempotent removal of the alert-state file. A missing file is a
    /// success, not an error.
    pub fn clear(workspace: &Path) -> Result<()> {
        let path = alert_state_path(workspace);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_missing_returns_empty() {
        let dir = TempDir::new().unwrap();
        let state = AlertState::load_or_default(dir.path());
        assert!(state.alerts.is_empty());
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = TempDir::new().unwrap();
        let mut state = AlertState::default();
        let now = Utc::now();
        state.alerts.insert(
            AlertCategory::BranchPushFailure,
            AlertEntry {
                last_alerted_at: now,
                last_error_excerpt: "refusing to update protected branch".into(),
            },
        );
        state.save(dir.path()).unwrap();

        let reloaded = AlertState::load_or_default(dir.path());
        let entry = reloaded
            .alerts
            .get(&AlertCategory::BranchPushFailure)
            .expect("entry roundtrips");
        // Timestamps may differ in trailing-precision encoding; compare via
        // round-trip serialization rather than direct equality.
        assert_eq!(entry.last_error_excerpt, "refusing to update protected branch");
        let diff = (entry.last_alerted_at - now).num_milliseconds().abs();
        assert!(diff < 5, "timestamps must roundtrip within 5ms; diff = {diff}");
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let mut state = AlertState::default();
        state.alerts.insert(
            AlertCategory::PrCreationFailure,
            AlertEntry {
                last_alerted_at: Utc::now(),
                last_error_excerpt: "403 Forbidden".into(),
            },
        );
        state.save(dir.path()).unwrap();
        assert!(dir.path().join(".alert-state.json").exists());
        AlertState::clear(dir.path()).expect("first clear ok");
        assert!(!dir.path().join(".alert-state.json").exists());
        // Second clear must also succeed.
        AlertState::clear(dir.path()).expect("second clear ok");
    }

    #[test]
    fn clear_does_not_error_on_missing() {
        let dir = TempDir::new().unwrap();
        // File never created.
        AlertState::clear(dir.path()).expect("clear without prior save must succeed");
    }

    #[test]
    fn json_keys_use_snake_case_for_categories() {
        // The spec's `.alert-state.json` shape labels the categories in
        // snake_case; guard against accidental rename downstream.
        let mut state = AlertState::default();
        state.alerts.insert(
            AlertCategory::WorkspaceInitFailure,
            AlertEntry {
                last_alerted_at: Utc::now(),
                last_error_excerpt: "x".into(),
            },
        );
        let s = serde_json::to_string(&state).unwrap();
        assert!(
            s.contains("workspace_init_failure"),
            "json must use snake_case category key; got: {s}"
        );
    }
}
