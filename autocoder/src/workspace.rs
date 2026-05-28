//! Per-repository workspace management: deterministic path derivation,
//! idempotent clone-or-fetch, and startup-time collision detection.

use crate::config::GithubConfig;
use crate::github::{self, DeleteOutcome};
use crate::github_credentials::resolve_token;
use crate::{config::RepositoryConfig, git};
use crate::paths;
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Compute the workspace root: `<cache_dir>/workspaces/`. Resolved from
/// the process-global `DaemonPaths` when initialized (production); in
/// tests that haven't installed paths this falls back to a fixed root
/// under the system temp dir, preserving pre-`DaemonPaths` behavior for
/// existing test fixtures.
pub fn workspace_root() -> PathBuf {
    paths::current().workspaces_dir()
}

/// Derive a per-repo workspace path under `<cache_dir>/workspaces/`.
/// Deterministic: the same URL always produces the same path. SSH and
/// HTTPS forms of the same repository collapse to the same derived path.
pub fn derive_path(url: &str) -> PathBuf {
    workspace_root().join(sanitize(url))
}

/// URL → directory-name sanitization, exposed so other state writes
/// (failure-state, revisions, run-log per-repo subdirs) can derive
/// matching per-repo identifiers. Keeps the convention single-sourced.
#[allow(dead_code)]
pub fn sanitize_url(url: &str) -> String {
    sanitize(url)
}

fn sanitize(url: &str) -> String {
    let stripped = url
        .strip_prefix("git@")
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let stripped = stripped.strip_suffix(".git").unwrap_or(stripped);
    stripped
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve the workspace path for a repository: explicit `local_path` if set,
/// otherwise the derived path.
pub fn resolve_path(repo: &RepositoryConfig) -> PathBuf {
    repo.local_path
        .clone()
        .unwrap_or_else(|| derive_path(&repo.url))
}

/// Ensure the repository is locally cloned. If the path does not exist, run
/// `git clone`. If it exists and is a git repository, run `git fetch`. If it
/// exists but is not a git repo, return an error without modifying the path.
///
/// When `fork` is `Some((fork_url, agent_branch))`, after the clone or fetch
/// the manager idempotently registers a second remote named `fork` pointing
/// at that URL — used by fork-PR mode to push the agent branch to a fork
/// instead of upstream. On a fresh clone the manager also fetches ONLY
/// `agent_branch` from the fork (via an explicit refspec), populating
/// `refs/remotes/fork/<agent_branch>` so that subsequent
/// `git push --force-with-lease fork <agent_branch>` has accurate local
/// tracking data. Other branches on the fork are intentionally NOT fetched,
/// so a fork branch whose name shadows an upstream branch (e.g. both have
/// `dev`) cannot cause `git checkout <base>` DWIM to fail with "matched
/// multiple remote tracking branches".
pub fn ensure_initialized(
    workspace: &Path,
    url: &str,
    fork: Option<(&str, &str)>,
) -> Result<()> {
    // Partial-clone self-heal: if the directory exists but has no
    // `.git/`, it is almost certainly leftover from a previously
    // interrupted clone. Verify there is no operator-meaningful state
    // we'd be destroying, then wipe and re-clone fresh. See
    // `workspace-self-heal-partial-clone` for the full safety contract.
    if workspace.is_dir() && !workspace.join(".git").is_dir() {
        match safe_to_auto_clean(workspace) {
            Ok(()) => {
                tracing::warn!(
                    workspace = %workspace.display(),
                    repo = %url,
                    "workspace exists without .git; partial clone artifact detected. Deleting and re-cloning."
                );
                std::fs::remove_dir_all(workspace).with_context(|| {
                    format!(
                        "auto-cleanup of partial workspace at {} failed",
                        workspace.display()
                    )
                })?;
            }
            Err(tripwire) => {
                return Err(anyhow!(
                    "workspace path exists but is not a git repository (no .git directory): {} \
                     (partial cleanup refused: {tripwire}; manual operator inspection required)",
                    workspace.display()
                ));
            }
        }
    }
    let did_clone = !workspace.exists();
    if did_clone {
        git::clone(workspace, url)
            .with_context(|| format!("cloning {url} into {}", workspace.display()))?;
    } else {
        git::fetch(workspace)
            .with_context(|| format!("fetching origin in {}", workspace.display()))?;
    }
    if let Some((fork_url, agent_branch)) = fork {
        git::ensure_remote(workspace, "fork", fork_url)
            .with_context(|| format!("ensuring fork remote points at {fork_url}"))?;
        // After a fresh clone, populate `refs/remotes/fork/<agent_branch>`
        // so the local tracking ref reflects the fork's actual state for
        // that branch. Without this, the next iteration's
        // `git push --force-with-lease fork <agent_branch>` compares an
        // empty local tracking value against the remote's existing commits
        // and fails with "stale info". The single-branch refspec also
        // prevents the fork's other branches from materializing as
        // `refs/remotes/fork/*` refs, which would otherwise break
        // `git checkout <base>` DWIM when the fork has shadow branches
        // (a fork branch with the same name as an upstream branch). A
        // fetch failure here is non-fatal: the empty tracking ref is no
        // worse than pre-fix behavior, and any real divergence still
        // surfaces via the existing branch-push-failure alert path.
        if did_clone {
            if let Err(e) = git::fetch_remote_branch(workspace, "fork", agent_branch) {
                tracing::warn!(
                    workspace = %workspace.display(),
                    fork_url = %fork_url,
                    agent_branch = %agent_branch,
                    "post-clone `git fetch fork <agent_branch>` failed; local tracking ref will be empty until first successful push: {e:#}"
                );
            }
        }
    }
    // Per-workspace bookkeeping files live at the workspace root and must
    // not appear in `git status` output (the dirty-check before each pass
    // would otherwise refuse to proceed). Register them in
    // `.git/info/exclude` once at init; the function is idempotent so a
    // duplicate entry is never added.
    if let Err(e) = ensure_git_info_excluded(workspace, ".failure-state.json") {
        tracing::warn!(
            workspace = %workspace.display(),
            "could not register .failure-state.json in .git/info/exclude: {e:#}"
        );
    }
    if let Err(e) = ensure_git_info_excluded(workspace, ".audit-state.json") {
        tracing::warn!(
            workspace = %workspace.display(),
            "could not register .audit-state.json in .git/info/exclude: {e:#}"
        );
    }
    // Per-change perma-stuck markers live at
    // `openspec/changes/<change>/.perma-stuck.json`. They are operator-
    // managed (deletion is the "retry this change" signal), so they
    // must NOT trip the pre-pass dirty check AND must survive the
    // per-iteration `git clean -fd` recovery. `.git/info/exclude` makes
    // them gitignored at any depth, which gets both behaviors for free:
    // `git status --porcelain` omits ignored files, and `git clean -fd`
    // (without `-x`) preserves them.
    if let Err(e) = ensure_git_info_excluded(workspace, ".perma-stuck.json") {
        tracing::warn!(
            workspace = %workspace.display(),
            "could not register .perma-stuck.json in .git/info/exclude: {e:#}"
        );
    }
    // Per-change spec-revision markers live at
    // `openspec/changes/<change>/.needs-spec-revision.json`. They are
    // operator-managed (deletion is the "retry this change" signal) and
    // follow the same gitignore contract as `.perma-stuck.json`: they
    // must not trip the pre-pass dirty check and must survive
    // `git clean -fd` during per-iteration recovery.
    if let Err(e) = ensure_git_info_excluded(workspace, ".needs-spec-revision.json") {
        tracing::warn!(
            workspace = %workspace.display(),
            "could not register .needs-spec-revision.json in .git/info/exclude: {e:#}"
        );
    }
    Ok(())
}

/// Inspect a `<workspace>` directory that has no `.git/` and decide
/// whether auto-cleanup (delete + re-clone) is safe. Returns `Ok(())`
/// when the directory is structurally a partial-clone artifact with no
/// operator-meaningful state. Returns `Err(tripwire)` describing the
/// first marker found if the directory contains anything that must not
/// be silently destroyed (operator-managed change markers, in-progress
/// locks). The `.alert-state.json` at the workspace root is explicitly
/// NOT a tripwire — it is daemon-written and reproducible.
fn safe_to_auto_clean(workspace: &Path) -> Result<(), &'static str> {
    fn walk(
        dir: &Path,
        under_openspec_changes: bool,
    ) -> Result<(), &'static str> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_file() || file_type.is_symlink() {
                if name.starts_with(".in-progress") {
                    return Err("contains .in-progress lock file");
                }
                if under_openspec_changes {
                    if name == ".perma-stuck.json" || name == ".needs-spec-revision.json" {
                        return Err(
                            "contains .perma-stuck.json or .needs-spec-revision.json marker",
                        );
                    }
                    if name == ".question.json" || name == ".answer.json" {
                        return Err("contains AskUser .question.json or .answer.json marker");
                    }
                }
            } else if file_type.is_dir() {
                let next_under_changes = under_openspec_changes
                    || (dir
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|p| p == "openspec")
                        .unwrap_or(false)
                        && name == "changes");
                walk(&path, next_under_changes)?;
            }
        }
        Ok(())
    }
    walk(workspace, false)
}

/// Append `entry` to `<workspace>/.git/info/exclude` if it is not already
/// present (whitespace-trimmed line match). The file is created if absent.
/// Errors propagate so the caller can decide whether to ignore them; this
/// function never panics or fails silently on parse issues.
pub fn ensure_git_info_excluded(workspace: &Path, entry: &str) -> Result<()> {
    let exclude_path = workspace.join(".git").join("info").join("exclude");
    let parent = exclude_path
        .parent()
        .ok_or_else(|| anyhow!("exclude path has no parent: {}", exclude_path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let existing = match std::fs::read_to_string(&exclude_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading {}", exclude_path.display()));
        }
    };
    let already = existing.lines().any(|line| line.trim() == entry);
    if already {
        return Ok(());
    }
    let needs_newline = !existing.is_empty() && !existing.ends_with('\n');
    let mut updated = existing;
    if needs_newline {
        updated.push('\n');
    }
    updated.push_str(entry);
    updated.push('\n');
    std::fs::write(&exclude_path, updated)
        .with_context(|| format!("writing {}", exclude_path.display()))?;
    Ok(())
}

/// Outcome of a `recreate_fork` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecreateOutcome {
    /// DELETE returned 2xx/404 and the subsequent CREATE succeeded and
    /// the fork is reachable. The local workspace state is unchanged
    /// (caller still needs to run `ensure_initialized` to clone).
    Recreated,
    /// DELETE returned 403 (typically: the PAT lacks the `delete_repo`
    /// scope). No CREATE was attempted. The caller must fall back to
    /// the conservative `ensure_initialized` path.
    Forbidden,
}

/// Destructively recreate the fork for `repo` on GitHub: DELETE the
/// existing fork under `github_cfg.fork_owner`, wait briefly for the
/// deletion to propagate, then POST a fresh fork from upstream and
/// poll until reachable (up to 30s).
///
/// Caller must verify `github_cfg.fork_owner.is_some()` and the
/// `recreate_fork_on_reinit` flag before invoking; this function does
/// not re-check those conditions.
///
/// Does NOT touch the local workspace directory — only GitHub state.
/// The caller is responsible for invoking `ensure_initialized` after
/// a successful return to clone from upstream and register the (now
/// pristine) fork remote.
pub async fn recreate_fork(
    github_cfg: &GithubConfig,
    repo: &RepositoryConfig,
) -> Result<RecreateOutcome> {
    recreate_fork_inner(github::DEFAULT_API_BASE, github_cfg, repo, true).await
}

/// Same as `recreate_fork` but with an injectable API base URL and a
/// flag to skip the (slow) post-create reachability poll. Used by
/// tests to drive both 2xx and error paths through a mockito server
/// without waiting on a real GitHub round-trip.
#[cfg(test)]
pub(crate) async fn recreate_fork_at_for_test(
    api_base: &str,
    github_cfg: &GithubConfig,
    repo: &RepositoryConfig,
) -> Result<RecreateOutcome> {
    recreate_fork_inner(api_base, github_cfg, repo, false).await
}

async fn recreate_fork_inner(
    api_base: &str,
    github_cfg: &GithubConfig,
    repo: &RepositoryConfig,
    poll_reachable: bool,
) -> Result<RecreateOutcome> {
    let fork_owner = github_cfg
        .fork_owner
        .as_deref()
        .ok_or_else(|| anyhow!("recreate_fork called with fork_owner unset"))?;
    let (upstream_owner, repo_name) = github::parse_repo_url(&repo.url)
        .with_context(|| format!("parsing upstream URL `{}`", repo.url))?;
    let token = resolve_token(github_cfg, &upstream_owner)
        .with_context(|| format!("resolving GitHub token for owner `{upstream_owner}`"))?;

    match github::delete_repo_at(api_base, fork_owner, &repo_name, &token).await? {
        DeleteOutcome::Deleted => {
            tracing::info!(
                upstream = %repo.url,
                fork_owner = %fork_owner,
                "recreate_fork: deleted existing fork on GitHub"
            );
        }
        DeleteOutcome::AlreadyGone => {
            tracing::info!(
                upstream = %repo.url,
                fork_owner = %fork_owner,
                "recreate_fork: fork already absent; proceeding to recreate"
            );
        }
        DeleteOutcome::Forbidden => {
            tracing::error!(
                upstream = %repo.url,
                fork_owner = %fork_owner,
                "recreate_fork: DELETE returned 403 — the operator's PAT \
                 likely lacks the `delete_repo` scope. Falling back to \
                 the conservative (non-recreating) workspace init path."
            );
            return Ok(RecreateOutcome::Forbidden);
        }
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    github::create_fork_at(api_base, &upstream_owner, &repo_name, &token)
        .await
        .with_context(|| {
            format!("re-forking `{upstream_owner}/{repo_name}` under `{fork_owner}`")
        })?;

    if poll_reachable {
        let fork_url = github::derive_fork_url(&repo.url, fork_owner)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut reachable = false;
        while Instant::now() < deadline {
            if git::ls_remote_head(&fork_url).is_ok() {
                reachable = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if !reachable {
            return Err(anyhow!(
                "recreate_fork: re-forked `{upstream_owner}/{repo_name}` under \
                 `{fork_owner}` but `{fork_url}` was not reachable within 30s"
            ));
        }
    }

    tracing::info!(
        upstream = %repo.url,
        fork_owner = %fork_owner,
        "recreate_fork: re-forked successfully (fork is now a pristine mirror of upstream)"
    );
    Ok(RecreateOutcome::Recreated)
}

/// Detect any two configured repositories that resolve to the same workspace
/// path. Returns an error naming both URLs and the shared path when found.
pub fn detect_collisions(repos: &[RepositoryConfig]) -> Result<()> {
    let mut seen: HashMap<PathBuf, &str> = HashMap::new();
    for repo in repos {
        let path = resolve_path(repo);
        if let Some(prior_url) = seen.get(&path) {
            return Err(anyhow!(
                "workspace path collision: `{prior}` and `{current}` both resolve to {path}",
                prior = prior_url,
                current = repo.url,
                path = path.display(),
            ));
        }
        seen.insert(path, &repo.url);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn cfg(url: &str) -> RepositoryConfig {
        RepositoryConfig {
            url: url.to_string(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    fn cfg_with_local(url: &str, local: &str) -> RepositoryConfig {
        RepositoryConfig {
            url: url.to_string(),
            local_path: Some(PathBuf::from(local)),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    #[test]
    fn derive_path_ssh_form() {
        let p = derive_path("git@github.com:owner/repo.git");
        assert_eq!(p.file_name().unwrap(), "github_com_owner_repo");
        assert_eq!(
            p.parent().unwrap().file_name().and_then(|s| s.to_str()),
            Some("workspaces"),
            "parent must be the `workspaces` subdir"
        );
    }

    #[test]
    fn derive_path_https_form() {
        let p = derive_path("https://github.com/owner/repo.git");
        assert_eq!(p.file_name().unwrap(), "github_com_owner_repo");
    }

    #[test]
    fn derive_path_strips_git_suffix() {
        let with_git = derive_path("git@github.com:owner/repo.git");
        let without = derive_path("git@github.com:owner/repo");
        assert_eq!(with_git, without);
    }

    #[test]
    fn derive_path_distinct_for_different_repos() {
        let a = derive_path("git@github.com:owner/repo-a.git");
        let b = derive_path("git@github.com:owner/repo-b.git");
        assert_ne!(a, b);
    }

    #[test]
    fn derive_path_is_stable() {
        let url = "git@github.com:owner/repo.git";
        assert_eq!(derive_path(url), derive_path(url));
    }

    #[test]
    fn collision_detected() {
        let repos = vec![
            cfg("git@github.com:owner/repo.git"),
            cfg("https://github.com/owner/repo.git"),
        ];
        let err = detect_collisions(&repos).expect_err("should detect collision");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("git@github.com:owner/repo.git"),
            "first url should be in error: {msg}"
        );
        assert!(
            msg.contains("https://github.com/owner/repo.git"),
            "second url should be in error: {msg}"
        );
    }

    #[test]
    fn collision_detected_via_explicit_local_path() {
        let repos = vec![
            cfg_with_local("git@github.com:owner/a.git", "/tmp/workspaces/shared"),
            cfg_with_local("git@github.com:owner/b.git", "/tmp/workspaces/shared"),
        ];
        let err = detect_collisions(&repos).expect_err("should detect explicit collision");
        let msg = format!("{err:#}");
        assert!(msg.contains("/tmp/workspaces/shared"), "got: {msg}");
    }

    #[test]
    fn no_collisions_when_distinct() {
        let repos = vec![
            cfg("git@github.com:owner/a.git"),
            cfg("git@github.com:owner/b.git"),
        ];
        detect_collisions(&repos).expect("distinct repos should pass");
    }

    fn run_git(path: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(path).status().unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// Create a regular fixture repo with one commit at `path`. Suitable as
    /// the "remote" target for a clone.
    fn make_fixture_remote(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init", "-q", "-b", "main"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "test"]);
        std::fs::write(path.join("README.md"), "hi\n").unwrap();
        run_git(path, &["add", "README.md"]);
        run_git(path, &["commit", "-q", "-m", "initial"]);
    }

    #[test]
    fn ensure_initialized_clones_when_absent() {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("local");
        make_fixture_remote(&remote);
        let url = remote.to_string_lossy().to_string();
        ensure_initialized(&workspace, &url, None).unwrap();
        assert!(workspace.join(".git").is_dir());
        assert!(workspace.join("README.md").is_file());
    }

    #[test]
    fn ensure_initialized_fetches_when_present() {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("local");
        make_fixture_remote(&remote);
        let url = remote.to_string_lossy().to_string();
        ensure_initialized(&workspace, &url, None).unwrap();
        // Make a local branch in the workspace; we'll verify it survives a fetch.
        run_git(&workspace, &["branch", "local-only-branch"]);
        // Second call should fetch (not re-clone) and preserve local branches.
        ensure_initialized(&workspace, &url, None).unwrap();
        let output = Command::new("git")
            .args(["branch", "--list", "local-only-branch"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("local-only-branch"),
            "local branch should survive ensure_initialized re-entry"
        );
    }

    fn list_remotes(workspace: &Path) -> String {
        let out = Command::new("git")
            .args(["remote", "-v"])
            .current_dir(workspace)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[test]
    fn adds_fork_remote_on_first_clone() {
        let dir = TempDir::new().unwrap();
        let upstream = dir.path().join("upstream");
        let fork = dir.path().join("fork");
        make_fixture_remote(&upstream);
        make_fixture_remote(&fork);
        let workspace = dir.path().join("local");
        let upstream_url = upstream.to_string_lossy().to_string();
        let fork_url = fork.to_string_lossy().to_string();
        ensure_initialized(&workspace, &upstream_url, Some((&fork_url, "main"))).unwrap();
        let remotes = list_remotes(&workspace);
        assert!(remotes.contains("origin"), "origin must be present: {remotes}");
        assert!(remotes.contains("fork"), "fork must be present: {remotes}");
        assert!(remotes.contains(&fork_url), "fork URL must match: {remotes}");
    }

    #[test]
    fn fork_remote_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let upstream = dir.path().join("upstream");
        let fork = dir.path().join("fork");
        make_fixture_remote(&upstream);
        make_fixture_remote(&fork);
        let workspace = dir.path().join("local");
        let upstream_url = upstream.to_string_lossy().to_string();
        let fork_url = fork.to_string_lossy().to_string();
        ensure_initialized(&workspace, &upstream_url, Some((&fork_url, "main"))).unwrap();
        // Second invocation must not error or duplicate the remote.
        ensure_initialized(&workspace, &upstream_url, Some((&fork_url, "main"))).unwrap();
        let remotes = list_remotes(&workspace);
        let fork_lines = remotes.lines().filter(|l| l.starts_with("fork")).count();
        // git remote -v emits two lines per remote (fetch + push).
        assert_eq!(fork_lines, 2, "fork should be listed exactly once: {remotes}");
    }

    #[test]
    fn ensure_initialized_fetches_fork_on_fresh_clone() {
        let dir = TempDir::new().unwrap();
        let upstream = dir.path().join("upstream");
        let fork = dir.path().join("fork");
        make_fixture_remote(&upstream);
        make_fixture_remote(&fork);
        // Distinguish fork's HEAD from upstream's: add an extra commit
        // only to fork. After ensure_initialized's fresh-clone path,
        // the post-clone `git fetch fork` should populate
        // refs/remotes/fork/main with fork's commit.
        std::fs::write(fork.join("FORK_ONLY.md"), "fork-only").unwrap();
        run_git(&fork, &["add", "FORK_ONLY.md"]);
        run_git(&fork, &["commit", "-q", "-m", "fork-only commit"]);

        let workspace = dir.path().join("local");
        let upstream_url = upstream.to_string_lossy().to_string();
        let fork_url = fork.to_string_lossy().to_string();
        ensure_initialized(&workspace, &upstream_url, Some((&fork_url, "main"))).unwrap();

        // refs/remotes/fork/main must resolve (the fetch ran).
        let probe = Command::new("git")
            .args(["rev-parse", "refs/remotes/fork/main"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "refs/remotes/fork/main must resolve after ensure_initialized's fresh-clone path"
        );
        // And its SHA must match fork's HEAD, not upstream's.
        let fork_head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&fork)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&probe.stdout).trim(),
            String::from_utf8_lossy(&fork_head.stdout).trim(),
            "local tracking ref must match fork's actual HEAD"
        );
    }

    #[test]
    fn ensure_initialized_does_not_re_fetch_fork_on_existing_workspace() {
        let dir = TempDir::new().unwrap();
        let upstream = dir.path().join("upstream");
        let fork = dir.path().join("fork");
        make_fixture_remote(&upstream);
        make_fixture_remote(&fork);
        let workspace = dir.path().join("local");
        let upstream_url = upstream.to_string_lossy().to_string();
        let fork_url = fork.to_string_lossy().to_string();
        // First init: fresh clone → fetch fork runs, captures fork's
        // current HEAD.
        ensure_initialized(&workspace, &upstream_url, Some((&fork_url, "main"))).unwrap();
        let initial = Command::new("git")
            .args(["rev-parse", "refs/remotes/fork/main"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        let initial_sha = String::from_utf8_lossy(&initial.stdout).trim().to_string();

        // Now advance fork by one commit.
        std::fs::write(fork.join("NEW.md"), "new").unwrap();
        run_git(&fork, &["add", "NEW.md"]);
        run_git(&fork, &["commit", "-q", "-m", "advance fork"]);

        // Second init: workspace exists → only `git fetch origin` runs,
        // NOT `git fetch fork`. Local tracking ref must remain stale.
        ensure_initialized(&workspace, &upstream_url, Some((&fork_url, "main"))).unwrap();
        let after = Command::new("git")
            .args(["rev-parse", "refs/remotes/fork/main"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        let after_sha = String::from_utf8_lossy(&after.stdout).trim().to_string();
        assert_eq!(
            initial_sha, after_sha,
            "fork tracking ref must NOT be updated on re-init of existing workspace; \
             only fresh-clone path fetches fork"
        );
    }

    #[test]
    fn ensure_initialized_fetches_only_agent_branch_from_fork() {
        let dir = TempDir::new().unwrap();
        let upstream = dir.path().join("upstream");
        let fork = dir.path().join("fork");
        make_fixture_remote(&upstream);
        make_fixture_remote(&fork);
        // Give the fork a second branch besides `main`. The fetch must
        // NOT pick this one up — it isn't the named agent branch.
        run_git(&fork, &["checkout", "-q", "-b", "leftover-fork-branch"]);
        std::fs::write(fork.join("LEFTOVER.md"), "leftover").unwrap();
        run_git(&fork, &["add", "LEFTOVER.md"]);
        run_git(&fork, &["commit", "-q", "-m", "leftover branch commit"]);
        run_git(&fork, &["checkout", "-q", "main"]);

        let workspace = dir.path().join("local");
        let upstream_url = upstream.to_string_lossy().to_string();
        let fork_url = fork.to_string_lossy().to_string();
        // Pretend `main` is the agent branch (fixture's only branch).
        ensure_initialized(&workspace, &upstream_url, Some((&fork_url, "main"))).unwrap();

        // refs/remotes/fork/main MUST resolve (the agent branch was fetched).
        let main_probe = Command::new("git")
            .args(["rev-parse", "refs/remotes/fork/main"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(
            main_probe.status.success(),
            "refs/remotes/fork/main must resolve after single-branch fetch"
        );
        // refs/remotes/fork/leftover-fork-branch MUST NOT resolve.
        let leftover_probe = Command::new("git")
            .args(["rev-parse", "refs/remotes/fork/leftover-fork-branch"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(
            !leftover_probe.status.success(),
            "refs/remotes/fork/leftover-fork-branch MUST NOT resolve — the \
             single-branch refspec did not match it"
        );
    }

    #[test]
    fn checkout_base_branch_after_fork_init_does_not_ambiguate() {
        let dir = TempDir::new().unwrap();
        let upstream = dir.path().join("upstream");
        let fork = dir.path().join("fork");
        make_fixture_remote(&upstream);
        make_fixture_remote(&fork);
        // Give BOTH upstream and fork a `dev` branch. This reproduces
        // the production failure: previously `git fetch fork` (all
        // branches) caused `refs/remotes/fork/dev` to exist alongside
        // `refs/remotes/origin/dev`, and `git checkout dev` failed
        // with "matched multiple (2) remote tracking branches". After
        // the fix only `refs/remotes/fork/agent-q` is populated, so
        // `dev` resolves unambiguously to origin's copy.
        run_git(&upstream, &["checkout", "-q", "-b", "dev"]);
        std::fs::write(upstream.join("UPSTREAM_DEV.md"), "upstream dev").unwrap();
        run_git(&upstream, &["add", "UPSTREAM_DEV.md"]);
        run_git(&upstream, &["commit", "-q", "-m", "upstream dev work"]);
        run_git(&upstream, &["checkout", "-q", "main"]);

        run_git(&fork, &["checkout", "-q", "-b", "dev"]);
        std::fs::write(fork.join("FORK_DEV.md"), "fork dev shadow").unwrap();
        run_git(&fork, &["add", "FORK_DEV.md"]);
        run_git(&fork, &["commit", "-q", "-m", "fork dev shadow"]);
        run_git(&fork, &["checkout", "-q", "main"]);
        // Fork's agent branch.
        run_git(&fork, &["checkout", "-q", "-b", "agent-q"]);
        std::fs::write(fork.join("AGENT.md"), "agent").unwrap();
        run_git(&fork, &["add", "AGENT.md"]);
        run_git(&fork, &["commit", "-q", "-m", "agent work"]);
        run_git(&fork, &["checkout", "-q", "main"]);

        let workspace = dir.path().join("local");
        let upstream_url = upstream.to_string_lossy().to_string();
        let fork_url = fork.to_string_lossy().to_string();
        ensure_initialized(&workspace, &upstream_url, Some((&fork_url, "agent-q"))).unwrap();

        // The regression: `git checkout dev` must succeed without DWIM
        // ambiguity. (origin/dev is the only candidate; fork's dev
        // was filtered out by the single-branch refspec.)
        let checkout = Command::new("git")
            .args(["checkout", "dev"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        assert!(
            checkout.status.success(),
            "`git checkout dev` must succeed; stderr: {stderr}"
        );
        assert!(
            !stderr.contains("matched multiple"),
            "DWIM ambiguity error must not appear; stderr: {stderr}"
        );
    }

    #[test]
    fn ensure_initialized_tolerates_fork_fetch_failure() {
        let dir = TempDir::new().unwrap();
        let upstream = dir.path().join("upstream");
        make_fixture_remote(&upstream);
        let workspace = dir.path().join("local");
        let upstream_url = upstream.to_string_lossy().to_string();
        // Point fork to a non-existent path — the fetch will fail.
        let bogus_fork_url = dir.path().join("does-not-exist").to_string_lossy().to_string();
        // ensure_initialized must still return Ok (the fetch is best-effort).
        ensure_initialized(&workspace, &upstream_url, Some((&bogus_fork_url, "main")))
            .expect("ensure_initialized must tolerate fork fetch failure");
        // The fork remote was still registered.
        let remotes = list_remotes(&workspace);
        assert!(remotes.contains("fork"));
    }

    #[test]
    fn no_fork_remote_when_disabled() {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        make_fixture_remote(&remote);
        let workspace = dir.path().join("local");
        let url = remote.to_string_lossy().to_string();
        ensure_initialized(&workspace, &url, None).unwrap();
        let remotes = list_remotes(&workspace);
        assert!(remotes.contains("origin"), "origin must be present");
        assert!(!remotes.contains("fork"), "fork must NOT be present");
    }

    #[test]
    fn ensure_git_info_excluded_adds_once_and_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("local");
        make_fixture_remote(&remote);
        let url = remote.to_string_lossy().to_string();
        ensure_initialized(&workspace, &url, None).unwrap();

        let exclude_path = workspace.join(".git/info/exclude");
        // After ensure_initialized, every per-workspace bookkeeping file
        // should be registered.
        let contents = std::fs::read_to_string(&exclude_path).unwrap();
        for entry in [
            ".failure-state.json",
            ".audit-state.json",
            ".perma-stuck.json",
            ".needs-spec-revision.json",
        ] {
            assert!(
                contents.lines().any(|l| l.trim() == entry),
                "exclude file must contain {entry}: {contents}"
            );
        }

        // Calling ensure_initialized again must NOT duplicate any entry.
        ensure_initialized(&workspace, &url, None).unwrap();
        let contents = std::fs::read_to_string(&exclude_path).unwrap();
        for entry in [
            ".failure-state.json",
            ".audit-state.json",
            ".perma-stuck.json",
            ".needs-spec-revision.json",
        ] {
            let occurrences = contents.lines().filter(|l| l.trim() == entry).count();
            assert_eq!(occurrences, 1, "duplicate `{entry}` entry added: {contents}");
        }
    }

    // ============================================================
    // safe_to_auto_clean: unit tests for the safety check.
    // ============================================================

    #[test]
    fn safe_to_auto_clean_empty_directory_is_ok() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        safe_to_auto_clean(&workspace).expect("empty dir must be safe to auto-clean");
    }

    #[test]
    fn safe_to_auto_clean_alert_state_only_is_ok() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join(".alert-state.json"), r#"{"x":1}"#).unwrap();
        safe_to_auto_clean(&workspace)
            .expect(".alert-state.json is daemon-written and must not be a tripwire");
    }

    #[test]
    fn safe_to_auto_clean_partial_clone_tree_no_markers_is_ok() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        let change_dir = workspace.join("openspec/changes/foo");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join("proposal.md"), "## proposal").unwrap();
        std::fs::write(change_dir.join("tasks.md"), "## tasks").unwrap();
        safe_to_auto_clean(&workspace)
            .expect("partial tree without markers must be safe to auto-clean");
    }

    #[test]
    fn safe_to_auto_clean_perma_stuck_marker_blocks() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        let change_dir = workspace.join("openspec/changes/foo");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join(".perma-stuck.json"), "{}").unwrap();
        let err = safe_to_auto_clean(&workspace)
            .expect_err(".perma-stuck.json marker must block auto-cleanup");
        assert!(
            err.contains(".perma-stuck.json") || err.contains(".needs-spec-revision.json"),
            "tripwire description must name the marker: {err}"
        );
    }

    #[test]
    fn safe_to_auto_clean_needs_revision_marker_blocks() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        let change_dir = workspace.join("openspec/changes/foo");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join(".needs-spec-revision.json"), "{}").unwrap();
        safe_to_auto_clean(&workspace)
            .expect_err(".needs-spec-revision.json marker must block auto-cleanup");
    }

    #[test]
    fn safe_to_auto_clean_question_marker_blocks() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        let change_dir = workspace.join("openspec/changes/foo");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join(".question.json"), "{}").unwrap();
        let err = safe_to_auto_clean(&workspace)
            .expect_err(".question.json marker must block auto-cleanup");
        assert!(err.contains("AskUser"), "tripwire must name AskUser: {err}");
    }

    #[test]
    fn safe_to_auto_clean_answer_marker_blocks() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        let change_dir = workspace.join("openspec/changes/foo");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join(".answer.json"), "{}").unwrap();
        safe_to_auto_clean(&workspace)
            .expect_err(".answer.json marker must block auto-cleanup");
    }

    #[test]
    fn safe_to_auto_clean_in_progress_lock_blocks() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join(".in-progress-bar"), "").unwrap();
        let err = safe_to_auto_clean(&workspace)
            .expect_err(".in-progress* lock file must block auto-cleanup");
        assert!(
            err.contains(".in-progress"),
            "tripwire must name the in-progress lock: {err}"
        );
    }

    #[test]
    fn safe_to_auto_clean_in_progress_lock_at_depth_blocks() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        let change_dir = workspace.join("openspec/changes/foo");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join(".in-progress"), "").unwrap();
        safe_to_auto_clean(&workspace)
            .expect_err(".in-progress lock at any depth must block auto-cleanup");
    }

    #[test]
    fn safe_to_auto_clean_marker_outside_openspec_changes_is_ignored() {
        // A `.perma-stuck.json` directly at the workspace root is NOT a
        // tripwire — only files under `openspec/changes/` are operator-
        // meaningful change markers. (A file by that name at root would
        // be uncategorized junk and the marker-prefix check should not
        // misfire on it.)
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join(".perma-stuck.json"), "{}").unwrap();
        safe_to_auto_clean(&workspace)
            .expect("marker at root (outside openspec/changes) must not block");
    }

    // ============================================================
    // ensure_initialized: auto-cleanup integration tests.
    // ============================================================

    #[test]
    fn ensure_initialized_auto_cleans_partial_clone_and_re_clones() {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("local");
        make_fixture_remote(&remote);
        // Fixture: workspace dir exists, has openspec partial-clone
        // content but no .git/. ensure_initialized must auto-clean and
        // re-clone.
        std::fs::create_dir_all(workspace.join("openspec/changes/foo")).unwrap();
        std::fs::write(
            workspace.join("openspec/changes/foo/proposal.md"),
            "## proposal\n",
        )
        .unwrap();
        let url = remote.to_string_lossy().to_string();
        ensure_initialized(&workspace, &url, None)
            .expect("auto-cleanup + re-clone must succeed on a partial-clone artifact");
        assert!(
            workspace.join(".git").is_dir(),
            ".git/ must exist after auto-clean + re-clone"
        );
        assert!(
            workspace.join("README.md").is_file(),
            "remote's README.md must exist after re-clone (the auto-cleaned partial tree must be replaced)"
        );
        assert!(
            !workspace.join("openspec/changes/foo/proposal.md").exists(),
            "partial-clone artifact under openspec/changes/foo must NOT survive auto-cleanup"
        );
    }

    #[test]
    fn ensure_initialized_auto_clean_refuses_on_marker() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("not-a-repo");
        let change_dir = workspace.join("openspec/changes/foo");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(change_dir.join(".perma-stuck.json"), "{}").unwrap();
        let err = ensure_initialized(&workspace, "irrelevant-url", None)
            .expect_err("auto-cleanup must be refused when a marker is present");
        let msg = format!("{err:#}");
        assert!(msg.contains(".git"), "error should mention missing .git: {msg}");
        assert!(
            msg.contains("partial cleanup refused"),
            "error must mention partial-cleanup refusal: {msg}"
        );
        assert!(
            msg.contains(".perma-stuck.json")
                || msg.contains(".needs-spec-revision.json"),
            "error must name the tripwire: {msg}"
        );
        assert!(
            workspace.join("openspec/changes/foo/.perma-stuck.json").exists(),
            "marker must NOT be deleted when auto-cleanup is refused"
        );
    }

    #[test]
    fn ensure_initialized_re_clone_failure_surfaces_real_error() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("local");
        // Workspace exists with partial-clone-shape content but no .git/.
        std::fs::create_dir_all(workspace.join("openspec/changes/foo")).unwrap();
        std::fs::write(
            workspace.join("openspec/changes/foo/proposal.md"),
            "## proposal\n",
        )
        .unwrap();
        // Bogus URL guarantees the second clone fails.
        let bogus_url = dir.path().join("does-not-exist").to_string_lossy().to_string();
        let err = ensure_initialized(&workspace, &bogus_url, None)
            .expect_err("re-clone must fail when remote URL is bogus");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cloning") || msg.contains("clone") || msg.to_lowercase().contains("does not exist"),
            "error must surface the real clone failure, not 'exists but no .git': {msg}"
        );
        assert!(
            !msg.contains("exists but is not a git repository"),
            "secondary detection text must NOT appear after auto-cleanup ran: {msg}"
        );
    }

    #[test]
    fn ensure_initialized_does_not_auto_clean_when_workspace_absent() {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("never-existed");
        make_fixture_remote(&remote);
        let url = remote.to_string_lossy().to_string();
        // Workspace doesn't exist at all → fresh-clone path; auto-clean
        // branch must NOT be entered. The outcome is identical to the
        // happy-path clone.
        ensure_initialized(&workspace, &url, None).unwrap();
        assert!(workspace.join(".git").is_dir());
        assert!(workspace.join("README.md").is_file());
    }

    #[test]
    fn ensure_initialized_does_not_auto_clean_when_git_present() {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("local");
        make_fixture_remote(&remote);
        let url = remote.to_string_lossy().to_string();
        // First call clones normally.
        ensure_initialized(&workspace, &url, None).unwrap();
        // Plant a marker INSIDE openspec/changes/ to prove the safety
        // check is NOT invoked on the existing-with-.git/ path: if the
        // auto-cleanup branch were taken, the marker would trip the
        // safety check and return Err.
        std::fs::create_dir_all(workspace.join("openspec/changes/foo")).unwrap();
        std::fs::write(
            workspace.join("openspec/changes/foo/.perma-stuck.json"),
            "{}",
        )
        .unwrap();
        ensure_initialized(&workspace, &url, None)
            .expect("existing .git/ takes the fetch path; auto-clean branch must not fire");
        // Marker survives because we took the fetch path, not auto-clean.
        assert!(workspace.join("openspec/changes/foo/.perma-stuck.json").exists());
    }

    /// Build a `GithubConfig` whose token resolves inline (no env vars
    /// needed) so the recreate_fork tests stay hermetic.
    fn github_cfg_with_inline_token(fork_owner: &str) -> GithubConfig {
        GithubConfig {
            token_env: "AUTOCODER_RECREATE_TEST_UNSET".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "fake-pat-for-test".into(),
            }),
            owner_tokens: None,
            fork_owner: Some(fork_owner.into()),
            recreate_fork_on_reinit: true,
        }
    }

    fn repo_cfg(url: &str) -> RepositoryConfig {
        RepositoryConfig {
            url: url.into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    #[tokio::test]
    async fn recreate_fork_returns_recreated_on_normal_path() {
        let mut server = mockito::Server::new_async().await;
        let delete_mock = server
            .mock("DELETE", "/repos/fork-owner/repo")
            .match_header("authorization", "Bearer fake-pat-for-test")
            .with_status(204)
            .create_async()
            .await;
        let create_mock = server
            .mock("POST", "/repos/upstream-org/repo/forks")
            .match_header("authorization", "Bearer fake-pat-for-test")
            .with_status(202)
            .with_body(r#"{"name":"repo"}"#)
            .create_async()
            .await;

        let github_cfg = github_cfg_with_inline_token("fork-owner");
        let repo = repo_cfg("https://github.com/upstream-org/repo.git");
        let outcome = recreate_fork_at_for_test(&server.url(), &github_cfg, &repo)
            .await
            .expect("recreate should succeed on 204+202");
        assert_eq!(outcome, RecreateOutcome::Recreated);
        delete_mock.assert_async().await;
        create_mock.assert_async().await;
    }

    #[tokio::test]
    async fn recreate_fork_already_gone_proceeds_to_create() {
        let mut server = mockito::Server::new_async().await;
        let delete_mock = server
            .mock("DELETE", "/repos/fork-owner/repo")
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create_async()
            .await;
        let create_mock = server
            .mock("POST", "/repos/upstream-org/repo/forks")
            .with_status(202)
            .with_body(r#"{"name":"repo"}"#)
            .create_async()
            .await;

        let github_cfg = github_cfg_with_inline_token("fork-owner");
        let repo = repo_cfg("https://github.com/upstream-org/repo.git");
        let outcome = recreate_fork_at_for_test(&server.url(), &github_cfg, &repo)
            .await
            .expect("404-then-create path must succeed");
        assert_eq!(outcome, RecreateOutcome::Recreated);
        delete_mock.assert_async().await;
        create_mock.assert_async().await;
    }

    #[tokio::test]
    async fn recreate_fork_forbidden_returns_forbidden_without_creating() {
        let mut server = mockito::Server::new_async().await;
        let delete_mock = server
            .mock("DELETE", "/repos/fork-owner/repo")
            .with_status(403)
            .with_body(r#"{"message":"Resource not accessible by personal access token"}"#)
            .create_async()
            .await;
        // CREATE must NOT be invoked when DELETE returns 403.
        let create_mock = server
            .mock("POST", "/repos/upstream-org/repo/forks")
            .expect(0)
            .create_async()
            .await;

        let github_cfg = github_cfg_with_inline_token("fork-owner");
        let repo = repo_cfg("https://github.com/upstream-org/repo.git");
        let outcome = recreate_fork_at_for_test(&server.url(), &github_cfg, &repo)
            .await
            .expect("403 must surface as Forbidden, not Err");
        assert_eq!(outcome, RecreateOutcome::Forbidden);
        delete_mock.assert_async().await;
        create_mock.assert_async().await;
    }

    #[tokio::test]
    async fn recreate_fork_errors_when_fork_owner_unset() {
        let mut github_cfg = github_cfg_with_inline_token("placeholder");
        github_cfg.fork_owner = None;
        let repo = repo_cfg("https://github.com/upstream-org/repo.git");
        let err = recreate_fork_at_for_test("http://example.invalid", &github_cfg, &repo)
            .await
            .expect_err("missing fork_owner must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fork_owner"),
            "error must mention fork_owner: {msg}"
        );
    }
}
