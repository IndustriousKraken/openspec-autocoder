## Why

When workspace-init / clone / fetch recovery fails (per the existing `Dirty workspace auto-recovers at startup` requirement's "remains dirty after recovery" scenario AND the implicit equivalent for unreachable-remote cases), the repo is skipped for the daemon's process lifetime. The intent was conservative: a genuinely-unrecoverable workspace state probably needs operator inspection, not infinite retry.

In practice, the dominant cause of recovery failures is GitHub's intermittent unreachability. Operators on a working deployment report `unreachable` warnings multiple times per week as Microsoft/Azure pushes updates that briefly break github.com or the API endpoints. A daemon that gives up forever after a single network blip turns a 5-minute outage into a daemon-restart-required outage — fragility disproportionate to the actual problem.

The fix: separate transient failures (retry on next iteration) from genuinely-permanent failures (skip for daemon lifetime). Transient causes — network unreachable, transport timeout, auth-token blip, DNS resolution failure, HTTP 5xx, git fetch returning a "connection reset" error — go in the retry bucket with throttled chatops alerts. Truly permanent causes — configuration errors (missing required env var), irrecoverable workspace state (corruption, manual operator intervention required), missing prerequisites (openspec binary gone) — stay in the skip-for-lifetime bucket.

The bar for "permanent" is high: anything where "the same iteration five minutes later might succeed" is transient.

## What Changes

**Failure classification at the workspace-init / recovery layer.** When a recovery operation fails (git fetch / git clone / dirty-workspace cleanup), the failure SHALL be classified into one of two categories:

- **Transient** — network errors (DNS, connection refused, timeout, TLS handshake failure), HTTP 5xx from GitHub, git exit codes corresponding to network/transport (e.g. `git fetch` exiting 128 with stderr mentioning "Could not resolve host" / "Connection timed out" / "TLS handshake failed" / "the remote end hung up"), 4xx auth errors from GitHub (these are recoverable when the operator rotates a token without restarting). The iteration logs a WARN, the repo's failure-state increments, throttled chatops alert fires (per the existing `WorkspaceInitFailure` category which is already 24h-per-(repo, category)-throttled), AND the next polling iteration retries.
- **Permanent** — configuration errors (missing field, malformed YAML), `openspec` binary missing, workspace path collision detected at startup, irrecoverable git state where recovery commands themselves all complete successfully BUT the workspace is still dirty (i.e. the existing "remains dirty after recovery" scenario). These continue to use the existing skip-for-lifetime behavior.

**Classification logic lives in one helper function.** A `classify_recovery_failure(err: &Error) -> RecoveryFailureClass` function takes the error AND returns `Transient` or `Permanent`. The function inspects the error's source chain for the patterns above. The function is unit-testable against fixture error values.

**Retry-with-backoff is NOT introduced.** Failed iterations just wait for the next polling tick (which is `poll_interval_sec`, default 300s). The existing throttled-alert mechanism already prevents alert spam (one alert per 24h per (repo, category)); no additional backoff is needed at the polling layer.

**Operator-visible alert text changes slightly.** Today's `WorkspaceInitFailure` alert reads roughly:

```
⚠️ <repo>: workspace init failure for the past 24h. Latest: <error excerpt>
```

After this spec, transient alerts add a "retrying" suffix:

```
⚠️ <repo>: workspace init failure (transient; retrying) for the past 24h. Latest: <error excerpt>
```

Permanent failures (which the existing skip-for-lifetime path handles) get an "operator inspection required" suffix:

```
⚠️ <repo>: workspace init failure (permanent; skipped until daemon restart) — operator inspection required. Latest: <error excerpt>
```

This makes the operator's choice immediate: transient → wait, permanent → look.

**Startup recovery behavior unchanged for now.** The `Dirty workspace auto-recovers at startup` requirement's existing scenarios continue to apply at startup: try recovery, if successful proceed, if recovery itself fails skip-for-lifetime. The classification change applies to MID-ITERATION recovery (workspace becomes dirty between polls, network call fails during an iteration). Startup-time failures still treat the conservative path. A future spec could extend the classification to startup, but that's out of scope for this one — startup-time skip-for-lifetime is rarer (operator typically restarts the daemon to recover) AND less fragile than mid-iteration.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one ADDED requirement: `Mid-iteration recovery failures classify transient vs. permanent; transient retries on next iteration`.
  - `project-documentation` — one ADDED requirement: `OPERATIONS.md describes the transient vs. permanent classification AND the new alert text variants`.
- **Affected code:**
  - `autocoder/src/polling_loop.rs` (or wherever mid-iteration recovery handling lives) — when recovery returns `Err`, call `classify_recovery_failure(&err)`. Branch:
    - `Transient`: log WARN with the classification, increment failure-state, fire throttled chatops alert with "retrying" suffix, return from the iteration (next iteration retries naturally).
    - `Permanent`: log ERROR with the classification, mark repo as skipped-for-lifetime (existing behavior), fire alert with "operator inspection required" suffix.
  - New function `classify_recovery_failure(err: &Error) -> RecoveryFailureClass` in a sibling module. Inspects `err.source().to_string()` for the pattern matches. Where the error type is structured (e.g. `git2::Error` for libgit2 cases), use the structured fields.
  - `autocoder/src/chatops/alert.rs` (or equivalent) — extend the alert composition to include the transient/permanent suffix per the format above.
  - `docs/OPERATIONS.md` — update the workspace-recovery section.
- **Operator-visible behavior:**
  - Most github.com hiccups stop causing repo-lifetime skips. The daemon's WARN logs surface them, the throttled chatops alert names them, AND the next polling iteration retries.
  - Genuinely permanent failures still get the operator-action signal — the suffix in the alert AND the absence of any "retrying" hint make it clear what's required.
  - The daemon recovers from transient GitHub outages without operator intervention.
- **Breaking:** behavior change, but in the operator-favorable direction. Operators relying on the daemon's previous "fail fast and stop" behavior for some reason can simulate it by setting `executor.timeout_secs` low AND letting iterations fail on every recovery attempt (no actual mechanism to opt OUT of the new classification — the change is unconditional).
- **Acceptance:** `cargo test` passes; `openspec validate a14-no-permanent-skip-on-transient-unreachable --strict` passes. Unit tests cover the classification function against fixture error values for each transient AND permanent pattern. Integration test simulates a transient network failure → next iteration succeeds; a permanent failure → repo is skipped for daemon lifetime.
