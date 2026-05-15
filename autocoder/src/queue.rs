//! OpenSpec queue engine — enumerate, lock, archive, and unarchive changes
//! against a workspace.
//!
//! All functions operate on a `workspace` path that contains an
//! `openspec/changes/` directory. The filesystem is the source of truth.

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
/// file. Returns sorted ascending.
pub fn list_pending(workspace: &Path) -> Result<Vec<String>> {
    let root = changes_dir(workspace);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
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
        if dir.join(PERMA_STUCK_FILE).exists() {
            // Perma-stuck: the change has hit the consecutive-failure
            // threshold and autocoder will not retry until the operator
            // removes the marker file.
            continue;
        }
        if !dir.join(PROPOSAL_FILE).is_file() {
            continue;
        }
        out.push(name);
    }
    out.sort();
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

/// Move `<workspace>/openspec/changes/<change>/` to
/// `<workspace>/openspec/changes/archive/<UTC YYYY-MM-DD>-<change>/`.
/// Errors if the destination already exists.
pub fn archive(workspace: &Path, change: &str) -> Result<()> {
    let src = change_dir(workspace, change);
    if !src.is_dir() {
        return Err(anyhow!(
            "cannot archive change `{change}`: source directory {} not found",
            src.display()
        ));
    }
    let archive_root = changes_dir(workspace).join(ARCHIVE_DIR);
    std::fs::create_dir_all(&archive_root)
        .with_context(|| format!("creating archive dir {}", archive_root.display()))?;
    let dated_name = format!("{}-{change}", Utc::now().format("%Y-%m-%d"));
    let dst = archive_root.join(&dated_name);
    if dst.exists() {
        return Err(anyhow!(
            "archive destination already exists: {}",
            dst.display()
        ));
    }
    std::fs::rename(&src, &dst)
        .with_context(|| format!("renaming {} to {}", src.display(), dst.display()))?;
    Ok(())
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
    fn archive_round_trip() {
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
    fn archive_collision_errors() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        make_change(ws, "feature-a");
        // Pre-create a collision in the archive.
        let dated = format!("{}-feature-a", Utc::now().format("%Y-%m-%d"));
        let pre = ws.join(CHANGES_SUBDIR).join(ARCHIVE_DIR).join(&dated);
        std::fs::create_dir_all(&pre).unwrap();
        let err = archive(ws, "feature-a").expect_err("collision should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "got: {msg}");
        // Source must still be in place (not deleted).
        assert!(ws.join(CHANGES_SUBDIR).join("feature-a").exists());
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
}
