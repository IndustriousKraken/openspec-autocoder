//! Per-change perma-stuck marker. When the consecutive-failure counter for
//! a change reaches `executor.perma_stuck_after_failures`, autocoder writes
//! `<workspace>/openspec/changes/<change>/.perma-stuck.json`. The marker's
//! presence is a presence-only flag consulted by `queue::list_pending` —
//! the change is excluded from the queue until the operator removes the
//! marker manually.

use crate::failure_state::FailureEntry;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MARKER_FILE: &str = ".perma-stuck.json";
const OPERATOR_ACTION: &str = "Delete this file to retry the change.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermaStuckMarker {
    pub change: String,
    pub consecutive_failures: u32,
    pub last_reason: String,
    pub marked_stuck_at: DateTime<Utc>,
    pub operator_action: String,
}

fn marker_path(workspace: &Path, change: &str) -> PathBuf {
    workspace
        .join("openspec/changes")
        .join(change)
        .join(MARKER_FILE)
}

/// True when `<workspace>/openspec/changes/<change>/.perma-stuck.json`
/// exists. Pure filesystem check — no JSON parsing.
pub fn marker_exists(workspace: &Path, change: &str) -> bool {
    marker_path(workspace, change).exists()
}

/// Write the marker file. The change directory must already exist.
pub fn write_marker(workspace: &Path, change: &str, entry: &FailureEntry) -> Result<()> {
    let path = marker_path(workspace, change);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("destination path has no parent: {}", path.display()))?;
    if !parent.is_dir() {
        return Err(anyhow!(
            "change directory does not exist: {}",
            parent.display()
        ));
    }
    let marker = PermaStuckMarker {
        change: change.to_string(),
        consecutive_failures: entry.count,
        last_reason: entry.last_reason.clone(),
        marked_stuck_at: Utc::now(),
        operator_action: OPERATOR_ACTION.to_string(),
    };
    let tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    serde_json::to_writer_pretty(&tmp, &marker)
        .with_context(|| format!("serializing perma-stuck marker for {}", path.display()))?;
    tmp.persist(&path)
        .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
    Ok(())
}

/// Idempotent removal of the marker. A missing file is success.
pub fn remove_marker(workspace: &Path, change: &str) -> Result<()> {
    let path = marker_path(workspace, change);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_change_dir(workspace: &Path, name: &str) {
        std::fs::create_dir_all(workspace.join("openspec/changes").join(name)).unwrap();
    }

    fn fixture_entry() -> FailureEntry {
        FailureEntry {
            count: 2,
            last_reason: "agent gave up".into(),
            last_failed_at: Utc::now(),
        }
    }

    #[test]
    fn write_then_exists_returns_true() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        assert!(!marker_exists(ws, "foo"));
        write_marker(ws, "foo", &fixture_entry()).unwrap();
        assert!(marker_exists(ws, "foo"));
        // Sanity: schema fields are present.
        let raw = std::fs::read_to_string(
            ws.join("openspec/changes/foo/.perma-stuck.json"),
        )
        .unwrap();
        assert!(raw.contains("\"change\""));
        assert!(raw.contains("\"consecutive_failures\""));
        assert!(raw.contains("\"last_reason\""));
        assert!(raw.contains("\"marked_stuck_at\""));
        assert!(raw.contains("\"operator_action\""));
        assert!(raw.contains("Delete this file to retry the change."));
    }

    #[test]
    fn remove_makes_exists_false() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        write_marker(ws, "foo", &fixture_entry()).unwrap();
        assert!(marker_exists(ws, "foo"));
        remove_marker(ws, "foo").unwrap();
        assert!(!marker_exists(ws, "foo"));
        // Idempotent: second remove is also fine.
        remove_marker(ws, "foo").unwrap();
    }

    #[test]
    fn marker_exists_false_for_clean_change_dir() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        assert!(!marker_exists(ws, "foo"));
    }

    #[test]
    fn write_marker_errors_when_change_directory_absent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Intentionally do NOT create openspec/changes/foo/.
        let result = write_marker(ws, "foo", &fixture_entry());
        let err = result.expect_err("write_marker should fail when change dir is absent");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("change directory does not exist"),
            "error message missing guard text: {msg}"
        );
        assert!(
            msg.contains("foo"),
            "error message missing change name: {msg}"
        );
        // Guard runs before any filesystem write — no marker file should exist.
        assert!(
            !ws.join("openspec/changes/foo/.perma-stuck.json").exists(),
            "marker file should not exist after failed guard",
        );
    }
}
