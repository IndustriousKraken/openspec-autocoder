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

