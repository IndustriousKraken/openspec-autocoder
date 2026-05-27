## 1. `update.sh` bootstrap

- [ ] 1.1 Create `update.sh` at the repository root, mode `0755`. Use the same opening conventions as `install.sh`: `set -euo pipefail`, `OWNER`/`REPO` constants, `STEP` variable + `trap` for failure reporting.
- [ ] 1.2 Argument parsing (`while (( $# )); case "$1" in ...`):
  - `--version <tag>` → opt in to a specific tag (including pre-releases).
  - `--dry-run` → set DRY_RUN=1; skip the swap + restart.
  - `--config-dir <path>` → override config-path resolution.
  - `--` → end of flags.
- [ ] 1.3 Function `detect_target_triple` (same as install.sh; copy verbatim — keep these in sync via a comment pointing at install.sh).

## 2. Version resolution

- [ ] 2.1 Function `current_version()`: runs `autocoder --version 2>/dev/null | head -n1 | awk '{print $NF}'` (or similar — the exact `--version` output format is the binary's contract). Strip leading `v` for comparison; preserve the original string for printing.
- [ ] 2.2 Function `target_version()`:
  - If `--version <tag>` was passed, echo that tag.
  - Else: `curl -fsSL "https://api.github.com/repos/${OWNER}/${REPO}/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1`.
- [ ] 2.3 If target == current, print `autocoder is already on <tag>; nothing to do` and exit 0.

## 3. Download + verify

- [ ] 3.1 Same `STEP="download"` + `STEP="verify"` blocks as `install.sh`: download `autocoder-<tag>-<triple>` and its `.sha256` companion to a `mktemp -d` directory; verify via `sha256sum -c` (or `shasum -a 256 -c` on macOS).
- [ ] 3.2 On checksum mismatch: print the mismatch detail AND the tempdir path (preserved for inspection), exit non-zero. Daemon is untouched at this point.

## 4. Config-path resolution + preflight

- [ ] 4.1 Function `resolve_config_path()`:
  - If `--config-dir <path>` was passed, echo `<path>/config.yaml`.
  - Else: run `systemctl show autocoder.service -p ExecStart 2>/dev/null` AND extract the `--config <path>` token from the output. Same parsing convention as `a01`'s probe.
  - If neither resolves to an existing file, bail with `update.sh: cannot find config; pass --config-dir <path> if your install is non-standard`.
- [ ] 4.2 Function `run_preflight(new_binary_path, config_path)`:
  - Run `<new_binary_path> check-config --config <config_path> --json` (the verb from `a03`).
  - Exit 0 (valid) → proceed.
  - Exit 1 (warnings only) → print the warnings to stdout, proceed.
  - Exit 2 (hard errors) → dump the JSON findings to stderr, print `update.sh: preflight failed; not swapping. Daemon continues on <current-version>.`, exit non-zero.
- [ ] 4.3 If `DRY_RUN=1`, after preflight prints `[dry-run] Would swap to <tag>` and exit 0.

## 5. Atomic swap

- [ ] 5.1 Resolve the installed binary path. Default: `/usr/local/bin/autocoder` (matches `install.sh`'s server-mode destination). Override via `AUTOCODER_BINARY_PATH` env var if set.
- [ ] 5.2 Function `swap_binary(new_path, current_path)`:
  - `mv "$current_path" "${current_path}.previous"` (overwrites any existing `.previous`).
  - `install -m 755 "$new_path" "$current_path"` (atomically, via mv-from-tmp).
  - The kernel handles the live daemon's open FD; nothing to do for the running process.
- [ ] 5.3 Use sudo when `$EUID != 0` AND `sudo` is on PATH (same pattern as install.sh).

## 6. Restart + verify

- [ ] 6.1 `sudo systemctl restart autocoder` (no-op the `sudo` when already root).
- [ ] 6.2 Function `wait_for_active(timeout_secs)`:
  - Loop with 1s sleeps up to `timeout_secs` (default 30).
  - Check `systemctl is-active autocoder`.
  - Return 0 on `active`, 1 on timeout.
- [ ] 6.3 On wait-for-active failure → call `swap_binary "${current_path}.previous" "${current_path}"` AND `sudo systemctl restart autocoder` AND exit non-zero with `update.sh: new binary failed to start; rolled back to <previous-version>. Check journalctl -u autocoder.`

## 7. Log + summary

- [ ] 7.1 On success: print AND `logger -t autocoder-update`-emit one line: `autocoder updated <current-version> → <target-version>`.
- [ ] 7.2 Exit 0.

## 8. Daemon startup notification

- [ ] 8.1 In `autocoder/src/cli/run.rs` (or wherever the daemon's polling-task bring-up happens), AFTER the chatops backend is constructed AND validated AND BEFORE the first polling iteration:
  - If a chatops backend is configured, dispatch one `post_notification` call:
    ```rust
    let version = env!("CARGO_PKG_VERSION");
    let msg = format!("🆙 autocoder v{} started — {} repository(ies) configured", version, repos.len());
    if let Some(chatops) = &chatops_backend {
        if let Err(e) = chatops.post_notification(&msg, &channel_for_default).await {
            tracing::warn!("startup version notification failed: {e}");
        }
    }
    ```
- [ ] 8.2 The notification fires exactly once per daemon startup, regardless of `chatops.notifications.*` flags (it's a lifecycle signal, not a per-change signal).
- [ ] 8.3 The notification fires only when a chatops backend is configured. Without one, the daemon emits an INFO log line (`startup version: vX.Y.Z; N repositories`) but no chatops post.
- [ ] 8.4 Unit test using `MockChatOpsBackend`:
  - Boot the daemon's bring-up function against a config with 3 repos and a mock chatops.
  - Assert one `post_notification` call with the expected message containing `vX.Y.Z` AND `3 repository(ies)`.
  - Boot the daemon against a config without chatops → assert no `post_notification` calls.

## 9. Docs

- [ ] 9.1 In `docs/DEPLOYMENT.md`, add a new section `Unattended updates via cron` between `Upgrading` and the trailing material:
  - Place `update.sh` somewhere the autocoder user can run it (e.g. `/home/autocoder/update.sh`).
  - Recommended crontab entry: `0 3 * * * /home/autocoder/update.sh >> /var/log/autocoder-update.log 2>&1`. Suggest jittering the minute if running across a fleet (`$((RANDOM % 60))`).
  - `--version <tag>` for explicit pinning (some operators freeze on a known-good release).
  - `--dry-run` for the first scheduled run (see the JSON preflight output without swapping).
  - Audience caveat: this is for homelab / indie / SBC deployments. Enterprise change-control environments should use Ansible / apt / their existing config management instead.
- [ ] 9.2 In `docs/CLI.md`, in the `## \`run\`` entry, add a sentence: `If a chatops backend is configured, the daemon posts a one-line \`🆙 autocoder vX.Y.Z started\` notification on every successful startup. Operators tracking unattended-update transitions watch this line in chat.`
- [ ] 9.3 Resolve the dead anchor from `a01`: the `Unattended updates via cron` section that `a01`'s DEPLOYMENT.md addition cross-linked to now exists. Verify the link resolves.

## 10. Spec deltas

- [ ] 10.1 `openspec/changes/a04-update-script-and-version-notification/specs/orchestrator-cli/spec.md` ADDs one requirement covering the startup version notification (message format, unconditional fire on startup, suppressed without chatops, NOT gated by `notifications.*` flags).
- [ ] 10.2 `openspec/changes/a04-update-script-and-version-notification/specs/chatops-manager/spec.md` ADDs one requirement noting `daemon_started` as a notification category in the existing notification-type family (or, if there's an enum that needs MODIFYING to admit it, MODIFIES that enum requirement instead).
- [ ] 10.3 `openspec/changes/a04-update-script-and-version-notification/specs/project-documentation/spec.md` ADDs TWO requirements: `update.sh is a thin bash bootstrap script for binary upgrades` (parallel to the existing `install.sh` requirement; ≤ 150 lines, no business logic, delegates preflight to the binary via `check-config`) AND `DEPLOYMENT.md documents unattended updates via cron`.

## 11. Verification

- [ ] 11.1 `cargo test` passes (new + existing).
- [ ] 11.2 `openspec validate a04-update-script-and-version-notification --strict` passes.
- [ ] 11.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
- [ ] 11.4 `bash -n update.sh` passes (syntax check). `shellcheck update.sh` produces no errors (warnings about `local` in non-functions are acceptable if any).
