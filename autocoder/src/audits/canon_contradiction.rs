//! Canon-internal contradiction audit (a75). Scans the whole canonical
//! spec set for pairs of requirements that cannot both hold AND reports
//! them advisorily so a maintainer can heal them on purpose.
//!
//! This completes the contradiction family: `within a change` (a59),
//! `change vs canon` (a62), and — here — `canon vs canon`. The two
//! pre-flight gates only catch *new* work; a contradiction that already
//! lives inside the canon (landed before the gates existed, or slipped
//! past them) sits silently until an implementer is handed
//! mutually-impossible instructions. This periodic audit surfaces it.
//!
//! Like `drift_audit`, it invokes the wrapped agent CLI with a read-only
//! sandbox plus a per-role `submit_*` MCP tool (a56) and an embedded
//! prompt. Distinct from `drift`:
//!   - The sandbox excludes `Bash` (read-only `Read`/`Glob`/`Grep` only).
//!   - The agent returns findings via `submit_canon_internal_contradictions`
//!     (a symmetric capability+title pair on both sides), not
//!     `submit_findings`.
//!   - A session that records NO submission consumes as an empty result
//!     (a clean canon), NOT an audit failure — the audit is advisory.
//!   - RAG-assisted detection: when a21's canonical-spec RAG is enabled
//!     the agent retrieves the nearest requirements per requirement via
//!     `query_canonical_specs`; when it is off the audit degrades to a
//!     best-effort direct read AND logs the degradation.
//!   - Re-report suppression: reported pairs are persisted (keyed by an
//!     order-independent capability+title pair plus a content hash of each
//!     requirement) so an unhealed contradiction does not re-spam chatops,
//!     while an edited pair re-surfaces and a healed pair is pruned. The
//!     suppression state is daemon bookkeeping, so it lives under
//!     `<state_dir>/canon-contradiction-state/<workspace-basename>.json`
//!     (resolved via `DaemonPaths`), NOT in the managed repo's workspace —
//!     per the canonical "Workspaces, markers, and state move to standard
//!     locations" requirement.
//!
//! `requires_head_change = true` — a canon contradiction can only emerge
//! when the canon changes. `WritePolicy::None` — the audit never writes
//! the canon; the maintainer heals via the existing audit-thread `send it`.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    Audit, AuditContext, AuditLogWriter, AuditOutcome, Finding, Severity, WritePolicy,
    workspace_is_valid, workspace_unavailable_outcome,
};
use crate::config::{AuditSettings, ChunkStrategy, ExecutorConfig, ResolvedSandbox};
use crate::paths::DaemonPaths;
use crate::prompts::{PromptId, PromptLoader};

/// Tools the audit agent may call. Read-only by construction: NO `Bash`,
/// `Write`, or `Edit`. `query_canonical_specs` / `ask_user` / the role's
/// `submit_*` tool are auto-included by `run_audit_cli_with_submit`.
const ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep"];

/// Maximum characters of stderr embedded in a parse-failure error
/// message. The full stderr always lands in the audit-run log.
const STDERR_EXCERPT_CHARS: usize = 400;

/// Default number of nearest requirements to retrieve per requirement
/// when RAG is enabled. Operator-tunable via
/// `audits.settings.canon_contradiction_audit.extra.retrieval_breadth`.
const DEFAULT_RETRIEVAL_BREADTH: u64 = 8;

/// Default cap on findings surfaced per run. Pairs beyond the cap surface
/// on subsequent runs. Operator-tunable via
/// `audits.settings.canon_contradiction_audit.extra.max_findings_per_run`.
const DEFAULT_MAX_FINDINGS_PER_RUN: u64 = 20;

pub struct CanonContradictionAudit {
    settings: AuditSettings,
    executor_command: String,
    executor_timeout_secs: u64,
    sandbox: ResolvedSandbox,
    /// Directory under `<state_dir>` holding this audit's per-workspace
    /// re-report suppression state (`<basename>.json`). Resolved from the
    /// daemon-threaded [`DaemonPaths`] at construction so the state is
    /// daemon bookkeeping under `<state_dir>`, NOT a file in the managed
    /// repo's workspace (per the canonical "standard locations"
    /// requirement). Cf. `alert_state` / `failure_state` / `revisions`.
    report_state_dir: PathBuf,
    /// Override for the directory the per-invocation sandbox settings file
    /// is written to. `None` (production) means `std::env::temp_dir()`.
    settings_dir: Option<PathBuf>,
    /// Test-only injected submission. `Some(Some(p))` stands in for a
    /// recorded payload; `Some(None)` simulates "agent never submitted";
    /// `None` (default) uses the real control-socket consume path.
    #[cfg(test)]
    test_submission: Option<Option<serde_json::Value>>,
    /// Test-only override for the RAG-enabled detection (which otherwise
    /// reads the process-global `crate::rag::shared_config`).
    #[cfg(test)]
    test_rag_enabled: Option<bool>,
}

impl CanonContradictionAudit {
    pub const TYPE: &'static str = "canon_contradiction_audit";

    pub fn new(
        audit_settings: &HashMap<String, AuditSettings>,
        executor: &ExecutorConfig,
        paths: &DaemonPaths,
    ) -> Self {
        let settings = audit_settings.get(Self::TYPE).cloned().unwrap_or_default();
        let sandbox = ResolvedSandbox::resolve(executor.sandbox.as_ref());
        Self {
            settings,
            executor_command: executor.command.clone(),
            executor_timeout_secs: executor.timeout_secs,
            sandbox,
            report_state_dir: paths.canon_contradiction_state_dir(),
            settings_dir: None,
            #[cfg(test)]
            test_submission: None,
            #[cfg(test)]
            test_rag_enabled: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_settings_dir(mut self, dir: PathBuf) -> Self {
        self.settings_dir = Some(dir);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_submission(mut self, submission: Option<serde_json::Value>) -> Self {
        self.test_submission = Some(submission);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_rag_enabled(mut self, enabled: bool) -> Self {
        self.test_rag_enabled = Some(enabled);
        self
    }

    /// Operator-tunable retrieval breadth (top_k) for `query_canonical_specs`.
    fn retrieval_breadth(&self) -> u64 {
        self.settings
            .extra
            .get("retrieval_breadth")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_RETRIEVAL_BREADTH)
    }

    /// Operator-tunable cap on findings surfaced per run.
    fn max_findings_per_run(&self) -> usize {
        self.settings
            .extra
            .get("max_findings_per_run")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_FINDINGS_PER_RUN) as usize
    }

    /// Whether a21's canonical-spec RAG is enabled for this daemon. Drives
    /// the prompt's retrieval guidance AND the best-effort degradation log.
    fn rag_enabled(&self) -> bool {
        #[cfg(test)]
        if let Some(over) = self.test_rag_enabled {
            return over;
        }
        crate::rag::shared_config()
            .map(|c| c.is_active())
            .unwrap_or(false)
    }

    /// Drain the agent's submission. In tests an injected override
    /// short-circuits the control socket; in production this relays
    /// `consume_submission` to the daemon.
    async fn consume_submission(&self, workspace: &Path) -> Option<serde_json::Value> {
        #[cfg(test)]
        if let Some(over) = &self.test_submission {
            return over.clone();
        }
        super::try_consume_submission(workspace, Self::TYPE).await
    }

    fn resolve_prompt(&self, workspace: Option<&Path>) -> Result<String> {
        Ok(PromptLoader::load(
            PromptId::AuditCanonContradiction,
            self.settings.prompt_path.as_deref(),
            None,
            workspace,
        ))
    }

    /// The per-workspace re-report suppression state file under
    /// `<state_dir>/canon-contradiction-state/`, keyed by the workspace
    /// basename (mirrors `DaemonPaths::alert_state_path`). The file is
    /// daemon bookkeeping, NOT an in-tree marker.
    fn report_state_path(&self, workspace: &Path) -> PathBuf {
        let basename = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        self.report_state_dir.join(format!("{basename}.json"))
    }
}

#[async_trait]
impl Audit for CanonContradictionAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn description(&self) -> &'static str {
        "canon-internal contradiction scan (two requirements that can't both hold)"
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::None
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        if !workspace_is_valid(ctx.workspace) {
            return Ok(workspace_unavailable_outcome(
                Self::TYPE,
                ctx.workspace,
                &ctx.repo.url,
            ));
        }

        let rag_on = self.rag_enabled();
        let breadth = self.retrieval_breadth();
        let cap = self.max_findings_per_run();

        let base_prompt = self.resolve_prompt(Some(ctx.workspace))?;
        let prompt = compose_prompt(&base_prompt, rag_on, breadth);

        let mut sandbox = self.sandbox.clone();
        sandbox.allowed_tools = ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect();

        let _ = ctx.log_writer.write_section(
            "canon_contradiction_audit_preamble",
            &format!(
                "executor_command: {}\ntimeout_secs: {}\nprompt_source: {}\nallowed_tools: {}\nmax_findings_per_run: {}",
                self.executor_command,
                self.executor_timeout_secs,
                self.settings
                    .prompt_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<embedded default>".to_string()),
                sandbox.allowed_tools.join(","),
                cap,
            ),
        );
        // RAG mode is behavior (not prompt wording): log the decision and,
        // when RAG is off, the best-effort degradation.
        let _ = ctx.log_writer.write_section(
            "canon_contradiction_audit_rag",
            &format!(
                "rag_enabled: {rag_on}\nretrieval_breadth: {breadth}\ncoverage: {}",
                if rag_on {
                    "rag-assisted (query_canonical_specs)"
                } else {
                    "best-effort direct read (RAG not configured; subtle cross-capability pairs may be missed)"
                }
            ),
        );
        if !rag_on {
            // no-url: audit driver, no per-repo URL in scope beyond ctx.repo
            tracing::info!(
                url = %ctx.repo.url,
                audit_type = Self::TYPE,
                "canon_contradiction_audit: a21 RAG not configured; coverage is best-effort"
            );
        }
        let _ = ctx
            .log_writer
            .write_section("canon_contradiction_audit_prompt", &prompt);

        // audit-model-selection: route this audit to its configured model
        // (if any); `None` keeps the default `claude` strategy.
        let model = super::audit_resolved_model(&self.settings);
        let outcome = super::run_audit_cli_with_submit(
            &self.executor_command,
            &sandbox,
            ctx.workspace,
            &prompt,
            Duration::from_secs(self.executor_timeout_secs),
            self.settings_dir.as_deref(),
            Self::TYPE,
            model.as_ref(),
            // Writability derives from the declared WritePolicy (None →
            // read-only) so the mount can never drift from the policy the
            // post-hoc check enforces.
            self.write_policy().workspace_writable(),
        )
        .await
        .context("spawning canon-contradiction-audit CLI subprocess")?;

        let _ = ctx.log_writer.write_section(
            "canon_contradiction_audit_stdout",
            if outcome.stdout.is_empty() {
                "(empty)"
            } else {
                outcome.stdout.as_str()
            },
        );
        let _ = ctx.log_writer.write_section(
            "canon_contradiction_audit_stderr",
            if outcome.stderr.is_empty() {
                "(empty)"
            } else {
                outcome.stderr.as_str()
            },
        );

        if let Some(err) = outcome_to_terminal_err(
            &outcome,
            &mut ctx.log_writer,
            Self::TYPE,
            self.executor_timeout_secs,
        ) {
            return Err(err);
        }

        // Advisory: a missing submission consumes as an empty result (a
        // clean canon), NOT a failure. A present-but-malformed payload is a
        // hard error (the agent recorded something that did not validate).
        let detected: Vec<Contradiction> = match self.consume_submission(ctx.workspace).await {
            Some(payload) => match payload_to_contradictions(&payload) {
                Ok(c) => c,
                Err(e) => {
                    let _ = ctx.log_writer.write_section(
                        "canon_contradiction_audit_outcome",
                        &format!("kind: Err\nreason: {e}"),
                    );
                    return Err(anyhow!("canon_contradiction_audit: {e}"));
                }
            },
            None => {
                let _ = ctx.log_writer.write_section(
                    "canon_contradiction_audit_outcome",
                    "kind: Reported\nfindings_count: 0\nnote: no submission recorded — consumed as empty (clean canon)",
                );
                Vec::new()
            }
        };

        // Build a (capability, title) → requirement-text index from the
        // canon so reported pairs can be content-hashed for suppression.
        let req_index = build_requirement_index(ctx.workspace);
        let detected_pairs: Vec<DetectedPair> = detected
            .iter()
            .map(|c| DetectedPair::from_contradiction(c, &req_index))
            .collect();

        // Re-report suppression. The state lives under `<state_dir>` (NOT
        // in the workspace), so it is daemon bookkeeping that never appears
        // in the managed repo's working tree — no `.git/info/exclude`
        // registration is needed AND the `WritePolicy::None` post-hoc
        // clean-tree check is satisfied trivially.
        let state_path = self.report_state_path(ctx.workspace);
        let mut report_state = ReportState::load_or_default(&state_path);
        let (to_report, new_reported) =
            apply_suppression(detected_pairs, &report_state.reported, cap);
        let suppressed = detected.len().saturating_sub(to_report.len());
        report_state.reported = new_reported;
        if let Err(e) = report_state.save(&state_path) {
            tracing::warn!(
                url = %ctx.repo.url,
                "canon_contradiction_audit: failed to persist report state: {e:#}"
            );
        }

        let findings: Vec<Finding> = to_report.iter().map(|d| d.to_finding()).collect();
        let _ = ctx.log_writer.write_section(
            "canon_contradiction_audit_outcome",
            &format!(
                "kind: Reported\ndetected: {}\nsuppressed_or_capped: {}\nfindings_count: {}",
                detected.len(),
                suppressed,
                findings.len(),
            ),
        );
        Ok(AuditOutcome::reported(findings))
    }
}

/// Append the daemon-set retrieval configuration to the embedded prompt.
/// This is operational guidance (which tool to use, how many neighbors,
/// the best-effort fallback) — distinct from the precision/recall design
/// intent baked into the embedded template, which is deliberately NOT
/// pinned by a test.
fn compose_prompt(base: &str, rag_on: bool, breadth: u64) -> String {
    let mut out = base.trim_end().to_string();
    out.push_str("\n\n---\n\n## Retrieval configuration (set by the daemon for this run)\n\n");
    if rag_on {
        out.push_str(&format!(
            "Canonical-spec RAG is ENABLED. The `query_canonical_specs` MCP tool is backed by an \
             index this run. Enumerate the canonical requirements across `openspec/specs/*/spec.md`, \
             and for each requirement retrieve the {breadth} most semantically-similar requirements \
             via `query_canonical_specs` and check that focused bundle for a pair that cannot both \
             hold. This bounds each check AND targets related requirements, where contradictions \
             actually live.\n",
        ));
    } else {
        out.push_str(
            "Canonical-spec RAG is NOT configured this run; `query_canonical_specs` will return \
             empty hits. Fall back to a best-effort direct read of `openspec/specs/*/spec.md`. \
             Coverage is best-effort — subtle cross-capability contradictions may be missed without \
             retrieval.\n",
        );
    }
    out.push_str(
        "\nReturn every contradiction you are confident about by calling \
         `submit_canon_internal_contradictions` exactly once (an empty `contradictions` array means \
         a clean canon).\n",
    );
    out
}

/// A single canon-internal contradiction as the agent submitted it: both
/// sides are canonical (capability + requirement title) plus a one-line
/// conflict summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Contradiction {
    pub capability_a: String,
    pub requirement_a: String,
    pub capability_b: String,
    pub requirement_b: String,
    pub summary: String,
}

/// Validate AND map a consumed `submit_canon_internal_contradictions`
/// payload into [`Contradiction`]s. This is BOTH the daemon-side schema
/// validator (registered with its `Ok` value discarded) AND the
/// consume-time mapper, so a payload that records successfully is exactly
/// one that maps — the two can never drift. Returns `Err(reason)` (a
/// correction-suitable string) on a malformed payload; `record_submission`
/// surfaces the reason to the agent as a correctable tool error.
pub(crate) fn payload_to_contradictions(
    payload: &serde_json::Value,
) -> std::result::Result<Vec<Contradiction>, String> {
    let sub: RawSubmission = serde_json::from_value(payload.clone()).map_err(|e| {
        format!(
            "submit_canon_internal_contradictions: payload does not match the expected shape \
             {{ contradictions: [{{ capability_a, requirement_a, capability_b, requirement_b, \
             summary }}] }}: {e}"
        )
    })?;
    Ok(sub
        .contradictions
        .into_iter()
        .map(|c| Contradiction {
            capability_a: c.capability_a,
            requirement_a: c.requirement_a,
            capability_b: c.capability_b,
            requirement_b: c.requirement_b,
            summary: c.summary,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct RawContradiction {
    capability_a: String,
    requirement_a: String,
    capability_b: String,
    requirement_b: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct RawSubmission {
    contradictions: Vec<RawContradiction>,
}

/// A detected contradiction enriched with the order-independent pair key
/// AND the content hashes of each side's requirement text, used for
/// re-report suppression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetectedPair {
    contradiction: Contradiction,
    /// Order-independent key over the two (capability + title) sides.
    key: String,
    /// Content hash of the lexicographically-smaller side's requirement
    /// text (the side that sorts first into `key`).
    hash_0: String,
    /// Content hash of the larger side's requirement text.
    hash_1: String,
}

impl DetectedPair {
    fn from_contradiction(
        c: &Contradiction,
        req_index: &HashMap<(String, String), String>,
    ) -> Self {
        let side_a = SideKey::new(&c.capability_a, &c.requirement_a);
        let side_b = SideKey::new(&c.capability_b, &c.requirement_b);
        let hash_a = content_hash(&lookup_text(req_index, &side_a, &c.requirement_a));
        let hash_b = content_hash(&lookup_text(req_index, &side_b, &c.requirement_b));
        // Order-independent: sort the two sides; the smaller side's hash is
        // hash_0 so the same pair reported A-vs-B or B-vs-A keys identically.
        let (key, hash_0, hash_1) = if side_a <= side_b {
            (format!("{side_a}\u{241e}{side_b}"), hash_a, hash_b)
        } else {
            (format!("{side_b}\u{241e}{side_a}"), hash_b, hash_a)
        };
        Self {
            contradiction: c.clone(),
            key,
            hash_0,
            hash_1,
        }
    }

    fn to_finding(&self) -> Finding {
        let c = &self.contradiction;
        let subject = format!(
            "[{cap_a}] {req_a} ⟂ [{cap_b}] {req_b}",
            cap_a = c.capability_a,
            req_a = c.requirement_a,
            cap_b = c.capability_b,
            req_b = c.requirement_b,
        );
        let body = format!(
            "Requirement A: [{cap_a}] {req_a}\nRequirement B: [{cap_b}] {req_b}\n\nConflict: {summary}",
            cap_a = c.capability_a,
            req_a = c.requirement_a,
            cap_b = c.capability_b,
            req_b = c.requirement_b,
            summary = c.summary,
        );
        Finding {
            severity: Severity::Medium,
            subject,
            body,
            anchor: None,
        }
    }
}

/// A normalized (capability, requirement-title) side, used both as the
/// pair-key component AND the requirement-text lookup key. Normalization
/// (trim + lowercase) makes the order-independent key stable against
/// trivial casing/whitespace differences in how the agent names a side.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SideKey {
    capability: String,
    title: String,
}

impl SideKey {
    fn new(capability: &str, title: &str) -> Self {
        Self {
            capability: capability.trim().to_lowercase(),
            title: title.trim().to_lowercase(),
        }
    }
}

impl std::fmt::Display for SideKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\u{241f}{}", self.capability, self.title)
    }
}

/// Look up a requirement's text from the canon index by normalized side
/// key. Falls back to the raw title when the side cannot be located (a
/// renamed/hallucinated requirement) so suppression still keys
/// deterministically; if the requirement later resolves, its hash changes
/// and the pair re-surfaces — the conservative behavior.
fn lookup_text(
    req_index: &HashMap<(String, String), String>,
    side: &SideKey,
    raw_title: &str,
) -> String {
    req_index
        .get(&(side.capability.clone(), side.title.clone()))
        .cloned()
        .unwrap_or_else(|| raw_title.trim().to_string())
}

/// Non-cryptographic stable content hash for change detection. Uses the
/// std SipHash (`DefaultHasher::new`, fixed keys) — deterministic across
/// runs, no extra dependency. Hashes the trimmed text so trailing
/// whitespace alone does not re-surface a pair.
fn content_hash(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.trim().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Apply re-report suppression. Returns the pairs to report this run AND
/// the suppression state to persist.
///
/// Rules (per the `Re-report suppression` requirement):
///   - A recorded pair whose two requirements are textually unchanged is
///     suppressed (kept in state, not reported).
///   - A pair re-surfaces (reported) when either requirement's text has
///     changed since it was recorded, OR when it has never been recorded.
///   - A recorded pair no longer detected is pruned (absent from the new
///     state).
///   - Findings are capped at `cap`; pairs beyond the cap are NOT recorded
///     so they surface on a subsequent run.
fn apply_suppression(
    detected: Vec<DetectedPair>,
    prior: &HashMap<String, ReportedPair>,
    cap: usize,
) -> (Vec<DetectedPair>, HashMap<String, ReportedPair>) {
    let mut new_state: HashMap<String, ReportedPair> = HashMap::new();
    let mut candidates: Vec<DetectedPair> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for d in detected {
        // The agent may submit the same pair twice (A-vs-B and B-vs-A, or a
        // literal duplicate); collapse on the order-independent key.
        if !seen.insert(d.key.clone()) {
            continue;
        }
        match prior.get(&d.key) {
            Some(p) if p.hash_0 == d.hash_0 && p.hash_1 == d.hash_1 => {
                // Unchanged recorded pair → suppress; keep its record so it
                // is not pruned.
                new_state.insert(d.key.clone(), p.clone());
            }
            _ => {
                // New or edited → report (subject to the cap below).
                candidates.push(d);
            }
        }
    }

    let mut to_report = Vec::new();
    for (i, d) in candidates.into_iter().enumerate() {
        if i < cap {
            new_state.insert(
                d.key.clone(),
                ReportedPair {
                    hash_0: d.hash_0.clone(),
                    hash_1: d.hash_1.clone(),
                },
            );
            to_report.push(d);
        }
        // Overflow (i >= cap): deliberately NOT recorded → surfaces next run.
    }

    (to_report, new_state)
}

/// Build a `(normalized-capability, normalized-title) → requirement-text`
/// index from every `openspec/specs/<cap>/spec.md`. Reused for content
/// hashing. Failures to read/parse a spec file are logged and skipped.
fn build_requirement_index(workspace: &Path) -> HashMap<(String, String), String> {
    let specs_dir = workspace.join("openspec/specs");
    let mut index = HashMap::new();
    let Ok(read) = std::fs::read_dir(&specs_dir) else {
        return index;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let spec = path.join("spec.md");
        if !spec.is_file() {
            continue;
        }
        match crate::rag::chunk_canonical_spec(&spec, ChunkStrategy::PerRequirement) {
            Ok(chunks) => {
                for c in chunks {
                    index.insert(
                        (
                            c.capability.trim().to_lowercase(),
                            c.requirement_title.trim().to_lowercase(),
                        ),
                        c.text,
                    );
                }
            }
            Err(e) => {
                // no-url: canon-index builder, keyed by spec path
                tracing::warn!(
                    spec = %spec.display(),
                    "canon_contradiction_audit: failed to chunk canonical spec: {e:#}"
                );
            }
        }
    }
    index
}

// ---------------------------------------------------------------------------
// Re-report suppression state (persisted under `<state_dir>`, keyed by
// workspace basename — daemon bookkeeping, never in the repo workspace).
// ---------------------------------------------------------------------------

/// The two content hashes of a recorded contradiction pair, ordered to
/// match the order-independent key (hash_0 = smaller side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReportedPair {
    pub hash_0: String,
    pub hash_1: String,
}

/// On-disk re-report suppression state for the canon-contradiction audit.
/// Keyed by the order-independent (capability + title) pair key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReportState {
    #[serde(default)]
    pub reported: HashMap<String, ReportedPair>,
}

impl ReportState {
    /// Load the report state from the given file path (under `<state_dir>`).
    /// Missing → empty default; corrupt → WARN + empty default (never blocks
    /// the audit).
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                // no-url: report-state loader keyed on state-dir path
                tracing::warn!(
                    "canon_contradiction_audit report state at {} is corrupt; starting empty: {e:#}",
                    path.display()
                );
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                // no-url: report-state loader keyed on state-dir path
                tracing::warn!(
                    "canon_contradiction_audit report state at {} unreadable; starting empty: {e:#}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Atomically persist to the given file path via tempfile-then-rename in
    /// its parent directory (created if absent).
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("destination path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
        let tmp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("creating tempfile in {}", parent.display()))?;
        serde_json::to_writer_pretty(&tmp, self)
            .with_context(|| format!("serializing report state for {}", path.display()))?;
        tmp.persist(&path)
            .map_err(|e| anyhow!("atomically persisting {}: {e}", path.display()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shared small helpers (mirrors of drift's, scoped here to avoid coupling).
// ---------------------------------------------------------------------------

fn excerpt(s: &str) -> String {
    let mut out: String = s.chars().take(STDERR_EXCERPT_CHARS).collect();
    if s.chars().count() > STDERR_EXCERPT_CHARS {
        out.push('…');
    }
    out
}

/// Return `Some(error)` when the run outcome is terminal (timed out OR
/// non-zero exit). Mirrors `drift::outcome_to_terminal_err`.
fn outcome_to_terminal_err(
    outcome: &crate::agentic_run::AgenticRunOutcome,
    log_writer: &mut AuditLogWriter,
    audit_type: &str,
    timeout_secs: u64,
) -> Option<anyhow::Error> {
    if outcome.timed_out {
        let _ = log_writer.write_section(
            &format!("{audit_type}_outcome"),
            "kind: Err\nreason: timeout",
        );
        return Some(anyhow!("{audit_type}: CLI exceeded the {timeout_secs}s timeout"));
    }
    if let Some(status) = outcome.exit_status
        && !status.success()
    {
        let _ = log_writer.write_section(
            &format!("{audit_type}_outcome"),
            &format!("kind: Err\nreason: exit {status}"),
        );
        return Some(anyhow!(
            "{audit_type}: CLI exited {status}; stderr excerpt: {}",
            excerpt(&outcome.stderr)
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audits::AuditLogWriter;
    use crate::config::{ExecutorKind, RepositoryConfig};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn executor_cfg(command: &str) -> ExecutorConfig {
        ExecutorConfig {
            kind: ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: command.to_string(),
            timeout_secs: 30,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: crate::config::default_wipe_drain_timeout_secs(),
            output_format: crate::config::default_output_format(),
            log_retention_days: crate::config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check: crate::config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check: crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check: crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        }
    }

    fn fixture_repo() -> RepositoryConfig {
        RepositoryConfig {
            forge: None,
            url: "git@github.com:test/repo.git".into(),
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

    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn make_log_writer(workspace: &std::path::Path) -> AuditLogWriter {
        let (td, paths) = crate::testing::test_daemon_paths();
        std::mem::forget(td);
        AuditLogWriter::open(&paths, workspace, "canon_contradiction_audit")
            .expect("log writer opens")
    }

    /// A tempdir-scoped [`DaemonPaths`] for constructing the audit. The
    /// tempdir is leaked (as `make_log_writer` does) so the resolved
    /// `<state_dir>/canon-contradiction-state/` survives for the duration
    /// of the test, including across the run-1/run-2 suppression checks.
    fn test_paths() -> DaemonPaths {
        let (td, paths) = crate::testing::test_daemon_paths();
        std::mem::forget(td);
        paths
    }

    /// Build a workspace with the given capability→spec.md map and a bare
    /// `.git/` directory (enough to satisfy `workspace_is_valid`). Use this
    /// for tests that do NOT exercise the post-hoc `git status` clean-tree
    /// check.
    fn workspace_with_specs(specs: &[(&str, &str)]) -> TempDir {
        let ws = TempDir::new().unwrap();
        std::fs::create_dir_all(ws.path().join(".git")).unwrap();
        write_specs(ws.path(), specs);
        ws
    }

    fn write_specs(root: &std::path::Path, specs: &[(&str, &str)]) {
        for (cap, body) in specs {
            let dir = root.join("openspec/specs").join(cap);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("spec.md"), body).unwrap();
        }
    }

    /// Build a REAL git repository with the specs committed, for tests that
    /// assert the post-hoc clean-tree check (`git status` empty after run).
    fn git_workspace_with_specs(specs: &[(&str, &str)]) -> TempDir {
        let ws = TempDir::new().unwrap();
        write_specs(ws.path(), specs);
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(ws.path())
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        ws
    }

    fn submission(pairs: &[(&str, &str, &str, &str, &str)]) -> serde_json::Value {
        let arr: Vec<serde_json::Value> = pairs
            .iter()
            .map(|(ca, ra, cb, rb, s)| {
                serde_json::json!({
                    "capability_a": ca,
                    "requirement_a": ra,
                    "capability_b": cb,
                    "requirement_b": rb,
                    "summary": s,
                })
            })
            .collect();
        serde_json::json!({ "contradictions": arr })
    }

    // ---- payload mapping (schema validator = mapper) --------------------

    #[test]
    fn payload_round_trips_to_contradictions() {
        let payload = submission(&[(
            "storage",
            "All data in a relational database",
            "storage",
            "Records live in a document store",
            "Both cannot hold: relational vs document store for the same records.",
        )]);
        let parsed = payload_to_contradictions(&payload).expect("deserializes");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].capability_a, "storage");
        assert!(parsed[0].summary.contains("Both cannot hold"));
    }

    #[test]
    fn empty_contradictions_array_is_clean() {
        let payload = serde_json::json!({ "contradictions": [] });
        let parsed = payload_to_contradictions(&payload).expect("empty array ok");
        assert!(parsed.is_empty());
    }

    #[test]
    fn missing_contradictions_key_returns_err() {
        let payload = serde_json::json!({ "results": [] });
        let err = payload_to_contradictions(&payload).expect_err("missing key errors");
        assert!(err.contains("contradictions"), "got: {err}");
    }

    #[test]
    fn non_array_contradictions_returns_err() {
        let payload = serde_json::json!({ "contradictions": "nope" });
        let err = payload_to_contradictions(&payload).expect_err("non-array errors");
        assert!(err.contains("contradictions"), "got: {err}");
    }

    #[test]
    fn entry_missing_required_field_returns_err() {
        // `requirement_b` omitted — correctable tool error.
        let payload = serde_json::json!({
            "contradictions": [
                { "capability_a": "a", "requirement_a": "ra", "capability_b": "b", "summary": "s" }
            ]
        });
        let err = payload_to_contradictions(&payload).expect_err("missing field errors");
        assert!(err.contains("contradictions"), "got: {err}");
    }

    // ---- finding composition -------------------------------------------

    #[test]
    fn finding_names_both_requirements_and_reason() {
        let c = Contradiction {
            capability_a: "storage".into(),
            requirement_a: "Relational store".into(),
            capability_b: "ingest".into(),
            requirement_b: "Document store".into(),
            summary: "Cannot both hold.".into(),
        };
        let d = DetectedPair::from_contradiction(&c, &HashMap::new());
        let f = d.to_finding();
        assert!(f.subject.contains("[storage] Relational store"));
        assert!(f.subject.contains("[ingest] Document store"));
        assert!(f.body.contains("Cannot both hold."));
        assert!(f.body.contains("Requirement A"));
        assert!(f.body.contains("Requirement B"));
    }

    // ---- order-independent keying --------------------------------------

    #[test]
    fn pair_key_is_order_independent() {
        let ab = Contradiction {
            capability_a: "a".into(),
            requirement_a: "Foo".into(),
            capability_b: "b".into(),
            requirement_b: "Bar".into(),
            summary: "x".into(),
        };
        let ba = Contradiction {
            capability_a: "b".into(),
            requirement_a: "Bar".into(),
            capability_b: "a".into(),
            requirement_b: "Foo".into(),
            summary: "x".into(),
        };
        let da = DetectedPair::from_contradiction(&ab, &HashMap::new());
        let db = DetectedPair::from_contradiction(&ba, &HashMap::new());
        assert_eq!(da.key, db.key, "A-vs-B and B-vs-A must key identically");
        assert_eq!(da.hash_0, db.hash_0);
        assert_eq!(da.hash_1, db.hash_1);
    }

    #[test]
    fn case_and_whitespace_do_not_change_key() {
        let a = Contradiction {
            capability_a: "Storage".into(),
            requirement_a: "  Relational Store ".into(),
            capability_b: "ingest".into(),
            requirement_b: "Document store".into(),
            summary: "x".into(),
        };
        let b = Contradiction {
            capability_a: "storage".into(),
            requirement_a: "relational store".into(),
            capability_b: "INGEST".into(),
            requirement_b: "document store".into(),
            summary: "x".into(),
        };
        let da = DetectedPair::from_contradiction(&a, &HashMap::new());
        let db = DetectedPair::from_contradiction(&b, &HashMap::new());
        assert_eq!(da.key, db.key);
    }

    // ---- suppression algorithm -----------------------------------------

    fn detected(c: &Contradiction, idx: &HashMap<(String, String), String>) -> DetectedPair {
        DetectedPair::from_contradiction(c, idx)
    }

    #[test]
    fn unchanged_recorded_pair_is_suppressed_edited_resurfaces_healed_pruned() {
        let mut idx: HashMap<(String, String), String> = HashMap::new();
        idx.insert(("a".into(), "foo".into()), "FOO TEXT v1".into());
        idx.insert(("b".into(), "bar".into()), "BAR TEXT".into());
        idx.insert(("c".into(), "baz".into()), "BAZ TEXT".into());
        idx.insert(("d".into(), "qux".into()), "QUX TEXT".into());

        let pair_ab = Contradiction {
            capability_a: "a".into(),
            requirement_a: "Foo".into(),
            capability_b: "b".into(),
            requirement_b: "Bar".into(),
            summary: "ab".into(),
        };
        let pair_cd = Contradiction {
            capability_a: "c".into(),
            requirement_a: "Baz".into(),
            capability_b: "d".into(),
            requirement_b: "Qux".into(),
            summary: "cd".into(),
        };

        // Run 1: both new → both reported, both recorded.
        let d1 = vec![detected(&pair_ab, &idx), detected(&pair_cd, &idx)];
        let (rep1, state1) = apply_suppression(d1, &HashMap::new(), 100);
        assert_eq!(rep1.len(), 2, "first run reports both new pairs");
        assert_eq!(state1.len(), 2);

        // Run 2: same canon, both detected again, unchanged → both suppressed.
        let d2 = vec![detected(&pair_ab, &idx), detected(&pair_cd, &idx)];
        let (rep2, state2) = apply_suppression(d2, &state1, 100);
        assert!(rep2.is_empty(), "unchanged recorded pairs are suppressed");
        assert_eq!(state2.len(), 2, "records retained while still detected");

        // Run 3: edit requirement A's text → pair_ab re-surfaces; pair_cd
        // still suppressed.
        idx.insert(("a".into(), "foo".into()), "FOO TEXT v2 EDITED".into());
        let d3 = vec![detected(&pair_ab, &idx), detected(&pair_cd, &idx)];
        let (rep3, state3) = apply_suppression(d3, &state2, 100);
        assert_eq!(rep3.len(), 1, "edited pair re-surfaces");
        assert_eq!(rep3[0].contradiction.capability_a, "a");
        assert_eq!(state3.len(), 2);

        // Run 4: pair_ab healed (no longer detected) → pruned; pair_cd
        // remains suppressed.
        let d4 = vec![detected(&pair_cd, &idx)];
        let (rep4, state4) = apply_suppression(d4, &state3, 100);
        assert!(rep4.is_empty());
        assert_eq!(state4.len(), 1, "healed pair pruned from state");
        assert!(
            state4.keys().next().unwrap().contains("baz"),
            "only the cd pair survives"
        );
    }

    #[test]
    fn cap_limits_findings_and_overflow_surfaces_next_run() {
        let idx: HashMap<(String, String), String> = HashMap::new();
        let make = |i: usize| Contradiction {
            capability_a: format!("cap{i}"),
            requirement_a: format!("req{i}a"),
            capability_b: format!("cap{i}"),
            requirement_b: format!("req{i}b"),
            summary: format!("s{i}"),
        };
        let all: Vec<Contradiction> = (0..5).map(make).collect();
        let d1: Vec<DetectedPair> = all.iter().map(|c| detected(c, &idx)).collect();

        // Cap of 2: only 2 reported, only 2 recorded; the other 3 surface
        // next run.
        let (rep1, state1) = apply_suppression(d1, &HashMap::new(), 2);
        assert_eq!(rep1.len(), 2);
        assert_eq!(state1.len(), 2, "overflow pairs are NOT recorded");

        // Next run, same detections: the 2 recorded are suppressed; the
        // remaining 3 are now candidates, capped to 2 again.
        let d2: Vec<DetectedPair> = all.iter().map(|c| detected(c, &idx)).collect();
        let (rep2, state2) = apply_suppression(d2, &state1, 2);
        assert_eq!(rep2.len(), 2, "previously-overflowed pairs surface");
        assert_eq!(state2.len(), 4);
    }

    #[test]
    fn duplicate_submitted_pair_collapses() {
        let idx: HashMap<(String, String), String> = HashMap::new();
        let c = Contradiction {
            capability_a: "a".into(),
            requirement_a: "Foo".into(),
            capability_b: "b".into(),
            requirement_b: "Bar".into(),
            summary: "x".into(),
        };
        let d = vec![detected(&c, &idx), detected(&c, &idx)];
        let (rep, state) = apply_suppression(d, &HashMap::new(), 100);
        assert_eq!(rep.len(), 1, "duplicate collapses to one finding");
        assert_eq!(state.len(), 1);
    }

    // ---- requirement index ---------------------------------------------

    #[test]
    fn build_requirement_index_parses_canon() {
        let ws = workspace_with_specs(&[(
            "storage",
            "# storage\n\n### Requirement: Relational store\nAll data SHALL live in a relational database.\n\n#### Scenario: x\n- **WHEN** y\n- **THEN** z\n",
        )]);
        let idx = build_requirement_index(ws.path());
        assert!(
            idx.contains_key(&("storage".into(), "relational store".into())),
            "index keys: {:?}",
            idx.keys().collect::<Vec<_>>()
        );
        let text = &idx[&("storage".into(), "relational store".into())];
        assert!(text.contains("relational database"));
    }

    // ---- report state round-trip ---------------------------------------

    #[test]
    fn report_state_round_trips_and_handles_missing_and_corrupt() {
        let dir = TempDir::new().unwrap();
        // The state file lives under a state-dir-shaped subdir (created on
        // save), NOT in any repo workspace.
        let path = dir.path().join("canon-contradiction-state/repo.json");
        // missing → empty
        let s = ReportState::load_or_default(&path);
        assert!(s.reported.is_empty());
        // round-trip
        let mut s2 = ReportState::default();
        s2.reported.insert(
            "k".into(),
            ReportedPair {
                hash_0: "h0".into(),
                hash_1: "h1".into(),
            },
        );
        s2.save(&path).unwrap();
        let reloaded = ReportState::load_or_default(&path);
        assert_eq!(reloaded, s2);
        // corrupt → empty
        std::fs::write(&path, "{not json").unwrap();
        let s3 = ReportState::load_or_default(&path);
        assert!(s3.reported.is_empty());
    }

    // ---- prompt composition (RAG mode is derivation, not wording) -------

    #[test]
    fn compose_prompt_reflects_rag_on_with_breadth() {
        let out = compose_prompt("BASE PROMPT", true, 7);
        assert!(out.contains("BASE PROMPT"));
        assert!(out.contains("query_canonical_specs"));
        assert!(out.contains('7'), "breadth value must be injected");
        assert!(out.contains("submit_canon_internal_contradictions"));
    }

    #[test]
    fn compose_prompt_reflects_rag_off_best_effort() {
        let out = compose_prompt("BASE PROMPT", false, 8);
        assert!(out.contains("best-effort"));
        assert!(out.contains("submit_canon_internal_contradictions"));
    }

    // ---- trait fixedness -----------------------------------------------

    #[test]
    fn allowed_tools_are_read_only() {
        assert_eq!(ALLOWED_TOOLS, &["Read", "Glob", "Grep"]);
        for forbidden in ["Bash", "Write", "Edit"] {
            assert!(
                !ALLOWED_TOOLS.contains(&forbidden),
                "{forbidden} must NOT be in the read-only audit sandbox"
            );
        }
    }

    #[test]
    fn audit_type_and_policy_are_fixed() {
        let cfg = executor_cfg("/bin/true");
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &test_paths());
        assert_eq!(audit.audit_type(), "canon_contradiction_audit");
        assert!(audit.requires_head_change());
        assert!(matches!(audit.write_policy(), WritePolicy::None));
        let d = audit.description();
        assert!(!d.is_empty() && d.chars().count() <= 80);
    }

    #[test]
    fn new_reads_tunable_knobs_from_settings() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("retrieval_breadth".into(), serde_yml::Value::from(12u64));
        extra.insert("max_findings_per_run".into(), serde_yml::Value::from(3u64));
        let mut map = HashMap::new();
        map.insert(
            CanonContradictionAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
                ..Default::default()
            },
        );
        let cfg = executor_cfg("claude");
        let audit = CanonContradictionAudit::new(&map, &cfg, &test_paths());
        assert_eq!(audit.retrieval_breadth(), 12);
        assert_eq!(audit.max_findings_per_run(), 3);
    }

    #[test]
    fn knob_defaults_apply_when_absent_or_zero() {
        let cfg = executor_cfg("claude");
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &test_paths());
        assert_eq!(audit.retrieval_breadth(), DEFAULT_RETRIEVAL_BREADTH);
        assert_eq!(
            audit.max_findings_per_run(),
            DEFAULT_MAX_FINDINGS_PER_RUN as usize
        );
    }

    #[test]
    fn resolve_prompt_uses_embedded_default() {
        let cfg = executor_cfg("/bin/true");
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &test_paths());
        let prompt = audit.resolve_prompt(None).expect("default resolves");
        assert!(prompt.contains("submit_canon_internal_contradictions"));
        assert!(prompt.contains("openspec/specs"));
    }

    // ---- run() integration ---------------------------------------------

    #[tokio::test]
    async fn run_reports_pair_with_clean_tree_and_rag_log() {
        let ws = git_workspace_with_specs(&[
            (
                "storage",
                "# storage\n\n### Requirement: Relational store\nAll data SHALL live in a relational database.\n",
            ),
            (
                "ingest",
                "# ingest\n\n### Requirement: Document store\nRecords SHALL live in a document store.\n",
            ),
        ]);
        let workspace = ws.path();
        // The fake CLI lives OUTSIDE the workspace so it does not pollute the
        // tree the clean-tree check inspects.
        let script_dir = TempDir::new().unwrap();
        let script = write_script(script_dir.path(), "fake.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let sub = submission(&[(
            "storage",
            "Relational store",
            "ingest",
            "Document store",
            "Relational-only vs document-store: cannot both hold.",
        )]);
        let paths = test_paths();
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &paths)
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_submission(Some(sub))
            .with_rag_enabled(true);
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        match outcome {
            AuditOutcome::Reported { findings, .. } => {
                assert_eq!(findings.len(), 1);
                assert!(findings[0].subject.contains("[storage] Relational store"));
                assert!(findings[0].subject.contains("[ingest] Document store"));
            }
            other => panic!("expected Reported, got {other:?}"),
        }
        // The audit must not modify the canon (WritePolicy::None). It writes
        // NO file into the workspace — the suppression state lives under
        // `<state_dir>`, so the tree is clean with no `.git/info/exclude`
        // dance required.
        let entries = crate::git::status_entries(workspace).expect("status");
        assert!(
            entries.is_empty(),
            "tree must be clean after run (state lives under <state_dir>, not the workspace); got: {entries:?}"
        );
        let basename = workspace.file_name().unwrap().to_str().unwrap();
        let state_file = paths
            .canon_contradiction_state_dir()
            .join(format!("{basename}.json"));
        assert!(
            state_file.exists(),
            "report state must be persisted under <state_dir> at {}",
            state_file.display()
        );
        assert!(
            state_file.starts_with(&paths.state),
            "report state must live under <state_dir>, not the workspace: {}",
            state_file.display()
        );
        let log = std::fs::read_to_string(&log_path).expect("log");
        assert!(log.contains("canon_contradiction_audit_rag"));
        assert!(log.contains("rag_enabled: true"));
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_off_rag_logs_best_effort() {
        let ws = workspace_with_specs(&[]);
        let workspace = ws.path();
        let script = write_script(workspace, "fake.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &test_paths())
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_submission(Some(serde_json::json!({ "contradictions": [] })))
            .with_rag_enabled(false);
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        assert!(matches!(outcome, AuditOutcome::Reported { ref findings, .. } if findings.is_empty()));
        let log = std::fs::read_to_string(&log_path).expect("log");
        assert!(log.contains("rag_enabled: false"));
        assert!(log.contains("best-effort"));
        // The run uses the read-only sandbox (Read/Glob/Grep only).
        assert!(
            log.contains("allowed_tools: Read,Glob,Grep"),
            "run must use the read-only sandbox: {log}"
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_missing_submission_consumes_as_empty() {
        let ws = workspace_with_specs(&[]);
        let workspace = ws.path();
        let script = write_script(workspace, "fake.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        // `with_submission(None)` simulates "agent never submitted".
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &test_paths())
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_submission(None);
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit
            .run(&mut ctx)
            .await
            .expect("missing submission is not an error");
        match outcome {
            AuditOutcome::Reported { findings, .. } => {
                assert!(findings.is_empty(), "missing submission → clean canon");
            }
            other => panic!("expected Reported(empty), got {other:?}"),
        }
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_returns_err_on_malformed_submission() {
        let ws = workspace_with_specs(&[]);
        let workspace = ws.path();
        let script = write_script(workspace, "fake.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &test_paths())
            .with_settings_dir(settings_dir.path().to_path_buf())
            .with_submission(Some(serde_json::json!({ "wrong": [] })));
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let err = audit.run(&mut ctx).await.expect_err("malformed payload errors");
        assert!(format!("{err:#}").contains("contradictions"));
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_returns_err_on_nonzero_exit() {
        let ws = workspace_with_specs(&[]);
        let workspace = ws.path();
        let script = write_script(workspace, "fail.sh", "#!/bin/sh\necho boom >&2\nexit 7\n");
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &test_paths())
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(workspace),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let err = audit.run(&mut ctx).await.expect_err("nonzero exit errors");
        assert!(format!("{err:#}").contains("exit"));
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[tokio::test]
    async fn run_persists_suppression_across_invocations() {
        let ws = workspace_with_specs(&[
            (
                "storage",
                "# storage\n\n### Requirement: Relational store\nAll data SHALL live in a relational database.\n",
            ),
            (
                "ingest",
                "# ingest\n\n### Requirement: Document store\nRecords SHALL live in a document store.\n",
            ),
        ]);
        let workspace = ws.path();
        let script = write_script(workspace, "fake.sh", "#!/bin/sh\nexit 0\n");
        let cfg = executor_cfg(&script.to_string_lossy());
        let settings_dir = TempDir::new().unwrap();
        let sub = submission(&[(
            "storage",
            "Relational store",
            "ingest",
            "Document store",
            "cannot both hold",
        )]);
        let repo = fixture_repo();
        // Both runs share one state dir so the persisted suppression state
        // from run 1 is visible to run 2.
        let paths = test_paths();

        // Run 1 reports the pair.
        {
            let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &paths)
                .with_settings_dir(settings_dir.path().to_path_buf())
                .with_submission(Some(sub.clone()));
            let mut ctx = AuditContext {
                workspace,
                repo: &repo,
                chatops_ctx: None,
                log_writer: make_log_writer(workspace),
                max_validation_retries: 0,
            };
            let outcome = audit.run(&mut ctx).await.expect("run 1");
            assert!(
                matches!(outcome, AuditOutcome::Reported { ref findings, .. } if findings.len() == 1)
            );
        }
        // Run 2 (same canon, same detection) suppresses.
        {
            let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &paths)
                .with_settings_dir(settings_dir.path().to_path_buf())
                .with_submission(Some(sub.clone()));
            let mut ctx = AuditContext {
                workspace,
                repo: &repo,
                chatops_ctx: None,
                log_writer: make_log_writer(workspace),
                max_validation_retries: 0,
            };
            let outcome = audit.run(&mut ctx).await.expect("run 2");
            assert!(
                matches!(outcome, AuditOutcome::Reported { ref findings, .. } if findings.is_empty()),
                "unchanged pair must be suppressed on the second run"
            );
        }
    }

    #[tokio::test]
    async fn workspace_unavailable_when_dot_git_missing() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws-no-git");
        std::fs::create_dir_all(&workspace).unwrap();
        let cfg = executor_cfg("/bin/true");
        let settings_dir = TempDir::new().unwrap();
        let audit = CanonContradictionAudit::new(&HashMap::new(), &cfg, &test_paths())
            .with_settings_dir(settings_dir.path().to_path_buf());
        let repo = fixture_repo();
        let mut ctx = AuditContext {
            workspace: &workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer: make_log_writer(tmp.path()),
            max_validation_retries: 0,
        };
        let log_path = ctx.log_writer.path().to_path_buf();
        let outcome = audit.run(&mut ctx).await.expect("gate returns Ok");
        assert!(matches!(
            outcome,
            AuditOutcome::WorkspaceUnavailable { .. }
        ));
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }
}
