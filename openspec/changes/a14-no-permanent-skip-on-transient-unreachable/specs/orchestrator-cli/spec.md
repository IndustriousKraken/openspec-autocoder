## ADDED Requirements

### Requirement: Mid-iteration recovery failures classify transient vs. permanent; transient retries on next iteration
When a mid-iteration recovery operation (workspace re-clone, dirty cleanup, git fetch retry) returns `Err`, the daemon SHALL classify the failure into one of two categories via a `classify_recovery_failure(err) -> RecoveryFailureClass` helper:

- **Transient**: network errors (DNS resolution failures, connection refused / reset / timed out, TLS handshake failures), GitHub HTTP 5xx (502, 503, 504, 522, 524), HTTP 401 / 403 (auth blip — recoverable on token rotation without daemon restart), HTTP 429 (rate limit), git exit code 128 with stderr matching common network strings ("Could not resolve host", "Connection timed out", "the remote end hung up"), I/O error kinds (`WouldBlock`, `TimedOut`, `ConnectionReset`, `ConnectionAborted`, `BrokenPipe`).
- **Permanent**: configuration errors (missing required field, malformed YAML, no matching token route), missing prerequisites (binaries not on PATH: `openspec`, `git`, `claude`), "remains dirty after recovery" (the existing scenario from `Dirty workspace auto-recovers at startup`).

The default classification for an unrecognized error SHALL be `Transient` — the conservative choice is to retry, since operators have `clear-perma-stuck` AND manual-skip escape hatches for genuinely-permanent failures that mis-classify.

**Transient** failures: log WARN with `class=transient`, fire the existing 24h-throttled `WorkspaceInitFailure` chatops alert with a ` (transient; retrying)` suffix, return from the iteration. The NEXT polling iteration retries automatically — no special backoff state is needed.

**Permanent** failures: log ERROR with `class=permanent`, mark the repo as skipped-for-lifetime (existing helper), fire the alert with a ` (permanent; skipped until daemon restart) — operator inspection required` suffix.

This requirement applies to MID-ITERATION recovery only. Startup-time recovery (the existing `Dirty workspace auto-recovers at startup` requirement) continues its conservative skip-for-lifetime behavior. A future spec MAY extend classification to startup; not in scope here.

#### Scenario: Transient network failure retries automatically
- **WHEN** a mid-iteration recovery operation returns an error whose source chain contains "Could not resolve host github.com"
- **THEN** `classify_recovery_failure` returns `Transient`
- **AND** the iteration logs WARN with `class=transient`
- **AND** a chatops alert (subject to the 24h throttle) fires with the ` (transient; retrying)` suffix
- **AND** the repo is NOT marked skipped-for-lifetime
- **AND** the next polling iteration attempts the recovery again
- **AND** if that iteration succeeds, the repo proceeds normally

#### Scenario: HTTP 503 from GitHub is transient
- **WHEN** a mid-iteration `POST /repos/.../pulls` call returns HTTP 503
- **THEN** the classification is `Transient` (per the 5xx pattern match)
- **AND** the iteration retries on the next polling tick

#### Scenario: 401 auth blip retries (operator may rotate token without restart)
- **WHEN** a GitHub API call returns HTTP 401
- **THEN** the classification is `Transient`
- **AND** the operator can rotate the env-var-backed token (and the daemon's hot-reload picks it up via `autocoder reload`) without restarting

#### Scenario: Permanent failure skips-for-lifetime as before
- **WHEN** the dirty-workspace recovery commands all complete BUT `git status --porcelain` is still non-empty
- **THEN** `classify_recovery_failure` returns `Permanent`
- **AND** the iteration logs ERROR with `class=permanent`
- **AND** a chatops alert fires with the ` (permanent; skipped until daemon restart) — operator inspection required` suffix
- **AND** the repo is skipped for the daemon's process lifetime (existing behavior preserved)

#### Scenario: Default-to-transient handles unknown errors conservatively
- **WHEN** a recovery operation returns an error whose source chain matches none of the documented transient OR permanent patterns
- **THEN** `classify_recovery_failure` returns `Transient`
- **AND** the iteration logs WARN with `class=transient (unclassified)` so the unfamiliar pattern is visible in journalctl
- **AND** the next iteration retries — the choice to retry on unknown failures favors operator-friendly resilience over fast-fail-on-uncertainty

#### Scenario: Startup recovery is unchanged
- **WHEN** a workspace is dirty at daemon startup AND recovery fails
- **THEN** the existing `Dirty workspace auto-recovers at startup` requirement's behavior applies (skip-for-lifetime regardless of classification)
- **AND** this requirement applies only to mid-iteration recovery
