# orchestrator-cli — delta for a72-changelog-tag-gap-fill

## ADDED Requirements

### Requirement: The `changelog` chatops verb defaults to tag-driven gap-fill
When the `@<bot> changelog <repo>` verb is invoked WITHOUT an explicit `--since` OR `--to` argument, the daemon SHALL document every stable release tag that is missing from the workspace's `CHANGELOG.md`, oldest-first — rather than producing a single section for the default `(last tag … HEAD]` range. Passing `--since` AND/OR `--to` selects the explicit single-range behavior of the `changelog chatops verb queues an LLM-styled CHANGELOG.md update via the standard triage path` requirement AND bypasses gap-fill. The shared mechanics of that requirement — stylist styling/insertion, path-scope validation, commit to a `changelog-<short-hash>` branch, a single PR, and the revision loop — are unchanged; this requirement governs only WHICH versions a flagless run documents AND that it may document several at once.

**Stable release tags only.** Gap-fill SHALL consider only tags that parse as a release version with NO pre-release component (e.g. `v1.2.0`). Tags carrying a semver pre-release suffix — `-dev`, `-rc`, `-alpha`, `-beta`, etc., including the daemon's own `-dev` build tags — SHALL be skipped, so the changelog tracks real releases and does not churn on build tags.

**Deterministic gap detection.** The daemon (NOT the stylist LLM) SHALL read the existing `CHANGELOG.md`, identify the versions it already documents from its headings, AND compute which stable release tags are undocumented. For each missing tag, oldest-first, the daemon SHALL run the deterministic extractor (per the `a05` requirement) over that tag's range — `(previous stable release tag … this tag]` — AND combine the per-tag results into the JSON handed to the stylist. The stylist inserts each as its own section in chronological position. The run SHALL be idempotent: only missing versions are added; already-documented versions are neither regenerated nor duplicated.

**Nothing to do.** When every stable release tag is already documented, OR the repo has no stable release tags, the verb SHALL make no `CHANGELOG.md` change AND post a short thread reply stating so, rather than opening an empty PR.

#### Scenario: Flagless run fills every missing stable release tag, oldest-first
- **WHEN** `@<bot> changelog <repo>` is invoked with no `--since`/`--to` AND the repo has stable release tags `v1.0.0` AND `v1.1.0` not yet in `CHANGELOG.md`
- **THEN** the daemon documents both, oldest-first (`v1.0.0` then `v1.1.0`), each as its own section, in a single PR
- **AND** each section's range is `(previous stable release tag … this tag]`

#### Scenario: Pre-release tags are skipped
- **WHEN** the repo has tags `v1.2.0`, `v1.2.0-dev-108`, AND `v1.2.0-rc.1`
- **THEN** only `v1.2.0` is considered for documentation
- **AND** the pre-release tags (`-dev`, `-rc`) are given no changelog section

#### Scenario: Already-documented versions are not regenerated (idempotent)
- **WHEN** `CHANGELOG.md` already documents `v1.0.0` AND the only newer stable tag is `v1.1.0`
- **THEN** only `v1.1.0` is added AND `v1.0.0`'s existing section is left intact
- **AND** a subsequent flagless run with no new stable tags makes no `CHANGELOG.md` change AND posts that the log is already current

#### Scenario: Explicit --since/--to bypasses gap-fill
- **WHEN** `@<bot> changelog <repo> --since v1.0.0 --to v1.1.0` is invoked
- **THEN** the single-range behavior of the `changelog chatops verb queues an LLM-styled CHANGELOG.md update via the standard triage path` requirement applies (one section for that range)
- **AND** no gap-fill across other missing tags occurs

#### Scenario: No stable release tags yet
- **WHEN** `@<bot> changelog <repo>` is invoked AND the repo has no stable release tags (only pre-release tags, or none)
- **THEN** no `CHANGELOG.md` change is made
- **AND** the bot posts a thread reply that there are no stable release tags to document yet (the operator can tag a release, or pass `--since`/`--to` for an explicit range)
