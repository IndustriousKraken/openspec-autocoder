//! Scout-run state IO for the `scout` chatops verb (a25).
//!
//! When an operator posts `@<bot> scout <repo> [guidance]`, the chatops
//! dispatcher submits a `ScoutAction` over the control socket; the
//! polling-iteration's scout handler invokes the executor in scout mode,
//! parses the resulting opportunity-item list, AND writes a
//! `ScoutRunState` file under the workspace so a later `@<bot> spec-it`
//! reply can resolve item ids back to their full descriptions.
//!
//! State files live at
//! `<workspace>/.state/scout_runs/<request_id>.json`. The "current"
//! scout for a repo is the most-recent file by mtime — older runs
//! remain on disk for audit purposes; operators can wipe them via
//! `@<bot> clear-scout <repo>`.
//!
//! Writes are atomic (tempfile-then-rename) so a torn write is never
//! visible to a concurrent reader.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Cap on the operator's free-form guidance text. Mirrors the
/// `brownfield` AND `propose` caps so operators learn one limit.
#[allow(dead_code)]
pub const GUIDANCE_CAP: usize = 10_000;

/// Categories the scout LLM is permitted to assign to opportunity items.
/// Kept here (rather than at the call site) so the polling handler AND
/// any future consumer reference the same canonical set.
pub const ALLOWED_CATEGORIES: &[&str] = &[
    "security",
    "bug",
    "error_handling",
    "type_tightening",
    "code_smell",
    "perf",
    "documentation",
    "test_coverage",
    "issue",
    "todo_fixme",
    "research",
];

/// Tractability values the scout LLM is permitted to assign. Mirrors
/// `ALLOWED_CATEGORIES` shape.
pub const ALLOWED_TRACTABILITY: &[&str] = &["small", "medium", "large"];

/// One opportunity item produced by a scout run. Field shapes match
/// the documented JSON-array shape the scout prompt instructs the LLM
/// to produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoutItem {
    pub id: usize,
    pub category: String,
    pub title: String,
    pub body: String,
    pub source: String,
    pub tractability: String,
}

/// Persisted state for one scout run. Lives on disk so later
/// `spec-it`/`clear-scout` invocations can resolve item ids without
/// re-running the executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoutRunState {
    pub request_id: String,
    pub repo_url: String,
    /// Optional operator-supplied guidance. Trimmed AND capped at
    /// `GUIDANCE_CAP` by the chatops parser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    /// Workspace HEAD SHA at the moment the scout completed. Compared
    /// against the current HEAD on `spec-it` to detect drift; mismatch
    /// triggers the staleness warning.
    pub head_sha_at_run: String,
    pub completed_at: DateTime<Utc>,
    pub channel: String,
    /// Bot's ack-message ts; the request's lifecycle thread anchor.
    pub thread_ts: String,
    pub items: Vec<ScoutItem>,
}

/// Per-workspace state directory: `<workspace>/.state/scout_runs/`.
pub fn state_dir(workspace: &Path) -> PathBuf {
    workspace.join(".state").join("scout_runs")
}

/// Canonical state-file path:
/// `<workspace>/.state/scout_runs/<request_id>.json`.
pub fn state_path(workspace: &Path, request_id: &str) -> PathBuf {
    state_dir(workspace).join(format!("{request_id}.json"))
}

/// Atomically write `state` to its canonical file. Parent directories
/// are created if absent.
pub fn write_state(workspace: &Path, state: &ScoutRunState) -> Result<()> {
    let dir = state_dir(workspace);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating scout_runs dir {}", dir.display()))?;
    let path = state_path(workspace, &state.request_id);
    let tmp = tempfile::NamedTempFile::new_in(&dir)
        .with_context(|| format!("creating tempfile in {}", dir.display()))?;
    serde_json::to_writer_pretty(&tmp, state)
        .with_context(|| format!("serializing scout-run state for {}", path.display()))?;
    tmp.persist(&path)
        .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
    Ok(())
}

/// Read the scout-run state for `request_id`. Returns `Ok(None)` when
/// no file exists; propagates IO/parse errors.
pub fn read_state(workspace: &Path, request_id: &str) -> Result<Option<ScoutRunState>> {
    let path = state_path(workspace, request_id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("reading {}: {e}", path.display())),
    };
    serde_json::from_str::<ScoutRunState>(&raw)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

/// Find the most-recently-modified scout-run state file in the
/// workspace's `scout_runs/` directory. Returns `Ok(None)` when no
/// `.json` files exist (or the directory is absent). Files that fail
/// to stat are skipped with a WARN log.
///
/// Exposed for tests AND future callers (the "current scout for a
/// repo is the most-recent file by mtime" rule from the a25 spec).
/// Not yet consumed by production code paths.
#[allow(dead_code)]
pub fn latest_state(workspace: &Path) -> Result<Option<ScoutRunState>> {
    let dir = state_dir(workspace);
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("reading scout_runs dir {}", dir.display()))?
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("scout_runs: skipping unreadable entry: {e}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    "scout_runs: skipping file with unreadable mtime: {e}"
                );
                continue;
            }
        };
        match &best {
            None => best = Some((mtime, path.clone())),
            Some((b, _)) if mtime > *b => best = Some((mtime, path.clone())),
            Some(_) => {}
        }
    }
    match best {
        None => Ok(None),
        Some((_, path)) => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading latest scout-run state {}", path.display()))?;
            let state: ScoutRunState = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(state))
        }
    }
}

/// Enumerate every scout-run state file under the workspace AND return
/// the (request_id, thread_ts) pairs. Used by the chatops listener to
/// decide whether an incoming `spec-it` reply is in scope for a known
/// scout lifecycle thread.
pub fn list_thread_anchors(workspace: &Path) -> Result<Vec<(String, String)>> {
    let dir = state_dir(workspace);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("reading scout_runs dir {}", dir.display()))?
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("scout_runs: list_thread_anchors: skipping entry: {e}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    "scout_runs: list_thread_anchors: unreadable: {e}"
                );
                continue;
            }
        };
        let state: ScoutRunState = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    "scout_runs: list_thread_anchors: unparseable: {e}"
                );
                continue;
            }
        };
        out.push((state.request_id, state.thread_ts));
    }
    Ok(out)
}

/// Remove every scout-run state file under the workspace. Returns the
/// number of files removed. Missing directory is treated as zero.
pub fn clear_all(workspace: &Path) -> Result<usize> {
    let dir = state_dir(workspace);
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("reading scout_runs dir {}", dir.display()))?
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("scout_runs clear_all: skipping unreadable entry: {e}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(
                path = %path.display(),
                "scout_runs clear_all: remove failed: {e}"
            ),
        }
    }
    Ok(removed)
}

/// Validate every item in a parsed-from-JSON list. Returns `Ok(())` on
/// success; surfaces a one-line description of the first violation
/// otherwise. The polling handler bubbles the error up into the thread
/// reply naming the validation failure.
pub fn validate_items(items: &[ScoutItem], max_items: usize) -> Result<()> {
    if items.len() > max_items {
        return Err(anyhow!(
            "item count {} exceeds features.scout.max_items={max_items}",
            items.len()
        ));
    }
    for (idx, item) in items.iter().enumerate() {
        if item.title.trim().is_empty() {
            return Err(anyhow!("item #{} has empty title", idx + 1));
        }
        if item.body.trim().is_empty() {
            return Err(anyhow!("item #{} has empty body", idx + 1));
        }
        if item.source.trim().is_empty() {
            return Err(anyhow!("item #{} has empty source", idx + 1));
        }
        if !ALLOWED_CATEGORIES.contains(&item.category.as_str()) {
            return Err(anyhow!(
                "item #{} has unknown category `{}` (allowed: {:?})",
                idx + 1,
                item.category,
                ALLOWED_CATEGORIES,
            ));
        }
        if !ALLOWED_TRACTABILITY.contains(&item.tractability.as_str()) {
            return Err(anyhow!(
                "item #{} has unknown tractability `{}` (allowed: {:?})",
                idx + 1,
                item.tractability,
                ALLOWED_TRACTABILITY,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture_item(id: usize, category: &str, tractability: &str) -> ScoutItem {
        ScoutItem {
            id,
            category: category.to_string(),
            title: format!("item {id} title"),
            body: format!("item {id} body — what AND why"),
            source: format!("src/lib.rs:{id}"),
            tractability: tractability.to_string(),
        }
    }

    fn fixture_state(request_id: &str) -> ScoutRunState {
        ScoutRunState {
            request_id: request_id.to_string(),
            repo_url: "git@github.com:acme/myrepo.git".to_string(),
            guidance: None,
            head_sha_at_run: "abc1234".to_string(),
            completed_at: Utc::now(),
            channel: "C_OPS".to_string(),
            thread_ts: "1748399999.001234".to_string(),
            items: vec![
                fixture_item(1, "bug", "small"),
                fixture_item(2, "security", "medium"),
            ],
        }
    }

    #[test]
    fn write_then_read_round_trips_every_field() {
        let tmp = TempDir::new().unwrap();
        let mut state = fixture_state("req-1");
        state.guidance = Some("focus on error handling".into());
        write_state(tmp.path(), &state).unwrap();
        let got = read_state(tmp.path(), &state.request_id).unwrap().unwrap();
        assert_eq!(got, state);
    }

    #[test]
    fn read_missing_returns_none() {
        let tmp = TempDir::new().unwrap();
        let got = read_state(tmp.path(), "no-such-id").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn state_path_is_under_workspace_state_dir() {
        let p = state_path(Path::new("/tmp/ws"), "req-xyz");
        let s = p.to_string_lossy();
        assert!(s.starts_with("/tmp/ws/"), "{s}");
        assert!(s.contains(".state/scout_runs"), "{s}");
        assert!(s.ends_with("req-xyz.json"), "{s}");
    }

    #[test]
    fn atomic_write_leaves_no_tempfiles() {
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            let state = fixture_state(&format!("req-{i}"));
            write_state(tmp.path(), &state).unwrap();
        }
        let dir = state_dir(tmp.path());
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 5);
        assert!(!entries.iter().any(|n| n.contains(".tmp")));
    }

    #[test]
    fn latest_state_resolves_most_recent_by_mtime() {
        let tmp = TempDir::new().unwrap();
        let older = fixture_state("req-older");
        write_state(tmp.path(), &older).unwrap();
        // Backdate the first file so the second is unambiguously newer
        // even on filesystems with coarse mtime resolution. `set_modified`
        // has been stable since Rust 1.75; the autocoder crate already
        // builds on a newer toolchain.
        let older_path = state_path(tmp.path(), &older.request_id);
        let one_minute_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&older_path)
            .unwrap()
            .set_modified(one_minute_ago)
            .unwrap();
        let newer = fixture_state("req-newer");
        write_state(tmp.path(), &newer).unwrap();
        let got = latest_state(tmp.path()).unwrap().unwrap();
        assert_eq!(got.request_id, "req-newer");
    }

    #[test]
    fn latest_state_missing_dir_returns_none() {
        let tmp = TempDir::new().unwrap();
        let got = latest_state(tmp.path()).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn list_thread_anchors_returns_all_pairs() {
        let tmp = TempDir::new().unwrap();
        let a = fixture_state("req-a");
        let mut b = fixture_state("req-b");
        b.thread_ts = "9876543210.000111".to_string();
        write_state(tmp.path(), &a).unwrap();
        write_state(tmp.path(), &b).unwrap();
        let mut pairs = list_thread_anchors(tmp.path()).unwrap();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("req-a".to_string(), "1748399999.001234".to_string()),
                ("req-b".to_string(), "9876543210.000111".to_string()),
            ]
        );
    }

    #[test]
    fn clear_all_removes_every_json_and_reports_count() {
        let tmp = TempDir::new().unwrap();
        for i in 0..3 {
            write_state(tmp.path(), &fixture_state(&format!("req-{i}"))).unwrap();
        }
        let n = clear_all(tmp.path()).unwrap();
        assert_eq!(n, 3);
        let again = clear_all(tmp.path()).unwrap();
        assert_eq!(again, 0, "clear-scout must be idempotent");
    }

    #[test]
    fn clear_all_missing_dir_is_zero() {
        let tmp = TempDir::new().unwrap();
        let n = clear_all(tmp.path()).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn validate_items_accepts_well_formed_list() {
        let items = vec![
            fixture_item(1, "security", "small"),
            fixture_item(2, "perf", "medium"),
            fixture_item(3, "documentation", "large"),
        ];
        validate_items(&items, 30).unwrap();
    }

    #[test]
    fn validate_items_rejects_unknown_category() {
        let items = vec![fixture_item(1, "totally-bogus", "small")];
        let err = validate_items(&items, 30).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown category"), "got: {msg}");
    }

    #[test]
    fn validate_items_rejects_unknown_tractability() {
        let items = vec![fixture_item(1, "bug", "huge")];
        let err = validate_items(&items, 30).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown tractability"), "got: {msg}");
    }

    #[test]
    fn validate_items_rejects_overflow_against_max() {
        let items = vec![
            fixture_item(1, "bug", "small"),
            fixture_item(2, "bug", "small"),
            fixture_item(3, "bug", "small"),
        ];
        let err = validate_items(&items, 2).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("max_items") || msg.contains("exceeds"),
            "got: {msg}"
        );
    }

    #[test]
    fn validate_items_rejects_blank_title_body_source() {
        let mut item = fixture_item(1, "bug", "small");
        item.title = "".into();
        assert!(validate_items(&[item], 30).is_err());
        let mut item = fixture_item(1, "bug", "small");
        item.body = "  \n".into();
        assert!(validate_items(&[item], 30).is_err());
        let mut item = fixture_item(1, "bug", "small");
        item.source = "".into();
        assert!(validate_items(&[item], 30).is_err());
    }

    #[test]
    fn vecdeque_round_trip_for_pending_request_ids() {
        use std::collections::VecDeque;
        let mut q: VecDeque<String> = VecDeque::new();
        q.push_back("req-a".into());
        q.push_back("req-b".into());
        q.push_back("req-c".into());
        assert_eq!(q.pop_front().as_deref(), Some("req-a"));
        assert_eq!(q.pop_front().as_deref(), Some("req-b"));
        assert_eq!(q.pop_front().as_deref(), Some("req-c"));
        assert!(q.pop_front().is_none());
    }
}
