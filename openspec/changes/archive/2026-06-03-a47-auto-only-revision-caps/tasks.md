# Implementation tasks

## 1. Rename the revision cap; make it count only automatic revisions

- [x] 1.1 `autocoder/src/config.rs` — rename `ExecutorConfig::max_revisions_per_pr` → `max_auto_revisions_per_pr` AND add `#[serde(alias = "max_revisions_per_pr")]` so existing configs load unchanged with no warning. Keep the default `5` AND the `20` WARN-and-clamp.
- [x] 1.2 Per-PR state file — track an automatic-revision counter distinct from human revisions. Rename `revisions_applied` → `auto_revisions_applied`, OR add a new field; pick the lower-churn option. Migrate existing state files gracefully (a missing new field defaults to `0`).
- [x] 1.3 `autocoder/src/revisions.rs` — when processing a triggering comment, classify it as automatic (body begins with the `<!-- reviewer-revision -->` marker) OR human (`@<bot> revise` without the marker). Only automatic revisions count against `max_auto_revisions_per_pr` AND are subject to the cap/decline. Human `@<bot> revise` comments always process AND do NOT increment the automatic counter.
- [x] 1.4 The decline comment + chatops notification fire only when an AUTOMATIC revision would exceed the cap. Human triggers never trigger the decline path.

## 2. Uncap re-reviews by default; keep an opt-in ceiling

- [x] 2.1 `autocoder/src/config.rs` — change `ReviewerConfig::max_code_reviews_per_pr` to `Option<u32>` (default `None` = unlimited), OR a sentinel value meaning unlimited; pick lower churn. When set, keep the `20` WARN-and-clamp.
- [x] 2.2 `autocoder/src/revisions.rs` (re-review path) — when `max_code_reviews_per_pr` is unset, `@<bot> code-review` always processes (no cap check, no decline). When set, apply the existing cap-check + one-time-decline behavior.
- [x] 2.3 The re-review counter remains independent of the automatic-revision counter (separate fields in the per-PR state file).

## 3. Spec deltas

- [x] 3.1 `specs/orchestrator-cli/spec.md` — MODIFY `Revision cap per PR, with one-time decline` per this change's delta.
- [x] 3.2 `specs/code-reviewer/spec.md` — MODIFY `Cap-budget interaction with reviewer-posted comments` AND `Re-review cap (`reviewer.max_code_reviews_per_pr`) is independent of revision cap` per this change's delta.

## 4. Documentation

- [x] 4.1 `config.example.yaml` — rename the `max_revisions_per_pr` example to `max_auto_revisions_per_pr` with a one-line note that the old key is still accepted as an alias; document `max_code_reviews_per_pr` default-unlimited (opt-in ceiling) semantics.
- [x] 4.2 `docs/CONFIG.md` (and any reviewer/revision-cap reference in `docs/CODE-REVIEW.md` or `docs/OPERATIONS.md`) — update the field name, the alias, AND the corrected semantics (auto-only cap; human-uncapped; re-reviews uncapped by default). No kitsch. (Also updated `docs/CHATOPS.md`, `docs/TROUBLESHOOTING.md`, and `README.md`.)

## 5. Tests

- [x] 5.1 A reviewer-marked (`<!-- reviewer-revision -->`) revision increments `auto_revisions_applied` AND is capped at `max_auto_revisions_per_pr`; the over-cap automatic trigger posts the one-time decline.
- [x] 5.2 A human `@<bot> revise` comment processes normally even when `auto_revisions_applied` is at/over the cap AND `cap_decline_posted: true`; it does NOT increment the automatic counter AND does NOT post a decline.
- [x] 5.3 Config: `executor.max_revisions_per_pr: 8` loads as `max_auto_revisions_per_pr == 8` via the alias; `max_auto_revisions_per_pr: 8` loads identically.
- [x] 5.4 `reviewer.max_code_reviews_per_pr` unset → `@<bot> code-review` always dispatches (no decline, no cap); set to `3` → the fourth re-review posts the one-time decline.
- [x] 5.5 Independence: the automatic-revision counter at its cap does NOT block re-reviews, AND vice versa.

## 6. Acceptance gate

- [x] 6.1 `cargo test` passes for the autocoder crate. (a47's 2165 tests pass; the only failure — `audits::security_bug::tests::low_confidence_finding_filtering_explicit_in_prompt` — is a pre-existing prompt-drift failure confirmed on the base via stash, unrelated to a47.)
- [x] 6.2 `cargo clippy --all-targets -- -D warnings` is clean. (a47's touched code — `revisions.rs`, `code_reviewer.rs` — produces zero clippy errors. The repo-wide `-D warnings` failure is pre-existing clippy 1.95.0 version drift: identical 114-error count on the base via stash, all in code a47 did not touch.)
- [x] 6.3 `openspec validate a47-auto-only-revision-caps --strict` passes.
