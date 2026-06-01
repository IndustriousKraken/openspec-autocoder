# orchestrator-cli — delta for a40-chatops-tolerant-change-args

## ADDED Requirements

### Requirement: Partial change-slug resolution in marker-clearing control-socket actions
The four marker-clearing control-socket actions — `clear_perma_stuck_marker`, `clear_revision_marker`, `ignore_for_queue_marker`, `clear_ignore_for_queue_marker` — SHALL resolve the operator-supplied `change` field as either an exact change-directory name OR a case-sensitive leading prefix, scoped to the directories carrying the action's relevant marker file. Resolution happens before any marker-removal or marker-writing filesystem call.

The per-action marker scope is:

| Action | Scope (directories carrying any of) |
| --- | --- |
| `clear_revision_marker` | `.needs-spec-revision.json` |
| `clear_perma_stuck_marker` | `.perma-stuck.json` |
| `ignore_for_queue_marker` | `.perma-stuck.json` OR `.needs-spec-revision.json` |
| `clear_ignore_for_queue_marker` | `.ignore-for-queue.json` |

Resolution algorithm: when the supplied `change` value names a directory under `<workspace>/openspec/changes/` AND that directory carries any of the scope-required markers, the resolved value is the supplied value verbatim (fast-path). Otherwise, the handler enumerates the change-root directory (skipping the archive subdirectory AND dotfile entries, matching the canonical `list_pending` skip rules), filters to directories carrying any scope-required marker, AND collects entries whose name `str::starts_with` the supplied value (case-sensitive). A unique candidate is the resolved value. Zero candidates produce a `NoMatch` error. Two or more candidates produce a `MultiMatch` error with the candidate list sorted ascending.

Error messages SHALL name the marker scope explicitly so the operator can act without consulting documentation: `no change matching prefix '<prefix>' has a .needs-spec-revision.json marker` for `clear_revision_marker`'s no-match path, AND analogous messages per action. The multi-match message SHALL list the candidates AND end with `Retype with a longer prefix or the full slug.`

The handler's success response JSON SHALL carry the resolved canonical slug in the `change` field, NOT the operator-supplied prefix, so downstream consumers (chatops formatter, journalctl, audit log) see the authoritative name.

When the supplied value exactly equals the canonical slug (the common case for operators who paste the full slug from an alert), the resolver SHALL return the value WITHOUT logging the resolution. A non-trivial resolution (prefix → canonical) SHALL log `INFO control_socket: resolved partial change '<prefix>' → '<canonical>' for action <action>` so operators reading journalctl can confirm the disambiguation.

#### Scenario: Exact slug match unchanged
- **GIVEN** `<workspace>/openspec/changes/a37-unify-llm-provider-config/.needs-spec-revision.json` exists
- **WHEN** the operator submits `clear_revision_marker` with `change: "a37-unify-llm-provider-config"`
- **THEN** the resolver returns `Ok("a37-unify-llm-provider-config")` via the exact-match fast path
- **AND** the marker file is removed
- **AND** the response is `{"ok": true, "change": "a37-unify-llm-provider-config", "url": "<repo-url>"}`
- **AND** NO `resolved partial change` INFO log is emitted (the value was already canonical)

#### Scenario: Unique prefix match resolves to canonical slug
- **GIVEN** the workspace contains exactly one change directory matching the prefix `a37` AND carrying `.needs-spec-revision.json` (`a37-unify-llm-provider-config`)
- **WHEN** the operator submits `clear_revision_marker` with `change: "a37"`
- **THEN** the resolver returns `Ok("a37-unify-llm-provider-config")`
- **AND** the marker file under the canonical directory is removed
- **AND** the response is `{"ok": true, "change": "a37-unify-llm-provider-config", "url": "<repo-url>"}` (the resolved canonical slug, NOT the supplied prefix)
- **AND** the daemon log contains `INFO control_socket: resolved partial change 'a37' → 'a37-unify-llm-provider-config' for action clear_revision_marker`

#### Scenario: Zero candidates with no matching marker produce a scope-naming error
- **GIVEN** no change directory has both the prefix match for `a99` AND a `.needs-spec-revision.json` marker
- **WHEN** the operator submits `clear_revision_marker` with `change: "a99"`
- **THEN** the resolver returns `Err(NoMatch { scope: NeedsRevision })`
- **AND** the response is `{"ok": false, "error": "no change matching prefix 'a99' has a .needs-spec-revision.json marker"}`
- **AND** no marker file is read or modified

#### Scenario: Multiple candidates produce a candidate-listing error
- **GIVEN** the workspace contains both `a37-foo/.needs-spec-revision.json` AND `a38-bar/.needs-spec-revision.json`
- **WHEN** the operator submits `clear_revision_marker` with `change: "a3"`
- **THEN** the resolver returns `Err(MultiMatch { candidates: ["a37-foo", "a38-bar"] })`
- **AND** the response is `{"ok": false, "error": "multiple changes match prefix 'a3': a37-foo, a38-bar. Retype with a longer prefix or the full slug."}`
- **AND** no marker file is read or modified

#### Scenario: Per-action scope isolates markers correctly
- **GIVEN** the workspace contains `a37-foo/.perma-stuck.json` AND `a37-foo` carries no `.needs-spec-revision.json`
- **WHEN** the operator submits `clear_revision_marker` with `change: "a37"`
- **THEN** the resolver returns `Err(NoMatch { scope: NeedsRevision })` (the wrong marker for this action's scope)
- **AND** the response error names the `.needs-spec-revision.json` scope
- **AND** the same workspace responds to `clear_perma_stuck_marker` with `change: "a37"` by resolving to `a37-foo` (the perma-stuck scope DOES include the directory)

#### Scenario: `ignore_for_queue_marker` accepts either blocking marker
- **GIVEN** the workspace contains `a37-foo/.needs-spec-revision.json` AND `a38-bar/.perma-stuck.json` AND `a39-baz` carrying neither marker
- **WHEN** the operator submits `ignore_for_queue_marker` with `change: "a37"`
- **THEN** the resolver returns `Ok("a37-foo")` (the `EitherBlocking` scope accepts `.needs-spec-revision.json`)
- **AND** submitting `change: "a38"` to the same action resolves to `a38-bar` (the `EitherBlocking` scope also accepts `.perma-stuck.json`)
- **AND** submitting `change: "a39"` returns `Err(NoMatch { scope: EitherBlocking })` with the message naming both marker files

#### Scenario: End-to-end happy path — backtick-wrapped prefix from a marker alert
- **GIVEN** the chatops alert template has fired `⚠️ \`a37-unify-llm-provider-config\` has unarchivable spec deltas (pre-flight)...`
- **AND** the workspace contains exactly one change (`a37-unify-llm-provider-config`) carrying `.needs-spec-revision.json`
- **WHEN** the operator copies the alert's wrapped slug verbatim AND submits `@<bot> clear-revision myrepo \`a37\`` (a shortened prefix wrapped in backticks)
- **THEN** the parser strips the surrounding backticks AND extracts `change: "a37"` after regex validation
- **AND** the dispatcher submits a `clear_revision_marker` control-socket action carrying `change: "a37"`
- **AND** the control-socket handler resolves the prefix to `a37-unify-llm-provider-config` via `ChangePrefixMarkerScope::NeedsRevision`
- **AND** the `.needs-spec-revision.json` marker file under `a37-unify-llm-provider-config/` is removed
- **AND** the control-socket response is `{"ok": true, "change": "a37-unify-llm-provider-config", "url": "<repo-url>"}` (the canonical slug, NOT the supplied prefix)
- **AND** the chatops dispatcher's reply text names `a37-unify-llm-provider-config`
- **AND** the daemon log records `INFO control_socket: resolved partial change 'a37' → 'a37-unify-llm-provider-config' for action clear_revision_marker`

#### Scenario: Archive directory AND dotfile entries are skipped during enumeration
- **GIVEN** the workspace contains `archive/a01-something/.needs-spec-revision.json` (under the archive subdirectory) AND `.scratch/.needs-spec-revision.json` (a dotfile dir) AND `a37-foo/.needs-spec-revision.json`
- **WHEN** the operator submits `clear_revision_marker` with `change: "a"`
- **THEN** the resolver enumerates only `a37-foo` as a candidate
- **AND** archive entries AND dotfile entries are not considered for prefix matching even when their leading characters match the prefix
