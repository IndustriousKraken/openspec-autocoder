## ADDED Requirements

### Requirement: Wipe-workspace confirmation shows live repository context
The first-step warning message for `@<bot> wipe-workspace <repo>` SHALL include a context preview drawn from the same live data the per-repo `status` command surfaces. The preview names the workspace path being deleted, the currently-busy state (`idle` or `working on <change> (started <age> ago) — will be cancelled`), a one-line queue summary, and any active git-tracked operator markers that would persist across the wipe. Sections collapse when their underlying data is empty (no marker section when no markers exist; queue clause collapses to `empty queue` when all categories are zero). The trailing `Reply 'confirm' within 60 seconds to proceed.` line is unchanged.

#### Scenario: Confirmation message names the in-flight change when busy
- **WHEN** an operator posts `@<bot> wipe-workspace myrepo` AND the daemon is currently working on change `audit-proposal-self-validation` (busy marker present, started 5 minutes ago)
- **THEN** the first-step warning text contains `Currently: working on \`audit-proposal-self-validation\` (started 5m ago) — will be cancelled`
- **AND** the warning text contains the workspace path being deleted
- **AND** the warning text contains the queue clause

#### Scenario: Confirmation message reads `idle` when no iteration is in flight
- **WHEN** an operator posts `@<bot> wipe-workspace myrepo` AND no busy marker exists for the repo
- **THEN** the warning text contains `Currently: idle`
- **AND** the warning text does NOT contain a `— will be cancelled` clause

#### Scenario: Active markers section appears only when markers exist
- **WHEN** the repo has at least one `.perma-stuck.json` OR `.needs-spec-revision.json` marker file under any active or excluded change
- **THEN** the warning text contains an `Active markers (git-tracked; preserved across the wipe):` section listing each marker as `• <change> (<marker-file>)`
- **WHEN** the repo has no such markers
- **THEN** the warning text does NOT contain the active-markers section at all (no empty section, no `(none)` placeholder)

#### Scenario: Queue clause collapses to `empty queue` when all categories are zero
- **WHEN** the repo's pending, waiting, and excluded queue categories are all empty
- **THEN** the warning text's queue line reads `Queue (continues after wipe): empty queue`

#### Scenario: User-controlled fields are Slack-escaped
- **WHEN** a change name appearing in the queue clause OR the markers section contains a `<` character (despite the parser's allowlist; belt-and-braces)
- **THEN** the rendered warning text contains `&lt;` in place of the literal `<`

### Requirement: Wipe-workspace drains the in-flight iteration before deleting
On `confirm`, the daemon SHALL signal the per-repo polling task's per-iteration cancel token, await the per-repo `iteration_drained` Notify with a timeout of `executor.wipe_drain_timeout_secs` seconds (default 30, clamped at 300 with WARN), then perform the directory deletion. The deletion runs regardless of whether the drain completed within the timeout — the directory is going to be gone either way; the drain is a politeness, not a hard precondition. The reply text names which of four drain outcomes occurred so operators see at a glance whether the iteration drained cleanly or whether it was stuck enough to require force.

#### Scenario: Iteration drains cleanly within the timeout
- **WHEN** a wipe is confirmed AND the per-repo polling task has an in-flight iteration AND the iteration exits within `executor.wipe_drain_timeout_secs` of receiving the cancel signal
- **THEN** the success reply text contains `(drained cleanly in <Xs>)` where X is the elapsed seconds (one-decimal precision)
- **AND** the workspace directory is deleted after the drain
- **AND** no SIGTERM-shaped failure log entry (exit status 143) appears in `journalctl` for the cancelled iteration

#### Scenario: Drain timeout fires; wipe proceeds anyway
- **WHEN** a wipe is confirmed AND the in-flight iteration does NOT exit within the configured timeout
- **THEN** the success reply text contains `(drain timeout — iteration may have been stuck)`
- **AND** the workspace directory is deleted regardless of the drain not completing
- **AND** the daemon logs a WARN naming the stuck iteration's change for operator follow-up

#### Scenario: No iteration in flight short-circuits the drain
- **WHEN** a wipe is confirmed AND the per-repo polling task has no in-flight iteration (between iterations, in the inter-iteration sleep) AND the per-iteration cancel handle is `None`
- **THEN** the success reply text contains `(no iteration in flight)`
- **AND** no Notify is awaited; the wipe proceeds immediately to the directory deletion

#### Scenario: Workspace already absent renders the existing outcome
- **WHEN** a wipe is confirmed AND the workspace directory does not exist on disk AND no iteration is in flight
- **THEN** the success reply text contains `(already absent)` (the existing pre-this-change outcome wording is preserved for the idempotent no-op case)

#### Scenario: Per-iteration cancel does NOT propagate to the global cancel
- **WHEN** a wipe is confirmed AND the per-iteration cancel fires
- **THEN** only the in-flight iteration exits
- **AND** the per-repo polling task itself remains alive
- **AND** the global daemon-shutdown cancel token is not affected
- **AND** the next polling tick fires normally, observes the missing workspace, and re-clones via the existing `workspace::ensure_initialized` path
