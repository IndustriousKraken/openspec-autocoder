---
changelog: skip
---

## Why

Three small pure helpers in `autocoder/src/polling_loop.rs` have **no
test coverage**, even though they all have branchy logic with
behavioral consequences if they regress:

- `extract_stdout_section` (polling_loop.rs:1871-1888) — parses the
  executor log's `=== STDOUT (...)` / `=== STDERR (...)` section
  delimiters and returns the stdout body. Has three early returns: no
  STDOUT marker found → `""`; STDOUT marker present but no newline
  after the header → `""`; no STDERR marker → return until end of
  input. Called by `build_implementer_summary` which DOES have
  tests, but those tests pass through `extract_stdout_section`
  indirectly with only one input shape — the branchier inputs
  (missing markers, malformed headers) are never exercised.
- `filter_alert_state_lines` (polling_loop.rs:1310-1328) — filters
  `git status --porcelain` lines that name autocoder's own
  `.alert-state.json` bookkeeping file so the workspace-dirty check
  ignores it. Called twice in `run_pass_through_commits` (lines 585,
  597). Untested. A regression that, say, matched `.alert-state.json`
  anywhere in the path (instead of as the path's exact value) would
  silently drop real porcelain entries that happen to mention the
  filename — including dirty-workspace alerts the operator depends on.
- `truncate_reason` (polling_loop.rs:1278-1287) — chars-aware
  truncation with `…` suffix when `reason.chars().count()` exceeds
  `PERMA_STUCK_REASON_EXCERPT_MAX`. Used in the perma-stuck chatops
  message. Untested; a byte-vs-char regression on a multibyte input
  would panic in production but never in CI.

## What Changes

Add tests under `autocoder/src/polling_loop.rs`'s existing tests
module covering:

- `extract_stdout_section` against each of: a well-formed log with
  both markers; a log with STDOUT only (no STDERR); a log with
  neither marker; a log with the STDOUT marker but no terminating
  newline after the header (the "advance past header line"
  early-return guard).
- `filter_alert_state_lines` against: an input with no
  `.alert-state.json` line (unchanged passthrough); an input where
  `.alert-state.json` is the only entry (becomes empty); an input
  with a mix of `.alert-state.json` and real-file entries (real
  entries survive); a guard test against false-positives — a file
  named `something.alert-state.json` or a path
  `subdir/.alert-state.json` should NOT be filtered (the production
  check is "path equals `.alert-state.json`", not "path contains").
- `truncate_reason` against: a string under the cap (passthrough,
  no ellipsis); a string exactly at the cap (passthrough); a string
  one char over the cap (truncated + `…` appended); a multibyte
  string longer than the cap in chars but shorter in bytes
  (truncation respects char boundaries).

No production code changes.

## Impact

- Affected code: `autocoder/src/polling_loop.rs`
  (`#[cfg(test)] mod tests` — same module the existing
  `build_implementer_summary_extracts_stdout_only`,
  `humanize_slug_strips_aNN_prefix_into_label`, etc. tests live in).
- No spec changes — these are private internal helpers; no
  capability requirement currently mentions them.
- Breaking: no.
