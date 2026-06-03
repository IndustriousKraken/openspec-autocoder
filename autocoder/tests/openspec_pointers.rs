//! CI-enforced rule (`a41-link-openspec-conventions`): the OpenSpec
//! upstream-docs pointer SHALL stay present in the spec-drafting prompt
//! set AND `docs/README.md`.
//!
//! A prior session direct-edited eight agent-facing prompts AND
//! `docs/README.md` to carry a pointer to OpenSpec's upstream
//! documentation (`https://github.com/Fission-AI/OpenSpec/tree/main/docs`).
//! The links give agents drafting spec content — AND humans drafting
//! their first OpenSpec change — a canonical reference for scenario
//! syntax (`GIVEN`/`WHEN`/`THEN`), delta format
//! (`ADDED`/`MODIFIED`/`REMOVED`/`RENAMED`), AND requirement-header
//! rules without authoring a parallel convention document.
//!
//! The prompts are large AND the reference is a single paragraph that
//! does NOT visibly anchor the prompt's operational rules, so a future
//! contributor could trim it out without noticing. This regression test
//! is the CI backstop: for each covered file it asserts BOTH (a) the
//! literal URL substring is present, AND (b) at least one topical hint
//! is present (so the link is not left stranded without the surrounding
//! context that motivates it).
//!
//! When a future change adds a new spec-drafting prompt, removes one,
//! OR introduces a project-local convention document (e.g.
//! `openspec/AGENTS.md`), update `COVERED_FILES` below in lockstep with
//! the canonical requirement in
//! `openspec/specs/project-documentation/spec.md`.
//!
//! Determinism: file reads are the only I/O. No network, no clock, no
//! environment mutation. `CARGO_MANIFEST_DIR` is resolved at compile
//! time via `env!`, so path resolution is stable regardless of the
//! directory `cargo test` is invoked from.

use std::fs;
use std::path::PathBuf;

/// The literal substring every covered file must contain. The files
/// carry the fuller `.../tree/main/docs` form; this prefix is what the
/// check pins so the path can evolve without breaking the test, while
/// the host/org/repo identity stays locked.
const URL_SUBSTRING: &str = "https://github.com/Fission-AI/OpenSpec";

/// At least one of these topical hints must appear in each covered
/// file. Their presence is a proxy for "the link still sits next to the
/// format vocabulary that motivates it" — stripping the explanatory
/// paragraph while leaving a bare URL would drop all of these.
const TOPICAL_HINTS: &[&str] = &["GIVEN", "WHEN", "scenario", "delta", "Requirement"];

/// The covered set, as paths relative to the repository root. Mirrors
/// the canonical requirement's covered set exactly. Keep the two in
/// lockstep (see module docs).
const COVERED_FILES: &[&str] = &[
    "prompts/implementer.md",
    "prompts/implementer-revision.md",
    "prompts/chat-request-triage.md",
    "prompts/audit-triage.md",
    "prompts/missing-tests-audit.md",
    "prompts/security-bug-audit.md",
    "prompts/brownfield-draft.md",
    "prompts/scout.md",
    "docs/README.md",
];

/// Resolve the repository root from `CARGO_MANIFEST_DIR` (the
/// `autocoder/` crate directory) AND its parent. Making the resolution
/// explicit keeps the test correct regardless of how `cargo test` is
/// invoked (from the repo root or from the crate directory).
fn repo_root() -> PathBuf {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .expect("autocoder crate dir (CARGO_MANIFEST_DIR) must have a parent — the repo root")
        .to_path_buf()
}

/// Check one file's contents against both rules. Returns zero, one, or
/// two violation messages — each names the file path AND which check
/// failed. Pure: takes the already-read contents so the detection logic
/// is unit-testable without touching the filesystem.
fn check_contents(rel: &str, contents: &str) -> Vec<String> {
    let mut violations = Vec::new();
    if !contents.contains(URL_SUBSTRING) {
        violations.push(format!(
            "{rel}: missing required substring '{URL_SUBSTRING}'"
        ));
    }
    if !TOPICAL_HINTS.iter().any(|hint| contents.contains(hint)) {
        violations.push(format!(
            "{rel}: missing topical hint (one of {})",
            TOPICAL_HINTS.join(", ")
        ));
    }
    violations
}

/// Main regression test: every covered file must contain the OpenSpec
/// URL substring AND at least one topical hint. Failures are collected
/// across ALL files (NOT first-failure-only) so a contributor editing
/// several files at once sees every offender in a single run.
#[test]
fn openspec_pointer_present_in_covered_files() {
    let root = repo_root();

    let mut violations: Vec<String> = Vec::new();
    for rel in COVERED_FILES {
        let path = root.join(rel);
        match fs::read_to_string(&path) {
            Ok(contents) => violations.extend(check_contents(rel, &contents)),
            Err(e) => violations.push(format!(
                "{rel}: could not read file at {} ({e})",
                path.display()
            )),
        }
    }

    assert!(
        violations.is_empty(),
        "OpenSpec upstream-docs pointer check failed for {} item(s):\n\n{}\n\n\
         Each covered file must contain the substring '{URL_SUBSTRING}' AND \
         at least one topical hint (one of {}). The pointer gives agents AND \
         human contributors a canonical reference for scenario syntax, delta \
         format, AND requirement-header rules — see \
         openspec/specs/project-documentation/spec.md. Restore the missing \
         link/context, OR (if the covered set legitimately changed) update \
         both COVERED_FILES here AND the canonical requirement in lockstep.",
        violations.len(),
        violations.join("\n"),
        TOPICAL_HINTS.join(", "),
    );
}

/// Self-test: a file missing the URL is flagged with the exact
/// diagnostic the spec's failure scenario names. Permanently guards the
/// failure path in CI (the one-time manual smoke test in tasks.md 2.3
/// exercises the same path against a real file).
#[test]
fn check_flags_missing_url() {
    let got = check_contents("prompts/implementer.md", "no link here, but GIVEN a hint");
    assert_eq!(
        got,
        vec![
            "prompts/implementer.md: missing required substring \
             'https://github.com/Fission-AI/OpenSpec'"
                .to_string()
        ]
    );
}

/// Self-test: a file that keeps the URL but strips every topical hint
/// is flagged with the hint diagnostic.
#[test]
fn check_flags_missing_topical_hint() {
    let contents = "see https://github.com/Fission-AI/OpenSpec/tree/main/docs for details";
    let got = check_contents("prompts/audit-triage.md", contents);
    assert_eq!(
        got,
        vec![
            "prompts/audit-triage.md: missing topical hint \
             (one of GIVEN, WHEN, scenario, delta, Requirement)"
                .to_string()
        ]
    );
}

/// Self-test: when both the URL AND a topical hint are present, no
/// violation is produced.
#[test]
fn check_passes_when_both_present() {
    let contents =
        "GIVEN a scenario, see https://github.com/Fission-AI/OpenSpec/tree/main/docs";
    assert!(check_contents("docs/README.md", contents).is_empty());
}

// -------- a45-revision-summary-surfaces-in-pr-comment --------
//
// CI-enforced rule: `prompts/implementer-revision.md` must carry the
// outcome-signal section that directs the revision agent to call
// `outcome_success` with a content-shaped `final_answer`. Without it the
// PR-comment composer (orchestrator-cli "Revision execution ... posts a
// reply comment") has no substantive summary to surface. The markers
// below are the load-bearing contract; the surrounding prose is free to
// be reworded. See openspec/specs/project-documentation/spec.md.

/// The relative path to the revision prompt the a45 check covers.
const REVISION_PROMPT: &str = "prompts/implementer-revision.md";

/// Required substrings for the revision prompt's outcome-signal section.
const REVISION_OUTCOME_MARKERS: &[&str] =
    &["outcome_success", "final_answer", "declined", "Test counts"];

/// Pure detection helper: one violation line per missing marker, each
/// naming the file AND the missing substring (matching the a41
/// diagnostic shape). Pure so the failure path is unit-testable without
/// filesystem I/O. Collects ALL misses (NOT first-failure-only).
fn check_revision_outcome_markers(rel: &str, contents: &str) -> Vec<String> {
    REVISION_OUTCOME_MARKERS
        .iter()
        .filter(|needle| !contents.contains(**needle))
        .map(|needle| format!("{rel}: missing required substring '{needle}'"))
        .collect()
}

/// Main a45 regression test: the revision prompt must contain every
/// required outcome-signal marker. Misses are reported in one combined
/// listing so a contributor dropping several at once sees them all.
#[test]
fn revision_prompt_carries_outcome_signal_markers() {
    let path = repo_root().join(REVISION_PROMPT);
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => panic!(
            "{REVISION_PROMPT}: could not read file at {} ({e})",
            path.display()
        ),
    };

    let violations = check_revision_outcome_markers(REVISION_PROMPT, &contents);
    assert!(
        violations.is_empty(),
        "revision-prompt outcome-signal marker check failed for {} item(s):\n\n{}\n\n\
         The `## Outcome signal` section of {REVISION_PROMPT} must direct the revision \
         agent to call `outcome_success` with a content-shaped `final_answer` (covering \
         that `declined` is a valid outcome AND `Test counts`) so the PR-comment composer \
         has substantive text to surface — see \
         openspec/specs/project-documentation/spec.md. Restore the missing marker(s).",
        violations.len(),
        violations.join("\n"),
    );
}

/// Self-test: dropping one marker yields exactly the spec's named
/// diagnostic (the `declined`-missing failure scenario).
#[test]
fn revision_markers_flag_single_missing() {
    let contents = "call outcome_success with final_answer; cover Test counts.";
    let got = check_revision_outcome_markers(REVISION_PROMPT, contents);
    assert_eq!(
        got,
        vec![
            "prompts/implementer-revision.md: missing required substring 'declined'".to_string()
        ]
    );
}

/// Self-test: dropping two markers reports BOTH in one combined listing
/// (the spec's multiple-missing scenario).
#[test]
fn revision_markers_report_multiple_missing() {
    let contents = "call outcome_success with final_answer.";
    let got = check_revision_outcome_markers(REVISION_PROMPT, contents);
    assert_eq!(
        got,
        vec![
            "prompts/implementer-revision.md: missing required substring 'declined'".to_string(),
            "prompts/implementer-revision.md: missing required substring 'Test counts'"
                .to_string(),
        ]
    );
}

/// Self-test: when every marker is present, no violation is produced
/// (the rewording-within-contract scenario).
#[test]
fn revision_markers_pass_when_all_present() {
    let contents = "outcome_success / final_answer / declined / Test counts";
    assert!(check_revision_outcome_markers(REVISION_PROMPT, contents).is_empty());
}
