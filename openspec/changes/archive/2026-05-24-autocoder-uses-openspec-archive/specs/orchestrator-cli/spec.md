## REMOVED Requirements

### Requirement: Archived-spec-sync audit
The previously-added `spec_sync_audit` audit (introduced by the `archived-spec-sync-audit` change, archived 2026-05-23) is being removed. Its premise — that `openspec archive` is broken and autocoder must implement its own Rust delta-merge — was incorrect. Hands-on testing showed `openspec archive` performs the canonical-spec merge correctly when the host has the openspec `sync` workflow configured. autocoder's drift was caused by `queue::archive` bypassing openspec entirely (doing a Rust `fs::rename` instead of invoking `openspec archive`), not by any upstream defect. The "Switch to openspec archive" requirement below replaces the audit's role; the new "sync-specs --backfill subcommand" handles the one-shot reconciliation of pre-existing drift without needing the Rust merge module.

The associated code is removed in this same change: `autocoder/src/spec_sync.rs`, `autocoder/src/audits/spec_sync.rs`, the `WritePolicy::CanonicalSpecMerge` variant, the `spec_sync_audit` slug from `validate_audit_type_names`, and the `config.example.yaml` entries.

## ADDED Requirements

### Requirement: autocoder invokes openspec archive for the archive step
autocoder SHALL perform per-change archive operations by invoking `openspec archive <change> -y` as a subprocess in the workspace directory, rather than doing its own filesystem move. The `-y` flag suppresses confirmation prompts so the subprocess runs cleanly in the non-interactive polling-loop context. On exit code 0, autocoder treats the change as successfully archived (the change directory has moved to `openspec/changes/archive/<UTC-date>-<slug>/` AND the canonical specs at `openspec/specs/<capability>/spec.md` have been merged with the change's `## ADDED`/`## MODIFIED`/`## REMOVED`/`## RENAMED` deltas). On any non-zero exit, autocoder treats the iteration as Failed for that change, with the openspec stderr as the failure reason; the change stays at the active path for the operator to investigate.

The merge step requires the openspec host profile to have the `sync` workflow enabled (one-time `openspec config profile`). Without `sync`, `openspec archive` will move the change directory but the canonical-spec merge will not run. autocoder iterations on such a host succeed at the file-move level; drift accumulates until either the operator enables `sync` and re-runs the backfill subcommand, OR (when OpenSpec re-bundles `sync` by default in a future release) the host's openspec installation acquires the workflow automatically.

#### Scenario: Successful archive merges canonical specs
- **WHEN** autocoder finishes implementing change `<slug>`,
  commits the working tree, and invokes
  `openspec archive <slug> -y`
- **AND** the host's openspec profile has `sync` enabled
- **THEN** the subprocess exits 0
- **AND** the change directory has moved from
  `openspec/changes/<slug>/` to
  `openspec/changes/archive/<UTC-date>-<slug>/`
- **AND** each capability spec under
  `openspec/specs/<capability>/spec.md` named in the
  change's deltas has been updated with the requirement
  blocks from the corresponding delta section

#### Scenario: openspec archive failure surfaces as Failed iteration
- **WHEN** `openspec archive <slug> -y` exits non-zero
  (validation error in the rebuilt canonical spec, the
  archive destination collides with an existing dated dir,
  the change is malformed, openspec is missing from PATH,
  etc.)
- **THEN** autocoder treats the change as Failed for the
  iteration with the openspec stderr (truncated to a
  reasonable size for log/alert display) as the failure
  reason
- **AND** the change stays at
  `openspec/changes/<slug>/` (the active path) for the
  operator to investigate
- **AND** the standard per-change failure handling applies
  (failure-state counter increments, perma-stuck after
  threshold, queue walk halts for this iteration per the
  existing halt-on-non-archive semantic)

#### Scenario: Host without openspec sync configured
- **WHEN** autocoder runs on a host whose openspec profile
  does NOT have `sync` enabled
- **AND** an iteration calls `openspec archive <slug> -y`
- **THEN** the subprocess still exits 0 (archive's file
  move always succeeds), the change is archived correctly,
  but the canonical specs at `openspec/specs/` are NOT
  updated for this change's deltas
- **AND** drift accumulates: the change's `## ADDED`
  requirements are documented in the archived entry but
  not present in the canonical spec
- **AND** the operator can reconcile via
  `autocoder sync-specs --backfill` (see below)

#### Scenario: openspec missing from PATH
- **WHEN** the openspec CLI is not on the autocoder user's
  PATH
- **THEN** `Command::new("openspec")` returns an
  ErrorKind::NotFound IO error
- **AND** autocoder surfaces this as the Failed reason for
  the change with an explicit "openspec not found on PATH"
  message and a pointer to the README's openspec install
  step
- **AND** the daemon does NOT crash or halt — the iteration
  fails, the polling loop continues to the next sleep

Backfill of pre-existing drift is a separate concern handled by the companion `rebuild-canonical-specs-from-archive` change. This change is scoped strictly to "stop creating new drift."
