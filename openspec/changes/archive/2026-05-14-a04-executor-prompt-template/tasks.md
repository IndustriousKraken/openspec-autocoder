## 1. Ship the implementer prompt template

- [x] 1.1 Create `prompts/implementer.md` containing the role + imperative wrapper. Body (verbatim):

    ```
    You are an autonomous code-implementation agent running inside a CI-style
    pipeline. The repository at your current working directory is a checked-out
    clone of a Git project that uses OpenSpec for change management. You have
    been invoked to implement one specific OpenSpec change, described below.

    Your job:
    1. Read every context file referenced in the change.
    2. Write the code and tests needed to satisfy the spec.
    3. Use the available tools (Read, Write, Edit, Glob, Grep, Bash) freely.
    4. Do not ask the operator for clarification. Make reasonable decisions
       and proceed. If a decision is genuinely irrecoverable, use the
       `ask_user` MCP tool (available in this session) to escalate.
    5. Do not archive the change yourself; `openspec archive` is denied in
       this sandbox. Leave the working tree dirty — autocoder will commit
       your diff and archive on success.
    6. Mark tasks in tasks.md as you complete them (`- [ ]` → `- [x]`).

    Begin implementation now.

    --- BEGIN CHANGE ---

    {{change_body}}

    --- END CHANGE ---
    ```
- [x] 1.2 In `claude_cli.rs`, add `const DEFAULT_IMPLEMENTER_TEMPLATE: &str = include_str!("../../../prompts/implementer.md");` (mirror the path style used by `code_reviewer::DEFAULT_TEMPLATE`).

## 2. Config: optional template path override

- [x] 2.1 In `config::ExecutorConfig` (raw form), add `pub implementer_prompt_path: Option<PathBuf>` with `#[serde(default)]`.
- [x] 2.2 In `config::ResolvedExecutor` (or equivalent post-resolution struct), surface an owned `template: String` field populated from the override path (read at config-resolution time with anyhow `Context` naming the file) or `DEFAULT_IMPLEMENTER_TEMPLATE.to_string()` when unset.
- [x] 2.3 If the override file is unreadable or empty, propagate the error so `cli::run` aborts at config-load time with a clear message — do not silently fall back to the default.
- [x] 2.4 **Verify:** add tests `config::tests::implementer_prompt_default_template_when_unset`, `config::tests::implementer_prompt_loads_override_file`, `config::tests::implementer_prompt_errors_when_override_file_missing`.

## 3. ClaudeCliExecutor: hold and substitute template

- [x] 3.1 Add `template: String` to `ClaudeCliExecutor`. Update `new` / `with_args` (and any test constructors) to accept the template. Internal default for ad-hoc construction stays the embedded constant.
- [x] 3.2 Rewrite `ClaudeCliExecutor::build_prompt` to:
    1. Run `openspec instructions apply --change <change>` in the workspace.
    2. If spawn fails (`ErrorKind::NotFound`), return `Err(anyhow!(...))` naming `openspec_not_found` and instructing the operator to fix PATH.
    3. If exit is non-zero, return Err naming `openspec_exited_nonzero` with exit code + stderr tail.
    4. If stdout is empty (after trim), return Err naming `openspec_empty_stdout`.
    5. Otherwise: return `self.template.replace("{{change_body}}", &stdout)`.
- [x] 3.3 Update `run` and `resume` to handle the new `build_prompt` error: a build_prompt error propagates up to `polling_loop`, which treats it as a fatal iteration error (the existing `Err` branch in `walk_queue` already logs and breaks). The change is unlocked by the existing `walk_queue` unlock logic.
- [x] 3.4 Remove `tracing::warn!` calls in `build_prompt` (they are superseded by the new Err return — the error is already logged at the call site).
- [x] 3.5 The resume-path "Earlier you asked... continue" preamble stays. It is concatenated before the templated prompt so the substituted body still contains the openspec output.

## 4. cli/run: preflight `openspec --version`

- [x] 4.1 Add `fn openspec_preflight() -> Result<()>` in `cli/run.rs`. Implementation: `Command::new("openspec").arg("--version").output()`. Map `NotFound` and non-zero exits to anyhow errors with the spec-mandated messages.
- [x] 4.2 Call `openspec_preflight()` once at the top of `cli::run` (the daemon entry point), after `load_config` but before any repo polling task is spawned.
- [x] 4.3 **Verify:** `cli::run::tests::preflight_fails_when_openspec_missing` — wrap the preflight in a function callable from tests, pass it a constructed `Command` whose program is `openspec-definitely-not-installed`, assert Err with `openspec` mentioned and a clear next-step. (Acceptable alternative: invoke the preflight with a fake `OPENSPEC_BIN` env var if a small indirection makes the test cleaner — pick whichever fits the existing test idiom.)
- [x] 4.4 **Verify:** the preflight does NOT run when invoking subcommands other than `run` (e.g. `rewind`, `mcp-ask-user-server`) — only the polling daemon needs the check.

## 5. Documentation

- [x] 5.1 README's Quick Start §3 (Build the daemon): add `cp -r prompts/ ~/autocoder/` next to the existing `cp config.yaml ~/autocoder/config.yaml`. Note: the `prompts/` directory contains the default implementer template; the binary embeds this template at build time, but operators may override it by setting `executor.implementer_prompt_path` in `config.yaml`.
- [x] 5.2 README's Configuration Reference (`executor:` section): document the new optional `implementer_prompt_path` field.
- [x] 5.3 README's Operating Notes: brief paragraph naming the preflight check ("autocoder verifies `openspec --version` at startup and exits non-zero if the binary is not on PATH"). No kitschy framing.
- [x] 5.4 Remove any remaining references to the silent fallback path from README (the wording introduced by the prior `prompt-fallback-diagnostics` change becomes obsolete).

## 6. Verification

- [x] 6.1 `cargo test` passes; net new tests = at least 4 (3 config + 1 preflight + N build_prompt as appropriate).
- [x] 6.2 `cargo build --release` produces a binary that, when invoked without `openspec` on PATH, exits non-zero at startup with the spec-mandated message — no polling loop entered.
- [x] 6.3 `openspec validate executor-prompt-template --strict` passes.
