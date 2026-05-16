## Why

The current perma-stuck chatops alert names the repo, change, count, last reason, and the path to the marker file the operator must remove. It does NOT name the per-change run log path where the executor's stdout/stderr were captured. An operator landing on the alert has to know the path convention (`/tmp/autocoder/logs/<workspace-basename>/<change>.log`) to find the diagnostic data, or read the source to discover it.

Adding the log path to the alert body closes the diagnostic loop: alert → log path → root cause, with no implicit knowledge required.

## What Changes

- **ADDED capability:** `orchestrator-cli` gains a "Perma-stuck chatops alert content" requirement specifying the alert's structure and fields, including the per-change run-log path. (The perma-stuck behavior was previously described inline in the original `perma-stuck-change-detection` change but never landed as a standalone requirement in the canonical spec; this change adds it explicitly.)
- **Code:** `post_perma_stuck_alert` in `polling_loop.rs` is extended to include a `run_log: <path>` line in the message body. The path is computed via the existing `executor::claude_cli::run_log_path(workspace, change)` helper.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/polling_loop.rs::post_perma_stuck_alert` (one additional formatted line in the message body).
- Operator-visible behavior: the chatops alert grows by one line. No other behavior changes.
- Breaking: no.
