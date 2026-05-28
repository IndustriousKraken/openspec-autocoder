## Why

`openspec validate --strict` checks a change's spec deltas are well-formed (frontmatter present, sections named correctly, scenarios use proper WHEN/THEN structure, normative keywords appear). It does NOT verify the deltas can actually be applied to the canonical specs at archive time. This gap is the root cause of the a07 perma-stuck incident on 2026-05-27:

The change `a07-reviewer-prompt-budget-and-per-change-mode` shipped a `### Requirement: Reviewer prompt budget is operator-configurable` MODIFIED block. The title didn't exist in the canonical `code-reviewer` spec (the existing requirement is `### Requirement: AI-driven code-quality review`). `openspec validate --strict` passed. The implementer ran to completion, used ~$3 of LLM credits, produced a working diff. Then `openspec archive` aborted with `code-reviewer MODIFIED failed for header "..." not found`. The change went into the Failed bucket. Two iterations later, perma-stuck.

The check is mechanical and cheap. Every spec-delta header preconditions can be verified by string comparison against the canonical specs:

- `## ADDED Requirements` headers SHOULD NOT exist in canonical (catching duplicate-add).
- `## MODIFIED Requirements` headers SHOULD exist in canonical (catching the a07 bug).
- `## REMOVED Requirements` headers SHOULD exist in canonical (catching removal of nothing).
- `## RENAMED Requirements` `from:` headers SHOULD exist; `to:` headers SHOULD NOT exist.

Running this check BEFORE the executor saves the LLM cost AND surfaces the spec defect to the operator immediately, via the existing `.needs-spec-revision.json` mechanism (no new recovery primitive needed).

## What Changes

**New pre-flight step in the iteration's pre-executor pipeline.** Before invoking the executor against any change, autocoder SHALL parse the change's `specs/<capability>/spec.md` files, extract every delta-block header, AND verify the preconditions against the canonical `openspec/specs/<capability>/spec.md` for each affected capability.

**On any precondition violation, the change is flagged via `.needs-spec-revision.json` with an `unarchivable_deltas` field.** The existing needs-spec-revision marker schema is extended with an optional `unarchivable_deltas: [{ capability, kind, header, reason }]` field alongside the existing `unimplementable_tasks` field. The chatops alert under `AlertCategory::SpecNeedsRevision` (existing) describes the failure as "spec deltas would not archive cleanly" with the specific mismatches enumerated. The polling iteration halts the queue walk for that repo per the existing same-repo blocking policy (operationalized further by `a18`).

**The executor is NOT invoked when the pre-flight fails.** This is the principal cost savings — no LLM call, no implementation work, no diff produced. The operator sees the failure immediately, edits the spec, runs `@<bot> clear-revision <repo> <change>`, AND the next iteration retries with the corrected spec.

**The check runs on every change before every executor invocation.** No caching of "this change passed pre-flight before" — the canonical specs might have changed since (a previous iteration's archive could have updated them). Cost is negligible: parse two markdown files per affected capability, set intersection. Sub-millisecond per change.

**Pre-existing failures of the kind a17 would catch.** Any change already in `openspec/changes/` whose spec deltas would fail pre-flight on next iteration will be caught by the next polling pass after a17 ships. The marker is written, queue blocks, operator fixes. No data loss; just earlier surfacing of pre-existing latent defects.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one ADDED requirement: `Spec-delta archivability pre-flight check`. Defines the check, the marker-file extension, AND the executor-skip behavior.
  - `project-documentation` — one ADDED requirement: `OPERATIONS.md and TROUBLESHOOTING.md document the pre-flight check AND the unarchivable-deltas marker shape`.
- **Affected code:**
  - `autocoder/src/preflight/spec_archivability.rs` (new) — module containing the check:
    ```rust
    pub struct UnarchivableDelta {
        pub capability: String,
        pub kind: DeltaKind,  // Added | Modified | Removed | Renamed
        pub header: String,    // For Renamed: stored as "from <a> to <b>"
        pub reason: String,    // e.g. "header not found in canonical"
    }
    pub fn check_spec_deltas_archivable(
        workspace_root: &Path,
        change_slug: &str,
    ) -> Result<Vec<UnarchivableDelta>>;
    ```
    Returns an empty Vec on success, populated Vec on any precondition violation.
  - `autocoder/src/polling_loop.rs` (or wherever the pre-executor pipeline lives) — invoke the check BEFORE `executor.run(...)`. On non-empty result: write the extended `.needs-spec-revision.json`, fire the existing chatops alert with the new "unarchivable spec deltas" framing, halt the queue walk, return.
  - `autocoder/src/state/needs_spec_revision.rs` (or equivalent) — extend the schema with the `unarchivable_deltas` field. The existing `unimplementable_tasks` field is preserved; both can be populated, but in practice they come from different code paths (pre-flight vs agent-detected) AND only one fires per iteration.
  - `docs/OPERATIONS.md` — extend the "Spec marked as needing revision" section to describe the new failure mode.
  - `docs/TROUBLESHOOTING.md` — add an entry "openspec archive aborts with 'MODIFIED failed for header'" referencing the pre-flight that now catches this earlier.
- **Operator-visible behavior:**
  - Spec drafts with hallucinated MODIFIED titles fail FAST AND with a precise diagnostic, before any LLM cost.
  - The `.needs-spec-revision.json` marker's body includes the specific delta mismatches: `{"unarchivable_deltas": [{"capability": "code-reviewer", "kind": "Modified", "header": "Reviewer prompt budget is operator-configurable", "reason": "header not found in canonical openspec/specs/code-reviewer/spec.md"}]}`. The operator knows exactly what to fix.
  - The recovery workflow is unchanged from existing needs-spec-revision: edit the spec on local machine, push to base branch, `@<bot> clear-revision <repo> <change>` from chat.
- **Breaking:** no. Changes whose spec deltas pass the pre-flight see identical pre-spec behavior. Changes whose deltas fail were going to fail at archive time anyway — they now fail earlier AND cheaper.
- **Acceptance:** `cargo test` passes; `openspec validate a17-pre-flight-spec-delta-archivability-check --strict` passes. New unit tests cover each delta kind × each precondition (ADDED-duplicate, MODIFIED-missing, REMOVED-missing, RENAMED-from-missing, RENAMED-to-duplicate). Integration test simulates the a07 scenario: a change with an invented MODIFIED header → pre-flight catches it → marker written → executor NOT invoked.
