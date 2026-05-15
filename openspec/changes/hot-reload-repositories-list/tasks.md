## 1. Per-task config holder

- [x] 1.1 Change `polling_loop::run`'s signature: `repo: RepositoryConfig` becomes `repo: Arc<ArcSwap<RepositoryConfig>>`. The function reads `repo.load()` at the top of each iteration to obtain an `Arc<RepositoryConfig>` snapshot.
- [x] 1.2 Within `run`, the iteration body uses the snapshot reference (`&snapshot`) exclusively. The snapshot lives for the iteration; the next iteration re-reads via `load()`.
- [x] 1.3 The inter-poll sleep duration is read from the SNAPSHOT, not the current swap. Rationale: lets the operator see "next iteration uses the new poll_interval"; an alternative semantics (next sleep duration uses the new value mid-cycle) is more complex without a clear benefit.
- [x] 1.4 **Verify:** existing tests of `polling_loop::run` and `execute_one_pass` update to construct `Arc<ArcSwap<RepositoryConfig>>` instead of owned `RepositoryConfig`. Test scaffolding helpers (`fixture_repo`, etc.) gain wrapper variants.

## 2. Per-repo cancellation tokens + task map

- [x] 2.1 Define `RepoTaskHandle { cancel: CancellationToken, config: Arc<ArcSwap<RepositoryConfig>>, join: JoinHandle<()> }` in `cli/run.rs`.
- [x] 2.2 Daemon-level state: `Arc<Mutex<HashMap<String, RepoTaskHandle>>>` keyed by repo URL.
- [x] 2.3 At startup, for each configured repo: create `let cancel = global_cancel.child_token()`; create the swap holder seeded with the repo's config; spawn the polling task with both; insert the `RepoTaskHandle` into the map.
- [x] 2.4 Each polling task, on exit, removes its own entry from the map. (Acquire the mutex, remove, drop the lock.)
- [x] 2.5 The global cancellation token (from SIGTERM/SIGINT) cancels every child token via the existing `child_token()` parent-cancels-child semantics. No special-case code needed in tasks.

## 3. Reload handler extension

- [x] 3.1 In the reload handler (from `daemon-control-socket-and-easy-reload`), add a `repositories` step after the `chatops` step. Diff `current_urls` (the task map's keys) vs `new_urls` (the parsed new YAML's `repositories[].url` list).
- [x] 3.2 For each URL in `removed = current - new`: take the lock on the task map, look up the handle, call `handle.cancel.cancel()`. Do NOT remove the map entry here; the task's exit path does that.
- [x] 3.3 For each URL in `added = new - current`: spawn a new polling task with a child cancellation token + a new swap holder. Insert into the map under the lock. If the URL is somehow already in the map (transient state from a recently-cancelled task that hasn't exited), log WARN and skip — count this URL as `unchanged` for the response, not `added`.
- [x] 3.4 For each URL in `existing = new ∩ current`: compare the new entry's `RepositoryConfig` (excluding `url`) against the current snapshot. If different, call `handle.config.store(Arc::new(new_config))` to swap. The next iteration of that task picks up the new values.
- [x] 3.5 Populate the response: `applied` includes `"repositories"` iff at least one of `added`, `removed`, or `changed` is non-empty. `repositories_delta` always present in the response (even if empty) so client tooling has a consistent shape.

## 4. Promote executor to the only restart-required section

- [x] 4.1 In the same handler, the `requires_restart` reporting for `repositories` is removed (it's now hot-applied). Only `executor` remains in the restart-required category. Update the test from §7.4 of `daemon-control-socket-and-easy-reload` to reflect the new defaults if it asserts on the section list.

## 5. Tests

- [x] 5.1 `control_socket::tests::reload_adds_repository_spawns_task` — fixture daemon with one repo; write a new YAML with two repos. Send reload. Assert the response's `applied` contains `"repositories"`, `repositories_delta.added` contains the new URL, AND a polling task is now running for the new URL (verify via task-map inspection or via a fixture executor that records which repos it was invoked against).
- [x] 5.2 `control_socket::tests::reload_removes_repository_cancels_task` — fixture with two repos; new YAML has one. Send reload. Assert the removed URL is in `repositories_delta.removed`. Use a slow-iteration fixture executor (small sleep) so the cancellation can be observed: after a short wait, the removed repo's task has exited and is no longer in the map.
- [x] 5.3 `control_socket::tests::reload_changes_repository_settings_in_place` — fixture with one repo whose `base_branch` is `main`. New YAML changes it to `dev`. Send reload. Assert the swap holder now contains `base_branch == "dev"`, AND the URL is in `repositories_delta.changed`, NOT in `added` or `removed`.
- [x] 5.4 `control_socket::tests::reload_repo_url_change_is_remove_plus_add` — fixture with one repo URL X; new YAML has URL Y instead. Send reload. Assert `removed: [X]` and `added: [Y]`.
- [x] 5.5 `control_socket::tests::reload_executor_change_still_requires_restart` — same as the existing daemon-control-socket test but verify it still works after this change layers in.
- [x] 5.6 `control_socket::tests::reload_transient_cancelled_url_is_not_respawned` — set up the transient state by: (a) acquire the task map lock, (b) manually insert a handle with `cancel` already cancelled (simulating a task that's mid-shutdown), (c) trigger reload with that URL still in the YAML. Assert: WARN log, response's `repositories_delta.added` does NOT include the URL, AND no second task is spawned.

## 6. Documentation

- [x] 6.1 README "Runtime control": expand the per-section table to indicate `repositories` is now hot-applied. Note the URL-as-identity rule and the in-flight-iteration safety guarantees.
- [x] 6.2 README "Runtime control": add a short subsection "Adding a repository at runtime" with the operator flow: edit YAML, run `sudo -u autocoder autocoder reload`, expect the response's `repositories_delta.added` to confirm.

## 7. Verification

- [x] 7.1 `cargo test` passes.
- [x] 7.2 `openspec validate hot-reload-repositories-list --strict` passes.
