## ADDED Requirements

### Requirement: `docs/CHATOPS.md`, `docs/OPERATIONS.md`, AND `docs/CONFIG.md` document the `scout`, `spec-it`, AND `clear-scout` verbs AND the `features.scout` config block
`docs/CHATOPS.md` SHALL contain three new subsections under the appropriate categories:

- `### scout` under chat-driven workflow with syntax, output shape, lifecycle-thread behavior, AND the disabled-verb refusal.
- `### spec-it` immediately after scout, marked as scout-thread-only, with the item-number rules AND a brief description of the translation to a propose-request.
- `### clear-scout` under operator-recovery verbs alongside `clear-perma-stuck`, `clear-revision`, `wipe-workspace`.

`docs/OPERATIONS.md` SHALL contain a section (existing onboarding section OR a new "Finding things to work on" section) describing the scout → pick → spec-it discovery loop AS the recommended pattern for both unfamiliar projects (OSS-contribution mode) AND owned projects (periodic fresh-eyes pass).

`docs/CONFIG.md` SHALL document the `features.scout.{enabled, prompt_path, max_items, include_issues, staleness_warn_days}` block with defaults, valid ranges, AND a note linking to the uniform Prompt overrides table (`a24`) for the `prompt_path` field.

The `a24` Prompt overrides table SHALL be extended with the `Scout` entry (logical id `Scout`, embedded path `prompts/scout.md`, per-workspace override `features.scout.prompt_path`, legacy field `—`).

`config.example.yaml` SHALL include the `features.scout` block commented out, with each field's default in a comment.

#### Scenario: CHATOPS.md documents the scout verb
- **WHEN** an operator reads `docs/CHATOPS.md`'s chat-driven-workflow section
- **THEN** a `### scout` subsection appears with:
  - Syntax: `@<bot> scout <repo-substring> [optional guidance]`
  - Output shape: numbered items with category, title, body, source, tractability, grouped by category
  - Lifecycle thread: top-level ack + threaded follow-ups
  - Refusals: scout disabled, ambiguous repo

#### Scenario: CHATOPS.md documents spec-it as scout-thread-only
- **WHEN** an operator reads the `### spec-it` subsection
- **THEN** the subsection explicitly names the thread-scope constraint (only valid inside a scout lifecycle thread)
- **AND** documents the item-number range check AND the propose-request translation
- **AND** notes the staleness warning behavior (warns, does not block)

#### Scenario: CHATOPS.md documents clear-scout under recovery verbs
- **WHEN** an operator reads `docs/CHATOPS.md`'s operator-recovery section
- **THEN** a `### clear-scout` subsection appears alongside `clear-perma-stuck`, `clear-revision`, `wipe-workspace`
- **AND** the subsection describes the wipe-all-scout-state-for-this-repo behavior AND its idempotence

#### Scenario: OPERATIONS.md describes the scout → pick → spec-it loop
- **WHEN** an operator reads the section describing discovery workflows
- **THEN** a paragraph names the three-step loop (scout to surface candidates, operator review, spec-it to scope work on one item)
- **AND** the section gives one example each for OSS-contribution context AND owned-project context

#### Scenario: CONFIG.md documents `features.scout`
- **WHEN** an operator reads the `features.scout` subsection
- **THEN** each field is documented with its default AND its meaning
- **AND** `max_items`'s valid range `1..=50` is named
- **AND** the `prompt_path` entry links to the Prompt overrides table

#### Scenario: Prompt overrides table includes Scout
- **WHEN** an operator reads the `## Prompt overrides` table in `docs/CONFIG.md`
- **THEN** a `Scout` row appears with embedded path `prompts/scout.md`, per-workspace override `features.scout.prompt_path`, legacy field `—`
