## Why

The `@<bot> status <repo>` reply's `currently:` line says either `idle` or `working on <change> (started <age> ago)`. The "working on <change>" branch fires when the busy marker has a non-empty `change` field. But for daemon work that isn't tied to a specific change (audit runs, recovery, rebuild-specs operations), the `change` field is empty, AND the status reply falls back to `idle` — even though the marker IS present AND the daemon IS busy.

This is the diagnostic gap an operator hits when their pending change isn't being picked up. They run `@<bot> status coterie`, see `currently: idle`, look at the queue (`1 pending`), and have no idea why the change isn't being processed. The actual cause might be:

- An audit is in flight (long-running LLM call, change field empty in the marker)
- A stale marker is blocking the iteration (per `a08`, dead-pid recovery may not have fired yet)
- The daemon is in a recovery operation (rebuild-specs, fork recreation)

Today's status reply collapses all of these into `idle`, which is misleading at best and actively obstructs operator self-diagnosis.

## What Changes

**The `currently:` line of the status reply SHALL surface the busy marker's actual contents.** When a busy marker exists, the line reports what the daemon is doing rather than `idle`:

- `working on <change> (started <age> ago)` — marker with non-empty `change` field (existing behavior, unchanged).
- `running audit <audit_type> (started <age> ago)` — marker with `stage=executor` AND `change` empty, AND an audit is currently logged as in-flight (the per-iteration audit-log file's path matches the marker's `started_at`). The audit_type is read from that log's header or filename.
- `<stage> in progress (started <age> ago)` — marker with any other `stage` value (`commit`, `review`, `push`, `pr`) AND `change` empty. Names the stage so the operator sees which phase the daemon is in.
- `recovery in progress (started <age> ago, type=<recovery-type>)` — when a recovery operation has stamped its own marker (rebuild-specs, fork recreation). These flows already write distinguishable marker metadata.
- `stale marker from pid <pid> (age <age>, recovery <eligible|in <duration>>)` — when the busy-marker classification per `a08` would skip this iteration AND the marker is recovery-eligible OR will be soon. Operators see at a glance "the daemon thinks it's busy but the marker is stale; recovery fires in X minutes."
- `idle` — no marker present (existing behavior, unchanged).

**The status code path reads the busy marker from the same resolved runtime-dir path the daemon writes to.** Today's pattern of "read from `/tmp/autocoder/busy/...` while the daemon writes to `<runtime_dir>/busy/...`" causes the status command to report `idle` while the daemon is actually busy. The fix is part of `a09`'s broader state-path-resolution sweep, but this spec depends on the read path being correct.

**The age formatting matches the existing convention.** Use the same human-readable format the existing status reply uses (`3m ago`, `2h17m ago`).

## Impact

- **Affected specs:**
  - `chatops-manager` — one MODIFIED requirement: the existing `Status reply always shows live workspace snapshot` requirement's `currently:` line scenario gains coverage for the new marker-content branches.
  - `project-documentation` — one ADDED requirement: `CHATOPS.md status reply documentation enumerates the new currently: line variants`.
- **Affected code:**
  - `autocoder/src/chatops/status.rs` (or wherever the status reply is composed) — when reading the busy marker, branch on `marker.stage` + `marker.change` + (for audit case) check whether an audit-log file matches the marker's `started_at` timestamp to identify the running audit type.
  - The branch order matters: check stale-marker (per `a08`'s classification) FIRST so operators see "stale marker" instead of "running audit X" for a stale marker. After that, check `change` non-empty → change-name branch. After that, check `stage=executor` + audit-log match → audit branch. After that, generic `<stage>` branch. Else: marker is present but unclassifiable → fall back to generic "busy (stage=<stage>, started <age> ago)".
  - The recovery-operation case (`rebuild-specs`, `recreate_fork_on_reinit`) writes its own marker metadata; the status code checks for the relevant marker shape AND emits the recovery line.
  - `docs/CHATOPS.md` — extend the `status` verb's reply-shape examples in the existing operator-recovery-commands section to show each new variant.
- **Operator-visible behavior:**
  - `@<bot> status` no longer says `idle` when the daemon is actually doing something. The reply identifies which phase / which audit / which recovery is in flight.
  - Operators diagnosing a "stuck pending change" see one of three useful lines: "running audit X" (just wait), "stale marker from pid Y, recovery in Z minutes" (`a08`'s fix will clear it), or `idle` + queue lines (truly nothing happening; possibly a different bug).
- **Breaking:** no. The existing `working on <change>` AND `idle` cases preserve their pre-spec format. The change is additive — new variants for cases the pre-spec format collapsed into one of those two.
- **Acceptance:** `cargo test` passes; `openspec validate a11-status-surfaces-busy-marker-contents --strict` passes. New unit tests cover each variant against fixture marker contents.
