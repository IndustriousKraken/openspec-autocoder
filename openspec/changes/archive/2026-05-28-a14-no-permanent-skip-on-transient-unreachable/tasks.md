## 1. Classification function

- [x] 1.1 Create `autocoder/src/recovery_classification.rs` (or sibling module). Public surface:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum RecoveryFailureClass {
      Transient,
      Permanent,
  }

  pub fn classify_recovery_failure(err: &anyhow::Error) -> RecoveryFailureClass { ... }
  ```
- [x] 1.2 Classification logic — return `Transient` when any of these patterns appear in the error or its source chain:
  - String contains "Could not resolve host" / "Connection timed out" / "Connection refused" / "TLS handshake" / "the remote end hung up" / "Network is unreachable" / "Temporary failure in name resolution" / "Operation timed out".
  - HTTP error with status 502, 503, 504, 522, 524 (gateway / upstream failures).
  - HTTP error with status 401 / 403 (auth blip — recoverable on token rotation without daemon restart).
  - HTTP error with status 429 (rate limit — retry after the existing throttle interval).
  - `git` exit code 128 with stderr matching any of the above strings.
  - I/O error kind = `WouldBlock` / `TimedOut` / `ConnectionReset` / `ConnectionAborted` / `BrokenPipe`.
- [x] 1.3 Return `Permanent` when:
  - The error or its source mentions any of: "permission denied" (after exhausting retry — distinguishable by call site OR by an outer wrapper) on the WORKSPACE side (not the remote auth side), "no such file or directory" for a required binary (`openspec`, `git`, `claude`), "invalid configuration", "malformed YAML", "no matching token route".
  - The "remains dirty after recovery" branch from the existing `Dirty workspace auto-recovers at startup` requirement.
- [x] 1.4 Default classification (unclassified error): `Transient`. The conservative choice is to retry — operators always have the `chatops 🛑 perma-stuck` AND manual-skip escape hatches if a genuinely-permanent failure is mis-classified as transient.
- [x] 1.5 Tests: each pattern from §1.2 AND §1.3 has a unit test against a fixture `anyhow::Error`. Default-to-transient case has a test.

## 2. Apply classification at recovery sites

- [x] 2.1 Locate every mid-iteration recovery call site (workspace init, git fetch, dirty cleanup). For each, when the call returns `Err`:
  - Call `classify_recovery_failure(&err)`.
  - Branch:
    - `Transient` → log WARN with `class=transient`, fire throttled chatops alert with "retrying" suffix, return from the iteration. Next iteration retries automatically.
    - `Permanent` → log ERROR with `class=permanent`, mark the repo as skipped-for-lifetime (existing helper), fire alert with "operator inspection required" suffix.
- [x] 2.2 The existing skip-for-lifetime path stays — only the conditions under which it fires change.
- [x] 2.3 Tests:
  - Iteration encounters a `Could not resolve host` git error → classified Transient → next iteration retries → if the second iteration succeeds, the repo proceeds normally.
  - Iteration encounters a "remains dirty after recovery" error → classified Permanent → skip-for-lifetime fires.

## 3. Alert text update

- [x] 3.1 In `autocoder/src/chatops/alert.rs` (or equivalent), extend the alert composition for the `WorkspaceInitFailure` category:
  - Transient suffix: ` (transient; retrying)`.
  - Permanent suffix: ` (permanent; skipped until daemon restart) — operator inspection required`.
- [x] 3.2 The 24h-per-(repo, category) throttle is unchanged. The suffix changes only the message text.
- [x] 3.3 Tests: alert composition for both classes produces the expected text.

## 4. Startup behavior is unchanged for this spec

- [x] 4.1 The existing `Dirty workspace auto-recovers at startup` requirement's scenarios continue to apply at startup. The classification logic introduced in this spec applies to MID-ITERATION recovery, not startup.
- [x] 4.2 Add a TODO comment in the startup path noting that a future spec could extend classification there too.

## 5. Docs

- [x] 5.1 In `docs/OPERATIONS.md`'s workspace-recovery sections (the existing dirty-workspace auto-recovery + the partial-clone self-heal sections), add a paragraph describing the new mid-iteration classification: transient failures (network, transport, auth blip) retry on the next polling iteration with a throttled alert; permanent failures (config errors, irrecoverable state, missing binaries) trigger skip-for-lifetime as before.
- [x] 5.2 Update the chatops-alert text examples in `docs/CHATOPS.md` to show the new suffix variants.

## 6. Spec deltas

- [x] 6.1 `openspec/changes/a14-no-permanent-skip-on-transient-unreachable/specs/orchestrator-cli/spec.md` ADDs one requirement covering the classification function, the per-class behavior, the alert-text update, AND the boundary (startup unchanged; mid-iteration classified).
- [x] 6.2 `openspec/changes/a14-no-permanent-skip-on-transient-unreachable/specs/project-documentation/spec.md` ADDs one requirement covering the OPERATIONS.md AND CHATOPS.md updates.

## 7. Verification

- [x] 7.1 `cargo test` passes (new + existing).
- [x] 7.2 `openspec validate a14-no-permanent-skip-on-transient-unreachable --strict` passes.
- [x] 7.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
