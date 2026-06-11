## Why

A fork-setup failure for a single repository currently aborts the **entire** daemon at startup. When `github.fork_owner` is set, the fork-existence verification creates/probes each repo's fork and, on any failure (creation non-2xx, or the fork not reachable within the 60-second timeout), aggregates the failures into one startup error and exits non-zero — before any polling task is spawned. Under systemd that is a restart loop: one repository whose fork is missing, unreachable, or misnamed crash-loops the whole service, taking down every **other** repository AND chatops, with no operator-visible signal beyond the journal.

This is easy to trigger operationally — e.g. an upstream repository rename leaves the derived fork name pointing at a fork the account holds under a different name, so the probe reports "fork creation succeeded but the fork was not reachable within 60s" and the daemon exits(1) and loops, with no startup notification and no response to chatops. One misconfigured repository should not take down the fleet.

## What Changes

A per-repo fork-setup failure no longer aborts startup. The daemon records the failure, **skips that repository for the process lifetime** (no polling task spawned for it) — the same per-repo skip already used when a fork URL cannot be derived — emits a **chatops alert** naming the repository and a brief remedy, and continues setting up AND serving every other repository and chatops. The daemon never exits non-zero for a per-repo fork-setup failure; even if every configured repository fails fork setup, it stays up so an operator can remediate and recover (fix the fork, then restart or reload).

## Impact

- **Affected specs:** `orchestrator-cli` — MODIFY `Startup verification of fork existence` (failure handling: skip + alert + continue instead of aggregate-and-exit).
- **Affected code:** the startup fork-setup routine in `cli/run.rs` (the aggregate-and-exit-non-zero path) — replace with per-repo skip + chatops alert + continue, reusing the existing skip-for-lifetime mechanism that fork-URL-derivation failures already use. Ensure the chatops outbound backend is initialized before per-repo fork setup runs so the alert is deliverable (reorder startup if needed).
- **Operator-visible behavior:** a repository with a broken fork produces a chatops alert and is skipped; the daemon and all other repositories and chatops keep running. Recovery: fix the fork, then restart (or `reload`).
- **Non-goals:**
  - NOT changing how the fork name is derived. autocoder assumes the fork is named after the upstream; the renamed-upstream case (GitHub allows one fork per network and returns the existing, differently-named fork) is left for a follow-up that uses the fork name returned by the fork API. This change makes that case a graceful skip+alert rather than a crash.
  - NOT changing the 60-second reachability timeout (graceful degradation turns a slow fork into a skip+alert, not a crash).
  - NOT changing mid-iteration error tolerance (already covered by `Iteration-level error tolerance`).
- **Dependencies:** builds on `Startup verification of fork existence`, `github.fork_owner opt-in to fork-PR mode`, and the existing chatops notification path. No unmerged dependencies.
- **Acceptance:** `cargo test` passes; `openspec validate fork-setup-failure-degrades-gracefully --strict` passes. Tests: a fork-setup failure for one repo skips it, emits an alert, and does NOT exit while reachable repos spawn; all-repos-fail still starts and stays up; the happy paths (all forks already exist / fork created and reachable) are unchanged.
