## 1. Config schema

- [ ] 1.1 In `autocoder/src/config.rs`, extend `ExecutorConfig`:
  ```rust
  #[serde(default = "default_busy_marker_stale_threshold_secs")]
  pub busy_marker_stale_threshold_secs: u64,
  ```
- [ ] 1.2 Add `fn default_busy_marker_stale_threshold_secs() -> u64 { 600 }`.
- [ ] 1.3 Clamp at startup: values > 7200 → clamp to 7200 with a WARN log naming the requested and clamped values. Value 0 is permitted (every marker is "stale"; recovery fires immediately for any age — useful for diagnostics).
- [ ] 1.4 Add the field to `config.example.yaml` under the `executor:` block, commented with an explanation.
- [ ] 1.5 Update the `project-documentation` config-example-coverage test list.
- [ ] 1.6 Tests: default parses; explicit values within bounds pass through; out-of-bounds values clamp with WARN.

## 2. Marker classification refactor

- [ ] 2.1 Locate the marker classification function (likely in `autocoder/src/polling_loop.rs` or a sibling module). The function takes the marker file's parsed contents AND returns a classification enum or makes the skip/proceed decision directly.
- [ ] 2.2 Restructure the classification branches in this order:
  1. **File absent** → acquire, run iteration. (unchanged)
  2. **Malformed JSON** → WARN log, clear marker, proceed. (unchanged)
  3. **PID not in `/proc`** → clear marker, WARN log naming the dead pid, proceed. **NEW: no age check.**
  4. **Age < `busy_marker_stale_threshold_secs` AND PID alive** → skip iteration with the enhanced log line.
  5. **Age ≥ threshold AND PID alive AND `comm` matches** → SIGTERM the process group, wait 5s, SIGKILL if still alive, clear marker, post chatops alert, proceed.
  6. **Age ≥ threshold AND PID alive AND `comm` differs** → ambiguous (PID reuse) → ERROR log, post chatops alert, SKIP iteration, leave marker.
- [ ] 2.3 The PID-alive check SHALL be a stat against `/proc/<pid>` (not signal-0 or other approaches that may not distinguish reliably). The check returns `false` on `ENOENT`; any other error (permission, transient) treats the PID as unknown → fall through to the "age < threshold" branch to be safe (assume alive).
- [ ] 2.4 Replace every reference to `executor.timeout_secs + 600` (or the `Duration::from_secs(timeout + 600)` formula) in the classification path with the new field.

## 3. Enhanced log line

- [ ] 3.1 The "busy marker present; skipping iteration" INFO log line MUST include the marker's age, the configured threshold, the PID-alive state, AND the recovery-eligibility:
  ```
  INFO ... busy marker present; skipping iteration url=<url> pid=<pid> stage=<stage> age=<duration> threshold=<duration> pid_alive=<bool> recovery_eligible=<bool>
  ```
- [ ] 3.2 Format `age` AND `threshold` as human-readable (`53m`, `10m`, `2h17m`) using the same convention chatops `status` uses (cap at hours; don't print seconds for ages > 1 min).
- [ ] 3.3 `recovery_eligible = !pid_alive || age >= threshold` (a marker is recovery-eligible when the dead-pid branch OR the live-pid-but-stale branch would fire on the next iteration).
- [ ] 3.4 Tests: format the log line for a fixture marker; assert the new fields appear with correct values.

## 4. Dead-pid recovery test

- [ ] 4.1 Unit test: a marker file with `pid = <a pid not in /proc>` AND `started_at = now - 1 second` (well under any threshold) → classification returns "recover and proceed."
- [ ] 4.2 Unit test: same marker file, but `started_at = now - 90 days` (well over threshold) → also "recover and proceed."
- [ ] 4.3 Unit test: marker with a live PID + age 1 second → "skip iteration."
- [ ] 4.4 Unit test: marker with a live PID + age past threshold + comm matches → "SIGTERM + recover."
- [ ] 4.5 Unit test: marker with a live PID + age past threshold + comm differs → "ambiguous; skip."

## 5. Startup log on threshold drift

- [ ] 5.1 At daemon startup, after resolving `executor.timeout_secs` AND `executor.busy_marker_stale_threshold_secs`:
  - If `busy_marker_stale_threshold_secs` < the previous formula's value (`timeout_secs + 600`) AND the operator did NOT explicitly set `busy_marker_stale_threshold_secs` in config:
    - Emit one INFO log line: `busy marker stale threshold is now <new>s (was implicit <old>s via timeout_secs+10min). Pre-spec operators raising timeout_secs no longer see proportional recovery delays. Set executor.busy_marker_stale_threshold_secs explicitly to override.`
  - Otherwise: one INFO log line naming both resolved values: `executor timeout: <X>s; busy_marker_stale_threshold: <Y>s`.
- [ ] 5.2 Detecting "operator did not explicitly set" requires the deserializer to preserve the "was field present" signal. Use serde's `#[serde(default)]` pattern that captures into an Option, then unwrap with the default — or use a sentinel value. Simplest: deserialize into a wrapper struct with `Option<u64>` for the new field, then resolve at config-validation time.
- [ ] 5.3 Test: a config without the field set produces the migration-aware INFO line; a config with the field explicit produces the regular line.

## 6. Docs

- [ ] 6.1 In `docs/OPERATIONS.md`'s `## Busy marker` section, update the classification table to reflect the new ordering AND the immediate dead-pid recovery. Add a paragraph explaining the decoupled threshold AND the new field.
- [ ] 6.2 In `docs/CONFIG.md`'s `executor:` table, add a row for `busy_marker_stale_threshold_secs` (type `u64`, default `600`, max `7200`).
- [ ] 6.3 In `docs/TROUBLESHOOTING.md`, add a section "Repo stuck on stale busy marker after daemon restart" describing the symptom (status shows idle but every iteration logs "busy marker present"), the immediate fix (delete the marker file), AND noting that the fix shipped in this spec eliminates the underlying cause for dead-pid markers.

## 7. Spec deltas

- [ ] 7.1 `openspec/changes/a08-busy-marker-recovery-semantics/specs/orchestrator-cli/spec.md` MODIFIES the existing busy-marker classification requirement to reflect the new dead-pid-immediate AND decoupled-threshold behavior AND the enhanced log line.
- [ ] 7.2 `openspec/changes/a08-busy-marker-recovery-semantics/specs/project-documentation/spec.md` ADDs one requirement covering the OPERATIONS.md and CONFIG.md updates.

## 8. Verification

- [ ] 8.1 `cargo test` passes (new + existing).
- [ ] 8.2 `openspec validate a08-busy-marker-recovery-semantics --strict` passes.
- [ ] 8.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
