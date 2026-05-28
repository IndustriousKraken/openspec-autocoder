## 1. Delta-parsing helper

- [x] 1.1 New module `autocoder/src/preflight/spec_archivability.rs`.
- [x] 1.2 Define the public surface:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum DeltaKind { Added, Modified, Removed, Renamed }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct UnarchivableDelta {
      pub capability: String,
      pub kind: DeltaKind,
      pub header: String,       // For Renamed: stored as "from <a> to <b>" for diagnostic legibility
      pub reason: String,
  }

  pub fn check_spec_deltas_archivable(
      workspace_root: &Path,
      change_slug: &str,
  ) -> Result<Vec<UnarchivableDelta>>;
  ```
- [x] 1.3 Parser internals: for each `<workspace>/openspec/changes/<change>/specs/<cap>/spec.md`:
  - Read the file.
  - Identify each `## ADDED Requirements` / `## MODIFIED Requirements` / `## REMOVED Requirements` / `## RENAMED Requirements` block.
  - Within each block, extract every `### Requirement: <title>` line. For RENAMED, also extract the `- FROM: \`<title>\`` and `- TO: \`<title>\`` lines (the openspec rename schema).
  - Load the canonical `<workspace>/openspec/specs/<cap>/spec.md` AND extract its set of `### Requirement: <title>` lines.
  - Apply the precondition checks per kind (see §2).
- [x] 1.4 Handle edge cases:
  - Capability spec file absent in canonical → ADDED is fine (creating a new capability); MODIFIED / REMOVED / RENAMED-from all fail with reason `capability <cap> has no canonical spec — cannot modify/remove/rename within it`.
  - Empty delta block (heading present, no requirements) → no checks fire (treats as a no-op).
  - Malformed delta block (missing `### Requirement:`) → log WARN, skip the block; openspec validate should already catch the well-formedness issue.

## 2. Precondition checks

- [x] 2.1 **ADDED**: title MUST NOT exist in canonical. Violation reason: `header already exists in canonical openspec/specs/<cap>/spec.md — use MODIFIED instead`.
- [x] 2.2 **MODIFIED**: title MUST exist in canonical (exact match, character-for-character). Violation reason: `header not found in canonical openspec/specs/<cap>/spec.md (this is the a07-style bug; check spelling AND capitalization)`.
- [x] 2.3 **REMOVED**: title MUST exist in canonical. Violation reason: `header not found in canonical openspec/specs/<cap>/spec.md — cannot remove non-existent requirement`.
- [x] 2.4 **RENAMED**: `from:` title MUST exist; `to:` title MUST NOT exist. Two checks per RENAMED entry; either failure produces a violation.
- [x] 2.5 Tests cover each kind × each precondition:
  - ADDED title that exists in canonical → flagged.
  - ADDED title that doesn't exist → passes.
  - MODIFIED title that exists → passes.
  - MODIFIED title with one character different → flagged (the a07 case).
  - REMOVED title that exists → passes.
  - REMOVED title that doesn't exist → flagged.
  - RENAMED from→to where from exists AND to doesn't → passes.
  - RENAMED from→to where from doesn't exist → flagged.
  - RENAMED from→to where to already exists → flagged.

## 3. Marker-file schema extension

- [x] 3.1 In `autocoder/src/state/needs_spec_revision.rs` (or wherever the marker is defined), extend the schema:
  ```rust
  #[derive(Serialize, Deserialize, Debug, Clone)]
  pub struct NeedsSpecRevision {
      pub change: String,
      pub marked_at: DateTime<Utc>,
      #[serde(default, skip_serializing_if = "Vec::is_empty")]
      pub unimplementable_tasks: Vec<UnimplementableTask>,
      #[serde(default, skip_serializing_if = "Vec::is_empty")]
      pub unarchivable_deltas: Vec<UnarchivableDeltaRecord>,
      pub revision_suggestion: String,
      pub operator_action: String,
  }
  #[derive(Serialize, Deserialize, Debug, Clone)]
  pub struct UnarchivableDeltaRecord {
      pub capability: String,
      pub kind: String,    // "Added" / "Modified" / "Removed" / "Renamed"
      pub header: String,
      pub reason: String,
  }
  ```
- [x] 3.2 Backwards-compatible: pre-spec markers (with only `unimplementable_tasks` populated) deserialize unchanged. Post-spec markers MAY have either OR both arrays populated.
- [x] 3.3 Tests: serialize a marker with only `unarchivable_deltas` populated; deserialize matches expected shape. Round-trip with mixed populations.

## 4. Pre-executor pipeline integration

- [x] 4.1 In the polling iteration's per-change pre-executor pipeline (likely `autocoder/src/polling_loop.rs` or sibling), AFTER the existing `openspec validate --strict` check AND BEFORE `executor.run(...)`:
  - Call `check_spec_deltas_archivable(workspace_root, change_slug)`.
  - If empty vec returned: proceed to executor.
  - If non-empty: short-circuit:
    1. Write `.needs-spec-revision.json` with the `unarchivable_deltas` field populated AND `revision_suggestion` text auto-generated from the violations.
    2. Post a chatops alert via the existing `AlertCategory::SpecNeedsRevision` (subject to the 24h throttle). Body names the change AND enumerates the violations.
    3. Halt the queue walk for this iteration (per the existing same-repo blocking policy; `a18` extends this to perma-stuck too).
    4. Return from this iteration's change-processing loop without invoking the executor for ANY subsequent changes.
- [x] 4.2 The marker's `revision_suggestion` is auto-generated:
  ```
  Pre-flight check found 1 unarchivable spec delta:
  - capability=code-reviewer kind=Modified header="Reviewer prompt budget is operator-configurable" reason="header not found in canonical openspec/specs/code-reviewer/spec.md (this is the a07-style bug; check spelling AND capitalization)"

  Edit openspec/changes/<change>/specs/<capability>/spec.md to use the
  exact canonical header. After fixing, push the spec change AND clear
  this marker via @<bot> clear-revision <repo> <change>.
  ```
- [x] 4.3 Tests:
  - Mock workspace with a change whose MODIFIED header doesn't match canonical → pre-flight returns non-empty → marker written → executor NOT invoked → chatops alert fires.
  - Mock workspace with a clean change (passes pre-flight) → executor IS invoked (unchanged behavior).
  - Marker written contains the expected `unarchivable_deltas` array AND the auto-generated suggestion.

## 5. Docs

- [x] 5.1 In `docs/OPERATIONS.md`'s `Spec marked as needing revision` section, add a paragraph describing the new pre-flight failure mode: the daemon detected spec deltas that wouldn't archive cleanly (e.g. a MODIFIED header that doesn't match canonical). The marker's `unarchivable_deltas` array lists each violation. Operator fixes the spec AND clears the marker.
- [x] 5.2 In `docs/TROUBLESHOOTING.md`, add an entry titled "openspec archive aborts with 'MODIFIED failed for header'" describing:
  - Pre-a17 behavior: archive failed after the implementer ran, change perma-stuck.
  - Post-a17 behavior: pre-flight catches the issue BEFORE the implementer runs; needs-spec-revision marker written immediately.
  - The marker's `unarchivable_deltas` field enumerates exactly what's wrong AND what to fix.

## 6. Spec deltas

- [x] 6.1 `openspec/changes/a17-pre-flight-spec-delta-archivability-check/specs/orchestrator-cli/spec.md` ADDs `Spec-delta archivability pre-flight check`.
- [x] 6.2 `openspec/changes/a17-pre-flight-spec-delta-archivability-check/specs/project-documentation/spec.md` ADDs the OPERATIONS.md + TROUBLESHOOTING.md updates requirement.

## 7. Verification

- [x] 7.1 `cargo test` passes (new + existing).
- [x] 7.2 `openspec validate a17-pre-flight-spec-delta-archivability-check --strict` passes.
- [x] 7.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
- [ ] 7.4 Manual verification: stage a change with an invented MODIFIED header; run the daemon; observe pre-flight failure + marker + no LLM cost.
