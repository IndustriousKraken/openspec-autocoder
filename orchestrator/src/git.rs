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

pub fn push_force_with_lease(workspace: &Path, branch: &str) -> Result<()> {
    run_git(
        workspace,
        "push --force-with-lease",
        &["push", "--force-with-lease", "origin", branch],
    )?;
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

/// `git push origin --delete <branch>` — delete a branch on the remote.
/// Idempotent for the "remote branch does not exist" case (logs at debug
/// and returns Ok). Other failures (auth, network, etc.) propagate as `Err`.
pub fn delete_branch_remote(workspace: &Path, branch: &str) -> Result<()> {
    let probe = Command::new("git")
        .args(["ls-remote", "--heads", "origin", branch])
        .current_dir(workspace)
        .output()
        .with_context(|| format!("spawning `git ls-remote` to probe remote branch {branch}"))?;
    if !probe.status.success() {
        let stderr = String::from_utf8_lossy(&probe.stderr).trim().to_string();
        return Err(anyhow!("git ls-remote failed: {stderr}"));
    }
    if probe.stdout.is_empty() {
        tracing::debug!("remote branch `{branch}` already absent; nothing to delete");
        return Ok(());
    }
    run_git(
        workspace,
        "push --delete",
        &["push", "origin", "--delete", branch],
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

        delete_branch_remote(&ws, "doomed").unwrap();

        let probe = Command::new("git")
            .args(["ls-remote", "--heads", "origin", "doomed"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(probe.stdout.is_empty(), "remote should be gone after delete");

        // Idempotent: second call against an absent remote branch is Ok.
        delete_branch_remote(&ws, "doomed").unwrap();
        delete_branch_remote(&ws, "never-existed-remote").unwrap();
    }
}
