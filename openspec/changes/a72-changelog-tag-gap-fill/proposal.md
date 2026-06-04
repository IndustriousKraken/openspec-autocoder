## Why

The chatops `changelog` verb (a06) is range-driven: the operator specifies `--since`/`--to`, and each run produces one `CHANGELOG.md` section. That's clunky for the common case — keeping `CHANGELOG.md` current across releases — and it creates a chicken-and-egg. A flagless run defaults to "since the last tag → HEAD" (an *unreleased* range), so a freshly-tagged release never gets its own section unless the operator hand-specifies `--to <new-tag>`; and there's no way to backfill a series of releases at once. Meanwhile the daemon tags `-dev` builds, which nobody wants as changelog entries.

The cleaner model — tag a release, then run the verb flagless and let it fill the gap — removes both the manual-range burden and the chicken-and-egg. (Latency to *get* that run on a busy repo is fixed separately by a71; this change is the UX of *what* a flagless run does.)

## What Changes

**The `changelog` verb defaults to tag-driven gap-fill.** With no `--since`/`--to`, the verb documents every **stable** release tag that is missing from `CHANGELOG.md`, **oldest-first**, each as its own section:

- **Stable tags only.** Only tags that parse as a release version with NO pre-release component (e.g. `v1.2.0`) are considered. Pre-release tags (`-dev`, `-rc`, `-alpha`, `-beta` — including the daemon's own `-dev` build tags) are skipped, so the changelog tracks real releases and doesn't churn on build tags.
- **Deterministic gap detection.** The daemon reads the existing `CHANGELOG.md`, matches the version headings it already carries, and computes which stable tags are undocumented. For each missing tag, oldest-first, it runs the a05 extractor for that tag's range — `(previous stable tag … this tag]` — and combines the results into the JSON handed to the stylist, which inserts each as its own section. Idempotent: only missing versions are added; documented ones are never regenerated or duplicated.
- **Nothing to do → friendly no-op.** When all stable tags are documented (or the repo has no stable release tags), no `CHANGELOG.md` change is made and the bot posts a short thread reply saying so, rather than opening an empty PR.

**`--since`/`--to` remain as an explicit single-range override** — when either is passed, the existing one-section behavior applies and gap-fill is bypassed.

## Impact

- **Affected specs:** `orchestrator-cli` — ADD `The changelog chatops verb defaults to tag-driven gap-fill`.
- **Affected code:** `changelog_triage.rs` — tag enumeration + a stable/pre-release semver filter; `CHANGELOG.md` documented-version detection; missing-tag computation (oldest-first); per-tag extraction combined into the stylist JSON (extends the single-version path); flag-gated bypass when `--since`/`--to` is present; no-op messaging. The stylist prompt (`prompts/changelog-stylist.md`) gains an instruction to insert *each* provided version section in chronological position.
- **Operator-visible behavior:** `@<bot> changelog <repo>` with no flags now fills in every missing stable-release section (tag a release, run it, done); `-dev`/`-rc` tags are ignored; re-running is a no-op once current. `--since`/`--to` still work for one-off ranges.
- **Dependencies:** builds on a05 (extractor) and the a06 verb. Independent of and complementary to a71 (which bounds the *latency* of getting the run; this changes *what* the run does).
- **Acceptance:** `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean; `openspec validate a72-changelog-tag-gap-fill --strict` passes. Tests: a flagless run fills two undocumented stable tags oldest-first; pre-release tags are skipped; an already-current log is a no-op; `--since`/`--to` bypasses gap-fill.
