## Why

The per-repo busy marker's recovery logic has a defect that bricks any repo whose daemon was killed mid-iteration. Empirical evidence from a production deployment:

```
17:33  drift_audit killed by SIGTERM (daemon restart). Marker for pid 490170 left behind.
17:35  New daemon (pid 538463) starts. Sees stale marker. Skips iteration.
17:52  Daemon 538463 exits. New daemon (pid 546769) starts. Same stale marker. Skips iteration.
17:35-18:26   51+ minutes of skipped iterations, marker never recovered.
```

The marker file held `pid=490170`. That process was gone from `/proc` the entire time. Per the spec, the marker classification rule is:

> | Age over threshold, PID dead | Auto-recover: clear marker, WARN log, proceed |

Where threshold = `executor.timeout_secs + 10 min`. The deployment had `timeout_secs` bumped to 5400s (90 min) to accommodate a long-running change, making the threshold 6000s (100 min). The marker was only ~50 minutes old when iterations were skipped, so the age check failed AND recovery never fired even though the PID was provably dead.

Two interlocking bugs:

1. **The dead-pid case shouldn't be gated on the age threshold.** A pid that no longer exists in `/proc` cannot be doing legitimate work; the marker is unambiguously stale the moment that's true. The age threshold exists to protect a live-but-slow executor from a sibling daemon's SIGTERM — it has no rationale when the executor is provably dead.

2. **The threshold being coupled to `executor.timeout_secs` is a config trap.** Raising the executor timeout for legitimately long work shouldn't penalize stale-marker recovery on completely unrelated iterations. The two values represent different concerns: one is "how long do we let a normal executor run," the other is "how long do we wait before assuming a marker is stale."

The fix is two changes to the marker-recovery logic, both in the same code path.

## What Changes

**Dead-pid recovery fires immediately, no age check.** When the marker classification logic finds `marker.pid` is not present in `/proc`, the marker is cleared, a WARN is logged, AND the iteration proceeds — regardless of the marker's age. The previous `age > threshold` gate on this branch is removed.

**Live-pid stale threshold gets its own config field.** A new `executor.busy_marker_stale_threshold_secs` (`u64`, default `600` = 10 min, max `7200` = 2 hours with WARN-and-clamp). The existing `executor.timeout_secs + 10 min` formula is replaced with `executor.busy_marker_stale_threshold_secs` for the "live PID, age over threshold" case (where the daemon SIGTERMs the process group and recovers).

**The default of 600s (10 min) is intentionally short.** A live executor that hasn't checked in for 10 minutes is suspect even if `executor.timeout_secs` allows it to keep running. The recovery path SIGTERMs the process and waits 5 seconds before SIGKILL, then clears the marker — same behavior as today, just on a tighter timeline.

**Operators with genuinely long-running executors can raise the threshold.** An operator who actually needs `timeout_secs: 5400` AND expects the executor to legitimately not check in for the full duration can set `busy_marker_stale_threshold_secs: 5500` to match. But this is opt-in — the default protects the common case (operator bumps timeout for one stubborn change, doesn't realize it affects stale-marker recovery elsewhere).

**The "busy marker present; skipping" log line gains the marker's age AND the resolved threshold.** Currently:

```
busy marker present; another pass is in progress — skipping iteration url=... pid=490170 stage=executor
```

Becomes:

```
busy marker present; skipping iteration url=... pid=490170 stage=executor age=53m threshold=10m pid_alive=false recovery_eligible=true
```

This makes the diagnostic visible directly in the log instead of requiring an operator to read the marker file's `started_at` AND do the math against the configured threshold.

**Marker check-in / liveness heartbeat is out of scope.** A future change could have the executor refresh `started_at` periodically (so a long-running but live executor doesn't trip the stale threshold), but the current spec is just "fix the recovery defect." Heartbeats are a separate concern with their own design surface.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one MODIFIED requirement (the existing busy-marker classification requirement now uses the new decoupled threshold AND immediate dead-pid recovery; the log-line shape includes the new fields).
  - `project-documentation` — one ADDED requirement: `OPERATIONS.md and CONFIG.md document the busy-marker-stale-threshold field and the decoupled recovery semantics`.
- **Affected code:**
  - `autocoder/src/config.rs` — extend `ExecutorConfig`:
    ```rust
    #[serde(default = "default_busy_marker_stale_threshold_secs")]
    pub busy_marker_stale_threshold_secs: u64,
    ```
    Plus `fn default_busy_marker_stale_threshold_secs() -> u64 { 600 }`. Clamp at 7200 with WARN.
  - `autocoder/src/polling_loop.rs` (or wherever `classify_busy_marker` lives) — restructure the classification branches:
    1. File absent → acquire, run iteration. (unchanged)
    2. **PID not in /proc → clear marker, WARN log, proceed. (NEW: no age check)**
    3. Age < `busy_marker_stale_threshold_secs` AND PID alive → skip iteration. (was: age < timeout+10min)
    4. Age ≥ threshold AND PID alive AND comm matches → SIGTERM the process group, wait 5s, SIGKILL if alive, clear marker, post chatops alert, proceed.
    5. Age ≥ threshold AND PID alive AND comm differs → ambiguous (PID reuse) → ERROR log, post chatops alert, SKIP iteration, leave marker for human inspection.
    6. Malformed JSON → WARN log, clear marker, proceed. (unchanged)
  - The "busy marker present; skipping" log line gains the new fields (age, threshold, pid_alive, recovery_eligible).
  - `docs/OPERATIONS.md` — update the busy-marker section's classification table.
  - `docs/CONFIG.md` — add the new field to the `executor:` table.
- **Operator-visible behavior:**
  - A daemon restart mid-iteration no longer bricks the affected repo. The next polling iteration's classification sees the dead PID AND clears the marker immediately.
  - Operators reading `journalctl` see the marker's age + threshold + recovery state inline; no need to read the marker file separately.
  - Operators with bumped `timeout_secs` no longer wait `timeout_secs + 10 min` for stale-marker recovery; the new threshold defaults to 10 min regardless of `timeout_secs`.
- **Breaking:** technically yes, in the sense that the recovery threshold changes from `executor.timeout_secs + 10 min` to a separate field defaulting to 600s. Operators who currently rely on the coupled threshold being long need to set `busy_marker_stale_threshold_secs` explicitly. The migration path: the daemon's startup log line names both the resolved `timeout_secs` AND the resolved `busy_marker_stale_threshold_secs` so operators see the values they're running with. A WARN-level startup log fires if the previous formula (`timeout_secs + 10 min`) would have produced a longer threshold than the new default, naming the gap explicitly.
- **Acceptance:** `cargo test` passes; `openspec validate a08-busy-marker-recovery-semantics --strict` passes. New unit tests cover: dead-pid recovery fires immediately regardless of age; live-pid recovery uses the new field; the new log-line format includes age + threshold + pid_alive + recovery_eligible.
