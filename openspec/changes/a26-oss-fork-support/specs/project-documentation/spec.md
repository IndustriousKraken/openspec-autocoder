## ADDED Requirements

### Requirement: `docs/CHATOPS.md`, `docs/OPERATIONS.md`, AND `docs/CONFIG.md` document the OSS-fork workflow knobs AND the `sync-upstream` verb
`docs/CHATOPS.md` SHALL contain a `### sync-upstream` subsection under operator-driven verbs documenting the syntax, the rebase behavior, the conflict-abort behavior, AND the explicit no-push guarantee (the verb fetches + rebases the workspace's base branch but does NOT push to any remote).

`docs/OPERATIONS.md` SHALL contain an "OSS contribution workflow" section describing the recommended setup AS a discrete operator workflow:

1. Fork the upstream project on GitHub.
2. Clone the fork as the autocoder workspace.
3. Configure the `upstream` block pointing at the upstream repo.
4. Set `auto_submit_pr: false`.
5. Configure `spec_storage.path` pointing at a sibling specs repo.
6. (Optional) Configure a tighter `executor.implementer.prompt_path` emphasizing minimal-diff + follow-existing-conventions style. The section SHALL include a sample prompt snippet operators can adapt.

The section SHALL document the typical operator loop: `scout` → `spec-it` → review the auto-generated fork PR → merge into fork — then manually `gh pr create` to upstream.

`docs/CONFIG.md` SHALL document each new field with default, validation rules, AND a cross-link to the OPERATIONS.md OSS-workflow section:

- `spec_storage.path: Option<String>` — workspace-relative OR absolute path; SHALL be a git working tree containing `openspec/`; validation rules listed.
- `upstream.{remote, branch, url}` — defaults named; validation rules listed.
- `auto_submit_pr: bool` — default `true`; behavior described per polling-iteration outcome.

`config.example.yaml` SHALL include all three blocks commented out with each field's default in a comment.

#### Scenario: CHATOPS.md documents sync-upstream
- **WHEN** an operator reads `docs/CHATOPS.md`'s operator-driven-verbs section
- **THEN** a `### sync-upstream` subsection appears with:
  - Syntax: `@<bot> sync-upstream <repo-substring>`
  - Behavior: fetches the upstream remote, rebases the configured base branch, posts the result
  - Conflict behavior: rebase aborted, files named, operator advised to resolve manually
  - No-push guarantee: the verb never pushes to origin OR the fork

#### Scenario: OPERATIONS.md OSS-workflow section is complete
- **WHEN** an operator reads `docs/OPERATIONS.md`'s "OSS contribution workflow" section
- **THEN** the section lists the six-step setup in order
- **AND** includes a sample tighter implementer-prompt snippet operators can adapt
- **AND** documents the scout → spec-it → review → merge-fork → manual-upstream-PR loop

#### Scenario: CONFIG.md documents each new field
- **WHEN** an operator reads `docs/CONFIG.md`'s per-repo-config section
- **THEN** subsections appear for `spec_storage`, `upstream`, AND `auto_submit_pr`
- **AND** each subsection names the field's default, validation rules, AND cross-links to OPERATIONS.md for the workflow context
- **AND** the `auto_submit_pr` subsection explicitly notes the chatops notification difference (`📦 Branch pushed` vs `✅ PR opened`) on the two settings

#### Scenario: config.example.yaml includes the three blocks
- **WHEN** an operator opens `config.example.yaml`
- **THEN** commented-out blocks for `spec_storage`, `upstream`, AND `auto_submit_pr` appear under the per-repo configuration section
- **AND** each field's default value is named in a comment
- **AND** a header comment links to `docs/OPERATIONS.md`'s OSS-workflow section for usage guidance
