//! `autocoder rewind` — recover from a failed PR or bad implementation by
//! deleting the agent branch and unarchiving named changes back into the
//! active queue.

use crate::config::{GithubConfig, RepositoryConfig};
use crate::{git, queue, workspace};
use anyhow::{Result, anyhow};
use std::io::{BufRead, Write};

/// Arguments to the `rewind` subcommand collected from clap.
#[derive(Debug, Clone)]
pub struct RewindArgs {
    pub changes: Vec<String>,
    pub hard: bool,
    pub repo: Option<String>,
}

/// Entry point invoked by `cli::dispatch`. Wraps real stdin/stdout for the
/// confirmation prompt; tests use `execute_with_io` directly.
pub async fn execute(
    repos: Vec<RepositoryConfig>,
    github: GithubConfig,
    args: RewindArgs,
) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    execute_with_io(repos, github, args, &mut stdin.lock(), &mut stdout.lock()).await
}

/// IO-injected core of `execute`. Tests pass in-memory cursors.
pub async fn execute_with_io<R: BufRead, W: Write>(
    repos: Vec<RepositoryConfig>,
    github: GithubConfig,
    args: RewindArgs,
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    let repo = resolve_repo(&repos, args.repo.as_deref())?;
    tracing::info!(url = repo.url.as_str(), "rewind targeting repository");

    let workspace_path = workspace::resolve_path(repo);
    let fork_url = match github.fork_owner.as_deref() {
        Some(owner) => Some(crate::github::derive_fork_url(&repo.url, owner)?),
        None => None,
    };
    workspace::ensure_initialized(&workspace_path, &repo.url, fork_url.as_deref())?;
    let remote_name = if github.fork_owner.is_some() { "fork" } else { "origin" };

    if !args.hard {
        if !confirm(repo, &args.changes, reader, writer)? {
            tracing::info!("rewind cancelled");
            return Ok(());
        }
    }

    // Move off the agent branch before deleting it; `git branch -D` refuses
    // to delete the currently-checked-out branch.
    git::checkout(&workspace_path, &repo.base_branch)?;

    // Local delete is mandatory on both soft AND hard rewind per spec.
    git::delete_branch_local(&workspace_path, &repo.agent_branch)?;

    if args.hard {
        // Remote delete only on --hard. Errors are logged but do not block
        // the unarchive step per design.md's "Hard rewind" decision.
        if let Err(e) = git::delete_branch_remote(&workspace_path, &repo.agent_branch, remote_name) {
            tracing::error!(
                url = repo.url.as_str(),
                "remote branch deletion failed for `{}`: {e:#}; unarchive will still proceed",
                repo.agent_branch
            );
        }
    }

    let mut successes: Vec<String> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    for change in &args.changes {
        match queue::unarchive(&workspace_path, change) {
            Ok(()) => {
                tracing::info!("unarchived change `{change}`");
                successes.push(change.clone());
            }
            Err(e) => {
                let reason = format!("{e:#}");
                tracing::error!("unarchive of `{change}` failed: {reason}");
                failures.push((change.clone(), reason));
            }
        }
    }

    if !failures.is_empty() {
        let summary = failures
            .iter()
            .map(|(name, reason)| format!("`{name}`: {reason}"))
            .collect::<Vec<_>>()
            .join("; ");
        let succeeded = if successes.is_empty() {
            "none".to_string()
        } else {
            successes
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(anyhow!(
            "rewind partially failed: succeeded=[{succeeded}]; failed=[{summary}]"
        ));
    }

    tracing::info!(
        url = repo.url.as_str(),
        "rewind complete: agent branch `{}` deleted (hard={}), unarchived {} change(s): {}",
        repo.agent_branch,
        args.hard,
        successes.len(),
        successes.join(", ")
    );
    Ok(())
}

/// Match the user-supplied `--repo` selector (or its absence) against the
/// configured repositories. Returns the chosen repo or a clear error.
pub fn resolve_repo<'a>(
    repos: &'a [RepositoryConfig],
    selector: Option<&str>,
) -> Result<&'a RepositoryConfig> {
    if repos.is_empty() {
        return Err(anyhow!("no repositories configured"));
    }
    match (repos.len(), selector) {
        (1, None) => {
            tracing::info!(
                url = repos[0].url.as_str(),
                "single-repo config; --repo defaulted to the only configured repository"
            );
            Ok(&repos[0])
        }
        (_, None) => {
            let available = repos
                .iter()
                .map(|r| short_name(&r.url))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow!(
                "--repo is required with multiple configured repositories. Available: {available}"
            ))
        }
        (_, Some(sel)) => {
            let matches: Vec<&RepositoryConfig> = repos
                .iter()
                .filter(|r| r.url == sel || short_name(&r.url) == sel)
                .collect();
            match matches.len() {
                1 => Ok(matches[0]),
                0 => {
                    let available = repos
                        .iter()
                        .map(|r| short_name(&r.url))
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(anyhow!(
                        "--repo `{sel}` did not match any configured repository. Available: {available}"
                    ))
                }
                _ => {
                    let conflicting = matches
                        .iter()
                        .map(|r| r.url.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(anyhow!(
                        "--repo `{sel}` ambiguously matched multiple repositories: {conflicting}. Use the full URL to disambiguate."
                    ))
                }
            }
        }
    }
}

/// Derive a short-name from a git URL: the basename, minus a trailing `.git`
/// if present.
fn short_name(url: &str) -> String {
    let stripped = url.strip_suffix(".git").unwrap_or(url);
    // For `git@host:owner/name` we want "name". For `https://host/owner/name`
    // we also want "name". Splitting on `/` works for HTTPS; the SSH form's
    // last `/`-separated segment is also `name` because the path after `:`
    // uses `/` for the owner/repo separator.
    stripped
        .rsplit('/')
        .next()
        .unwrap_or(stripped)
        .to_string()
}

fn confirm<R: BufRead, W: Write>(
    repo: &RepositoryConfig,
    changes: &[String],
    reader: &mut R,
    writer: &mut W,
) -> Result<bool> {
    let names = changes
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    write!(
        writer,
        "This will delete branch '{}' (local) and unarchive {} change(s) ({}). Proceed? [y/N] ",
        repo.agent_branch,
        changes.len(),
        names
    )?;
    writer.flush()?;
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let response = buf.trim();
    Ok(response == "y" || response == "Y")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

    /// Default test GithubConfig — direct-push mode, no fork.
    fn direct_push_github() -> GithubConfig {
        GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
        }
    }

    fn cfg(url: &str) -> RepositoryConfig {
        RepositoryConfig {
            url: url.to_string(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
        }
    }

    fn cfg_local(url: &str, local: &Path) -> RepositoryConfig {
        RepositoryConfig {
            url: url.to_string(),
            local_path: Some(local.to_path_buf()),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
        }
    }

    // ============================================================
    // 3.2 resolve_repo tests
    // ============================================================

    #[test]
    fn resolve_single_default() {
        let repos = vec![cfg("git@github.com:owner/only.git")];
        let r = resolve_repo(&repos, None).unwrap();
        assert_eq!(r.url, "git@github.com:owner/only.git");
    }

    #[test]
    fn resolve_multi_requires_selector() {
        let repos = vec![
            cfg("git@github.com:owner/a.git"),
            cfg("git@github.com:owner/b.git"),
        ];
        let err = resolve_repo(&repos, None).expect_err("multi w/o selector errors");
        let msg = format!("{err:#}");
        assert!(msg.contains("--repo is required"), "got: {msg}");
        assert!(msg.contains("a") && msg.contains("b"), "must list options: {msg}");
    }

    #[test]
    fn resolve_match_by_url() {
        let repos = vec![
            cfg("git@github.com:owner/a.git"),
            cfg("git@github.com:owner/b.git"),
        ];
        let r = resolve_repo(&repos, Some("git@github.com:owner/b.git")).unwrap();
        assert_eq!(r.url, "git@github.com:owner/b.git");
    }

    #[test]
    fn resolve_match_by_short_name() {
        let repos = vec![
            cfg("git@github.com:owner/a.git"),
            cfg("git@github.com:owner/b.git"),
        ];
        let r = resolve_repo(&repos, Some("b")).unwrap();
        assert_eq!(r.url, "git@github.com:owner/b.git");

        // HTTPS form short-name also works.
        let repos = vec![cfg("https://github.com/owner/repo.git")];
        let r = resolve_repo(&repos, Some("repo")).unwrap();
        assert_eq!(r.url, "https://github.com/owner/repo.git");
    }

    #[test]
    fn resolve_zero_matches_errors() {
        let repos = vec![cfg("git@github.com:owner/a.git")];
        let err = resolve_repo(&repos, Some("nope")).expect_err("no match errors");
        let msg = format!("{err:#}");
        assert!(msg.contains("nope"), "must name selector: {msg}");
        assert!(msg.contains("a"), "must list available: {msg}");
    }

    #[test]
    fn resolve_multi_matches_errors() {
        // Two repos with the same basename across different orgs collide.
        let repos = vec![
            cfg("git@github.com:org-a/shared.git"),
            cfg("git@github.com:org-b/shared.git"),
        ];
        let err = resolve_repo(&repos, Some("shared")).expect_err("ambiguous errors");
        let msg = format!("{err:#}");
        assert!(msg.contains("ambiguously"), "msg should call it ambiguous: {msg}");
        assert!(msg.contains("org-a") && msg.contains("org-b"), "must list both URLs: {msg}");
    }

    #[test]
    fn resolve_empty_repos_errors() {
        let repos: Vec<RepositoryConfig> = Vec::new();
        let err = resolve_repo(&repos, None).expect_err("empty errors");
        assert!(format!("{err:#}").contains("no repositories"), "{err:#}");
    }

    // ============================================================
    // 4.3 execute_with_io tests against bare-remote fixture
    // ============================================================

    fn run_git(path: &Path, args: &[&str]) {
        let st = Command::new("git").args(args).current_dir(path).status().unwrap();
        assert!(st.success(), "git {args:?} failed");
    }

    /// Set up: bare remote + working clone with `main` and `agent-q` branches,
    /// both pushed to the remote. Also seeds an archived change directory in
    /// the working clone so we can exercise unarchive.
    fn rewind_fixture(
        change_name: &str,
        extra_archived: &[&str],
    ) -> (TempDir, PathBuf, PathBuf) {
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
        run_git(&workspace, &["config", "user.email", "test@example.com"]);
        run_git(&workspace, &["config", "user.name", "test"]);
        std::fs::write(workspace.join("README.md"), "x\n").unwrap();
        run_git(&workspace, &["add", "README.md"]);
        run_git(&workspace, &["commit", "-q", "-m", "initial"]);
        run_git(&workspace, &["push", "-q", "-u", "origin", "main"]);

        // Create an agent-q branch with one extra commit (the work we're
        // rewinding); push it so the remote has it too.
        run_git(&workspace, &["checkout", "-q", "-B", "agent-q"]);
        std::fs::write(workspace.join("AGENT.md"), "agent\n").unwrap();
        run_git(&workspace, &["add", "AGENT.md"]);
        run_git(&workspace, &["commit", "-q", "-m", "agent work"]);
        run_git(&workspace, &["push", "-q", "-u", "origin", "agent-q"]);
        // Return to main so the working tree is clean and on base.
        run_git(&workspace, &["checkout", "-q", "main"]);

        // Place archived directories with date prefixes the unarchive regex
        // will match.
        for name in std::iter::once(&change_name).chain(extra_archived.iter()) {
            let archived =
                workspace.join("openspec/changes/archive").join(format!("2026-01-01-{name}"));
            std::fs::create_dir_all(&archived).unwrap();
            std::fs::write(archived.join("proposal.md"), "## Why\nfixture\n").unwrap();
            std::fs::write(archived.join("tasks.md"), "- [ ] x\n").unwrap();
        }

        (dir, workspace, remote)
    }

    fn remote_has_branch(workspace: &Path, branch: &str) -> bool {
        let out = Command::new("git")
            .args(["ls-remote", "--heads", "origin", branch])
            .current_dir(workspace)
            .output()
            .unwrap();
        out.status.success() && !out.stdout.is_empty()
    }

    fn local_has_branch(workspace: &Path, branch: &str) -> bool {
        let out = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .current_dir(workspace)
            .output()
            .unwrap();
        out.status.success()
    }

    /// 4.3(a): `--repo <selector>` picks the right repo from a multi-repo
    /// config. (b): `--hard` removes local AND remote agent branch.
    #[tokio::test]
    async fn hard_rewind_deletes_local_and_remote_via_selector() {
        let (_dir_a, ws_a, _remote_a) = rewind_fixture("feature-a", &[]);
        let (_dir_b, ws_b, _remote_b) = rewind_fixture("feature-b", &[]);

        let repos = vec![
            cfg_local("git@example.com:org/repo-a.git", &ws_a),
            cfg_local("git@example.com:org/repo-b.git", &ws_b),
        ];

        // Sanity pre-conditions on repo-b.
        assert!(local_has_branch(&ws_b, "agent-q"));
        assert!(remote_has_branch(&ws_b, "agent-q"));

        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        execute_with_io(
            repos,
            direct_push_github(),
            RewindArgs {
                changes: vec!["feature-b".to_string()],
                hard: true,
                repo: Some("repo-b".to_string()),
            },
            &mut input,
            &mut output,
        )
        .await
        .expect("rewind succeeds");

        // repo-b's agent-q must be gone locally AND remotely.
        assert!(!local_has_branch(&ws_b, "agent-q"),
            "local agent-q must be deleted");
        assert!(!remote_has_branch(&ws_b, "agent-q"),
            "remote agent-q must be deleted");
        // Unarchive happened.
        assert!(ws_b.join("openspec/changes/feature-b/proposal.md").is_file());

        // repo-a is untouched.
        assert!(local_has_branch(&ws_a, "agent-q"));
        assert!(remote_has_branch(&ws_a, "agent-q"));
        assert!(!ws_a.join("openspec/changes/feature-a/proposal.md").exists());
    }

    /// 4.3(c): partial unarchive failure surfaces as Err with both successful
    /// AND failed change names in the message.
    #[tokio::test]
    async fn partial_unarchive_failure_reports_both_lists() {
        let (_dir, ws, _remote) = rewind_fixture("feature-real", &[]);
        let repos = vec![cfg_local("git@example.com:org/single.git", &ws)];

        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err = execute_with_io(
            repos,
            direct_push_github(),
            RewindArgs {
                changes: vec!["feature-real".to_string(), "never-existed".to_string()],
                hard: true,
                repo: None, // single-repo config → selector optional
            },
            &mut input,
            &mut output,
        )
        .await
        .expect_err("partial failure must error");

        let msg = format!("{err:#}");
        // The succeeded one is listed.
        assert!(msg.contains("feature-real"), "succeeded should be listed: {msg}");
        // The failed one is listed.
        assert!(msg.contains("never-existed"), "failed should be listed: {msg}");
        assert!(msg.contains("partially failed"), "must call it partial: {msg}");

        // Despite the partial failure, the successful unarchive happened.
        assert!(ws.join("openspec/changes/feature-real").exists());
    }

    /// Soft rewind: deletes the local branch, leaves the remote alone, and
    /// is gated by the confirmation prompt.
    #[tokio::test]
    async fn soft_rewind_deletes_local_only_after_confirmation() {
        let (_dir, ws, _remote) = rewind_fixture("feature-x", &[]);
        let repos = vec![cfg_local("git@example.com:org/x.git", &ws)];

        let mut input = std::io::Cursor::new(b"y\n".to_vec());
        let mut output = Vec::<u8>::new();
        execute_with_io(
            repos,
            direct_push_github(),
            RewindArgs {
                changes: vec!["feature-x".to_string()],
                hard: false,
                repo: None,
            },
            &mut input,
            &mut output,
        )
        .await
        .expect("rewind succeeds");

        // Confirmation prompt was emitted.
        let prompt = String::from_utf8(output).unwrap();
        assert!(prompt.contains("delete branch 'agent-q'"), "prompt content: {prompt}");
        assert!(prompt.contains("feature-x"), "prompt should name changes: {prompt}");

        // Local agent-q is gone.
        assert!(!local_has_branch(&ws, "agent-q"));
        // Remote agent-q is preserved on soft rewind.
        assert!(remote_has_branch(&ws, "agent-q"),
            "soft rewind must NOT delete the remote branch");
        // Unarchive happened.
        assert!(ws.join("openspec/changes/feature-x").exists());
    }

    /// Soft rewind cancelled by declining: NO state change.
    #[tokio::test]
    async fn soft_rewind_declined_leaves_state_intact() {
        let (_dir, ws, _remote) = rewind_fixture("feature-y", &[]);
        let repos = vec![cfg_local("git@example.com:org/y.git", &ws)];

        let mut input = std::io::Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        execute_with_io(
            repos,
            direct_push_github(),
            RewindArgs {
                changes: vec!["feature-y".to_string()],
                hard: false,
                repo: None,
            },
            &mut input,
            &mut output,
        )
        .await
        .expect("decline returns Ok");

        // Everything is unchanged.
        assert!(local_has_branch(&ws, "agent-q"));
        assert!(remote_has_branch(&ws, "agent-q"));
        assert!(!ws.join("openspec/changes/feature-y").exists(),
            "decline must NOT unarchive");
        assert!(ws.join("openspec/changes/archive/2026-01-01-feature-y").exists(),
            "archived dir must still be in archive");
    }
}
