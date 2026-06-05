# Implementation tasks

## 1. Resolve the CLI binary + its home-resident dependency closure (`sandbox.rs`)

- [ ] 1.1 Resolve the running strategy's CLI command to its real absolute path (`which`, then follow symlinks to the target).
- [ ] 1.2 Determine the binary's runtime dependency closure that resides under `$HOME` (e.g. a versioned install dir or bundled modules the launcher reads); paths outside `$HOME` are already visible via the `--ro-bind / /` base.
- [ ] 1.3 Add the resolved binary path and its home-resident deps to the `SandboxPlan` bound set (read-only, executable).

## 2. Bind them in each mechanism

- [ ] 2.1 `bwrap_argv`: bind the binary + deps back with `--ro-bind` AFTER the `--tmpfs <home>` so they survive the home mask.
- [ ] 2.2 systemd-run: add `BindReadOnlyPaths` entries for the binary + deps (alongside the existing workspace + store binds under `ProtectHome=tmpfs`).
- [ ] 2.3 macOS `sandbox-exec` (a73): allow read+exec on the resolved binary path + deps in the generated profile.
- [ ] 2.4 Keep the binding surgical — only the binary and its dependencies, never the whole home directory; the rest of home stays masked.

## 3. Plumb the resolved path

- [ ] 3.1 `agentic_run` passes the strategy's resolved CLI binary path into the `SandboxPlan` so the allowlist can bind it.

## 4. Tests

- [ ] 4.1 Given a CLI binary path under `$HOME`, the sandbox plan binds it (and its symlink target) read-only/executable.
- [ ] 4.2 The rest of `$HOME` (other stores, `~/.ssh`, autocoder config) is NOT bound.
- [ ] 4.3 A symlinked binary resolves to and binds its real target.
- [ ] 4.4 A system-path CLI (`/usr/local/bin/<cli>`) resolves and is included (no regression).

## 5. Acceptance gate

- [ ] 5.1 `cargo test` passes for the autocoder crate.
- [ ] 5.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] 5.3 `openspec validate a012-sandbox-binds-cli-binary --strict` passes.
