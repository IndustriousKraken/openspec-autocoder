## Why

a58 shipped the agentic reviewer as opt-in with `reviewer.kind` defaulting to `oneshot`, deliberately deferring the default flip: agentic-via-`claude` only reaches Anthropic-shaped endpoints, so flipping the default would have broken the reviewer for the exact operators the project is built for — the model-diversity operators running a non-Anthropic reviewer (Qwen/Ollama) for cross-check blind-spot coverage. a60 removes that blocker: `OpencodeStrategy` makes the agentic path provider-agnostic, so agentic is now a viable default for every provider.

This change makes the flip — `reviewer.kind` defaults to `agentic` — and makes it upgrade-safe. The agentic reviewer is the better default (reads files on demand, no 2M-char truncation, schema-validated verdict, discard-and-alert instead of default-approve), but the flip must not break an operator whose reviewer provider has no CLI installed on the daemon host. The safety mechanism is a startup fallback: when the effective kind is `agentic` but the resolved reviewer CLI is unavailable, the reviewer degrades to the `oneshot` HTTP path for that boot with one loud WARN, rather than failing. Because every provider has a working `oneshot` HTTP client, a missing CLI means "review over HTTP," never "no review."

## What Changes

**`reviewer.kind` defaults to `agentic` (code-reviewer).** The `Agentic reviewer mode` requirement is MODIFIED: the field now defaults to `agentic`. Operators who set `reviewer.kind: oneshot` explicitly keep the HTTP path unchanged; operators who set nothing get agentic when their reviewer CLI is available.

**Upgrade-safe startup fallback (code-reviewer).** When the effective reviewer kind is `agentic` (defaulted OR explicit) but the resolved reviewer CLI is unavailable at startup — its strategy is not registered, OR its binary is not found on the daemon host — the reviewer falls back to `oneshot` for that boot and logs one loud startup WARN naming the missing CLI and the remedy (install it, or set `reviewer.kind: oneshot`). Review is never disabled. This supersedes a58's "a reviewer command with no registered strategy returns a clear error" scenario: under the old `oneshot` default an explicit-agentic operator with a missing CLI was a hard misconfiguration to surface; under the new `agentic` default the same condition must degrade gracefully so the flip doesn't break existing reviewers. A restart or `autocoder reload` re-evaluates availability.

## Impact

- **Affected specs:**
  - `code-reviewer` — MODIFIED `Agentic reviewer mode` (default `oneshot` → `agentic`; add the startup CLI-availability fallback; replace the "no registered strategy → error" scenario with graceful fallback).
- **Affected code:**
  - `autocoder/src/config.rs` — `reviewer.kind` default flips to `Agentic` (the field stays optional; unset → `Agentic`).
  - `autocoder/src/code_reviewer.rs` — a startup CLI-availability check for the effective reviewer kind; on unavailable CLI, WARN once and route reviews through the `oneshot` path for the boot.
  - Startup preflight wiring — emit the one-time WARN (alongside the existing startup preflight/loud-warning patterns).
- **Operator-visible behavior:** operators who do not set `reviewer.kind` now get the agentic reviewer when their reviewer CLI is present (the common case — the daemon already requires `claude` for the executor). An operator whose reviewer provider has no installed CLI sees a one-time startup WARN and keeps reviewing over HTTP. Explicit `reviewer.kind` settings are unaffected.
- **Acceptance:** `cargo test` passes; `openspec validate a64-reviewer-agentic-by-default --strict` passes. Tests: unset `reviewer.kind` + available CLI → agentic; unset (or explicit agentic) + unavailable CLI → one WARN + oneshot for the boot (review not disabled); explicit `reviewer.kind: oneshot` → HTTP path, no agentic session, no WARN.
- **Dependencies:** stacks on **a58** (the `Agentic reviewer mode` requirement it modifies) and **a60** (the `opencode` strategy that makes agentic provider-agnostic and so justifies the default flip). Sorts after a60.
