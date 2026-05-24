## Why

The `archived-spec-sync-audit` change (merged 2026-05-23) was built on a wrong premise. Its proposal claimed OpenSpec's `archive` command was broken — that the 0.18 archive/sync split meant `openspec archive` archived the change directory but never merged deltas into canonical specs. Acting on that diagnosis, the change shipped a ~1000-line pure-Rust delta-merge module (`autocoder/src/spec_sync.rs` + `autocoder/src/audits/spec_sync.rs`) plus an opt-in audit to apply it.

Hands-on testing on 2026-05-24 disproved the premise. `openspec archive` works correctly when the host has `sync` enabled in its openspec profile — both the file move AND the canonical-spec merge succeed in one operation. The merge is byte-reasonable (modulo minor blank-line handling), aborts atomically on validation errors (no half-applied state), and creates missing canonical capabilities with a placeholder Purpose when needed.

The drift autocoder caused in its target repos has a much simpler cause: **autocoder's `queue::archive` is `std::fs::rename` in Rust and never calls `openspec archive` at all.** Every autocoder-driven archive bypasses openspec entirely and therefore bypasses any sync step openspec would have done. The OpenSpec design's optional-sync-skill quirk is a real thing operators using openspec directly should know about — but it's not why autocoder-managed repos have drift. Autocoder's drift is a self-inflicted missing-implementation problem.

The honest fix is to remove the audit and route autocoder's archive operation through `openspec archive`. That gets the sync for free, removes ~1000 lines of code we shouldn't have needed to write, and aligns autocoder's behavior with what every other openspec consumer does. The previous proposal's apologetic framing in the README ("workaround for broken upstream") also needs to come out — there was no upstream bug for autocoder's case.

For backfilling existing drift (this repo has ~30 unsynced requirements from autocoder-driven archives over the last few weeks; coterie and Jeremy's repos have similar gaps): a tiny shell-style loop using openspec archive's own re-archive behavior handles it. Move each archived dir back to the active path, re-run `openspec archive`. Skip changes already-synced (their `## ADDED` requirements already exist in canonical, so the re-archive would abort). No Rust merge code needed.

## What Changes

**1. Replace `queue::archive`'s rename with an `openspec archive` subprocess call.** The polling loop's flow becomes: executor returns `Completed` → autocoder commits the working-tree changes → autocoder runs `openspec archive <change> -y` in the workspace → on success the change is both moved AND synced. Error handling: if openspec archive aborts (validation error in the rebuilt spec), autocoder treats the change as Failed for the iteration with the openspec stderr as the reason. The `--yes` flag skips the confirmation prompt that would otherwise block non-interactive use.

**2. Delete the spec-sync audit and merge module.** Specifically:
- Remove `autocoder/src/spec_sync.rs` (the merge primitives — 500+ lines).
- Remove `autocoder/src/audits/spec_sync.rs` (the audit wrapper).
- Remove `pub mod spec_sync;` from `autocoder/src/audits/mod.rs`.
- Remove `SpecSyncAudit` registration in `autocoder/src/cli/run.rs`.
- Remove `spec_sync_audit` from the `validate_audit_type_names` recognized-slugs list.
- Remove `WritePolicy::CanonicalSpecMerge` variant (no other audit uses it; reverting keeps the WritePolicy surface narrow).
- Remove the audit's README table row + `config.example.yaml` entries.
- Remove the README addition from commit `085cb8d` that frames `openspec config profile` as a "workaround for broken upstream." Replace with a normal setup-prerequisite section: "the autocoder host needs the openspec `sync` workflow enabled (one-time `openspec config profile`). Without it, `openspec archive` will move the change directory but won't merge deltas into canonical specs — autocoder iterations will succeed but drift will accumulate."

**3. Install path documents the openspec-sync prerequisite.** The install script's existing optional steps (system deps, Claude CLI) gain a new step:

> "After installing the openspec CLI, autocoder needs the `sync` workflow enabled in your openspec profile so `openspec archive` does the canonical-spec merge. Run `openspec config profile` once on this host to enable it."

Optional automation: pipe predetermined answers to `openspec config profile`'s TUI. Fragile (breaks if openspec changes the prompts), so defer to manual operator step unless openspec exposes a `--workflows` non-interactive flag in the future.

## Impact

- Affected specs: `orchestrator-cli` — one REMOVED requirement ("Archived-spec-sync audit") + one ADDED requirement ("autocoder invokes openspec archive"). The REMOVED entry rolls back the requirement added by the previously-merged `archived-spec-sync-audit` change.
- Affected code:
  - `autocoder/src/queue.rs` — `archive()` function changes from `fs::rename` to subprocess invocation of `openspec archive`. The collision-check (`archive_collision_path`, `would_collide_on_archive`) can stay — it's a pre-flight that prevents wasted executor runs on conflicting dates, which still applies.
  - `autocoder/src/polling_loop.rs` — archive call site unchanged at the surface (still calls `queue::archive(...)`) but error handling adapts to subprocess failures.
  - `autocoder/src/spec_sync.rs` — DELETED.
  - `autocoder/src/audits/spec_sync.rs` — DELETED.
  - `autocoder/src/audits/mod.rs` — `pub mod spec_sync;` line removed.
  - `autocoder/src/cli/run.rs` — `SpecSyncAudit` registration removed.
  - `autocoder/src/config.rs` — `spec_sync_audit` removed from `validate_audit_type_names`'s known list.
  - `autocoder/src/audits/scheduler.rs` (or wherever) — `WritePolicy::CanonicalSpecMerge` variant removed.
  - README — strip the "OpenSpec is broken" framing from the openspec section, replace with a neutral setup step. Drop the audit's table row.
  - `config.example.yaml` — drop the `spec_sync_audit` entries.
- Operator-visible behavior:
  - autocoder hosts need `openspec config profile` to have `sync` enabled. The install path documents this. On a host without sync configured, autocoder iterations will succeed at the file-move level but won't sync canonical specs.
  - Operators who had configured `spec_sync_audit: daily` (none yet, since the audit just shipped) will get a startup error from `validate_audit_type_names` with the now-missing slug. The fix is to remove the entry; the released audit is gone.
- Backfill of existing drift is a SEPARATE concern handled by the companion `rebuild-canonical-specs-from-archive` change. This change is intentionally scoped to "stop creating new drift" only.
- Breaking: minor. The `spec_sync_audit` slug is removed from the recognized list. Anyone with it configured (zero people today) needs to remove their config entry.
- Honesty: the README no longer apologetically blames openspec for a problem autocoder caused. The commit message for this change explicitly names the prior misframing so future readers of `git log` see the correction.

## Acceptance

- `cargo test` passes (with the audit's tests deleted).
- `openspec validate autocoder-uses-openspec-archive --strict` passes.
- Manual: running an autocoder iteration in a test workspace produces a change directory at `openspec/changes/archive/<date>-<slug>` AND updates the corresponding canonical spec(s) — both in one openspec archive subprocess call.
