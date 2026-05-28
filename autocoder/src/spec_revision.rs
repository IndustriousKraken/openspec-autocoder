//! Per-change spec-needs-revision marker. When the executor returns
//! `ExecutorOutcome::SpecNeedsRevision` for a change, autocoder writes
//! `<workspace>/openspec/changes/<change>/.needs-spec-revision.json`. The
//! marker's presence is a presence-only flag consulted by
//! `queue::list_pending` — the change is excluded from the queue until the
//! operator removes the marker manually (typically after editing tasks.md
//! to remove or revise the flagged tasks).

use crate::executor::UnimplementableTask;
use crate::preflight::spec_archivability::UnarchivableDelta;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MARKER_FILE: &str = ".needs-spec-revision.json";
const OPERATOR_ACTION: &str = "Edit openspec/changes/<change>/tasks.md to remove or revise the flagged tasks, commit + push, then delete this marker file.";
const OPERATOR_ACTION_UNARCHIVABLE: &str = "Edit openspec/changes/<change>/specs/<capability>/spec.md so each delta block's `### Requirement:` header matches the canonical openspec/specs/<capability>/spec.md. Commit + push, then `@<bot> clear-revision <repo> <change>` from chat (or delete this marker file directly).";

/// Outcome details captured at the moment the marker is written. Either
/// `unimplementable_tasks` (from the executor's `SpecNeedsRevision`) OR
/// `unarchivable_deltas` (from the pre-executor pipeline's spec-delta
/// archivability check) may be populated; in practice exactly one
/// non-empty array per write, but the schema permits both.
#[derive(Debug, Clone, Default)]
pub struct SpecNeedsRevisionDetail {
    pub unimplementable_tasks: Vec<UnimplementableTask>,
    pub unarchivable_deltas: Vec<UnarchivableDelta>,
    pub revision_suggestion: String,
}

/// JSON-friendly mirror of [`UnarchivableDelta`]. The on-disk JSON
/// stores `kind` as a stable string ("Added" / "Modified" / "Removed" /
/// "Renamed") so operators reading the marker by eye don't need to
/// memorise an enum-tag convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnarchivableDeltaRecord {
    pub capability: String,
    pub kind: String,
    pub header: String,
    pub reason: String,
}

impl From<&UnarchivableDelta> for UnarchivableDeltaRecord {
    fn from(d: &UnarchivableDelta) -> Self {
        Self {
            capability: d.capability.clone(),
            kind: d.kind.as_str().to_string(),
            header: d.header.clone(),
            reason: d.reason.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecNeedsRevisionMarker {
    pub change: String,
    pub marked_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unimplementable_tasks: Vec<UnimplementableTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unarchivable_deltas: Vec<UnarchivableDeltaRecord>,
    pub revision_suggestion: String,
    pub operator_action: String,
}

fn marker_path(workspace: &Path, change: &str) -> PathBuf {
    workspace
        .join("openspec/changes")
        .join(change)
        .join(MARKER_FILE)
}

/// True when `<workspace>/openspec/changes/<change>/.needs-spec-revision.json`
/// exists. Pure filesystem check — no JSON parsing.
pub fn marker_exists(workspace: &Path, change: &str) -> bool {
    marker_path(workspace, change).exists()
}

/// Write the marker file atomically (tempfile + rename in the change
/// directory). The change directory must already exist.
pub fn write_marker(
    workspace: &Path,
    change: &str,
    detail: &SpecNeedsRevisionDetail,
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
    let unarchivable_records: Vec<UnarchivableDeltaRecord> = detail
        .unarchivable_deltas
        .iter()
        .map(UnarchivableDeltaRecord::from)
        .collect();
    // Pre-flight failures (unarchivable_deltas) need a spec-file edit,
    // not a tasks.md edit; pick the operator_action string that matches.
    let operator_action = if !unarchivable_records.is_empty()
        && detail.unimplementable_tasks.is_empty()
    {
        OPERATOR_ACTION_UNARCHIVABLE
    } else {
        OPERATOR_ACTION
    };
    let marker = SpecNeedsRevisionMarker {
        change: change.to_string(),
        marked_at: Utc::now(),
        unimplementable_tasks: detail.unimplementable_tasks.clone(),
        unarchivable_deltas: unarchivable_records,
        revision_suggestion: detail.revision_suggestion.clone(),
        operator_action: operator_action.to_string(),
    };
    let tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    serde_json::to_writer_pretty(&tmp, &marker).with_context(|| {
        format!("serializing spec-needs-revision marker for {}", path.display())
    })?;
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

    fn fixture_detail() -> SpecNeedsRevisionDetail {
        SpecNeedsRevisionDetail {
            unimplementable_tasks: vec![
                UnimplementableTask {
                    task_id: "5.2".into(),
                    task_text: "install actionlint locally".into(),
                    reason: "no apt access in sandbox".into(),
                },
                UnimplementableTask {
                    task_id: "15.3".into(),
                    task_text: "smoke-test on macOS".into(),
                    reason: "no macOS host available".into(),
                },
            ],
            unarchivable_deltas: Vec::new(),
            revision_suggestion:
                "Replace 5.2 with a CI gate. Drop 15.3 — the workflow's own first real run is the integration test.".into(),
        }
    }

    #[test]
    fn write_then_exists_returns_true() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        assert!(!marker_exists(ws, "foo"));
        write_marker(ws, "foo", &fixture_detail()).unwrap();
        assert!(marker_exists(ws, "foo"));
    }

    #[test]
    fn write_marker_roundtrips_all_fields() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        let detail = fixture_detail();
        write_marker(ws, "foo", &detail).unwrap();

        let raw = std::fs::read_to_string(
            ws.join("openspec/changes/foo/.needs-spec-revision.json"),
        )
        .unwrap();
        let parsed: SpecNeedsRevisionMarker = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.change, "foo");
        assert_eq!(parsed.unimplementable_tasks, detail.unimplementable_tasks);
        assert_eq!(parsed.revision_suggestion, detail.revision_suggestion);
        assert!(parsed
            .operator_action
            .contains("delete this marker file"));
        // marked_at is recent.
        let age = (Utc::now() - parsed.marked_at).num_seconds().abs();
        assert!(age < 5, "marked_at must be ~now; age = {age}s");
    }

    #[test]
    fn write_marker_errors_when_change_directory_absent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let detail = fixture_detail();
        let err = write_marker(ws, "missing", &detail)
            .expect_err("write_marker should fail when change dir is absent");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("change directory does not exist"),
            "error must mention missing change dir: {msg}"
        );
    }

    use crate::preflight::spec_archivability::{DeltaKind, UnarchivableDelta};

    fn fixture_unarchivable_detail() -> SpecNeedsRevisionDetail {
        SpecNeedsRevisionDetail {
            unimplementable_tasks: Vec::new(),
            unarchivable_deltas: vec![UnarchivableDelta {
                capability: "code-reviewer".into(),
                kind: DeltaKind::Modified,
                header: "Reviewer prompt budget is operator-configurable".into(),
                reason: "header not found in canonical openspec/specs/code-reviewer/spec.md (this is the a07-style bug; check spelling AND capitalization)".into(),
            }],
            revision_suggestion: "Pre-flight check found 1 unarchivable spec delta:\n- capability=code-reviewer kind=Modified header=\"...\" reason=\"...\"".into(),
        }
    }

    #[test]
    fn write_marker_serialises_unarchivable_deltas() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "foo");
        let detail = fixture_unarchivable_detail();
        write_marker(ws, "foo", &detail).unwrap();

        let raw = std::fs::read_to_string(
            ws.join("openspec/changes/foo/.needs-spec-revision.json"),
        )
        .unwrap();
        let parsed: SpecNeedsRevisionMarker = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.change, "foo");
        assert_eq!(parsed.unarchivable_deltas.len(), 1);
        assert_eq!(parsed.unarchivable_deltas[0].capability, "code-reviewer");
        assert_eq!(parsed.unarchivable_deltas[0].kind, "Modified");
        // unimplementable_tasks omitted from JSON when empty.
        assert!(parsed.unimplementable_tasks.is_empty());
        // The operator action targets the spec file, not tasks.md.
        assert!(
            parsed.operator_action.contains("specs/<capability>/spec.md"),
            "operator_action must point at spec edit for unarchivable-deltas marker: {:?}",
            parsed.operator_action
        );
    }

    /// Pre-spec markers (only `unimplementable_tasks`, no
    /// `unarchivable_deltas` field) must still deserialize. Verifies the
    /// `#[serde(default)]` on the new field.
    #[test]
    fn pre_spec_marker_without_unarchivable_field_deserializes() {
        let raw = r#"{
            "change": "old",
            "marked_at": "2026-05-27T10:00:00Z",
            "unimplementable_tasks": [
                {"task_id": "5.2", "task_text": "install actionlint", "reason": "no apt access"}
            ],
            "revision_suggestion": "Replace 5.2 with a CI gate.",
            "operator_action": "Edit tasks.md, commit + push, then delete this marker file."
        }"#;
        let parsed: SpecNeedsRevisionMarker = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.change, "old");
        assert_eq!(parsed.unimplementable_tasks.len(), 1);
        assert!(parsed.unarchivable_deltas.is_empty());
    }

    /// Round-trip a marker with BOTH arrays populated (rare in practice
    /// but the schema permits it). Verifies serialization preserves both.
    #[test]
    fn marker_with_mixed_population_roundtrips() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change_dir(ws, "mixed");
        let detail = SpecNeedsRevisionDetail {
            unimplementable_tasks: vec![UnimplementableTask {
                task_id: "1.1".into(),
                task_text: "x".into(),
                reason: "y".into(),
            }],
            unarchivable_deltas: vec![UnarchivableDelta {
                capability: "cap".into(),
                kind: DeltaKind::Renamed,
                header: "from A to B".into(),
                reason: "from-title not found".into(),
            }],
            revision_suggestion: "fix both".into(),
        };
        write_marker(ws, "mixed", &detail).unwrap();
        let raw = std::fs::read_to_string(
            ws.join("openspec/changes/mixed/.needs-spec-revision.json"),
        )
        .unwrap();
        let parsed: SpecNeedsRevisionMarker = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.unimplementable_tasks.len(), 1);
        assert_eq!(parsed.unarchivable_deltas.len(), 1);
        assert_eq!(parsed.unarchivable_deltas[0].kind, "Renamed");
        assert_eq!(parsed.unarchivable_deltas[0].header, "from A to B");
    }
}
