//! Per-workspace persistence for the consecutive-failure counter that
//! drives perma-stuck change detection.
//!
//! Lives at `<workspace>/.failure-state.json` alongside `.alert-state.json`.
//! Each Failed outcome increments the per-change counter; each Archived
//! outcome clears it. Reaching `executor.perma_stuck_after_failures` is
//! what flips a change into the perma-stuck state.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const FAILURE_STATE_FILE: &str = ".failure-state.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    pub count: u32,
    pub last_reason: String,
    pub last_failed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailureState {
    #[serde(flatten)]
    pub entries: HashMap<String, FailureEntry>,
}

fn failure_state_path(workspace: &Path) -> PathBuf {
    workspace.join(FAILURE_STATE_FILE)
}

/// Load the failure-state file. Missing file → empty state. A corrupt file
/// is logged at WARN and treated as empty (conservative: a stale or
/// unreadable file should not interfere with the agent's ability to retry
/// changes — the alternative would be the daemon refusing to make
/// progress on any change in the affected workspace).
pub fn load(workspace: &Path) -> Result<FailureState> {
    let path = failure_state_path(workspace);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<FailureState>(&raw) {
            Ok(state) => Ok(state),
            Err(e) => {
                tracing::warn!(
                    "failure-state file at {} is corrupt; starting empty: {e:#}",
                    path.display()
                );
                Ok(FailureState::default())
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FailureState::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn save(state: &FailureState, workspace: &Path) -> Result<()> {
    let path = failure_state_path(workspace);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("destination path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating parent dir {}", parent.display()))?;
    let tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    serde_json::to_writer_pretty(&tmp, state)
        .with_context(|| format!("serializing failure state for {}", path.display()))?;
    tmp.persist(&path)
        .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
    Ok(())
}

/// Increment the failure counter for `change`, recording the reason and
/// timestamp. Creates the entry if absent. Returns the new count.
pub fn record_failure(workspace: &Path, change: &str, reason: &str) -> Result<u32> {
    let mut state = load(workspace)?;
    let entry = state
        .entries
        .entry(change.to_string())
        .or_insert(FailureEntry {
            count: 0,
            last_reason: String::new(),
            last_failed_at: Utc::now(),
        });
    entry.count = entry.count.saturating_add(1);
    entry.last_reason = reason.to_string();
    entry.last_failed_at = Utc::now();
    let new_count = entry.count;
    save(&state, workspace)?;
    Ok(new_count)
}

/// Remove `change`'s entry from the failure-state file. Silent on
/// "entry not present" — that's a no-op.
pub fn clear(workspace: &Path, change: &str) -> Result<()> {
    let mut state = load(workspace)?;
    if state.entries.remove(change).is_none() {
        return Ok(());
    }
    save(&state, workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_missing_returns_empty() {
        let dir = TempDir::new().unwrap();
        let state = load(dir.path()).unwrap();
        assert!(state.entries.is_empty());
    }

    #[test]
    fn record_failure_creates_entry() {
        let dir = TempDir::new().unwrap();
        let n = record_failure(dir.path(), "foo", "first failure").unwrap();
        assert_eq!(n, 1);
        let state = load(dir.path()).unwrap();
        let entry = state.entries.get("foo").expect("entry present");
        assert_eq!(entry.count, 1);
        assert_eq!(entry.last_reason, "first failure");
    }

    #[test]
    fn record_failure_increments_existing() {
        let dir = TempDir::new().unwrap();
        let n1 = record_failure(dir.path(), "foo", "first").unwrap();
        let n2 = record_failure(dir.path(), "foo", "second").unwrap();
        assert_eq!(n1, 1);
        assert_eq!(n2, 2);
        let state = load(dir.path()).unwrap();
        let entry = state.entries.get("foo").expect("entry present");
        assert_eq!(entry.count, 2);
        assert_eq!(entry.last_reason, "second");
    }

    #[test]
    fn clear_removes_entry() {
        let dir = TempDir::new().unwrap();
        let _ = record_failure(dir.path(), "foo", "x").unwrap();
        clear(dir.path(), "foo").unwrap();
        let state = load(dir.path()).unwrap();
        assert!(state.entries.get("foo").is_none());
    }

    #[test]
    fn clear_is_idempotent_when_entry_absent() {
        let dir = TempDir::new().unwrap();
        clear(dir.path(), "never-existed").expect("clear of absent entry must succeed");
        clear(dir.path(), "still-absent").expect("second clear is also fine");
    }

    #[test]
    fn corrupt_file_treated_as_empty() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".failure-state.json"), "{not json").unwrap();
        let state = load(dir.path()).unwrap();
        assert!(
            state.entries.is_empty(),
            "corrupt file must be treated as fresh state"
        );
    }
}
