## 1. Locate the originally-named test

- [ ] 1.1 Search the current tree (`autocoder/src/`) for test names matching the pattern `*weekly*`, `*creates*occurrences*`, `*about_52*`. Document the exact grep commands run and their outputs in `docs/test-reliability.md` under "Test name lookup" so the trail is preserved.
- [ ] 1.2 If a test by that name does not exist, search git history (`git log --all --oneline -S "creates_about_52" -- '*.rs'`) for a removed or renamed test. Document the finding.
- [ ] 1.3 If neither the current tree nor history contains the name, document the negative result and proceed with the category-based audit; do not block the change on locating a specific test that may not exist in this repo.

## 2. Category audit: clock and timing

- [ ] 2.1 List every test under `autocoder/src/` that calls `Instant::now()` or `chrono::Utc::now()` directly (not in production code paths). For each: classify as deterministic (compares `then.elapsed() > X` after a `sleep(X+slack)` — fine), risky (asserts on absolute wall-clock value), or correct-via-injected-clock (uses a `Clock` trait or similar).
- [ ] 2.2 List every test that uses `tokio::time::sleep` or `std::thread::sleep` followed by a timing assertion. Particularly suspect: tests that compute a duration and assert it falls in a narrow window.
- [ ] 2.3 For audit-scheduler tests under `autocoder/src/audits/scheduler.rs` and cadence tests under `autocoder/src/config.rs`: confirm `Cadence::*::interval()` is deterministic. If any test does forward-projection ("how many times would this fire in a year?") with floating-point arithmetic, that's a classic source of off-by-one — flag it.
- [ ] 2.4 Record findings in `docs/test-reliability.md` under "Clock and timing".

## 3. Category audit: env mutation

- [ ] 3.1 Find every test using `std::env::set_var` or `std::env::remove_var`. For each: check that the test's module (or test-group) holds a `Mutex` guard for the duration of the env mutation + the call under test (the `github_credentials.rs` pattern of `static ENV_LOCK: Mutex<()>`).
- [ ] 3.2 For modules using env-mutation without a module-level lock: flag as race-prone. Fix by adding a `static MODULE_ENV_LOCK: Mutex<()>` and acquiring it at the top of every env-mutating test.
- [ ] 3.3 Record findings in `docs/test-reliability.md` under "Env mutation".

## 4. Category audit: mockito and HTTP fixtures

- [ ] 4.1 Spot-check 5–10 mockito-using tests for: (a) tests that share a single mockito server across multiple parallel test functions (port-bind / route-collision risk), (b) tests that don't `.assert_async().await` on their mocks (silently-passing tests when a mock isn't hit).
- [ ] 4.2 Test the suite under aggressive parallelism: `cargo test -- --test-threads=$(sysctl -n hw.logicalcpu) --nocapture 2>&1 | tail -50` and again at default thread count. Record any test that fails on one configuration and not the other.
- [ ] 4.3 Record findings in `docs/test-reliability.md` under "Mockito / HTTP".

## 5. Category audit: filesystem fixtures

- [ ] 5.1 Find tests using `tempfile::TempDir` plus filesystem-mutating helpers. Check that no two tests use a hard-coded path under `/tmp/<fixed-name>/` (that would race). Filter out the false-positive case where `WORKSPACE_ROOT = "/tmp/workspaces"` is used only by code-under-test that constructs sub-paths per repo URL — the prod path is safe because the URL hash differs, but tests that construct hard-coded URLs could collide. Document any collision risk.
- [ ] 5.2 Record findings in `docs/test-reliability.md` under "Filesystem fixtures".

## 6. Empirical flake-hunting

- [ ] 6.1 Run the test suite 20 times in a row at default parallelism: `for i in (seq 20); cargo test --quiet 2>&1 | tail -2; end`. Record any failures (test name + iteration number + stderr snippet). If any test fails even once, treat as confirmed flake and prioritize it in the report.
- [ ] 6.2 Run the test suite 10 times under maximum parallelism: `for i in (seq 10); cargo test -- --test-threads=32 2>&1 | tail -2; end`. Record any new failures.
- [ ] 6.3 Record findings in `docs/test-reliability.md` under "Empirical observations" with absolute counts (e.g. "0/20 default, 0/10 stressed" or "1/20 default — see entry for `<test_name>`").

## 7. Fixes

- [ ] 7.1 For each flake whose root cause was identified AND whose fix is local to test code (no production-code signature changes): implement the fix. Add a one-line comment at the fix site naming the report entry that explains why.
- [ ] 7.2 For each flake requiring production-code changes (e.g. injectable clock): assess scope. If <50 lines of production change, implement here. Otherwise document the required change in the report and leave the test in place (the operator instruction allows reporting as a valid disposition).
- [ ] 7.3 Re-run the empirical loop from §6 after every fix to confirm the flake is gone (or, if still flaky, update the report).

## 8. The report

- [ ] 8.1 Write `docs/test-reliability.md` with sections matching §§1–6 above, plus a final "Disposition summary" table: `Test | Module | Category | Disposition | Notes`. Dispositions: `fixed-in-this-change`, `mitigated`, `accepted-known-flaky`, `unfixable-needs-architecture-change`, `not-flaky-on-inspection`.
- [ ] 8.2 Add a short "How to investigate a new flake" paragraph at the top so future implementers extending the doc have a starting point.

## 9. Verification

- [ ] 9.1 `cargo test` passes after all fixes are in.
- [ ] 9.2 `openspec validate audit-flaky-tests --strict` passes.
- [ ] 9.3 `docs/test-reliability.md` exists and has an entry (or a categorical statement) for every test category enumerated in §§2–5.
