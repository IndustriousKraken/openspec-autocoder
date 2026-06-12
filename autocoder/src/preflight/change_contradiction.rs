//! Change-internal contradiction pre-flight check (a19; agentic transport a59).
//!
//! `a17`'s archivability check catches structural defects (MODIFIED title
//! missing from canonical, ADDED title already present). It does NOT
//! catch semantic defects — a change whose requirements are individually
//! well-formed AND archivable but contradict each other (ADDED A says
//! "all secrets in env vars"; ADDED B says "the API key in
//! `config.yaml`"). Pure-text logic cannot reliably detect this;
//! contradictions hide in domain language across multiple SHALL clauses.
//!
//! a59 migrated this check off the `LlmClient::complete` + stdout-JSON
//! transport onto a56's shared [`crate::agentic_run`] primitive: the check
//! runs a CLI-wrapped agentic session in a read-only sandbox (`Read`,
//! `Glob`, `Grep` — NO `Bash`/`Write`/`Edit`) with `ORCH_MCP_ROLE =
//! contradiction_check` AND the `submit_contradictions` MCP tool. The agent
//! reads the change's spec-delta files on demand AND returns its findings by
//! calling `submit_contradictions` instead of emitting JSON on stdout.
//!
//! The check is **fail-CLOSED by contract** (gatekeepers-fail-closed standard):
//! a session error (spawn, timeout, a resolved CLI strategy that is not
//! registered yet), a schema-rejected submission the agent never corrects, OR a
//! session that ends with no submission all log a WARN AND yield an `Errored`
//! outcome — NOT "no contradictions found." The `[in]` gate then HOLDS the change
//! in an explicit failed-to-run state (it was NOT evaluated). The hold is
//! enforced structurally by the default-deny verdict ledger
//! (verifier-gates-fail-closed): the gate's runner records `FAILED_TO_RUN`, and
//! the executor runs only when every blocking gate is `PASS`/`DISABLED`.

use crate::agentic_run::ResolvedModel;
use crate::verifier_gate::VerifierGate;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The MCP role AND submission routing key the contradiction check uses.
/// The per-execution MCP child advertises `submit_contradictions` ONLY when
/// `ORCH_MCP_ROLE` equals this value; the daemon-side schema validator is
/// registered under the same key (a56/a59).
pub const CONTRADICTION_CHECK_ROLE: &str = "contradiction_check";

/// Read-only CLI tool permissions for the contradiction-check sandbox. NO
/// `Bash`, NO `Write`, NO `Edit` — the agent reads the change's spec-delta
/// files on demand AND returns its findings through `submit_contradictions`.
pub const AGENTIC_CONTRADICTION_ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep"];

/// Wall-clock cap for one contradiction-check session. Mirrors the agentic
/// reviewer's bound (a58): the oneshot path had no analogous timeout (the
/// HTTP client owned it); this bounds the wrapped CLI subprocess.
const AGENTIC_CONTRADICTION_TIMEOUT: Duration = Duration::from_secs(900);

/// The full `--allowedTools` list the contradiction-check sandbox grants:
/// the read-only file tools PLUS the qualified `submit_contradictions` MCP
/// tool. Notably absent: `Bash`, `Write`, `Edit`. Exposed so tests can
/// assert the advertised surface.
pub fn agentic_contradiction_allowed_tools() -> Vec<String> {
    let mut tools: Vec<String> = AGENTIC_CONTRADICTION_ALLOWED_TOOLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(t) =
        crate::mcp_askuser_server::submission_tool_name_for_role(CONTRADICTION_CHECK_ROLE)
    {
        tools.push(crate::mcp_askuser_server::qualified_tool_name(t));
    }
    tools
}

/// Runtime context for the contradiction-check pre-flight.
///
/// a59: holds the agentic-transport pieces instead of an `LlmClient`. The
/// `model` tuple (a56) is translated into the wrapped CLI's model-selection
/// mechanism by the resolved [`crate::agentic_run::CliStrategy`]; its
/// `provider` also selects which CLI strategy runs. `command` is the wrapped
/// CLI binary (`executor.command`). `prompt_template` is the resolved prompt
/// body — either the embedded default OR the override file's contents.
///
/// Constructed once at daemon startup when the check is enabled. The polling
/// loop reads it on every iteration via [`current`].
pub struct ContradictionCheckCtx {
    /// Wrapped CLI binary the agentic session spawns (`executor.command`).
    pub command: String,
    /// Resolved `(provider, model, api_base_url, api_key)` tuple (a56). The
    /// `claude` strategy translates it into `ANTHROPIC_*`; its `provider`
    /// selects the CLI strategy (Anthropic → `claude` until a60).
    pub model: ResolvedModel,
    /// Resolved prompt body (embedded default OR override file contents).
    pub prompt_template: String,
    /// Redaction-safe `<provider>/<model>` attribution (a49) for the
    /// configured contradiction-check model. Surfaced as
    /// `*Contradiction-check: <provider>/<model>*` on the operator-facing
    /// findings alert. `None` only for test contexts built without a
    /// resolved config block.
    pub attribution: Option<String>,
    /// Bounded retry of the agentic session on a no-submission outcome
    /// (`executor.verifier_gate_retries`). Counts ADDITIONAL attempts; `0`
    /// is the historical single-attempt behavior. Only the flaky
    /// no-submission case retries — the gate still fails closed after the
    /// bound is exhausted (gatekeepers-fail-closed standard).
    pub retries: u32,
    /// Test-only injected `submit_contradictions` submission, bypassing the
    /// CLI subprocess AND the control socket. `Some(Some(p))` stands in for
    /// a recorded payload; `Some(None)` simulates "agent never submitted";
    /// `None` (default/production) uses the real CLI + `consume_submission`
    /// path.
    #[cfg(test)]
    pub test_submission: Option<Option<serde_json::Value>>,
}

tokio::task_local! {
    /// Per-task contradiction-check context. Set ONCE by [`scope`] at
    /// the top of the polling-task future; the polling loop reads it
    /// at each per-change pre-flight via [`current`]. Tests that do
    /// not call `scope` see `None`, so the global-state pollution
    /// problem from `OnceLock`-based designs does not apply.
    static CTX: Option<Arc<ContradictionCheckCtx>>;
}

/// Run `fut` with the given contradiction-check context bound for the
/// duration of the future. `None` represents the disabled state; the
/// polling loop's [`current`] reader returns `None` AND the check is a
/// no-op. Production callers (one per polling task) wrap the top-level
/// future once at startup.
pub fn scope<F>(ctx: Option<Arc<ContradictionCheckCtx>>, fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    CTX.scope(ctx, fut)
}

/// Snapshot of the current task's context. `None` when the operator
/// did not opt in OR the surrounding task did not call [`scope`].
/// Cheap clone of an `Arc`.
pub fn current() -> Option<Arc<ContradictionCheckCtx>> {
    CTX.try_with(|c| c.clone()).ok().flatten()
}

/// Default prompt template embedded at compile time. Overridable via
/// `executor.change_internal_contradiction_check_prompt_path`.
pub const EMBEDDED_PROMPT: &str =
    include_str!("../../../prompts/change-contradiction-check.md");

/// Resolve the prompt template. `None` returns the embedded default.
/// `Some(path)` reads the override file; an empty file (after `trim`) is
/// an error so the daemon does NOT feed an empty prompt to the session.
pub fn load_prompt_template(override_path: Option<&Path>) -> Result<String> {
    match override_path {
        None => Ok(EMBEDDED_PROMPT.to_string()),
        Some(path) => {
            let body = std::fs::read_to_string(path).with_context(|| {
                format!(
                    "reading change-contradiction-check prompt override at {}",
                    path.display()
                )
            })?;
            if body.trim().is_empty() {
                return Err(anyhow!(
                    "change-contradiction-check prompt override at {} is empty; refusing to feed an empty prompt to the session",
                    path.display()
                ));
            }
            Ok(body)
        }
    }
}

/// One contradiction surfaced by [`run_agentic_contradiction_check`].
/// Mirrors the `submit_contradictions` payload's entry shape one-for-one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContradictionFinding {
    pub requirement_a: String,
    pub requirement_b: String,
    pub summary: String,
}

/// One entry as it arrives in the `submit_contradictions` payload.
#[derive(Debug, Deserialize)]
struct RawContradiction {
    requirement_a: String,
    requirement_b: String,
    summary: String,
}

/// The `submit_contradictions` payload shape.
#[derive(Debug, Deserialize)]
struct RawContradictionSubmission {
    contradictions: Vec<RawContradiction>,
}

const PROMPT_DELIMITER: &str = "\n\n---\n\n";
const RESPONSE_EXCERPT_MAX: usize = 200;

/// Validate AND map a consumed `submit_contradictions` payload into
/// [`ContradictionFinding`]s (a59). This is BOTH the daemon-side schema
/// validator (registered via [`register_contradiction_submission_schema`]
/// with its `Ok` value discarded) AND the consume-time mapper — so a payload
/// that records successfully is exactly one that maps, and the two can never
/// drift (mirrors the advisory audits' `payload_to_findings` and the
/// reviewer's `payload_to_review_result`).
///
/// Returns `Err(reason)` (a correction-suitable string) when the payload is
/// missing the `contradictions` array, when it is not an array, OR when an
/// entry is missing a required field. `record_submission` surfaces the
/// reason to the agent as a correctable tool error.
pub(crate) fn payload_to_contradictions(
    payload: &serde_json::Value,
) -> std::result::Result<Vec<ContradictionFinding>, String> {
    let sub: RawContradictionSubmission =
        serde_json::from_value(payload.clone()).map_err(|e| {
            format!(
                "submit_contradictions: payload does not match the expected shape \
                 {{ contradictions: [{{ requirement_a, requirement_b, summary }}] }}: {e}"
            )
        })?;
    Ok(sub
        .contradictions
        .into_iter()
        .map(|c| ContradictionFinding {
            requirement_a: c.requirement_a,
            requirement_b: c.requirement_b,
            summary: c.summary,
        })
        .collect())
}

/// Register the contradiction check's `submit_contradictions` payload schema
/// (a59) with the daemon's submission store, under
/// [`CONTRADICTION_CHECK_ROLE`]. The validator IS [`payload_to_contradictions`]
/// with its `Ok` value discarded, so a payload that records successfully is
/// exactly one that maps. Called once at daemon startup alongside the
/// advisory audits' AND the reviewer's schema registration.
pub fn register_contradiction_submission_schema(
    store: &crate::submission_store::SubmissionStore,
) {
    store.register_schema(
        CONTRADICTION_CHECK_ROLE,
        Arc::new(|p: &serde_json::Value| payload_to_contradictions(p).map(|_| ())),
    );
}

/// Outcome of one contradiction-check session: the consumed submission (or
/// `None` when the agent recorded nothing valid) AND a truncated stdout
/// excerpt for the no-submission fail-open WARN.
struct ContradictionSessionOutcome {
    submission: Option<serde_json::Value>,
    stdout_excerpt: String,
}

impl crate::verifier_gate::SessionSubmission for ContradictionSessionOutcome {
    fn has_submission(&self) -> bool {
        self.submission.is_some()
    }
}

/// Abstracts "run ONE contradiction-check session AND drain its submission"
/// so the orchestration ([`run_agentic_contradiction_check_with_runner`]) is
/// unit-testable without spawning a CLI. Production is
/// [`CliContradictionSessionRunner`]; tests inject canned submissions.
#[async_trait]
trait ContradictionSessionRunner: Send + Sync {
    async fn run_session(&self, prompt: &str) -> Result<ContradictionSessionOutcome>;
}

/// Production session runner: writes the per-execution MCP config
/// (`ORCH_MCP_ROLE = contradiction_check`), runs the wrapped CLI through
/// [`crate::agentic_run::agentic_run`] in a read-only capture sandbox, AND
/// drains the stored submission via the control socket. Mirrors the agentic
/// reviewer's `CliReviewSessionRunner`.
struct CliContradictionSessionRunner<'a> {
    workspace: &'a Path,
    strategy: &'a dyn crate::agentic_run::CliStrategy,
    model: &'a ResolvedModel,
    settings_dir: Option<&'a Path>,
    timeout: Duration,
}

#[async_trait]
impl ContradictionSessionRunner for CliContradictionSessionRunner<'_> {
    async fn run_session(&self, prompt: &str) -> Result<ContradictionSessionOutcome> {
        // Write the per-execution MCP config advertising `submit_contradictions`.
        // `change == CONTRADICTION_CHECK_ROLE` keys the submission-store entry;
        // this runner consumes the same key after exit.
        crate::executor::claude_cli::ClaudeCliExecutor::write_mcp_config(
            self.workspace,
            CONTRADICTION_CHECK_ROLE,
            Some(CONTRADICTION_CHECK_ROLE),
        )
        .context("writing contradiction-check MCP config")?;

        // a70: a single-shot role — prune the session it creates on completion.
        let result = crate::agentic_run::agentic_run_with_session(
            crate::agentic_run::AgenticRunOpts {
            workspace: self.workspace,
            change: CONTRADICTION_CHECK_ROLE,
            strategy: self.strategy,
            prompt,
            sandbox: crate::agentic_run::SandboxConfig {
                allowed_tools: agentic_contradiction_allowed_tools(),
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
            // a006: read-only contradiction-check role — read-only workspace;
            // self-store from the resolved model's provider (task 2.5).
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

        let outcome = result.context("spawning contradiction-check subprocess")?;
        if outcome.timed_out {
            return Err(anyhow!(
                "contradiction-check session timed out after {}s",
                self.timeout.as_secs()
            ));
        }
        // Include stderr — opencode/agy write their real failure there, leaving
        // stdout empty, so a stdout-only excerpt is blank when it matters most.
        let stdout_excerpt = crate::agentic_run::failure_excerpt(&outcome, RESPONSE_EXCERPT_MAX);
        let submission =
            crate::audits::try_consume_submission(self.workspace, CONTRADICTION_CHECK_ROLE).await;
        Ok(ContradictionSessionOutcome {
            submission,
            stdout_excerpt,
        })
    }
}

/// Test-only session runner that stands in for the CLI + control socket:
/// returns a canned submission (`Some(payload)`) or `None` for the
/// no-submission case, with an empty stdout excerpt. Defined at module level
/// (not inside `mod tests`) so the `#[cfg(test)]` seam in
/// [`run_agentic_contradiction_check`] can construct it.
#[cfg(test)]
struct CannedContradictionRunner {
    submission: Option<serde_json::Value>,
}

#[cfg(test)]
#[async_trait]
impl ContradictionSessionRunner for CannedContradictionRunner {
    async fn run_session(&self, _prompt: &str) -> Result<ContradictionSessionOutcome> {
        Ok(ContradictionSessionOutcome {
            submission: self.submission.clone(),
            stdout_excerpt: String::new(),
        })
    }
}

/// Run the contradiction check for `change_slug` under `workspace_root`
/// (a59). Production entry point invoked from the polling loop's pre-flight.
///
/// Resolves the CLI strategy from the model's provider (a56); a provider
/// whose CLI has no registered strategy yet (a60) fails open here with a
/// WARN AND no subprocess is spawned. Otherwise runs one agentic session in
/// the read-only sandbox, drains the `submit_contradictions` submission, AND
/// maps it to findings.
///
/// Returns an empty `Vec` on EVERY fail-open path: strategy-not-registered,
/// session error (spawn/timeout), a never-corrected schema rejection, OR a
/// session that ends with no submission. WARN logs name the specific failure
/// so operators can investigate via journalctl.
/// Outcome of the `[in]` gate. The gate FAILS CLOSED: an inability to run is
/// `Errored`, NEVER `Clean` — see the project-documentation standard
/// "Control-plane gatekeepers fail closed". The caller holds the change on
/// `Errored` (it was not evaluated), blocks on `Found`, and proceeds on `Clean`.
#[derive(Debug)]
pub enum ContradictionCheckOutcome {
    /// Ran successfully; no contradictions. Proceed.
    Clean,
    /// Ran successfully; found contradictions. Block (needs revision).
    Found(Vec<ContradictionFinding>),
    /// Could NOT run (CLI unavailable, session error, no submission, or a
    /// re-map failure). Hold the change — never treat as `Clean`.
    Errored { cause: String },
}

pub async fn run_agentic_contradiction_check(
    ctx: &ContradictionCheckCtx,
    workspace_root: &Path,
    change_slug: &str,
) -> ContradictionCheckOutcome {
    // Test seam: an injected submission stands in for the CLI + control
    // socket so the orchestration is exercised without spawning a process.
    #[cfg(test)]
    if let Some(injected) = &ctx.test_submission {
        let runner = CannedContradictionRunner {
            submission: injected.clone(),
        };
        return run_agentic_contradiction_check_with_runner(
            ctx,
            workspace_root,
            change_slug,
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
            let label = VerifierGate::In.label();
            let cause = format!("CLI strategy unavailable: {e:#}");
            tracing::warn!(
                change = %change_slug,
                "{label} change-contradiction-check could not run ({cause}); holding the change (fail-closed)"
            );
            return ContradictionCheckOutcome::Errored { cause };
        }
    };
    let runner = CliContradictionSessionRunner {
        workspace: workspace_root,
        strategy: strategy.as_ref(),
        model: &ctx.model,
        settings_dir: None,
        timeout: AGENTIC_CONTRADICTION_TIMEOUT,
    };
    run_agentic_contradiction_check_with_runner(ctx, workspace_root, change_slug, &runner).await
}

/// Orchestration shared by production AND tests. Builds the prompt, runs one
/// session via `runner`, AND applies the fail-open policy uniformly: a
/// session error, a missing submission, OR a submission that fails re-mapping
/// each WARN AND yield an empty `Vec`.
async fn run_agentic_contradiction_check_with_runner(
    ctx: &ContradictionCheckCtx,
    workspace_root: &Path,
    change_slug: &str,
    runner: &dyn ContradictionSessionRunner,
) -> ContradictionCheckOutcome {
    let prompt = build_contradiction_prompt(&ctx.prompt_template, workspace_root, change_slug);
    // a61: every diagnostic this gate emits carries the `[in]` verifier-gate
    // label so it is attributable to the gate. The gate FAILS CLOSED: any
    // could-not-run path is `Errored` (the change is held), never `Clean`.
    let label = VerifierGate::In.label();
    // Bounded retry of the agentic session on the flaky no-submission case
    // (`executor.verifier_gate_retries`); a successful submission, a session
    // error, a timeout, AND an unregistered-strategy / CLI-unavailable error
    // are NOT retried. After the bound is exhausted the gate still fails
    // closed (gatekeepers-fail-closed standard).
    let session = crate::verifier_gate::run_session_with_retry(
        VerifierGate::In,
        change_slug,
        ctx.retries,
        || runner.run_session(&prompt),
    )
    .await;
    match session {
        Err(e) => {
            let cause = format!("session failed: {e:#}");
            tracing::warn!(
                change = %change_slug,
                "{label} change-contradiction-check could not run ({cause}); holding the change (fail-closed)"
            );
            ContradictionCheckOutcome::Errored { cause }
        }
        Ok(outcome) => match outcome.submission {
            None => {
                let cause = format!(
                    "session ended with no submit_contradictions submission (excerpt: {})",
                    outcome.stdout_excerpt
                );
                tracing::warn!(
                    change = %change_slug,
                    "{label} change-contradiction-check could not run ({cause}); holding the change (fail-closed)"
                );
                ContradictionCheckOutcome::Errored { cause }
            }
            Some(payload) => match payload_to_contradictions(&payload) {
                Ok(findings) if findings.is_empty() => ContradictionCheckOutcome::Clean,
                Ok(findings) => ContradictionCheckOutcome::Found(findings),
                Err(e) => {
                    // The payload passed `record_submission`'s validator, so a
                    // re-map failure is an internal invariant violation — hold
                    // (fail-closed), do NOT silently treat as clean.
                    let cause = format!("submission failed re-validation: {e}");
                    tracing::warn!(
                        change = %change_slug,
                        "{label} change-contradiction-check could not run ({cause}); holding the change (fail-closed)"
                    );
                    ContradictionCheckOutcome::Errored { cause }
                }
            },
        },
    }
}

/// Build the session prompt: the resolved template body, the change's
/// spec-delta file PATHS (the agent reads them on demand via `Read` —
/// contents are NOT inlined), AND the `submit_contradictions` instruction.
fn build_contradiction_prompt(
    template: &str,
    workspace_root: &Path,
    change_slug: &str,
) -> String {
    let paths = spec_delta_paths(workspace_root, change_slug);
    let mut out = String::new();
    out.push_str(template.trim_end());
    out.push_str(PROMPT_DELIMITER);
    out.push_str("# This change's spec-delta files\n\n");
    if paths.is_empty() {
        out.push_str(
            "(this change has no spec-delta files under \
             openspec/changes/<change>/specs/ — there is nothing to check)\n",
        );
    } else {
        out.push_str(
            "Read each of these files with the `Read` tool, then analyze them together for \
             internal contradictions:\n\n",
        );
        for p in &paths {
            out.push_str(&format!("- {p}\n"));
        }
    }
    out.push_str(
        "\nWhen your analysis is complete, call the `submit_contradictions` MCP tool exactly \
         once with `{ contradictions: [{ requirement_a, requirement_b, summary }] }` (an empty \
         array means \"no contradictions found\"). Do NOT print the result to stdout — the \
         daemon reads it ONLY from `submit_contradictions`.\n",
    );
    out
}

/// Enumerate every `openspec/changes/<change>/specs/<cap>/spec.md` path
/// (workspace-relative) for the change, sorted by capability. Returns an
/// empty `Vec` when the change has no `specs/` subdir or no per-capability
/// spec files. The path-listing form replaces a59's predecessor, which
/// concatenated file CONTENTS into the prompt; the agent now reads them on
/// demand via the read-only sandbox.
fn spec_delta_paths(workspace_root: &Path, change_slug: &str) -> Vec<String> {
    let specs_dir = workspace_root
        .join("openspec/changes")
        .join(change_slug)
        .join("specs");
    let Ok(read) = std::fs::read_dir(&specs_dir) else {
        return Vec::new();
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

    let mut out = Vec::new();
    for (cap_name, cap_path) in caps {
        if cap_path.join("spec.md").is_file() {
            out.push(format!(
                "openspec/changes/{change_slug}/specs/{cap_name}/spec.md"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmProvider;
    use tempfile::TempDir;

    /// Test runner that simulates a session error (spawn/timeout/strategy).
    struct ErrorContradictionRunner;

    #[async_trait]
    impl ContradictionSessionRunner for ErrorContradictionRunner {
        async fn run_session(&self, _prompt: &str) -> Result<ContradictionSessionOutcome> {
            Err(anyhow!("simulated session spawn error"))
        }
    }

    /// Test runner that plays back a SCRIPTED sequence of session submissions
    /// (one per call) AND counts invocations; the last entry repeats once the
    /// script is exhausted. Drives the shared retry-loop tests.
    struct ScriptedContradictionRunner {
        script: Vec<Option<serde_json::Value>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedContradictionRunner {
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

    #[async_trait]
    impl ContradictionSessionRunner for ScriptedContradictionRunner {
        async fn run_session(&self, _prompt: &str) -> Result<ContradictionSessionOutcome> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let idx = n.min(self.script.len().saturating_sub(1));
            Ok(ContradictionSessionOutcome {
                submission: self.script[idx].clone(),
                stdout_excerpt: String::new(),
            })
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

    fn test_ctx() -> ContradictionCheckCtx {
        ContradictionCheckCtx {
            command: "claude".into(),
            model: test_model(),
            prompt_template: "TEST_PROMPT".into(),
            attribution: None,
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

    // ---- payload_to_contradictions (the registered validator + mapper) ----

    #[test]
    fn empty_contradictions_array_maps_to_empty_vec() {
        let payload = serde_json::json!({ "contradictions": [] });
        let out = payload_to_contradictions(&payload).expect("empty array deserializes");
        assert!(out.is_empty());
    }

    #[test]
    fn single_contradiction_is_mapped() {
        let payload = serde_json::json!({
            "contradictions": [
                { "requirement_a": "A", "requirement_b": "B", "summary": "A and B cannot both hold" }
            ]
        });
        let out = payload_to_contradictions(&payload).expect("deserializes");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].requirement_a, "A");
        assert_eq!(out[0].requirement_b, "B");
        assert_eq!(out[0].summary, "A and B cannot both hold");
    }

    #[test]
    fn missing_contradictions_key_is_correctable_error() {
        let payload = serde_json::json!({ "results": [] });
        let err = payload_to_contradictions(&payload).expect_err("missing key must error");
        assert!(err.contains("contradictions"), "got: {err}");
    }

    #[test]
    fn non_array_contradictions_is_correctable_error() {
        let payload = serde_json::json!({ "contradictions": "not-an-array" });
        let err = payload_to_contradictions(&payload).expect_err("non-array must error");
        assert!(err.contains("contradictions"), "got: {err}");
    }

    #[test]
    fn entry_missing_field_is_correctable_error() {
        let payload = serde_json::json!({
            "contradictions": [ { "requirement_a": "A", "summary": "no b" } ]
        });
        let err =
            payload_to_contradictions(&payload).expect_err("missing required field must error");
        assert!(err.contains("submit_contradictions"), "got: {err}");
    }

    // ---- orchestration (run_agentic_contradiction_check_with_runner) ----

    /// A schema-valid non-empty submission is consumed into findings.
    #[tokio::test]
    async fn valid_submission_is_consumed_into_findings() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_change_spec(
            ws,
            "c1",
            "cap",
            "## ADDED Requirements\n\n### Requirement: A\nThe system SHALL a.\n",
        );
        let ctx = test_ctx();
        let runner = CannedContradictionRunner {
            submission: Some(serde_json::json!({
                "contradictions": [
                    { "requirement_a": "A", "requirement_b": "B", "summary": "x" }
                ]
            })),
        };
        let out =
            run_agentic_contradiction_check_with_runner(&ctx, ws, "c1", &runner).await;
        match out {
            ContradictionCheckOutcome::Found(f) => {
                assert_eq!(f.len(), 1);
                assert_eq!(f[0].requirement_a, "A");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    /// An empty submission is a CLEAN run (proceed-to-executor).
    #[tokio::test]
    async fn empty_submission_is_clean() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        let runner = CannedContradictionRunner {
            submission: Some(serde_json::json!({ "contradictions": [] })),
        };
        let out =
            run_agentic_contradiction_check_with_runner(&ctx, ws, "c1", &runner).await;
        assert!(
            matches!(out, ContradictionCheckOutcome::Clean),
            "empty submission is clean: {out:?}"
        );
    }

    /// A session that records NO submission FAILS CLOSED (Errored → held).
    #[tokio::test]
    async fn no_submission_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        let runner = CannedContradictionRunner { submission: None };
        let out =
            run_agentic_contradiction_check_with_runner(&ctx, ws, "c1", &runner).await;
        assert!(
            matches!(out, ContradictionCheckOutcome::Errored { .. }),
            "no submission must fail CLOSED (held): {out:?}"
        );
    }

    /// The fail-CLOSED diagnostics carry the `[verifier:in]` gate identifier so
    /// the held change is attributable to the gate that could not run.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn fail_closed_diagnostics_carry_the_in_gate_label() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        // A session that records no submission takes the fail-CLOSED hold path.
        let runner = CannedContradictionRunner { submission: None };
        let out =
            run_agentic_contradiction_check_with_runner(&ctx, ws, "c1", &runner).await;
        assert!(
            matches!(out, ContradictionCheckOutcome::Errored { .. }),
            "no submission fails CLOSED (held)"
        );
        assert!(
            logs_contain("[verifier:in]"),
            "the fail-closed WARN must carry the [verifier:in] gate identifier"
        );
    }

    /// A session error (spawn/timeout/strategy) FAILS CLOSED (Errored).
    #[tokio::test]
    async fn session_error_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let ctx = test_ctx();
        let out = run_agentic_contradiction_check_with_runner(
            &ctx,
            ws,
            "c1",
            &ErrorContradictionRunner,
        )
        .await;
        assert!(
            matches!(out, ContradictionCheckOutcome::Errored { .. }),
            "session error must fail CLOSED (held): {out:?}"
        );
    }

    // ---- bounded retry on the flaky no-submission case (shared seam) ----

    /// No submission on attempt 1, an empty (clean) submission on attempt 2 →
    /// the gate succeeds (Clean), not held. The flaky case is retried.
    #[tokio::test]
    async fn no_submission_then_clean_succeeds_on_retry() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.retries = 2;
        let runner = ScriptedContradictionRunner::new(vec![
            None,
            Some(serde_json::json!({ "contradictions": [] })),
        ]);
        let out = run_agentic_contradiction_check_with_runner(&ctx, ws, "c1", &runner).await;
        assert!(
            matches!(out, ContradictionCheckOutcome::Clean),
            "a retry that submits an empty result is Clean: {out:?}"
        );
        assert_eq!(runner.call_count(), 2, "exactly two attempts (1 retry)");
    }

    /// No submission on EVERY attempt → after `retries` retries the gate fails
    /// closed (Errored → held), invoked exactly `retries + 1` times.
    #[tokio::test]
    async fn no_submission_every_attempt_fails_closed_after_bound() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.retries = 2;
        let runner = ScriptedContradictionRunner::new(vec![None]);
        let out = run_agentic_contradiction_check_with_runner(&ctx, ws, "c1", &runner).await;
        assert!(
            matches!(out, ContradictionCheckOutcome::Errored { .. }),
            "exhausted retries must fail closed (held): {out:?}"
        );
        assert_eq!(runner.call_count(), 3, "retries(2) + 1 = 3 attempts");
    }

    /// `retries == 0` → exactly one attempt, fails closed on no submission
    /// (historical single-attempt behavior preserved).
    #[tokio::test]
    async fn zero_retries_is_one_attempt() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.retries = 0;
        let runner = ScriptedContradictionRunner::new(vec![None]);
        let out = run_agentic_contradiction_check_with_runner(&ctx, ws, "c1", &runner).await;
        assert!(matches!(out, ContradictionCheckOutcome::Errored { .. }));
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
        let runner = ScriptedContradictionRunner::new(vec![Some(serde_json::json!({
            "contradictions": [
                { "requirement_a": "A", "requirement_b": "B", "summary": "x" }
            ]
        }))]);
        let out = run_agentic_contradiction_check_with_runner(&ctx, ws, "c1", &runner).await;
        assert!(matches!(out, ContradictionCheckOutcome::Found(_)));
        assert_eq!(runner.call_count(), 1, "a submission on attempt 1 needs no retry");
    }

    /// A non-`claude` provider resolves to a CLI with no registered strategy
    /// (a60), so the production entry point FAILS CLOSED with no spawn.
    #[tokio::test]
    async fn unregistered_strategy_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let mut ctx = test_ctx();
        ctx.model.provider = LlmProvider::Ollama;
        ctx.command = "opencode".into();
        let out = run_agentic_contradiction_check(&ctx, ws, "c1").await;
        assert!(
            matches!(out, ContradictionCheckOutcome::Errored { .. }),
            "unregistered strategy must fail CLOSED (held): {out:?}"
        );
    }

    // ---- prompt construction ----

    #[tokio::test]
    async fn prompt_lists_every_capability_spec_path_and_submit_instruction() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        write_change_spec(
            ws,
            "c1",
            "alpha",
            "## ADDED Requirements\n\n### Requirement: A1\nBody.\n",
        );
        write_change_spec(
            ws,
            "c1",
            "beta",
            "## ADDED Requirements\n\n### Requirement: B1\nBody.\n",
        );
        let prompt = build_contradiction_prompt("PROMPT_TEMPLATE", ws, "c1");
        assert!(prompt.starts_with("PROMPT_TEMPLATE"));
        assert!(prompt.contains("openspec/changes/c1/specs/alpha/spec.md"));
        assert!(prompt.contains("openspec/changes/c1/specs/beta/spec.md"));
        assert!(
            prompt.contains("submit_contradictions"),
            "prompt must instruct the agent to call submit_contradictions"
        );
        // The agent reads files on demand — contents are NOT inlined.
        assert!(!prompt.contains("Requirement: A1"));
    }

    #[test]
    fn spec_delta_paths_empty_when_no_specs_dir() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("openspec/changes/c1")).unwrap();
        assert!(spec_delta_paths(ws, "c1").is_empty());
    }

    // ---- allowed-tools surface ----

    #[test]
    fn allowed_tools_are_read_only_plus_submit_contradictions() {
        let tools = agentic_contradiction_allowed_tools();
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&"Glob".to_string()));
        assert!(tools.contains(&"Grep".to_string()));
        assert!(
            !tools.iter().any(|t| t == "Bash" || t == "Write" || t == "Edit"),
            "sandbox must deny Bash/Write/Edit: {tools:?}"
        );
        assert!(
            tools.iter().any(|t| t.contains("submit_contradictions")),
            "submit_contradictions must be allowed: {tools:?}"
        );
    }

    // ---- prompt loader (unchanged behavior) ----

    #[test]
    fn embedded_prompt_template_is_non_empty() {
        assert!(!EMBEDDED_PROMPT.trim().is_empty(), "embedded template must not be empty");
        assert!(EMBEDDED_PROMPT.contains("contradictions"));
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
        let err =
            load_prompt_template(Some(&p)).expect_err("empty override must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains(p.display().to_string().as_str()));
        assert!(
            msg.contains("empty"),
            "error must name the empty condition; got: {msg}"
        );
    }

    #[test]
    fn load_prompt_template_missing_override_path_errors() {
        let p = Path::new("/nonexistent/path/to/template.md");
        let err = load_prompt_template(Some(p)).expect_err("missing path must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("/nonexistent/path/to/template.md"));
    }
}
