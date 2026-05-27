## Why

Operators on production deployments today upgrade autocoder by either re-running `install.sh` (destructive — see `a01`) or by manually downloading the binary + checksum, verifying, swapping, and `systemctl restart`-ing. The manual sequence is correct but tedious; releases ship multiple times a week. For the natural autocoder audience — single-host SBC, indie VPS, homelab deployments — set-and-forget binary updates are a meaningful improvement.

`install.sh` is the wrong tool for this. Its job is first-time setup and the existing-install detection from `a01` makes re-running it a no-op anyway. A dedicated `update.sh` script with cron-friendly semantics is the right shape: it knows it's an update, has no interactive prompts, defaults to the latest non-prerelease tag, and runs the `check-config` preflight from `a03` against the downloaded binary BEFORE swapping — so a broken release does not put the daemon into a restart loop overnight.

Operationally, even after `update.sh` lands, operators need visibility into "what version is the daemon currently running?" without SSH-ing in. The daemon emits a one-time `🆙 autocoder vX.Y.Z started` chatops notification on every startup. With cron-driven updates plus this notification, the channel becomes the operator's record of when the daemon updated AND to what version.

## What Changes

**New `update.sh` script at repo root.** Parallel to `install.sh`. Bash, target ≤ 150 lines (more than install.sh's 80 because it does atomic swap + rollback + restart verification). Responsibilities:

1. **Resolve current version.** Run the installed `autocoder --version`; parse the tag.
2. **Resolve target version.** Default: query `GET /repos/<owner>/<repo>/releases/latest` (which by GitHub's contract returns only the most recent non-prerelease tag). With `--version <tag>` flag: use that exact tag, allowing pre-release tags by explicit opt-in.
3. **No-op early exit.** If current == target, print `autocoder is already on <tag>; nothing to do` and exit 0. Cron-safe.
4. **Download + checksum.** Same logic as `install.sh`'s `download` + `verify` steps. Bail on checksum mismatch.
5. **Preflight: validate config against the new binary.** Run `<downloaded-binary> check-config --config <config-path> --json`. The config path is resolved the same way `a01`'s systemd probe resolved it: read `--config` from `systemctl show autocoder.service`'s `ExecStart`. If preflight exits 2 (hard errors), bail with the JSON findings dumped to stderr — the daemon is left running on the old binary. Exit 1 (warnings) is acceptable; proceed.
6. **Atomic swap.** Move the current `/usr/local/bin/autocoder` to `/usr/local/bin/autocoder.previous` (overwriting any existing `.previous`). `install -m 755` the new binary in place. The kernel decouples the running daemon's inode from the path, so the live daemon is unaffected.
7. **Restart + verify.** `systemctl restart autocoder` (with sudo where needed). Wait up to 30 seconds polling `systemctl is-active autocoder`. If active: continue to step 8. If not active by the deadline: swap `autocoder.previous` back over `autocoder`, `systemctl restart autocoder` again, exit non-zero with a "rollback applied; check journalctl" diagnostic.
8. **Log + summary.** Emit one journalctl-bound INFO line via `logger -t autocoder-update` naming the version transition. Print the same line to stdout.

**Defaults summary:**

- Default action: `update.sh` with no args fetches the latest non-prerelease.
- `update.sh --version <tag>`: opt in to a specific tag, including pre-releases (e.g. `v2.0.0-rc1`).
- `update.sh --dry-run`: resolve target, download, checksum, run preflight, report what would happen — without swapping. Useful for the first cron entry on a new host.
- `update.sh --config-dir <path>`: override the config-path resolution (mirrors `install.sh --config-dir`). Mainly for test rigs and unusual deployments.

**Daemon emits a startup version notification.** In `autocoder run`'s initialization (after configs validate AND the chatops backend is constructed), the daemon SHALL post one notification:

```
🆙 autocoder vX.Y.Z started — <N> repository(ies) configured
```

The notification fires on every startup, not just after an `update.sh`-driven restart — every restart is a relevant operator signal. The notification is NOT gated by `chatops.notifications.start_work` (the existing per-change pickup flag); it's a daemon-lifecycle signal, not a per-change signal. It IS suppressed when no chatops backend is configured.

**No new chatops `notifications` config flag.** The startup notification fires unconditionally when chatops is configured — same shape as the daemon's existing audit-finding posts and proposal-created notifications. An operator who really doesn't want the line can comment out the daemon-side code path or, more usefully, file a `notifications.daemon_started: false` ask if it becomes annoying.

**`docs/DEPLOYMENT.md` gains an "Unattended updates via cron" section.** Describes the cron-driven workflow: stage `update.sh` in `/home/autocoder/`, set a crontab entry running it at a low-traffic time with stagger, point at the recommended-for-cron `--version` policy (default latest non-prerelease; manual override per release if the operator wants a known-good freeze). The section also documents the audience boundary explicitly: this is for homelab / indie / SBC deployments, NOT for enterprise change-control environments where Ansible / apt own upgrade orchestration.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one ADDED requirement: `Daemon emits a startup version notification on every successful boot`. Documents the message format, the unconditional fire on every startup, the no-chatops-backend suppress, and the absence of a `notifications.daemon_started` gate.
  - `chatops-manager` — one MODIFIED requirement updating the existing notification-type enumeration to include `daemon_started` as a new category (the type is operator-facing; consistency with the `🚀` / `⚠️` / `🔍` etc. emoji family).
  - `project-documentation` — TWO ADDED requirements: `update.sh is a thin bash bootstrap script for binary upgrades` (parallel to the existing `install.sh` requirement) AND `DEPLOYMENT.md documents unattended updates via cron`.
- **Affected code:**
  - `update.sh` (new, at repo root) — the bash script per the spec above. Same structure as `install.sh`'s download + verify, plus preflight + swap + restart + rollback.
  - `autocoder/src/cli/run.rs` (or wherever the daemon's chatops bring-up sequence lives): after the chatops backend is constructed AND validated, dispatch one `🆙` post via the existing `ChatOpsBackend::post_notification` (or whichever method handles non-threaded operator notifications). The message reads `🆙 autocoder vX.Y.Z started — <N> repository(ies) configured`. The version string is `env!("CARGO_PKG_VERSION")` from build time, optionally prefixed with `v` to match release tag conventions.
  - `docs/DEPLOYMENT.md` — new section `Unattended updates via cron` between `Upgrading` and the existing closing material. Documents the script invocation, the crontab entry shape (with stagger guidance), the `--version` opt-out, and the audience caveat.
  - `docs/CLI.md` — no new entry (the daemon-side notification is not a verb), but the existing `## \`run\`` entry gains a sentence noting that the daemon emits a startup notification to chatops if configured.
- **Operator-visible behavior:**
  - `update.sh` is the canonical update path. Operators using it never need to re-run `install.sh` after first-time setup.
  - Cron entries like `0 3 * * * /home/autocoder/update.sh >> /var/log/autocoder-update.log 2>&1` reliably keep the daemon current without intervention.
  - Every daemon restart posts a `🆙 autocoder vX.Y.Z started ...` line to the configured chatops channel — operators see the version in chat AND can confirm an update landed.
- **Breaking:** no for end users; the existing manual-upgrade flow (`cargo build --release && cp && systemctl restart`) continues to work. The new chatops notification fires only when a backend is configured — operators on the no-chatops deployment are unaffected.
- **Acceptance:** `cargo test` passes; `openspec validate a04-update-script-and-version-notification --strict` passes. `update.sh --dry-run` against a fixture host (no actual binary swap) reports the version transition AND the preflight result. A new unit test covers the daemon's startup-notification dispatch via the existing `MockChatOpsBackend`.
