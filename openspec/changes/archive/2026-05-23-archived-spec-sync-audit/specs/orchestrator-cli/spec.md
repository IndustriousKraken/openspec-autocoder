## ADDED Requirements

### Requirement: Archived-spec-sync audit
autocoder SHALL register an `spec_sync_audit` audit in the periodic-audit framework whose job is to merge `## ADDED Requirements`, `## MODIFIED Requirements`, `## REMOVED Requirements`, and `## RENAMED Requirements` blocks from every archived change's `specs/<capability>/spec.md` files into the canonical `openspec/specs/<capability>/spec.md`. The audit exists because OpenSpec 0.18+ split the sync step out of `openspec archive` into a `/opsx:sync` skill that the core profile doesn't install (per Fission-AI/OpenSpec issue #913); without this audit, drift accumulates silently in every repo autocoder operates on.

The audit SHALL be idempotent: when no drift exists, no files are written and no commit is created. The audit SHALL walk archived changes in chronological order (by their `YYYY-MM-DD-<change>` date prefix) so that MODIFIED requirements correctly supersede earlier ADDED requirements. The audit's writes SHALL be constrained to `openspec/specs/**` via a new `WritePolicy::CanonicalSpecMerge` post-hoc check; writes outside that prefix SHALL be reverted by the audit framework.

#### Scenario: First run on a repo with historical drift backfills automatically
- **WHEN** an operator enables `spec_sync_audit` on a repo
  whose archived changes contain `## ADDED Requirements`
  blocks that never made it to the canonical
  `openspec/specs/<capability>/spec.md` (e.g. all archived
  changes from before OpenSpec 0.18's archive/sync split, or
  any archive operation since on a host without the sync
  skill installed)
- **THEN** the audit's first run identifies every drift item,
  applies the merges in chronological order, and produces
  exactly one commit titled `audit: spec-sync — merge deltas
  from N archived change(s)` (or similar) on the agent
  branch
- **AND** the iteration's existing push/PR flow ships the
  commit alongside any implementation work that pass
  produced
- **AND** the audit emits one `Finding` summarizing the
  merge (count of capabilities touched, count of
  requirements added/modified/removed/renamed)

#### Scenario: Subsequent runs on a clean repo are noops
- **WHEN** the audit runs on a repo where every archived
  change's deltas are already present in the canonical
  spec files (either because a prior audit run synced them
  OR because OpenSpec's archive step did)
- **THEN** the audit writes nothing
- **AND** no commit is created on the agent branch
- **AND** the audit returns `AuditOutcome::Reported(vec![])`
  (no findings)
- **AND** if `notify_on_clean` is the default `false`, no
  chatops post is made

#### Scenario: Drift introduced after the audit's previous run
- **WHEN** the audit's prior run synced everything AND a
  subsequent iteration archived a new change (via the
  current broken `openspec archive` behavior that doesn't
  sync) AND the audit fires again
- **THEN** the audit detects the new change's deltas as
  drift, merges them into the canonical spec, commits, AND
  ships in the iteration's PR
- **AND** the gap between archive and next-audit-run is
  bounded by the audit's configured cadence (default
  `disabled`; common operator setting: `daily`)

#### Scenario: ADDED requirement whose title already exists in canonical
- **WHEN** an archived change's `## ADDED Requirements`
  block contains a `### Requirement: <title>` AND the
  canonical spec already contains a requirement with the
  same title (because a prior archive or a hand-edit added
  it)
- **THEN** the audit treats the duplicate as MODIFIED:
  replaces the canonical block with the archived block AND
  logs a WARN naming the title and the source archive
- **AND** the merge does not error; the WARN is operator-
  informational only

#### Scenario: MODIFIED requirement whose title does NOT exist in canonical
- **WHEN** an archived change's `## MODIFIED Requirements`
  block contains a `### Requirement: <title>` AND the
  canonical spec has no requirement with that title
- **THEN** the audit treats the MODIFIED as ADDED: appends
  the block to the canonical spec AND logs a WARN naming
  the title and the source archive
- **AND** the merge does not error

#### Scenario: REMOVED requirement whose title does NOT exist in canonical
- **WHEN** an archived change's `## REMOVED Requirements`
  block names a title that the canonical spec doesn't
  contain
- **THEN** the audit logs a DEBUG (the removal is already
  in the desired end state) AND continues
- **AND** the merge does not error

#### Scenario: Capability mentioned in archives but absent from openspec/specs/
- **WHEN** an archived change's `specs/<capability>/spec.md`
  references a capability whose canonical
  `openspec/specs/<capability>/spec.md` does NOT exist
- **THEN** the audit creates the canonical file with a
  standard header (capability name, `## Purpose`, `##
  Requirements`) AND applies the merged requirements
  underneath
- **AND** the new capability spec is committed alongside
  any other merges in the same audit commit

#### Scenario: WritePolicy enforcement
- **WHEN** the `spec_sync_audit` declares
  `WritePolicy::CanonicalSpecMerge` AND completes its run
- **THEN** the audit framework's post-hoc diff check
  verifies every modified or new path is under
  `openspec/specs/`
- **AND** any path outside that prefix is reverted by the
  framework (matching the existing `OpenSpecOnly` reversion
  pattern) AND the audit's outcome is replaced with a
  failure naming the violating path

#### Scenario: Idempotency across repeated invocations
- **WHEN** the audit runs three times in succession on the
  same workspace without any other operations
- **THEN** the first run may produce a backfill commit if
  drift was present
- **AND** the second and third runs produce no commits and
  no file writes
- **AND** the audit's reported finding count drops to zero
  after the first run

#### Scenario: Forward compatibility with upstream OpenSpec sync fix
- **WHEN** a future OpenSpec release re-bundles the sync
  step into `openspec archive` (per the Fission-AI founder's
  reply on Discord, 2026-05-23, this is expected in the
  next release)
- **THEN** this audit's behavior on autocoder-archived
  changes becomes mostly-noop (because `openspec archive`
  itself now syncs)
- **AND** the audit remains useful as a backstop for repos
  onboarded with pre-existing drift, manual `openspec
  archive` operations run by operators on hosts without
  the sync skill, and any other source of drift outside
  autocoder's iteration loop
- **AND** no spec or code change to this audit is needed
  when the upstream fix lands; the idempotency contract
  means the audit silently does the right thing in both
  worlds
