## Why

OpenSpec 0.18 split the spec-merge step out of `openspec archive` into a separate `/opsx:sync` skill. Per issue Fission-AI/OpenSpec#913, the archive workflow's "sync now" prompt points at a skill (`openspec-sync-specs`) that the core profile doesn't install, so in practice `openspec archive` archives the change directory cleanly but never propagates the change's `## ADDED` / `## MODIFIED` / `## REMOVED` requirements into the canonical `openspec/specs/<capability>/spec.md` files. An OpenSpec founder confirmed on Discord (2026-05-23) that "archive should still sync" and the next release will re-bundle the sync skill by default — but autocoder shouldn't depend on that fix. Three reasons:

1. **The fix's timing is out of autocoder's control.** "Next release" is good news but unscheduled. Meanwhile autocoder is running against ~9 of Rab's repos plus an unknown number of Fake Jeremy's, every one of which has been silently accumulating drift since archive operations started.
2. **The fix only helps repos that adopt the fixed OpenSpec version.** Existing drift in already-archived repos won't be reconciled by future-OpenSpec; only by autocoder doing it.
3. **Repos can arrive at autocoder pre-existing with drift.** New customer onboarding, repos migrated from older OpenSpec setups, repos where someone ran `openspec archive` by hand without the sync skill — all these land at autocoder's door with drift autocoder needs to handle. A passive "we'll wait for the upstream fix" stance doesn't cover these.

The bug is empirically present in autocoder's own repo: 89 unique `### Requirement:` titles appear in archived `## ADDED Requirements` blocks; only 59 made it into `openspec/specs/*/spec.md`. The 30-requirement gap includes major items (the entire periodic-audit framework, the install subcommand, the release pipeline, perma-stuck detection, throttled failure alerts, etc.). Independently spot-checked: every capability dir exists in `openspec/specs/` — the gap is at the requirement level inside those existing capability files, not at the capability level.

The audit framework is the right home: opt-in via cadence, periodic, idempotent (most runs find nothing), works on whatever repo autocoder is operating on. Drift detection IS the audit shape.

## What Changes

**New audit `archived_spec_sync_audit`**. Walks every change directory under `openspec/changes/archive/` in chronological order (the date-prefix naming makes this trivial). For each archived change, opens its `specs/<capability>/spec.md` files and merges each delta block (ADDED / MODIFIED / REMOVED / RENAMED Requirements) into the canonical `openspec/specs/<capability>/spec.md`. If any merges produced changes, commits the result as `audit: spec-sync — merge deltas from N archived change(s)` on the agent branch so the iteration's existing push/PR flow ships the sync alongside any implementation work that pass produced.

**Pure-data merge module** at `autocoder/src/spec_sync.rs`. Public API: `pub fn compute_sync_plan(archive_root: &Path, canonical_specs_root: &Path) -> Result<SyncPlan>` returning a structured list of per-capability merges to apply. `pub fn apply_sync_plan(plan: &SyncPlan, canonical_specs_root: &Path) -> Result<Vec<PathBuf>>` writes the resulting files and returns the list that actually changed (empty Vec means "no drift detected; noop"). Separating compute from apply lets the audit log what it's about to do before doing it, makes unit testing trivial (the compute function is pure given a filesystem snapshot), and gives a future `autocoder sync-specs --dry-run` CLI a natural API.

**Per-iteration sync hook IS NOT in this spec.** Initial scope is audit-only. The audit fires daily (or whatever cadence operators set) and catches drift in a bounded window. If real-world usage shows the window between archive and next-audit-run causes problems (e.g., self-heal probe failing because canonical spec is stale and `openspec validate` references something missing), a follow-up change adds the per-iteration hook. Smaller initial spec, less risk of getting the integration wrong on the first try.

**New `WritePolicy::CanonicalSpecMerge` variant**. The existing `WritePolicy::OpenSpecOnly` constrains audit writes to `openspec/changes/<change>/`. This audit writes to `openspec/specs/<capability>/spec.md` — a different prefix. Rather than loosening `OpenSpecOnly` (which would weaken the guarantee for other audits), add a new variant whose contract is: "writes are limited to files under `openspec/specs/`; post-hoc diff check reverts anything outside that prefix." This is a strict, narrow grant suitable for ONLY this audit (and any future audit that mechanically merges committed history into canonical specs).

**Idempotency contract**. Re-running the audit on a clean repo (no drift) writes nothing and produces no commit. Re-running after an upstream-OpenSpec sync fix lands (when `openspec archive` does its own sync again) likewise writes nothing because the canonical specs already match the archive deltas. The audit is harmless to keep running indefinitely.

**Backfill IS automatic**. The audit walks ALL archived changes, not just recent ones. On a repo with historical drift (like this autocoder repo's 30-requirement gap), the audit's first run produces a single large merge commit that closes the entire backfill. No separate `autocoder sync-specs --backfill` subcommand needed; the audit IS the backfill on first run.

**Operator-visible behavior on the first run** of a repo with significant drift: one commit titled `audit: spec-sync — merge deltas from N archived change(s)` containing edits to potentially many `openspec/specs/*/spec.md` files. The chatops alert (if `notify_on_clean` is the default false, no alert on the noop case; if drift detected, the existing audit-findings notification fires naming the changed capability count). Operator reviews the merge in the PR, merges if it looks right, autocoder's next iteration sees the synced canonical specs.

## Impact

- Affected specs: `orchestrator-cli` — one ADDED requirement establishing the audit's contract.
- Affected code:
  - `autocoder/src/spec_sync.rs` — NEW module with the merge logic. Pure data; ~300–500 lines including tests.
  - `autocoder/src/audits/spec_sync.rs` — NEW audit module implementing the `Audit` trait. ~100 lines; wraps `spec_sync` and produces the audit-framework outcome.
  - `autocoder/src/audits/mod.rs` — `pub mod spec_sync;` declaration; trait additions if needed.
  - `autocoder/src/cli/run.rs` — registry registration for the new audit.
  - `autocoder/src/audits/scheduler.rs` (or wherever WritePolicy lives) — `CanonicalSpecMerge` variant + post-hoc diff-check support.
  - `autocoder/src/config.rs` — add the new audit slug to the recognized-slugs list so `validate_audit_type_names` accepts it.
  - README — table row in the audits section.
  - `config.example.yaml` — entry under the registered-audits comment + a per-audit `extra` block if any knobs land (none planned for v1).
- Operator-visible behavior: opt-in audit. Operators enabling it on any repo with drift get a one-time backfill commit; subsequent runs are noops until new drift appears.
- Across-projects impact: this fixes the drift everywhere autocoder runs (Rab's 9 repos, Jeremy's repos, future onboarded repos). The audit is enabled per-repo via the existing `audits.defaults` / `repositories[].audits` cadence config — no per-repo code changes needed.
- Breaking: no.
- Forward-compatibility with upstream OpenSpec fix: when OpenSpec re-bundles sync (per the founder's note), `openspec archive` will start syncing again. This audit becomes mostly-noop in that world (it only finds drift introduced by sources OTHER than autocoder's archive operations, e.g. operator manual `openspec archive` runs without the skill installed, or pre-existing drift in newly-onboarded repos). The audit stays useful as a defensive backstop.
- Acceptance: `cargo test` passes (new tests). `openspec validate archived-spec-sync-audit --strict` passes. A unit test seeds a fixture workspace with one capability spec missing two requirements (introduced by two archived changes) and asserts the audit produces a sync plan that adds both requirements in the correct order with the correct text.
