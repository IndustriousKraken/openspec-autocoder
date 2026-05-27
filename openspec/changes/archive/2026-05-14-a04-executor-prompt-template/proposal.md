## Why

Two related defects observed in production:

1. The implementer prompt sent to Claude is just the openspec change content with no project context or role-establishing imperative. Claude responds with chat-style clarification ("let me know what you'd like to do — implement, review, validate, or something else") instead of doing implementation work.
2. `build_prompt` has a silent raw-markdown fallback that fires when `openspec instructions apply` fails. The fallback returns a prompt with no instruction at all. Combined with #1, this means a misconfigured deployment (openspec missing or unreachable) loops forever producing nothing.

Both defects share a root cause: the implementer prompt is half-formed and assembled inline in the code, with degenerate failure modes baked in.

## What Changes

- **REMOVED capability bit:** `executor::build_prompt`'s raw-markdown fallback. There is no longer a degraded path. If `openspec instructions apply` cannot produce a prompt, the iteration fails with a clear error and the change re-enters pending.
- **ADDED capability bit:** the executor loads an implementer prompt template at startup from `prompts/implementer.md` (or the path named in `executor.implementer_prompt_path` if set). The template wraps the openspec output with a role + imperative. A built-in default is compiled into the binary via `include_str!` so a fresh install works without committing the file to the deploy host.
- **ADDED capability bit:** startup preflight. Before the polling loop starts, the daemon runs `openspec --version` once. If it fails, the daemon exits non-zero with a message naming the underlying failure. A misconfigured deployment fails loudly at startup instead of looping silently.
- **Code:**
  - `prompts/implementer.md` is added with the role-and-imperative wrapper text (see template body in tasks.md task 1.1).
  - `claude_cli::build_prompt` is rewritten to: (a) call `openspec instructions apply`, (b) return Err if it fails or returns empty stdout, (c) substitute `{{change_body}}` into the template, (d) return the wrapped result. No fallback.
  - `config::ExecutorConfig` gains `pub implementer_prompt_path: Option<PathBuf>`.
  - `ClaudeCliExecutor` gains a `template: String` field, populated at construction from the config-overridden path or the embedded default.
  - `cli::run` gains an `openspec_preflight` step before `start_polling`: runs `Command::new("openspec").arg("--version")`, errors with a clear message naming PATH if NotFound or non-zero.
- **Tests:**
  - `build_prompt_errors_when_openspec_fails` — fixture workspace without a usable openspec setup; assert Err with a non-empty message.
  - `build_prompt_substitutes_change_body_into_template` — fixture executor with a known template containing `{{change_body}}`; assert the rendered prompt contains both the wrapper text and the openspec body.
  - `preflight_fails_when_openspec_not_on_path` — temporarily clear PATH (per-test, single-threaded subset), call the preflight, assert Err with `openspec` mentioned.
- **README:**
  - Replace the silent-fallback narrative (where it leaked into the README) with a description of the required `prompts/implementer.md` file and the preflight check.
  - The deploy guide now lists `cp -r prompts/ /home/autocoder/autocoder/` as a required step alongside `config.yaml`.

## Impact

- Affected specs: `executor` (build_prompt scenario rewritten), `orchestrator-cli` (preflight scenario added).
- Affected code: `autocoder/src/executor/claude_cli.rs`, `autocoder/src/config.rs`, `autocoder/src/cli/run.rs`.
- New required asset: `prompts/implementer.md` (default template, ships in repo, embedded in binary).
- Operator action required on upgrade: ensure `prompts/implementer.md` is alongside `config.yaml`, or omit the optional `executor.implementer_prompt_path` to use the embedded default.
- Breaking? Yes if a deployment was relying on the silent fallback to "work" without openspec — those deployments were already producing nothing useful, so the breakage is reclassifying noise as a clear startup error.
