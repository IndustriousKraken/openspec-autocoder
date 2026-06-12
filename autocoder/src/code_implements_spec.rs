//! Code-implements-spec verification — the `[out]` gate of the verifier
//! framework (a61; realized by a63).
//!
//! The code-reviewer deliberately reviews code QUALITY only and explicitly
//! defers spec-compliance: its prompt states "Do NOT assess whether the diff
//! implements the spec; that is handled separately by the verifier step."
//! This gate IS that deferred verifier step: a post-executor, advisory check
//! that judges — requirement by requirement, scenario by scenario — whether
//! the executor's implementation satisfies the change's spec delta.
//!
//! It is the `[out]` sibling of the pre-executor `[in]`/`[canon]` gates: same
//! agentic transport (a56 [`crate::agentic_run`] + a `submit_*` tool), same
//! opt-in posture. The check runs a CLI-wrapped agentic session in a read-only
//! sandbox (`Read`, `Glob`, `Grep` — NO `Bash`/`Write`/`Edit`) with
//! `ORCH_MCP_ROLE = code_implements_spec` AND the `submit_verdict` MCP tool.
//! The prompt carries the change's spec-delta file paths, the unified diff,
//! AND the changed-file list; the agent reads source on demand AND returns its
//! verdict by calling `submit_verdict`.
//!
//! Unlike the pre-executor gates (which are fail-OPEN — a gate failure must
//! never block the iteration), the `[out]` gate is **advisory**: it annotates
//! operator surfaces (a `## Spec Verification` PR-body section + a chatops note
//! only when gaps are found) AND never auto-acts. A gate failure (session
//! error, an unregistered CLI strategy, a schema-rejected submission never
//! corrected, OR no submission) logs a WARN carrying the `[verifier:out]`
//! label AND omits the section; it NEVER opens a revision AND NEVER blocks PR
//! creation. The no-block decision lives in the orchestrator-cli caller; this
//! module surfaces the distinction as [`SpecVerificationOutcome`].

use crate::agentic_run::ResolvedModel;
use crate::verifier_gate::VerifierGate;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The MCP role AND submission routing key the code-implements-spec check
/// uses. The per-execution MCP child advertises `submit_verdict` ONLY when
/// `ORCH_MCP_ROLE` equals this value; the daemon-side schema validator is
/// registered under the same key (a56/a63).
pub const CODE_IMPLEMENTS_SPEC_ROLE: &str = "code_implements_spec";

/// Read-only CLI tool permissions for the code-implements-spec sandbox. NO
/// `Bash`, NO `Write`, NO `Edit` — the agent reads the spec delta, the diff,
/// AND source on demand AND returns its verdict through `submit_verdict`.
pub const AGENTIC_CODE_IMPLEMENTS_SPEC_ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep"];

/// Wall-clock cap for one code-implements-spec session. Mirrors the
/// pre-executor gates' bound: the wrapped CLI subprocess is the thing being
/// bounded.
const AGENTIC_CODE_IMPLEMENTS_SPEC_TIMEOUT: Duration = Duration::from_secs(900);

/// The full `--allowedTools` list the code-implements-spec sandbox grants: the
/// read-only file tools PLUS the qualified `submit_verdict` MCP tool. Notably
/// absent: `Bash`, `Write`, `Edit`. Exposed so tests can assert the surface.
pub fn agentic_code_implements_spec_allowed_tools() -> Vec<String> {
    let mut tools: Vec<String> = AGENTIC_CODE_IMPLEMENTS_SPEC_ALLOWED_TOOLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(t) =
        crate::mcp_askuser_server::submission_tool_name_for_role(CODE_IMPLEMENTS_SPEC_ROLE)
    {
        tools.push(crate::mcp_askuser_server::qualified_tool_name(t));
    }
    tools
}

/// The verdict the agent returns: either the implementation satisfies the
/// change's spec delta, or one or more gaps were found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecVerdict {
    /// Every requirement AND scenario in the delta is satisfied.
    Implemented,
    /// One or more requirements/scenarios are unmet (see the `gaps`).
    GapsFound,
}

/// Whether a gap is wholly unimplemented or only partially honored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapStatus {
    /// The behavior is not implemented at all.
    Missing,
    /// Some of the behavior is implemented, but the requirement/scenario is
    /// not fully honored.
    Partial,
}

impl GapStatus {
    /// Operator-facing token (`missing` / `partial`).
    pub fn as_str(self) -> &'static str {
        match self {
            GapStatus::Missing => "missing",
            GapStatus::Partial => "partial",
        }
    }
}

/// One gap surfaced by the verifier: a requirement (AND optionally a scenario
/// under it) the implementation does not satisfy, with concrete evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecGap {
    pub requirement: String,
    pub scenario: Option<String>,
    pub status: GapStatus,
    pub evidence: String,
}

/// The consumed `submit_verdict` payload, mapped into the daemon's domain
/// type. `attribution` is stamped from the gate's configured model after the
/// session so the PR-body section can render `*Spec verification: …*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecVerification {
    pub verdict: SpecVerdict,
    pub summary: String,
    pub gaps: Vec<SpecGap>,
    /// Redaction-safe `<provider>/<model>` attribution (a49) for the gate's
    /// configured model. `None` in test contexts built without a resolved
    /// config block.
    pub attribution: Option<String>,
}

impl SpecVerification {
    /// Whether the verdict reports gaps. Drives the chatops heads-up (posted
    /// ONLY when gaps are found) in the orchestrator-cli caller.
    pub fn has_gaps(&self) -> bool {
        matches!(self.verdict, SpecVerdict::GapsFound)
    }
}

/// One gap entry as it arrives in the `submit_verdict` payload.
#[derive(Debug, Deserialize)]
struct RawSpecGap {
    requirement: String,
    #[serde(default)]
    scenario: Option<String>,
    status: String,
    evidence: String,
}

/// The `submit_verdict` payload shape.
#[derive(Debug, Deserialize)]
struct RawVerdictSubmission {
    verdict: String,
    summary: String,
    #[serde(default)]
    gaps: Vec<RawSpecGap>,
}

const PROMPT_DELIMITER: &str = "\n\n---\n\n";
const RESPONSE_EXCERPT_MAX: usize = 200;

/// Validate AND map a consumed `submit_verdict` payload into a
/// [`SpecVerification`] (a63). This is BOTH the daemon-side schema validator
/// (registered via [`register_code_implements_spec_submission_schema`] with
/// its `Ok` value discarded) AND the consume-time mapper — so a payload that
/// records successfully is exactly one that maps, and the two can never drift
/// (mirrors the reviewer's `payload_to_review_result`).
///
/// Returns `Err(reason)` (a correction-suitable string) when the `verdict` is
/// outside `{implemented, gaps_found}`, when a `gaps_found` verdict carries an
/// empty `gaps` array, when a gap's `status` is outside `{missing, partial}`,
/// OR when the payload does not match the expected shape. `record_submission`
/// surfaces the reason to the agent as a correctable tool error it can retry
/// in the same session.
pub(crate) fn payload_to_verification(
    payload: &serde_json::Value,
) -> std::result::Result<SpecVerification, String> {
    let sub: RawVerdictSubmission = serde_json::from_value(payload.clone()).map_err(|e| {
        format!(
            "submit_verdict: payload does not match the expected shape \
             {{ verdict: \"implemented\" | \"gaps_found\", summary: string, \
             gaps: [{{ requirement, scenario, status, evidence }}] }}: {e}"
        )
    })?;
    let verdict = match sub.verdict.as_str() {
        "implemented" => SpecVerdict::Implemented,
        "gaps_found" => SpecVerdict::GapsFound,
        other => {
            return Err(format!(
                "submit_verdict: verdict must be one of implemented | gaps_found; got `{other}`"
            ));
        }
    };
    let mut gaps: Vec<SpecGap> = Vec::with_capacity(sub.gaps.len());
    for (idx, g) in sub.gaps.into_iter().enumerate() {
        let status = match g.status.as_str() {
            "missing" => GapStatus::Missing,
            "partial" => GapStatus::Partial,
            other => {
                return Err(format!(
                    "submit_verdict: gaps[{idx}].status must be one of missing | partial; got `{other}`"
                ));
            }
        };
        gaps.push(SpecGap {
            requirement: g.requirement,
            scenario: g.scenario,
            status,
            evidence: g.evidence,
        });
    }
    // The schema's cross-field rule basic JSON Schema cannot express: a
    // gaps_found verdict MUST name at least one gap (per the executor
    // requirement). A schema-invalid payload is surfaced to the agent as a
    // correctable tool error it can retry in the same session.
    if matches!(verdict, SpecVerdict::GapsFound) && gaps.is_empty() {
        return Err(
            "submit_verdict: verdict `gaps_found` requires a non-empty `gaps` array; \
             pass the requirement/scenario gaps you found, or use verdict `implemented`"
                .to_string(),
        );
    }
    Ok(SpecVerification {
        verdict,
        summary: sub.summary,
        gaps,
        // Stamped by the caller from the gate's configured model.
        attribution: None,
    })
}

/// Register the gate's `submit_verdict` payload schema (a63) with the daemon's
/// submission store, under [`CODE_IMPLEMENTS_SPEC_ROLE`]. The validator IS
/// [`payload_to_verification`] with its `Ok` value discarded, so a payload
/// that records successfully is exactly one that maps. Called once at daemon
/// startup alongside the other gates' schema registration.
pub fn register_code_implements_spec_submission_schema(
    store: &crate::submission_store::SubmissionStore,
) {
    store.register_schema(
        CODE_IMPLEMENTS_SPEC_ROLE,
        Arc::new(|p: &serde_json::Value| payload_to_verification(p).map(|_| ())),
    );
}

/// Default prompt template embedded at compile time. Overridable via
/// `executor.code_implements_spec_check_prompt_path`.
pub const EMBEDDED_PROMPT: &str = include_str!("../../prompts/code-implements-spec-check.md");

/// Resolve the prompt template. `None` returns the embedded default.
/// `Some(path)` reads the override file; an empty file (after `trim`) is an
/// error so the daemon does NOT feed an empty prompt to the session.
pub fn load_prompt_template(override_path: Option<&Path>) -> Result<String> {
    match override_path {
        None => Ok(EMBEDDED_PROMPT.to_string()),
        Some(path) => {
            let body = std::fs::read_to_string(path).with_context(|| {
                format!(
                    "reading code-implements-spec-check prompt override at {}",
                    path.display()
                )
            })?;
            if body.trim().is_empty() {
                return Err(anyhow!(
                    "code-implements-spec-check prompt override at {} is empty; refusing to feed an empty prompt to the session",
                    path.display()
                ));
            }
            Ok(body)
        }
    }
}

/// Runtime context for the code-implements-spec `[out]` gate.
///
/// Holds the agentic-transport pieces (parallel to the pre-executor gates'
/// `*CheckCtx`). The `model` tuple (a56) is translated into the wrapped CLI's
/// model-selection mechanism by the resolved [`crate::agentic_run::CliStrategy`];
/// its `provider` also selects which CLI strategy runs. `command` is the
/// wrapped CLI binary (`executor.command`). `prompt_template` is the resolved
/// prompt body — either the embedded default OR the override file's contents.
///
/// Constructed once at daemon startup when the check is enabled. The polling
/// loop reads it on every iteration via [`current`].
pub struct CodeImplementsSpecCheckCtx {
    /// Wrapped CLI binary the agentic session spawns (`executor.command`).
    pub command: String,
    /// Resolved `(provider, model, api_base_url, api_key)` tuple (a56). The
    /// `claude` strategy translates it into `ANTHROPIC_*`; its `provider`
    /// selects the CLI strategy.
    pub model: ResolvedModel,
    /// Resolved prompt body (embedded default OR override file contents).
    pub prompt_template: String,
    /// Redaction-safe `<provider>/<model>` attribution (a49) for the
    /// configured model. Stamped onto the consumed verdict so the
    /// `## Spec Verification` PR-body section can render
    /// `*Spec verification: <provider>/<model>*`. `None` only for test
    /// contexts built without a resolved config block.
    pub attribution: Option<String>,
    /// Bounded retry of the agentic session on a no-submission outcome
    /// (`executor.verifier_gate_retries`). Counts ADDITIONAL attempts; `0`
    /// is the historical single-attempt behavior. Only the flaky
    /// no-submission case retries — the advisory gate still renders FAILED
    /// TO RUN after the bound is exhausted (gatekeepers-fail-closed standard).
    pub retries: u32,
    /// Test-only injected `submit_verdict` submission, bypassing the CLI
    /// subprocess AND the control socket. `Some(Some(p))` stands in for a
    /// recorded payload; `Some(None)` simulates "agent never submitted";
    /// `None` (default/production) uses the real CLI + `consume_submission`
    /// path.
    #[cfg(test)]
    pub test_submission: Option<Option<serde_json::Value>>,
}

tokio::task_local! {
    /// Per-task code-implements-spec context. Set ONCE by [`scope`] at the top
    /// of the polling-task future; the polling loop reads it post-executor via
    /// [`current`]. Tests that do not call `scope` see `None`, so there is no
    /// global-state pollution.
    static CTX: Option<Arc<CodeImplementsSpecCheckCtx>>;
}

/// Run `fut` with the given code-implements-spec context bound for the
/// duration of the future. `None` represents the disabled state; the polling
/// loop's [`current`] reader returns `None` AND the gate is a no-op.
/// Production callers (one per polling task) wrap the top-level future once at
/// startup.
pub fn scope<F>(
    ctx: Option<Arc<CodeImplementsSpecCheckCtx>>,
    fut: F,
) -> impl Future<Output = F::Output>
where
    F: Future,
{
    CTX.scope(ctx, fut)
}

/// Snapshot of the current task's context. `None` when the operator did not
/// opt in OR the surrounding task did not call [`scope`]. Cheap clone of an
/// `Arc`.
pub fn current() -> Option<Arc<CodeImplementsSpecCheckCtx>> {
    CTX.try_with(|c| c.clone()).ok().flatten()
}

/// Outcome of one code-implements-spec session, surfaced to the orchestrator-
/// cli caller. The advisory no-block decision lives in the caller; this enum
/// just distinguishes "we have a verdict to render" from "render nothing".
#[derive(Debug, Clone)]
pub enum SpecVerificationOutcome {
    /// A schema-valid verdict was consumed; render the `## Spec Verification`
    /// section (AND post a chatops note when [`SpecVerification::has_gaps`]).
    Verified(SpecVerification),
    /// The gate could NOT run (session error, unregistered strategy,
    /// never-corrected schema rejection, OR no submission). Advisory gates fail
    /// CLOSED to a VISIBLE state, not silence (gatekeepers-fail-closed standard):
    /// the caller renders an explicit `## Spec Verification: FAILED TO RUN`
    /// section — it still NEVER blocks PR creation — so an absent gate is
    /// distinguishable from one that ran AND found nothing. `cause` is the
    /// human-readable reason (also logged with the `[verifier:out]` label).
    FailedToRun { cause: String },
}

/// Outcome of one code-implements-spec session at the runner boundary: the
/// consumed submission (or `None` when the agent recorded nothing valid) AND a
/// truncated stdout excerpt for the no-submission WARN.
struct VerdictSessionOutcome {
    submission: Option<serde_json::Value>,
    stdout_excerpt: String,
}

impl crate::verifier_gate::SessionSubmission for VerdictSessionOutcome {
    fn has_submission(&self) -> bool {
        self.submission.is_some()
    }
}

/// Abstracts "run ONE code-implements-spec session AND drain its submission"
/// so the orchestration ([`run_code_implements_spec_check_with_runner`]) is
/// unit-testable without spawning a CLI. Production is
/// [`CliVerdictSessionRunner`]; tests inject canned submissions.
#[async_trait]
trait VerdictSessionRunner: Send + Sync {
    async fn run_session(&self, prompt: &str) -> Result<VerdictSessionOutcome>;
}

/// Production session runner: writes the per-execution MCP config
/// (`ORCH_MCP_ROLE = code_implements_spec`), runs the wrapped CLI through
/// [`crate::agentic_run::agentic_run`] in a read-only capture sandbox, AND
/// drains the stored submission via the control socket. Mirrors the reviewer's
/// `CliReviewSessionRunner`.
struct CliVerdictSessionRunner<'a> {
    workspace: &'a Path,
    strategy: &'a dyn crate::agentic_run::CliStrategy,
    model: &'a ResolvedModel,
    settings_dir: Option<&'a Path>,
    timeout: Duration,
}

#[async_trait]
impl VerdictSessionRunner for CliVerdictSessionRunner<'_> {
    async fn run_session(&self, prompt: &str) -> Result<VerdictSessionOutcome> {
        // Write the per-execution MCP config advertising `submit_verdict`.
        // `change == CODE_IMPLEMENTS_SPEC_ROLE` keys the submission-store
        // entry; this runner consumes the same key after exit.
        crate::executor::claude_cli::ClaudeCliExecutor::write_mcp_config(
            self.workspace,
            CODE_IMPLEMENTS_SPEC_ROLE,
            Some(CODE_IMPLEMENTS_SPEC_ROLE),
        )
        .context("writing code-implements-spec MCP config")?;

        // a70: a single-shot role — prune the session it creates on completion.
        let result = crate::agentic_run::agentic_run_with_session(
            crate::agentic_run::AgenticRunOpts {
            workspace: self.workspace,
            change: CODE_IMPLEMENTS_SPEC_ROLE,
            strategy: self.strategy,
            prompt,
            sandbox: crate::agentic_run::SandboxConfig {
                allowed_tools: agentic_code_implements_spec_allowed_tools(),
                disallowed_bash_patterns: Vec::new(),
                disallowed_read_paths: Vec::new(),
                deny_writes: true,
            },
            model: Some(self.model),
            output_mode: crate::agentic_run::OutputMode::Capture,
            timeout: self.timeout,
            paths: None,
            settings_dir: self.settings_dir,
            include_autocoder_tools: true,
            emit_stream_json_in_capture: false,
            resume_session_id: None,
            track_subprocess_marker: false,
            etxtbsy_retry_spawn: true,
            // a006: read-only verdict role — read-only workspace; self-store
            // derived from the resolved model's provider (task 2.5).
            os_sandbox: crate::sandbox::current_run_sandbox(
                crate::config::default_cli_for(self.model.provider),
                false,
            ),
            },
            true,
            None,
        )
        .await;

        // Always remove the config we wrote, regardless of run outcome.
        crate::executor::claude_cli::ClaudeCliExecutor::delete_mcp_config(self.workspace);

        let outcome = result.context("spawning code-implements-spec subprocess")?;
        if outcome.timed_out {
            return Err(anyhow!(
                "code-implements-spec session timed out after {}s",
                self.timeout.as_secs()
            ));
        }
        // Include stderr — opencode/agy write their real failure there, leaving
        // stdout empty, so a stdout-only excerpt is blank when it matters most.
        let stdout_excerpt = crate::agentic_run::failure_excerpt(&outcome, RESPONSE_EXCERPT_MAX);
        let submission =
            crate::audits::try_consume_submission(self.workspace, CODE_IMPLEMENTS_SPEC_ROLE).await;
        Ok(VerdictSessionOutcome {
            submission,
            stdout_excerpt,
        })
    }
}

/// Test-only session runner that stands in for the CLI + control socket:
/// returns a canned submission (`Some(payload)`) or `None` for the
/// no-submission case, with an empty stdout excerpt. Defined at module level
/// (not inside `mod tests`) so the `#[cfg(test)]` seam in
/// [`run_code_implements_spec_check`] can construct it.
#[cfg(test)]
struct CannedVerdictRunner {
    submission: Option<serde_json::Value>,
}

#[cfg(test)]
#[async_trait]
impl VerdictSessionRunner for CannedVerdictRunner {
    async fn run_session(&self, _prompt: &str) -> Result<VerdictSessionOutcome> {
        Ok(VerdictSessionOutcome {
            submission: self.submission.clone(),
            stdout_excerpt: String::new(),
        })
    }
}

/// Test-only runner that plays back a SCRIPTED sequence of session outcomes,
/// one per `run_session` call, AND counts invocations. Each scripted entry is
/// the session's `submission` (`Some(payload)` for a recorded submission,
/// `None` for the flaky no-submission case). When the script is exhausted, the
/// last entry repeats — so a single-element `[None]` script models "every
/// attempt fails to submit." Drives the retry-loop tests.
#[cfg(test)]
struct ScriptedVerdictRunner {
    script: Vec<Option<serde_json::Value>>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl ScriptedVerdictRunner {
    fn new(script: Vec<Option<serde_json::Value>>) -> Self {
        Self {
            script,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
#[async_trait]
impl VerdictSessionRunner for ScriptedVerdictRunner {
    async fn run_session(&self, _prompt: &str) -> Result<VerdictSessionOutcome> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let idx = n.min(self.script.len().saturating_sub(1));
        Ok(VerdictSessionOutcome {
            submission: self.script[idx].clone(),
            stdout_excerpt: String::new(),
        })
    }
}

/// Run the code-implements-spec `[out]` gate for `change_slugs` under
/// `workspace_root` (a63). Production entry point invoked from the polling
/// loop AFTER the executor implements the change(s), before PR-body assembly.
///
/// Resolves the CLI strategy from the model's provider (a56); a provider whose
/// CLI has no registered strategy yet returns [`SpecVerificationOutcome::FailedToRun`]
/// (a WARN, no subprocess spawned). Otherwise runs one agentic session in the
/// read-only sandbox, drains the `submit_verdict` submission, AND maps it to a
/// [`SpecVerification`] stamped with the gate's attribution.
///
/// EVERY failure path yields [`SpecVerificationOutcome::FailedToRun`]:
/// strategy-not-registered, session error (spawn/timeout), a never-corrected
/// schema rejection, OR a session that ends with no submission. The advisory
/// no-block policy is the caller's; this function never errors out the gate.
pub async fn run_code_implements_spec_check(
    ctx: &CodeImplementsSpecCheckCtx,
    workspace_root: &Path,
    change_slugs: &[String],
    diff: &str,
    changed_files: &[String],
) -> SpecVerificationOutcome {
    // Test seam: an injected submission stands in for the CLI + control socket
    // so the orchestration is exercised without spawning a process.
    #[cfg(test)]
    if let Some(injected) = &ctx.test_submission {
        let runner = CannedVerdictRunner {
            submission: injected.clone(),
        };
        return run_code_implements_spec_check_with_runner(
            ctx,
            workspace_root,
            change_slugs,
            diff,
            changed_files,
            &runner,
        )
        .await;
    }

    let strategy = match crate::agentic_run::strategy_for_provider(
        ctx.model.provider,
        ctx.command.clone(),
        Vec::new(),
    ) {
        Ok(s) => s,
        Err(e) => {
            let label = VerifierGate::Out.label();
            let cause = format!("CLI strategy unavailable: {e:#}");
            tracing::warn!(
                changes = %change_slugs.join(","),
                "{label} code-implements-spec could not run ({cause}); rendering FAILED TO RUN (advisory, never blocks)"
            );
            return SpecVerificationOutcome::FailedToRun { cause };
        }
    };
    let runner = CliVerdictSessionRunner {
        workspace: workspace_root,
        strategy: strategy.as_ref(),
        model: &ctx.model,
        settings_dir: None,
        timeout: AGENTIC_CODE_IMPLEMENTS_SPEC_TIMEOUT,
    };
    run_code_implements_spec_check_with_runner(
        ctx,
        workspace_root,
        change_slugs,
        diff,
        changed_files,
        &runner,
    )
    .await
}

/// Orchestration shared by production AND tests. Builds the prompt, runs one
/// session via `runner`, AND applies the advisory policy uniformly: a session
/// error, a missing submission, OR a submission that fails re-mapping each WARN
/// (labeled `[verifier:out]`) AND yield [`SpecVerificationOutcome::FailedToRun`].
/// A schema-valid submission is mapped AND stamped with the gate's attribution.
async fn run_code_implements_spec_check_with_runner(
    ctx: &CodeImplementsSpecCheckCtx,
    workspace_root: &Path,
    change_slugs: &[String],
    diff: &str,
    changed_files: &[String],
    runner: &dyn VerdictSessionRunner,
) -> SpecVerificationOutcome {
    let prompt = build_code_implements_spec_prompt(
        &ctx.prompt_template,
        workspace_root,
        change_slugs,
        diff,
        changed_files,
    );
    // a61: every advisory diagnostic this gate emits carries the `[out]`
    // verifier-gate label so the finding is attributable to the gate.
    let label = VerifierGate::Out.label();
    let changes = change_slugs.join(",");
    // Bounded retry of the agentic session on the flaky no-submission case
    // (`executor.verifier_gate_retries`); a successful submission, a session
    // error, a timeout, AND an unregistered-strategy / CLI-unavailable error
    // are NOT retried. After the bound is exhausted the advisory gate still
    // renders FAILED TO RUN (gatekeepers-fail-closed standard).
    let session = crate::verifier_gate::run_session_with_retry(
        VerifierGate::Out,
        &changes,
        ctx.retries,
        || runner.run_session(&prompt),
    )
    .await;
    match session {
        Err(e) => {
            let cause = format!("session failed: {e:#}");
            tracing::warn!(
                changes = %changes,
                "{label} code-implements-spec could not run ({cause}); rendering FAILED TO RUN (advisory, never blocks)"
            );
            SpecVerificationOutcome::FailedToRun { cause }
        }
        Ok(outcome) => match outcome.submission {
            None => {
                let cause = format!(
                    "session ended with no submit_verdict submission (excerpt: {})",
                    outcome.stdout_excerpt
                );
                tracing::warn!(
                    changes = %changes,
                    "{label} code-implements-spec could not run ({cause}); rendering FAILED TO RUN (advisory, never blocks)"
                );
                SpecVerificationOutcome::FailedToRun { cause }
            }
            Some(payload) => match payload_to_verification(&payload) {
                Ok(mut verification) => {
                    verification.attribution = ctx.attribution.clone();
                    SpecVerificationOutcome::Verified(verification)
                }
                Err(e) => {
                    // The payload passed `record_submission`'s validator, so a
                    // re-map failure is an internal invariant violation — the
                    // gate is advisory, so render FAILED TO RUN (visible, never
                    // blocks) rather than silently omit.
                    let cause = format!("submission failed re-validation: {e}");
                    tracing::warn!(
                        changes = %changes,
                        "{label} code-implements-spec could not run ({cause}); rendering FAILED TO RUN (advisory, never blocks)"
                    );
                    SpecVerificationOutcome::FailedToRun { cause }
                }
            },
        },
    }
}

/// Build the session prompt: the resolved template body, the change(s)'
/// spec-delta file PATHS (the agent reads them on demand via `Read` — contents
/// are NOT inlined), the changed-file PATH list, AND the unified diff inlined
/// for context, plus the `submit_verdict` instruction.
fn build_code_implements_spec_prompt(
    template: &str,
    workspace_root: &Path,
    change_slugs: &[String],
    diff: &str,
    changed_files: &[String],
) -> String {
    let delta_paths = spec_delta_paths(workspace_root, change_slugs);
    let mut out = String::new();
    out.push_str(template.trim_end());
    out.push_str(PROMPT_DELIMITER);

    out.push_str("# The change's spec-delta files\n\n");
    if delta_paths.is_empty() {
        out.push_str(
            "(this pass has no spec-delta files under \
             openspec/changes/<change>/specs/ — there is nothing to verify against)\n",
        );
    } else {
        out.push_str(
            "Read each of these files with the `Read` tool — they are the requirements AND \
             scenarios the implementation must satisfy:\n\n",
        );
        for p in &delta_paths {
            out.push_str(&format!("- {p}\n"));
        }
    }

    out.push_str("\n# Changed files\n\n");
    if changed_files.is_empty() {
        out.push_str("(no changed files reported for this pass)\n");
    } else {
        out.push_str(
            "These files were modified by the executor. Read whatever you need on demand with \
             `Read`, `Glob`, AND `Grep` to confirm the requirements are satisfied:\n\n",
        );
        for f in changed_files {
            out.push_str(&format!("- {f}\n"));
        }
    }

    out.push_str("\n# Unified diff\n\n");
    if diff.trim().is_empty() {
        out.push_str("(no diff produced this pass)\n");
    } else {
        out.push_str("```diff\n");
        out.push_str(diff);
        if !diff.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
    }

    out.push_str(
        "\nWhen your analysis is complete, call the `submit_verdict` MCP tool exactly once with \
         `{ verdict: \"implemented\" | \"gaps_found\", summary, gaps }` — a `gaps_found` verdict \
         MUST carry a non-empty `gaps` array. Do NOT print the result to stdout — the daemon reads \
         it ONLY from `submit_verdict`.\n",
    );
    out
}

/// Enumerate every `openspec/changes/<change>/specs/<cap>/spec.md` path
/// (workspace-relative) across `change_slugs`, sorted by `(change, capability)`.
/// Returns an empty `Vec` when no change has a `specs/` subdir with
/// per-capability spec files. The agent reads them on demand via the read-only
/// sandbox.
fn spec_delta_paths(workspace_root: &Path, change_slugs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for change_slug in change_slugs {
        let specs_dir = workspace_root
            .join("openspec/changes")
            .join(change_slug)
            .join("specs");
        let Ok(read) = std::fs::read_dir(&specs_dir) else {
            continue;
        };
        let mut caps: Vec<(String, PathBuf)> = Vec::new();
        for entry in read.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = entry.path();
            if path.is_dir() {
                caps.push((name, path));
            }
        }
        caps.sort_by(|a, b| a.0.cmp(&b.0));
        for (cap_name, cap_path) in caps {
            if cap_path.join("spec.md").is_file() {
                out.push(format!(
                    "openspec/changes/{change_slug}/specs/{cap_name}/spec.md"
                ));
            }
        }
    }
    out
}

/// Render the advisory `## Spec Verification` section for the FAIL-CLOSED case:
/// the gate could not run, so it reports FAILED TO RUN (NOT a pass) rather than
/// being silently omitted (gatekeepers-fail-closed standard — an advisory gate
/// fails to a VISIBLE state). The gate still never blocks PR creation; the
/// `cause` lets an operator distinguish an un-run gate from a clean verdict.
pub fn render_spec_verification_failed_section(cause: &str) -> String {
    format!(
        "## Spec Verification\n\nVerdict: FAILED TO RUN — the spec-verification gate could not evaluate this change, so it is NOT verified (this is NOT a pass).\n\nCause: {cause}\n\nThis gate is advisory, so PR creation was not blocked. Fix the gate (install/authenticate the configured CLI, or check the daemon control socket) to get a verdict on the next run.\n"
    )
}

/// Render the advisory `## Spec Verification` PR-body section from a consumed
/// verdict (a63). Returns the FULL section, starting with the `## Spec
/// Verification` heading, so the PR-assembly path appends it verbatim
/// (parallel to the reviewer's `## Code Review` block). An `implemented`
/// verdict reports the implementation as complete; a `gaps_found` verdict
/// lists each gap (`requirement`, optional `scenario`, `status`, `evidence`).
/// A `*Spec verification: <provider>/<model>*` attribution line (a49) is
/// appended when the gate carried a daemon-known model.
pub fn render_spec_verification_section(verification: &SpecVerification) -> String {
    let mut out = String::from("## Spec Verification\n\n");
    match verification.verdict {
        SpecVerdict::Implemented => {
            out.push_str("Verdict: implemented — the change's requirements and scenarios are satisfied.\n");
            if !verification.summary.trim().is_empty() {
                out.push_str(&format!("\n{}\n", verification.summary.trim()));
            }
        }
        SpecVerdict::GapsFound => {
            out.push_str("Verdict: gaps found — the implementation does not fully satisfy the change's spec delta.\n");
            if !verification.summary.trim().is_empty() {
                out.push_str(&format!("\n{}\n", verification.summary.trim()));
            }
            out.push_str("\n### Gaps\n");
            for gap in &verification.gaps {
                let scenario = match gap.scenario.as_deref() {
                    Some(s) if !s.trim().is_empty() => format!(" — scenario: {}", s.trim()),
                    _ => String::new(),
                };
                out.push_str(&format!(
                    "\n- **{}** ({}){}\n  {}\n",
                    gap.requirement.trim(),
                    gap.status.as_str(),
                    scenario,
                    gap.evidence.trim()
                ));
            }
        }
    }
    if let Some(attr) = verification.attribution.as_deref() {
        out.push('\n');
        out.push_str(&crate::attribution::attribution_line("Spec verification", attr));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmProvider;
    use tempfile::TempDir;

    /// Test runner that simulates a session error (spawn/timeout/strategy).
    struct ErrorVerdictRunner;

    #[async_trait]
    impl VerdictSessionRunner for ErrorVerdictRunner {
        async fn run_session(&self, _prompt: &str) -> Result<VerdictSessionOutcome> {
            Err(anyhow!("simulated session spawn error"))
        }
    }

    fn test_model() -> ResolvedModel {
        ResolvedModel {
            provider: LlmProvider::Anthropic,
            model: "claude-test".into(),
            api_base_url: "https://example.invalid".into(),
            api_key: "sk-test".into(),
        }
    }

    fn test_ctx() -> CodeImplementsSpecCheckCtx {
        CodeImplementsSpecCheckCtx {
            command: "claude".into(),
            model: test_model(),
            prompt_template: "TEST_PROMPT".into(),
            attribution: Some("anthropic/claude-test".into()),
            // Default to no retry so the canned-runner tests below run the
            // session exactly once; the retry behavior has its own tests.
            retries: 0,
            test_submission: None,
        }
    }

    fn write(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn write_change_spec(workspace: &Path, change: &str, capability: &str, body: &str) {
        write(
            &workspace
                .join("openspec/changes")
                .join(change)
                .join("specs")
                .join(capability)
                .join("spec.md"),
            body,
        );
    }

    // ---- payload_to_verification (the registered validator + mapper) ----

    #[test]
    fn implemented_verdict_with_empty_gaps_maps() {
        let payload = serde_json::json!({
            "verdict": "implemented",
            "summary": "all good",
            "gaps": []
        });
        let v = payload_to_verification(&payload).expect("implemented deserializes");
        assert_eq!(v.verdict, SpecVerdict::Implemented);
        assert!(v.gaps.is_empty());
        assert_eq!(v.summary, "all good");
    }

    #[test]
    fn implemented_verdict_allows_missing_gaps_field() {
        // `gaps` defaults to empty when absent.
        let payload = serde_json::json!({ "verdict": "implemented", "summary": "ok" });
        let v = payload_to_verification(&payload).expect("missing gaps defaults to empty");
        assert_eq!(v.verdict, SpecVerdict::Implemented);
        assert!(v.gaps.is_empty());
    }

    #[test]
    fn gaps_found_verdict_is_mapped() {
        let payload = serde_json::json!({
            "verdict": "gaps_found",
            "summary": "one requirement unmet",
            "gaps": [
                {
                    "requirement": "Does the thing",
                    "scenario": "When X then Y",
                    "status": "partial",
                    "evidence": "the Y branch is stubbed"
                },
                {
                    "requirement": "Other thing",
                    "scenario": null,
                    "status": "missing",
                    "evidence": "no code realizes it"
                }
            ]
        });
        let v = payload_to_verification(&payload).expect("gaps_found deserializes");
        assert_eq!(v.verdict, SpecVerdict::GapsFound);
        assert!(v.has_gaps());
        assert_eq!(v.gaps.len(), 2);
        assert_eq!(v.gaps[0].requirement, "Does the thing");
        assert_eq!(v.gaps[0].scenario.as_deref(), Some("When X then Y"));
        assert_eq!(v.gaps[0].status, GapStatus::Partial);
        assert_eq!(v.gaps[1].scenario, None);
        assert_eq!(v.gaps[1].status, GapStatus::Missing);
    }

    #[test]
    fn gaps_found_with_empty_gaps_is_correctable_error() {
        let payload = serde_json::json!({
            "verdict": "gaps_found",
            "summary": "x",
            "gaps": []
        });
        let err = payload_to_verification(&payload)
            .expect_err("gaps_found with empty gaps must error");
        assert!(err.contains("gaps_found"), "got: {err}");
        assert!(err.contains("non-empty"), "got: {err}");
    }

    #[test]
    fn verdict_outside_enum_is_correctable_error() {
        let payload = serde_json::json!({
            "verdict": "maybe",
            "summary": "x",
            "gaps": []
        });
        let err =
            payload_to_verification(&payload).expect_err("verdict outside enum must error");
        assert!(err.contains("verdict"), "got: {err}");
    }

    #[test]
    fn gap_status_outside_enum_is_correctable_error() {
        let payload = serde_json::json!({
            "verdict": "gaps_found",
            "summary": "x",
            "gaps": [
                { "requirement": "R", "status": "broken", "evidence": "e" }
            ]
        });
        let err =
            payload_to_verification(&payload).expect_err("status outside enum must error");
        assert!(err.contains("status"), "got: {err}");
    }

    #[test]
    fn gap_missing_required_field_is_correctable_error() {
        let payload = serde_json::json!({
            "verdict": "gaps_found",
            "summary": "x",
            "gaps": [ { "requirement": "R", "status": "missing" } ]
        });
        let err = payload_to_verification(&payload)
            .expect_err("gap missing evidence must error");
        assert!(err.contains("submit_verdict"), "got: {err}");
    }

    // ---- orchestration (run_code_implements_spec_check_with_runner) ----

    /// A schema-valid `implemented` submission is consumed into a verdict AND
    /// stamped with the gate's attribution.
    #[tokio::test]
    async fn implemented_submission_is_consumed_and_attributed() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        let runner = CannedVerdictRunner {
            submission: Some(serde_json::json!({
                "verdict": "implemented",
                "summary": "satisfied",
                "gaps": []
            })),
        };
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &["a.rs".to_string()],
            &runner,
        )
        .await;
        match out {
            SpecVerificationOutcome::Verified(v) => {
                assert_eq!(v.verdict, SpecVerdict::Implemented);
                assert!(!v.has_gaps());
                assert_eq!(v.attribution.as_deref(), Some("anthropic/claude-test"));
            }
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    /// A schema-valid `gaps_found` submission is consumed; `has_gaps()` drives
    /// the chatops heads-up in the caller.
    #[tokio::test]
    async fn gaps_found_submission_is_consumed() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        let runner = CannedVerdictRunner {
            submission: Some(serde_json::json!({
                "verdict": "gaps_found",
                "summary": "missing one",
                "gaps": [
                    { "requirement": "R", "scenario": null, "status": "missing", "evidence": "e" }
                ]
            })),
        };
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &runner,
        )
        .await;
        match out {
            SpecVerificationOutcome::Verified(v) => {
                assert!(v.has_gaps());
                assert_eq!(v.gaps.len(), 1);
            }
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    /// A session that records NO submission is advisory-unavailable (no
    /// section), never an error.
    #[tokio::test]
    async fn no_submission_is_unavailable() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        let runner = CannedVerdictRunner { submission: None };
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &runner,
        )
        .await;
        assert!(
            matches!(out, SpecVerificationOutcome::FailedToRun { .. }),
            "no submission must be Unavailable: {out:?}"
        );
    }

    /// a61 (task 2.4 / 4.5): a no-submission session is advisory-unavailable
    /// AND its emitted diagnostics carry the `[verifier:out]` gate identifier.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn unavailable_diagnostics_carry_the_out_gate_label() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        let runner = CannedVerdictRunner { submission: None };
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &runner,
        )
        .await;
        assert!(matches!(out, SpecVerificationOutcome::FailedToRun { .. }));
        assert!(
            logs_contain("[verifier:out]"),
            "the advisory WARN must carry the [verifier:out] gate identifier"
        );
    }

    /// A session error is advisory-unavailable (never blocks).
    #[tokio::test]
    async fn session_error_is_unavailable() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &ErrorVerdictRunner,
        )
        .await;
        assert!(
            matches!(out, SpecVerificationOutcome::FailedToRun { .. }),
            "session error must be Unavailable: {out:?}"
        );
    }

    // ---- bounded retry on the flaky no-submission case (shared seam) ----

    fn implemented_payload() -> serde_json::Value {
        serde_json::json!({ "verdict": "implemented", "summary": "ok", "gaps": [] })
    }

    /// No submission on attempt 1, a valid submission on attempt 2 → the gate
    /// succeeds (Verified), no FAILED TO RUN. The flaky case is retried.
    #[tokio::test]
    async fn no_submission_then_valid_succeeds_on_retry() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.retries = 2;
        let runner = ScriptedVerdictRunner::new(vec![None, Some(implemented_payload())]);
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &runner,
        )
        .await;
        assert!(
            matches!(out, SpecVerificationOutcome::Verified(_)),
            "a retry that submits must yield Verified: {out:?}"
        );
        assert_eq!(runner.call_count(), 2, "exactly two attempts (1 retry)");
    }

    /// No submission on EVERY attempt → after `retries` retries the gate fails
    /// closed (FailedToRun), and the runner was invoked exactly `retries + 1`
    /// times.
    #[tokio::test]
    async fn no_submission_every_attempt_fails_closed_after_bound() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.retries = 2;
        let runner = ScriptedVerdictRunner::new(vec![None]);
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &runner,
        )
        .await;
        assert!(
            matches!(out, SpecVerificationOutcome::FailedToRun { .. }),
            "exhausted retries must fail closed: {out:?}"
        );
        assert_eq!(runner.call_count(), 3, "retries(2) + 1 = 3 attempts");
    }

    /// `retries == 0` → exactly one attempt, fails closed on no submission
    /// (the historical single-attempt behavior is preserved).
    #[tokio::test]
    async fn zero_retries_is_one_attempt() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.retries = 0;
        let runner = ScriptedVerdictRunner::new(vec![None]);
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &runner,
        )
        .await;
        assert!(
            matches!(out, SpecVerificationOutcome::FailedToRun { .. }),
            "no submission with retries=0 fails closed: {out:?}"
        );
        assert_eq!(runner.call_count(), 1, "retries=0 means exactly one attempt");
    }

    /// A valid submission on attempt 1 → exactly one attempt (no needless
    /// retry), even with a non-zero retry bound.
    #[tokio::test]
    async fn valid_first_attempt_does_not_retry() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.retries = 2;
        let runner = ScriptedVerdictRunner::new(vec![Some(implemented_payload())]);
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &runner,
        )
        .await;
        assert!(matches!(out, SpecVerificationOutcome::Verified(_)));
        assert_eq!(runner.call_count(), 1, "a submission on attempt 1 needs no retry");
    }

    /// A session ERROR is NOT retried (a timeout would just time out again; an
    /// unregistered-strategy / CLI-unavailable error is config-level). The
    /// error short-circuits on the first attempt.
    #[tokio::test]
    async fn session_error_is_not_retried() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.retries = 5;
        let out = run_code_implements_spec_check_with_runner(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
            &ErrorVerdictRunner,
        )
        .await;
        assert!(
            matches!(out, SpecVerificationOutcome::FailedToRun { .. }),
            "a session error fails closed: {out:?}"
        );
    }

    /// A non-`claude` provider resolves to a CLI with no registered strategy,
    /// so the production entry point is advisory-unavailable with no spawn.
    #[tokio::test]
    async fn unregistered_strategy_is_unavailable() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.model.provider = LlmProvider::Ollama;
        ctx.command = "definitely-not-a-registered-cli".into();
        let out = run_code_implements_spec_check(
            &ctx,
            ws,
            &["c1".to_string()],
            "diff",
            &[],
        )
        .await;
        assert!(
            matches!(out, SpecVerificationOutcome::FailedToRun { .. }),
            "unregistered strategy must be Unavailable: {out:?}"
        );
    }

    // ---- prompt construction ----

    #[tokio::test]
    async fn prompt_carries_delta_paths_diff_and_submit_instruction() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_change_spec(
            ws,
            "c1",
            "alpha",
            "## ADDED Requirements\n\n### Requirement: A1\nBody.\n",
        );
        let prompt = build_code_implements_spec_prompt(
            "PROMPT_TEMPLATE",
            ws,
            &["c1".to_string()],
            "diff --git a/a.rs b/a.rs\n+fn x() {}",
            &["a.rs".to_string()],
        );
        assert!(prompt.starts_with("PROMPT_TEMPLATE"));
        // The change's delta path is listed (read on demand, not inlined).
        assert!(prompt.contains("openspec/changes/c1/specs/alpha/spec.md"));
        assert!(!prompt.contains("Requirement: A1"));
        // The changed-file list AND the diff are carried.
        assert!(prompt.contains("a.rs"));
        assert!(prompt.contains("diff --git a/a.rs b/a.rs"));
        // The submit instruction.
        assert!(
            prompt.contains("submit_verdict"),
            "prompt must instruct the agent to call submit_verdict"
        );
    }

    #[test]
    fn spec_delta_paths_spans_multiple_changes_sorted() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_change_spec(ws, "c1", "alpha", "body");
        write_change_spec(ws, "c2", "beta", "body");
        let paths = spec_delta_paths(ws, &["c1".to_string(), "c2".to_string()]);
        assert_eq!(
            paths,
            vec![
                "openspec/changes/c1/specs/alpha/spec.md".to_string(),
                "openspec/changes/c2/specs/beta/spec.md".to_string(),
            ]
        );
    }

    #[test]
    fn spec_delta_paths_empty_when_no_specs_dir() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/c1")).unwrap();
        assert!(spec_delta_paths(ws, &["c1".to_string()]).is_empty());
    }

    // ---- section rendering ----

    #[test]
    fn render_implemented_section_has_heading_and_attribution() {
        let v = SpecVerification {
            verdict: SpecVerdict::Implemented,
            summary: "checked everything".into(),
            gaps: Vec::new(),
            attribution: Some("anthropic/claude-test".into()),
        };
        let section = render_spec_verification_section(&v);
        assert!(section.starts_with("## Spec Verification"));
        assert!(section.contains("checked everything"));
        // The attribution line (a49).
        assert!(section.contains("*Spec verification: anthropic/claude-test*"));
    }

    #[test]
    fn render_gaps_section_lists_each_gap() {
        let v = SpecVerification {
            verdict: SpecVerdict::GapsFound,
            summary: "two gaps".into(),
            gaps: vec![
                SpecGap {
                    requirement: "Req One".into(),
                    scenario: Some("Scenario A".into()),
                    status: GapStatus::Partial,
                    evidence: "branch stubbed".into(),
                },
                SpecGap {
                    requirement: "Req Two".into(),
                    scenario: None,
                    status: GapStatus::Missing,
                    evidence: "no code".into(),
                },
            ],
            attribution: None,
        };
        let section = render_spec_verification_section(&v);
        assert!(section.starts_with("## Spec Verification"));
        // Each gap's requirement, status, scenario, AND evidence appear.
        assert!(section.contains("Req One"));
        assert!(section.contains("partial"));
        assert!(section.contains("Scenario A"));
        assert!(section.contains("branch stubbed"));
        assert!(section.contains("Req Two"));
        assert!(section.contains("missing"));
        assert!(section.contains("no code"));
        // No attribution line when the gate carried no model.
        assert!(!section.contains("*Spec verification:"));
    }

    // ---- allowed-tools surface ----

    #[test]
    fn allowed_tools_are_read_only_plus_submit_verdict() {
        let tools = agentic_code_implements_spec_allowed_tools();
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&"Glob".to_string()));
        assert!(tools.contains(&"Grep".to_string()));
        assert!(
            !tools.iter().any(|t| t == "Bash" || t == "Write" || t == "Edit"),
            "sandbox must deny Bash/Write/Edit: {tools:?}"
        );
        assert!(
            tools.iter().any(|t| t.contains("submit_verdict")),
            "submit_verdict must be allowed: {tools:?}"
        );
    }

    // ---- prompt loader ----

    #[test]
    fn embedded_prompt_template_is_non_empty() {
        assert!(!EMBEDDED_PROMPT.trim().is_empty(), "embedded template must not be empty");
        assert!(EMBEDDED_PROMPT.contains("submit_verdict"));
    }

    #[test]
    fn load_prompt_template_none_returns_embedded() {
        let body = load_prompt_template(None).unwrap();
        assert_eq!(body, EMBEDDED_PROMPT);
    }

    #[test]
    fn load_prompt_template_some_reads_override_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("custom.md");
        std::fs::write(&p, "CUSTOM_TEMPLATE_BODY").unwrap();
        let body = load_prompt_template(Some(&p)).unwrap();
        assert_eq!(body, "CUSTOM_TEMPLATE_BODY");
    }

    #[test]
    fn load_prompt_template_empty_override_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("empty.md");
        std::fs::write(&p, "   \n\n  ").unwrap();
        let err = load_prompt_template(Some(&p)).expect_err("empty override must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains(p.display().to_string().as_str()));
        assert!(msg.contains("empty"), "error must name the empty condition; got: {msg}");
    }
}
