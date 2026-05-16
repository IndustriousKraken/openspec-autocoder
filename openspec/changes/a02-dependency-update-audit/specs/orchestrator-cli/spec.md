## ADDED Requirements

### Requirement: Dependency update triage audit
autocoder SHALL register a `dependency_update_triage` audit in the periodic-audit framework. The audit SHALL list Dependabot pull requests on the bot's fork (or upstream when no fork is configured), classify each by a strict "safe shape" filter, approve the safe ones via the GitHub Reviews API, and report unsafe ones via chatops. The audit is `requires_head_change = false` and `WritePolicy::None`.

#### Scenario: Lists Dependabot PRs on the fork in fork-PR mode
- **WHEN** the audit runs AND `github.fork_owner` is set
- **THEN** autocoder calls
  `GET /repos/<fork_owner>/<repo_name>/pulls?state=open` with the
  appropriate token, filters the response to PRs whose author
  `login` is `dependabot[bot]` OR `dependabot-preview[bot]`, AND
  iterates the resulting list

#### Scenario: Lists Dependabot PRs on upstream when fork mode is disabled
- **WHEN** the audit runs AND `github.fork_owner` is NOT set
- **THEN** autocoder lists PRs on the upstream repository
  (`<owner>/<repo_name>`) with the same Dependabot author filter
- **AND** the operator is responsible for ensuring the configured
  token has approval rights on upstream (the audit does not
  pre-check this)

#### Scenario: Safe-shape filter approves manifest-only version bumps
- **WHEN** a Dependabot PR's diff modifies only files matching the
  known-manifest list (`Cargo.toml`, `Cargo.lock`, `package.json`,
  `package-lock.json`, `yarn.lock`, `requirements.txt`,
  `pyproject.toml`, `*.csproj`, `packages.lock.json`, `go.mod`,
  `go.sum`, `Gemfile`, `Gemfile.lock`, `composer.json`,
  `composer.lock`, `pom.xml`, `build.gradle`, `build.gradle.kts`)
  AND every change within those files is a version-string update
  (no new top-level dependency entries, no removed entries, no
  `repository` / `homepage` / `registry` field changes, no new
  `scripts` / `postinstall` / `preinstall` / `prepublish` entries)
- **THEN** the audit submits an approving review:
  `POST /repos/<owner>/<repo>/pulls/<number>/reviews`
  with `{"event": "APPROVE", "body": "autocoder: safe-shape
  filter passed (manifest-only version bumps)"}`
- **AND** the approval counts toward the per-run cap

#### Scenario: Adding a new dependency entry fails safe-shape filter
- **WHEN** a Dependabot PR adds a `[dependencies] foo = "1.0"`
  line that did not exist in the base, OR adds a key to
  `package.json`'s `dependencies` / `devDependencies` map
- **THEN** the audit does NOT approve the PR
- **AND** posts a chatops finding of severity `medium` with
  subject `"PR #<num> adds new dependency entry — manual review
  required"`

#### Scenario: Changes to scripts / postinstall fail safe-shape filter
- **WHEN** a Dependabot PR adds or modifies any of:
  - `package.json`'s `scripts.postinstall`,
    `scripts.preinstall`, `scripts.prepublish`
  - any new top-level `scripts.*` entry that didn't exist before
  - `Cargo.toml`'s `build = "..."` field
  - a `pre-commit-hook` or `prepare` script field
- **THEN** the audit does NOT approve AND posts a chatops finding
  of severity `high` with subject `"PR #<num> modifies install
  scripts — manual review required"`

#### Scenario: Changes to URL/registry fields fail safe-shape filter
- **WHEN** a Dependabot PR modifies a `registry`, `repository`,
  `homepage`, `download-url`, or equivalent URL-bearing field for
  an existing dependency
- **THEN** the audit does NOT approve AND posts a chatops finding
  of severity `high` with subject `"PR #<num> changes dependency
  source URL — manual review required"`

#### Scenario: Non-manifest files in diff fail safe-shape filter
- **WHEN** a Dependabot PR's diff includes any file NOT in the
  known-manifest list (e.g. source files, README changes,
  workflow files)
- **THEN** the audit does NOT approve AND posts a chatops finding
  of severity `low` with subject `"PR #<num> modifies non-manifest
  files — manual review required"` and the body lists the
  unexpected paths

#### Scenario: Per-run approval cap enforced
- **WHEN** the audit's per-run `max_approvals_per_run` (default
  `5`) has been reached during the current invocation AND
  additional safe PRs remain in the list
- **THEN** the audit stops approving for this run
- **AND** posts a single chatops finding of severity `low` listing
  the deferred PR numbers, so the operator knows how many remain
- **AND** the next audit invocation continues from the same list
  (idempotent on already-approved PRs — GitHub returns the
  existing review without creating a duplicate)

#### Scenario: Already-approved PR is not re-approved
- **WHEN** a Dependabot PR has already been approved by the
  bot's user (visible in
  `GET /repos/<owner>/<repo>/pulls/<num>/reviews`)
- **THEN** the audit skips it for this run AND does NOT count it
  toward `max_approvals_per_run`
- **AND** does NOT post a chatops finding for it

#### Scenario: GitHub API failure on listing aborts the audit
- **WHEN** `GET /repos/<owner>/<repo_name>/pulls?state=open`
  returns non-2xx
- **THEN** the audit returns `Err` with the status code and
  response excerpt
- **AND** the framework treats this as audit failure: state is
  NOT updated, chatops alert is posted under the existing
  `audit-failure` category

#### Scenario: GitHub API failure on individual diff fetch skips that PR
- **WHEN** fetching a single PR's diff fails
- **THEN** the audit logs WARN, posts a chatops finding of
  severity `low` with subject `"PR #<num> diff fetch failed,
  skipping"`, AND continues to the next PR
- **AND** the audit itself returns successfully (so cadence
  advances normally)
