//! Spec-delta archivability pre-flight check.
//!
//! `openspec validate --strict` checks a change's spec deltas are
//! well-formed (frontmatter present, sections named correctly, scenarios
//! use proper WHEN/THEN structure, normative keywords appear). It does
//! NOT verify the deltas can actually be applied to the canonical specs
//! at archive time. That gap was the root cause of the a07 perma-stuck
//! incident: a `## MODIFIED Requirements` block whose `### Requirement:`
//! header did not exist in the canonical spec passed `validate --strict`
//! but later aborted `openspec archive` with
//! `<cap> MODIFIED failed for header "..." not found`. The change went
//! into the Failed bucket after ~$3 of LLM spend.
//!
//! This module performs the mechanical, string-comparison header check
//! per kind (ADDED / MODIFIED / REMOVED / RENAMED) BEFORE the executor
//! runs. On any precondition violation the polling loop short-circuits:
//! a `.needs-spec-revision.json` marker is written with an
//! `unarchivable_deltas` field enumerating each mismatch, the existing
//! `AlertCategory::SpecNeedsRevision` chatops alert fires, and no LLM
//! cost is incurred.

use crate::cli::sync_specs_deps::{DeltaEntry, parse_capability_deltas};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

/// Delta kind matching the four `## …Requirements` block headers in
/// OpenSpec change specs. `Renamed` records both `from` and `to` titles
/// in a single `header` field for diagnostic legibility (see
/// [`UnarchivableDelta::header`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Added,
    Modified,
    Removed,
    Renamed,
}

impl DeltaKind {
    /// Stable string used in JSON serialization (the marker file) and
    /// the chatops alert body.
    pub fn as_str(self) -> &'static str {
        match self {
            DeltaKind::Added => "Added",
            DeltaKind::Modified => "Modified",
            DeltaKind::Removed => "Removed",
            DeltaKind::Renamed => "Renamed",
        }
    }
}

/// One precondition violation surfaced by [`check_spec_deltas_archivable`].
///
/// `header` is the requirement title for ADDED / MODIFIED / REMOVED.
/// For RENAMED it's the human-readable `"from <a> to <b>"` form so the
/// chatops alert body identifies the offending entry without the
/// operator having to cross-reference the spec file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnarchivableDelta {
    pub capability: String,
    pub kind: DeltaKind,
    pub header: String,
    pub reason: String,
}

/// Run the per-kind precondition checks for every capability delta block
/// in `<workspace>/openspec/changes/<change_slug>/specs/<cap>/spec.md`.
///
/// Returns an empty `Vec` when every delta block's headers satisfy the
/// precondition for its kind. A non-empty `Vec` lists every violation;
/// the caller is expected to halt the executor and write a
/// `.needs-spec-revision.json` marker with the entries as
/// `unarchivable_deltas`.
///
/// Returns `Err` only when the change's specs directory cannot be
/// enumerated (filesystem-level error). Individual spec file read errors
/// emit a WARN log and skip that file rather than aborting the check —
/// one unreadable capability spec must not mask the rest.
pub fn check_spec_deltas_archivable(
    workspace_root: &Path,
    change_slug: &str,
) -> Result<Vec<UnarchivableDelta>> {
    let specs_dir = workspace_root
        .join("openspec/changes")
        .join(change_slug)
        .join("specs");
    if !specs_dir.is_dir() {
        // No specs/ subdir → no deltas to check. This is a fine state
        // for code-only changes; openspec validate flags any structural
        // issue separately.
        return Ok(Vec::new());
    }

    let mut violations: Vec<UnarchivableDelta> = Vec::new();
    let read = std::fs::read_dir(&specs_dir)
        .with_context(|| format!("reading {}", specs_dir.display()))?;
    let mut caps: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        caps.push((name, path));
    }
    // Deterministic order so violation Vec is stable across runs (cheaper
    // tests, more legible alert bodies).
    caps.sort_by(|a, b| a.0.cmp(&b.0));

    for (cap_name, cap_path) in caps {
        let spec_md = cap_path.join("spec.md");
        if !spec_md.is_file() {
            continue;
        }
        let body = match std::fs::read_to_string(&spec_md) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    capability = %cap_name,
                    "check_spec_deltas_archivable: cannot read {}: {e}; skipping",
                    spec_md.display()
                );
                continue;
            }
        };
        let deltas = parse_capability_deltas(&body);
        if deltas.is_empty() {
            continue;
        }

        // Load the canonical capability spec's requirement headers, if
        // any. A capability without a canonical spec is fine for ADDED
        // (it's introducing a new capability); MODIFIED / REMOVED /
        // RENAMED-from blocks against a non-existent canonical produce
        // a dedicated reason.
        let canonical_headers = load_canonical_headers(workspace_root, &cap_name);

        for delta in deltas {
            match delta {
                DeltaEntry::Added { header } => {
                    if let Some(canon) = canonical_headers.as_ref()
                        && canon.contains(&header)
                    {
                        violations.push(UnarchivableDelta {
                            capability: cap_name.clone(),
                            kind: DeltaKind::Added,
                            header,
                            reason: format!(
                                "header already exists in canonical openspec/specs/{cap_name}/spec.md — use MODIFIED instead"
                            ),
                        });
                    }
                    // No canonical → ADDED is fine (creating a new capability).
                }
                DeltaEntry::Modified { header } => {
                    match canonical_headers.as_ref() {
                        Some(canon) => {
                            if !canon.contains(&header) {
                                violations.push(UnarchivableDelta {
                                    capability: cap_name.clone(),
                                    kind: DeltaKind::Modified,
                                    header,
                                    reason: format!(
                                        "header not found in canonical openspec/specs/{cap_name}/spec.md (this is the a07-style bug; check spelling AND capitalization)"
                                    ),
                                });
                            }
                        }
                        None => {
                            violations.push(UnarchivableDelta {
                                capability: cap_name.clone(),
                                kind: DeltaKind::Modified,
                                header,
                                reason: format!(
                                    "capability {cap_name} has no canonical spec — cannot modify within it"
                                ),
                            });
                        }
                    }
                }
                DeltaEntry::Removed { header } => {
                    match canonical_headers.as_ref() {
                        Some(canon) => {
                            if !canon.contains(&header) {
                                violations.push(UnarchivableDelta {
                                    capability: cap_name.clone(),
                                    kind: DeltaKind::Removed,
                                    header,
                                    reason: format!(
                                        "header not found in canonical openspec/specs/{cap_name}/spec.md — cannot remove non-existent requirement"
                                    ),
                                });
                            }
                        }
                        None => {
                            violations.push(UnarchivableDelta {
                                capability: cap_name.clone(),
                                kind: DeltaKind::Removed,
                                header,
                                reason: format!(
                                    "capability {cap_name} has no canonical spec — cannot remove within it"
                                ),
                            });
                        }
                    }
                }
                DeltaEntry::Renamed { from, to } => {
                    let display_header = format!("from {from} to {to}");
                    match canonical_headers.as_ref() {
                        Some(canon) => {
                            if !canon.contains(&from) {
                                violations.push(UnarchivableDelta {
                                    capability: cap_name.clone(),
                                    kind: DeltaKind::Renamed,
                                    header: display_header.clone(),
                                    reason: format!(
                                        "from-title not found in canonical openspec/specs/{cap_name}/spec.md"
                                    ),
                                });
                            }
                            if canon.contains(&to) {
                                violations.push(UnarchivableDelta {
                                    capability: cap_name.clone(),
                                    kind: DeltaKind::Renamed,
                                    header: display_header,
                                    reason: format!(
                                        "to-title already exists in canonical openspec/specs/{cap_name}/spec.md — rename would create a duplicate"
                                    ),
                                });
                            }
                        }
                        None => {
                            violations.push(UnarchivableDelta {
                                capability: cap_name.clone(),
                                kind: DeltaKind::Renamed,
                                header: display_header,
                                reason: format!(
                                    "capability {cap_name} has no canonical spec — cannot rename within it"
                                ),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(violations)
}

/// Read `<workspace>/openspec/specs/<cap>/spec.md` and extract the set of
/// `### Requirement: <title>` titles. Returns `None` if the canonical
/// spec is absent (a new-capability change), `Some(set)` otherwise.
/// Read failures are treated as "no canonical headers visible" — same
/// effect as an absent file — and emit a WARN log.
fn load_canonical_headers(workspace_root: &Path, capability: &str) -> Option<HashSet<String>> {
    let path = workspace_root
        .join("openspec/specs")
        .join(capability)
        .join("spec.md");
    if !path.is_file() {
        return None;
    }
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                capability = %capability,
                "load_canonical_headers: cannot read {}: {e}; treating as absent",
                path.display()
            );
            return None;
        }
    };
    let mut out = HashSet::new();
    for line in body.lines() {
        if let Some(header) = extract_requirement_title(line.trim()) {
            out.insert(header);
        }
    }
    Some(out)
}

/// Extract the requirement header from a `### Requirement: <header>`
/// line. Accepts 3+ leading `#` for parser robustness (the canonical
/// shape is exactly three).
fn extract_requirement_title(line: &str) -> Option<String> {
    let stripped = line.trim_start_matches('#');
    if stripped == line {
        return None;
    }
    let stripped = stripped.trim_start();
    let rest = stripped.strip_prefix("Requirement:")?;
    let header = rest.trim();
    if header.is_empty() {
        None
    } else {
        Some(header.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn write_change_spec(workspace: &Path, change: &str, capability: &str, body: &str) {
        write(
            &workspace
                .join("openspec/changes")
                .join(change)
                .join("specs")
                .join(capability)
                .join("spec.md"),
            body,
        );
    }

    fn write_canonical_spec(workspace: &Path, capability: &str, body: &str) {
        write(
            &workspace
                .join("openspec/specs")
                .join(capability)
                .join("spec.md"),
            body,
        );
    }

    // ---------- ADDED kind ----------

    #[test]
    fn added_title_not_in_canonical_passes() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: Existing\nThe system SHALL existing.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## ADDED Requirements\n\n### Requirement: New thing\nThe system SHALL new.\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert!(v.is_empty(), "expected pass, got {v:#?}");
    }

    #[test]
    fn added_title_already_in_canonical_is_flagged() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: Existing\nThe system SHALL existing.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## ADDED Requirements\n\n### Requirement: Existing\nThe system SHALL existing.\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DeltaKind::Added);
        assert_eq!(v[0].capability, "cap");
        assert_eq!(v[0].header, "Existing");
        assert!(v[0].reason.contains("use MODIFIED instead"), "got {:?}", v[0].reason);
    }

    // ---------- MODIFIED kind (the a07 case) ----------

    #[test]
    fn modified_title_in_canonical_passes() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "code-reviewer",
            "## Requirements\n\n### Requirement: AI-driven code-quality review\nThe reviewer SHALL accept.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "code-reviewer",
            "## MODIFIED Requirements\n\n### Requirement: AI-driven code-quality review\nReplacement body SHALL.\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert!(v.is_empty(), "expected pass, got {v:#?}");
    }

    #[test]
    fn modified_title_missing_from_canonical_is_flagged_a07_case() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "code-reviewer",
            "## Requirements\n\n### Requirement: AI-driven code-quality review\nBody.\n",
        );
        // The a07 case: invented header, capitalisation drift.
        write_change_spec(
            ws,
            "c1",
            "code-reviewer",
            "## MODIFIED Requirements\n\n### Requirement: Reviewer prompt budget is operator-configurable\nBody SHALL.\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DeltaKind::Modified);
        assert_eq!(v[0].capability, "code-reviewer");
        assert_eq!(v[0].header, "Reviewer prompt budget is operator-configurable");
        assert!(v[0].reason.contains("header not found in canonical"));
        assert!(v[0].reason.contains("a07-style"));
    }

    #[test]
    fn modified_title_with_one_char_difference_is_flagged() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: Existing requirement\nBody.\n",
        );
        // Note trailing period in MODIFIED — different by exactly one character.
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## MODIFIED Requirements\n\n### Requirement: Existing requirement.\nNew body SHALL.\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert_eq!(v.len(), 1, "got {v:#?}");
        assert_eq!(v[0].kind, DeltaKind::Modified);
    }

    // ---------- REMOVED kind ----------

    #[test]
    fn removed_title_in_canonical_passes() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: Going away\nBody.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## REMOVED Requirements\n\n### Requirement: Going away\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert!(v.is_empty(), "expected pass, got {v:#?}");
    }

    #[test]
    fn removed_title_not_in_canonical_is_flagged() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: Different\nBody.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## REMOVED Requirements\n\n### Requirement: Not here\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DeltaKind::Removed);
        assert!(v[0].reason.contains("cannot remove non-existent"));
    }

    // ---------- RENAMED kind ----------

    #[test]
    fn renamed_from_exists_to_does_not_passes() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: Old\nBody.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## RENAMED Requirements\n\n- FROM: `Old`\n  TO: `New`\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert!(v.is_empty(), "expected pass, got {v:#?}");
    }

    #[test]
    fn renamed_from_missing_is_flagged() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: Unrelated\nBody.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## RENAMED Requirements\n\n- FROM: `Old`\n  TO: `New`\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DeltaKind::Renamed);
        assert_eq!(v[0].header, "from Old to New");
        assert!(v[0].reason.contains("from-title not found"));
    }

    #[test]
    fn renamed_to_already_exists_is_flagged() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: Old\nBody.\n\n### Requirement: New\nBody.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## RENAMED Requirements\n\n- FROM: `Old`\n  TO: `New`\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DeltaKind::Renamed);
        assert!(v[0].reason.contains("rename would create a duplicate"));
    }

    // ---------- New capability (no canonical spec) ----------

    #[test]
    fn new_capability_added_only_passes() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        // No canonical spec for "brand-new-cap".
        write_change_spec(
            ws,
            "c1",
            "brand-new-cap",
            "## ADDED Requirements\n\n### Requirement: First\nThe system SHALL first.\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert!(v.is_empty(), "ADDs on new capability must pass, got {v:#?}");
    }

    #[test]
    fn new_capability_modified_is_flagged() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_change_spec(
            ws,
            "c1",
            "brand-new-cap",
            "## MODIFIED Requirements\n\n### Requirement: First\nThe system SHALL first.\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DeltaKind::Modified);
        assert!(v[0].reason.contains("has no canonical spec"));
    }

    // ---------- Multi-capability + multi-violation aggregation ----------

    #[test]
    fn multiple_capabilities_aggregate_violations() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "alpha",
            "## Requirements\n\n### Requirement: Real-alpha\nBody.\n",
        );
        write_canonical_spec(
            ws,
            "beta",
            "## Requirements\n\n### Requirement: Real-beta\nBody.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "alpha",
            "## MODIFIED Requirements\n\n### Requirement: Wrong-alpha\nBody SHALL.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "beta",
            "## REMOVED Requirements\n\n### Requirement: Wrong-beta\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert_eq!(v.len(), 2, "expected one per capability, got {v:#?}");
        // Deterministic capability ordering — alpha sorts before beta.
        assert_eq!(v[0].capability, "alpha");
        assert_eq!(v[1].capability, "beta");
    }

    // ---------- Edge cases ----------

    #[test]
    fn change_without_specs_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/c1")).unwrap();
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn empty_delta_block_is_no_op() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_canonical_spec(
            ws,
            "cap",
            "## Requirements\n\n### Requirement: X\nBody.\n",
        );
        // Header present, no `### Requirement:` lines below it.
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## ADDED Requirements\n\n(no requirements yet)\n",
        );
        let v = check_spec_deltas_archivable(ws, "c1").unwrap();
        assert!(v.is_empty(), "empty delta block must be a no-op, got {v:#?}");
    }
}
