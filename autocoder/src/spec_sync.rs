//! Delta-merge primitives for the `spec_sync_audit`. Pure-data: parses
//! archived-change delta specs and canonical capability specs, applies
//! ADDED/MODIFIED/REMOVED/RENAMED operations in chronological order,
//! serializes the result back to canonical-spec form, and surfaces a
//! [`SyncPlan`] the audit can execute. Separating compute from apply
//! keeps the merge algorithm trivially testable (no filesystem mutation
//! inside `compute_sync_plan`) and gives a future `autocoder
//! sync-specs --dry-run` CLI a natural API.

use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A single requirement block parsed out of either a delta spec
/// (under one of the four section headers) or a canonical spec.
///
/// `block_text` is the verbatim heading + body + every `#### Scenario:`
/// block, captured up to (but not including) the next requirement or
/// next `## ` section heading. The serializer just glues these together
/// so the algorithm preserves operator-authored formatting byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub title: String,
    pub block_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamedRequirement {
    pub from: String,
    pub to: String,
    /// Some RENAMED entries carry replacement body text; others rename
    /// in place. When `None` the original canonical block is kept and
    /// only its `### Requirement: <title>` heading is rewritten.
    pub new_block: Option<Requirement>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedDelta {
    pub added: Vec<Requirement>,
    pub modified: Vec<Requirement>,
    pub removed: Vec<String>,
    pub renamed: Vec<RenamedRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSpec {
    /// Everything up to and including the `## Requirements` header. The
    /// serializer re-emits this verbatim so any operator-authored
    /// `## Purpose` / capability-title content is preserved.
    pub preamble: String,
    pub requirements: Vec<Requirement>,
    /// Anything after the last requirement. Rare, but some hand-edited
    /// specs include trailing notes; the serializer preserves them.
    pub trailing: String,
}

/// Result of one [`apply_delta`] call. Aggregated across all deltas to
/// power the audit's `Finding` body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub renamed: usize,
    /// Operator-informational notes ("requirement X was MODIFIED but
    /// didn't exist in canonical — treated as ADDED", etc.). The audit
    /// logs each at WARN level.
    pub warnings: Vec<String>,
}

impl MergeReport {
    pub fn merge(&mut self, other: MergeReport) {
        self.added += other.added;
        self.modified += other.modified;
        self.removed += other.removed;
        self.renamed += other.renamed;
        self.warnings.extend(other.warnings);
    }
}

// ---------------- Section-header constants ----------------

const SECTION_ADDED: &str = "## ADDED Requirements";
const SECTION_MODIFIED: &str = "## MODIFIED Requirements";
const SECTION_REMOVED: &str = "## REMOVED Requirements";
const SECTION_RENAMED: &str = "## RENAMED Requirements";
const SECTION_REQUIREMENTS: &str = "## Requirements";

// ---------------- Parsing ----------------

/// Parse an archived-change `specs/<capability>/spec.md` into its four
/// delta sections. Missing sections are simply absent from the result;
/// section order doesn't matter (some archives have MODIFIED before
/// ADDED). A spec containing only a header and nothing else parses to
/// an empty `ParsedDelta`.
pub fn parse_delta_spec(path: &Path) -> Result<ParsedDelta> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading delta spec {}", path.display()))?;
    Ok(parse_delta_str(&raw))
}

pub(crate) fn parse_delta_str(src: &str) -> ParsedDelta {
    let mut out = ParsedDelta::default();
    let sections = split_into_sections(src);
    for (header, body) in sections {
        match header.as_str() {
            SECTION_ADDED => out.added = parse_requirement_blocks(&body),
            SECTION_MODIFIED => out.modified = parse_requirement_blocks(&body),
            SECTION_REMOVED => out.removed = parse_removed_titles(&body),
            SECTION_RENAMED => out.renamed = parse_renamed_blocks(&body),
            _ => {}
        }
    }
    out
}

/// Parse a canonical `openspec/specs/<capability>/spec.md`. Returns Err
/// when the file lacks a `## Requirements` header — the merge algorithm
/// can't insert blocks without that anchor.
pub fn parse_canonical_spec(path: &Path) -> Result<CanonicalSpec> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading canonical spec {}", path.display()))?;
    parse_canonical_str(&raw)
        .with_context(|| format!("parsing canonical spec {}", path.display()))
}

pub(crate) fn parse_canonical_str(src: &str) -> Result<CanonicalSpec> {
    let lines: Vec<&str> = src.lines().collect();
    let req_header_idx = lines
        .iter()
        .position(|l| l.trim_end() == SECTION_REQUIREMENTS)
        .ok_or_else(|| {
            anyhow!(
                "canonical spec is missing the `## Requirements` header — \
                 cannot determine where to insert merged requirement blocks"
            )
        })?;
    // Preamble = everything up to AND INCLUDING the `## Requirements`
    // line. We include the trailing newline so the serializer just
    // appends the requirements blocks after it.
    let mut preamble = lines[..=req_header_idx].join("\n");
    preamble.push('\n');

    // Body after the requirements header runs until either EOF or the
    // next `## ` top-level heading. Anything beyond that is `trailing`.
    let body_start = req_header_idx + 1;
    let mut body_end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(body_start) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("## ") && !trimmed.starts_with("### ") {
            body_end = i;
            break;
        }
    }
    let body_block = lines[body_start..body_end].join("\n");
    let requirements = parse_requirement_blocks(&body_block);
    let trailing = if body_end < lines.len() {
        let mut t = lines[body_end..].join("\n");
        if !t.ends_with('\n') {
            t.push('\n');
        }
        t
    } else {
        String::new()
    };

    Ok(CanonicalSpec {
        preamble,
        requirements,
        trailing,
    })
}

/// Split a full spec into `(section_header, body)` pairs. Each header is
/// any line matching `^## ` (without `###`). The body is every line
/// between this header and the next `## ` (or EOF).
fn split_into_sections(src: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut headers: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("## ") && !trimmed.starts_with("### ") {
            headers.push((i, trimmed.trim_end().to_string()));
        }
    }
    let mut out = Vec::new();
    for (idx, (line_no, header)) in headers.iter().enumerate() {
        let body_start = line_no + 1;
        let body_end = headers
            .get(idx + 1)
            .map(|(n, _)| *n)
            .unwrap_or(lines.len());
        let body = lines[body_start..body_end].join("\n");
        out.push((header.clone(), body));
    }
    out
}

/// Parse a section body into [`Requirement`] blocks. Each block starts
/// at a `### Requirement: <title>` line and runs to the next
/// `### Requirement:` line (or end of body).
fn parse_requirement_blocks(body: &str) -> Vec<Requirement> {
    let lines: Vec<&str> = body.lines().collect();
    let mut starts: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("### Requirement:") {
            starts.push(i);
        }
    }
    let mut out = Vec::new();
    for (idx, start) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(lines.len());
        let title = extract_requirement_title(lines[*start]);
        // Drop a single trailing blank line so blocks concatenate cleanly.
        let mut block_lines = lines[*start..end].to_vec();
        while block_lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
            block_lines.pop();
        }
        let block_text = block_lines.join("\n");
        out.push(Requirement { title, block_text });
    }
    out
}

fn extract_requirement_title(line: &str) -> String {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("### Requirement:").unwrap_or(trimmed);
    rest.trim().to_string()
}

/// Parse the body of `## REMOVED Requirements` into a list of titles.
/// Each `### Requirement: <title>` heading (or, defensively, a bare
/// title bullet) counts as one removal.
fn parse_removed_titles(body: &str) -> Vec<String> {
    let mut titles = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("### Requirement:") {
            titles.push(rest.trim().to_string());
        }
    }
    titles
}

/// Parse the body of `## RENAMED Requirements`. The OpenSpec convention
/// is a sequence of blocks beginning `### Requirement: <to>` whose body
/// contains `- **FROM:** <from>` (and optionally the new body). When no
/// FROM marker is present, the rename is interpreted as an in-place
/// rename of the title only.
fn parse_renamed_blocks(body: &str) -> Vec<RenamedRequirement> {
    let blocks = parse_requirement_blocks(body);
    let mut out = Vec::new();
    for block in blocks {
        // Walk the block text looking for `**FROM:**` or `**FROM**:`.
        let from = block
            .block_text
            .lines()
            .find_map(|line| {
                let l = line.trim_start();
                let lower = l.to_ascii_lowercase();
                // Tolerate `**FROM:** name`, `- **FROM:** name`, etc.
                let needle = "**from:**";
                if let Some(idx) = lower.find(needle) {
                    let tail = l[idx + needle.len()..].trim();
                    return Some(tail.trim_matches('`').to_string());
                }
                None
            });
        let to = block.title.clone();
        let new_block = Some(block);
        out.push(RenamedRequirement {
            from: from.unwrap_or_else(|| to.clone()),
            to,
            new_block,
        });
    }
    out
}

// ---------------- Merge ----------------

/// Apply one delta's four sections to `canonical`. See the rules in
/// `tasks.md` §2.1. Mutates in place; returns a [`MergeReport`] the
/// caller aggregates across all deltas.
pub fn apply_delta(canonical: &mut CanonicalSpec, delta: &ParsedDelta) -> MergeReport {
    let mut report = MergeReport::default();

    for req in &delta.added {
        if let Some(existing) = canonical
            .requirements
            .iter_mut()
            .find(|r| r.title == req.title)
        {
            report.warnings.push(format!(
                "ADDED requirement `{}` already exists in canonical — treating as MODIFIED",
                req.title
            ));
            *existing = req.clone();
            report.modified += 1;
        } else {
            canonical.requirements.push(req.clone());
            report.added += 1;
        }
    }

    for req in &delta.modified {
        if let Some(existing) = canonical
            .requirements
            .iter_mut()
            .find(|r| r.title == req.title)
        {
            *existing = req.clone();
            report.modified += 1;
        } else {
            report.warnings.push(format!(
                "MODIFIED requirement `{}` not found in canonical — treating as ADDED",
                req.title
            ));
            canonical.requirements.push(req.clone());
            report.added += 1;
        }
    }

    for title in &delta.removed {
        let before = canonical.requirements.len();
        canonical.requirements.retain(|r| &r.title != title);
        if canonical.requirements.len() < before {
            report.removed += 1;
        }
        // No warning — REMOVED of an absent title is already the end
        // state (DEBUG-only per spec).
    }

    for rename in &delta.renamed {
        let pos = canonical
            .requirements
            .iter()
            .position(|r| r.title == rename.from);
        match pos {
            Some(idx) => {
                if let Some(new_block) = &rename.new_block {
                    canonical.requirements[idx] = Requirement {
                        title: rename.to.clone(),
                        block_text: new_block.block_text.clone(),
                    };
                } else {
                    // Rewrite the title-line in place; everything else
                    // stays untouched.
                    let block = &canonical.requirements[idx];
                    let rewritten = rewrite_requirement_title(&block.block_text, &rename.to);
                    canonical.requirements[idx] = Requirement {
                        title: rename.to.clone(),
                        block_text: rewritten,
                    };
                }
                report.renamed += 1;
            }
            None => {
                if let Some(new_block) = &rename.new_block {
                    report.warnings.push(format!(
                        "RENAMED requirement `{from}` -> `{to}` not found in canonical — \
                         falling back to ADDED of `{to}`",
                        from = rename.from,
                        to = rename.to,
                    ));
                    canonical.requirements.push(new_block.clone());
                    report.added += 1;
                } else {
                    report.warnings.push(format!(
                        "RENAMED requirement `{}` -> `{}` not found in canonical and \
                         no replacement body provided — skipping",
                        rename.from, rename.to,
                    ));
                }
            }
        }
    }

    report
}

fn rewrite_requirement_title(block_text: &str, new_title: &str) -> String {
    let mut out = String::with_capacity(block_text.len());
    let mut first = true;
    for line in block_text.lines() {
        if first && line.trim_start().starts_with("### Requirement:") {
            // Preserve any indentation before the heading.
            let indent_len = line.len() - line.trim_start().len();
            out.push_str(&line[..indent_len]);
            out.push_str("### Requirement: ");
            out.push_str(new_title);
        } else {
            out.push_str(line);
        }
        out.push('\n');
        first = false;
    }
    // Strip the trailing newline `lines()` doesn't reintroduce on its
    // own — but `block_text` may or may not have ended with one. Match
    // the original.
    if !block_text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Serialize a [`CanonicalSpec`] back to file text. The output is
/// idempotent under `parse_canonical_str(serialize_canonical(spec))`
/// up to trailing-newline normalization.
pub fn serialize_canonical(spec: &CanonicalSpec) -> String {
    let mut out = String::with_capacity(
        spec.preamble.len()
            + spec
                .requirements
                .iter()
                .map(|r| r.block_text.len() + 2)
                .sum::<usize>()
            + spec.trailing.len(),
    );
    out.push_str(&spec.preamble);
    // A blank line between the `## Requirements` header and the first
    // block reads better. Existing canonical files mostly have it.
    if !spec.requirements.is_empty() {
        out.push('\n');
    }
    for (i, req) in spec.requirements.iter().enumerate() {
        out.push_str(&req.block_text);
        if !req.block_text.ends_with('\n') {
            out.push('\n');
        }
        if i + 1 < spec.requirements.len() {
            out.push('\n');
        }
    }
    if !spec.trailing.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&spec.trailing);
    }
    out
}

// ---------------- Planner ----------------

#[derive(Debug, Clone)]
pub struct CapabilityPlan {
    pub canonical_path: PathBuf,
    /// Ordered (chronological by archived-change name) list of deltas
    /// to apply to this capability. The `String` key is the archive
    /// directory name (e.g. `2026-05-23-foo-bar`) so the audit can name
    /// the source archive in WARN logs.
    pub deltas: Vec<(String, ParsedDelta)>,
}

#[derive(Debug, Clone, Default)]
pub struct SyncPlan {
    pub per_capability: BTreeMap<String, CapabilityPlan>,
}

/// Scan `<workspace>/openspec/changes/archive/` and build a [`SyncPlan`].
/// Archives lacking a `specs/` subdir are silently skipped (a proposal-
/// only archive is legitimate). Archives whose `specs/` is malformed
/// produce a parse error — better to surface a malformed spec than
/// silently drop it.
pub fn compute_sync_plan(workspace: &Path) -> Result<SyncPlan> {
    let mut plan = SyncPlan::default();
    let archive_root = workspace.join("openspec/changes/archive");
    if !archive_root.is_dir() {
        return Ok(plan);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&archive_root)
        .with_context(|| format!("reading archive root {}", archive_root.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    entries.sort();

    for archive_dir in entries {
        let change_name = archive_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let specs_dir = archive_dir.join("specs");
        if !specs_dir.is_dir() {
            continue;
        }
        let cap_entries = match std::fs::read_dir(&specs_dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for cap_entry in cap_entries.flatten() {
            let cap_path = cap_entry.path();
            if !cap_path.is_dir() {
                continue;
            }
            let cap_name = match cap_path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let spec_path = cap_path.join("spec.md");
            if !spec_path.is_file() {
                continue;
            }
            let delta = parse_delta_spec(&spec_path).with_context(|| {
                format!(
                    "parsing delta spec for archived change `{change_name}`, capability `{cap_name}`"
                )
            })?;
            // Skip empty deltas — every section absent or empty means
            // this archive had nothing to merge for this capability.
            if delta.added.is_empty()
                && delta.modified.is_empty()
                && delta.removed.is_empty()
                && delta.renamed.is_empty()
            {
                continue;
            }
            let entry = plan
                .per_capability
                .entry(cap_name.clone())
                .or_insert_with(|| CapabilityPlan {
                    canonical_path: workspace
                        .join("openspec/specs")
                        .join(&cap_name)
                        .join("spec.md"),
                    deltas: Vec::new(),
                });
            entry.deltas.push((change_name.clone(), delta));
        }
    }
    Ok(plan)
}

/// Execute a [`SyncPlan`] against the on-disk canonical specs. Returns
/// the list of paths actually modified (empty == no drift).
///
/// For each capability:
/// 1. Parse the canonical spec (or build a minimal empty one if absent).
/// 2. Apply every delta in chronological order.
/// 3. Serialize. Compare with the original file contents on disk. If
///    bytes unchanged, no write. Otherwise overwrite + add to result.
///
/// The return value's `(report, paths)` lets the audit summarize what
/// happened across all capabilities for the chatops `Finding`.
pub fn apply_sync_plan(plan: &SyncPlan) -> Result<(Vec<PathBuf>, MergeReport)> {
    let mut changed = Vec::new();
    let mut aggregate = MergeReport::default();
    for (cap_name, cap_plan) in &plan.per_capability {
        let (mut canonical, original_text) = load_or_build_canonical(&cap_plan.canonical_path, cap_name)?;
        for (_archive_name, delta) in &cap_plan.deltas {
            let report = apply_delta(&mut canonical, delta);
            aggregate.merge(report);
        }
        let serialized = serialize_canonical(&canonical);
        if Some(serialized.as_str()) == original_text.as_deref() {
            continue;
        }
        if let Some(parent) = cap_plan.canonical_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating spec parent dir {}", parent.display()))?;
        }
        std::fs::write(&cap_plan.canonical_path, &serialized).with_context(|| {
            format!(
                "writing merged canonical spec to {}",
                cap_plan.canonical_path.display()
            )
        })?;
        changed.push(cap_plan.canonical_path.clone());
    }
    Ok((changed, aggregate))
}

/// Load the canonical spec at `path`, or fabricate a minimal valid one
/// when the file does not yet exist. The fabricated spec has a header
/// matching the capability name and an empty requirements section so
/// `apply_delta` can append into it.
fn load_or_build_canonical(
    path: &Path,
    capability_name: &str,
) -> Result<(CanonicalSpec, Option<String>)> {
    if path.is_file() {
        let parsed = parse_canonical_spec(path)?;
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading canonical spec {}", path.display()))?;
        return Ok((parsed, Some(raw)));
    }
    let preamble = format!(
        "# {capability_name} Specification\n\n## Purpose\nTBD - created by spec_sync_audit; update Purpose by hand.\n\n## Requirements\n"
    );
    Ok((
        CanonicalSpec {
            preamble,
            requirements: Vec::new(),
            trailing: String::new(),
        },
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---------------- §1 parsing tests ----------------

    #[test]
    fn parse_delta_spec_extracts_added_modified_removed_renamed() {
        let src = r#"## ADDED Requirements

### Requirement: A
A body.

#### Scenario: A1
- WHEN x
- THEN y

### Requirement: B
B body.

## MODIFIED Requirements

### Requirement: C
C body changed.

## REMOVED Requirements

### Requirement: D

## RENAMED Requirements

### Requirement: E new name
- **FROM:** E old name
E new body.
"#;
        let parsed = parse_delta_str(src);
        let titles: Vec<&str> = parsed.added.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["A", "B"]);
        assert_eq!(parsed.modified.len(), 1);
        assert_eq!(parsed.modified[0].title, "C");
        assert_eq!(parsed.removed, vec!["D".to_string()]);
        assert_eq!(parsed.renamed.len(), 1);
        assert_eq!(parsed.renamed[0].from, "E old name");
        assert_eq!(parsed.renamed[0].to, "E new name");
        assert!(parsed.renamed[0].new_block.is_some());
    }

    #[test]
    fn parse_delta_spec_handles_section_order_variation() {
        let src = r#"## MODIFIED Requirements

### Requirement: M1
M body.

## ADDED Requirements

### Requirement: A1
A body.
"#;
        let parsed = parse_delta_str(src);
        assert_eq!(parsed.added.len(), 1);
        assert_eq!(parsed.modified.len(), 1);
        assert_eq!(parsed.added[0].title, "A1");
        assert_eq!(parsed.modified[0].title, "M1");
    }

    #[test]
    fn parse_delta_spec_empty_sections_yield_empty_vecs() {
        let src = "## ADDED Requirements\n\n## REMOVED Requirements\n";
        let parsed = parse_delta_str(src);
        assert!(parsed.added.is_empty());
        assert!(parsed.removed.is_empty());
    }

    #[test]
    fn parse_canonical_spec_round_trips() {
        let src = r#"# foo Specification

## Purpose
Foo does foo.

## Requirements

### Requirement: First
First body.

#### Scenario: One
- WHEN
- THEN

### Requirement: Second
Second body.
"#;
        let parsed = parse_canonical_str(src).expect("parses");
        assert_eq!(parsed.requirements.len(), 2);
        assert_eq!(parsed.requirements[0].title, "First");
        let serialized = serialize_canonical(&parsed);
        let reparsed = parse_canonical_str(&serialized).expect("reparses");
        assert_eq!(parsed.requirements, reparsed.requirements);
    }

    #[test]
    fn parse_canonical_spec_errors_on_missing_requirements_header() {
        let src = "# foo Specification\n\nNo requirements header here.\n";
        let err = parse_canonical_str(src).expect_err("must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Requirements"),
            "error must name the missing structure: {msg}"
        );
    }

    // ---------------- §2 merge tests ----------------

    fn canonical_with(reqs: Vec<(&str, &str)>) -> CanonicalSpec {
        let requirements = reqs
            .into_iter()
            .map(|(title, body)| Requirement {
                title: title.into(),
                block_text: format!("### Requirement: {title}\n{body}"),
            })
            .collect();
        CanonicalSpec {
            preamble: "# cap Specification\n\n## Requirements\n".into(),
            requirements,
            trailing: String::new(),
        }
    }

    fn req(title: &str, body: &str) -> Requirement {
        Requirement {
            title: title.into(),
            block_text: format!("### Requirement: {title}\n{body}"),
        }
    }

    #[test]
    fn apply_added_appends_when_absent() {
        let mut c = canonical_with(vec![("A", "body A")]);
        let d = ParsedDelta {
            added: vec![req("B", "body B")],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.added, 1);
        assert_eq!(report.modified, 0);
        assert!(report.warnings.is_empty());
        assert_eq!(c.requirements.len(), 2);
        assert_eq!(c.requirements[1].title, "B");
    }

    #[test]
    fn apply_added_replaces_when_present_with_warn() {
        let mut c = canonical_with(vec![("A", "body A old")]);
        let d = ParsedDelta {
            added: vec![req("A", "body A NEW")],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.modified, 1);
        assert_eq!(report.added, 0);
        assert_eq!(report.warnings.len(), 1);
        assert!(c.requirements[0].block_text.contains("body A NEW"));
    }

    #[test]
    fn apply_modified_replaces() {
        let mut c = canonical_with(vec![("A", "body A old")]);
        let d = ParsedDelta {
            modified: vec![req("A", "body A new")],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.modified, 1);
        assert!(report.warnings.is_empty());
        assert!(c.requirements[0].block_text.contains("body A new"));
    }

    #[test]
    fn apply_modified_appends_with_warn_when_absent() {
        let mut c = canonical_with(vec![("A", "body A")]);
        let d = ParsedDelta {
            modified: vec![req("Ghost", "body G")],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.added, 1);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("Ghost"));
        assert_eq!(c.requirements.len(), 2);
    }

    #[test]
    fn apply_removed_removes() {
        let mut c = canonical_with(vec![("A", "x"), ("B", "y")]);
        let d = ParsedDelta {
            removed: vec!["A".into()],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.removed, 1);
        assert_eq!(c.requirements.len(), 1);
        assert_eq!(c.requirements[0].title, "B");
    }

    #[test]
    fn apply_removed_noop_when_absent() {
        let mut c = canonical_with(vec![("A", "x")]);
        let d = ParsedDelta {
            removed: vec!["Ghost".into()],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.removed, 0);
        // Per spec: DEBUG only, no warning.
        assert!(report.warnings.is_empty());
        assert_eq!(c.requirements.len(), 1);
    }

    #[test]
    fn apply_renamed_changes_title() {
        let mut c = canonical_with(vec![("Old name", "body")]);
        let d = ParsedDelta {
            renamed: vec![RenamedRequirement {
                from: "Old name".into(),
                to: "New name".into(),
                new_block: None,
            }],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.renamed, 1);
        assert_eq!(c.requirements[0].title, "New name");
        assert!(c.requirements[0].block_text.contains("### Requirement: New name"));
        assert!(c.requirements[0].block_text.contains("body"));
    }

    #[test]
    fn apply_renamed_with_new_block_replaces_body() {
        let mut c = canonical_with(vec![("Old", "old body")]);
        let d = ParsedDelta {
            renamed: vec![RenamedRequirement {
                from: "Old".into(),
                to: "New".into(),
                new_block: Some(req("New", "fresh body")),
            }],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.renamed, 1);
        assert!(c.requirements[0].block_text.contains("fresh body"));
    }

    #[test]
    fn apply_renamed_falls_back_to_added_when_from_missing() {
        let mut c = canonical_with(vec![("Other", "x")]);
        let d = ParsedDelta {
            renamed: vec![RenamedRequirement {
                from: "Ghost".into(),
                to: "Newly added".into(),
                new_block: Some(req("Newly added", "n body")),
            }],
            ..Default::default()
        };
        let report = apply_delta(&mut c, &d);
        assert_eq!(report.added, 1);
        assert_eq!(report.renamed, 0);
        assert!(report.warnings[0].contains("Ghost"));
        assert!(c.requirements.iter().any(|r| r.title == "Newly added"));
    }

    #[test]
    fn serialize_round_trip() {
        let mut c = canonical_with(vec![("A", "a body"), ("B", "b body")]);
        let d = ParsedDelta {
            added: vec![req("C", "c body")],
            modified: vec![req("A", "a body REVISED")],
            ..Default::default()
        };
        apply_delta(&mut c, &d);
        let serialized = serialize_canonical(&c);
        let reparsed = parse_canonical_str(&serialized).expect("reparses");
        assert_eq!(reparsed.requirements.len(), 3);
        let titles: Vec<&str> = reparsed
            .requirements
            .iter()
            .map(|r| r.title.as_str())
            .collect();
        assert_eq!(titles, vec!["A", "B", "C"]);
    }

    // ---------------- §3 planner tests ----------------

    /// Lay out a fake workspace under `root` with the given archived
    /// changes. Each archive entry is `(name, capability, delta_body)`.
    fn fixture_workspace(
        root: &Path,
        archives: &[(&str, &str, &str)],
        canonical_files: &[(&str, &str)],
    ) {
        for (archive_name, capability, body) in archives {
            let p = root
                .join("openspec/changes/archive")
                .join(archive_name)
                .join("specs")
                .join(capability);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("spec.md"), body).unwrap();
        }
        for (capability, body) in canonical_files {
            let p = root.join("openspec/specs").join(capability);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("spec.md"), body).unwrap();
        }
    }

    #[test]
    fn compute_sync_plan_finds_all_capabilities_across_chronological_archives() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        fixture_workspace(
            ws,
            &[
                (
                    "2026-01-01-first",
                    "cap-a",
                    "## ADDED Requirements\n\n### Requirement: One\nbody one.\n",
                ),
                (
                    "2026-02-01-second",
                    "cap-a",
                    "## ADDED Requirements\n\n### Requirement: Two\nbody two.\n",
                ),
                (
                    "2026-01-15-other",
                    "cap-b",
                    "## ADDED Requirements\n\n### Requirement: Three\nbody three.\n",
                ),
            ],
            &[],
        );
        let plan = compute_sync_plan(ws).unwrap();
        assert_eq!(plan.per_capability.len(), 2);
        let cap_a = plan.per_capability.get("cap-a").unwrap();
        assert_eq!(cap_a.deltas.len(), 2);
        // Chronological order — first archive earlier than second.
        assert_eq!(cap_a.deltas[0].0, "2026-01-01-first");
        assert_eq!(cap_a.deltas[1].0, "2026-02-01-second");
        assert!(plan.per_capability.contains_key("cap-b"));
    }

    #[test]
    fn apply_sync_plan_idempotent_on_clean_repo() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Canonical already contains the requirement; the archive's
        // delta would be a no-op.
        let canonical_body = "# cap Specification\n\n## Purpose\nP.\n\n## Requirements\n\n### Requirement: One\nbody one.\n";
        fixture_workspace(
            ws,
            &[(
                "2026-01-01-first",
                "cap",
                "## ADDED Requirements\n\n### Requirement: One\nbody one.\n",
            )],
            &[("cap", canonical_body)],
        );
        let plan = compute_sync_plan(ws).unwrap();
        let (changed, _report) = apply_sync_plan(&plan).unwrap();
        // The canonical already has this requirement with identical
        // text → no write.
        assert!(
            changed.is_empty(),
            "clean repo must produce zero writes; got {changed:?}"
        );
    }

    #[test]
    fn apply_sync_plan_backfill() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Empty canonical; two archives each add one requirement.
        let canonical_body = "# cap Specification\n\n## Purpose\nP.\n\n## Requirements\n";
        fixture_workspace(
            ws,
            &[
                (
                    "2026-01-01-first",
                    "cap",
                    "## ADDED Requirements\n\n### Requirement: One\nbody one.\n",
                ),
                (
                    "2026-02-01-second",
                    "cap",
                    "## ADDED Requirements\n\n### Requirement: Two\nbody two.\n",
                ),
            ],
            &[("cap", canonical_body)],
        );
        let plan = compute_sync_plan(ws).unwrap();
        let (changed, report) = apply_sync_plan(&plan).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(report.added, 2);
        let body = std::fs::read_to_string(&changed[0]).unwrap();
        let pos_one = body.find("### Requirement: One").expect("One present");
        let pos_two = body.find("### Requirement: Two").expect("Two present");
        assert!(pos_one < pos_two, "chronological order preserved");
    }

    #[test]
    fn apply_sync_plan_creates_canonical_when_absent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        fixture_workspace(
            ws,
            &[(
                "2026-01-01-first",
                "new-cap",
                "## ADDED Requirements\n\n### Requirement: One\nbody one.\n",
            )],
            &[],
        );
        let plan = compute_sync_plan(ws).unwrap();
        let (changed, _report) = apply_sync_plan(&plan).unwrap();
        assert_eq!(changed.len(), 1);
        let canonical = ws.join("openspec/specs/new-cap/spec.md");
        assert!(canonical.is_file());
        let body = std::fs::read_to_string(&canonical).unwrap();
        assert!(body.contains("### Requirement: One"));
        assert!(body.contains("## Requirements"));
    }

    #[test]
    fn apply_sync_plan_second_run_is_noop_after_first() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let canonical_body = "# cap Specification\n\n## Purpose\nP.\n\n## Requirements\n";
        fixture_workspace(
            ws,
            &[(
                "2026-01-01-first",
                "cap",
                "## ADDED Requirements\n\n### Requirement: One\nbody one.\n",
            )],
            &[("cap", canonical_body)],
        );
        let plan1 = compute_sync_plan(ws).unwrap();
        let (changed1, _) = apply_sync_plan(&plan1).unwrap();
        assert_eq!(changed1.len(), 1);
        // Re-plan + re-apply should see canonical already-matching →
        // empty change list.
        let plan2 = compute_sync_plan(ws).unwrap();
        let (changed2, _) = apply_sync_plan(&plan2).unwrap();
        assert!(
            changed2.is_empty(),
            "second run must be noop; got {changed2:?}"
        );
    }
}
