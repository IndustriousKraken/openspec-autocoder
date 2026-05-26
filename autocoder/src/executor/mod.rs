//! Backend-agnostic executor abstraction. autocoder invokes
//! implementations through this trait. The architecture-level spec lives at
//! `openspec/specs/executor/spec.md`; concrete backends are introduced by
//! per-change implementations (this phase: `claude_cli`).

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub mod claude_cli;

#[async_trait]
pub trait Executor: Send + Sync {
    /// Run the agent against `change` in `workspace`.
    ///
    /// Returns `SpecNeedsRevision` when one or more tasks in tasks.md
    /// require capabilities outside the executor's sandbox. The agent
    /// flags upfront, before starting implementation.
    async fn run(&self, workspace: &Path, change: &str) -> Result<ExecutorOutcome>;
    async fn resume(&self, handle: ResumeHandle, answer: &str) -> Result<ExecutorOutcome>;
    /// Re-invoke the agent against an already-archived `change` in
    /// `workspace`, passing the operator's revision text and the
    /// current PR diff as context. The default implementation calls
    /// `run`, so backends that have not yet been taught about revision
    /// mode degrade to a plain re-run; the production
    /// `ClaudeCliExecutor` overrides this to build a revision-mode
    /// prompt.
    async fn run_revision(
        &self,
        workspace: &Path,
        change: &str,
        revision_context: &crate::revisions::RevisionContext,
    ) -> Result<ExecutorOutcome> {
        let _ = revision_context;
        self.run(workspace, change).await
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecutorOutcome {
    /// The underlying agent reported successful completion of the change.
    /// autocoder decides what to do with a no-diff `Completed`.
    Completed,
    /// The agent has signaled ambiguity. autocoder persists the
    /// `resume_handle` to `.question.json`, posts the question to ChatOps,
    /// and unlocks the change.
    AskUser {
        question: String,
        resume_handle: ResumeHandle,
    },
    /// Unrecoverable failure. autocoder unlocks the change and does
    /// NOT archive it.
    Failed { reason: String },
    /// The agent inspected `tasks.md` and identified one or more tasks
    /// that require capabilities outside its sandbox. autocoder writes
    /// a `.needs-spec-revision.json` marker, posts a chatops alert under
    /// `AlertCategory::SpecNeedsRevision`, and halts the queue walk. The
    /// change is excluded from future `list_pending` calls until the
    /// operator deletes the marker.
    SpecNeedsRevision {
        unimplementable_tasks: Vec<UnimplementableTask>,
        revision_suggestion: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnimplementableTask {
    /// Task identifier from tasks.md, e.g. "5.2" or "13.1".
    pub task_id: String,
    /// The literal task text, quoted from tasks.md for the alert body.
    pub task_text: String,
    /// One-line reason the task is outside the agent's sandbox.
    pub reason: String,
}

impl PartialEq for ResumeHandle {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

/// Opaque payload passed between `run` and `resume`. JSON-serializable so
/// autocoder can persist it across daemon restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeHandle(pub serde_json::Value);
