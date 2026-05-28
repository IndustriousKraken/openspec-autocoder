## 1. Status reply composer extension

- [x] 1.1 Locate the status reply composer (likely in `autocoder/src/chatops/status.rs` or similar). The current code reads the busy marker AND produces either `idle` or `working on <change> (started <age> ago)`.
- [x] 1.2 Refactor the `currently:` line logic to a single function that branches on marker contents in this order:
  1. **No marker present** → `idle`.
  2. **Marker present AND classification per `a08` says stale (dead pid OR age ≥ threshold)** → `stale marker from pid <pid> (age <age>, recovery eligible)` OR `(age <age>, recovery in <duration>)` if still under threshold.
  3. **Marker present AND `change` non-empty** → `working on <change> (started <age> ago)`.
  4. **Marker present AND `stage=executor` AND `change` empty AND an audit log matches the marker's `started_at`** → `running audit <audit_type> (started <age> ago)`.
  5. **Marker present AND `stage` ∈ {commit, review, push, pr}** → `<stage> in progress (started <age> ago)`.
  6. **Marker present AND `stage` matches a recovery operation (rebuild-specs, fork recreation)** → `recovery in progress (started <age> ago, type=<recovery-type>)`.
  7. **Marker present but unclassifiable** → `busy (stage=<stage>, started <age> ago)` fallback.
- [x] 1.3 The age formatting matches the existing convention (`3m ago`, `2h17m ago` — hours+minutes for older).

## 2. Audit-type lookup

- [x] 2.1 When the marker has `stage=executor` AND `change` is empty, the status code attempts to identify which audit is running by scanning the audit logs directory:
  ```
  <logs_dir>/runs/<workspace-basename>/audits/<audit_type>-<UTC-RFC3339>.log
  ```
  The filename's UTC timestamp matches the marker's `started_at` (or is within 1 second of it). If a match exists, return the audit_type from the filename. If no match, return `None` AND fall through to the generic `executor in progress` line.
- [x] 2.2 The lookup uses the daemon's resolved logs-dir path (per `a09`). No hard-coded `/tmp/autocoder/logs/...` literals.
- [x] 2.3 Tests:
  - Marker stage=executor, change="", audit log file matches timestamp → returns `running audit <type>`.
  - Marker stage=executor, change="", no audit log matches → falls through to generic `executor in progress`.
  - Marker stage=commit, change="a36-expense-tracking" → returns `working on a36-expense-tracking` (change branch takes priority).

## 3. Stale-marker detection in status

- [x] 3.1 The status code reuses `a08`'s classification helper. When the helper says "this marker would trigger recovery on the next iteration" (dead pid OR age ≥ threshold), the status emits the stale-marker line.
- [x] 3.2 Format:
  - Dead pid (recovery fires immediately per `a08`): `stale marker from pid <pid> (age <age>, recovery eligible now)`.
  - Live pid + age ≥ threshold (SIGTERM recovery fires next iteration): `stale marker from pid <pid> (age <age>, threshold passed, recovery eligible next iteration)`.
  - Live pid + age < threshold + age > 80% of threshold (recovery soon): `stale marker from pid <pid> (age <age>, recovery in <remaining-time>)`. The 80%-threshold heuristic surfaces upcoming recoveries before they fire, so operators see "stuck-feeling" markers as transitioning rather than permanent.

## 4. Recovery-operation marker shapes

- [x] 4.1 Identify which recovery operations stamp distinguishable markers today: rebuild-specs, fork recreation. Each likely sets a unique `stage` value or extends the marker schema. Read the actual marker contents and surface them in the status reply.
- [x] 4.2 If no distinguishable marker shape exists for a recovery operation, the spec doesn't add one — the operation falls through to the generic `<stage> in progress` line.

## 5. Docs

- [x] 5.1 In `docs/CHATOPS.md`'s operator-recovery-commands section, extend the `status` verb's reply-shape examples to include each new variant:
  ```
  currently: idle
  currently: working on a36-expense-tracking (started 3m ago)
  currently: running audit drift_audit (started 14m ago)
  currently: push in progress (started 12s ago)
  currently: stale marker from pid 490170 (age 53m, recovery in 7m)
  currently: stale marker from pid 490170 (age 100m, recovery eligible now)
  ```
- [x] 5.2 Add a paragraph explaining the diagnostic value: operators wondering why a pending change isn't being picked up can read the `currently:` line AND distinguish "audit in flight, just wait" from "stale marker, need a08's recovery to fire (or manual `rm`)".

## 6. Spec deltas

- [x] 6.1 `openspec/changes/a11-status-surfaces-busy-marker-contents/specs/chatops-manager/spec.md` MODIFIES the existing `Status reply always shows live workspace snapshot` requirement to add scenarios for each new variant.
- [x] 6.2 `openspec/changes/a11-status-surfaces-busy-marker-contents/specs/project-documentation/spec.md` ADDs one requirement covering the CHATOPS.md documentation update.

## 7. Verification

- [x] 7.1 `cargo test` passes (new + existing).
- [x] 7.2 `openspec validate a11-status-surfaces-busy-marker-contents --strict` passes.
- [x] 7.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
