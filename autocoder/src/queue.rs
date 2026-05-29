//! OpenSpec queue engine — enumerate, lock, archive, and unarchive changes
//! against a workspace.
//!
//! All functions operate on a `workspace` path that contains an
//! `openspec/changes/` directory. The filesystem is the source of truth.

use crate::openspec_archive::{
    ArchiveFailure, ArchiveRunner, RealArchiveRunner,
    openspec_archive_with_postcondition, truncate_for_report,
};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use regex::Regex;
use std::path::{Path, PathBuf};

const CHANGES_SUBDIR: &str = "openspec/changes";
const ARCHIVE_DIR: &str = "archive";
const LOCK_FILE: &str = ".in-progress";
const PROPOSAL_FILE: &str = "proposal.md";
const QUESTION_FILE: &str = ".question.json";
const PERMA_STUCK_FILE: &str = ".perma-stuck.json";
const NEEDS_REVISION_FILE: &str = ".needs-spec-revision.json";
const IGNORE_FOR_QUEUE_FILE: &str = ".ignore-for-queue.json";

fn changes_dir(workspace: &Path) -> PathBuf {
    workspace.join(CHANGES_SUBDIR)
}

fn change_dir(workspace: &Path, change: &str) -> PathBuf {
    changes_dir(workspace).join(change)
}

/// List pending change names: direct subdirectories of
/// `<workspace>/openspec/changes/` that are not the literal `archive`
/// directory, do not begin with `.`, do not contain a `.in-progress` lock
/// file, do not contain a `.question.json` waiting marker, do not contain
/// a `.perma-stuck.json` marker, and contain at least a `proposal.md`
/// file.
///
/// Returns a two-tier ordering (a27a1):
/// - First tier: entries with `.iteration-pending.json` present, sorted
///   by the marker's `iteration_number` ascending (lower iteration first).
///   A corrupt marker is treated as `iteration_number: 0` for ordering;
///   the enumeration does NOT error.
/// - Second tier: entries WITHOUT the marker, sorted ascending by entry
///   name (UTF-8 byte order, which is alphabetical for ASCII names).
///
/// Within each tier, ties on the primary key fall back to entry name
/// ascending for determinism. Operators with stacked dependencies (in
/// the unmarked tier) encode explicit order via numeric prefixes
/// (`01-`, `02-`).
pub fn list_pending(workspace: &Path) -> Result<Vec<String>> {
    let root = changes_dir(workspace);
    if !root.exists() {
        return Ok(Vec::new());
    }
    // Build two tiers:
    // - marked: (iteration_number_for_ordering, name)
    // - unmarked: name
    let mut marked: Vec<(u32, String)> = Vec::new();
    let mut unmarked: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("reading {}", root.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue, // non-UTF8 filename: skip
        };
        if name == ARCHIVE_DIR {
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        let dir = entry.path();
        if dir.join(LOCK_FILE).exists() {
            continue;
        }
        if dir.join(QUESTION_FILE).exists() {
            // Waiting on a human reply — handled by `list_waiting`, not here.
            continue;
        }
        if dir.join(PERMA_STUCK_FILE).exists() || dir.join(NEEDS_REVISION_FILE).exists() {
            // Operator-action markers: perma-stuck (the change has hit the
            // consecutive-failure threshold) and spec-needs-revision (the
            // agent identified one or more unimplementable tasks). In both
            // cases autocoder will not retry until the operator removes the
            // marker file.
            continue;
        }
        if !dir.join(PROPOSAL_FILE).is_file() {
            continue;
        }
        // a27a1: iteration-pending tier check. The marker is NOT an
        // exclusion (unlike `.question.json`); it's a front-insertion
        // preference. A corrupt marker is treated as iteration_number 0
        // for ordering (sorts ahead of any valid marked entries) AND
        // does NOT cause `list_pending` to error.
        let marker_path = dir.join(crate::iteration_pending::MARKER_FILE);
        if marker_path.exists() {
            let iteration_number = match crate::iteration_pending::read_marker(workspace, &name) {
                Ok(Some(m)) => m.iteration_number,
                // A truly absent marker shouldn't happen here (we just
                // checked `exists()`), but treat absent like corrupt for
                // safety — both produce iteration_number 0.
                Ok(None) => 0,
                Err(_) => 0,
            };
            marked.push((iteration_number, name));
        } else {
            unmarked.push(name);
        }
    }
    // Within marked: sort by iteration_number ascending, then by name
    // ascending for ties.
    marked.sort_by(|(a_n, a_name), (b_n, b_name)| {
        a_n.cmp(b_n).then_with(|| a_name.cmp(b_name))
    });
    unmarked.sort();
    let mut out: Vec<String> = Vec::with_capacity(marked.len() + unmarked.len());
    out.extend(marked.into_iter().map(|(_, n)| n));
    out.extend(unmarked);
    Ok(out)
}

/// List changes currently waiting on a human reply (i.e. those containing a
/// `.question.json` file). Returned sorted ascending; archived/dotfile
/// entries are excluded.
pub fn list_waiting(workspace: &Path) -> Result<Vec<String>> {
    let root = changes_dir(workspace);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("reading {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if name == ARCHIVE_DIR || name.starts_with('.') {
            continue;
        }
        if entry.path().join(QUESTION_FILE).exists() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// True when `<workspace>/openspec/changes/<change>/.needs-spec-revision.json`
/// exists. Mirrors the `.perma-stuck.json` presence check: the marker is
/// operator-action territory, and its presence is the exclusive trigger
/// for keeping the change out of `list_pending`.
pub fn is_needs_spec_revision_marked(workspace: &Path, change: &str) -> bool {
    change_dir(workspace, change).join(NEEDS_REVISION_FILE).exists()
}

/// True when `<workspace>/openspec/changes/<change>/.perma-stuck.json` exists.
/// Pure filesystem check.
pub fn is_perma_stuck(workspace: &Path, change: &str) -> bool {
    change_dir(workspace, change).join(PERMA_STUCK_FILE).exists()
}

/// Remove `<workspace>/openspec/changes/<change>/.perma-stuck.json`. Errors
/// when the marker is absent so the operator can be told precisely "no
/// perma-stuck marker for change `<change>`" — chatops surfaces that
/// distinct from accidental success on a typo. Non-NotFound IO errors
/// propagate.
pub fn remove_perma_stuck_marker(workspace: &Path, change: &str) -> Result<()> {
    let path = change_dir(workspace, change).join(PERMA_STUCK_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "no perma-stuck marker for change `{change}`"
        )),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Remove `<workspace>/openspec/changes/<change>/.ignore-for-queue.json`.
/// Errors when the marker is absent with a clear "no ignore-for-queue
/// marker for change `<change>`" message; non-NotFound IO errors propagate.
pub fn remove_ignore_for_queue_marker(workspace: &Path, change: &str) -> Result<()> {
    let path = change_dir(workspace, change).join(IGNORE_FOR_QUEUE_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "no ignore-for-queue marker for change `{change}`"
        )),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Idempotent removal of `.ignore-for-queue.json`. Distinct from
/// `remove_ignore_for_queue_marker` — this is used by
/// `clear-perma-stuck` / similar full-resolution operations where the
/// ignore-marker's absence is a no-op rather than an error.
pub fn remove_ignore_for_queue_marker_idempotent(
    workspace: &Path,
    change: &str,
) -> Result<bool> {
    let path = change_dir(workspace, change).join(IGNORE_FOR_QUEUE_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Remove `<workspace>/openspec/changes/<change>/.needs-spec-revision.json`.
/// Errors when the marker is absent with a clear "no needs-spec-revision
/// marker for change `<change>`" message; non-NotFound IO errors propagate.
pub fn remove_revision_marker(workspace: &Path, change: &str) -> Result<()> {
    let path = change_dir(workspace, change).join(NEEDS_REVISION_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "no needs-spec-revision marker for change `{change}`"
        )),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// True when `<workspace>/openspec/changes/<change>/.ignore-for-queue.json`
/// exists. Pure filesystem check. The marker downgrades any sibling
/// operator-action marker (`.perma-stuck.json`, `.needs-spec-revision.json`,
/// or `.in-progress`/`.question.json` AskUser markers) from "blocks
/// subsequent queue processing" to "still excludes this change, but
/// doesn't block siblings."
pub fn is_ignore_for_queue_marked(workspace: &Path, change: &str) -> bool {
    change_dir(workspace, change).join(IGNORE_FOR_QUEUE_FILE).exists()
}

/// One change that has a queue-blocking marker. Returned by
/// `find_queue_blocking_markers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingMarker {
    pub change: String,
    /// Filename of the blocking marker file (e.g. `.perma-stuck.json`,
    /// `.needs-spec-revision.json`, `.question.json`). The first such
    /// file found in alphabetical order is reported; a change with
    /// multiple blocking markers reports only the first.
    pub marker: String,
}

/// Scan every direct subdirectory of `<workspace>/openspec/changes/`
/// (excluding `archive` and dotfile-named entries) and return the changes
/// that have at least one queue-blocking marker but NO accompanying
/// `.ignore-for-queue.json` marker. The set of blocking markers is:
/// `.question.json` (AskUser waiting), `.needs-spec-revision.json`,
/// `.perma-stuck.json`. The presence of `.ignore-for-queue.json`
/// downgrades the change's blocking effect on its siblings — it stays
/// excluded from `list_pending`, but doesn't gate subsequent changes.
///
/// Returns sorted ascending by change name. An empty result means the
/// queue walk is clear to proceed; a non-empty result means the polling
/// loop should halt the pending walk for this iteration.
pub fn find_queue_blocking_markers(workspace: &Path) -> Result<Vec<BlockingMarker>> {
    let root = changes_dir(workspace);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<BlockingMarker> = Vec::new();
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("reading {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if name == ARCHIVE_DIR || name.starts_with('.') {
            continue;
        }
        let dir = entry.path();
        // Downgrade: if the ignore-marker is present, this change does
        // NOT block the queue (it stays excluded from list_pending via
        // its own marker, but siblings proceed).
        if dir.join(IGNORE_FOR_QUEUE_FILE).exists() {
            continue;
        }
        // Check blocking markers in priority order. The first one
        // found is reported; chained markers (rare) report the
        // highest-priority one.
        let candidates = [
            QUESTION_FILE,
            NEEDS_REVISION_FILE,
            PERMA_STUCK_FILE,
        ];
        for marker in candidates {
            if dir.join(marker).exists() {
                out.push(BlockingMarker {
                    change: name.clone(),
                    marker: marker.to_string(),
                });
                break;
            }
        }
    }
    out.sort_by(|a, b| a.change.cmp(&b.change));
    Ok(out)
}

/// List changes excluded from `list_pending` by an operator-action marker.
/// Returns `(perma_stuck_changes, revision_marked_changes)` — each sorted
/// ascending. Subdirectories that begin with `.` or are the literal
/// `archive` directory are skipped.
pub fn list_marker_excluded(workspace: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let root = changes_dir(workspace);
    if !root.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut perma: Vec<String> = Vec::new();
    let mut revision: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("reading {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if name == ARCHIVE_DIR || name.starts_with('.') {
            continue;
        }
        let dir = entry.path();
        if dir.join(PERMA_STUCK_FILE).exists() {
            perma.push(name.clone());
        }
        if dir.join(NEEDS_REVISION_FILE).exists() {
            revision.push(name);
        }
    }
    perma.sort();
    revision.sort();
    Ok((perma, revision))
}

pub fn lock(workspace: &Path, change: &str) -> Result<()> {
    let path = change_dir(workspace, change).join(LOCK_FILE);
    std::fs::File::create(&path)
        .with_context(|| format!("creating lock file {}", path.display()))?;
    Ok(())
}

/// Idempotent: returns Ok if the lock file is already absent.
pub fn unlock(workspace: &Path, change: &str) -> Result<()> {
    let path = change_dir(workspace, change).join(LOCK_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing lock file {}", path.display())),
    }
}

/// Iterate every direct subdirectory of `<workspace>/openspec/changes/`
/// (excluding `archive`), delete any `.in-progress` file, and emit a log line
/// per cleared lock naming the change. Returns the list of cleared change
/// names so callers (and tests) can observe what was reclaimed.
pub fn clear_stale_locks(workspace: &Path) -> Result<Vec<String>> {
    let root = changes_dir(workspace);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut cleared: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("reading {}", root.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if name == ARCHIVE_DIR || name.starts_with('.') {
            continue;
        }
        let lock_path = entry.path().join(LOCK_FILE);
        if lock_path.exists() {
            std::fs::remove_file(&lock_path)
                .with_context(|| format!("removing stale lock {}", lock_path.display()))?;
            tracing::info!("cleared stale .in-progress lock for change `{name}`");
            cleared.push(name);
        }
    }
    cleared.sort();
    Ok(cleared)
}

/// Compute the dated archive path that `archive(workspace, change)` would
/// attempt to create on today's UTC date. Returns
/// `<workspace>/openspec/changes/archive/<UTC-YYYY-MM-DD>-<change>/`. The
/// date format mirrors `archive` so the returned path is byte-identical to
/// what `archive` would build at the same instant.
pub fn archive_collision_path(workspace: &Path, change: &str) -> PathBuf {
    let archive_root = changes_dir(workspace).join(ARCHIVE_DIR);
    let dated_name = format!("{}-{change}", Utc::now().format("%Y-%m-%d"));
    archive_root.join(dated_name)
}

/// True when `archive_collision_path(workspace, change)` already exists on
/// disk — i.e. a subsequent `archive(workspace, change)` call would fail
/// with "archive destination already exists". Thin wrapper over the path
/// helper; the named function exists so the call site at the polling loop
/// is self-documenting (`if queue::would_collide_on_archive(ws, c) { ... }`).
pub fn would_collide_on_archive(workspace: &Path, change: &str) -> bool {
    archive_collision_path(workspace, change).exists()
}

/// Archive `<change>` by invoking `openspec archive <change> -y` via
/// the shared `openspec_archive_with_postcondition` helper. The
/// helper checks BOTH openspec's exit + stdout (for the `Aborted.`
/// marker openspec emits when it refuses to apply a delta but still
/// exits 0) AND the on-disk post-condition (active path moved,
/// archive entry produced). Returns `Err` with a single message that
/// names the failure variant and includes the openspec output excerpt,
/// so callers (notably the self-heal flow in `polling_loop.rs`) can
/// surface an actionable cause line instead of silently swallowing
/// the skip.
pub fn archive(workspace: &Path, change: &str) -> Result<()> {
    archive_with_runner(&RealArchiveRunner, workspace, change)
}

/// SpecRoot-aware archive entry point. Use this when the caller has a
/// `&RepositoryConfig` in scope so per-repo `spec_storage` is honored.
pub fn archive_at(spec_root: &crate::spec_root::SpecRoot, change: &str) -> Result<()> {
    archive_at_with_runner(&RealArchiveRunner, spec_root, change)
}

/// SpecRoot + injectable-runner variant of [`archive_at`]. Production
/// uses [`archive_at`] (which delegates here with a `RealArchiveRunner`);
/// tests substitute mock runners to drive the four `ArchiveFailure`
/// branches without spawning real subprocesses.
pub fn archive_at_with_runner(
    runner: &dyn ArchiveRunner,
    spec_root: &crate::spec_root::SpecRoot,
    change: &str,
) -> Result<()> {
    let src = spec_root.changes_dir().join(change);
    if !src.is_dir() {
        return Err(anyhow!(
            "cannot archive change `{change}`: source directory {} not found",
            src.display()
        ));
    }
    match openspec_archive_with_postcondition(runner, spec_root, change) {
        Ok(_archive_path) => Ok(()),
        Err(ArchiveFailure::NonZeroExit { code, stderr, stdout }) => {
            let body = if !stderr.trim().is_empty() {
                truncate_for_report(stderr.trim())
            } else if !stdout.trim().is_empty() {
                truncate_for_report(stdout.trim())
            } else {
                "(no output)".to_string()
            };
            Err(anyhow!(
                "openspec archive `{change}` exited {code:?}: {body}"
            ))
        }
        Err(ArchiveFailure::AbortedMarker { reason, full_output }) => Err(anyhow!(
            "openspec archive `{change}` aborted by openspec: {reason}; full output: {full_output}"
        )),
        Err(ArchiveFailure::ActivePathStillPresent { path, full_output: _ }) => Err(anyhow!(
            "openspec archive `{change}` reported success but the change directory at {} still exists",
            path.display()
        )),
        Err(ArchiveFailure::NoArchiveEntryFound { full_output }) => Err(anyhow!(
            "openspec archive `{change}` reported success but neither the active path nor any archive entry exists; full output: {full_output}"
        )),
    }
}

/// Test-injectable variant of `archive`. The production entry point
/// delegates with a `RealArchiveRunner`; tests substitute mock
/// runners to drive the four `ArchiveFailure` branches without
/// spawning real subprocesses.
pub fn archive_with_runner(
    runner: &dyn ArchiveRunner,
    workspace: &Path,
    change: &str,
) -> Result<()> {
    let src = change_dir(workspace, change);
    if !src.is_dir() {
        return Err(anyhow!(
            "cannot archive change `{change}`: source directory {} not found",
            src.display()
        ));
    }
    // `workspace` here is the dir containing openspec/ (which for non-
    // spec_storage repos is the code workspace; for spec_storage repos
    // is the spec_storage path). Wrap it in a `SpecRoot::from_parts`
    // shim so `openspec_archive_with_postcondition` operates on the
    // right tree.
    let spec_root = crate::spec_root::SpecRoot::from_parts(
        workspace.to_path_buf(),
        workspace.join("openspec"),
        false,
    );
    match openspec_archive_with_postcondition(runner, &spec_root, change) {
        Ok(_archive_path) => Ok(()),
        Err(ArchiveFailure::NonZeroExit { code, stderr, stdout }) => {
            let body = if !stderr.trim().is_empty() {
                truncate_for_report(stderr.trim())
            } else if !stdout.trim().is_empty() {
                truncate_for_report(stdout.trim())
            } else {
                "(no output)".to_string()
            };
            Err(anyhow!(
                "openspec archive `{change}` exited {code:?}: {body}"
            ))
        }
        Err(ArchiveFailure::AbortedMarker { reason, full_output }) => Err(anyhow!(
            "openspec archive `{change}` aborted by openspec: {reason}; full output: {full_output}"
        )),
        Err(ArchiveFailure::ActivePathStillPresent { path, full_output: _ }) => Err(anyhow!(
            "openspec archive `{change}` reported success but the change directory at {} still exists",
            path.display()
        )),
        Err(ArchiveFailure::NoArchiveEntryFound { full_output }) => Err(anyhow!(
            "openspec archive `{change}` reported success but neither the active path nor any archive entry exists; full output: {full_output}"
        )),
    }
}

/// Move the most-recently-archived directory matching
/// `^\d{4}-\d{2}-\d{2}-<change>$` back to
/// `<workspace>/openspec/changes/<change>/`. Errors if no match is found or
/// the destination already exists.
pub fn unarchive(workspace: &Path, change: &str) -> Result<()> {
    let archive_root = changes_dir(workspace).join(ARCHIVE_DIR);
    if !archive_root.is_dir() {
        return Err(anyhow!(
            "no archive directory at {}",
            archive_root.display()
        ));
    }
    let pattern = format!(r"^\d{{4}}-\d{{2}}-\d{{2}}-{}$", regex::escape(change));
    let regex = Regex::new(&pattern).expect("regex compiles");
    let mut candidates: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&archive_root)
        .with_context(|| format!("reading {}", archive_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string() {
            if regex.is_match(&name) {
                candidates.push(name);
            }
        }
    }
    if candidates.is_empty() {
        return Err(anyhow!(
            "no archived change matching `{change}` (looked for {pattern} in {})",
            archive_root.display()
        ));
    }
    candidates.sort(); // lex-highest = most recent date prefix
    let chosen = candidates.last().unwrap();
    let src = archive_root.join(chosen);
    let dst = change_dir(workspace, change);
    if dst.exists() {
        return Err(anyhow!(
            "unarchive destination already exists: {}",
            dst.display()
        ));
    }
    std::fs::rename(&src, &dst)
        .with_context(|| format!("renaming {} to {}", src.display(), dst.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build an `openspec/changes/<name>/proposal.md` fixture inside `dir`.
    fn make_change(workspace: &Path, name: &str) {
        let dir = workspace.join(CHANGES_SUBDIR).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PROPOSAL_FILE), "## Why\nfixture\n").unwrap();
    }

    fn make_change_no_proposal(workspace: &Path, name: &str) {
        let dir = workspace.join(CHANGES_SUBDIR).join(name);
        std::fs::create_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_pending_filters_correctly() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "01-feature-a");
        make_change(ws, "02-feature-b");
        // Excluded: dotfile-named.
        make_change_no_proposal(ws, ".hidden");
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(".hidden")).unwrap();
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join(".hidden").join(PROPOSAL_FILE),
            "x",
        )
        .unwrap();
        // Excluded: archive subdir.
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR).join("foo")).unwrap();
        // Excluded: missing proposal.md.
        make_change_no_proposal(ws, "no-proposal");
        // Excluded: locked.
        make_change(ws, "locked-one");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("locked-one").join(LOCK_FILE),
            "",
        )
        .unwrap();
        // Excluded: a regular file (not a directory).
        std::fs::write(ws.join(CHANGES_SUBDIR).join("regular.txt"), "x").unwrap();

        let listed = list_pending(ws).unwrap();
        assert_eq!(listed, vec!["01-feature-a", "02-feature-b"]);
    }

    #[test]
    fn list_pending_handles_missing_changes_dir() {
        let dir = TempDir::new().unwrap();
        let listed = list_pending(dir.path()).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn lock_unlock_round_trip() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "x");
        let lock_path = ws.join(CHANGES_SUBDIR).join("x").join(LOCK_FILE);
        assert!(!lock_path.exists());
        lock(ws, "x").unwrap();
        assert!(lock_path.exists());
        unlock(ws, "x").unwrap();
        assert!(!lock_path.exists());
        // Idempotent — second unlock is fine.
        unlock(ws, "x").unwrap();
    }

    #[test]
    fn list_pending_excludes_locked_change() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "x");
        make_change(ws, "y");
        lock(ws, "x").unwrap();
        let listed = list_pending(ws).unwrap();
        assert_eq!(listed, vec!["y"]);
    }

    #[test]
    fn clear_stale_locks_removes_in_progress_files() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "x");
        make_change(ws, "y");
        make_change(ws, "z");
        lock(ws, "x").unwrap();
        lock(ws, "z").unwrap();
        // y is unlocked; should be untouched.
        let cleared = clear_stale_locks(ws).unwrap();
        // Returned list names exactly the changes whose locks were cleared.
        assert_eq!(cleared, vec!["x".to_string(), "z".to_string()]);
        assert!(!ws.join(CHANGES_SUBDIR).join("x").join(LOCK_FILE).exists());
        assert!(!ws.join(CHANGES_SUBDIR).join("y").join(LOCK_FILE).exists());
        assert!(!ws.join(CHANGES_SUBDIR).join("z").join(LOCK_FILE).exists());
        // After cleanup, x and z are pending again.
        let listed = list_pending(ws).unwrap();
        assert_eq!(listed, vec!["x", "y", "z"]);
    }

    #[test]
    fn clear_stale_locks_returns_empty_when_none_present() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "a");
        make_change(ws, "b");
        let cleared = clear_stale_locks(ws).unwrap();
        assert!(cleared.is_empty(), "expected nothing to clear, got {cleared:?}");
    }

    #[test]
    fn archive_missing_source_errors_without_invoking_openspec() {
        // Pure input-validation check: if the active change directory
        // doesn't exist, `archive` returns Err before spawning openspec.
        // This unit-tests the only Err branch reachable without a real
        // openspec binary on PATH.
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let err = archive(ws, "never-existed").expect_err("missing source must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("never-existed"),
            "error must name the change: {msg}"
        );
        assert!(
            msg.contains("not found"),
            "error must explain why: {msg}"
        );
    }

    /// End-to-end archive via the real `openspec` binary. Requires
    /// openspec on PATH and the host's profile to have the `sync`
    /// workflow enabled for the canonical-spec merge to fire. Marked
    /// `#[ignore]` so `cargo test` skips it on hosts without openspec;
    /// run with `cargo test -- --ignored` to exercise.
    #[test]
    #[ignore]
    fn archive_round_trip_via_openspec_cli() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "feature-a");
        archive(ws, "feature-a").unwrap();
        // Source is gone.
        assert!(!ws.join(CHANGES_SUBDIR).join("feature-a").exists());
        // Archive directory contains exactly one entry matching the date prefix.
        let archive_root = ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR);
        let entries: Vec<_> = std::fs::read_dir(&archive_root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries.len(), 1);
        let name = &entries[0];
        let regex = Regex::new(r"^\d{4}-\d{2}-\d{2}-feature-a$").unwrap();
        assert!(
            regex.is_match(name),
            "archived name should be date-prefixed: {name}"
        );
    }

    #[test]
    fn unarchive_picks_lex_highest_match() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let archive_root = ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR);
        std::fs::create_dir_all(archive_root.join("2026-01-01-feature-a")).unwrap();
        std::fs::write(
            archive_root.join("2026-01-01-feature-a").join(PROPOSAL_FILE),
            "old",
        )
        .unwrap();
        std::fs::create_dir_all(archive_root.join("2026-05-04-feature-a")).unwrap();
        std::fs::write(
            archive_root.join("2026-05-04-feature-a").join(PROPOSAL_FILE),
            "new",
        )
        .unwrap();

        unarchive(ws, "feature-a").unwrap();

        // The newer one moved back to active queue.
        let restored = ws.join(CHANGES_SUBDIR).join("feature-a").join(PROPOSAL_FILE);
        let contents = std::fs::read_to_string(&restored).unwrap();
        assert_eq!(contents, "new");
        // The older one stays in the archive.
        assert!(archive_root.join("2026-01-01-feature-a").exists());
    }

    #[test]
    fn unarchive_missing_errors() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR)).unwrap();
        let err = unarchive(ws, "never-existed").expect_err("should error on no match");
        let msg = format!("{err:#}");
        assert!(msg.contains("never-existed"), "got: {msg}");
    }

    #[test]
    fn pending_excludes_waiting() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "ready");
        make_change(ws, "waiting");
        // The `waiting` change has a `.question.json` marker.
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("waiting").join(QUESTION_FILE),
            r#"{"thread_ts":"x","channel":"C","resume_handle":null,"asked_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let pending = list_pending(ws).unwrap();
        assert_eq!(pending, vec!["ready"], "waiting change must be excluded from pending");
    }

    #[test]
    fn list_waiting_returns_questioned() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "ready");
        make_change(ws, "wait-a");
        make_change(ws, "wait-b");
        for name in &["wait-a", "wait-b"] {
            std::fs::write(
                ws.join(CHANGES_SUBDIR).join(name).join(QUESTION_FILE),
                r#"{"thread_ts":"x","channel":"C","resume_handle":null,"asked_at":"2026-01-01T00:00:00Z"}"#,
            )
            .unwrap();
        }
        let waiting = list_waiting(ws).unwrap();
        assert_eq!(waiting, vec!["wait-a".to_string(), "wait-b".to_string()]);
        // Sorted ascending.
    }

    #[test]
    fn list_waiting_excludes_archive() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Place a fake `.question.json` inside an archived directory entry;
        // it must NOT be returned by list_waiting.
        let archive_entry = ws
            .join(CHANGES_SUBDIR)
            .join(ARCHIVE_DIR)
            .join("2026-01-01-historic");
        std::fs::create_dir_all(&archive_entry).unwrap();
        std::fs::write(archive_entry.join(QUESTION_FILE), "{}").unwrap();
        // Also place a dotfile-prefixed directory that would otherwise match.
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(".hidden")).unwrap();
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join(".hidden").join(QUESTION_FILE),
            "{}",
        )
        .unwrap();
        // A real waiting entry.
        make_change(ws, "real-wait");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("real-wait").join(QUESTION_FILE),
            "{}",
        )
        .unwrap();

        let waiting = list_waiting(ws).unwrap();
        assert_eq!(waiting, vec!["real-wait".to_string()]);
    }

    #[test]
    fn list_pending_excludes_perma_stuck() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "alpha");
        make_change(ws, "beta");
        make_change(ws, "gamma");
        // Mark `beta` as perma-stuck.
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("beta").join(PERMA_STUCK_FILE),
            r#"{"change":"beta","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"Delete this file to retry the change."}"#,
        )
        .unwrap();

        let pending = list_pending(ws).unwrap();
        assert_eq!(
            pending,
            vec!["alpha".to_string(), "gamma".to_string()],
            "perma-stuck change must be excluded from list_pending"
        );
    }

    #[test]
    fn list_pending_excludes_needs_spec_revision() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "alpha");
        make_change(ws, "beta");
        make_change(ws, "gamma");
        // Mark `beta` as needing spec revision.
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("beta").join(NEEDS_REVISION_FILE),
            r#"{"change":"beta","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"Edit tasks.md, commit, then delete this marker."}"#,
        )
        .unwrap();

        let pending = list_pending(ws).unwrap();
        assert_eq!(
            pending,
            vec!["alpha".to_string(), "gamma".to_string()],
            "needs-spec-revision change must be excluded from list_pending"
        );
        assert!(is_needs_spec_revision_marked(ws, "beta"));
        assert!(!is_needs_spec_revision_marked(ws, "alpha"));
    }

    #[test]
    fn would_collide_on_archive_detects_dated_entry() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let today = Utc::now().format("%Y-%m-%d").to_string();

        // Active dir present, but no archive entry yet → no collision.
        make_change(ws, "foo");
        assert!(
            !would_collide_on_archive(ws, "foo"),
            "no collision when only the active dir exists"
        );

        // Pre-create the dated archive entry for today.
        let dated = format!("{today}-foo");
        let archived = ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR).join(&dated);
        std::fs::create_dir_all(&archived).unwrap();
        assert!(
            would_collide_on_archive(ws, "foo"),
            "collision must be detected when active dir AND dated archive entry both exist"
        );

        // The returned path matches exactly what `archive()` would build.
        assert_eq!(archive_collision_path(ws, "foo"), archived);

        // Remove the active dir — the dated archive entry alone is the
        // legitimate post-archive state, not a collision (the change is
        // not in `list_pending` either). The helper still reports `true`
        // because the path is purely a filesystem check; the caller (the
        // polling loop) guards entry with `list_pending`, which excludes
        // changes that have no active dir. Verify the wrapping behavior:
        // with no active dir, list_pending returns empty.
        std::fs::remove_dir_all(ws.join(CHANGES_SUBDIR).join("foo")).unwrap();
        assert!(
            list_pending(ws).unwrap().is_empty(),
            "with only the archive entry, list_pending must not return the change"
        );

        // Fresh workspace where only the archive entry exists for a
        // different change name → the helper for THAT change returns
        // true (filesystem-pure), but list_pending excludes it too.
        let dir2 = TempDir::new().unwrap();
        let ws2 = dir2.path();
        let only_archive = ws2.join(CHANGES_SUBDIR).join(ARCHIVE_DIR).join(format!("{today}-bar"));
        std::fs::create_dir_all(&only_archive).unwrap();
        assert!(would_collide_on_archive(ws2, "bar"));
        assert!(list_pending(ws2).unwrap().is_empty());

        // And on a workspace with NO archive entry, the helper returns false.
        let dir3 = TempDir::new().unwrap();
        let ws3 = dir3.path();
        make_change(ws3, "baz");
        assert!(!would_collide_on_archive(ws3, "baz"));
    }

    #[test]
    fn remove_perma_stuck_marker_returns_ok_and_clears_file() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "alpha");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("alpha").join(PERMA_STUCK_FILE),
            r#"{"change":"alpha","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();
        assert!(is_perma_stuck(ws, "alpha"));
        remove_perma_stuck_marker(ws, "alpha").unwrap();
        assert!(!is_perma_stuck(ws, "alpha"));
    }

    #[test]
    fn remove_perma_stuck_marker_errors_when_absent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "alpha");
        let err =
            remove_perma_stuck_marker(ws, "alpha").expect_err("missing marker must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no perma-stuck marker for change `alpha`"),
            "error must name change: {msg}"
        );
    }

    #[test]
    fn remove_revision_marker_returns_ok_and_clears_file() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "beta");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("beta").join(NEEDS_REVISION_FILE),
            r#"{"change":"beta","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"x"}"#,
        )
        .unwrap();
        assert!(is_needs_spec_revision_marked(ws, "beta"));
        remove_revision_marker(ws, "beta").unwrap();
        assert!(!is_needs_spec_revision_marked(ws, "beta"));
    }

    #[test]
    fn remove_revision_marker_errors_when_absent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "beta");
        let err =
            remove_revision_marker(ws, "beta").expect_err("missing marker must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no needs-spec-revision marker for change `beta`"),
            "error must name change: {msg}"
        );
    }

    #[test]
    fn find_queue_blocking_markers_empty_workspace_returns_empty() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let blocking = find_queue_blocking_markers(ws).unwrap();
        assert!(blocking.is_empty());
    }

    #[test]
    fn find_queue_blocking_markers_finds_perma_stuck() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "ready");
        make_change(ws, "broken");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("broken").join(PERMA_STUCK_FILE),
            r#"{"change":"broken","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();

        let blocking = find_queue_blocking_markers(ws).unwrap();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].change, "broken");
        assert_eq!(blocking[0].marker, PERMA_STUCK_FILE);
    }

    #[test]
    fn find_queue_blocking_markers_finds_needs_spec_revision() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "ready");
        make_change(ws, "broken");
        std::fs::write(
            ws.join(CHANGES_SUBDIR)
                .join("broken")
                .join(NEEDS_REVISION_FILE),
            r#"{"change":"broken","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"x"}"#,
        )
        .unwrap();

        let blocking = find_queue_blocking_markers(ws).unwrap();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].change, "broken");
        assert_eq!(blocking[0].marker, NEEDS_REVISION_FILE);
    }

    #[test]
    fn find_queue_blocking_markers_finds_question_waiting() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "ready");
        make_change(ws, "waiting");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("waiting").join(QUESTION_FILE),
            r#"{"thread_ts":"x","channel":"C","resume_handle":null,"asked_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let blocking = find_queue_blocking_markers(ws).unwrap();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].change, "waiting");
        assert_eq!(blocking[0].marker, QUESTION_FILE);
    }

    #[test]
    fn find_queue_blocking_markers_downgraded_by_ignore_for_queue() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "ready");
        make_change(ws, "broken-ignored");
        // Perma-stuck marker present.
        std::fs::write(
            ws.join(CHANGES_SUBDIR)
                .join("broken-ignored")
                .join(PERMA_STUCK_FILE),
            r#"{"change":"broken-ignored","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();
        // AND the ignore-for-queue downgrade marker.
        std::fs::write(
            ws.join(CHANGES_SUBDIR)
                .join("broken-ignored")
                .join(IGNORE_FOR_QUEUE_FILE),
            r#"{"change":"broken-ignored","marked_at":"2026-01-01T00:00:00Z","marked_by":"U_OP","reason":"x","operator_action":"x"}"#,
        )
        .unwrap();

        let blocking = find_queue_blocking_markers(ws).unwrap();
        assert!(
            blocking.is_empty(),
            "ignore-for-queue must downgrade blocking effect; got: {blocking:?}"
        );
    }

    #[test]
    fn find_queue_blocking_markers_multiple_changes_sorted() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "zeta-broken");
        make_change(ws, "alpha-waiting");
        std::fs::write(
            ws.join(CHANGES_SUBDIR)
                .join("zeta-broken")
                .join(PERMA_STUCK_FILE),
            r#"{"change":"zeta-broken","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();
        std::fs::write(
            ws.join(CHANGES_SUBDIR)
                .join("alpha-waiting")
                .join(QUESTION_FILE),
            r#"{"thread_ts":"x","channel":"C","resume_handle":null,"asked_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let blocking = find_queue_blocking_markers(ws).unwrap();
        assert_eq!(blocking.len(), 2);
        assert_eq!(blocking[0].change, "alpha-waiting");
        assert_eq!(blocking[1].change, "zeta-broken");
    }

    #[test]
    fn is_ignore_for_queue_marked_round_trip() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "foo");
        assert!(!is_ignore_for_queue_marked(ws, "foo"));
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("foo").join(IGNORE_FOR_QUEUE_FILE),
            r#"{"change":"foo","marked_at":"2026-01-01T00:00:00Z","marked_by":"U","reason":"x","operator_action":"x"}"#,
        )
        .unwrap();
        assert!(is_ignore_for_queue_marked(ws, "foo"));
    }

    #[test]
    fn remove_ignore_for_queue_marker_errors_when_absent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "foo");
        let err = remove_ignore_for_queue_marker(ws, "foo")
            .expect_err("must error when marker absent");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no ignore-for-queue marker for change `foo`"),
            "error must name change: {msg}"
        );
    }

    #[test]
    fn remove_ignore_for_queue_marker_clears_file() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "foo");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("foo").join(IGNORE_FOR_QUEUE_FILE),
            r#"{"change":"foo","marked_at":"2026-01-01T00:00:00Z","marked_by":"U","reason":"x","operator_action":"x"}"#,
        )
        .unwrap();
        assert!(is_ignore_for_queue_marked(ws, "foo"));
        remove_ignore_for_queue_marker(ws, "foo").unwrap();
        assert!(!is_ignore_for_queue_marked(ws, "foo"));
    }

    #[test]
    fn remove_ignore_for_queue_marker_idempotent_returns_false_when_absent() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "foo");
        let removed = remove_ignore_for_queue_marker_idempotent(ws, "foo").unwrap();
        assert!(!removed);
    }

    #[test]
    fn remove_ignore_for_queue_marker_idempotent_returns_true_when_present() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "foo");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("foo").join(IGNORE_FOR_QUEUE_FILE),
            r#"{"change":"foo","marked_at":"2026-01-01T00:00:00Z","marked_by":"U","reason":"x","operator_action":"x"}"#,
        )
        .unwrap();
        let removed = remove_ignore_for_queue_marker_idempotent(ws, "foo").unwrap();
        assert!(removed);
        assert!(!is_ignore_for_queue_marked(ws, "foo"));
    }

    #[test]
    fn list_marker_excluded_groups_by_marker_type() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "ready");
        make_change(ws, "alpha");
        make_change(ws, "beta");
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("alpha").join(PERMA_STUCK_FILE),
            r#"{"change":"alpha","consecutive_failures":2,"last_reason":"x","marked_stuck_at":"2026-01-01T00:00:00Z","operator_action":"x"}"#,
        )
        .unwrap();
        std::fs::write(
            ws.join(CHANGES_SUBDIR).join("beta").join(NEEDS_REVISION_FILE),
            r#"{"change":"beta","marked_at":"2026-01-01T00:00:00Z","unimplementable_tasks":[],"revision_suggestion":"x","operator_action":"x"}"#,
        )
        .unwrap();

        let (perma, revision) = list_marker_excluded(ws).unwrap();
        assert_eq!(perma, vec!["alpha".to_string()]);
        assert_eq!(revision, vec!["beta".to_string()]);
    }

    #[test]
    fn unarchive_destination_collision_errors() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let archive_root = ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR);
        std::fs::create_dir_all(archive_root.join("2026-05-04-x")).unwrap();
        std::fs::write(archive_root.join("2026-05-04-x").join(PROPOSAL_FILE), "x").unwrap();
        // Pre-create the active destination.
        make_change(ws, "x");

        let err = unarchive(ws, "x").expect_err("collision should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "got: {msg}");
    }

    // ---- archive_with_runner: structured-failure mapping tests ----

    use crate::openspec_archive::{ArchiveRunOutput, ArchiveRunner};

    fn fake_exit(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    /// Runner stub: succeeds and moves the change dir into the
    /// dated archive entry.
    struct SuccessRunner;
    impl ArchiveRunner for SuccessRunner {
        fn run(&self, workspace: &Path, slug: &str) -> Result<ArchiveRunOutput, String> {
            let from = workspace.join(CHANGES_SUBDIR).join(slug);
            let today = Utc::now().format("%Y-%m-%d").to_string();
            let to = workspace
                .join(CHANGES_SUBDIR)
                .join(ARCHIVE_DIR)
                .join(format!("{today}-{slug}"));
            std::fs::rename(&from, &to)
                .map_err(|e| format!("test rename failed: {e}"))?;
            Ok(ArchiveRunOutput {
                status: fake_exit(0),
                stdout: format!("archived {slug}\n"),
                stderr: String::new(),
            })
        }
    }

    /// Runner stub: exit 0, emits `Aborted.` marker, performs no fs work.
    struct AbortedRunner;
    impl ArchiveRunner for AbortedRunner {
        fn run(&self, _workspace: &Path, slug: &str) -> Result<ArchiveRunOutput, String> {
            Ok(ArchiveRunOutput {
                status: fake_exit(0),
                stdout: format!(
                    "{slug} MODIFIED failed for header \"### Requirement: X\" - not found\nAborted. No files were changed.\n"
                ),
                stderr: String::new(),
            })
        }
    }

    /// Runner stub: exit non-zero with stderr.
    struct FailingRunner;
    impl ArchiveRunner for FailingRunner {
        fn run(&self, _workspace: &Path, slug: &str) -> Result<ArchiveRunOutput, String> {
            Ok(ArchiveRunOutput {
                status: fake_exit(1),
                stdout: String::new(),
                stderr: format!("openspec validation error for {slug}\n"),
            })
        }
    }

    /// Runner stub: exit 0, benign stdout, performs no fs work
    /// (silent-skip without the marker).
    struct SilentSkipRunner;
    impl ArchiveRunner for SilentSkipRunner {
        fn run(&self, _workspace: &Path, slug: &str) -> Result<ArchiveRunOutput, String> {
            Ok(ArchiveRunOutput {
                status: fake_exit(0),
                stdout: format!("would archive {slug}\n"),
                stderr: String::new(),
            })
        }
    }

    /// Runner stub: exit 0, removes the change dir but produces no
    /// archive entry (data-loss case).
    struct DataLossRunner;
    impl ArchiveRunner for DataLossRunner {
        fn run(&self, workspace: &Path, slug: &str) -> Result<ArchiveRunOutput, String> {
            let from = workspace.join(CHANGES_SUBDIR).join(slug);
            std::fs::remove_dir_all(&from)
                .map_err(|e| format!("test removal failed: {e}"))?;
            Ok(ArchiveRunOutput {
                status: fake_exit(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn archive_with_runner_happy_path_returns_ok() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR)).unwrap();
        make_change(ws, "foo");
        archive_with_runner(&SuccessRunner, ws, "foo").unwrap();
        // Source gone; archive entry under today's date.
        assert!(!ws.join(CHANGES_SUBDIR).join("foo").exists());
        let today = Utc::now().format("%Y-%m-%d").to_string();
        assert!(
            ws.join(CHANGES_SUBDIR)
                .join(ARCHIVE_DIR)
                .join(format!("{today}-foo"))
                .is_dir()
        );
    }

    #[test]
    fn archive_with_runner_aborted_marker_surfaces_actionable_error() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR)).unwrap();
        make_change(ws, "foo");
        let err = archive_with_runner(&AbortedRunner, ws, "foo")
            .expect_err("aborted marker must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            msg.starts_with("openspec archive `foo` aborted by openspec:"),
            "error must name the variant and the change: {msg}"
        );
        assert!(
            msg.contains("MODIFIED failed for header"),
            "error must include the openspec-supplied cause line: {msg}"
        );
        // Source directory left in place for operator to investigate.
        assert!(ws.join(CHANGES_SUBDIR).join("foo").is_dir());
    }

    #[test]
    fn archive_with_runner_non_zero_exit_includes_stderr() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR)).unwrap();
        make_change(ws, "foo");
        let err = archive_with_runner(&FailingRunner, ws, "foo")
            .expect_err("non-zero exit must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            msg.starts_with("openspec archive `foo` exited"),
            "error must name the variant: {msg}"
        );
        assert!(
            msg.contains("validation error"),
            "error must include the openspec stderr: {msg}"
        );
    }

    #[test]
    fn archive_with_runner_silent_skip_surfaces_active_path_message() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR)).unwrap();
        make_change(ws, "foo");
        let err = archive_with_runner(&SilentSkipRunner, ws, "foo")
            .expect_err("silent skip must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reported success but the change directory at"),
            "error must name the active path remaining: {msg}"
        );
    }

    #[test]
    fn archive_with_runner_data_loss_surfaces_no_archive_entry_message() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR)).unwrap();
        make_change(ws, "foo");
        let err = archive_with_runner(&DataLossRunner, ws, "foo")
            .expect_err("data-loss must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("neither the active path nor any archive entry exists"),
            "error must name the data-loss condition explicitly: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // a27a1: iteration-pending two-tier ordering tests
    // -----------------------------------------------------------------

    fn write_iteration_marker(workspace: &Path, change: &str, iteration_number: u32) {
        let marker = crate::iteration_pending::IterationPendingMarker {
            completed_tasks: vec!["1".into()],
            remaining_tasks: vec!["2".into()],
            reason: "fixture".into(),
            iteration_number,
        };
        crate::iteration_pending::write_marker(workspace, change, &marker).unwrap();
    }

    /// Task 5.3: unmarked vs. marked — the iteration-pending entry
    /// comes first despite alphabetical disadvantage.
    #[test]
    fn list_pending_iteration_marker_preempts_alphabetical_order() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "a30-foo");
        make_change(ws, "a31-bar");
        write_iteration_marker(ws, "a31-bar", 2);
        let listed = list_pending(ws).unwrap();
        assert_eq!(
            listed,
            vec!["a31-bar".to_string(), "a30-foo".to_string()],
            "iteration-pending entry must come first"
        );
    }

    /// Task 5.4: among marked, lower iteration_number sorts first.
    #[test]
    fn list_pending_marked_tier_sorts_by_iteration_number_ascending() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "a30-foo");
        make_change(ws, "a31-bar");
        write_iteration_marker(ws, "a30-foo", 3);
        write_iteration_marker(ws, "a31-bar", 2);
        let listed = list_pending(ws).unwrap();
        assert_eq!(
            listed,
            vec!["a31-bar".to_string(), "a30-foo".to_string()],
            "lower iteration_number must sort first within the marked tier"
        );
    }

    /// Task 5.5: the existing alphabetical-among-unmarked behavior is
    /// preserved (regression test against today's enumeration).
    #[test]
    fn list_pending_unmarked_tier_alphabetical_unchanged() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "c-third");
        make_change(ws, "a-first");
        make_change(ws, "b-second");
        let listed = list_pending(ws).unwrap();
        assert_eq!(
            listed,
            vec![
                "a-first".to_string(),
                "b-second".to_string(),
                "c-third".to_string(),
            ]
        );
    }

    /// Task 5.6: a corrupt marker (unparseable JSON) is treated as
    /// "iteration_number: 0" for ordering. The corrupt marker does NOT
    /// cause `list_pending` to error.
    #[test]
    fn list_pending_corrupt_marker_treated_as_iteration_zero_and_does_not_error() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "a30-corrupt");
        make_change(ws, "a31-valid");
        // Inject a corrupt marker for a30-corrupt.
        std::fs::write(
            ws.join(CHANGES_SUBDIR)
                .join("a30-corrupt")
                .join(crate::iteration_pending::MARKER_FILE),
            "{ truncated json",
        )
        .unwrap();
        // Valid marker for a31-valid with iteration_number 2.
        write_iteration_marker(ws, "a31-valid", 2);
        let listed = list_pending(ws).unwrap();
        // Both are marked; corrupt's iteration_number-for-ordering is 0
        // (sorts ahead of the valid 2-iteration entry).
        assert_eq!(
            listed,
            vec!["a30-corrupt".to_string(), "a31-valid".to_string()],
        );
    }

    /// The marker is NOT an exclusion: an iteration-pending entry IS
    /// returned in the pending list (not excluded), AND the existing
    /// `.question.json` / `.perma-stuck.json` exclusion behavior is
    /// unchanged for entries with those markers.
    #[test]
    fn list_pending_iteration_marker_is_not_an_exclusion() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "a30-ready");
        make_change(ws, "a31-iteration");
        write_iteration_marker(ws, "a31-iteration", 2);
        let listed = list_pending(ws).unwrap();
        assert!(
            listed.contains(&"a31-iteration".to_string()),
            "iteration-pending change must be returned in the pending list"
        );
        assert!(listed.contains(&"a30-ready".to_string()));
    }

    /// Self-heal integration contract: when `queue::archive` returns
    /// an abort-marker error, the self-heal flow in `polling_loop.rs`
    /// wraps it with `format!("self-heal archive failed: {e:#}")`. The
    /// resulting `QueueStep::Failed { reason }`'s reason must contain
    /// BOTH `self-heal archive failed` AND the openspec-supplied
    /// cause line, so operators reading the perma-stuck alert can act
    /// on the actual failure. This test verifies that composition.
    #[test]
    fn self_heal_failure_reason_includes_openspec_cause_via_archive_err() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR)).unwrap();
        make_change(ws, "broken-delta");

        let err = archive_with_runner(&AbortedRunner, ws, "broken-delta")
            .expect_err("aborted runner must produce Err");
        // Reproduce the exact format string the self-heal block in
        // polling_loop.rs uses when queue::archive returns Err.
        let reason = format!("self-heal archive failed: {err:#}");

        assert!(
            reason.contains("self-heal archive failed"),
            "reason must include the self-heal prefix: {reason}"
        );
        assert!(
            reason.contains("aborted by openspec:"),
            "reason must name the failure variant: {reason}"
        );
        assert!(
            reason.contains("MODIFIED failed for header"),
            "reason must include the openspec-supplied cause line: {reason}"
        );
    }
}
