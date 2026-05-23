## 1. Delta-parsing primitives

- [x] 1.1 Create `autocoder/src/spec_sync.rs`. Define:
  ```rust
  pub struct ParsedDelta {
      pub added: Vec<Requirement>,
      pub modified: Vec<Requirement>,
      pub removed: Vec<String>,           // requirement titles only
      pub renamed: Vec<RenamedRequirement>,
  }
  pub struct Requirement {
      pub title: String,                  // e.g. "Daemon entry point"
      pub block_text: String,             // verbatim heading + body + scenarios, ready to paste
  }
  pub struct RenamedRequirement {
      pub from: String,
      pub to: String,
      pub new_block: Option<Requirement>, // some RENAMED entries carry replacement text
  }
  ```
- [x] 1.2 `pub fn parse_delta_spec(path: &Path) -> Result<ParsedDelta>` — reads an archive's `specs/<capability>/spec.md` and splits into the four sections. Each `### Requirement: <title>` block is captured verbatim (heading + body + all `#### Scenario:` blocks) up to the next `### Requirement:` or next `## ` section heading.
- [x] 1.3 `pub fn parse_canonical_spec(path: &Path) -> Result<CanonicalSpec>` — reads a canonical `openspec/specs/<capability>/spec.md`. Returns:
  ```rust
  pub struct CanonicalSpec {
      pub preamble: String,               // everything before "## Requirements"
      pub requirements: Vec<Requirement>, // each ### Requirement: block
      pub trailing: String,               // anything after the last requirement (rare)
  }
  ```
  The function tolerates malformed canonical specs (missing `## Requirements` section, etc.) — returns an explanatory Err rather than silently producing garbage.
- [x] 1.4 Tests:
  - `parse_delta_spec_extracts_added_modified_removed_renamed` against a synthetic delta file containing all four sections.
  - `parse_delta_spec_handles_section_order_variation` (some archives have MODIFIED before ADDED, etc.).
  - `parse_canonical_spec_round_trips` (parse, serialize, compare).
  - `parse_canonical_spec_errors_on_missing_requirements_header` — returns Err naming the missing structure.

## 2. Merge logic

- [x] 2.1 `pub fn apply_delta(canonical: &mut CanonicalSpec, delta: &ParsedDelta) -> MergeReport` mutates the canonical in place. Rules:
  - For each `delta.added`: if no requirement with that title exists in canonical, append. If one exists, log a WARN, treat as MODIFIED (replace).
  - For each `delta.modified`: replace the canonical requirement with the same title. If none exists, log a WARN, treat as ADDED (append).
  - For each `delta.removed`: remove the canonical requirement with that title. If none exists, log a DEBUG (already gone), no error.
  - For each `delta.renamed`: rename the canonical requirement's title from `from` to `to`. If `new_block` is Some, also replace the body with `new_block.block_text`. If no requirement matches `from`, log WARN, fall back to ADDED of `new_block` if present.
- [x] 2.2 `MergeReport` captures what happened: counts of added/modified/removed/renamed actually applied, plus any warnings ("requirement X was MODIFIED but didn't exist in canonical", etc.).
- [x] 2.3 `pub fn serialize_canonical(spec: &CanonicalSpec) -> String` produces the file text: preamble + `## Requirements\n\n` + each requirement's block separated by `\n\n` + trailing.
- [x] 2.4 Tests:
  - `apply_added_appends_when_absent` + `apply_added_replaces_when_present_with_warn`
  - `apply_modified_replaces` + `apply_modified_appends_with_warn_when_absent`
  - `apply_removed_removes` + `apply_removed_noop_when_absent`
  - `apply_renamed_changes_title` + edge cases
  - `serialize_round_trip` — parse → mutate → serialize → reparse → compare

## 3. Sync planner

- [x] 3.1 `pub struct SyncPlan { pub per_capability: BTreeMap<String, CapabilityPlan> }` and `pub struct CapabilityPlan { pub canonical_path: PathBuf, pub deltas: Vec<(String, ParsedDelta)> }` — the `String` keys are the archived-change names so the merge order is deterministic chronological (date-prefix sort).
- [x] 3.2 `pub fn compute_sync_plan(workspace: &Path) -> Result<SyncPlan>`:
  - Scan `<workspace>/openspec/changes/archive/` for dirs matching `YYYY-MM-DD-<change>`.
  - Sort by name (chronological).
  - For each, look for `specs/<capability>/spec.md` files. Parse each.
  - For each capability touched, append `(change_name, delta)` to the corresponding `CapabilityPlan.deltas` vec.
  - For capabilities that appear in any delta, set `canonical_path` to `<workspace>/openspec/specs/<capability>/spec.md`.
- [x] 3.3 `pub fn apply_sync_plan(plan: &SyncPlan) -> Result<Vec<PathBuf>>`:
  - For each capability in the plan: open + parse the canonical spec (or build an empty one if the file doesn't exist — the WARN-fallback in §2.1 handles "ADDED requirement with no canonical predecessor").
  - For each delta in chronological order, call `apply_delta`.
  - After all deltas applied, serialize, compare with original file contents. If unchanged: skip. If changed: write the new content + add the path to the return vec.
  - Returns the list of paths actually modified. Empty vec means "no drift; noop."
- [x] 3.4 Tests:
  - `compute_sync_plan_finds_all_capabilities_across_chronological_archives`
  - `apply_sync_plan_idempotent_on_clean_repo` — pre-place a canonical spec that already matches the merged deltas; assert empty return vec.
  - `apply_sync_plan_backfill` — pre-place empty canonical spec; seed two archives with ADDED requirements; assert the resulting canonical has both requirements in chronological order.

## 4. New WritePolicy variant

- [x] 4.1 In `autocoder/src/audits/scheduler.rs` (or wherever `WritePolicy` is defined), add a `CanonicalSpecMerge` variant. Its post-hoc diff check accepts modifications under `openspec/specs/**` AND rejects anything else (same shape as `OpenSpecOnly` but with a different allowed prefix).
- [x] 4.2 Update the existing scheduler tests to cover the new variant's rejection behavior (e.g., if an audit declaring `CanonicalSpecMerge` modifies a file outside `openspec/specs/`, the post-hoc check reverts).

## 5. The audit

- [x] 5.1 Create `autocoder/src/audits/spec_sync.rs`. Define `pub struct SpecSyncAudit { ... }`. `Audit` trait impl:
  - `audit_type() = "spec_sync_audit"`
  - `requires_head_change() = false` (drift can exist without HEAD movement)
  - `write_policy() = WritePolicy::CanonicalSpecMerge`
  - `run(ctx: &mut AuditContext) -> Result<AuditOutcome>`:
    1. `spec_sync::compute_sync_plan(ctx.workspace)`
    2. `spec_sync::apply_sync_plan(&plan)`
    3. If the result is empty (no drift): return `AuditOutcome::Reported(vec![])` (or whatever the audit's "clean" outcome is; check existing audits for the precedent).
    4. Otherwise: the files are already written. The audit framework's normal flow handles `git add -A && git commit`. Construct a `Finding` summarizing the merge (capability count, requirement counts per category) and return `AuditOutcome::Reported(vec![finding])`.
- [x] 5.2 Register the audit in `autocoder/src/cli/run.rs`'s registry init (alongside `ArchitectureBrightlineAudit`, etc.).
- [x] 5.3 Add the slug `spec_sync_audit` to `validate_audit_type_names`'s recognized-slugs list in `config.rs`.
- [x] 5.4 Tests in `audits::spec_sync::tests`:
  - `audit_no_drift_returns_empty_findings_and_no_commit`
  - `audit_backfills_existing_drift_writes_canonical_and_commits` — full end-to-end with a fixture repo containing archived changes that have never been synced.

## 6. Documentation

- [x] 6.1 README "Periodic audits" section gains a new row in the audit table for `spec_sync_audit`. Description: "Walks every archived change's `specs/<capability>/spec.md` deltas and merges any ADDED/MODIFIED/REMOVED/RENAMED requirements that haven't yet landed in the canonical `openspec/specs/<capability>/spec.md` files. Idempotent — most runs find nothing. Useful for: repos that pre-date the OpenSpec `archive`/`sync` split, repos onboarded with pre-existing drift, repos where operators occasionally run `openspec archive` by hand without the sync skill installed."
- [x] 6.2 In the same section, add a one-line caveat: "This audit exists because OpenSpec 0.18+ split sync into a separate skill that isn't installed in the core profile; the upstream is expected to re-bundle in a future release. Until then (and as a defensive backstop afterward), this audit catches the drift."
- [x] 6.3 `config.example.yaml` — add the slug to the commented audit-list in the `audits:` block, and add an entry under `audits.settings.spec_sync_audit: {}` (no `extra` knobs in v1; the empty mapping signals the audit takes default settings).

## 7. Spec delta

- [x] 7.1 Author the ADDED requirement under `orchestrator-cli` titled "Archived-spec-sync audit." Scenarios: drift detected (writes + commits), no drift (noop), backfill on first run with historical drift (single commit closes the gap), idempotency (re-running on clean repo writes nothing), WARN logging on MODIFIED-without-canonical-predecessor cases.

## 8. Verification

- [x] 8.1 `cargo test` passes.
- [x] 8.2 `openspec validate archived-spec-sync-audit --strict` passes.
