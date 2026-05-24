## ADDED Requirements

### Requirement: Rebuild canonical specs from archive
autocoder SHALL ship a mechanism to fully rebuild every canonical spec under `openspec/specs/` from the archived change history under `openspec/changes/archive/`. The mechanism SHALL be exposed via a CLI subcommand (`autocoder sync-specs --rebuild`) for operator use against any workspace AND via a chatops verb (`@<bot> rebuild-specs <repo>`) for in-channel triggering against daemon-managed repos. The rebuild SHALL iterate archives in chronological order, invoke `openspec archive` for each to replay the deltas onto a freshly-cleared canonical state, and preserve each archive directory's original date prefix via in-place rename after openspec produces a today-dated entry.

There is intentionally no incremental "sync only the missing changes" mode: incremental backfill is unreliable when drift is mid-history rather than end-of-history (later changes' MODIFIED requirements may have been built on top of merged versions of earlier changes; re-applying skipped earlier changes onto current canonical produces an incorrect end state). Full rebuild is the only safe operation; it's cheap enough that the simplicity is worth more than the small optimization a smarter mode would provide.

#### Scenario: Rebuild produces correct canonical state from archive history
- **WHEN** an operator runs
  `autocoder sync-specs --rebuild --workspace <path>` against
  a repo whose canonical specs are missing requirements that
  ARE present in the archived changes' deltas
- **THEN** the subcommand removes every existing canonical
  spec under `openspec/specs/<capability>/`
- **AND** iterates each archived change in chronological
  order (by name's date prefix)
- **AND** for each: moves the dated dir out of archive,
  invokes `openspec archive <slug> -y`, openspec applies
  the deltas (creating or updating canonical specs as
  needed), and the dir returns to archive with its original
  date prefix preserved via in-place rename
- **AND** at the end, every canonical spec contains every
  requirement from every archived change's deltas, in the
  correct chronologically-applied order

#### Scenario: Rebuild on a repo with no drift is a noop diff
- **WHEN** the rebuild runs on a repo whose canonical specs
  already match what would be produced by chronological
  replay (no drift)
- **THEN** the subcommand still runs the full rebuild cycle
  (clear + replay all archives) — there's no separate "is
  there drift?" mode
- **AND** `git diff openspec/specs/` after the rebuild
  shows no semantic changes (possibly minor formatting
  differences from openspec's serialization, but no
  requirement adds/removes/modifications)
- **AND** the operator reviewing the rebuild PR sees an
  empty-or-cosmetic diff and either merges (harmless) or
  declines

#### Scenario: Date prefixes preserved via in-place rename
- **WHEN** the rebuild processes archive
  `2026-05-15-foo-bar`
- **AND** `openspec archive foo-bar -y` succeeds, producing
  `archive/<today>-foo-bar`
- **THEN** the subcommand renames the new entry back to the
  original: `mv archive/<today>-foo-bar archive/2026-05-15-foo-bar`
- **AND** the archive directory's chronological order is
  preserved across the rebuild — subsequent rebuilds
  iterate in the same correct order
- **AND** the rebuild itself produces no net diff in the
  archive directory tree (each entry moves out and back
  with the same name)

#### Scenario: openspec archive failure during rebuild
- **WHEN** the rebuild is processing N changes and one
  fails (`openspec archive <slug> -y` exits non-zero — e.g.
  a delta references a requirement that openspec's
  validator rejects in the rebuilt context)
- **THEN** the subcommand logs an ERROR with the openspec
  stderr
- **AND** leaves the failing change at the active path
  (`openspec/changes/<slug>`) for the operator to inspect
- **AND** continues to the next archived change (subsequent
  changes may also fail if they depend on the failed one;
  these accumulate in the report)
- **AND** at the end the subcommand prints a summary listing
  every successful and every failed change with stderr
  excerpts, and exits non-zero

#### Scenario: Chatops verb schedules rebuild for next iteration
- **WHEN** an operator posts
  `@<bot> rebuild-specs <repo-substring>` in a chatops
  channel the listener is watching AND the substring
  resolves to exactly one configured repo
- **THEN** the listener submits a
  `RebuildSpecs { url, immediate: false }` action to the
  control socket
- **AND** the control socket sets `pending_rebuild = true`
  on the named repo's polling task in-memory state
- **AND** the bot replies in-channel:
  `✓ rebuild scheduled for <repo> — will run within ~Ns
  (current iteration must finish first)`
- **AND** when the polling loop's current iteration (if
  any) finishes, the next iteration checks the flag,
  clears it, runs the rebuild instead of the normal queue
  walk, commits the result, and the existing push/PR flow
  ships a PR with a recognizable title (e.g.
  `spec rebuild: <N> capability(ies) rebuilt`)

#### Scenario: --immediate cancels current iteration before rebuilding
- **WHEN** an operator runs
  `autocoder sync-specs --rebuild --immediate
  --workspace <path>` against a workspace where a daemon
  iteration is currently in progress
- **THEN** the subcommand reads the busy marker, sends
  SIGTERM to the recorded executor pid, and waits up to
  30 seconds for the busy marker to be released
- **AND** once released (or after the 30s timeout with a
  WARN log), runs the rebuild
- **AND** any partial workspace state left by the killed
  iteration is cleaned by the rebuild's first git-status
  check + dirty-workspace recovery (the existing
  recover-dirty-workspace-mid-iteration infrastructure)

#### Scenario: Without --immediate, CLI blocks waiting for iteration to finish
- **WHEN** an operator runs
  `autocoder sync-specs --rebuild --workspace <path>` (no
  `--immediate`) AND a daemon iteration is in progress
- **THEN** the CLI polls the busy marker periodically,
  logs progress so the operator can see what's happening,
  AND blocks until the iteration finishes naturally
- **AND** once the iteration releases the busy marker, the
  CLI proceeds with the rebuild
- **AND** the CLI never invokes SIGTERM in this mode

#### Scenario: Chatops verb does not support --immediate
- **WHEN** an operator posts
  `@<bot> rebuild-specs <repo-substring> --immediate`
- **THEN** the parser does NOT recognize `--immediate` as
  a valid argument in chatops; the verb parses as
  `rebuild-specs` with the entire remainder as the
  repo-substring (which won't match), OR the parser
  rejects the malformed invocation
- **AND** the bot replies with the same error shape used
  for any unrecognized verb shape: `✗ no repo matched
  '<repo-substring> --immediate'; configured: <list>`
- **AND** operators wanting `--immediate` must SSH to the
  daemon host and invoke the CLI directly

#### Scenario: Rebuild on a workspace with no daemon (local clone)
- **WHEN** the operator runs the CLI against a local clone
  of a repo (no autocoder daemon running on this host;
  no busy marker present)
- **THEN** the rebuild proceeds immediately
- **AND** `--immediate` and the absence of `--immediate`
  behave identically (no iteration to coordinate with)
- **AND** the operator commits + pushes the rebuild
  manually (the CLI does not push)

#### Scenario: Rebuild discards hand-edited canonical content
- **WHEN** a canonical spec contains a `## Purpose`
  paragraph OR a `### Requirement:` that was hand-edited
  into existence without any archived change introducing
  it
- **THEN** the rebuild discards that content (no archive
  references it, so the rebuilt canonical doesn't include
  it)
- **AND** any capability spec that openspec creates from
  scratch during the rebuild gets a placeholder Purpose
  (openspec's default: `"TBD - created by archiving
  change <X>. Update Purpose after archive."`)
- **AND** the README documents this loss-on-rebuild
  behavior so operators don't run rebuild expecting
  hand-edits to survive

#### Scenario: End-of-rebuild chatops notification — success with drift
- **WHEN** a rebuild iteration runs, every archived change
  re-archives successfully (`report.failed == 0`), the
  rebuild produces modified canonical files, and the
  iteration's push + PR creation succeed
- **THEN** exactly one chatops notification fires when
  chatops is configured:
  `✓ rebuild complete for <repo>: PR <pr_url> opened —
  <N> capability(ies) updated from <M> archived change(s)`
- **AND** the notification is NOT gated on
  `failure_alerts_enabled` or `pr_opened_enabled` (this
  is a direct response to an operator-triggered command;
  the operator wants the completion signal regardless of
  other notification toggles)
- **AND** the existing PR-opened notification ALSO fires
  per the established contract — operators see two posts:
  the generic "PR opened" notification and this rebuild-
  specific completion notification

#### Scenario: End-of-rebuild chatops notification — no drift
- **WHEN** a rebuild iteration runs AND every archived
  change re-archives successfully AND no canonical files
  end up modified (the rebuild reproduced the existing
  canonical exactly — no drift was present)
- **THEN** no commit is created (nothing to stage), no PR
  opens, no PR-opened notification fires
- **AND** exactly one chatops notification fires when
  chatops is configured:
  `✓ rebuild complete for <repo>: no drift detected,
  canonical specs already in sync`
- **AND** the operator gets explicit closure on the
  rebuild they requested — no silent disappearance

#### Scenario: End-of-rebuild chatops notification — partial failure
- **WHEN** a rebuild iteration runs AND one or more
  archived changes fail to re-archive (e.g. openspec
  archive exits non-zero on them; per the existing
  `Per-change failure during backfill does not abort the
  whole run` scenario, the rebuild continues with the
  remaining changes)
- **THEN** if any successful changes produced canonical
  modifications: those modifications are committed and a
  PR opens (containing the partial result)
- **AND** exactly one chatops notification fires:
  `⚠️ rebuild for <repo> completed with <N> failure(s);
  PR <pr_url-or-"(no PR — every change failed)"> opened
  with successful <M> change(s).
  Failed: <slug1>, <slug2>, ... [and K more].
  See journalctl -u autocoder for openspec stderr details.`
- **AND** the failed-slugs list truncates to the first 10
  entries with an `"and K more"` suffix to keep the
  notification body manageable in chat clients
- **AND** the failed changes' directories remain at the
  active path (`openspec/changes/<slug>/`) for the
  operator to inspect — they are NOT moved back to
  archive automatically

#### Scenario: End-of-rebuild notification when chatops is not configured
- **WHEN** a rebuild iteration completes AND
  `chatops_ctx.is_none()` (the daemon has no chatops
  configured)
- **THEN** no chatops post is attempted
- **AND** the rebuild iteration's outcome is unchanged
  (the existing INFO log lines + PR-creation flow still
  fire normally per their respective contracts)
- **AND** the operator monitors progress via
  `journalctl -u autocoder` as with any other iteration
