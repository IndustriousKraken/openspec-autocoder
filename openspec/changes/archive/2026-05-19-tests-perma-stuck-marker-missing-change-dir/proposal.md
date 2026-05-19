## Why

`autocoder/src/perma_stuck.rs::write_marker` (lines 40-65) has an
explicit early-return error:

```rust
if !parent.is_dir() {
    return Err(anyhow!(
        "change directory does not exist: {}",
        parent.display()
    ));
}
```

This guards against a caller-misuse pattern: writing the perma-stuck
marker for a change whose `openspec/changes/<change>/` directory does
not exist (e.g. the change was deleted out-of-band, or the caller
passed a typo). The error is reachable in production via
`polling_loop::handle_failure_counter` if the queue was reshaped
between the iteration's `record_failure` and the perma-stuck write.

Existing tests in `perma_stuck.rs` cover only the happy path
(`write_then_exists_returns_true`), `remove_marker` idempotency, and
the `marker_exists` false case. The guard branch is **untested**.
A regression that swapped `parent.is_dir()` for `parent.exists()` (or
removed the guard entirely, letting tempfile's create-in-parent fail
with a less specific error) would still pass the suite.

## What Changes

Add one test under `autocoder/src/perma_stuck.rs`'s existing tests
module that calls `write_marker` against a workspace where the change
directory has never been created, and asserts the result is an `Err`
whose message contains `change directory does not exist` AND the
attempted change name.

No production code changes.

## Impact

- Affected code: `autocoder/src/perma_stuck.rs`
  (`#[cfg(test)] mod tests`).
- No spec changes — the queue-engine spec governs `marker_exists`
  semantics (presence-only check) but not the write-side guard,
  which is an internal caller-misuse safeguard.
- Breaking: no.
