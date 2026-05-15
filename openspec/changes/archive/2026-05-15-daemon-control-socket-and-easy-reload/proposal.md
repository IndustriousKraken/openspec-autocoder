## Why

Operators need to update tokens, change the reviewer's LLM credentials, or adjust ChatOps configuration without disturbing in-flight Claude runs. Today the only way to apply a config change is `systemctl restart autocoder`, which either kills the daemon mid-iteration (orphaning Claude subprocesses and leaving busy markers behind) or waits 90 seconds for the iteration to finish and then gets SIGKILL'd anyway.

A control socket plus an `autocoder reload` CLI command lets operators apply most config changes at iteration boundaries with no disruption to running work. Tokens, reviewer setup, and chatops backend can be hot-swapped this way. The harder case (adding/removing repositories) is deferred to a follow-up change.

## What Changes

- **ADDED capability:** `orchestrator-cli` gains a Unix domain socket at `/tmp/autocoder/control/control.sock` (or under `/run/autocoder/` if the daemon has access to it) that accepts JSON line-delimited control requests. The daemon spawns a control-listener tokio task at startup and handles requests concurrently with the polling tasks.
- **ADDED capability:** `orchestrator-cli` gains an `autocoder reload` subcommand. It connects to the control socket, sends `{"action":"reload"}`, and prints the daemon's response. Exit 0 if the response indicates success; exit non-zero if the reload was rejected.
- **ADDED capability:** `orchestrator-cli`'s reload handler re-reads the YAML path the daemon was launched with, validates fully (`serde_yaml::from_str` with `deny_unknown_fields` + the same semantic checks startup runs), diffs against the in-memory config, hot-applies changes to `github`, `reviewer`, and `chatops`, and reports per-section status (`applied`, `unchanged`, `requires-restart`, or `rejected-validation`) in the response.
- **Hot-swap mechanics:**
  - `github` (per-owner tokens, default `token_env`, `fork_owner`): swap the `Arc<ArcSwap<GithubConfig>>` shared across all polling tasks. Existing iterations finish with the old `GithubConfig` (they already resolved their token at the top of the iteration); subsequent iterations read the new one.
  - `reviewer`: reconstruct `CodeReviewer` from the new config, swap the `Arc<ArcSwap<Option<CodeReviewer>>>`. If the reviewer was disabled and is now enabled (or vice versa), the swap toggles the `Option`. In-flight reviews finish with the old reviewer.
  - `chatops`: reconstruct the `ChatOpsBackend` from the new config, swap the `Arc<ArcSwap<Option<dyn ChatOpsBackend>>>`. Same in-flight semantics.
- **Validation failure is non-disruptive:** if the new YAML fails to parse or fails semantic validation, the reload is rejected, the daemon continues running with the previous in-memory config, and the CLI client sees an error response naming exactly what failed.
- **Restart-required sections (this change does NOT hot-reload):** `repositories` (deferred to follow-up), `executor` (only one executor instance exists, shared across tasks; not commonly changed). The reload handler reports these in the response as `requires-restart` when their config differs in the new YAML, so the operator knows exactly which fields still need a full restart.
- **Code:**
  - New `autocoder/src/control_socket.rs` module: listener task, request parsing, response formatting.
  - New CLI subcommand `cli::reload`.
  - `cli::run::execute` wires the existing `Arc`-held config values through `ArcSwap` so they can be replaced atomically.
  - `polling_loop::execute_one_pass` reads from the swappable holders at the top of each iteration.
  - A new `Cargo.toml` dep: `arc-swap = "1"` (small, well-maintained, the canonical choice for this pattern in Rust).

## Impact

- Affected specs: `orchestrator-cli` (three ADDED requirements: control socket, reload subcommand, reload handler).
- Affected code: `autocoder/src/cli/`, `autocoder/src/polling_loop.rs`, `autocoder/src/cli/run.rs`, new `autocoder/src/control_socket.rs`.
- New runtime artifact: a Unix domain socket under `/tmp/autocoder/control/`. Permissions: 0600, owned by the autocoder user. Operators not running as the autocoder user need `sudo -u autocoder autocoder reload` (which is fine — they already use `sudo -u autocoder` for other inspection commands).
- New dependency: `arc-swap`. Stable crate, tiny surface, widely used.
- Breaking? No. Existing systemctl-restart-based workflows still work; the new path is additive.
- Operator workflow change: edit `config.yaml`, run `sudo -u autocoder autocoder reload`. Per-iteration tokens / reviewer / chatops settings update at the next iteration boundary for each repo. Adding/removing repos still requires restart (until follow-up change ships).
