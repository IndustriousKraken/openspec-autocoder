# Test reliability

Living reference for known sources of flakiness in `autocoder/`'s test
suite. Update the **Disposition summary** at the bottom whenever a new
flake is discovered (or an old one is resolved); the per-section notes
above record the audit trail that justified each entry.

## How to investigate a new flake

1. Reproduce: run the suite in a loop (`for i in 1..20; cargo test
   --quiet 2>&1 | tail -2; done`). If the failure shows up at all in
   ~20 runs, it's a confirmed flake. If it never reproduces locally,
   try `cargo test -- --test-threads=$(nproc)` — host load matters.
2. Categorize: timing (uses `Instant::now`/`Utc::now`/`sleep` in a way
   that depends on wall-clock progress), env-mutation (calls
   `env::set_var` without an `ENV_LOCK`), filesystem (writes to a hard-
   coded `/tmp` path or has a fork+execve race against another test's
   script), mockito (asserts on a mock that doesn't get hit, or shares
   a server), or other. The categories below have prior-art examples.
3. Pick a disposition: `fixed-in-this-change`, `mitigated`,
   `accepted-known-flaky`, `unfixable-needs-architecture-change`,
   `not-flaky-on-inspection`. Add an entry to the summary table with
   the test name, module, category, and a one-line note. If the
   disposition is `unfixable-needs-architecture-change`, describe the
   required change so a follow-up has a starting point.

## Test name lookup

The originally-reported flake `weekly_creates_about_52_occurrences`
could not be located.

Commands run:

```
$ rg 'weekly|creates.*occurrences|about_52' autocoder/src/
autocoder/src/audits/architecture_consultative.rs:14://! cadence. Daily/weekly invocations produce noise.
autocoder/src/config.rs:503:/// `weekly`, `monthly`, `quarterly`.
autocoder/src/config.rs:556:            "weekly" => Ok(Self::Weekly),
... (only comment and parse-table matches; no fn weekly_creates_…)

$ git log --all --oneline -S "creates_about_52" -- '*.rs'
(no output)

$ git log --all --oneline -S "weekly_creates" -- '*.rs'
(no output — the only matching commit is 1d2f6f0 "investigate flaky
test", which adds the strings to the OpenSpec proposal text itself)

$ git log --all --oneline -S "about_52" -- '*.rs'
(same — only 1d2f6f0)
```

Conclusion: no test by that name exists in the current tree or in git
history. The reporter may have summarized a different test name, or
the name was approximate. Per the spec scenario "Investigating a
reported flake whose name cannot be located in the tree", we proceeded
with a category-based audit rather than blocking on the named test.

There is no `Cadence::interval`-using test that does forward-projection
("how many times would this fire in a year?"), so the suspected
floating-point off-by-one in such a hypothetical test cannot be
present here. See "Clock and timing" below.

## Clock and timing

`Instant::now()` call sites under `autocoder/src/` (10 total):

| Site | Context | Classification |
|---|---|---|
| `chatops/teams.rs:115,129` | prod: bearer-token cache TTL | prod-only, no test asserts on it |
| `polling_loop.rs:5986` | test `cancellation_in_startup_jitter_window` | **risky** — asserts elapsed `< 500 ms` after cancel; could fail under host load |
| `control_socket.rs:816,817` | test helper `wait_for` (poll-until-true with deadline) | deterministic — generous timeout, no narrow-window assertion |
| `workspace.rs:264,266` | prod: 30s fork-reachability poll | prod-only |
| `cli/run.rs:518,523` | prod: 60s fork-reachability poll | prod-only |
| `busy_marker.rs:450` | prod: `wait_for_exit` after SIGTERM | prod-only |
| `executor/claude_cli.rs:974` | test `timeout_kills_child` (`#[ignore]`d) | not running |

`Utc::now()` call sites (37 total across 11 files). Predominantly
production: marker timestamps (`alerts.rs`, `alert_state.rs`,
`failure_state.rs`, `perma_stuck.rs`, `queue.rs`, `audits/state.rs`,
`audits/mod.rs`, `busy_marker.rs`, `chatops/mod.rs`,
`polling_loop.rs`) and audit-scheduler `now`/`end_ts`
(`audits/scheduler.rs:102,166`). In tests, `Utc::now()` appears in:

- `audits/scheduler.rs:651,676,701,725` — used as anchor for
  "happened N days ago" by subtracting a `chrono::Duration`. **Safe**:
  the test asserts on the audit's run/skip decision, not on the
  absolute timestamp.
- `busy_marker.rs:527,686` — same pattern (`now - N seconds`,
  `now + N seconds`). **Safe**.
- `alerts.rs:226,264,346` — same pattern. **Safe**.

`tokio::time::sleep` / `std::thread::sleep` followed by a timing
assertion:

- `polling_loop.rs:3629` (`tokio::time::sleep(500ms)` then asserts
  `count >= 2`): the assertion bounds an inequality with slack — not a
  narrow-window check. Generally robust.
- `polling_loop.rs:3719` (`tokio::time::sleep(100ms)` then asserts the
  cancellation-driven shutdown completes within 1s): the 1s bound is
  generous; flaked 0/30 in this audit.
- `polling_loop.rs:5986` (`tokio::time::timeout(2s, handle)` and then
  `assert!(elapsed < 500 ms)`): **narrow window**. Flagged for future
  attention but did not flake in the 30 runs of this audit; left in
  place. Disposition: `not-flaky-on-inspection`.

`Cadence::*::interval()` is deterministic — every variant returns a
constant `chrono::Duration::days(N)`. No floating-point arithmetic
anywhere in the cadence module. No test does forward-projection over
a year. The original reporter's "creates about 52 occurrences"
description does not map to anything in the current implementation.

## Env mutation

ENV mutation sites and lock coverage:

| Module | Tests mutating env | Has `ENV_LOCK`? |
|---|---|---|
| `github_credentials.rs` | 11 tests | yes — `static ENV_LOCK: Mutex<()>` |
| `cli/run.rs` | 3 tests | yes — `static ENV_LOCK: Mutex<()>` |
| `audits/dependency_update.rs` | 7 tests | yes — `static ENV_LOCK: Mutex<()>` |
| `llm.rs` | 1 test (`inline_api_key_takes_precedence_over_env_var`) | **no** |
| `code_reviewer.rs` | 3 tests (`from_config_*`) | **no** |
| `config.rs` | 2 tests (`secret_source_resolve_env_var_*`) | **no** |
| `polling_loop.rs` | 2 tests (`pr_creation_uses_owner_specific_token`, `open_pr_check_*`) | **no** |

Each of the unlocked tests uses a **unique env-var name** (e.g.
`AUTOCODER_TEST_INLINE_PREC_KEY`, `REVIEWER_TEST_KEY_OVERRIDE`,
`AUTOCODER_TEST_SECRET_RESOLVE_SET`, `AUTOCODER_TEST_PR_ROUTING_TOKEN`,
`AUTOCODER_OPEN_PR_TEST_TOKEN`). That sidesteps the obvious collision
mode where two tests fight over the same key.

There is still a residual risk: `setenv`/`getenv` themselves are not
thread-safe in glibc (which is why Rust marked `env::set_var` as
`unsafe` from 1.79). One test mutating its unique key races with
another test reading a *different* unrelated key against the same
global env table. In practice the in-tree tests pass 30/30 (20
default + 10 stressed), so this is a latent risk rather than an
observed flake. Disposition: `accepted-known-flaky` (potential) for
the unlocked modules. If a future flake is observed in any of these
tests, the fix is to add a `static ENV_LOCK: Mutex<()>` per module
(mirroring `github_credentials.rs`) and acquire it at the top of every
env-mutating test in the module.

## Mockito / HTTP

Survey: 100 `mockito::Server::new_async()` call sites, 12 distinct
files with `.assert_async()`. No `static`/`once_cell` shared mockito
server — each test spawns its own server, mockito picks an ephemeral
port, no port-bind collision risk.

Spot-check of representative tests:

- `llm.rs::inline_api_key_takes_precedence_over_env_var` — uses
  `mock.assert_async().await` to enforce the mock was hit. ✓
- `polling_loop.rs::pr_creation_uses_owner_specific_token` — uses
  `.assert_async().await`. ✓
- `polling_loop.rs::open_pr_check_returns_true_when_pr_exists` —
  uses `.expect(1).create_async()` then `.assert_async().await`. ✓
- `workspace.rs::*` mockito tests — three sites, each uses
  `.assert_async().await`. ✓

There are tests that don't explicitly `assert_async()` on every mock
(they rely on the side-effect assertion to prove the mock was hit
indirectly). That's a stylistic concern but not a flake source — if
the mock isn't hit, the side-effect assertion fails.

Empirically: 0/30 mockito-driven failures across the audit's 30 runs.
Disposition: `not-flaky-on-inspection` for the category.

## Filesystem fixtures

Hard-coded `/tmp` paths in source:

- `workspace.rs:13` — `const WORKSPACE_ROOT: &str = "/tmp/workspaces"`.
  Production code; resolves sub-paths per repo URL hash. No collision
  in prod because URLs hash differently per repo.
- `workspace.rs:343,349,393,394,398` — assert on `resolve_path()`'s
  return value. **Pure string construction**, no filesystem write.
- `config.rs:760` — assert on a path construction result. No write.
- `busy_marker.rs:677,809` — assert on `marker_path()`'s return value.
  No write.
- `audits/drift.rs:552,559` and `audits/architecture_consultative.rs:583,592`
  — `prompt_path` config field stored as a `PathBuf`. No write.
- `audits/mod.rs:175` — `AuditLogWriter::open` writes to
  `/tmp/autocoder/logs/<workspace-basename>/audits/`. The basename
  derives from `workspace.file_name()`. In tests, `workspace` is a
  `TempDir`'s random suffix → unique per test.
- `audits/scheduler.rs:1082,1100` — test
  `audit_run_log_written_per_invocation` explicitly mints a per-test
  UUID basename for hermeticity. ✓

Conclusion: no test writes to a hard-coded `/tmp` path that another
test could collide with. Disposition: `not-flaky-on-inspection`.

### ETXTBSY from concurrent audit-CLI fixtures

The audit tests in `audits/drift.rs`, `audits/missing_tests.rs`, and
`audits/architecture_consultative.rs` write a small `fake-claude.sh`
shell script into a per-test `TempDir` and immediately spawn it via
`tokio::process::Command::spawn`. Under default parallelism on a
48-CPU host, two of the 20 baseline runs of this audit failed with
`Text file busy` (errno 26, `ETXTBSY`) when execve'ing the script:

```
iter 1: audits::missing_tests::tests::post_run_detects_only_new_change_dirs --- FAILED
        spawning missing_tests_audit command `/tmp/.tmpMaEkXS/fake-claude.sh`
        Caused by: Text file busy (os error 26)
iter 8: audits::drift::tests::sandbox_settings_file_cleaned_up_after_run --- FAILED
        spawning drift-audit command `/tmp/.tmpW32kup/fake-claude.sh`
        Caused by: Text file busy (os error 26)
```

Root cause: classic Linux fork-after-write race. Thread A is in the
brief window between `std::fs::write(&path, body)` returning and the
file being closed (via `Drop` of the temporary `File`). Concurrently,
Thread B calls `Command::spawn`, which `fork()`s; the child inherits
*all* fds open in the parent, including Thread A's writable fd to its
own to-be-exec'd script. The fd has `O_CLOEXEC` (Rust default), so it
will close on `execve` — but until `execve` runs, the kernel sees the
file as held open for write by Thread B's child process. If Thread A
then reaches its own `execve` before Thread B's child execve's, the
kernel refuses with `ETXTBSY`. Window is microseconds.

Fix (in this change): added `audits::spawn_with_etxtbsy_retry` —
takes a closure that builds a fresh `tokio::process::Command` and
retries the `spawn` up to 8 times with linear backoff (20–140 ms),
but only on `Err(ETXTBSY)`; any other error is returned immediately.
Wired into the three audit-CLI subprocess helpers
(`audits/drift.rs`, `audits/specs_writing.rs` — which serves both
`missing_tests` and `architecture_brightline` indirectly, and
`audits/architecture_consultative.rs`).

Production impact: negligible. ETXTBSY in production only happens if
something else on the host is mid-write to the very binary
autocoder is execve'ing. The retry is a no-op for the steady-state
case and rescues the rare collision case.

After-fix: 20/20 default-parallelism and 10/10 stress (32 threads)
runs clean.

## Empirical observations

Baseline (this commit, pre-fix):

- Default parallelism (`cargo test --quiet`, 20 iterations): **18/20 ok,
  2/20 failed**.
  - iter 1: `audits::missing_tests::tests::post_run_detects_only_new_change_dirs` (ETXTBSY)
  - iter 8: `audits::drift::tests::sandbox_settings_file_cleaned_up_after_run` (ETXTBSY)
- Stress (`--test-threads=32`, 10 iterations): **10/10 ok**.
  (Default thread count on this host is the full 48 CPUs; 32 is
  actually *less* contended, which is why no ETXTBSY here.)

Post-fix (`spawn_with_etxtbsy_retry` in audit subprocess helpers):

- Default parallelism (20 iterations): **20/20 ok**.
- Stress (32 threads, 10 iterations): **10/10 ok**.

## Disposition summary

| Test | Module | Category | Disposition | Notes |
|---|---|---|---|---|
| `weekly_creates_about_52_occurrences` | — | (unlocatable) | `not-flaky-on-inspection` | Test does not exist in current tree or git history (see "Test name lookup"). Per-spec, documented as a ghost so future operators don't reopen the search. |
| `audits::missing_tests::tests::post_run_detects_only_new_change_dirs` | `audits/missing_tests.rs` | filesystem (fork-after-write) | `fixed-in-this-change` | ETXTBSY race resolved by `spawn_with_etxtbsy_retry`. |
| `audits::drift::tests::sandbox_settings_file_cleaned_up_after_run` | `audits/drift.rs` | filesystem (fork-after-write) | `fixed-in-this-change` | ETXTBSY race resolved by `spawn_with_etxtbsy_retry`. |
| `audits::architecture_consultative::tests::*` (CLI-spawning tests) | `audits/architecture_consultative.rs` | filesystem (fork-after-write) | `fixed-in-this-change` (preventive) | Same pattern as drift/missing_tests; helper applied here too even though no failure was observed in 30 runs. |
| `polling_loop::tests::cancellation_in_startup_jitter_window` | `polling_loop.rs` | timing (narrow window) | `not-flaky-on-inspection` | Asserts `elapsed < 500 ms` after cancel. Did not flake in this audit's 30 runs. Future fix if it surfaces: relax the bound to e.g. 1500 ms, since the test's purpose is "cancel won the select", not "cancel is fast". |
| `llm::tests::inline_api_key_takes_precedence_over_env_var` | `llm.rs` | env mutation (no `ENV_LOCK`) | `mitigated` | Unique env-var name per test prevents direct collision. Glibc `setenv`/`getenv` not strictly thread-safe; if a flake surfaces, add `static ENV_LOCK: Mutex<()>` per module. |
| `code_reviewer::tests::from_config_*` (3 tests) | `code_reviewer.rs` | env mutation (no `ENV_LOCK`) | `mitigated` | Same — unique env-var names. |
| `config::tests::secret_source_resolve_env_var_*` (2 tests) | `config.rs` | env mutation (no `ENV_LOCK`) | `mitigated` | Same — unique env-var names. |
| `polling_loop::tests::pr_creation_uses_owner_specific_token`, `open_pr_check_*` | `polling_loop.rs` | env mutation (no `ENV_LOCK`) | `mitigated` | Same — unique env-var names. |
| (category) mockito-using tests | various | mockito | `not-flaky-on-inspection` | No shared servers; ephemeral ports; 0 failures in 30 runs. |
| (category) `TempDir`-based filesystem fixtures | various | filesystem | `not-flaky-on-inspection` | All hard-coded `/tmp` paths in tests are read-only string assertions; writing tests use random-suffixed paths. |
