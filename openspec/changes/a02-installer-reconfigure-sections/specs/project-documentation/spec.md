## ADDED Requirements

### Requirement: Documentation surfaces the `--reconfigure` verb across CLI, DEPLOYMENT, and CONFIG
The repository SHALL document the `autocoder install --reconfigure <section>` verb in three places, each scoped to its audience: `docs/CLI.md` (the CLI reference, for operators looking up the flag), `docs/DEPLOYMENT.md` (in the source-to-binary switching section, as one of the post-install workflows), AND `docs/CONFIG.md` (near the `audits.defaults.*` schema table, as a cross-link for operators looking up that block).

#### Scenario: CLI.md documents the verb with its three accepted values
- **WHEN** an operator reads `docs/CLI.md`
- **THEN** the page contains an `install` entry naming the `--reconfigure <section>` flag
- **AND** the documented accepted values are `audits`, `reviewer`, and `chatops` (exact strings, no additional values)
- **AND** the entry names the mutual-exclusion with `--non-interactive` and the per-section behavior (audits patches in place; reviewer / chatops diff-confirm)
- **AND** the entry names the post-patch `sudo -u autocoder autocoder reload` step

#### Scenario: DEPLOYMENT.md mentions `--reconfigure` as the section-edit alternative
- **WHEN** an operator reads `docs/DEPLOYMENT.md`'s `Switching from source-build to binary updates` section (added by `a01`)
- **THEN** the section contains a paragraph describing `--reconfigure` as the "edit one section without re-doing the whole wizard" tool
- **AND** the paragraph uses the audits example as the most common use case
- **AND** the paragraph explains that `repositories` changes are handled via `autocoder reload` instead, so `--reconfigure repos` is intentionally absent

#### Scenario: CONFIG.md cross-links from the audits schema
- **WHEN** an operator reads `docs/CONFIG.md`'s `audits:` block
- **THEN** the section contains a one-line note: `Operators can re-prompt these cadences via \`autocoder install --reconfigure audits\` as an alternative to editing YAML directly.`
- **AND** the note links to `docs/CLI.md` for the full flag reference
