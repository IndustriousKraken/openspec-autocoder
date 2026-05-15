## 1. Dependency

- [x] 1.1 Add `arc-swap = "1"` to `[dependencies]` in `autocoder/Cargo.toml`. Small, single-purpose crate; canonical Rust choice for atomic Arc-swap.

## 2. Control socket module

- [x] 2.1 Create `autocoder/src/control_socket.rs`. Public surface:
    ```rust
    pub fn socket_path() -> PathBuf;  // <system-temp>/autocoder/control/control.sock
    pub async fn listen(state: ControlState, cancel: CancellationToken) -> Result<()>;
    pub struct ControlState { /* Arc-held handles to swappable config + the config path */ }
    ```
- [x] 2.2 `socket_path()` returns `std::env::temp_dir().join("autocoder").join("control").join("control.sock")`. The directory is created on demand by `listen` before binding.
- [x] 2.3 `listen` removes any pre-existing file at `socket_path()`, binds a `tokio::net::UnixListener`, sets the file mode to `0o600` via `std::fs::set_permissions`, and enters an `accept` loop. Each accepted connection is handled in a spawned task. The accept loop watches the cancellation token via `tokio::select!`; on cancellation, the loop breaks AND the socket file is removed.
- [x] 2.4 Per-connection handler: read one line with `BufReader::read_line`, parse as JSON, dispatch on the `action` field, write the JSON response, close. Unknown actions return `{"ok": false, "error": "unknown action: <name>"}`. Malformed JSON returns `{"ok": false, "error": "<parse error>"}`.
- [x] 2.5 First registered action: `reload`. Handler implementation in §4.

## 3. Hot-swappable config holders

- [x] 3.1 In `cli/run.rs`, replace the direct ownership of `github`, `reviewer`, and `chatops` in each polling task with `Arc<ArcSwap<>>`-wrapped equivalents shared across tasks AND the control socket:
    ```rust
    let github = Arc::new(ArcSwap::from_pointee(cfg.github.clone()));
    let reviewer = Arc::new(ArcSwap::from_pointee(reviewer_initial));  // Option<Arc<CodeReviewer>>
    let chatops = Arc::new(ArcSwap::from_pointee(chatops_initial));    // Option<Arc<dyn ChatOpsBackend>>
    ```
- [x] 3.2 Plumb the `Arc<ArcSwap<...>>` handles through `polling_loop::run`'s signature. Internally, `execute_one_pass` reads via `holder.load()` at the top of the iteration (once per pass — not mid-iteration) to obtain an `Arc<T>` snapshot for that iteration.
- [x] 3.3 The `ChatOpsContext` struct (already passed through the loop) is rebuilt at the top of each iteration from the current `chatops` snapshot. Notification-flag fields update on reload along with the backend.
- [x] 3.4 **Verify:** existing tests that previously held a raw `Arc<CodeReviewer>` or similar update to construct the `ArcSwap`-wrapped form. No behavioral test changes expected.

## 4. Reload handler

- [x] 4.1 Implement `pub async fn handle_reload(state: &ControlState) -> serde_json::Value` in `control_socket.rs` (or a sibling module if it grows beyond ~150 lines). Sequence:
    1. Read the YAML at `state.config_path`. If IO error, return `{"ok": false, "error": "config file <path>: <error>"}`.
    2. Parse + validate via the same `Config::load_from` already used at startup.
    3. If validation fails, return `{"ok": false, "error": "<error>"}` naming the failure.
    4. Diff the new config against the current snapshots from each `ArcSwap` holder. For each section, classify as `unchanged` / `applied` / `requires_restart`.
    5. For `github`, `reviewer`, `chatops`: if changed, swap into the holder. Reviewer reconstruction may fail (e.g. bad LLM provider); on failure, log ERROR, leave the old reviewer in place, and report that section as `applied: false` with a per-section error in the response.
    6. For `repositories`, `executor`: if changed, do NOT apply; include in `requires_restart`.
    7. Return `{"ok": true, "applied": [...], "requires_restart": [...], "unchanged": [...]}`.
- [x] 4.2 The diff helpers compare structurally (not via raw equality on `Arc`-wrapped values). For complex sections like `reviewer` (which has nested `SecretSource`), the diff is "the YAML serialization differs" — re-serialize both and compare.

## 5. `autocoder reload` subcommand

- [x] 5.1 In `cli/mod.rs` (or wherever subcommands are registered), add a new `Reload` variant to the command enum. No arguments needed (the daemon knows its own config path).
- [x] 5.2 New module `cli/reload.rs` with `pub async fn execute() -> Result<()>`. Implementation:
    1. Open a `tokio::net::UnixStream` to `control_socket::socket_path()`. On connection refused or file-missing, print a clear error to stderr naming the socket path and likely causes, exit non-zero.
    2. Write `{"action":"reload"}\n`.
    3. Read one line of response.
    4. Parse as JSON.
    5. Pretty-print the response to stdout.
    6. Exit 0 if `ok == true`, else exit 1.
- [x] 5.3 Wire `Command::Reload => cli::reload::execute().await` in `cli::dispatch`.

## 6. Wire into daemon startup

- [x] 6.1 In `cli::run::execute`, after the polling tasks are spawned, spawn the control socket listener as an additional task: `tasks.spawn(control_socket::listen(state, cancel.clone()))`. The listener task is included in the same `JoinSet`, so it shares the lifecycle of the polling tasks.
- [x] 6.2 Construct `ControlState` with handles to the three `ArcSwap` holders plus the config path (from the `--config` argument). Pass it into the listener task.

## 7. Tests

- [x] 7.1 `control_socket::tests::socket_path_is_under_temp_autocoder_control` — assert the path string contains the expected segments.
- [x] 7.2 `control_socket::tests::reload_with_no_changes_responds_unchanged` — fixture: spawn the listener with a known config + ControlState, connect, send `{"action":"reload"}`, read response, assert `applied` is empty and all five sections appear in `unchanged`.
- [x] 7.3 `control_socket::tests::reload_applies_github_changes` — fixture with an initial config; write a new YAML to disk with a changed `github.token_env`. Send reload. Assert `applied` contains `"github"`, and the new `GithubConfig` is present in the holder after the reload returns.
- [x] 7.4 `control_socket::tests::reload_reports_requires_restart_for_executor_change` — same fixture; new YAML changes `executor.timeout_secs`. Send reload. Assert `requires_restart` contains `"executor"` and the in-memory executor is unchanged.
- [x] 7.5 `control_socket::tests::reload_rejected_on_invalid_yaml` — fixture writes invalid YAML to the config path. Send reload. Assert `ok == false` and the error message names the parse failure.
- [x] 7.6 `control_socket::tests::reload_rejected_on_validation_failure` — fixture writes valid YAML that fails semantic validation (e.g. two repos colliding on the same `local_path`). Assert `ok == false` and the error names the collision.
- [x] 7.7 `control_socket::tests::unknown_action_returns_error` — send `{"action":"nonsense"}`. Assert error response.
- [x] 7.8 `cli::reload::tests::exits_zero_on_ok_response` — fork a fake server on a temp Unix socket that responds `{"ok": true, ...}`; assert the CLI exit code is 0. (Or mock the unix-stream interaction.)
- [x] 7.9 `cli::reload::tests::exits_nonzero_on_failure_response` — same fake but responds `{"ok": false, "error": "x"}`. Assert exit code is non-zero.

## 8. Documentation

- [x] 8.1 README "Operating Notes" or new section "Runtime control": describe the control socket location, the `autocoder reload` command, what gets hot-applied vs requires-restart, the per-section response shape, and the validation-rejection behavior. No kitschy framing.
- [x] 8.2 README "Deployment": note that the `autocoder` user must own the socket file (it's chmod 0600), and `sudo -u autocoder autocoder reload` is the standard invocation.

## 9. Verification

- [x] 9.1 `cargo test` passes.
- [x] 9.2 `openspec validate daemon-control-socket-and-easy-reload --strict` passes.
