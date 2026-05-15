## 1. Config schema

- [x] 1.1 In `autocoder/src/config.rs::ExecutorConfig`, add two new optional fields:
  - `#[serde(default)] pub startup_jitter_max_secs: Option<u64>` (effective default `30` when `None`)
  - `#[serde(default)] pub inter_iteration_jitter_pct: Option<u8>` (effective default `10` when `None`)
- [x] 1.2 Add small accessor methods on `ExecutorConfig`:
  - `pub fn startup_jitter_max_secs(&self) -> u64 { self.startup_jitter_max_secs.unwrap_or(30) }`
  - `pub fn inter_iteration_jitter_pct(&self) -> u8 { self.inter_iteration_jitter_pct.unwrap_or(10).min(100) }` (cap at 100 to keep arithmetic well-defined; a negative-offset > interval would underflow)
- [x] 1.3 Tests in `config::tests`:
  - `startup_jitter_default_is_30`
  - `startup_jitter_explicit_zero_is_zero`
  - `inter_iteration_jitter_default_is_10`
  - `inter_iteration_jitter_above_100_is_clamped`

## 2. Polling loop changes

- [x] 2.1 In `autocoder/src/polling_loop.rs::run`, accept the `ExecutorConfig` (or just the two resolved values) and before the `loop {}`, sleep for `rand::thread_rng().gen_range(0..=startup_jitter_max_secs)` seconds. Use `tokio::select!` against the cancellation token so SIGTERM/SIGINT interrupts the wait. If cancelled during startup jitter, exit `run` without iterating.
- [x] 2.2 Replace the existing inter-iteration `sleep(Duration::from_secs(repo.poll_interval_sec))` with a helper:
  ```rust
  fn jittered_sleep_duration(base_secs: u64, jitter_pct: u8) -> Duration {
      if jitter_pct == 0 { return Duration::from_secs(base_secs); }
      let max_offset = (base_secs * jitter_pct as u64) / 100;
      let offset = rand::thread_rng().gen_range(0..=2 * max_offset) as i64 - max_offset as i64;
      let secs = (base_secs as i64 + offset).max(0) as u64;
      Duration::from_secs(secs)
  }
  ```
  Use it in the existing `tokio::select!`. The cancellation arm remains unchanged.
- [x] 2.3 Emit a startup INFO log line per repository naming the chosen startup-jitter delay (e.g. `polling task for <url> will wait 17s before first iteration`). This is operator-visible diagnostics during the staggering window — without it, "why is nothing happening for half a minute?" is surprising on first boot.
- [x] 2.4 **Verify:** existing tests of `polling_loop::run` either explicitly configure `startup_jitter_max_secs = 0` (most tests) or accept that the test may take up to 30s. The default test fixture helper should pass `0` to keep tests fast and deterministic.

## 3. Dependency

- [x] 3.1 If `rand` is not already a direct dependency of `autocoder/Cargo.toml`, add it. (Check with `cargo tree -i rand` or by inspecting `Cargo.toml` — it may already be transitively present via another crate but not direct.) Use the same major version as anywhere else in the workspace if possible.

## 4. Tests

- [x] 4.1 `polling_loop::tests::startup_jitter_in_range` — pure-function test on the jitter selection logic. Sample 1000 draws with `startup_jitter_max_secs = 30` and assert every value is in `[0, 30]` AND that both extremes appear within the sample (1000 draws of a 0..=30 range produce both endpoints with extremely high probability; the test is robust unless `rand`'s contract changes).
- [x] 4.2 `polling_loop::tests::startup_jitter_zero_returns_zero` — `startup_jitter_max_secs = 0` always produces `0` (degenerate range; no random draw needed but verify the implementation handles it).
- [x] 4.3 `polling_loop::tests::jittered_sleep_duration_within_band` — for `base = 300, jitter_pct = 10`, sample 1000 values and assert every value is in `[270, 330]` AND the mean is within `±5` of `300`.
- [x] 4.4 `polling_loop::tests::jittered_sleep_duration_zero_pct_is_exact` — for `jitter_pct = 0`, every draw is exactly `base`.
- [x] 4.5 `polling_loop::tests::jittered_sleep_duration_no_underflow_when_pct_is_100` — for `base = 10, jitter_pct = 100`, draws are in `[0, 20]`. Specifically, when the random draw selects the maximum negative offset (`-10`), the result is `0` and not a panic/underflow.
- [x] 4.6 `polling_loop::tests::run_exits_during_startup_jitter` — fixture: launch `run` with `startup_jitter_max_secs = 60` and immediately cancel the token. Assert: `run` returns within 200 ms (cancellation observed during the jitter sleep, not after a full 60s).

## 5. Documentation

- [x] 5.1 README "Config reference" — under `executor` table, add two rows:
  - `startup_jitter_max_secs` (`u64?`, default `30`, "Each polling task waits a uniformly random `[0, startup_jitter_max_secs]` seconds before its first iteration. Set to `0` to disable.")
  - `inter_iteration_jitter_pct` (`u8?`, default `10`, "Each inter-iteration sleep is `poll_interval_sec` adjusted by ±this percent. Set to `0` for exact intervals.")
- [x] 5.2 README "Operating notes" — add a short subsection "Polling cadence and your firewall": explain that for ≥5 configured repositories, the burst of simultaneous `git fetch` calls at startup can look like a port scan to network IDS. The jitter defaults are tuned to defuse this. Operators on isolated networks can set both to 0 if they prefer deterministic timing.

## 6. Verification

- [x] 6.1 `cargo test` passes.
- [x] 6.2 `openspec validate poll-jitter-and-staggering --strict` passes.
