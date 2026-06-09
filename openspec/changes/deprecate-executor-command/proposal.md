## Why

`executor.command` lets an operator point the daemon at a specific `claude` binary. It predates a014 (which now captures the operator's login-shell PATH and propagates it to every agentic subprocess, including the implementer), so its original job — "find claude when it's not on the service PATH" — is handled by simply having `claude` on the login PATH. The canonical way to control which binary a strategy spawns is OS-level: put the right `claude`/`opencode`/`agy` on the daemon user's PATH, and symlink the canonical name to a fork/clone if needed (a wrapper script can even inject flags). The config field is a redundant, claude-only knob that produced a confusing asymmetry (only claude had a configurable path) and, when set to a custom path, mis-routed non-claude roles.

Rather than remove it (which would break existing configs and churn several spec requirements + scenarios for a field that is invisible at its default), we deprecate it: keep accepting it for backward compatibility, but stop advertising it.

## What Changes

- `executor.command` is marked **deprecated** in the source. It remains accepted AND honored (no behavior change), so existing configs keep working.
- It is **removed from the operator-facing surfaces**: the `config.example.yaml` entry and the `docs/CONFIG.md` row. New operators are steered to the OS-level approach (PATH + symlinks), which is documented where the daemon's binary resolution is described.
- The "every field appears in `config.example.yaml`" standard gains a **deprecated-field carve-out**: a field marked deprecated in the source is intentionally absent from `config.example.yaml`, `docs/CONFIG.md`, AND (when its name is not shared with a live field) the coverage test's field-name list — so operator surfaces carry no cruft. The carve-out is general; `executor.command` is its first application.

## Impact

- **Affected specs:** `project-documentation` — MODIFY `config.example.yaml is the canonical operator reference for the YAML schema` (add the deprecated-field carve-out).
- **Affected code:** `config.rs` — a `DEPRECATED` doc comment on `ExecutorConfig::command` (no functional change; the field is still parsed AND still honored by the implementer's strategy resolution). `config.example.yaml` — drop the commented `command:` line from the `executor:` block. `docs/CONFIG.md` — drop the `command` row. The coverage test is unchanged: `command` stays in `EXPECTED_FIELDS` because the name is still live via the reviewer's `command` field (and `command_authorization`).
- **Operator-visible:** no behavior change. The example + CONFIG no longer show `executor.command`; the binary-resolution docs say "put the CLI on the daemon login PATH; symlink the canonical name for a fork." An operator who already set `executor.command` keeps working.
- **Non-goals:** NOT removing the field from the schema (that would error existing configs); NOT changing how a set `executor.command` behaves; NOT touching the reviewer's analogous `command` (a separate field, left for a follow-up if wanted).
- **Acceptance:** `cargo test` (config round-trip + the example-coverage test still pass) + `openspec validate deprecate-executor-command --strict`.
