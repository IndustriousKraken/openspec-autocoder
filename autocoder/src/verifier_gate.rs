//! Verifier-gate framework (a61).
//!
//! autocoder runs a set of LLM-driven, change-lifecycle consistency checks
//! around the executor. a61 organizes them as a framework of exactly three
//! named gates positioned around the executor run:
//!
//! - the `[in]` gate — change-internal consistency, run BEFORE the executor
//!   (IS a59's change-internal contradiction pre-flight);
//! - the `[canon]` gate — change-vs-canonical consistency, run BEFORE the
//!   executor (realized by a62);
//! - the `[out]` gate — code-implements-spec, run AFTER the executor
//!   (realized by a63).
//!
//! This module owns ONLY the shared vocabulary: the gate identifiers, their
//! lifecycle positions, the stable diagnostic label each gate's findings
//! carry (`[verifier:in]` / `[verifier:canon]` / `[verifier:out]`), AND the
//! registry that maps a gate identifier to its installed implementation. a61
//! realizes ONLY the `[in]` gate — it maps to the a59 contradiction check;
//! `[canon]` and `[out]` are in the vocabulary but resolve to no installed
//! gate until a62/a63 register them. The framework invokes nothing for an
//! unrealized gate; no gate is run speculatively.
//!
//! This is a deliberately thin, low-behavior-change reframe: it changes
//! NOTHING about what the `[in]` gate decides, its config key, or its alert
//! category — it only gives the existing check a stable identity within the
//! framework so a62/a63 plug into an established frame.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::OnceLock;

/// One session outcome at the gate's runner boundary, abstracted so the
/// shared retry loop ([`run_session_with_retry`]) can tell a productive
/// session (the agent called its `submit_*` tool) from the flaky no-submission
/// case without knowing each gate's concrete outcome type. All three gates'
/// `*SessionOutcome` structs carry a `submission: Option<serde_json::Value>`;
/// this trait surfaces ONLY whether that field is present.
pub(crate) trait SessionSubmission {
    /// Whether the agent recorded a submission in this session — `true` for
    /// any `Some(payload)` (even a schema-invalid one, which the in-session
    /// correction loop already handles), `false` ONLY for the flaky case where
    /// the session ended having never called its `submit_*` tool.
    fn has_submission(&self) -> bool;
}

/// Run ONE agentic gate session via `run_one`, retrying ONLY the flaky
/// no-submission case up to `retries` additional attempts (so `retries + 1`
/// attempts total; `retries == 0` preserves the historical single-attempt
/// behavior). This is the SHARED seam all three verifier gates ([in]/[canon]/
/// [out]) wrap their "run a session, then drain the submission" step in.
///
/// Weak local models are non-deterministically flaky: they often read the
/// spec then end the session WITHOUT calling their `submit_*` MCP tool, so the
/// gate sees no submission and fails closed (holds the change / renders an
/// advisory FAILED TO RUN). The SAME model+change sometimes submits and
/// sometimes does not; a bounded retry catches the "sometimes works" case
/// before failing closed. This is the gatekeepers-fail-closed standard's
/// sanctioned transient-failure tolerance: bounded retry, NOT fail-open —
/// after the bound is exhausted the gate still returns its no-submission
/// outcome and fails closed.
///
/// Retry policy — retry ONLY the flaky case:
///   - **Retry on:** `Ok(outcome)` whose [`SessionSubmission::has_submission`]
///     is `false` (the model never called `submit_*`).
///   - **Do NOT retry on:** `Ok(outcome)` WITH a submission (a successful run —
///     Clean/Found/verdict — even a schema-invalid payload, which the
///     in-session correction loop owns), OR `Err(_)`. The `Err(_)` path covers
///     a timeout (it would just time out again — wasteful) AND an
///     unregistered-strategy / CLI-unavailable error (config-level, retrying
///     cannot fix it). Those errors short-circuit immediately; only the
///     no-submission case loops.
///
/// Each retry logs at INFO with the gate label, the change, the attempt
/// number, AND the bound (e.g. `[verifier:in] no submission on attempt 1/3;
/// retrying`). The final attempt's outcome (submission or not) is returned
/// verbatim, so the caller's existing submission/no-submission branching —
/// and its fail-closed disposition — is unchanged.
pub(crate) async fn run_session_with_retry<O, F, Fut>(
    gate: VerifierGate,
    change: &str,
    retries: u32,
    mut run_one: F,
) -> anyhow::Result<O>
where
    O: SessionSubmission,
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<O>>,
{
    let total_attempts = retries.saturating_add(1);
    let label = gate.label();
    let mut attempt: u32 = 1;
    loop {
        let result = run_one().await;
        match &result {
            // A successful run with a submission, OR any session error: return
            // immediately. Errors are NOT retried (timeout would just time out
            // again; unregistered-strategy / CLI-unavailable is config-level).
            Ok(o) if o.has_submission() => return result,
            Err(_) => return result,
            // The flaky case: the session ended with no submission. Retry up to
            // the bound, then return the (still-no-submission) outcome so the
            // caller fails closed.
            Ok(_) => {
                if attempt >= total_attempts {
                    return result;
                }
                tracing::info!(
                    change = %change,
                    "{label} no submission on attempt {attempt}/{total_attempts}; retrying"
                );
                attempt += 1;
            }
        }
    }
}

/// Where a gate runs relative to the executor. The `[in]` and `[canon]`
/// gates run BEFORE the executor (fail-CLOSED posture — a gate that cannot run
/// HOLDS the change, enforced structurally by the default-deny verdict ledger);
/// the `[out]` gate runs AFTER (advisory posture — it annotates operator
/// surfaces AND fails to a VISIBLE state, it never auto-acts).
#[cfg_attr(not(test), allow(dead_code))] // queried by a62/a63 when they realize [canon]/[out].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePosition {
    /// Runs before the executor; fail-open.
    PreExecutor,
    /// Runs after the executor; advisory.
    PostExecutor,
}

/// One of exactly three named verifier gates positioned around the executor.
/// The identifier (`in` / `canon` / `out`) is stable: it keys the registry
/// AND forms the diagnostic label so a finding is attributable to the gate
/// that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VerifierGate {
    /// Change-internal consistency, pre-executor. IS the a59 change-internal
    /// contradiction pre-flight check.
    In,
    /// Change-vs-canonical consistency, pre-executor. Realized by a62.
    Canon,
    /// Code-implements-spec, post-executor. Realized by a63.
    Out,
}

impl VerifierGate {
    /// Every gate in the fixed vocabulary, in lifecycle order.
    #[cfg_attr(not(test), allow(dead_code))] // iterated by tests and by a62/a63 startup wiring.
    pub const ALL: [VerifierGate; 3] = [VerifierGate::In, VerifierGate::Canon, VerifierGate::Out];

    /// The gate's stable identifier (`in` / `canon` / `out`). Keys the
    /// registry AND forms the diagnostic label.
    #[cfg_attr(not(test), allow(dead_code))] // asserted by tests; the framework keys the registry on the enum directly.
    pub const fn id(self) -> &'static str {
        match self {
            VerifierGate::In => "in",
            VerifierGate::Canon => "canon",
            VerifierGate::Out => "out",
        }
    }

    /// Where this gate runs relative to the executor.
    #[cfg_attr(not(test), allow(dead_code))] // dispatched on by a62/a63 when they realize their gates.
    pub const fn position(self) -> LifecyclePosition {
        match self {
            VerifierGate::In | VerifierGate::Canon => LifecyclePosition::PreExecutor,
            VerifierGate::Out => LifecyclePosition::PostExecutor,
        }
    }

    /// The gate's stable diagnostic label, e.g. `[verifier:in]`. The shared
    /// labeling token (task 1.2) that prefixes a gate's diagnostics so a
    /// finding is attributable to the gate that produced it. A `const fn`
    /// returning a `&'static str`: the label set is fixed by the enum, so it
    /// allocates nothing even on the polling-loop hot path.
    pub const fn label(self) -> &'static str {
        match self {
            VerifierGate::In => "[verifier:in]",
            VerifierGate::Canon => "[verifier:canon]",
            VerifierGate::Out => "[verifier:out]",
        }
    }

    /// Prefix one log/diagnostic line with this gate's stable label. Callers
    /// build their message AND pass it here so every gate diagnostic — log
    /// line OR operator surface — is uniformly attributable to its gate.
    pub fn label_line(self, line: &str) -> String {
        format!("{} {}", self.label(), line)
    }
}

/// The concrete check an installed gate runs. a61 realized
/// [`GateImpl::ContradictionCheck`] (the a59 change-internal contradiction
/// pre-flight, reframed as the `[in]` gate); a62 adds
/// [`GateImpl::CanonContradictionCheck`] (the change-vs-canonical pre-flight,
/// the `[canon]` gate). a63 adds the `[out]` variant when it realizes that
/// gate.
// Each variant is named for the check it maps to (`…Check`); the shared suffix
// is meaningful, not redundant noise. The lint only fires now that a63 added
// the third `…Check` variant (`enum_variant_names` needs ≥3 to trigger).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateImpl {
    /// The change-internal contradiction pre-flight check (a59). Entry point:
    /// [`crate::preflight::change_contradiction::run_agentic_contradiction_check`].
    ContradictionCheck,
    /// The change-vs-canonical contradiction pre-flight check (a62). Entry
    /// point:
    /// [`crate::preflight::canon_contradiction::run_agentic_canon_contradiction_check`].
    CanonContradictionCheck,
    /// The code-implements-spec verification check (a63) — the post-executor
    /// `[out]` gate. Entry point:
    /// [`crate::code_implements_spec::run_code_implements_spec_check`].
    CodeImplementsSpecCheck,
}

/// Maps a [`VerifierGate`] to its installed [`GateImpl`]. A gate that is NOT
/// in the map is unrealized: resolving it yields "no installed gate" AND the
/// framework invokes nothing for it (no gate is run speculatively).
///
/// [`GateRegistry::standard`] is the daemon's shared, build-once singleton
/// registry as of a61 — only the `[in]` gate is installed. It is constructed
/// lazily on first access AND handed out by `&'static` reference, so the
/// polling loop resolves gates without re-allocating the map on every pending
/// change. a62/a63 extend it by adding their
/// [`register`](GateRegistry::register) calls to `standard`'s initializer —
/// registration happens once, at startup (first access), onto the one shared
/// instance.
#[derive(Debug, Default, Clone)]
pub struct GateRegistry {
    installed: BTreeMap<VerifierGate, GateImpl>,
}

impl GateRegistry {
    /// The daemon's standard registry as of a63: all three gates are
    /// installed — the `[in]` gate (mapped to the a59 contradiction check), the
    /// `[canon]` gate (mapped to the a62 change-vs-canonical check), AND the
    /// `[out]` gate (mapped to the a63 code-implements-spec check). The
    /// vocabulary is now fully realized.
    ///
    /// Returns a `&'static` reference to a single, lazily-built instance (via
    /// [`OnceLock`]): the `BTreeMap` is allocated exactly once for the process
    /// rather than on every call, so resolving a gate on the polling-loop hot
    /// path costs no allocation.
    pub fn standard() -> &'static GateRegistry {
        static STANDARD: OnceLock<GateRegistry> = OnceLock::new();
        STANDARD.get_or_init(|| {
            let mut reg = GateRegistry::default();
            reg.register(VerifierGate::In, GateImpl::ContradictionCheck);
            reg.register(VerifierGate::Canon, GateImpl::CanonContradictionCheck);
            reg.register(VerifierGate::Out, GateImpl::CodeImplementsSpecCheck);
            reg
        })
    }

    /// Install (or replace) the implementation for a gate. Called from
    /// [`standard`](GateRegistry::standard)'s initializer to build the shared
    /// registry; a62/a63 add their `register` calls there to realize the
    /// `[canon]` / `[out]` gates (registration done once, at startup).
    pub fn register(&mut self, gate: VerifierGate, gate_impl: GateImpl) {
        self.installed.insert(gate, gate_impl);
    }

    /// Resolve a gate to its installed implementation, OR `None` when the
    /// gate is unrealized ("no installed gate"). The framework invokes
    /// nothing for a `None` resolution.
    pub fn resolve(&self, gate: VerifierGate) -> Option<GateImpl> {
        self.installed.get(&gate).copied()
    }

    /// Whether a gate has an installed implementation.
    #[cfg_attr(not(test), allow(dead_code))] // convenience predicate; used by tests and a62/a63.
    pub fn is_installed(&self, gate: VerifierGate) -> bool {
        self.installed.contains_key(&gate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- gate vocabulary: identifiers, positions, labels ----

    #[test]
    fn ids_are_stable_and_distinct() {
        assert_eq!(VerifierGate::In.id(), "in");
        assert_eq!(VerifierGate::Canon.id(), "canon");
        assert_eq!(VerifierGate::Out.id(), "out");
    }

    #[test]
    fn lifecycle_positions_match_the_framework() {
        // Two pre-executor gates, one post-executor gate.
        assert_eq!(VerifierGate::In.position(), LifecyclePosition::PreExecutor);
        assert_eq!(VerifierGate::Canon.position(), LifecyclePosition::PreExecutor);
        assert_eq!(VerifierGate::Out.position(), LifecyclePosition::PostExecutor);
    }

    #[test]
    fn labels_carry_the_stable_identifier() {
        assert_eq!(VerifierGate::In.label(), "[verifier:in]");
        assert_eq!(VerifierGate::Canon.label(), "[verifier:canon]");
        assert_eq!(VerifierGate::Out.label(), "[verifier:out]");
    }

    #[test]
    fn label_line_prefixes_the_message() {
        let line = VerifierGate::In.label_line("session failed (fail-closed)");
        assert_eq!(line, "[verifier:in] session failed (fail-closed)");
        // The identifier leads the line so the finding is attributable.
        assert!(line.starts_with("[verifier:in]"));
    }

    #[test]
    fn all_lists_exactly_the_three_gates() {
        assert_eq!(
            VerifierGate::ALL,
            [VerifierGate::In, VerifierGate::Canon, VerifierGate::Out]
        );
    }

    // ---- registry: the [in] gate is installed; [canon]/[out] are inert ----

    /// Task 2.2 (a61) / a62 / a63: every gate is resolvable by name to its
    /// installed implementation — the `[in]` gate to the change-internal
    /// contradiction check, the `[canon]` gate to the change-vs-canonical
    /// check, AND (as of a63) the `[out]` gate to the code-implements-spec
    /// check.
    #[test]
    fn standard_registry_installs_every_gate() {
        let reg = GateRegistry::standard();
        assert_eq!(
            reg.resolve(VerifierGate::In),
            Some(GateImpl::ContradictionCheck),
            "the [in] gate must map to the a59 contradiction check"
        );
        assert!(reg.is_installed(VerifierGate::In));
        assert_eq!(
            reg.resolve(VerifierGate::Canon),
            Some(GateImpl::CanonContradictionCheck),
            "the [canon] gate must map to the a62 change-vs-canonical check"
        );
        assert!(reg.is_installed(VerifierGate::Canon));
        assert_eq!(
            reg.resolve(VerifierGate::Out),
            Some(GateImpl::CodeImplementsSpecCheck),
            "the [out] gate must map to the a63 code-implements-spec check"
        );
        assert!(reg.is_installed(VerifierGate::Out));
    }

    /// As of a63 the vocabulary is fully realized: every gate in `ALL` has an
    /// installed implementation, so none resolves to "no installed gate".
    #[test]
    fn every_gate_is_realized_in_the_standard_registry() {
        let reg = GateRegistry::standard();
        for gate in VerifierGate::ALL {
            assert!(
                reg.is_installed(gate),
                "{gate:?} must be installed in the standard registry"
            );
            assert!(reg.resolve(gate).is_some(), "{gate:?} must resolve to an impl");
        }
    }

    /// The registry is extensible via `register()`: `standard()`'s initializer
    /// builds the installed set this way. Starting from an empty registry, we
    /// realize a gate to verify the builder mechanism (the standard set has no
    /// inert gates left to realize as of a63).
    #[test]
    fn register_realizes_a_previously_inert_gate() {
        let mut reg = GateRegistry::default();
        assert!(!reg.is_installed(VerifierGate::Out));
        reg.register(VerifierGate::Out, GateImpl::CodeImplementsSpecCheck);
        assert!(reg.is_installed(VerifierGate::Out));
        assert_eq!(
            reg.resolve(VerifierGate::Out),
            Some(GateImpl::CodeImplementsSpecCheck)
        );
    }

    /// `standard()` hands out one shared, build-once instance: repeated calls
    /// return the same `&'static` (no per-call allocation on the hot path).
    #[test]
    fn standard_returns_the_same_shared_instance() {
        let a = GateRegistry::standard();
        let b = GateRegistry::standard();
        assert!(
            std::ptr::eq(a, b),
            "standard() must return the same singleton, not a fresh allocation"
        );
    }

    #[test]
    fn empty_registry_installs_nothing() {
        let reg = GateRegistry::default();
        for gate in VerifierGate::ALL {
            assert_eq!(reg.resolve(gate), None, "{gate:?} must be inert in an empty registry");
        }
    }

    // ---- shared bounded-retry helper (run_session_with_retry) ----

    use std::cell::Cell;

    /// A test outcome: `has` is whether the agent submitted this session.
    struct TestOutcome {
        has: bool,
    }

    impl SessionSubmission for TestOutcome {
        fn has_submission(&self) -> bool {
            self.has
        }
    }

    /// A successful run with no submission on the first `no_submit_for`
    /// attempts, a submission afterward. `Cell` tracks the call count.
    async fn run_helper(
        gate: VerifierGate,
        retries: u32,
        no_submit_for: u32,
        calls: &Cell<u32>,
    ) -> anyhow::Result<TestOutcome> {
        run_session_with_retry(gate, "c1", retries, || {
            let n = calls.get();
            calls.set(n + 1);
            async move { Ok(TestOutcome { has: n >= no_submit_for }) }
        })
        .await
    }

    /// No submission on attempt 1, a submission on attempt 2 → returns the
    /// submitting outcome after exactly two attempts.
    #[tokio::test]
    async fn retry_returns_submission_after_one_retry() {
        let calls = Cell::new(0);
        let out = run_helper(VerifierGate::In, 2, 1, &calls).await.unwrap();
        assert!(out.has_submission());
        assert_eq!(calls.get(), 2, "one retry → two attempts");
    }

    /// No submission on every attempt → returns a no-submission outcome after
    /// exactly `retries + 1` attempts (the caller then fails closed).
    #[tokio::test]
    async fn retry_exhausts_bound_then_returns_no_submission() {
        let calls = Cell::new(0);
        // no_submit_for is huge → the closure never submits.
        let out = run_helper(VerifierGate::Canon, 2, u32::MAX, &calls).await.unwrap();
        assert!(!out.has_submission());
        assert_eq!(calls.get(), 3, "retries(2) + 1 = 3 attempts");
    }

    /// `retries == 0` → exactly one attempt.
    #[tokio::test]
    async fn retry_zero_is_one_attempt() {
        let calls = Cell::new(0);
        let out = run_helper(VerifierGate::Out, 0, u32::MAX, &calls).await.unwrap();
        assert!(!out.has_submission());
        assert_eq!(calls.get(), 1, "retries=0 → one attempt");
    }

    /// A submission on attempt 1 → exactly one attempt (no needless retry).
    #[tokio::test]
    async fn retry_no_op_when_first_attempt_submits() {
        let calls = Cell::new(0);
        let out = run_helper(VerifierGate::In, 5, 0, &calls).await.unwrap();
        assert!(out.has_submission());
        assert_eq!(calls.get(), 1, "a submission on attempt 1 needs no retry");
    }

    /// A session ERROR short-circuits immediately — it is NOT retried (a
    /// timeout would just time out again; an unregistered-strategy /
    /// CLI-unavailable error is config-level).
    #[tokio::test]
    async fn retry_does_not_retry_errors() {
        let calls = Cell::new(0);
        let result: anyhow::Result<TestOutcome> =
            run_session_with_retry(VerifierGate::In, "c1", 5, || {
                let n = calls.get();
                calls.set(n + 1);
                async move { Err::<TestOutcome, _>(anyhow::anyhow!("spawn error")) }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 1, "an error is not retried");
    }

    /// Each retry logs at INFO with the gate label, attempt number, AND bound.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn retry_logs_attempt_and_bound_at_info() {
        let calls = Cell::new(0);
        let _ = run_helper(VerifierGate::In, 2, u32::MAX, &calls).await.unwrap();
        assert!(
            logs_contain("[verifier:in] no submission on attempt 1/3; retrying"),
            "the first retry must log the gate label, attempt, and bound"
        );
        assert!(
            logs_contain("[verifier:in] no submission on attempt 2/3; retrying"),
            "the second retry must log the gate label, attempt, and bound"
        );
    }
}
