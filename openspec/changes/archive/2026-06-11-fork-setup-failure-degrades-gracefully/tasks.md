# Tasks

## 1. Per-repo skip instead of aggregate-and-exit

- [x] 1.1 In the startup fork-setup routine (`cli/run.rs`, the path that today records per-repo failures then exits non-zero), change the failure handling: on a per-repo fork-setup failure (creation non-2xx OR fork not reachable within the timeout), do NOT exit. Skip that repository for the process lifetime (no polling task spawned), reusing the existing skip-for-lifetime mechanism that fork-URL-derivation failures already use.
- [x] 1.2 Continue processing the remaining repositories; spawn polling tasks for every repository whose fork is reachable. The daemon SHALL NOT exit non-zero for a per-repo fork-setup failure, even when all repositories fail.

## 2. Chatops alert on fork-setup failure

- [x] 2.1 Emit a chatops alert through the existing outbound notification path for each repository whose fork setup failed. The alert identifies the repository AND carries a brief remedy hint (e.g. ensure the fork exists/reachable, then restart or reload).
- [x] 2.2 Ensure the chatops outbound backend is initialized BEFORE per-repo fork setup runs so the alert is deliverable; reorder the startup sequence if fork setup currently precedes chatops init.

## 3. Tests

- [x] 3.1 One repo's fork setup fails AND another's succeeds → the reachable repo's polling task is spawned, the failed repo is skipped, a chatops alert fires for the failed repo, AND the routine does NOT exit / return a fatal error.
- [x] 3.2 Every repo's fork setup fails → the daemon still starts (no non-zero exit), one alert per failed repo.
- [x] 3.3 Happy paths unchanged: all forks already exist (no creation, all spawn); a fork is created and becomes reachable (spawns normally).

## 4. Documentation

- [x] 4.1 Update the fork-PR-mode docs (`docs/SECURITY.md` fork-and-PR section and/or `docs/OPERATIONS.md`) to state that a per-repo fork-setup failure degrades to a chatops alert + skipped repo (the daemon and other repos keep running), and name the recovery (fix the fork, then restart or reload).

## 5. Acceptance

- [x] 5.1 `cargo test` passes.
- [x] 5.2 `openspec validate fork-setup-failure-degrades-gracefully --strict` passes.
