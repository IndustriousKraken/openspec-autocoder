#![allow(clippy::all)]
use super::*;

mod support0;
pub(crate) use support0::*;
mod support1;
pub(crate) use support1::*;

/// ChatOps backend that records every threaded reply for assertion.
pub(crate) struct RecordingChatOps {
    replies: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ChatOpsBackend for RecordingChatOps {
    fn provider_name(&self) -> &'static str {
        "recording"
    }
    fn is_experimental(&self) -> bool {
        true
    }
    async fn post_question(&self, _: &str, _: &str, _: &str) -> Result<String> {
        unreachable!("triage handlers never post_question")
    }
    async fn poll_thread_for_human_reply(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<crate::chatops::HumanReply>> {
        Ok(None)
    }
    async fn post_notification(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn post_threaded_reply(&self, _: &str, _: &str, text: &str) -> Result<()> {
        self.replies.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

/// ChatOps backend that records every `post_notification` for assertion.
pub(crate) struct NotifRecordingChatOps {
    notifications: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ChatOpsBackend for NotifRecordingChatOps {
    fn provider_name(&self) -> &'static str {
        "notif-recording"
    }
    fn is_experimental(&self) -> bool {
        true
    }
    async fn post_question(&self, _: &str, _: &str, _: &str) -> Result<String> {
        unreachable!("the [out] gate never posts questions")
    }
    async fn poll_thread_for_human_reply(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<crate::chatops::HumanReply>> {
        Ok(None)
    }
    async fn post_notification(&self, _: &str, text: &str) -> Result<()> {
        self.notifications.lock().unwrap().push(text.to_string());
        Ok(())
    }
    async fn post_threaded_reply(&self, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
}

/// Executor that returns `Completed` and writes a file so
/// `git status --porcelain` is non-empty and a real commit gets made.
pub(crate) struct CompletingExecutorWithDiff {
    artifact_name: String,
    artifact_text: String,
}

#[async_trait::async_trait]
impl Executor for CompletingExecutorWithDiff {
    async fn run(&self, workspace: &Path, _c: &str) -> Result<ExecutorOutcome> {
        std::fs::write(workspace.join(&self.artifact_name), &self.artifact_text)?;
        Ok(ExecutorOutcome::Completed { final_answer: None })
    }
    async fn resume(&self, _h: crate::executor::ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
        unreachable!()
    }
}

/// Executor that returns `Completed` but writes nothing. Exercises the
/// "Completed but no diff" architecture scenario.
pub(crate) struct CompletingExecutorNoDiff;

#[async_trait::async_trait]
impl Executor for CompletingExecutorNoDiff {
    async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
        Ok(ExecutorOutcome::Completed { final_answer: None })
    }
    async fn resume(&self, _h: crate::executor::ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
        unreachable!()
    }
}

/// Executor that always returns `Failed`. Exercises the "backend failure"
/// architecture scenario.
pub(crate) struct AlwaysFailingExecutor;

#[async_trait::async_trait]
impl Executor for AlwaysFailingExecutor {
    async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
        Ok(ExecutorOutcome::Failed {
            reason: "fixture failure".into(),
        })
    }
    async fn resume(&self, _h: crate::executor::ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
        unreachable!()
    }
}

/// Pending-pass executor that returns `AskUser` on first invocation
/// and `Completed` (with a file write) on resume.
pub(crate) struct AskThenComplete {
    ws: std::path::PathBuf,
}

#[async_trait::async_trait]
impl Executor for AskThenComplete {
    async fn run(&self, _w: &Path, change: &str) -> Result<ExecutorOutcome> {
        Ok(ExecutorOutcome::AskUser {
            question: "What name should the file have?".to_string(),
            resume_handle: ResumeHandle(
                serde_json::json!({"change": change, "workspace": self.ws}),
            ),
        })
    }
    async fn resume(&self, _h: ResumeHandle, answer: &str) -> Result<ExecutorOutcome> {
        std::fs::write(self.ws.join("RESUME_ARTIFACT.txt"), answer.as_bytes())?;
        Ok(ExecutorOutcome::Completed { final_answer: None })
    }
}

/// Counting failing executor: increments a shared counter on every call,
/// fires a `Notify` so tests can await iteration completion event-driven,
/// always returns `Failed`.
pub(crate) struct CountingFailingExecutor {
    count: std::sync::atomic::AtomicUsize,
    invoked: Arc<tokio::sync::Notify>,
}

impl CountingFailingExecutor {
    fn new() -> Self {
        Self {
            count: std::sync::atomic::AtomicUsize::new(0),
            invoked: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

#[async_trait::async_trait]
impl Executor for CountingFailingExecutor {
    async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.invoked.notify_waiters();
        Ok(ExecutorOutcome::Failed {
            reason: "fixture".into(),
        })
    }
    async fn resume(&self, _h: crate::executor::ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
        unreachable!()
    }
}

/// Executor that returns `SpecNeedsRevision` with a fixed payload on
/// every `run`. Useful for asserting marker write + alert + queue halt.
pub(crate) struct SpecRevisionExecutor {
    tasks: Vec<UnimplementableTask>,
    suggestion: String,
    invocations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Executor for SpecRevisionExecutor {
    async fn run(&self, _w: &Path, _c: &str) -> Result<ExecutorOutcome> {
        self.invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ExecutorOutcome::SpecNeedsRevision {
            unimplementable_tasks: self.tasks.clone(),
            revision_suggestion: self.suggestion.clone(),
        })
    }
    async fn resume(&self, _h: crate::executor::ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
        unreachable!()
    }
}

/// Executor that writes a per-change file so every change produces a
/// distinct diff and can archive cleanly across iterations.
pub(crate) struct PerChangeArtifactExecutor;

#[async_trait::async_trait]
impl Executor for PerChangeArtifactExecutor {
    async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
        std::fs::write(
            workspace.join(format!("artifact-{change}.txt")),
            format!("contents for {change}\n"),
        )?;
        Ok(ExecutorOutcome::Completed { final_answer: None })
    }
    async fn resume(&self, _h: crate::executor::ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
        unreachable!()
    }
}

/// Executor that PANICS if invoked. Use this in collision tests to
/// assert the pre-flight filter ran and excluded the change before
/// any executor work happened.
pub(crate) struct UnreachableExecutorForCollision;

#[async_trait::async_trait]
impl Executor for UnreachableExecutorForCollision {
    async fn run(&self, _w: &Path, change: &str) -> Result<ExecutorOutcome> {
        unreachable!(
            "archive collision pre-flight must exclude `{change}` before the executor runs"
        );
    }
    async fn resume(&self, _h: crate::executor::ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
        unreachable!()
    }
}

/// Executor that records the order of `run` invocations into a shared
/// log and writes a unique artifact per change so each invocation
/// produces a real commit. Lets ordering tests assert the order of
/// `executor:<change>` and `audit:<type>` entries.
pub(crate) struct OrderRecordingExecutor {
    log: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Executor for OrderRecordingExecutor {
    async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome> {
        self.log.lock().unwrap().push(format!("executor:{change}"));
        // Produce a deterministic, change-scoped artifact so the
        // commit step has a non-empty diff and the change archives.
        let artifact_dir = workspace.join("openspec/changes").join(change);
        std::fs::create_dir_all(&artifact_dir)?;
        std::fs::write(
            artifact_dir.join("IMPL_NOTES.md"),
            format!("implementation for {change}\n"),
        )?;
        Ok(ExecutorOutcome::Completed { final_answer: None })
    }
    async fn resume(&self, _h: crate::executor::ResumeHandle, _a: &str) -> Result<ExecutorOutcome> {
        unreachable!()
    }
}

/// Test audit fixture used to assert iteration-ordering. Records each
/// invocation in a shared log under the slug `audit:<audit_type>` and
/// returns the configured outcome. When the outcome includes new
/// `openspec/changes/<name>/` directories, the audit commits them on
/// the agent branch so the post-hoc `OpenSpecOnly` enforcement passes.
pub(crate) struct OrderRecordingAudit {
    audit_type: &'static str,
    log: Arc<std::sync::Mutex<Vec<String>>>,
    creates_changes: Vec<String>,
    write_policy: crate::audits::WritePolicy,
}

#[async_trait::async_trait]
impl crate::audits::Audit for OrderRecordingAudit {
    fn audit_type(&self) -> &'static str {
        self.audit_type
    }
    fn description(&self) -> &'static str {
        "ordering-test audit fixture"
    }
    fn requires_head_change(&self) -> bool {
        false
    }
    fn write_policy(&self) -> crate::audits::WritePolicy {
        self.write_policy
    }
    async fn run(
        &self,
        ctx: &mut crate::audits::AuditContext<'_>,
    ) -> Result<crate::audits::AuditOutcome> {
        self.log
            .lock()
            .unwrap()
            .push(format!("audit:{}", self.audit_type));
        if self.creates_changes.is_empty() {
            return Ok(crate::audits::AuditOutcome::NoFindings);
        }
        // Create + commit each new openspec/changes/<name>/ directory
        // so the post-hoc `OpenSpecOnly` enforcement sees a clean
        // tree. This mirrors what real spec-writing audits do via
        // the `specs_writing` helper.
        for name in &self.creates_changes {
            let dir = ctx.workspace.join("openspec/changes").join(name);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("proposal.md"),
                format!("## Why\nfixture proposal {name}\n"),
            )?;
            std::fs::write(dir.join("tasks.md"), "- [ ] do thing\n")?;
        }
        let st = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(ctx.workspace)
            .status()?;
        anyhow::ensure!(st.success(), "git add failed in fixture audit");
        let subject = format!(
            "audit: {} proposals ({} change(s))",
            self.audit_type,
            self.creates_changes.len()
        );
        let st = std::process::Command::new("git")
            .args(["commit", "-q", "-m", &subject])
            .current_dir(ctx.workspace)
            .status()?;
        anyhow::ensure!(st.success(), "git commit failed in fixture audit");
        Ok(crate::audits::AuditOutcome::specs_written(
            self.creates_changes.clone(),
        ))
    }
}

mod t00;
mod t01;
mod t02;
mod t03;
mod t04;
mod t05;
mod t06;
mod t07;
mod t08;
mod t09;
mod t10;
mod t11;
mod t12;
mod t13;
mod t14;
mod t15;
mod t16;
mod t17;
mod t18;
mod t19;
mod t20;
mod t21;
