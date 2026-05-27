## Why

Production diagnosis 2026-05-14: Claude returned a clarification ("you've shared the proposal, design, and tasks but haven't said what you'd like me to do") instead of implementing the change. Inspection of the run-log (added by `executor-output-visibility`) showed Claude received markdown without an imperative, which matches `build_prompt`'s silent fallback path at `claude_cli.rs:99-113` — but no log line said the fallback fired. Three failure modes all reach the fallback with no diagnostic:

- `openspec` binary not on autocoder's PATH (`Command::new("openspec")` returns `NotFound` Err)
- `openspec instructions apply` exits non-zero (e.g. workspace missing `openspec/` dir, change name typo)
- `openspec` exits 0 but stdout is empty

In every case the operator has no way to know they're hitting the fallback, and the fallback prompt contains no instruction telling Claude to implement — Claude reasonably asks "what do you want?"

Additionally: the systemd PATH gotcha that triggered this in production is not documented in the README's Deployment section.

## What Changes

- **MODIFIED capability:** `executor` — `build_prompt` SHALL log a WARN naming the reason whenever it falls back to raw-markdown concatenation, and the prompt (whether from openspec or fallback) SHALL be persisted to the per-change run-log alongside stdout/stderr.
- **Code:**
  - `claude_cli::build_prompt` is restructured to inspect each `Command` result, log a structured WARN naming which branch fired the fallback (`openspec_not_found`, `openspec_exited_nonzero`, `openspec_empty_stdout`), and return the fallback prompt.
  - `persist_run_log` is extended to also write the prompt (passed in by `run`/`resume`). New format: `=== PROMPT (n bytes) ===\n...\n=== STDOUT ...\n=== STDERR ...\n`.
- **Tests:**
  - `build_prompt_logs_warn_when_openspec_missing` — set PATH to exclude openspec, assert WARN fires with `reason=openspec_not_found`.
  - `build_prompt_logs_warn_when_openspec_exits_nonzero` — fake openspec script that exits 1, assert WARN fires with `reason=openspec_exited_nonzero`.
  - `run_log_contains_prompt_section` — fixture run, assert the persisted log file contains `=== PROMPT (` and the actual prompt text.
- **README:** Deployment section gains an explicit `Environment="PATH=..."` example in the systemd unit snippet, plus a note that the PATH must cover `openspec` and `claude` (and that npm-installed openspec usually lands in `/usr/bin/openspec` or `/usr/local/bin/openspec`).

## Impact

- Affected specs: `executor` (one scenario modified)
- Affected code: `autocoder/src/executor/claude_cli.rs`
- Affected docs: `README.md` (Deployment §3 systemd unit)
- No new dependencies, no config knobs added
