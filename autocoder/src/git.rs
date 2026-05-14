//! Thin wrappers around `git` invoked as a subprocess.
//!
//! Every function takes `workspace: &Path` and runs the corresponding `git`
//! command with that path as the working directory. Non-zero exits are
//! converted to `Err(anyhow::anyhow!("git <op> failed: <stderr>"))`.

use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::{Command, Output};

/// Run a git command inside `workspace` and return captured `Output` on
/// success. Returns an error containing the trimmed stderr on non-zero exit.
fn run_git(workspace: &Path, op: &str, args: &[&str]) -> Result<Output> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .with_context(|| format!("spawning `git {op}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!("git {op} failed: {stderr}"));
    }
    Ok(output)
}

/// `git clone <url> <target>` — runs in the parent directory of `target` if it
/// exists, otherwise wherever (clone creates the directory itself).
pub fn clone(target: &Path, url: &str) -> Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating workspace parent {}", parent.display()))?;
    let target_str = target
        .to_str()
        .ok_or_else(|| anyhow!("workspace path is not valid UTF-8: {}", target.display()))?;
    let output = Command::new("git")
        .args(["clone", url, target_str])
        .current_dir(parent)
        .output()
        .context("spawning `git clone`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!("git clone failed: {stderr}"));
    }
    Ok(())
}

pub fn fetch(workspace: &Path) -> Result<()> {
    run_git(workspace, "fetch", &["fetch", "origin"])?;
    Ok(())
}

pub fn checkout(workspace: &Path, branch: &str) -> Result<()> {
    run_git(workspace, "checkout", &["checkout", branch])?;
    Ok(())
}

/// `git pull --ff-only origin <branch>`. Errors if the pull is not a
/// fast-forward (network failure, divergence, etc.).
pub fn pull_ff_only(workspace: &Path, branch: &str) -> Result<()> {
    run_git(workspace, "pull --ff-only", &["pull", "--ff-only", "origin", branch])?;
    Ok(())
}

/// `git checkout -B <branch>` — recreate the branch at HEAD, overwriting
/// any prior local content.
pub fn recreate_branch(workspace: &Path, branch: &str) -> Result<()> {
    run_git(workspace, "checkout -B", &["checkout", "-B", branch])?;
    Ok(())
}

pub fn add_all(workspace: &Path) -> Result<()> {
    run_git(workspace, "add -A", &["add", "-A"])?;
    Ok(())
}

pub fn commit(workspace: &Path, message: &str) -> Result<()> {
    run_git(workspace, "commit", &["commit", "-m", message])?;
    Ok(())
}

/// `git reset --hard origin/<branch>` — discard all local changes and align
/// HEAD with the remote tip of `branch`. Used by startup auto-recovery to
/// scrub residue from a prior failed iteration.
pub fn reset_hard_to_remote(workspace: &Path, branch: &str) -> Result<()> {
    let target = format!("origin/{branch}");
    run_git(workspace, "reset --hard origin/<branch>", &["reset", "--hard", &target])?;
    Ok(())
}

/// `git reset --hard HEAD` — discard staged AND unstaged changes back to
/// the current HEAD commit. Used to revert an executor's lazy-archive
/// rename without changing the active branch.
pub fn reset_hard_head(workspace: &Path) -> Result<()> {
    run_git(workspace, "reset --hard HEAD", &["reset", "--hard", "HEAD"])?;
    Ok(())
}

/// `git clean -fd` — remove untracked files and directories from the
/// workspace. Best-effort: errors propagate to the caller.
pub fn clean_force(workspace: &Path) -> Result<()> {
    run_git(workspace, "clean -fd", &["clean", "-fd"])?;
    Ok(())
}

pub fn push_force_with_lease(workspace: &Path, branch: &str, remote: &str) -> Result<()> {
    run_git(
        workspace,
        "push --force-with-lease",
        &["push", "--force-with-lease", remote, branch],
    )?;
    Ok(())
}

/// Idempotently ensure a remote named `name` exists with the given `url`. If
/// the remote is absent, run `git remote add`. If it exists with a stale
/// URL, run `git remote set-url`. If it already has the right URL, do
/// nothing.
pub fn ensure_remote(workspace: &Path, name: &str, url: &str) -> Result<()> {
    let probe = Command::new("git")
        .args(["remote", "get-url", name])
        .current_dir(workspace)
        .output()
        .with_context(|| format!("running `git remote get-url {name}` in {}", workspace.display()))?;
    if probe.status.success() {
        let current = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        if current == url {
            return Ok(());
        }
        run_git(
            workspace,
            "remote set-url",
            &["remote", "set-url", name, url],
        )?;
        return Ok(());
    }
    run_git(workspace, "remote add", &["remote", "add", name, url])?;
    Ok(())
}

/// Probe whether a remote URL is reachable for read. Used at startup to
/// verify fork existence before any polling task spawns. Returns Ok(()) on
/// reachable; Err with the git stderr on failure (network error, 404,
/// auth failure).
pub fn ls_remote_head(url: &str) -> Result<()> {
    let out = Command::new("git")
        .args(["ls-remote", "--quiet", url, "HEAD"])
        .output()
        .with_context(|| format!("running `git ls-remote {url}`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!("git ls-remote `{url}` failed: {stderr}"));
    }
    Ok(())
}

/// `git branch -D <branch>` — force-delete a local branch. Idempotent: if the
/// branch does not exist locally, this logs at debug and returns Ok. Any
/// other git failure propagates as `Err`.
pub fn delete_branch_local(workspace: &Path, branch: &str) -> Result<()> {
    // Probe for existence first rather than relying on git's stderr string,
    // which is not stable across versions.
    let probe = Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(workspace)
        .output()
        .with_context(|| format!("spawning `git rev-parse` to probe branch {branch}"))?;
    if !probe.status.success() {
        tracing::debug!("local branch `{branch}` already absent; nothing to delete");
        return Ok(());
    }
    run_git(workspace, "branch -D", &["branch", "-D", branch])?;
    Ok(())
}

/// `git push <remote> --delete <branch>` — delete a branch on the named
/// remote. Idempotent for the "remote branch does not exist" case (logs at
/// debug and returns Ok). Other failures (auth, network, etc.) propagate
/// as `Err`.
pub fn delete_branch_remote(workspace: &Path, branch: &str, remote: &str) -> Result<()> {
    let probe = Command::new("git")
        .args(["ls-remote", "--heads", remote, branch])
        .current_dir(workspace)
        .output()
        .with_context(|| format!("spawning `git ls-remote` to probe remote branch {branch}"))?;
    if !probe.status.success() {
        let stderr = String::from_utf8_lossy(&probe.stderr).trim().to_string();
        return Err(anyhow!("git ls-remote failed: {stderr}"));
    }
    if probe.stdout.is_empty() {
        tracing::debug!("remote branch `{branch}` on `{remote}` already absent; nothing to delete");
        return Ok(());
    }
    run_git(
        workspace,
        "push --delete",
        &["push", remote, "--delete", branch],
    )?;
    Ok(())
}

/// Return the trimmed stdout of `git status --porcelain`. Empty string ⇒
/// clean working tree.
pub fn status_porcelain(workspace: &Path) -> Result<String> {
    let output = run_git(workspace, "status --porcelain", &["status", "--porcelain"])?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Return the 40-character commit SHA pointed to by `rev`.
pub fn rev_parse(workspace: &Path, rev: &str) -> Result<String> {
    let output = run_git(workspace, "rev-parse", &["rev-parse", rev])?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Return the count of commits in `range` (e.g. `"main..agent-q"`).
pub fn rev_list_count(workspace: &Path, range: &str) -> Result<usize> {
    let output = run_git(workspace, "rev-list --count", &["rev-list", "--count", range])?;
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    s.parse::<usize>()
        .with_context(|| format!("parsing rev-list count output: {s:?}"))
}

/// Return the three-dot diff between `base` and `head` — i.e. the changes
/// present on `head` since it diverged from `base`. Equivalent to
/// `git diff <base>...<head>`.
pub fn diff_three_dot(workspace: &Path, base: &str, head: &str) -> Result<String> {
    let range = format!("{base}...{head}");
    let output = run_git(workspace, "diff", &["diff", &range])?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Return the name-only file list for the three-dot diff between `base`
/// and `head`. Equivalent to `git diff --name-only <base>...<head>`.
/// Empty lines are filtered. Each entry is a workspace-relative path.
pub fn diff_files_changed(workspace: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let range = format!("{base}...{head}");
    let output = run_git(
        workspace,
        "diff --name-only",
        &["diff", "--name-only", &range],
    )?;
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Set up a fixture git repo with one commit. Returns the temp dir guard
    /// (drop = cleanup) and the workspace path.
    fn fixture_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        run_init(&path, &["init", "-q", "-b", "main"]);
        run_init(&path, &["config", "user.email", "test@example.com"]);
        run_init(&path, &["config", "user.name", "test"]);
        std::fs::write(path.join("README.md"), "hello\n").unwrap();
        run_init(&path, &["add", "README.md"]);
        run_init(&path, &["commit", "-q", "-m", "initial"]);
        (dir, path)
    }

    fn run_init(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn rev_parse_returns_40_char_hex() {
        let (_dir, path) = fixture_repo();
        let sha = rev_parse(&path, "HEAD").unwrap();
        assert_eq!(sha.len(), 40, "expected 40-char SHA, got {sha:?}");
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "expected lowercase hex, got {sha:?}"
        );
    }

    #[test]
    fn status_porcelain_empty_after_clean_commit() {
        let (_dir, path) = fixture_repo();
        let s = status_porcelain(&path).unwrap();
        assert_eq!(s, "", "expected empty porcelain on clean tree, got {s:?}");
    }

    #[test]
    fn status_porcelain_shows_dirty_tree() {
        let (_dir, path) = fixture_repo();
        std::fs::write(path.join("new.txt"), "x").unwrap();
        let s = status_porcelain(&path).unwrap();
        assert!(s.contains("new.txt"), "expected dirty tree to mention new.txt: {s:?}");
    }

    #[test]
    fn add_and_commit_round_trip() {
        let (_dir, path) = fixture_repo();
        std::fs::write(path.join("note.txt"), "added\n").unwrap();
        add_all(&path).unwrap();
        commit(&path, "add note").unwrap();
        let s = status_porcelain(&path).unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn recreate_branch_creates_or_resets() {
        let (_dir, path) = fixture_repo();
        recreate_branch(&path, "agent-q").unwrap();
        let head = rev_parse(&path, "HEAD").unwrap();
        let agent = rev_parse(&path, "agent-q").unwrap();
        assert_eq!(head, agent);
        // Idempotent: re-running succeeds.
        recreate_branch(&path, "agent-q").unwrap();
    }

    #[test]
    fn nonzero_exit_returns_err_with_stderr() {
        let (_dir, path) = fixture_repo();
        let err = checkout(&path, "definitely-nonexistent-branch")
            .expect_err("checkout to a missing branch must fail");
        let msg = format!("{err:#}");
        assert!(msg.starts_with("git checkout failed"), "got: {msg}");
    }

    #[test]
    fn delete_branch_local_idempotent() {
        let (_dir, path) = fixture_repo();
        recreate_branch(&path, "doomed").unwrap();
        // Switch off the branch we're about to delete.
        checkout(&path, "main").unwrap();

        // First delete succeeds and removes the branch.
        delete_branch_local(&path, "doomed").unwrap();
        let listed = Command::new("git")
            .args(["branch", "--list", "doomed"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(
            listed.status.success() && listed.stdout.is_empty(),
            "branch should be gone after delete_branch_local"
        );

        // Second delete on the already-absent branch must NOT error.
        delete_branch_local(&path, "doomed").unwrap();

        // Deleting a branch that never existed is also Ok.
        delete_branch_local(&path, "never-existed").unwrap();
    }

    /// Build a bare remote alongside a working clone so we can exercise
    /// `git push origin --delete` against a real writable remote.
    fn fixture_clone_with_bare_remote() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote.git");
        let workspace = dir.path().join("workspace");

        let st = Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&remote)
            .status()
            .unwrap();
        assert!(st.success(), "bare init failed");

        let st = Command::new("git")
            .args([
                "clone",
                "-q",
                remote.to_string_lossy().as_ref(),
                workspace.to_string_lossy().as_ref(),
            ])
            .status()
            .unwrap();
        assert!(st.success(), "clone failed");
        run_init(&workspace, &["config", "user.email", "test@example.com"]);
        run_init(&workspace, &["config", "user.name", "test"]);
        // Need an initial commit on main so we can checkout / push.
        std::fs::write(workspace.join("README.md"), "hi\n").unwrap();
        run_init(&workspace, &["add", "README.md"]);
        run_init(&workspace, &["commit", "-q", "-m", "initial"]);
        run_init(&workspace, &["push", "-q", "-u", "origin", "main"]);

        (dir, workspace, remote)
    }

    #[test]
    fn push_uses_specified_remote() {
        let (dir, ws, _origin) = fixture_clone_with_bare_remote();
        // Set up a second bare remote.
        let fork_remote = dir.path().join("fork.git");
        std::fs::create_dir_all(&fork_remote).unwrap();
        let st = Command::new("git")
            .args(["init", "--bare", "-q", "-b", "main"])
            .current_dir(&fork_remote)
            .status()
            .unwrap();
        assert!(st.success());
        run_init(&ws, &["remote", "add", "fork", fork_remote.to_string_lossy().as_ref()]);

        // Create a branch and push only to fork.
        recreate_branch(&ws, "agent-q").unwrap();
        std::fs::write(ws.join("CHANGE.md"), "x").unwrap();
        run_init(&ws, &["add", "CHANGE.md"]);
        run_init(&ws, &["commit", "-q", "-m", "agent work"]);

        push_force_with_lease(&ws, "agent-q", "fork").unwrap();

        // Origin must NOT have agent-q.
        let origin_probe = Command::new("git")
            .args(["ls-remote", "--heads", "origin", "agent-q"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(
            origin_probe.stdout.is_empty(),
            "origin must NOT have agent-q; got: {}",
            String::from_utf8_lossy(&origin_probe.stdout)
        );
        // Fork MUST have agent-q.
        let fork_probe = Command::new("git")
            .args(["ls-remote", "--heads", "fork", "agent-q"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(
            !fork_probe.stdout.is_empty(),
            "fork MUST have agent-q after push"
        );
    }

    #[test]
    fn delete_branch_remote_deletes_and_is_idempotent() {
        let (_dir, ws, _remote) = fixture_clone_with_bare_remote();

        // Push a branch we can then delete remotely.
        recreate_branch(&ws, "doomed").unwrap();
        std::fs::write(ws.join("ON_DOOMED.md"), "x").unwrap();
        run_init(&ws, &["add", "ON_DOOMED.md"]);
        run_init(&ws, &["commit", "-q", "-m", "doomed work"]);
        run_init(&ws, &["push", "-q", "origin", "doomed"]);

        // Confirm remote has the branch, then delete it.
        let probe = Command::new("git")
            .args(["ls-remote", "--heads", "origin", "doomed"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(!probe.stdout.is_empty(), "remote should have doomed before delete");

        delete_branch_remote(&ws, "doomed", "origin").unwrap();

        let probe = Command::new("git")
            .args(["ls-remote", "--heads", "origin", "doomed"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(probe.stdout.is_empty(), "remote should be gone after delete");

        // Idempotent: second call against an absent remote branch is Ok.
        delete_branch_remote(&ws, "doomed", "origin").unwrap();
        delete_branch_remote(&ws, "never-existed-remote", "origin").unwrap();
    }
}
