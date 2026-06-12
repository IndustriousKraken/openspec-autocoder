use super::*;

/// Init a throwaway git repo with a committed `src/bar.rs`, `README.md`
/// at base. Returns the temp-dir guard (drop = cleanup) and workspace.
pub(crate) fn dnsw_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let ws = dir.path().to_path_buf();
    let run = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?} failed");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/bar.rs"), "orig\n").unwrap();
    std::fs::write(ws.join("README.md"), "hi\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "base"]);
    (dir, ws)
}

pub(crate) fn recording_ctx(chatops: &Arc<RecordingChatOps>) -> ChatOpsContext {
    ChatOpsContext {
        chatops: chatops.clone(),
        channel: "C_TEST".to_string(),
        start_work_enabled: true,
        failure_alerts_enabled: true,
        pr_opened_enabled: true,
    }
}

pub(crate) fn triage_github_cfg() -> GithubConfig {
    GithubConfig {
        token_env: "X".into(),
        token: Some(crate::config::SecretSource::Inline {
            value: "inline-test-token".into(),
        }),
        owner_tokens: None,
        fork_owner: None,
        recreate_fork_on_reinit: false,
        command_authorization: Default::default(),
    }
}

pub(crate) fn audit_state() -> crate::audits::threads::AuditThreadState {
    crate::audits::threads::AuditThreadState {
        thread_ts: "T-audit".into(),
        channel: "C_TEST".into(),
        repo_url: "git@github.com:owner/fixture.git".into(),
        audit_type: "security_bug".into(),
        findings_excerpt: "FINDINGS".into(),
        posted_at: chrono::Utc::now(),
        status: crate::audits::threads::AuditThreadStatus::TriagePending,
        reason: None,
    }
}

pub(crate) fn proposal_state() -> crate::proposal_requests::ProposalRequestState {
    crate::proposal_requests::ProposalRequestState {
        request_id: "req-1".into(),
        repo_url: "git@github.com:owner/fixture.git".into(),
        channel: "C_TEST".into(),
        thread_ts: "T-chat".into(),
        ack_message_ts: "T-chat".into(),
        operator_user: "U_OP".into(),
        request_text: "add a /healthz endpoint".into(),
        submitted_at: chrono::Utc::now(),
        status: crate::proposal_requests::ProposalRequestStatus::TriagePending,
        reason: None,
    }
}

/// Write a fake spec change dir (mimics the executor's openspec write).
pub(crate) fn write_fake_spec(ws: &Path, slug: &str) {
    let dir = ws.join("openspec/changes").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("proposal.md"),
        "## Why\nfixture\n## What Changes\n- x\n## Impact\n- y\n",
    )
    .unwrap();
    std::fs::write(dir.join("tasks.md"), "- [ ] do the thing\n").unwrap();
}

/// Build a fixture remote repo with one commit on `main` AND a cloned
/// workspace whose `origin` points to the remote. Returns the temp dir
/// guard (drop = cleanup) plus the workspace path.
pub(crate) fn fixture_workspace_with_remote() -> (tempfile::TempDir, std::path::PathBuf) {
    use std::process::Command;
    fn run(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git {args:?} failed in {}",
            path.display()
        );
    }

    let dir = tempfile::TempDir::new().unwrap();
    let remote = dir.path().join("remote");
    let workspace = dir.path().join("workspace");

    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "-b", "main"]);
    run(&remote, &["config", "user.email", "test@example.com"]);
    run(&remote, &["config", "user.name", "test"]);
    std::fs::write(remote.join("README.md"), "hi\n").unwrap();
    run(&remote, &["add", "README.md"]);
    run(&remote, &["commit", "-q", "-m", "initial"]);

    let remote_url = remote.to_string_lossy().to_string();
    let parent = workspace.parent().unwrap();
    let status = Command::new("git")
        .args([
            "clone",
            "-q",
            &remote_url,
            workspace.to_string_lossy().as_ref(),
        ])
        .current_dir(parent)
        .status()
        .unwrap();
    assert!(status.success(), "clone failed");
    run(&workspace, &["config", "user.email", "test@example.com"]);
    run(&workspace, &["config", "user.name", "test"]);
    (dir, workspace)
}

/// Add an OpenSpec change with a known `## Why` line to a fixture
/// workspace and commit it locally so the working tree stays clean.
pub(crate) fn add_committed_change(workspace: &Path, name: &str, why_line: &str) {
    let dir = workspace.join("openspec/changes").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("proposal.md"), format!("## Why\n{why_line}\n")).unwrap();
    std::fs::write(dir.join("tasks.md"), "- [ ] do thing\n").unwrap();
    let st = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(st.success());
    let st = std::process::Command::new("git")
        .args(["commit", "-q", "-m", &format!("scaffold {name}")])
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(st.success());
}

pub(crate) fn notif_ctx(chatops: &std::sync::Arc<NotifRecordingChatOps>) -> ChatOpsContext {
    ChatOpsContext {
        chatops: chatops.clone(),
        channel: "C_TEST".to_string(),
        start_work_enabled: true,
        failure_alerts_enabled: true,
        pr_opened_enabled: true,
    }
}

/// Build a `[out]` gate ctx whose canned `test_submission` stands in for the
/// CLI session: `Some(payload)` is a recorded verdict, `None` simulates
/// "agent never submitted".
pub(crate) fn out_gate_ctx(
    submission: Option<serde_json::Value>,
) -> crate::code_implements_spec::CodeImplementsSpecCheckCtx {
    crate::code_implements_spec::CodeImplementsSpecCheckCtx {
        command: "claude".into(),
        model: crate::agentic_run::ResolvedModel {
            provider: crate::config::LlmProvider::Anthropic,
            model: "claude-test".into(),
            api_base_url: "https://example.invalid".into(),
            api_key: "sk-test".into(),
        },
        prompt_template: "T".into(),
        attribution: Some("anthropic/claude-test".into()),
        retries: 0,
        test_submission: Some(submission),
    }
}

/// Create the agent branch carrying a spec delta + a code change so the gate
/// has a non-empty diff AND a spec-delta path to reference.
pub(crate) fn seed_agent_branch_with_change(workspace: &Path) {
    fn run(path: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?} failed");
    }
    run(workspace, &["checkout", "-q", "-b", "agent-q"]);
    let spec = workspace.join("openspec/changes/c1/specs/cap/spec.md");
    std::fs::create_dir_all(spec.parent().unwrap()).unwrap();
    std::fs::write(
        &spec,
        "## ADDED Requirements\n\n### Requirement: A\nThe system SHALL a.\n",
    )
    .unwrap();
    std::fs::write(workspace.join("impl.rs"), "fn a() {}\n").unwrap();
    run(workspace, &["add", "-A"]);
    run(workspace, &["commit", "-q", "-m", "c1: implement"]);
}

pub(crate) fn init_bare(dir: &Path) {
    let st = std::process::Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .arg(dir)
        .status()
        .unwrap();
    assert!(st.success(), "bare init failed");
}

pub(crate) fn init_clone(remote: &Path, target: &Path) {
    let st = std::process::Command::new("git")
        .args([
            "clone",
            "-q",
            remote.to_string_lossy().as_ref(),
            target.to_string_lossy().as_ref(),
        ])
        .status()
        .unwrap();
    assert!(st.success(), "clone failed");
}

pub(crate) fn remote_url(workspace: &Path, name: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", name])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Build a `RepositoryConfig` pointing at a fixture workspace. Uses a
/// non-existent token env var so any attempt to open a PR errors fast
/// rather than reaching a live API.
pub(crate) fn fixture_repo(workspace: &Path) -> RepositoryConfig {
    RepositoryConfig { forge: None,
        url: "git@github.com:owner/fixture.git".into(),
        local_path: Some(workspace.to_path_buf()),
        base_branch: "main".into(),
        agent_branch: "agent-q".into(),
        poll_interval_sec: 60,
        chatops_channel_id: None,
        max_changes_per_pr: None,
        audits: None,
        spec_storage: None,
        upstream: None,
        auto_submit_pr: true,
        sandbox: None,
    }
}

/// Run a single pass through the commit step but skip push + PR. Tests
/// only need this when they want to verify commit/archive behavior
/// without an HTTP fixture for the GitHub API.
pub(crate) async fn run_one_pass_no_push(
    workspace: &Path,
    executor: &dyn Executor,
) -> Result<Vec<String>> {
    let (_td, paths) = crate::testing::test_daemon_paths();
    let repo = fixture_repo(workspace);
    let github_cfg = GithubConfig {
        token_env: "DOES_NOT_EXIST".into(),
        token: None,
        owner_tokens: None,
        fork_owner: None,
        recreate_fork_on_reinit: false,
        command_authorization: Default::default(),
    };
    // Use a very high threshold so existing tests' single-fail
    // iterations don't accidentally mark perma-stuck.
    let (processed, _self_heal) = run_pass_through_commits(
        &paths,
        workspace,
        &repo,
        &github_cfg,
        executor,
        None,
        u32::MAX,
        u32::MAX,
        &crate::audits::AuditRegistry::default(),
        None,
        &std::collections::HashMap::new(),
        &std::collections::HashSet::new(),
    )
    .await?;
    Ok(processed)
}

/// Build a ChatOps client wired against the given mockito server.
pub(crate) async fn fixture_chatops_for(server: &mut mockito::Server) -> Arc<dyn ChatOpsBackend> {
    let _ = server
        .mock("POST", "/auth.test")
        .with_status(200)
        .with_body(r#"{"ok":true,"user_id":"U_BOT"}"#)
        .create_async()
        .await;
    Arc::new(
        crate::chatops::SlackBackend::new_at(server.url(), "xoxb-fixture".into())
            .await
            .unwrap(),
    )
}

pub(crate) fn open_pr_test_repo() -> RepositoryConfig {
    RepositoryConfig { forge: None,
        url: "git@github.com:upstream-owner/upstream-repo.git".into(),
        local_path: None,
        base_branch: "main".into(),
        agent_branch: "agent-q".into(),
        poll_interval_sec: 60,
        chatops_channel_id: None,
        max_changes_per_pr: None,
        audits: None,
        spec_storage: None,
        upstream: None,
        auto_submit_pr: true,
        sandbox: None,
    }
}

pub(crate) fn open_pr_test_github(server_url: &str) -> GithubConfig {
    // Resolve_token reads from token_env (or inline). Use a fixture
    // env var unique to this test set so parallel tests don't clobber.
    unsafe { std::env::set_var("AUTOCODER_OPEN_PR_TEST_TOKEN", "testtoken") };
    let _ = server_url; // unused but kept for symmetry with future callers
    GithubConfig {
        token_env: "AUTOCODER_OPEN_PR_TEST_TOKEN".into(),
        token: None,
        owner_tokens: None,
        fork_owner: None,
        recreate_fork_on_reinit: false,
        command_authorization: Default::default(),
    }
}

/// Build a workspace whose `origin` URL points at a non-existent local
/// path so any `git push origin` fails — useful for simulating
/// `branch_push_failure`. The workspace basename is randomized via
/// `suffix` so the busy-marker path (which keys off workspace
/// basename) does not collide between parallel tests.
pub(crate) fn fixture_workspace_with_broken_remote(
    suffix: &str,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let (dir, ws) = fixture_workspace_with_remote();
    // Rename the workspace dir so its basename is unique per test.
    let renamed = ws.parent().unwrap().join(format!("workspace-{suffix}"));
    std::fs::rename(&ws, &renamed).unwrap();
    let ws = renamed;
    let bogus_push = dir.path().join("does-not-exist-push-target");
    let st = std::process::Command::new("git")
        .args([
            "remote",
            "set-url",
            "--push",
            "origin",
            &bogus_push.to_string_lossy(),
        ])
        .current_dir(&ws)
        .status()
        .unwrap();
    assert!(st.success());
    (dir, ws)
}

/// Write a fixture run-log file at the location
/// `<system-temp>/autocoder/logs/<workspace-basename>/<change>.log` so
/// `build_implementer_summary` can find it without invoking the
/// executor.
pub(crate) fn write_fixture_run_log(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    change: &str,
    prompt: &str,
    stdout: &str,
    stderr: &str,
) {
    let path = crate::executor::claude_cli::run_log_path(paths, workspace, change);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body = format!(
        "=== PROMPT ({p} bytes) ===\n{prompt}\n=== STDOUT ({n} bytes) ===\n{stdout}\n=== STDERR ({m} bytes) ===\n{stderr}\n",
        p = prompt.len(),
        n = stdout.len(),
        m = stderr.len(),
    );
    std::fs::write(&path, body).unwrap();
}

/// Make a workspace dir whose basename is unique per test so the
/// `<system-temp>/autocoder/logs/<basename>/` namespace does not
/// collide across parallel tests.
pub(crate) fn unique_workspace(suffix: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(&format!("autocoder-summary-{suffix}-"))
        .tempdir()
        .unwrap();
    dir
}

/// Write a fixture run-log in the new JSON-streaming shape
/// (PROMPT, ACTIONS, FINAL ANSWER, STDERR sections). Used by
/// tests that verify the PR-comment construction path reads from
/// FINAL ANSWER, not the action stream.
pub(crate) fn write_fixture_json_run_log(
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    change: &str,
    prompt: &str,
    actions_lines: &[&str],
    final_answer: &str,
    stderr: &str,
) {
    let path = crate::executor::claude_cli::run_log_path(paths, workspace, change);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = format!(
        "=== PROMPT ({p} bytes) ===\n{prompt}\n\n=== ACTIONS ===\n",
        p = prompt.len()
    );
    for line in actions_lines {
        body.push_str(line);
        body.push('\n');
    }
    body.push_str(&format!(
        "\n=== FINAL ANSWER ({n} bytes) ===\n{final_answer}\n\n=== STDERR ({m} bytes) ===\n{stderr}\n",
        n = final_answer.len(),
        m = stderr.len(),
    ));
    std::fs::write(&path, body).unwrap();
}

/// Run a single pass at the specified threshold and return its result.
/// Uses the existing remote fixture so the workspace's dirty-check
/// passes — perma-stuck logic exercises the same Failed paths.
pub(crate) async fn run_one_pass_with_threshold(
    paths: &DaemonPaths,
    workspace: &Path,
    executor: &dyn Executor,
    threshold: u32,
) -> Result<Vec<String>> {
    let repo = fixture_repo(workspace);
    let github_cfg = GithubConfig {
        token_env: "DOES_NOT_EXIST".into(),
        token: None,
        owner_tokens: None,
        fork_owner: None,
        recreate_fork_on_reinit: false,
        command_authorization: Default::default(),
    };
    let (processed, _) = run_pass_through_commits(
        paths,
        workspace,
        &repo,
        &github_cfg,
        executor,
        None,
        threshold,
        u32::MAX,
        &crate::audits::AuditRegistry::default(),
        None,
        &std::collections::HashMap::new(),
        &std::collections::HashSet::new(),
    )
    .await?;
    Ok(processed)
}

pub(crate) fn fixture_unimpl_tasks() -> Vec<UnimplementableTask> {
    vec![UnimplementableTask {
        task_id: "5.2".into(),
        task_text: "install actionlint locally".into(),
        reason: "no apt access".into(),
    }]
}

/// Write a canonical capability spec under
/// `openspec/specs/<cap>/spec.md` and commit it.
pub(crate) fn add_committed_canonical_spec(workspace: &Path, capability: &str, body: &str) {
    let dir = workspace.join("openspec/specs").join(capability);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("spec.md"), body).unwrap();
    let st = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(st.success());
    let st = std::process::Command::new("git")
        .args([
            "commit",
            "-q",
            "-m",
            &format!("scaffold canonical {capability}"),
        ])
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(st.success());
}

/// Write a change with a spec delta block and commit it.
pub(crate) fn add_committed_change_with_spec(
    workspace: &Path,
    name: &str,
    capability: &str,
    delta_body: &str,
) {
    let dir = workspace.join("openspec/changes").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("proposal.md"), format!("## Why\nfixture {name}\n")).unwrap();
    std::fs::write(dir.join("tasks.md"), "- [ ] do thing\n").unwrap();
    let spec_dir = dir.join("specs").join(capability);
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::write(spec_dir.join("spec.md"), delta_body).unwrap();
    let st = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(st.success());
    let st = std::process::Command::new("git")
        .args(["commit", "-q", "-m", &format!("scaffold {name}")])
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(st.success());
}

/// a59: build a contradiction-check context whose agentic session is
/// short-circuited by an injected `submit_contradictions` submission
/// (`Some(payload)`), a no-submission session (`None`), bypassing the
/// CLI subprocess AND the control socket entirely.
pub(crate) fn cc_test_ctx(
    submission: Option<serde_json::Value>,
    attribution: Option<String>,
) -> crate::preflight::change_contradiction::ContradictionCheckCtx {
    crate::preflight::change_contradiction::ContradictionCheckCtx {
        command: "claude".into(),
        model: crate::agentic_run::ResolvedModel {
            provider: crate::config::LlmProvider::Anthropic,
            model: "claude-test".into(),
            api_base_url: "https://example.invalid".into(),
            api_key: "sk-test".into(),
        },
        prompt_template: "TEST_PROMPT".into(),
        attribution,
        retries: 0,
        test_submission: Some(submission),
    }
}

/// a62: build a `[canon]`-gate context whose agentic session is
/// short-circuited by an injected `submit_canon_contradictions` submission
/// (`Some(payload)`), a no-submission session (`None`), bypassing the CLI
/// subprocess AND the control socket entirely.
pub(crate) fn canon_test_ctx(
    submission: Option<serde_json::Value>,
    attribution: Option<String>,
) -> crate::preflight::canon_contradiction::CanonContradictionCheckCtx {
    crate::preflight::canon_contradiction::CanonContradictionCheckCtx {
        command: "claude".into(),
        model: crate::agentic_run::ResolvedModel {
            provider: crate::config::LlmProvider::Anthropic,
            model: "claude-test".into(),
            api_base_url: "https://example.invalid".into(),
            api_key: "sk-test".into(),
        },
        prompt_template: "TEST_PROMPT".into(),
        attribution,
        retries: 0,
        test_submission: Some(submission),
    }
}
