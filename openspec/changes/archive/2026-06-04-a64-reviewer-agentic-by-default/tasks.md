# Implementation tasks

## 1. Flip the default (code-reviewer)

- [x] 1.1 `config.rs` — `reviewer.kind` defaults to `Agentic` when unset (the field stays optional; `None` resolves to `Agentic`). Explicit `oneshot` / `agentic` values are honored verbatim.

## 2. Upgrade-safe startup fallback (code-reviewer)

- [x] 2.1 At startup, when the effective reviewer kind is `agentic`, resolve the reviewer CLI (via `reviewer.command` + the a55/a56 `provider → CLI` rule) AND check availability: the strategy is registered AND the binary is found on the daemon host.
- [x] 2.2 If unavailable: log ONE loud startup WARN naming the missing CLI AND the remedy (install it, OR set `reviewer.kind: oneshot`), AND route reviews through the `oneshot` HTTP path for this boot. Review is NOT disabled.
- [x] 2.3 Availability is evaluated at startup (and on `autocoder reload` via the existing `reviewer:` hot-reload path); a transient resolution does not re-warn every iteration.
- [x] 2.4 Replace a58's "no registered strategy → error, no session" behavior with this fallback for the reviewer role specifically. (The contradiction-check / verifier-gate roles keep their own fail-open / advisory dispositions — unchanged.)

## 3. Tests

- [x] 3.1 Unset `reviewer.kind` + an available reviewer CLI → the reviewer runs in agentic mode.
- [x] 3.2 Unset `reviewer.kind` + an unavailable reviewer CLI (unregistered strategy OR missing binary) → one startup WARN naming the CLI + remedy, AND reviews run via the `oneshot` path for the boot (not disabled).
- [x] 3.3 Explicit `reviewer.kind: agentic` + unavailable CLI → same WARN + oneshot fallback (does not hard-fail the daemon).
- [x] 3.4 Explicit `reviewer.kind: oneshot` → HTTP one-shot path, no agentic session spawned, no fallback WARN.

## 4. Acceptance gate

- [x] 4.1 `cargo test` passes for the autocoder crate.
- [x] 4.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 4.3 `openspec validate a64-reviewer-agentic-by-default --strict` passes.
