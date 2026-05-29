//! Per-change iteration-pending marker (a27a1). When the polling
//! loop handles an `ExecutorOutcome::IterationRequested`, it writes
//! `<workspace>/openspec/changes/<change>/.iteration-pending.json`
//! after committing + force-pushing the WIP to the agent branch. The
//! marker survives the gap between subprocess-exit AND next-poll-cycle
//! (including a daemon restart) AND carries the cumulative completed/
//! remaining task lists, the agent's stated reason, AND the upcoming
//! iteration number into the next prompt's continuation block. The
//! marker's presence ALSO front-inserts the change in `list_pending`.
//!
//! Lifecycle:
//! - `IterationRequested` arm: write/replace marker with the new state
//!   (after WIP commit + push succeed).
//! - `Completed` arm: delete the marker after commit + push completes.
//! - `SpecNeedsRevision` arm: delete the marker.
//! - `Failed` arm: leave the marker untouched (retry preserves context).
//! - `AskUser` arm: leave the marker untouched.
//!
//! A corrupt marker is treated as `iteration_number: 0` for ordering
//! AND as "no marker" for prompt-builder continuation; the corrupt file
//! is NOT deleted (operator can inspect).

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MARKER_FILE: &str = ".iteration-pending.json";

/// On-disk shape of `.iteration-pending.json`. All fields are required;
/// the polling-loop's `IterationRequested` arm populates them from the
/// `ExecutorOutcome::IterationRequested` payload it consumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IterationPendingMarker {
    pub completed_tasks: Vec<String>,
    pub remaining_tasks: Vec<String>,
    pub reason: String,
    pub iteration_number: u32,
}

fn marker_path(workspace: &Path, change: &str) -> PathBuf {
    workspace
        .join("openspec/changes")
        .join(change)
        .join(MARKER_FILE)
}

/// True when `.iteration-pending.json` exists. Pure filesystem check —
/// no JSON parsing, so a corrupt marker still returns true.
pub fn marker_exists(workspace: &Path, change: &str) -> bool {
    marker_path(workspace, change).exists()
}

/// Read AND parse the marker. Returns `Ok(None)` when the file is
/// absent; `Err(...)` for any IO or parse failure. Callers that want
/// corrupt-as-absent semantics convert `Err(...)` to `None` themselves
/// (see classifier AND prompt-builder, both of which log a warning AND
/// fall through).
pub fn read_marker(workspace: &Path, change: &str) -> Result<Option<IterationPendingMarker>> {
    let path = marker_path(workspace, change);
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let marker: IterationPendingMarker = serde_json::from_str(&s)
                .with_context(|| format!("parsing iteration-pending marker {}", path.display()))?;
            Ok(Some(marker))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(e).with_context(|| format!("reading iteration-pending marker {}", path.display()))
        }
    }
}

/// Atomic write of the marker file (tempfile + rename). The change
/// directory must already exist; an absent change directory is an
/// error (mirrors `perma_stuck::write_marker`'s discipline).
pub fn write_marker(
    workspace: &Path,
    change: &str,
    marker: &IterationPendingMarker,
) -> Result<()> {
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
    let tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    serde_json::to_writer_pretty(&tmp, marker).with_context(|| {
        format!(
            "serializing iteration-pending marker for {}",
            path.display()
        )
    })?;
    tmp.persist(&path)
        .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
    Ok(())
}

/// Idempotent removal of the marker file. A missing file is success.
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

    fn sample_marker() -> IterationPendingMarker {
        IterationPendingMarker {
            completed_tasks: vec!["1".into(), "2".into()],
            remaining_tasks: vec!["3".into()],
            reason: "task 3 needs a refactor I want to plan more carefully".into(),
            iteration_number: 2,
        }
    }

    #[test]
    fn write_then_read_round_trips_marker() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        write_marker(ws, "foo", &sample_marker()).unwrap();
        assert!(marker_exists(ws, "foo"));
        let got = read_marker(ws, "foo").unwrap().unwrap();
        assert_eq!(got, sample_marker());
    }

    #[test]
    fn read_marker_absent_returns_none() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        let got = read_marker(ws, "foo").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn read_marker_corrupt_returns_err() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        std::fs::write(
            ws.join("openspec/changes/foo").join(MARKER_FILE),
            "{ not valid json",
        )
        .unwrap();
        let err = read_marker(ws, "foo").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parsing"), "msg: {msg}");
    }

    #[test]
    fn write_replaces_existing_marker() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        write_marker(ws, "foo", &sample_marker()).unwrap();
        let updated = IterationPendingMarker {
            completed_tasks: vec!["1".into(), "2".into(), "3".into()],
            remaining_tasks: vec!["4".into()],
            reason: "another reason".into(),
            iteration_number: 3,
        };
        write_marker(ws, "foo", &updated).unwrap();
        let got = read_marker(ws, "foo").unwrap().unwrap();
        assert_eq!(got, updated);
    }

    #[test]
    fn remove_makes_exists_false() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        write_marker(ws, "foo", &sample_marker()).unwrap();
        assert!(marker_exists(ws, "foo"));
        remove_marker(ws, "foo").unwrap();
        assert!(!marker_exists(ws, "foo"));
        // Idempotent: second remove is also fine.
        remove_marker(ws, "foo").unwrap();
    }

    #[test]
    fn write_marker_errors_when_change_dir_absent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let result = write_marker(ws, "foo", &sample_marker());
        let err = result.expect_err("should fail when change dir absent");
        let msg = format!("{err:#}");
        assert!(msg.contains("change directory does not exist"), "msg: {msg}");
    }

    /// The atomic tempfile + rename pattern protects against partial-
    /// write corruption: if the serialization step is interrupted, the
    /// tempfile remains in the parent directory AND the destination
    /// marker is unchanged. This exercises the rename-replace path: an
    /// existing marker is replaced atomically; mid-rename interruption
    /// would leave EITHER the old OR the new content on disk, never a
    /// truncated half-write of the new payload.
    #[test]
    fn atomic_write_does_not_truncate_destination_on_replace() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        // Establish a valid marker.
        write_marker(ws, "foo", &sample_marker()).unwrap();
        // Now write a different marker — the destination must be
        // replaced atomically. The intermediate path goes via tempfile +
        // rename, so reading the destination at ANY point in time
        // returns a parseable marker (either the old or the new), never
        // a truncated half-write.
        let updated = IterationPendingMarker {
            completed_tasks: vec!["1".into(), "2".into(), "3".into()],
            remaining_tasks: vec!["4".into()],
            reason: "atomicity check".into(),
            iteration_number: 4,
        };
        write_marker(ws, "foo", &updated).unwrap();
        let got = read_marker(ws, "foo").unwrap().unwrap();
        assert_eq!(got, updated);
    }

    /// Corrupt-state injection: if a prior tempfile partially-written
    /// JSON ended up at the destination (e.g. a non-atomic prior writer
    /// crashed), the read returns Err. Confirm the marker-write helper
    /// nonetheless succeeds AND replaces the corrupt content with a
    /// valid marker.
    #[test]
    fn write_marker_replaces_corrupt_existing_marker() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        // Inject a corrupt marker manually.
        std::fs::write(
            ws.join("openspec/changes/foo").join(MARKER_FILE),
            "{ truncated json",
        )
        .unwrap();
        // Sanity: read currently errors.
        assert!(read_marker(ws, "foo").is_err());
        // Atomic write must succeed AND make the file parseable.
        write_marker(ws, "foo", &sample_marker()).unwrap();
        let got = read_marker(ws, "foo").unwrap().unwrap();
        assert_eq!(got, sample_marker());
    }
}
