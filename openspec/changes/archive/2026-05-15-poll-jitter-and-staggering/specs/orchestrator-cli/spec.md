## MODIFIED Requirements

### Requirement: Per-repository asynchronous polling loop
autocoder SHALL implement the per-repository polling task referenced in `orchestrator-architecture/specs/orchestrator-cli/spec.md` as a sleep-then-iterate cycle that runs the architecture's single-pass workflow on every iteration. Each polling task SHALL apply a startup jitter (a random sleep in `[0, startup_jitter_max_secs]`) before its first iteration, and an inter-iteration jitter (a random uniform offset in `[-jitter_pct%, +jitter_pct%]` of `poll_interval_sec`) on every subsequent sleep. Both jitter sleeps SHALL respect the task's cancellation token.

#### Scenario: Spawn count matches config
- **WHEN** the daemon starts with a config containing N repositories AND the workspace collision check passes
- **THEN** exactly N polling tasks are spawned via `tokio::task::JoinSet`
- **AND** each task owns its own workspace path (no two tasks share a path; collision detection at startup enforces non-overlap)

#### Scenario: Startup jitter staggers first iterations
- **WHEN** the daemon spawns N polling tasks with default
  `startup_jitter_max_secs = 30`
- **THEN** each task draws a random sleep duration uniformly from
  `[0, 30]` seconds and waits that long BEFORE its first iteration
- **AND** different tasks draw independently, so the first iterations
  of the N tasks are spread across the 30-second window rather than
  beginning simultaneously

#### Scenario: Startup jitter of zero disables staggering
- **WHEN** `executor.startup_jitter_max_secs == 0`
- **THEN** every task begins its first iteration immediately on spawn
  (matching the pre-change behavior); no startup sleep occurs

#### Scenario: Normal iteration
- **WHEN** a polling task wakes (start of process or end of previous sleep)
- **THEN** it runs the full single-pass workflow for its repository: workspace init → stale-lock cleanup → dirty-workspace refusal → branch recreation → queue walk → push and PR creation if any commits were produced
- **AND** the task then sleeps for a jittered duration of
  `poll_interval_sec ± (poll_interval_sec * jitter_pct / 100)`
  before iterating again
- **AND** no two iterations within the same task overlap

#### Scenario: Inter-iteration jitter offset is uniformly distributed
- **WHEN** `executor.inter_iteration_jitter_pct = 10` AND
  `repo.poll_interval_sec = 300`
- **THEN** each inter-iteration sleep duration is drawn uniformly from
  `[270, 330]` seconds (300 ± 30, i.e. ±10%)
- **AND** the draw is independent per iteration; back-to-back
  iterations do not share a fixed offset

#### Scenario: Inter-iteration jitter of zero gives exact interval
- **WHEN** `executor.inter_iteration_jitter_pct == 0`
- **THEN** every inter-iteration sleep is exactly `poll_interval_sec`
  seconds (matching the pre-change behavior); the offset is not drawn

#### Scenario: Iteration runtime exceeds poll interval
- **WHEN** an iteration's wall-clock runtime exceeds the (possibly-jittered) `poll_interval_sec`
- **THEN** the next iteration begins immediately after the current one finishes
- **AND** no negative sleep is attempted; no two iterations within the same task run in parallel

#### Scenario: Cancellation interrupts startup jitter
- **WHEN** SIGINT or SIGTERM arrives while a task is in its startup
  jitter sleep (i.e. before its first iteration)
- **THEN** the task observes the cancellation token within 200 ms,
  exits the jitter sleep, and does NOT begin its first iteration
- **AND** the main process exits within 30 seconds total

#### Scenario: Cancellation interrupts jittered inter-iteration sleep
- **WHEN** SIGINT or SIGTERM arrives while a task is in its
  inter-iteration sleep
- **THEN** the task exits the sleep within 200 ms and does not begin
  another iteration
- **AND** this holds whether or not the sleep was the jittered or
  non-jittered branch (a `jitter_pct == 0` configuration produces the
  same cancellation latency)
