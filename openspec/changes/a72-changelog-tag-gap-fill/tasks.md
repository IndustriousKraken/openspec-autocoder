# Implementation tasks

## 1. Tag enumeration + stable filter

- [ ] 1.1 Add a helper that lists the repo's tags AND parses each as a semver release version, partitioning stable releases (no pre-release component) from pre-release tags (`-dev`/`-rc`/`-alpha`/`-beta`/any semver pre-release suffix). Return the stable releases sorted ascending by version.
- [ ] 1.2 Be tolerant of the `v` prefix (`v1.2.0`) and of tags that don't parse as versions (ignore non-version tags).

## 2. Documented-version detection (deterministic, in the daemon)

- [ ] 2.1 Read the existing `CHANGELOG.md` (if present) and extract the set of versions it already documents by matching its version headings (e.g. `## [1.2.0]` / `## v1.2.0`). This is done by the daemon, not the stylist.
- [ ] 2.2 Compute the missing set = stable release tags whose version is NOT already documented, sorted oldest-first.

## 3. Gap-fill extraction + stylist hand-off

- [ ] 3.1 In `changelog_triage.rs`, gate on flags: if `parse_changelog_args` yields an explicit `--since` OR `--to`, keep the current single-range path (bypass gap-fill). Otherwise run gap-fill.
- [ ] 3.2 For each missing tag, oldest-first, resolve its range `(previous stable release tag … this tag]` and run the a05 extractor; combine the per-tag JSON results into one payload (a list of version sections) for the stylist.
- [ ] 3.3 Update `prompts/changelog-stylist.md` to insert EACH provided version section in its correct chronological position (it already matches existing style and reads `CHANGELOG.md`; ensure it handles a list of sections, not just one).
- [ ] 3.4 No-op: when the missing set is empty (all stable tags documented) OR there are no stable tags, do not invoke the stylist or open a PR; post a short thread reply (already current / no stable tags yet). Advance the request to its terminal status.

## 4. Tests

- [ ] 4.1 Stable filter: given tags `v1.2.0`, `v1.2.0-dev-108`, `v1.2.0-rc.1`, `v1.1.0`, the helper returns `[v1.1.0, v1.2.0]` (pre-release skipped, ascending).
- [ ] 4.2 Documented-version detection: a `CHANGELOG.md` with `## [1.0.0]` headings yields the documented set `{1.0.0}`; the missing set against tags `{v1.0.0, v1.1.0}` is `[v1.1.0]`.
- [ ] 4.3 Gap-fill range: with undocumented `v1.0.0` and `v1.1.0`, the per-tag ranges are `(ever … v1.0.0]` and `(v1.0.0 … v1.1.0]`, processed oldest-first (assert the ranges/order, not message text).
- [ ] 4.4 No-op: all stable tags documented → no stylist invocation, no PR, terminal status reached (assert the absence of a PR, not the exact reply wording).
- [ ] 4.5 Override: an explicit `--since`/`--to` takes the single-range path and does not enumerate other missing tags.

## 5. Acceptance gate

- [ ] 5.1 `cargo test` passes for the autocoder crate.
- [ ] 5.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] 5.3 `openspec validate a72-changelog-tag-gap-fill --strict` passes.
