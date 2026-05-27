//! Thin wrappers around `git` invoked as a subprocess.
//!
//! Every function takes `workspace: &Path` and runs the corresponding `git`
//! command with that path as the working directory. Non-zero exits are
//! converted to `Err(anyhow::anyhow!("git <op> failed: <stderr>"))`.

use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::{Command, Output};

/// Run a git command inside `workspace` and return captured `Output` on
/// success. On non-zero exit, builds an error string that surfaces the
/// failed command's diagnostic output: prefer stderr, fall back to
/// stdout when stderr is empty, include both labelled with `stderr:` /
/// `stdout:` when both are non-empty, and name the exit code in
/// parentheses when both streams are empty. This is the contract that
/// keeps the self-heal flow's `git commit` "nothing to commit, working
/// tree clean" message visible: that diagnostic line is stdout-only.
fn run_git(workspace: &Path, op: &str, args: &[&str]) -> Result<Output> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .with_context(|| format!("spawning `git {op}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let msg = match (stderr.is_empty(), stdout.is_empty()) {
            (false, true) => stderr,
            (false, false) => format!("stderr: {stderr}; stdout: {stdout}"),
            (true, false) => stdout,
            (true, true) => format!("(no output; exit {:?})", output.status.code()),
        };
        return Err(anyhow!("git {op} failed: {msg}"));
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

/// `git fetch <remote>` — fetch ALL branches from a named remote
/// (e.g. `fork`). Currently unused in production; retained for
/// completeness and as a building block for future callers that need
/// the wholesale-fetch semantic. Prefer `fetch_remote_branch` for the
/// post-clone fork sync — fetching only the agent branch avoids
/// shadow-branch DWIM ambiguity on `git checkout <base_branch>`.
#[allow(dead_code)]
pub fn fetch_remote(workspace: &Path, remote: &str) -> Result<()> {
    run_git(workspace, "fetch <remote>", &["fetch", remote])?;
    Ok(())
}

/// `git fetch <remote> +refs/heads/<branch>:refs/remotes/<remote>/<branch>`
/// — fetch ONLY the named branch from the remote, populating the
/// corresponding local tracking ref. The leading `+` enables forced
/// update so a non-fast-forward update on the named branch
/// (e.g. the fork's agent branch was rewritten) does not fail the
/// fetch. All other branches on the remote are intentionally not
/// fetched, so their refs never appear under `refs/remotes/<remote>/*`
/// and cannot interfere with subsequent `git checkout` DWIM.
pub fn fetch_remote_branch(workspace: &Path, remote: &str, branch: &str) -> Result<()> {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    run_git(
        workspace,
        "fetch <remote> <refspec>",
        &["fetch", remote, &refspec],
    )?;
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

/// Return the trimmed stdout of `git status --porcelain -uall`. Unlike
/// `status_porcelain`, this expands untracked directories to every
/// individual file path inside them, so callers doing per-path policy
/// checks (e.g. the audit framework's `WritePolicy::OpenSpecOnly`
/// enforcement) see the actual paths, not just the parent dir.
pub fn status_porcelain_untracked_all(workspace: &Path) -> Result<String> {
    let output = run_git(
        workspace,
        "status --porcelain -uall",
        &["status", "--porcelain", "-uall"],
    )?;
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

/// Read the latest commit on `branch` and return a `CommitSummary` (short
/// SHA, subject, age). Returns `Ok(None)` when the branch does not exist
/// (e.g. fresh clone, agent branch not yet created) — git emits
/// `unknown revision` / `bad revision` on stderr for that case and the
/// caller can render `(none)` without distinguishing from "branch exists
/// but has no commits."
///
/// The git invocation uses `%h%x09%ct%x09%s`: short-sha, committer
/// timestamp (Unix epoch seconds), subject. The first two fields are
/// fixed-shape (no tabs), so a tab character inside the subject is
/// preserved by splitting on the first two tabs only.
pub fn last_commit_summary(
    workspace: &Path,
    branch: &str,
) -> Result<Option<crate::chatops::operator_commands::CommitSummary>> {
    let output = Command::new("git")
        .args([
            "log",
            "-1",
            "--pretty=format:%h%x09%ct%x09%s",
            branch,
            "--",
        ])
        .current_dir(workspace)
        .output()
        .with_context(|| format!("spawning `git log -1` for branch {branch}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if stderr.contains("unknown revision")
            || stderr.contains("bad revision")
            || stderr.contains("does not have any commits")
        {
            return Ok(None);
        }
        let stderr_raw = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!("git log -1 failed: {stderr_raw}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim_end_matches('\n');
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut parts = trimmed.splitn(3, '\t');
    let short_sha = parts.next().unwrap_or("").to_string();
    let ts_str = parts.next().unwrap_or("");
    let subject = parts.next().unwrap_or("").to_string();
    let ts: i64 = ts_str
        .parse()
        .with_context(|| format!("parsing committer timestamp from `git log` output: {ts_str:?}"))?;
    let when = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .ok_or_else(|| anyhow!("committer timestamp out of range: {ts}"))?;
    let age = chrono::Utc::now() - when;
    Ok(Some(crate::chatops::operator_commands::CommitSummary {
        short_sha,
        subject,
        age,
    }))
}

/// Find commits on `head` (since divergence from `base`) whose commit
/// subject matches `<change>:` — the convention used by the orchestrator
/// when shipping a change. Returns SHAs in chronological order
/// (`--reverse`). Empty when no matching commit exists (e.g. the change
/// was archived with no committed work, or the commit message format
/// differs).
pub fn commits_for_change(
    workspace: &Path,
    base: &str,
    head: &str,
    change: &str,
) -> Result<Vec<String>> {
    let range = format!("{base}..{head}");
    let pattern = format!("^{}:", regex_escape(change));
    let output = run_git(
        workspace,
        "log --grep",
        &[
            "log",
            "--reverse",
            "--pretty=format:%H",
            "-E",
            "--grep",
            &pattern,
            &range,
        ],
    )?;
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Escape regex metacharacters in a literal so that `git log -E --grep`
/// treats them as literal text. Hand-rolled to avoid adding the `regex`
/// crate to this module just for one helper.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(
            c,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '['
                | ']' | '{' | '}' | '|' | '\\' | '/'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Return the unified diff produced by the given commit SHAs, concatenated
/// in the order provided. Each `git show -p <sha>` call emits the
/// commit's metadata header followed by the diff body. Used by the
/// per-change reviewer mode to scope each per-change prompt to that
/// change's commits alone.
pub fn diff_for_commits(workspace: &Path, shas: &[String]) -> Result<String> {
    let mut out = String::new();
    for sha in shas {
        let output = run_git(workspace, "show", &["show", "-p", "--no-color", sha])?;
        out.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    Ok(out)
}

/// Return the deduplicated workspace-relative paths touched by the given
/// commit SHAs (union, preserving first-seen order). Used to scope the
/// per-change reviewer prompt to the files that specific commit touched.
pub fn files_for_commits(workspace: &Path, shas: &[String]) -> Result<Vec<String>> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for sha in shas {
        let output = run_git(
            workspace,
            "show --name-only",
            &["show", "--name-only", "--pretty=format:", sha],
        )?;
        let raw = String::from_utf8_lossy(&output.stdout);
        for line in raw.lines() {
            let l = line.trim();
            if l.is_empty() {
                continue;
            }
            if seen.insert(l.to_string()) {
                out.push(l.to_string());
            }
        }
    }
    Ok(out)
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
    fn fetch_remote_invokes_git_fetch_for_named_remote() {
        let (dir, ws, _origin) = fixture_clone_with_bare_remote();
        // Set up a second bare remote with a distinct commit.
        let alt_remote = dir.path().join("alt.git");
        std::fs::create_dir_all(&alt_remote).unwrap();
        let st = Command::new("git")
            .args(["init", "--bare", "-q", "-b", "main"])
            .current_dir(&alt_remote)
            .status()
            .unwrap();
        assert!(st.success());
        // Seed alt with its own commit by pushing from a side worktree.
        let alt_work = dir.path().join("alt-work");
        std::fs::create_dir_all(&alt_work).unwrap();
        let alt_url = alt_remote.to_string_lossy().to_string();
        run_init(&alt_work, &["clone", "-q", &alt_url, "."]);
        run_init(&alt_work, &["config", "user.email", "test@example.com"]);
        run_init(&alt_work, &["config", "user.name", "test"]);
        std::fs::write(alt_work.join("ALT.md"), "alt content").unwrap();
        run_init(&alt_work, &["add", "ALT.md"]);
        run_init(&alt_work, &["commit", "-q", "-m", "alt initial"]);
        run_init(&alt_work, &["push", "-q", "origin", "main"]);

        run_init(&ws, &["remote", "add", "alt", &alt_url]);
        fetch_remote(&ws, "alt").expect("fetch_remote should succeed");

        // After fetch, refs/remotes/alt/main should resolve to alt's commit.
        let probe = Command::new("git")
            .args(["rev-parse", "refs/remotes/alt/main"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "refs/remotes/alt/main must resolve after fetch_remote"
        );
    }

    #[test]
    fn fetch_remote_branch_populates_only_named_branch() {
        let (dir, ws, _origin) = fixture_clone_with_bare_remote();
        // Build a fork remote with TWO branches: main and extra-branch.
        let fork_remote = dir.path().join("fork.git");
        std::fs::create_dir_all(&fork_remote).unwrap();
        let st = Command::new("git")
            .args(["init", "--bare", "-q", "-b", "main"])
            .current_dir(&fork_remote)
            .status()
            .unwrap();
        assert!(st.success());
        let fork_work = dir.path().join("fork-work");
        std::fs::create_dir_all(&fork_work).unwrap();
        let fork_url = fork_remote.to_string_lossy().to_string();
        run_init(&fork_work, &["clone", "-q", &fork_url, "."]);
        run_init(&fork_work, &["config", "user.email", "test@example.com"]);
        run_init(&fork_work, &["config", "user.name", "test"]);
        std::fs::write(fork_work.join("FORK.md"), "fork main").unwrap();
        run_init(&fork_work, &["add", "FORK.md"]);
        run_init(&fork_work, &["commit", "-q", "-m", "fork main initial"]);
        run_init(&fork_work, &["push", "-q", "origin", "main"]);
        // Push a second branch on the fork.
        run_init(&fork_work, &["checkout", "-q", "-b", "extra-branch"]);
        std::fs::write(fork_work.join("EXTRA.md"), "extra").unwrap();
        run_init(&fork_work, &["add", "EXTRA.md"]);
        run_init(&fork_work, &["commit", "-q", "-m", "extra"]);
        run_init(&fork_work, &["push", "-q", "origin", "extra-branch"]);

        run_init(&ws, &["remote", "add", "fork", &fork_url]);
        fetch_remote_branch(&ws, "fork", "main")
            .expect("fetch_remote_branch should succeed");

        // refs/remotes/fork/main MUST resolve.
        let main_probe = Command::new("git")
            .args(["rev-parse", "refs/remotes/fork/main"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(
            main_probe.status.success(),
            "refs/remotes/fork/main must resolve after fetch_remote_branch(main)"
        );
        // refs/remotes/fork/extra-branch MUST NOT resolve (we asked for
        // main only).
        let extra_probe = Command::new("git")
            .args(["rev-parse", "refs/remotes/fork/extra-branch"])
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(
            !extra_probe.status.success(),
            "refs/remotes/fork/extra-branch MUST NOT resolve after a single-branch fetch; \
             got stdout={:?}",
            String::from_utf8_lossy(&extra_probe.stdout)
        );
    }

    #[test]
    fn fetch_remote_branch_force_updates_non_ff() {
        let (dir, ws, _origin) = fixture_clone_with_bare_remote();
        let fork_remote = dir.path().join("fork.git");
        std::fs::create_dir_all(&fork_remote).unwrap();
        let st = Command::new("git")
            .args(["init", "--bare", "-q", "-b", "main"])
            .current_dir(&fork_remote)
            .status()
            .unwrap();
        assert!(st.success());
        // Seed fork with an agent-q branch.
        let fork_work = dir.path().join("fork-work");
        std::fs::create_dir_all(&fork_work).unwrap();
        let fork_url = fork_remote.to_string_lossy().to_string();
        run_init(&fork_work, &["clone", "-q", &fork_url, "."]);
        run_init(&fork_work, &["config", "user.email", "test@example.com"]);
        run_init(&fork_work, &["config", "user.name", "test"]);
        std::fs::write(fork_work.join("FORK.md"), "fork main").unwrap();
        run_init(&fork_work, &["add", "FORK.md"]);
        run_init(&fork_work, &["commit", "-q", "-m", "fork main initial"]);
        run_init(&fork_work, &["push", "-q", "origin", "main"]);
        run_init(&fork_work, &["checkout", "-q", "-b", "agent-q"]);
        std::fs::write(fork_work.join("AGENT_V1.md"), "v1").unwrap();
        run_init(&fork_work, &["add", "AGENT_V1.md"]);
        run_init(&fork_work, &["commit", "-q", "-m", "agent v1"]);
        run_init(&fork_work, &["push", "-q", "origin", "agent-q"]);

        run_init(&ws, &["remote", "add", "fork", &fork_url]);
        fetch_remote_branch(&ws, "fork", "agent-q").expect("v1 fetch");
        let v1_sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "refs/remotes/fork/agent-q"])
                .current_dir(&ws)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        // Rewrite the fork's agent-q history: reset hard to main, then
        // a different commit. This is a non-fast-forward update.
        run_init(&fork_work, &["checkout", "-q", "main"]);
        run_init(&fork_work, &["branch", "-q", "-D", "agent-q"]);
        run_init(&fork_work, &["checkout", "-q", "-b", "agent-q"]);
        std::fs::write(fork_work.join("AGENT_V2.md"), "v2").unwrap();
        run_init(&fork_work, &["add", "AGENT_V2.md"]);
        run_init(&fork_work, &["commit", "-q", "-m", "agent v2"]);
        run_init(&fork_work, &["push", "-q", "--force", "origin", "agent-q"]);

        // The `+` refspec must accept the non-FF update.
        fetch_remote_branch(&ws, "fork", "agent-q")
            .expect("non-FF fetch must succeed with `+` refspec");
        let v2_sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "refs/remotes/fork/agent-q"])
                .current_dir(&ws)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_ne!(
            v1_sha, v2_sha,
            "tracking ref must have moved to the rewritten commit"
        );
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
    fn last_commit_summary_happy_path() {
        let (_dir, path) = fixture_repo();
        let summary = last_commit_summary(&path, "main")
            .expect("query succeeds")
            .expect("commit exists on main");
        assert_eq!(summary.short_sha.len(), 7, "default short sha is 7 chars");
        assert!(
            summary.short_sha.chars().all(|c| c.is_ascii_hexdigit()),
            "short sha should be hex: {}",
            summary.short_sha
        );
        assert_eq!(summary.subject, "initial");
        // The commit was just made, so age is small but non-negative.
        assert!(
            summary.age.num_seconds() >= 0,
            "age must be non-negative: {:?}",
            summary.age
        );
    }

    #[test]
    fn last_commit_summary_nonexistent_branch_returns_none() {
        let (_dir, path) = fixture_repo();
        // The branch doesn't exist (fresh clone, agent branch not yet created).
        // We must NOT propagate this as an error — the status formatter
        // renders `(none)` in this case.
        let res = last_commit_summary(&path, "definitely-not-a-branch")
            .expect("nonexistent branch should be Ok(None), not Err");
        assert!(res.is_none(), "expected None for missing branch");
    }

    #[test]
    fn last_commit_summary_preserves_tab_in_subject() {
        let (_dir, path) = fixture_repo();
        // A subject with a tab character. The git format is
        // `%h\t%ct\t%s`; the splitter must split only on the FIRST two
        // tabs so a tab inside the subject survives.
        run_init(
            &path,
            &["commit", "-q", "--allow-empty", "-m", "before\tafter"],
        );
        let summary = last_commit_summary(&path, "main")
            .expect("query succeeds")
            .expect("commit exists");
        assert_eq!(
            summary.subject, "before\tafter",
            "tab inside subject must survive splitting"
        );
    }

    #[test]
    fn last_commit_summary_repo_with_no_commits_returns_none() {
        // `git init` only — no commits at all. HEAD is unborn; `git log -1`
        // exits non-zero with "does not have any commits" / "bad revision".
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        run_init(&path, &["init", "-q", "-b", "main"]);
        let res = last_commit_summary(&path, "HEAD")
            .expect("empty repo should be Ok(None), not Err");
        assert!(res.is_none(), "expected None for unborn HEAD");
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

    // ----- run_git failure-message format tests -----

    /// Real `git commit` against a workspace with nothing staged
    /// exits non-zero and prints "nothing to commit, working tree
    /// clean" to STDOUT (not stderr). The pre-existing `run_git`
    /// captured only stderr, masking the cause and leaving the
    /// self-heal flow's failure_reason as a bare colon-space. Verify
    /// the new run_git surfaces the stdout text.
    #[test]
    fn run_git_failure_includes_stdout_when_stderr_empty() {
        let (_dir, path) = fixture_repo();
        // commit with nothing staged → exit 1, stdout="nothing to commit, ...", stderr=""
        let err = commit(&path, "should fail").expect_err("nothing to commit must error");
        let msg = format!("{err:#}");
        assert!(
            msg.starts_with("git commit failed: "),
            "preserves the `git commit failed: ` prefix: {msg}"
        );
        assert!(
            msg.contains("nothing to commit"),
            "stdout-only diagnostic must be surfaced: {msg}"
        );
        assert!(
            !msg.ends_with("failed: "),
            "error must NOT end in a bare colon-space: {msg:?}"
        );
    }

    /// Drive run_git directly with a fixture `git status --porcelain
    /// -uall --bogus-flag` invocation that produces non-empty stderr
    /// and empty stdout. This is the existing legacy-stderr-only
    /// pattern; the message should still contain the stderr alone
    /// (no `stderr:` prefix, no `; stdout:` suffix when stdout is
    /// empty).
    #[test]
    fn run_git_failure_with_stderr_only_keeps_legacy_format() {
        let (_dir, path) = fixture_repo();
        let err = run_git(&path, "bogus", &["status", "--definitely-not-a-flag"])
            .expect_err("invalid flag must error");
        let msg = format!("{err:#}");
        assert!(msg.starts_with("git bogus failed: "), "got: {msg}");
        // git prints its usage/error to stderr for unknown flags. The
        // exact text varies across git versions, but the message must
        // NOT contain the "stderr:" / "; stdout:" labelling pattern
        // because stdout was empty.
        assert!(
            !msg.contains("stdout:"),
            "stdout-only branch should not append a stdout: clause: {msg}"
        );
    }

    /// When both stderr and stdout carry content, both must appear in
    /// the error labelled `stderr:` / `stdout:`. Provoking this from
    /// git directly is awkward (git captures hook stdout and
    /// redirects it to its own stderr), so synthesize via a git shell
    /// alias whose pipes flow straight through to git's own.
    #[test]
    fn run_git_failure_with_both_streams_labels_each() {
        let (_dir, path) = fixture_repo();
        let err = run_git(
            &path,
            "alias-both",
            &[
                "-c",
                "alias.both=!sh -c 'echo TO_STDOUT; echo TO_STDERR 1>&2; exit 7'",
                "both",
            ],
        )
        .expect_err("aliased shell command exiting 7 must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            msg.starts_with("git alias-both failed: "),
            "preserves the op-named prefix: {msg}"
        );
        assert!(
            msg.contains("stderr:") && msg.contains("stdout:"),
            "both streams must be labelled: {msg}"
        );
        assert!(
            msg.contains("TO_STDOUT"),
            "stdout content must appear: {msg}"
        );
        assert!(
            msg.contains("TO_STDERR"),
            "stderr content must appear: {msg}"
        );
    }

    /// When both streams are empty (a command that exits non-zero
    /// without printing anything), the error names the exit code in
    /// parentheses so the operator at least knows the exit semantics.
    #[test]
    fn run_git_failure_with_no_output_names_exit_code() {
        // Build a fixture: a pre-commit hook that prints nothing and
        // exits 17. git's "pre-commit failed" stderr line is suppressed
        // by passing --no-verify-style hook semantics... actually git
        // always prints something on hook failure. We need a different
        // approach: drive run_git against a custom invocation that
        // exits non-zero with no output. Use `git rev-parse --verify
        // --quiet <invalid>` which exits non-zero with no stderr and
        // no stdout when the rev doesn't exist.
        let (_dir, path) = fixture_repo();
        let err = run_git(
            &path,
            "rev-parse-quiet",
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/definitely-not-a-branch",
            ],
        )
        .expect_err("missing rev with --quiet must exit non-zero with no output");
        let msg = format!("{err:#}");
        assert!(msg.starts_with("git rev-parse-quiet failed: "), "got: {msg}");
        assert!(
            msg.contains("(no output; exit"),
            "must name the parenthetical exit-code clause: {msg}"
        );
        assert!(
            !msg.ends_with("failed: "),
            "error must NOT end in a bare colon-space: {msg:?}"
        );
    }
}
