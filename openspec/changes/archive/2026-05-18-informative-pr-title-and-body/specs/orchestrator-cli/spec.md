## ADDED Requirements

### Requirement: PR title and body describe what landed
PRs opened by autocoder SHALL carry a title and body that describe the actual changes shipped, derived from data already on hand at PR-creation time (the change slugs and each change's archived `proposal.md`). The title SHALL humanize the change slug — replacing hyphens with spaces and (when the slug uses the `aNN-` stacked-change convention) preserving the prefix as a labeled segment. The body SHALL include each change's `## Why` text under a per-change markdown heading. Both fields SHALL be deterministic functions of the changes processed in this iteration so re-running the same pass produces the same title and body.

#### Scenario: Single-change PR
- **WHEN** an iteration archives exactly one change `a06-refactor-portal-handlers-to-fromref` AND opens a PR
- **THEN** the PR title is `"a06: refactor portal handlers to fromref"`
  (or equivalent: the `aNN-` prefix is preserved as the label, the
  remainder has hyphens replaced with spaces, the colon separates
  them)
- **AND** the PR body contains a `## a06-refactor-portal-handlers-to-fromref`
  heading followed by the verbatim contents of that change's
  archived `proposal.md`'s `## Why` section
- **AND** the PR body ends with the existing `"Changes implemented
  in this pass:\n\n- <slug>\n"` reference list (one bullet per
  archived change)

#### Scenario: Multi-change PR
- **WHEN** an iteration archives three changes `a04-foo`, `a05-bar`,
  `a06-baz` AND opens a PR
- **THEN** the PR title is `"a04: foo (+2 more)"` — the first
  change's humanized form plus a count suffix naming the
  remaining changes
- **AND** the PR body contains three `## <slug>` sections in input
  order, each followed by that change's `## Why` text
- **AND** the PR body's final section is the slug-list reference

#### Scenario: A change's proposal.md is missing or malformed
- **WHEN** an iteration archives a change whose proposal.md is
  unreadable (file absent, permissions error, or no `## Why`
  heading present)
- **THEN** the PR body's section for that change uses
  `_(no proposal.md available)_` (or similar placeholder) instead
  of crashing or omitting the section
- **AND** the other changes' sections are unaffected — the
  fallback is per-change, not per-PR
- **AND** the build does not panic; the iteration completes
  normally and the PR opens with degraded body content

#### Scenario: Title length cap
- **WHEN** a change slug is long enough that the humanized title
  would exceed 80 characters
- **THEN** the title is truncated to fit, with the truncated
  portion replaced by `"…"`
- **AND** the `aNN-` prefix label (if present) is preserved at the
  start of the truncated title so the change identifier remains
  recognizable in GitHub's PR list

#### Scenario: Self-heal disclaimer interacts with the new body shape
- **WHEN** an iteration's commits include one or more self-heal
  archive-only commits (existing requirement: "Reject archive-only
  iterations as Failed", self-heal exception)
- **THEN** the PR body's first paragraph remains the existing
  self-heal disclaimer (`"_This PR archives one or more changes
  whose implementation was already present on the base branch..."`)
- **AND** the per-change `## Why` sections follow the disclaimer,
  preserving the existing reader cue that some changes have no
  code diff
