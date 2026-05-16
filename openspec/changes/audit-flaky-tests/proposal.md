## Why

Autocoder has reported intermittent test failures — at least one named "weekly_creates_about_52_occurrences" — in production runs. Local searches under `autocoder/src/` cannot locate that exact test name (it may have been renamed, removed, or the name is approximate). Either way, three sequential local runs of the current 509-test suite pass cleanly, which means the flakes are either deeply intermittent (rare race) or environment-sensitive (host load on the the runtime server server, CPU contention under parallel test execution).

A quick survey of the codebase shows non-trivial flakiness surface area:

- 48 call sites using wall-clock primitives (`Instant::now`, `Utc::now`).
- 12 call sites using `tokio::time::sleep` or `std::thread::sleep` (any of which could be paired with timing-sensitive assertions).
- 37 call sites mutating process-global env vars, partially protected by `ENV_LOCK`-style mutexes in some modules but not necessarily all.
- 108 mockito uses; mockito spawns a real local HTTP server per test, with potential port-bind races under aggressive parallelism.

Letting these failures continue uninvestigated has two costs: the operator loses signal (when a real regression flakes, "oh it's flaky" muddies the diagnosis), and the autocoder loops on these reports without making progress because no implementing agent has the room to do a cross-cutting audit. A focused investigation now cleans up what's straightforward and produces a written reference for what isn't, so future flakes can be triaged faster.

## What Changes

- **Investigation deliverable:** a written report at `docs/test-reliability.md` enumerating every test class identified as potentially flaky, the root cause (where determinable), and the disposition (fixed in this change, mitigated, accepted as known-flaky, or unfixable-under-current-architecture). The report is developer-facing and lives outside the spec hierarchy (per the existing convention of not putting validation rituals in specs).
- **Targeted fixes** for any test where the root cause is clear and the fix is low-risk (typical patterns: replacing wall-clock comparisons with injected clocks, adding lock guards around env mutations, removing time-based assertions where the test could assert on the side effect instead).
- **Triage tracking:** for each test investigated, an entry in the report — `<test_name> | <module> | <category> | <root cause> | <disposition>`.
- **ADDED capability requirement** under `project-documentation`: a developer-reference doc covering test-suite reliability MUST live under `docs/`. This lets future implementing agents amend the report when they discover new flakes, without re-arguing whether the artifact belongs in the spec hierarchy.

The investigation explicitly targets categories rather than guessing at test names, since the originally-named test "weekly_creates_about_52_occurrences" cannot be located by exact match in the current tree. The categories below have known prior-art for flakiness in Rust/tokio test suites and bound the search space.

## Impact

- Affected specs: `project-documentation` (one ADDED requirement for the test-reliability report doc).
- Affected code: any test fixes are scoped per finding; expect changes under `autocoder/src/**/tests`. No production code paths should change. If a fix requires production code (e.g. wiring an injectable clock through a function signature), the proposal calls that out in the report and either implements it within this change or carves out a follow-up.
- Operator-visible behavior: none. CI/build/runtime semantics unchanged.
- Breaking: no.
- Acceptance: every test category enumerated in tasks.md has an entry in `docs/test-reliability.md` with a disposition. Tests judged unfixable are documented with a reason ("inherently timing-dependent — would need to refactor production to inject a clock; out of scope for this change") and that's an acceptable disposition per the operator's instruction ("IF the test is unfixable, I think a report on the issue is sufficient to check off the item").
