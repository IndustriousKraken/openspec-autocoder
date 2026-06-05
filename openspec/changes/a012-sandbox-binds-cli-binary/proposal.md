## Why

With `bwrap` now working, the sandbox fails to exec the wrapped CLI: `bwrap: execvp /home/<user>/.local/bin/claude: No such file or directory`. The cause is the credential-protection design colliding with where the CLI actually lives. The bwrap path does `--ro-bind / /` then `--tmpfs <home>` to mask the home directory (and binds the workspace + the self-CLI config store back), and the systemd-run path does `ProtectHome=tmpfs` + `BindReadOnlyPaths` for the store — but **neither binds the CLI binary back**. `claude`, `agy`, and `opencode` all install to `~/.local/bin/` by default, which the home mask hides, so the subprocess cannot find the binary to exec.

`a006`'s allowlist names "the minimal runtime (binaries/libraries)" but the implementation only made system paths visible (via `--ro-bind / /`) and masked everything under home — it did not account for the **user-local CLI install** that is the norm for these tools. This is a sandbox-allowlist defect, not a missing dependency, and it blocks every agentic run on a host where the CLI is installed under `$HOME`.

## What Changes

**The sandbox's filesystem allowlist binds the resolved CLI binary (and its runtime dependency closure) even when installed under the home directory** — `~/.local/bin/<cli>` and whatever it loads — read-only and executable, so the wrapped CLI execs inside the sandbox. Binary resolution follows symlinks to the real target. The binding is **surgical**: the specific binary and its dependencies, not the whole home directory; the rest of home (other CLIs' config stores, `~/.ssh`, autocoder's own config and state) stays masked. This is the resolution of "minimal runtime (binaries/libraries)" for user-local installs, and it applies to every mechanism — `bwrap` (bind back after the home tmpfs), `systemd-run` (`BindReadOnlyPaths`), and the macOS `sandbox-exec` profile (allow read+exec on the binary path).

## Impact

- **Affected specs:** `executor` — ADD `The sandbox binds the wrapped CLI binary even when installed under the home directory`.
- **Affected code:** `sandbox.rs` — resolve the running strategy's CLI command to its real absolute path (`which` + follow symlinks) and compute its home-resident runtime dependency closure; add them to the `SandboxPlan` bound set; in `bwrap_argv`, bind them back AFTER the `--tmpfs <home>` so they survive the home mask; on the systemd-run path, add the corresponding `BindReadOnlyPaths`. `agentic_run` passes the resolved binary path into the plan.
- **Operator-visible behavior:** a CLI installed under `~/.local/bin` (the default for `claude`/`agy`/`opencode`) execs inside the sandbox instead of failing `execvp`; the rest of the home directory stays masked.
- **Dependencies:** fixes the live `a006` sandbox. Complements `a011` (whose `doctor` checks the CLI is present *on the host*; this ensures it is *reachable inside the sandbox* — different checks). Platform-general: `bwrap`, `systemd-run`, and the `a73` `sandbox-exec` profile.
- **Acceptance:** `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean; `openspec validate a012-sandbox-binds-cli-binary --strict` passes. Tests: given a CLI binary path under `$HOME`, the sandbox plan binds it (and its symlink target) read-only/executable; the rest of `$HOME` is not bound; a system-path CLI also resolves and is included.
