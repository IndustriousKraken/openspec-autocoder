# project-documentation Specification

## Purpose
TBD - created by archiving change project-documentation. Update Purpose after archive.
## Requirements
### Requirement: Implementing agents update user-facing documentation
Agents that implement OpenSpec changes SHALL update `README.md` and any relevant `docs/` files when their change affects user-visible behavior — CLI commands, configuration keys, deployment steps, public APIs, environment variables, or architectural shifts that the operator must understand to run or maintain the system.

#### Scenario: User-facing change includes documentation update
- **WHEN** an implementing agent's change adds, modifies, or removes a user-visible feature, configuration option, CLI argument, or operational step
- **THEN** the agent's commit MUST also include corresponding edits to `README.md` AND/OR the relevant files under `docs/` so the documentation accurately reflects the new behavior
- **AND** if the change introduces a feature that is partially-implemented or aspirational, the documentation MUST mark that feature as such (e.g. with a "Status: aspirational" or "Planned" note) rather than describing it as fully working

#### Scenario: Internal-only change does not require docs update
- **WHEN** a change is purely internal — refactoring, internal renaming, dependency bumps, test-only changes, build-system adjustments that do not affect user invocation
- **THEN** no documentation update is required
- **AND** the agent SHOULD note the internal-only scope in the commit message so reviewers can confirm the assessment

#### Scenario: Removing a user-facing feature
- **WHEN** an implementing agent's change removes a user-visible feature
- **THEN** the agent's commit MUST also remove the corresponding documentation, OR mark it as deprecated/removed with a date and rationale
- **AND** README sections describing the removed feature MUST NOT be left in a misleading state suggesting the feature still exists

### Requirement: Developer-facing test-reliability reference
The repository SHALL include a developer-facing reference document at `docs/test-reliability.md` that enumerates known sources of test-suite flakiness, their root causes (where determinable), and their dispositions. The document is a living artifact: implementing agents that introduce new tests, fix existing flakes, or discover new flake patterns SHALL update the disposition table.

The reference is NOT a user-facing spec — it does not describe runtime behavior — but it is in-scope for `project-documentation` because it serves the same audience (operators and implementing agents) and the same purpose (preserving non-obvious knowledge that would otherwise have to be re-derived from logs and grep).

#### Scenario: Adding a new test that's expected to be reliable
- **WHEN** an implementing agent adds a new test that uses deterministic primitives (no wall-clock, no env mutation without a lock, no shared mockito server, no hard-coded /tmp paths)
- **THEN** no update to `docs/test-reliability.md` is required — the document tracks known risks, not every test that's correctly written

#### Scenario: Discovering a new flake
- **WHEN** an implementing agent observes a test failing intermittently AND can characterize the root cause (timing race, env race, mockito port collision, filesystem collision, etc.)
- **THEN** the agent SHALL add an entry to the disposition table in `docs/test-reliability.md` with the test name, module, category, and chosen disposition (one of `fixed-in-this-change`, `mitigated`, `accepted-known-flaky`, `unfixable-needs-architecture-change`, `not-flaky-on-inspection`)
- **AND** if the disposition is `fixed-in-this-change`, the agent's commit MUST include the fix and the entry MAY be moved to a "Resolved flakes" section in a follow-up cleanup
- **AND** if the disposition is `unfixable-needs-architecture-change`, the entry SHALL describe the architectural change required (e.g. "wire an injectable clock through `AuditScheduler::run`") so a future change has a starting point

#### Scenario: Investigating a reported flake whose name cannot be located in the tree
- **WHEN** an operator reports a flake by name AND the name cannot be matched in the current tree or git history
- **THEN** the investigating agent SHALL document the negative result in `docs/test-reliability.md` (with the grep commands tried) AND proceed with a category-based audit rather than blocking on the named test
- **AND** the report MAY note that the originally-named test was unlocatable, so future operators don't reopen the same investigation looking for the same ghost

### Requirement: config.example.yaml is the canonical operator reference for the YAML schema
The repository SHALL maintain `config.example.yaml` at the repo root as the operator-facing reference for every configurable field accepted by `Config` and its nested types. Every YAML-deserializable field — including fields whose default behavior makes them safe to omit — SHALL appear in the example, either as an active default value or as a commented annotation explaining what it does and what values are accepted. When a change ships a new configurable field, the change's commit MUST also update `config.example.yaml` so the example never lags the schema.

A CI-enforceable check (typically a unit test under `config::tests`) SHALL fail when a documented field name does not appear as a substring in the example file. This catches omissions at build time rather than at operator-onboarding time.

#### Scenario: Adding a new configurable field
- **WHEN** an implementing agent adds a new YAML-deserializable field
  to any struct used in `Config` deserialization (top-level
  `Config`, `RepositoryConfig`, `ExecutorConfig`, `GithubConfig`,
  `ReviewerConfig`, `ChatOpsConfig`, `AuditsConfig`, etc.)
- **THEN** the same commit SHALL update `config.example.yaml` with
  a corresponding entry — either active (showing the default value)
  or commented (showing typical usage with an explanatory comment)
- **AND** the same commit SHALL update the coverage test's field-name
  list so the test continues to assert the new field is present
- **AND** the change's commit message or PR description names the
  new field so reviewers can confirm all three artifacts (struct
  field, example entry, test list entry) landed together

#### Scenario: Coverage test catches a missing field
- **WHEN** a developer adds a new field to the schema AND updates
  the example AND updates the test field-name list, but the example
  entry has a typo (e.g., `recreate_fork_on_init` instead of
  `recreate_fork_on_reinit`)
- **THEN** the coverage test fails with a message naming the
  missing field name AND pointing the developer at both
  `config.example.yaml` and the test's field-name list so the
  source of truth is unambiguous

#### Scenario: A field is genuinely never useful in the example
- **WHEN** a new field is added that has no plausible operator-set
  value (e.g., an internal-only flag that only autocoder itself
  flips at runtime, exposed in the struct purely for serde
  round-tripping)
- **THEN** the field is still added to `config.example.yaml` as a
  commented entry whose comment explicitly notes "internal — do
  not set" so the operator knows it exists AND that they should
  not configure it
- **AND** the coverage test continues to assert the field name
  appears in the file (the comment counts as a mention)

#### Scenario: Existing optional features ship un-commented in the example
- **WHEN** the example file documents an optional feature (e.g.,
  `reviewer:`, `chatops:`, `audits:`) that is disabled by default
- **THEN** the entire feature block SHALL appear commented out,
  with a header comment explaining what the feature does and a
  pointer to the relevant README section
- **AND** each nested field within the commented block SHALL appear
  at least once so an operator who uncomments the block sees every
  knob the feature exposes

### Requirement: Install script is a thin bootstrap for `autocoder install`
The repository SHALL ship `install.sh` at the repo root as a minimal bootstrap (target ≤ 80 lines including comments) whose sole responsibilities are: detect OS + architecture, resolve a binary version (default latest production tag from the GitHub Releases API; overridable via `--version` flag or `AUTOCODER_VERSION` env var), download the binary and its SHA-256 checksum, verify the checksum, place the binary on PATH, and `exec autocoder install "$@"`. All wizard logic, system-user creation, config generation, systemd unit rendering, and optional Claude-CLI bootstrap SHALL live in the `autocoder install` subcommand (a tested Rust subcommand), NOT in bash.

This split exists because the project's automation model relies on autocoder being able to verify its own behavior via `cargo test`. Bash code cannot meaningfully be exercised inside autocoder's sandbox (no sudo, no useradd, no systemctl). Keeping `install.sh` small enough to read in one sitting AND moving the real logic into Rust where it can be unit-tested is the only way to maintain the install path without depending on manual smoke-testing.

README SHALL recommend the install script as the default onboarding path. The existing source-build instructions SHALL be preserved under a "Manual install from source" heading for contributors and operators who specifically want to avoid downloaded binaries.

#### Scenario: First-time install via the curl one-liner
- **WHEN** a new operator runs
  `curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/main/install.sh | bash`
- **THEN** `install.sh` detects OS + architecture, queries the
  GitHub Releases API for the latest production tag, downloads
  the matching binary asset + its `.sha256` file, verifies the
  checksum, places the binary at `/usr/local/bin/autocoder` (with
  sudo if needed) OR `~/.local/bin/autocoder` (no sudo path),
  AND execs `autocoder install`
- **AND** `autocoder install` handles every subsequent prompt
  via its own Rust-tested wizard flow

#### Scenario: install.sh is bounded in size and complexity
- **WHEN** a reviewer inspects `install.sh`
- **THEN** the entire file is ≤ 80 lines including comments
  AND contains no operator prompts, no useradd, no systemctl,
  no apt-get, no claude-installer invocation — those concerns
  live in `autocoder install`
- **AND** every step in install.sh is verifiable by visual
  inspection (the file is small enough to read in one minute)

#### Scenario: Reinstall / upgrade
- **WHEN** an operator re-runs `install.sh` against an existing
  install
- **THEN** the script downloads the latest binary (or the
  version named via `--version` / `AUTOCODER_VERSION`), verifies
  its checksum, and replaces the existing binary at the install
  path
- **AND** the subsequent `exec autocoder install` detects the
  existing config, prints a status block, and exits 0 without
  re-prompting

#### Scenario: README positions the install script as the default
- **WHEN** a new visitor reads README from the top
- **THEN** the first major section after the project description
  is "Quick install" featuring the curl one-liner prominently
- **AND** a one-paragraph explanation of the bootstrap →
  `autocoder install` handoff makes clear that the heavy lifting
  is tested Rust code, not unverified bash
- **AND** the source-build content appears LATER under a
  clearly-labeled "Manual install from source" heading

### Requirement: Tagged releases produce architecture-specific binaries on GitHub Releases
The repository SHALL contain a GitHub Actions workflow at `.github/workflows/release.yml` triggered by tag pushes matching `v*`. The workflow SHALL gate on a green `cargo test --release` run, then build release binaries for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `aarch64-apple-darwin`, attaching each binary and its SHA-256 checksum file to a GitHub Release named after the tag.

The asset naming convention is contractual — a downstream install script relies on it. Asset names SHALL be `autocoder-<tag>-<rust-target-triple>` for the binary and `<binary-name>.sha256` for the checksum file. The checksum file SHALL be in the `<hex-digest>  <binary-name>` format produced by `sha256sum` so `sha256sum -c` can verify it without reformatting.

#### Scenario: Production tag triggers a full release
- **WHEN** a maintainer pushes a tag matching `^v\d+\.\d+\.\d+$`
  (e.g. `v1.2.3`)
- **THEN** the workflow runs the test gate, builds three target
  binaries, attaches them and their `.sha256` files to a new
  GitHub Release at that tag
- **AND** the release is published as a normal (non-pre-release)
  release

#### Scenario: Pre-release tag triggers a pre-release
- **WHEN** a maintainer pushes a tag containing a dash suffix
  (e.g. `v1.2.3-rc1`, `v1.2.3-dev`, `v0.5.0-beta.2`)
- **THEN** the same build pipeline runs and uploads assets
- **AND** the resulting GitHub Release is marked as `prerelease:
  true` so the GitHub UI badges it visibly and the install
  script's "production releases" filter skips it by default

#### Scenario: Test gate failure halts the release
- **WHEN** the `cargo test --release` step on the test job fails
- **THEN** the matrix-build and publish jobs do NOT run
  (`needs: test` dependency chain)
- **AND** no GitHub Release is created for the tag; the failing
  workflow run is visible in the Actions tab so the maintainer
  can investigate and either delete the tag (recommended) or
  push a fix tag (e.g. `v1.2.4`) to retry

#### Scenario: Asset naming is stable
- **WHEN** the workflow runs for tag `v1.2.3`
- **THEN** the release exposes exactly six assets:
  `autocoder-v1.2.3-x86_64-unknown-linux-gnu`,
  `autocoder-v1.2.3-x86_64-unknown-linux-gnu.sha256`,
  `autocoder-v1.2.3-aarch64-unknown-linux-gnu`,
  `autocoder-v1.2.3-aarch64-unknown-linux-gnu.sha256`,
  `autocoder-v1.2.3-aarch64-apple-darwin`,
  `autocoder-v1.2.3-aarch64-apple-darwin.sha256`
- **AND** the install script's download URL pattern
  `https://github.com/<owner>/<repo>/releases/download/<tag>/<asset-name>`
  resolves directly to these files

#### Scenario: Adding a new target triple
- **WHEN** an implementing agent extends the build matrix with a
  new target (e.g. `x86_64-apple-darwin` for Intel Mac support)
- **THEN** this requirement's enumeration of target triples is
  updated in the same commit, AND the install script's
  architecture-detection logic is updated to match, so all three
  artifacts (workflow, this spec, install script) stay aligned

### Requirement: DEPLOYMENT.md documents switching from source-build to binary upgrades
`docs/DEPLOYMENT.md` SHALL include a section titled `Switching from source-build to binary updates` that targets operators whose existing deployment was built from source — typically a hand-written systemd unit pointing at a config under the operator's home directory — and who want to switch to the released-binary upgrade path. The section SHALL document two paths: the `install.sh --config-dir <existing-config-dir>` invocation that leverages the systemd-probe detection AND a manual `curl + sha256sum + install -m 755` sequence for operators who prefer to skip the bash wrapper entirely.

#### Scenario: Section exists and names both upgrade paths
- **WHEN** an operator reads `docs/DEPLOYMENT.md`
- **THEN** a section titled `Switching from source-build to binary updates` appears between `Recommended: install from a binary release` and `## 1. Install the binary`
- **AND** the section names the `install.sh --config-dir <path>` invocation
- **AND** the section names the manual-download alternative using the contractual asset name pattern `autocoder-<tag>-<triple>`
- **AND** the section names the post-update step `sudo systemctl restart autocoder`

#### Scenario: Section explains why a bare install.sh re-run is unsafe pre-systemd-probe
- **WHEN** the operator reads the section
- **THEN** the text explains that an unqualified `install.sh` re-run on a source-built deployment would have overwritten the systemd unit AND lost any custom `Environment="PATH=..."` entries the operator added (a common case is the openspec CLI living under `~/.nvm/versions/node/<v>/bin/`)
- **AND** the text explains that the install wizard's systemd probe now prevents that outcome — but only when the operator passes `--config-dir <existing-config-dir>` OR the existing unit can be detected via `systemctl show autocoder.service`

#### Scenario: Cross-link forward to unattended-update story is correct when it lands
- **WHEN** the section completes its source-to-binary content
- **THEN** the closing paragraph cross-links to `Unattended updates via cron` (the anchor lands when `update.sh` ships under a later stacked change)
- **AND** until that change merges, the cross-link is a dead anchor within the same file — acceptable for a stacked-dependency change and resolves automatically when the dependent change merges

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

### Requirement: CLI.md documents the `check-config` subcommand
`docs/CLI.md` SHALL include a `## \`check-config\`` section documenting the new subcommand's invocation, exit-code matrix, output formats, and intended use cases.

#### Scenario: CLI.md section exists with full coverage
- **WHEN** an operator reads `docs/CLI.md`
- **THEN** a section titled `## \`check-config\`` appears between the existing subcommand entries
- **AND** the section documents the required `--config <path>` argument
- **AND** the section documents the optional `--json` flag with the structured per-line JSON output
- **AND** the section enumerates the exit codes: `0` (valid), `1` (warnings only), `2` (hard errors)
- **AND** the section names the two intended audiences: operators editing YAML by hand AND scripted preflight (specifically `update.sh`, landing in a later stacked change)

#### Scenario: Section provides a copy-paste example for each exit code
- **WHEN** the operator reads the section
- **THEN** the page contains at least one example invocation each for an exit-0, exit-1, and exit-2 scenario
- **AND** each example shows both the stdout and stderr the operator would observe

### Requirement: `update.sh` is a thin bash bootstrap script for binary upgrades
The repository SHALL ship `update.sh` at the repo root as a bounded bash script (target ≤ 150 lines including comments) whose sole responsibilities are: resolve current and target versions, download the release binary and its SHA-256 companion, verify the checksum, invoke the binary's `check-config` subcommand as a preflight against the operator's existing config, atomically swap the binary with a `.previous` rollback artifact, restart the systemd unit, and verify the daemon comes back up — rolling back on failure. All business logic that benefits from unit-test coverage (config validation, version parsing) SHALL live in the autocoder Rust binary; `update.sh` orchestrates the steps but does not reimplement them.

`update.sh` SHALL NOT prompt the operator. The script is designed for cron invocation; every required input is either a flag, an environment variable, or derived from `systemctl show`. Operators wanting interaction stick to manual binary swaps.

`update.sh` SHALL default to the latest non-prerelease tag (via `GET /repos/<owner>/<repo>/releases/latest`). The `--version <tag>` flag opts in to a specific tag, including pre-releases.

#### Scenario: First-time run picks up latest tag and applies it
- **WHEN** an operator runs `./update.sh` AND the installed binary version is older than the latest non-prerelease tag
- **THEN** the script downloads the latest binary AND its `.sha256`
- **AND** verifies the checksum
- **AND** runs the new binary's `check-config --config <resolved-path> --json` preflight
- **AND** atomically swaps the old binary aside to `/usr/local/bin/autocoder.previous`
- **AND** installs the new binary at `/usr/local/bin/autocoder`
- **AND** restarts the `autocoder.service` systemd unit
- **AND** verifies the daemon is `active` within 30 seconds
- **AND** emits one INFO line to journalctl via `logger -t autocoder-update` naming the version transition
- **AND** exits 0

#### Scenario: Already-on-latest exit is a clean no-op
- **WHEN** the operator runs `./update.sh` AND the installed version matches the latest non-prerelease tag
- **THEN** the script prints `autocoder is already on <tag>; nothing to do`
- **AND** exits 0 without downloading anything
- **AND** the daemon is untouched

#### Scenario: Preflight failure leaves the daemon on the old binary
- **WHEN** the operator runs `./update.sh` AND the downloaded binary's `check-config` exits 2 (hard errors against the existing config)
- **THEN** the script dumps the JSON findings to stderr
- **AND** prints `update.sh: preflight failed; not swapping. Daemon continues on <current-version>.`
- **AND** exits non-zero
- **AND** the old binary at `/usr/local/bin/autocoder` is unchanged
- **AND** the daemon continues running on the old version

#### Scenario: Restart-verify failure triggers automatic rollback
- **WHEN** the swap completes AND `systemctl restart autocoder` runs AND the daemon does NOT reach `active` within 30 seconds
- **THEN** the script restores `/usr/local/bin/autocoder.previous` over `/usr/local/bin/autocoder`
- **AND** runs `systemctl restart autocoder` again
- **AND** prints `update.sh: new binary failed to start; rolled back to <previous-version>. Check journalctl -u autocoder.`
- **AND** exits non-zero
- **AND** the daemon resumes running on the previous version

#### Scenario: `--version <tag>` opts in to specific tags including pre-releases
- **WHEN** the operator runs `./update.sh --version v2.0.0-rc1`
- **THEN** the script uses `v2.0.0-rc1` as the target (bypassing the `/releases/latest` filter that excludes pre-releases)
- **AND** the rest of the flow (download, verify, preflight, swap, restart) is identical to a non-prerelease tag

#### Scenario: `--dry-run` reports without swapping
- **WHEN** the operator runs `./update.sh --dry-run`
- **THEN** the script resolves current and target versions, downloads, verifies, AND runs the preflight
- **AND** does NOT call `swap_binary` or `systemctl restart`
- **AND** prints `[dry-run] Would swap to <tag>` AND exits 0
- **AND** the daemon and binary on disk are unchanged

#### Scenario: Bounded size and complexity
- **WHEN** a reviewer inspects `update.sh`
- **THEN** the file is ≤ 150 lines including comments
- **AND** contains no business-logic implementations of config parsing, version comparison, or path resolution beyond what bash and `systemctl show` provide directly
- **AND** every preflight check delegates to the autocoder binary via `check-config`

### Requirement: DEPLOYMENT.md documents unattended updates via cron
`docs/DEPLOYMENT.md` SHALL include a section titled `Unattended updates via cron` documenting the `update.sh` workflow, the recommended crontab entry shape (with stagger guidance), the `--version` opt-out for operators who pin releases manually, and an explicit audience caveat naming who the workflow is for AND who it isn't.

#### Scenario: Section names the recommended crontab entry
- **WHEN** an operator reads `docs/DEPLOYMENT.md`'s `Unattended updates via cron` section
- **THEN** the section contains a sample crontab entry running `update.sh` at a low-traffic hour
- **AND** the entry redirects stdout + stderr to a log file under `/var/log/autocoder-update.log` (or operator-chosen location)
- **AND** the section suggests jittering the minute field when running across multiple hosts to avoid simultaneous fleet updates

#### Scenario: Section documents the `--version` opt-out for manual pinning
- **WHEN** the operator reads the section
- **THEN** the text describes `update.sh --version <tag>` for operators who freeze on a known-good release between manual upgrade reviews
- **AND** explains that pre-release tags require the explicit flag (default is non-prerelease only)

#### Scenario: Audience caveat is explicit
- **WHEN** the operator reads the section
- **THEN** the text names the intended audience (single-host SBC, indie VPS, homelab deployments where set-and-forget is the explicit goal)
- **AND** the text names the non-audience (enterprise change-control environments where Ansible / apt / k8s registries already own update orchestration) AND advises against using `update.sh` in those environments
- **AND** the text cross-links to the `Switching from source-build to binary updates` section (from `a01`) for operators upgrading their existing source-built deployment to the binary-update workflow

### Requirement: CLI.md documents the `changelog` subcommand
`docs/CLI.md` SHALL include a `## \`changelog\`` section documenting the subcommand's flags, default behavior, output formats, and intended use cases.

#### Scenario: CLI.md section exists with full coverage
- **WHEN** an operator reads `docs/CLI.md`
- **THEN** a section titled `## \`changelog\`` appears alongside the other subcommand entries
- **AND** the section documents `--workspace`, `--since`, `--to`, and `--format` with their defaults
- **AND** the section documents the `--since ever` sentinel AND the no-tags-fallback INFO line
- **AND** the section documents the `changelog:` frontmatter overrides (`skip`, `internal`, `hidden`, `summary`)
- **AND** the section includes at least one example markdown output AND one example JSON output

#### Scenario: Section describes cross-project applicability
- **WHEN** an operator reads the section
- **THEN** the text explains that the subcommand works against any OpenSpec checkout, not just autocoder's own repo
- **AND** the text provides examples for both `cd` + `autocoder changelog` AND `autocoder changelog --workspace <path>`
- **AND** the text cross-links to `docs/OPERATIONS.md` for the managed-workspace path under `<cache_dir>/workspaces/<sanitized-url>/`

### Requirement: Release workflow uses the changelog subcommand for release-body notes
`.github/workflows/release.yml` SHALL invoke `autocoder changelog` between the test gate AND the publish step AND pass the output to `gh release create --notes-file` (or the equivalent `body_path` field on the release-action variant in use). The release body on GitHub Releases SHALL display the harvested changelog instead of the auto-generated diff.

A failure in the changelog generation step SHALL NOT block the binary release — the step writes an empty notes file on error AND logs the error. The binary upload is the primary artifact; notes are a best-effort enhancement.

#### Scenario: Tagged release publishes a release body with the harvested notes
- **WHEN** a maintainer pushes a production tag matching `v\d+\.\d+\.\d+`
- **AND** the test gate passes
- **THEN** the workflow runs `autocoder changelog --since <previous-tag> --to <new-tag>` against the just-tagged commit
- **AND** the resulting markdown is written to a temp file
- **AND** the `gh release create` step passes `--notes-file <path>` so the release body on GitHub displays the markdown
- **AND** the release page shows human-readable section headings + bullets, NOT a raw commit diff

#### Scenario: No prior tag falls back to "ever"
- **WHEN** a maintainer pushes the FIRST tag in a repo
- **THEN** the workflow's `previous_tag` resolution (`git describe --tags --abbrev=0 HEAD^`) exits non-zero
- **AND** the workflow falls back to `--since ever` so the first release's notes cover every archive in history
- **AND** the resulting release body is non-empty (a first-release operator gets a meaningful notes block, not an empty one)

#### Scenario: Changelog step failure does not block the binary release
- **WHEN** the `autocoder changelog` invocation fails (binary panics, workspace has no archive, etc.)
- **THEN** the workflow step logs the error AND writes an empty `release-notes.md` AND continues
- **AND** the subsequent binary-upload step runs to completion
- **AND** the resulting GitHub Release has the binaries attached with an empty (or fallback-text) body
- **AND** the operator sees the failed workflow step in the Actions tab AND can investigate manually

### Requirement: CHATOPS.md and CLI.md document the `changelog` chatops verb and stylist prompt
`docs/CHATOPS.md` SHALL include a `### Generating a changelog: \`changelog\`` subsection within the `Chat-driven workflows` section, documenting the verb's syntax, flag surface, PR output shape, frontmatter propagation behavior, AND polite-refusal cases. `docs/CLI.md`'s existing `## \`changelog\`` section (from `a05`) SHALL gain a footer cross-link to the chatops verb so operators discovering the deterministic subcommand find the LLM-styled variant.

The stylist prompt template `prompts/changelog-stylist.md` SHALL ship in the repository alongside the other prompt templates (`prompts/implementer.md`, `prompts/code-review-default.md`, etc.) AND SHALL be embedded into the binary at compile time via `include_str!`. Operators MAY override the embedded prompt via a config knob parallel to the other prompt-override fields.

#### Scenario: CHATOPS.md subsection exists with full coverage
- **WHEN** an operator reads `docs/CHATOPS.md`
- **THEN** a subsection titled `### Generating a changelog: \`changelog\`` appears within the `Chat-driven workflows` section
- **AND** the subsection documents the verb syntax `@<bot> changelog <repo-substring> [<args>]`
- **AND** the subsection documents the accepted flags (`--since <tag>`, `--to <tag>`)
- **AND** the subsection documents the PR output shape (single PR; participates in the existing revision loop)
- **AND** the subsection documents frontmatter propagation (revisions implying durable classification may include `proposal.md` frontmatter edits in the same PR)
- **AND** the subsection enumerates the polite-refusal cases (`missing repo-substring`, `no repo matched`, `chatops backend not configured`, `could not post ack`)

#### Scenario: CLI.md cross-links to the chatops verb
- **WHEN** an operator reads `docs/CLI.md`'s `## \`changelog\`` section
- **THEN** the section ends with a footer paragraph: `For an LLM-styled draft that opens a PR for review, use the \`@<bot> changelog\` chatops verb instead. See [CHATOPS.md → Generating a changelog](CHATOPS.md#generating-a-changelog-changelog).`
- **AND** the link anchor resolves to the subsection's heading

#### Scenario: Stylist prompt is embedded and overridable
- **WHEN** an operator inspects the binary's behavior without setting any prompt-override config
- **THEN** the embedded `prompts/changelog-stylist.md` is used as the stylist prompt
- **WHEN** the operator sets `executor.changelog_stylist_prompt_path: /path/to/custom-prompt.md` AND restarts the daemon
- **THEN** the override file's contents replace the embedded prompt
- **AND** an empty override file is rejected at use-time so the daemon does not feed an empty prompt to the wrapped CLI (parallel to the audit prompt-path validation)

#### Scenario: Stylist prompt template explicitly handles the absent-CHANGELOG case
- **WHEN** a maintainer reads `prompts/changelog-stylist.md`
- **THEN** the template includes an explicit directive to check whether `CHANGELOG.md` exists in the workspace root
- **AND** describes both branches: matching the existing style when present, OR creating a fresh Keep a Changelog v1.1.0 file when absent
- **AND** the fresh-file branch specifies the file's expected structure (top-level project heading, `## [Unreleased]` placeholder, current release's `## [<version>] - <YYYY-MM-DD>` section)

### Requirement: CODE-REVIEW.md and CONFIG.md document the prompt-budget and per-change-mode fields
`docs/CODE-REVIEW.md` SHALL include a `## Prompt budget` subsection AND a `## Per-change reviewer mode` subsection documenting the new `reviewer.prompt_budget_chars` AND `reviewer.mode` config fields respectively. `docs/CONFIG.md`'s existing `reviewer:` table SHALL gain rows for both fields.

#### Scenario: CODE-REVIEW.md documents the prompt budget field
- **WHEN** an operator reads `docs/CODE-REVIEW.md`
- **THEN** a section titled `## Prompt budget` appears between the existing `## Review context` section AND `## Reviewer-initiated revisions on \`Block\` verdicts`
- **AND** the section names `reviewer.prompt_budget_chars` AND its default value (2_000_000)
- **AND** the section explains the no-hard-ceiling property — operators match the value to their provider's actual context window
- **AND** the section gives at least one example: Grok-4 / Claude Sonnet 4.6 → 4M (or whatever the current window is)

#### Scenario: CODE-REVIEW.md documents per-change mode
- **WHEN** an operator reads `docs/CODE-REVIEW.md`
- **THEN** a section titled `## Per-change reviewer mode` documents `reviewer.mode` with values `bundled` (default) AND `per_change`
- **AND** the section explains the LLM-cost trade-off (per_change = N× cost on N-change PRs)
- **AND** the section describes the PR-body shape change (one `## Code Review: <slug>` section per change instead of one combined block)
- **AND** the section explains the cross-change preamble (each per-change prompt includes a fixed-size list of the other changes in the same PR for cross-reference context)

#### Scenario: CONFIG.md table includes both fields
- **WHEN** an operator reads `docs/CONFIG.md`'s `reviewer:` table
- **THEN** the table contains a row for `prompt_budget_chars` (type `usize`, default `2_000_000`, no max)
- **AND** the table contains a row for `mode` (type enum, default `bundled`, values `bundled` / `per_change`)
- **AND** both rows link to the relevant `docs/CODE-REVIEW.md` section for the full discussion

### Requirement: OPERATIONS.md, CONFIG.md, and TROUBLESHOOTING.md document the busy-marker-stale-threshold field and the decoupled recovery semantics
`docs/OPERATIONS.md`'s `## Busy marker` section SHALL be updated to reflect the new classification ordering (dead-pid immediate, decoupled threshold). `docs/CONFIG.md`'s `executor:` table SHALL gain a row for `busy_marker_stale_threshold_secs`. `docs/TROUBLESHOOTING.md` SHALL include a "Repo stuck on stale busy marker after daemon restart" diagnostic section.

#### Scenario: OPERATIONS.md classification table reflects the new ordering
- **WHEN** an operator reads `docs/OPERATIONS.md`'s `## Busy marker` section
- **THEN** the classification table lists the branches in the spec's order
- **AND** the "PID dead" row notes that recovery fires immediately with no age check
- **AND** a paragraph explains that the threshold is the new `executor.busy_marker_stale_threshold_secs` field (default 600s) rather than the pre-spec `timeout_secs + 10 min` formula
- **AND** the paragraph names the migration log line operators will see if their pre-spec config had a longer implicit threshold

#### Scenario: CONFIG.md documents the new field
- **WHEN** an operator reads `docs/CONFIG.md`'s `executor:` table
- **THEN** the table contains a row for `busy_marker_stale_threshold_secs` (type `u64`, default `600`, max `7200`)
- **AND** the row describes the field's purpose (stale-threshold for the live-pid recovery branch) AND cross-links to the OPERATIONS.md section

#### Scenario: TROUBLESHOOTING.md helps operators diagnose stale-marker symptoms
- **WHEN** an operator reads `docs/TROUBLESHOOTING.md`
- **THEN** a section titled `Repo stuck on stale busy marker after daemon restart` describes the symptom (status shows `currently: idle`, queue shows pending changes, but every polling iteration logs `busy marker present; skipping`)
- **AND** the section gives the diagnostic commands (`ls`, `cat`, `ps -p <pid>`)
- **AND** the section gives the immediate fix (`rm` the marker file)
- **AND** the section notes that the underlying cause for dead-pid markers is fixed in this spec — operators upgrading to this version no longer hit the symptom for daemon-restart scenarios

### Requirement: STATE-LAYOUT.md documents the resolver-only rule and the CI check
`docs/STATE-LAYOUT.md` SHALL include a section titled "Path resolution rule" documenting that every daemon state-file read AND write routes through the `DaemonPaths` resolver, the rationale (preventing read/write drift bugs), AND the CI-enforced check that fails on hard-coded `/tmp/autocoder/` literals outside an allowlist.

#### Scenario: Section exists with full coverage
- **WHEN** a future contributor reads `docs/STATE-LAYOUT.md`
- **THEN** a section titled "Path resolution rule" appears alongside the existing migration AND defaults sections
- **AND** the section names the `DaemonPaths` resolver AND its helper methods
- **AND** the section explains the CI-test enforcement (the `path_literals_audit` test in `cargo test`)
- **AND** the section names what to do when adding a new state-file shape: add a helper to `DaemonPaths`, use it from the consumer, the CI check passes automatically

### Requirement: Test suite uses per-test tempdirs; CI grep enforces no `/tmp/autocoder` literals in test code
The autocoder test suite SHALL NOT write to any path the live daemon would legitimately use. Every test that needs a state directory SHALL use the `test_daemon_paths()` helper (which returns a tempdir-scoped `DaemonPaths`) OR an equivalent per-test tempdir. Tests setting `AUTOCODER_*_DIR` env vars SHALL use a scoped mechanism (e.g., `temp_env::with_var(...)`) so the env var doesn't leak across tests. The path-literals CI audit from `a09` SHALL be extended to scan test code; the test-code allowlist is empty.

The rule prevents two failure modes: (a) test fixtures leaking into production state paths when autocoder works on itself (the wrapped agent runs `cargo test` AND tests writing to `/tmp/autocoder/...` would land alongside live daemon state); (b) tests on parallel hosts trampling each other's state via shared `/tmp` paths.

#### Scenario: `test_daemon_paths()` returns a usable tempdir-scoped DaemonPaths
- **WHEN** a test calls `let (_temp, paths) = test_daemon_paths();`
- **THEN** the returned `DaemonPaths` has its four directories under the tempdir's root
- **AND** the four directories exist on disk
- **AND** dropping the `_temp` binding (at end of test) auto-cleans every file the test wrote

#### Scenario: CI grep catches new `/tmp/autocoder` literals in test code
- **WHEN** a contributor adds a hard-coded `/tmp/autocoder/...` path inside a test function
- **AND** `cargo test` runs
- **THEN** the `path_literals_audit` test fails with the offending file:line listed
- **AND** the failure message points at `test_daemon_paths()` as the correct fix

#### Scenario: Existing test surface is swept clean
- **WHEN** the path-literals audit runs against `autocoder/src/` AND `autocoder/tests/`
- **THEN** zero hits are found in test code
- **AND** every previously-offending test has been refactored to use `test_daemon_paths()` OR an equivalent per-test tempdir

#### Scenario: Env-var-setting tests are scoped
- **WHEN** a test needs to set `AUTOCODER_STATE_DIR` (or similar) to exercise a daemon code path that reads from env
- **THEN** the test uses a scoped mechanism (e.g., `temp_env::with_var("AUTOCODER_STATE_DIR", value, || { ... })`)
- **AND** the env var is unset when the closure returns
- **AND** parallel tests AND production daemons running on the same host are unaffected

#### Scenario: test-reliability.md documents the rule and the cleanup hint
- **WHEN** an operator reads `docs/test-reliability.md`
- **THEN** a "Test isolation" section names the per-test tempdir rule
- **AND** the disposition table contains an entry for the swept-and-fixed pattern
- **AND** a one-liner notes that operators with pre-spec dev machines can `rm -rf /tmp/autocoder/` to clean up stale test fixtures (the daemon never reads from there post-`a09`)

### Requirement: CHATOPS.md status reply documentation enumerates the new `currently:` line variants
`docs/CHATOPS.md`'s operator-recovery-commands section (where the `status` verb's reply shape is documented) SHALL include examples of every `currently:` line variant introduced by this spec AND explain the diagnostic value of each.

#### Scenario: Reply-shape examples include every variant
- **WHEN** an operator reads `docs/CHATOPS.md`'s `status` reply-shape examples
- **THEN** at least one example each appears for: `idle`, `working on <change>`, `running audit <type>`, `<stage> in progress`, `recovery in progress`, `stale marker from pid <pid> (... recovery eligible now)`, `stale marker from pid <pid> (... recovery in <duration>)`

#### Scenario: Section explains the diagnostic value
- **WHEN** an operator reads the section
- **THEN** a paragraph explains that the `currently:` line distinguishes "audit in flight, just wait" from "stale marker, need recovery to fire (or manual `rm`)" from "truly idle"
- **AND** the paragraph cross-links to `OPERATIONS.md`'s busy-marker section for the underlying classification logic
- **AND** the paragraph cross-links to `TROUBLESHOOTING.md`'s stale-marker section for the immediate-fix-by-hand path

### Requirement: OPERATIONS.md describes the new iteration ordering and the audit-to-implementation one-iteration delay
`docs/OPERATIONS.md`'s `## Periodic audits` section SHALL be updated to reflect that audits run AFTER the pending change queue walk (not before, as the pre-spec text stated). The same section SHALL include a paragraph explaining the one-iteration delay for audit-generated changes' implementation AND why the trade-off is favorable.

#### Scenario: OPERATIONS.md correctly names the new ordering
- **WHEN** an operator reads `docs/OPERATIONS.md`'s `## Periodic audits` section
- **THEN** the "When audits fire" paragraph reads "audits run AFTER `list_pending`" (or equivalent), not "BEFORE `list_pending`"
- **AND** the paragraph notes the motivation (preventing audit-storm monopolization when many audits become eligible at once)

#### Scenario: OPERATIONS.md explains the audit-to-implementation delay
- **WHEN** an operator reads the same section
- **THEN** a paragraph describes the one-iteration delay: an audit running in iteration N creates proposals that the implementer picks up in iteration N+1
- **AND** the paragraph explains the operator-visible effect: audit creation commits ship in one PR, audit-generated change implementations ship in a follow-up PR
- **AND** the paragraph names the benefit: reviewers see proposal contents before implementation, and can `@<bot> revise <text>` the proposals before implementer runs in the next iteration

### Requirement: OPERATIONS.md and CONFIG.md document `max_audits_per_iteration`
`docs/OPERATIONS.md`'s `## Periodic audits` section SHALL include a paragraph describing the `audits.max_audits_per_iteration` bound, its default (`1`), the rationale (prevent storm patterns), the override pattern, AND the interaction with on-demand queued runs. `docs/CONFIG.md`'s `audits:` table SHALL gain a row for the field.

#### Scenario: OPERATIONS.md describes the bound and its rationale
- **WHEN** an operator reads `docs/OPERATIONS.md`'s `## Periodic audits` section
- **THEN** a paragraph names `audits.max_audits_per_iteration` AND its default `1`
- **AND** the paragraph explains the rationale (preventing audit storms when many audits become eligible simultaneously, e.g. after a HEAD change)
- **AND** the paragraph names the typical override values (e.g. `3` for fast drainage during onboarding) AND the trade-off (longer iteration wall-clock per cycle)
- **AND** the paragraph explains that on-demand queued audits count against the bound — operators queuing many audits via `@<bot> audit ...` see them drain one per iteration at the default

#### Scenario: CONFIG.md documents the field
- **WHEN** an operator reads `docs/CONFIG.md`'s `audits:` table
- **THEN** the table contains a row for `max_audits_per_iteration` (type `usize`, default `1`, max `<count of registered audits>`)
- **AND** the row cross-links to the OPERATIONS.md section for the full discussion

### Requirement: OPERATIONS.md and CHATOPS.md document the transient vs. permanent classification
`docs/OPERATIONS.md`'s workspace-recovery sections SHALL include a paragraph describing the mid-iteration classification (transient retries; permanent skips). `docs/CHATOPS.md`'s chatops-alert text examples SHALL show the new ` (transient; retrying)` AND ` (permanent; skipped until daemon restart) — operator inspection required` suffixes.

#### Scenario: OPERATIONS.md names the classification rule
- **WHEN** an operator reads `docs/OPERATIONS.md`'s workspace-recovery sections
- **THEN** a paragraph names the mid-iteration classification AND enumerates the patterns that classify as transient (network, transport, auth blip) vs. permanent (config errors, irrecoverable state)
- **AND** the paragraph notes that startup-time recovery is unchanged (still skip-for-lifetime for any failure)
- **AND** the paragraph cross-links to the chatops-alert section for the visible suffix examples

#### Scenario: CHATOPS.md alert examples show the new suffixes
- **WHEN** an operator reads `docs/CHATOPS.md`'s `Throttled failure alerts` section
- **THEN** the example alert text includes a transient case with the ` (transient; retrying)` suffix
- **AND** the example includes a permanent case with the ` (permanent; skipped until daemon restart) — operator inspection required` suffix
- **AND** a one-line note explains the operator action: transient → wait; permanent → SSH and investigate

