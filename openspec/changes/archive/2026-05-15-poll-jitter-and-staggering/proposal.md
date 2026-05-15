## Why

When autocoder starts with N repositories configured, all N polling tasks begin their first iteration at essentially the same instant. With 8–9 repos pointing at GitHub, the burst of simultaneous `git fetch` operations from a single source IP looks like a port scan or scraping behavior to intrusion-detection systems (IDS) — the project author observed exactly this: their IDS killed SSH connections when the daemon tried to poll 8–9 repos at once.

The same problem recurs at every subsequent wake-up: tasks that all share the same `poll_interval_sec` (e.g. the default 300) drift only slightly across iterations because each iteration's runtime is dominated by `git fetch` (similar wall-clock per repo). Without active jitter, the cluster stays synchronized.

A startup jitter window (each task sleeps a small random duration before its first iteration) staggers the cluster. A small per-iteration jitter (random ±% of `poll_interval_sec`) prevents long-term re-synchronization. Together they cost almost nothing in wall-clock and remove the bursty-traffic signature.

## What Changes

- **MODIFIED capability:** `orchestrator-cli`'s "Per-repository asynchronous polling loop" requirement. Each polling task SHALL apply a startup jitter (random sleep in `[0, startup_jitter_max_secs]`) before its first iteration. Inter-iteration sleeps SHALL be `poll_interval_sec ± (poll_interval_sec * inter_iteration_jitter_pct / 100)` with uniform random offset.
- **Config:** new optional executor-level fields (defaults below) on `ExecutorConfig`:
  - `startup_jitter_max_secs: u64` (default `30`). Each task picks `rand::thread_rng().gen_range(0..=startup_jitter_max_secs)` seconds at spawn time.
  - `inter_iteration_jitter_pct: u8` (default `10`). Each inter-iteration sleep is `poll_interval_sec` adjusted by `±jitter_pct` (uniform). A value of `0` disables inter-iteration jitter.
- **Code:** `polling_loop::run` sleeps the startup jitter before its `loop {}` begins. The existing `sleep(Duration::from_secs(repo.poll_interval_sec))` is replaced by a jittered version that reads the config-resolved interval.
- **Cancellation respected.** Both jitter sleeps participate in the existing `tokio::select!` against the cancellation token, so SIGTERM/SIGINT exit them within 200 ms exactly like the existing inter-iteration sleep.

## Impact

- Affected specs: `orchestrator-cli` (one MODIFIED requirement).
- Affected code: `autocoder/src/polling_loop.rs` (startup + per-iteration jitter), `autocoder/src/config.rs` (two new optional fields on `ExecutorConfig`), `autocoder/Cargo.toml` (depend on `rand` if not already a direct dependency).
- Behavior change: first-iteration timing is randomized across `[0, 30s]` per task. Inter-iteration sleeps drift ±10% of `poll_interval_sec`. Neither change affects correctness; they only smooth traffic.
- Defaults rationale: 30 seconds across the first iteration is generous enough to defeat per-second-bucket rate counters for clusters of 10+ repos. 10% inter-iteration jitter keeps tasks from re-clumping over time. Operators with extreme requirements (sub-second polling, or absolute determinism in tests) can set both to 0.
- Tests: the existing shutdown-during-sleep test continues to work because the jitter sleep uses the same `tokio::select!` shape. New tests cover the jitter math itself with a seeded RNG.
- Breaking: no. Existing configs get the default 30s startup jitter + 10% inter-iteration jitter, which is invisible to operators monitoring iteration timing at human granularity.
