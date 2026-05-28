## ADDED Requirements

### Requirement: Spec-delta archivability pre-flight check
Before invoking the executor against any change, autocoder SHALL verify that every spec-delta block in the change's `specs/<capability>/spec.md` files satisfies the header preconditions that `openspec archive` enforces at archive time. The check is mechanical AND cheap: parse each delta block, compare its `### Requirement: <title>` headers against the canonical `openspec/specs/<capability>/spec.md` for the same capability, AND verify per-kind preconditions:

- **ADDED**: title MUST NOT exist in canonical (duplicate-add → flag).
- **MODIFIED**: title MUST exist in canonical, exact match character-for-character (the a07-incident class — invented MODIFIED titles → flag).
- **REMOVED**: title MUST exist in canonical (remove-nothing → flag).
- **RENAMED**: `from:` title MUST exist; `to:` title MUST NOT exist.

On ANY precondition violation, autocoder SHALL write `.needs-spec-revision.json` with the existing schema EXTENDED by an `unarchivable_deltas: [{ capability, kind, header, reason }]` field, post the existing chatops alert under `AlertCategory::SpecNeedsRevision` (subject to the 24h throttle, body enumerating the violations), AND halt the queue walk for this iteration per the existing same-repo blocking policy. The executor SHALL NOT be invoked for this change OR any subsequent change in the same iteration. The principal cost savings: no LLM call against a change whose deltas would fail at archive time.

The check runs on EVERY change before EVERY executor invocation. No caching — the canonical specs might have changed since the last check (a previous iteration's archive could have updated them).

#### Scenario: MODIFIED header missing from canonical is flagged before executor runs
- **WHEN** a change's `specs/code-reviewer/spec.md` contains a `## MODIFIED Requirements` block with header `### Requirement: Reviewer prompt budget is operator-configurable`
- **AND** the canonical `openspec/specs/code-reviewer/spec.md` does NOT contain that title
- **THEN** the pre-flight check returns one `UnarchivableDelta` with `kind=Modified`, `header="Reviewer prompt budget is operator-configurable"`, `reason="header not found in canonical openspec/specs/code-reviewer/spec.md ..."`
- **AND** autocoder writes `.needs-spec-revision.json` with `unarchivable_deltas` populated
- **AND** the executor is NOT invoked for this change
- **AND** no LLM cost is incurred
- **AND** the chatops alert fires under `AlertCategory::SpecNeedsRevision` with body enumerating the violation

#### Scenario: ADDED header duplicate is flagged
- **WHEN** a change's ADDED requirements block contains a title that already exists in canonical
- **THEN** the pre-flight check flags it with `kind=Added`, `reason="header already exists in canonical openspec/specs/<cap>/spec.md — use MODIFIED instead"`

#### Scenario: REMOVED header that doesn't exist is flagged
- **WHEN** a change's REMOVED requirements block contains a title that does NOT exist in canonical
- **THEN** the pre-flight check flags it with `kind=Removed`, `reason="header not found in canonical openspec/specs/<cap>/spec.md — cannot remove non-existent requirement"`

#### Scenario: RENAMED with invalid `from:` is flagged
- **WHEN** a change's RENAMED requirements block has a `from:` title that doesn't exist in canonical
- **THEN** the pre-flight check flags it with `kind=Renamed`, `header="from <a> to <b>"`, `reason="from-title not found in canonical openspec/specs/<cap>/spec.md"`

#### Scenario: RENAMED with `to:` colliding with existing canonical title is flagged
- **WHEN** a change's RENAMED requirements block has a `to:` title that ALREADY exists in canonical (as a different requirement)
- **THEN** the pre-flight check flags it with `kind=Renamed`, `reason="to-title already exists in canonical openspec/specs/<cap>/spec.md — rename would create a duplicate"`

#### Scenario: Clean spec passes pre-flight without ceremony
- **WHEN** every delta block's header preconditions are satisfied
- **THEN** the pre-flight check returns an empty Vec
- **AND** the executor IS invoked (pre-flight is no-op for clean specs)
- **AND** no marker is written
- **AND** no chatops alert fires

#### Scenario: Capability without canonical spec accepts only ADDED
- **WHEN** a change's `specs/<new-cap>/spec.md` introduces a capability that doesn't yet exist in canonical
- **AND** the change's delta blocks are all `## ADDED Requirements`
- **THEN** the pre-flight check passes (no canonical to compare against; new capabilities are fine)
- **WHEN** the same change includes a `## MODIFIED Requirements` block for the new capability
- **THEN** the pre-flight flags it with `reason="capability <cap> has no canonical spec — cannot modify within it"`

#### Scenario: Marker schema is backwards-compatible
- **WHEN** the daemon writes a `.needs-spec-revision.json` with `unarchivable_deltas` populated AND `unimplementable_tasks` empty
- **THEN** the on-disk JSON has both fields (the empty one serialized as `[]` OR omitted via `skip_serializing_if`)
- **WHEN** the daemon reads a pre-spec `.needs-spec-revision.json` (only `unimplementable_tasks` field, no `unarchivable_deltas`)
- **THEN** deserialization succeeds; `unarchivable_deltas` defaults to empty
- **AND** the operator workflow for the pre-spec marker case (edit tasks.md, clear marker) is unchanged

#### Scenario: Check runs on every iteration, no caching
- **WHEN** a change passes pre-flight on iteration N
- **AND** between iterations N AND N+1 the canonical spec is updated such that the change's delta is no longer archivable (e.g. a sibling change archived AND renamed the requirement the MODIFIED targets)
- **THEN** the pre-flight on iteration N+1 catches the new mismatch AND flags the change
- **AND** the check does NOT memoize prior passes
