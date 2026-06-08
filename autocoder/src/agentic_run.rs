//! Shared agentic-run primitive (a56).
//!
//! "Wrap a CLI as a subprocess, hand it a prompt, run an agentic session
//! to completion" was implemented five+ times (the executor's
//! `run_subprocess` plus four near-identical audit copies). This module
//! is the single source of truth for that pattern: [`agentic_run`] spawns
//! the child in its own process group, pipes the prompt on stdin, enforces
//! a timeout via the select-and-kill pattern, and returns a unified
//! [`AgenticRunOutcome`]. Streaming-JSON event parsing (`final_answer`,
//! `session_id`, incremental structured log) runs ONLY in
//! [`OutputMode::Streaming`]; [`OutputMode::Capture`] reads stdout/stderr
//! at exit.
//!
//! CLI selection is abstracted behind the [`CliStrategy`] trait so a
//! model's provider can pick the `claude` CLI or the provider-agnostic
//! `opencode` CLI without role code changing. Three strategies are
//! registered: [`ClaudeStrategy`] (Anthropic-shaped, streaming-capable),
//! [`OpencodeStrategy`] (a60 — any OpenAI-compatible / Ollama endpoint,
//! capture-mode only), AND [`AntigravityStrategy`] (a69 — Google's `agy`
//! CLI for Gemini-family models, capture-mode only). A provider that
//! resolves to any other CLI returns a clear "strategy not yet
//! implemented" error ([`strategy_for_provider`]).
//!
//! The refactor is behavior-neutral: the executor keeps streaming-JSON +
//! MCP + the recovery/session-reuse path; each audit keeps simple-capture
//! + no-MCP + its read-only tool list + its ETXTBSY retry.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::executor::event_log::{self, ActionKind, StructuredLogWriter};
use crate::executor::json_event::{self, AssistantBlock, JsonEvent, UserBlock};

/// Unified outcome returned by [`agentic_run`]. Replaces the per-module
/// `SubprocessOutcome` structs the executor and the audits each declared.
///
/// `final_answer` / `session_id` are populated only by the streaming-JSON
/// path (the executor); `streamed_log` is `true` when that path wrote the
/// structured log incrementally (so the legacy `persist_run_log` writer
/// should skip it).
#[derive(Default)]
pub struct AgenticRunOutcome {
    pub timed_out: bool,
    pub exit_status: Option<std::process::ExitStatus>,
    pub stdout: String,
    pub stderr: String,
    /// Agent's conversational summary from the `result` event. `None` in
    /// capture mode AND when a streaming run timed out before the result
    /// event arrived.
    pub final_answer: Option<String>,
    /// Session id captured from the `system`-event init subtype. `None`
    /// in capture mode OR when the system event was absent.
    pub session_id: Option<String>,
    /// `true` when the streaming path built the structured log itself.
    pub streamed_log: bool,
    /// a70: the strategy-agnostic handle for the session this run created,
    /// resolved by [`agentic_run_with_session`]. For a streaming `claude`
    /// run this is the streamed `session_id`; for a capture-mode run it is
    /// the new entry that appeared in the strategy's session-store directory.
    /// `None` when session management was not requested OR no new session
    /// could be attributed. Used by the cleanup (prune) AND, for the
    /// implementer, the AskUser resume step.
    pub session_handle: Option<String>,
}

/// Output handling for a run. `Streaming` adds `--verbose --output-format
/// stream-json`, parses each event, and writes the structured log
/// incrementally. `Capture` reads stdout/stderr at exit with no parsing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputMode {
    Streaming,
    Capture,
}

/// A resolved `(provider, model, api_base_url, api_key)` tuple a strategy
/// translates into that CLI's model-selection mechanism. Constructed from
/// the model registry's resolution of a role's model (a55); `None` at a
/// call site preserves the CLI's own default-model behavior.
pub struct ResolvedModel {
    /// The model's provider. The `claude` strategy carries it only for
    /// dispatch (its `apply_model_selection` keys off env, not provider);
    /// the `opencode` strategy reads it to build `--model <provider>/<model>`
    /// AND the `opencode.json` provider id.
    pub provider: crate::config::LlmProvider,
    pub model: String,
    pub api_base_url: String,
    /// The resolved LLM credential. When EMPTY (the default), no `CliStrategy`
    /// places a credential in the subprocess — the CLI authenticates from its
    /// own login/store (the safe, no-exposure default). When NON-EMPTY, the
    /// strategy passes it to the wrapped CLI so the CLI uses that key: `claude`
    /// via `ANTHROPIC_API_KEY`, `opencode` via an `{env:...}` reference in
    /// `opencode.json` resolved from the subprocess env, `agy` via `AV_API_KEY`.
    /// A supplied key is never written raw into a workspace file, but it does
    /// reach the subprocess env, where the same-uid model can read it — an
    /// opt-in exposure (see [`cli_role_key_exposure_warning`]). The key is also
    /// consumed by autocoder's in-process HTTP clients (the `oneshot` reviewer's
    /// `LlmClient`), which resolve it directly and spawn no subprocess.
    pub api_key: String,
}

/// Sandbox configuration for a run: the allowed-tools list, the disallowed
/// bash/read patterns, AND whether `Write`/`Edit` are denied in the
/// settings file. The executor allows writes (`deny_writes: false`); the
/// read-only audits deny them (`deny_writes: true`).
pub struct SandboxConfig {
    pub allowed_tools: Vec<String>,
    pub disallowed_bash_patterns: Vec<String>,
    pub disallowed_read_paths: Vec<String>,
    pub deny_writes: bool,
}

/// Context a [`CliStrategy`] reads when building the invocation. The
/// claude-format settings file has already been written by [`agentic_run`];
/// the `claude` strategy references it and only assembles argv. The
/// `opencode` strategy ignores `settings_path` (opencode uses its own
/// `opencode.json` permission config, which it writes from this context).
pub struct BuildContext<'a> {
    pub settings_path: &'a Path,
    pub allowed_tools: &'a [String],
    /// Append the autocoder MCP provided-tool names to `--allowedTools`
    /// (the executor's main path does this so the agent may call the
    /// `ask_user` / `outcome_*` / `query_canonical_specs` MCP tools).
    pub include_autocoder_tools: bool,
    /// Emit `--verbose --output-format stream-json` on the command.
    pub emit_stream_json: bool,
    /// `--resume <session_id>` for the recovery turn (claude only).
    pub resume_session_id: Option<&'a str>,
    /// The run's workspace. The `opencode` strategy writes `opencode.json`
    /// here (MCP block + provider config + permissions); the `claude`
    /// strategy does not read it (its caller writes `.mcp.json`).
    pub workspace: &'a Path,
    /// The MCP role this run serves (a56): the value written as
    /// `ORCH_MCP_ROLE` (and the submission-store key) into the `opencode`
    /// strategy's `opencode.json` `mcp` block so the role's `submit_*` tool
    /// is reachable. `None` → no submission tool is advertised. The
    /// `claude` strategy ignores it (its caller writes the MCP env via
    /// `write_mcp_config`).
    pub mcp_role: Option<&'a str>,
    /// The resolved model, so the `opencode` strategy can write the provider
    /// config (model + base URL, plus an `{env:...}` apiKey REFERENCE when a key
    /// is supplied — never the raw secret) into `opencode.json`. `None` preserves
    /// the CLI's own default-model behavior. The `claude` strategy ignores it
    /// here (it sets `ANTHROPIC_BASE_URL` / `ANTHROPIC_MODEL`, and
    /// `ANTHROPIC_API_KEY` when a key is supplied, in `apply_model_selection`).
    pub model: Option<&'a ResolvedModel>,
}

/// Roots a [`CliStrategy`]'s session capture / prune reads (a70). `home` is
/// the CLI's home directory — the real `$HOME` in production; a temp dir in
/// tests so the prune is exercised without touching the operator's store.
/// `workspace` is the run's working directory (the `claude` strategy keys its
/// per-project session store by a path hash of this).
#[derive(Clone, Copy)]
pub struct SessionStoreCtx<'a> {
    pub home: &'a Path,
    pub workspace: &'a Path,
}

/// Abstracts CLI invocation so a model's provider can determine the CLI
/// without role code changing. Two jobs: build the invocation (binary,
/// flags, allowed-tools/settings format) AND translate a [`ResolvedModel`]
/// into the CLI's model-selection mechanism.
///
/// a70 adds two further jobs behind defaulted methods so existing strategies
/// opt in incrementally: a native headless-resume mechanism
/// ([`Self::apply_resume`]) AND a surgical session-delete
/// ([`Self::delete_session`], scoped via [`Self::session_store_dir`]).
pub trait CliStrategy: Send + Sync {
    fn build_command(&self, ctx: &BuildContext<'_>) -> Command;
    fn apply_model_selection(&self, cmd: &mut Command, model: Option<&ResolvedModel>);

    /// Apply this CLI's native headless-resume mechanism to a freshly-built
    /// command so the next invocation continues the session named by
    /// `handle`, returning `true` (a70). A strategy with no headless resume
    /// returns `false` WITHOUT touching `cmd` — the caller then requeues the
    /// change rather than fresh-running (a70 §5.3: no stash-and-recombine
    /// fallback). Called from each strategy's `build_command` when
    /// [`BuildContext::resume_session_id`] is set, so the resume flag lands in
    /// the same place for every transport. Default: unsupported.
    fn apply_resume(&self, cmd: &mut Command, handle: &str) -> bool {
        let _ = (cmd, handle);
        false
    }

    /// The directory under `ctx.home` where this CLI persists a transcript
    /// per session for a run in `ctx.workspace`. `None` means the CLI's store
    /// layout is unknown to us, so capture AND prune become no-ops. Used to
    /// capture a freshly-created session handle (the entry that appears after
    /// a run — see [`agentic_run_with_session`]) AND to scope
    /// [`Self::delete_session`]. Default: `None`.
    fn session_store_dir(&self, ctx: SessionStoreCtx<'_>) -> Option<PathBuf> {
        let _ = ctx;
        None
    }

    /// Delete ONLY the session record named by `handle` from this CLI's store
    /// (a70). Surgical: it removes that one session's transcript (and any
    /// per-session sidecar the CLI keeps keyed by the SAME handle) and nothing
    /// else — never settings, memory/context files (`CLAUDE.md` / `GEMINI.md`
    /// / project memories), credentials, OR the generated MCP config. Returns
    /// `Ok(true)` when a record was removed, `Ok(false)` when none matched.
    /// Default: no-op (`Ok(false)`).
    fn delete_session(&self, ctx: SessionStoreCtx<'_>, handle: &str) -> Result<bool> {
        let _ = (ctx, handle);
        Ok(false)
    }
}

/// Encode an absolute workspace path the way the `claude` CLI names its
/// per-project session directory under `~/.claude/projects/`: every character
/// that is not ASCII-alphanumeric or `-` becomes `-` (so `/`, `.`, and `_`
/// all map to `-`). The integration spike confirmed this against a live store
/// (e.g. `/home/u/.cache/ws/github_com_x-y` →
/// `-home-u--cache-ws-github-com-x-y`).
pub(crate) fn claude_project_hash(workspace: &Path) -> String {
    workspace
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

/// Build the `--allowedTools` value Claude CLI expects. When
/// `include_autocoder_tools` is set, the autocoder MCP provided-tool names
/// (`mcp__ask_user__*`) are appended so the daemon's contract tools are
/// always allowed regardless of the operator's `allowed_tools` list.
pub(crate) fn build_allowed_tools_value(
    allowed: &[String],
    include_autocoder_tools: bool,
) -> String {
    let mut combined: Vec<String> = allowed.to_vec();
    if include_autocoder_tools {
        for tool in crate::mcp_askuser_server::PROVIDED_TOOL_NAMES {
            combined.push(crate::mcp_askuser_server::qualified_tool_name(tool));
        }
    }
    combined.join(",")
}

/// The `claude` CLI strategy. Reproduces the pre-refactor invocation
/// exactly: `--settings <file>`, `--allowedTools <combined>`,
/// `--permission-mode acceptEdits`, optional `--resume`, and — in
/// streaming mode — `--verbose --output-format stream-json`. Model
/// selection sets `ANTHROPIC_BASE_URL` / `ANTHROPIC_MODEL` ONLY when a
/// model is configured; with no model it sets neither (the executor's
/// current CLI-default behavior).
///
/// a003: model selection sets NO `ANTHROPIC_AUTH_TOKEN`. The resolved
/// `api_key` is a credential and never reaches the subprocess — claude
/// authenticates from its own login / credential store (`claude login`),
/// and the model is tunneled across that connection. An env-set auth token
/// would be readable from the agent's Bash AND (for Anthropic) would force
/// pay-per-token off the operator's subscription. `ANTHROPIC_BASE_URL` /
/// `ANTHROPIC_MODEL` are endpoint/model selection, NOT credentials, so they
/// remain.
pub struct ClaudeStrategy {
    pub command: String,
    pub args: Vec<String>,
}

impl ClaudeStrategy {
    pub fn new(command: String, args: Vec<String>) -> Self {
        Self { command, args }
    }
}

impl CliStrategy for ClaudeStrategy {
    fn build_command(&self, ctx: &BuildContext<'_>) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .arg("--settings")
            .arg(ctx.settings_path)
            .arg("--allowedTools")
            .arg(build_allowed_tools_value(
                ctx.allowed_tools,
                ctx.include_autocoder_tools,
            ))
            .arg("--permission-mode")
            .arg("acceptEdits");
        if let Some(sid) = ctx.resume_session_id {
            // a70: route resume through the trait method so every transport
            // injects its flag uniformly. For `claude` this is the same
            // `--resume <id>` in the same position as before (byte-identical).
            self.apply_resume(&mut cmd, sid);
        }
        if ctx.emit_stream_json {
            // `--verbose` is required by Claude CLI alongside `stream-json`
            // for non-interactive sessions; without it the CLI emits a
            // single result envelope rather than streaming events.
            cmd.arg("--verbose")
                .arg("--output-format")
                .arg("stream-json");
        }
        cmd
    }

    fn apply_model_selection(&self, cmd: &mut Command, model: Option<&ResolvedModel>) {
        if let Some(m) = model {
            cmd.env("ANTHROPIC_BASE_URL", &m.api_base_url);
            cmd.env("ANTHROPIC_MODEL", &m.model);
            // When the operator supplied an `api_key`, pass it so claude uses
            // THAT key. No key → set none: claude authenticates from its own
            // login/store, so no credential reaches the subprocess (the safe
            // default). claude has no key-via-config-file option, so a supplied
            // key rides `ANTHROPIC_API_KEY` in the subprocess env, where the
            // same-uid model can read it — a documented opt-in exposure.
            if !m.api_key.is_empty() {
                cmd.env("ANTHROPIC_API_KEY", &m.api_key);
            }
        }
        // model: None → set nothing; the CLI uses its own default model.
    }

    /// `claude --resume <session_id>` continues the conversation captured from
    /// the streamed `system`-init `session_id` (a70). Always supported.
    fn apply_resume(&self, cmd: &mut Command, handle: &str) -> bool {
        cmd.arg("--resume").arg(handle);
        true
    }

    /// `~/.claude/projects/<project-hash>/` holds one `<session_id>.jsonl`
    /// transcript per session (the store the upstream bug reports show growing
    /// unbounded). The project hash encodes the workspace path
    /// ([`claude_project_hash`]).
    fn session_store_dir(&self, ctx: SessionStoreCtx<'_>) -> Option<PathBuf> {
        Some(
            ctx.home
                .join(".claude")
                .join("projects")
                .join(claude_project_hash(ctx.workspace)),
        )
    }

    /// Remove ONLY `<store>/<handle>.jsonl` — the one session transcript,
    /// addressed by its `session_id`. Leaves `settings.json`, `.credentials.json`,
    /// `CLAUDE.md`, the generated `.mcp.json`, AND every other session intact.
    fn delete_session(&self, ctx: SessionStoreCtx<'_>, handle: &str) -> Result<bool> {
        // a70 hardening: a handle with a separator or `..` would let
        // `dir.join(..)` resolve OUTSIDE the store (Rust does not normalize
        // `..`); refuse it rather than traverse.
        if reject_unsafe_session_handle(handle) {
            return Ok(false);
        }
        let Some(dir) = self.session_store_dir(ctx) else {
            return Ok(false);
        };
        delete_session_file(&dir.join(format!("{handle}.jsonl")))
    }
}

/// Reject a session handle that could escape its store directory (a70
/// hardening). A real handle is a UUID (`claude`) or conversation id
/// (`antigravity`), OR a store-directory filename stem — none of which ever
/// contains a path separator or a `..` component. A handle emitted by a
/// compromised / malicious CLI that DID contain one could, via `Path::join`
/// (which does NOT normalize `..`) feeding `remove_file` / `remove_dir_all`,
/// redirect the surgical prune at a path OUTSIDE the store. Any handle with a
/// separator (`/` or `\`), a `..` component, an interior NUL, OR that is empty
/// (an empty handle joins to the store dir itself, turning the prune into a
/// directory wipe) is treated as unsafe so [`CliStrategy::delete_session`]
/// refuses it rather than traversing.
fn session_handle_is_safe(handle: &str) -> bool {
    !handle.is_empty()
        && !handle.contains('/')
        && !handle.contains('\\')
        && !handle.contains('\0')
        && !handle.contains("..")
}

/// Log + refuse an unsafe session handle (a70 hardening). Emits a warning so
/// the refusal is visible regardless of caller (the prune callers log `Ok`
/// outcomes at debug), then signals "nothing removed". Returns `true` when the
/// handle was rejected so the caller can early-return `Ok(false)`.
fn reject_unsafe_session_handle(handle: &str) -> bool {
    if session_handle_is_safe(handle) {
        return false;
    }
    tracing::warn!(
        session = %handle,
        "refusing to prune session: handle contains a path separator or `..` \
         (possible traversal) — skipping the surgical delete"
    );
    true
}

/// Remove a single session file if it exists, returning whether it was there.
/// A missing file is `Ok(false)` (idempotent prune); any other IO error
/// propagates so a real permission/disk problem is visible.
fn delete_session_file(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("pruning session record {}", path.display())),
    }
}

/// Filename of the opencode config the [`OpencodeStrategy`] writes into the
/// workspace. opencode auto-discovers `opencode.json` from the project root
/// (the run's working directory, set by [`agentic_run`]).
const OPENCODE_CONFIG_FILENAME: &str = "opencode.json";

/// Env var carrying a SUPPLIED provider key for the `opencode` strategy. The
/// workspace `opencode.json` references it as `{env:...}` (so the raw secret is
/// never written into that committed file); the strategy sets the variable to
/// the resolved key on the subprocess, and opencode interpolates it at run time.
const OPENCODE_PROVIDER_KEY_ENV: &str = "AUTOCODER_OPENCODE_API_KEY";

/// The `opencode` CLI strategy (a60). Builds `opencode run` invocations for
/// the provider-agnostic `opencode` CLI so a role whose model resolves to
/// `opencode` (a55's `provider → CLI` rule for `openai_compatible`/`ollama`,
/// OR an explicit `cli: opencode`) runs agentically instead of erroring.
///
/// Unlike [`ClaudeStrategy`], opencode carries everything in one workspace
/// config file, `opencode.json`: the MCP `mcp` block (`type: local`, the
/// MCP-child command, env including `ORCH_MCP_ROLE`), the resolved provider
/// config (model + base URL, NEVER the `api_key` — a003: a credential in a
/// workspace file could be committed, AND the model never needs it), AND a
/// `permission` block mapped from a56's sandbox.
/// [`OpencodeStrategy::build_command`] writes that file; model
/// selection is `--model <provider>/<model>` (NOT `ANTHROPIC_*` env). It
/// writes NO `.mcp.json` (the `claude` MCP format). The prompt is delivered
/// on stdin — [`agentic_run`] already pipes it, AND headless `opencode run`
/// reads its message from piped stdin — so `build_command` appends no
/// positional message (which would also risk `ARG_MAX` on large review
/// prompts; see the integration spike notes).
///
/// opencode is capture-mode only; the streaming-JSON event path
/// (`final_answer` / `session_id` / incremental log) stays claude-specific.
pub struct OpencodeStrategy {
    pub command: String,
    pub args: Vec<String>,
}

impl OpencodeStrategy {
    pub fn new(command: String, args: Vec<String>) -> Self {
        Self { command, args }
    }

    /// The MCP child's env map for `opencode.json` (`mcp.<server>.environment`).
    /// Mirrors `ClaudeCliExecutor::write_mcp_config`: always the workspace;
    /// the role (as both `ORCH_MCP_CHANGE` submission key AND `ORCH_MCP_ROLE`)
    /// when a role is set; the daemon control-socket vars when the parent
    /// process carries them (canonical_rag configured).
    fn mcp_environment(ctx: &BuildContext<'_>) -> serde_json::Value {
        let mut env = serde_json::Map::new();
        env.insert(
            crate::mcp_askuser_server::ENV_WORKSPACE.to_string(),
            serde_json::Value::String(ctx.workspace.to_string_lossy().into_owned()),
        );
        if let Some(role) = ctx.mcp_role {
            // For the submission roles the change name AND the role name are
            // the same value (the reviewer/contradiction call sites pass
            // their role as both); see `write_mcp_config`.
            env.insert(
                crate::mcp_askuser_server::ENV_CHANGE.to_string(),
                serde_json::Value::String(role.to_string()),
            );
            env.insert(
                crate::mcp_askuser_server::ENV_ROLE.to_string(),
                serde_json::Value::String(role.to_string()),
            );
        }
        if let Ok(socket) = std::env::var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET) {
            env.insert(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET.to_string(),
                serde_json::Value::String(socket),
            );
            let basename = std::env::var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME)
                .unwrap_or_else(|_| {
                    ctx.workspace
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown_workspace")
                        .to_string()
                });
            env.insert(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME.to_string(),
                serde_json::Value::String(basename),
            );
        }
        serde_json::Value::Object(env)
    }

    /// Map a56's allowed-tools list onto opencode's `permission` block. Each
    /// permission opencode gates is `"allow"` when the equivalent tool is in
    /// the allowed list, else `"deny"`. A read-only sandbox
    /// (`["Read","Glob","Grep"]`) therefore denies `edit` (file mutation —
    /// opencode's `edit` permission governs both its `write` and `edit`
    /// tools), `bash`, AND `webfetch`. The always-available read tools
    /// (read/grep/glob) are not permission-gated; the role's `submit_*` tool
    /// is exposed via the `mcp` block.
    fn permission_block(allowed_tools: &[String]) -> serde_json::Value {
        let allows = |name: &str| allowed_tools.iter().any(|t| t.eq_ignore_ascii_case(name));
        let verdict = |allowed: bool| if allowed { "allow" } else { "deny" };
        serde_json::json!({
            "edit": verdict(allows("Edit") || allows("Write")),
            "bash": verdict(allows("Bash")),
            "webfetch": verdict(allows("WebFetch")),
        })
    }

    /// The `provider` block for the resolved model, keyed by the provider's
    /// id (`openai_compatible` / `ollama`) so it matches the `--model
    /// <provider>/<model>` selection. `None` when no model is configured
    /// (opencode uses its own default).
    ///
    /// The raw `api_key` is NEVER written here as a literal: `opencode.json`
    /// lives at the workspace root and is not git-excluded, so a committed
    /// secret would leak. When NO key is supplied (the default), only the
    /// provider's model + base URL are written AND opencode authenticates from
    /// its own out-of-band provider config / login (e.g. opencode → OpenRouter).
    /// When a key IS supplied, `apiKey` is written as an `{env:...}` REFERENCE
    /// (not the secret); the secret rides the subprocess env (set in
    /// [`OpencodeStrategy::apply_model_selection`]) AND opencode interpolates it
    /// at run time. (Ollama never authenticates, so a key there is inert.)
    fn provider_block(model: Option<&ResolvedModel>) -> Option<serde_json::Value> {
        let m = model?;
        let provider_id = m.provider.as_str();
        let mut options = serde_json::Map::new();
        options.insert(
            "baseURL".to_string(),
            serde_json::Value::String(m.api_base_url.clone()),
        );
        // A supplied key is referenced via `{env:VAR}` — never written raw into
        // this workspace file. No key → omit (opencode uses its own auth).
        if !m.api_key.is_empty() {
            options.insert(
                "apiKey".to_string(),
                serde_json::Value::String(format!("{{env:{OPENCODE_PROVIDER_KEY_ENV}}}")),
            );
        }
        let mut models = serde_json::Map::new();
        models.insert(m.model.clone(), serde_json::json!({}));
        let mut entry = serde_json::Map::new();
        entry.insert(
            "npm".to_string(),
            serde_json::Value::String("@ai-sdk/openai-compatible".to_string()),
        );
        entry.insert(
            "name".to_string(),
            serde_json::Value::String(provider_id.to_string()),
        );
        entry.insert("options".to_string(), serde_json::Value::Object(options));
        entry.insert("models".to_string(), serde_json::Value::Object(models));
        let mut provider = serde_json::Map::new();
        provider.insert(provider_id.to_string(), serde_json::Value::Object(entry));
        Some(serde_json::Value::Object(provider))
    }

    /// Assemble the full `opencode.json` value: the `mcp` block, the
    /// `permission` block, AND (when a model is resolved) the `provider`
    /// block.
    fn config_value(ctx: &BuildContext<'_>) -> Result<serde_json::Value> {
        // We may be running from a non-autocoder binary (e.g. cargo test).
        // `current_exe` is the actual running binary; in production the
        // `autocoder` binary, whose `mcp-ask-user-server` subcommand the MCP
        // child runs.
        let exe = std::env::current_exe()
            .context("resolving current autocoder binary path for opencode MCP config")?;
        let mut server = serde_json::Map::new();
        server.insert(
            "type".to_string(),
            serde_json::Value::String("local".to_string()),
        );
        server.insert(
            "command".to_string(),
            serde_json::json!([exe.to_string_lossy(), "mcp-ask-user-server"]),
        );
        server.insert("environment".to_string(), Self::mcp_environment(ctx));
        server.insert("enabled".to_string(), serde_json::Value::Bool(true));

        let mut mcp = serde_json::Map::new();
        mcp.insert(
            crate::mcp_askuser_server::SERVER_NAME.to_string(),
            serde_json::Value::Object(server),
        );

        let mut config = serde_json::Map::new();
        config.insert(
            "$schema".to_string(),
            serde_json::Value::String("https://opencode.ai/config.json".to_string()),
        );
        config.insert("mcp".to_string(), serde_json::Value::Object(mcp));
        config.insert(
            "permission".to_string(),
            Self::permission_block(ctx.allowed_tools),
        );
        if let Some(provider) = Self::provider_block(ctx.model) {
            config.insert("provider".to_string(), provider);
        }
        Ok(serde_json::Value::Object(config))
    }

    /// Write `<workspace>/opencode.json`. `pub(crate)` so callers that wire
    /// opencode end-to-end can reuse the exact shape; returns the path.
    pub(crate) fn write_config(ctx: &BuildContext<'_>) -> Result<PathBuf> {
        let value = Self::config_value(ctx)?;
        let path = ctx.workspace.join(OPENCODE_CONFIG_FILENAME);
        let raw = serde_json::to_string_pretty(&value)?;
        std::fs::write(&path, raw)
            .with_context(|| format!("writing opencode config {}", path.display()))?;
        Ok(path)
    }
}

impl CliStrategy for OpencodeStrategy {
    fn build_command(&self, ctx: &BuildContext<'_>) -> Command {
        // Write the workspace `opencode.json` (MCP + permissions + provider).
        // Best-effort: a write failure is logged but does not abort argv
        // assembly (the run will surface the missing-config error itself).
        if let Err(e) = Self::write_config(ctx) {
            tracing::warn!(
                workspace = %ctx.workspace.display(),
                "failed to write opencode.json (run continues): {e:#}"
            );
        }
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args).arg("run");
        // The prompt is delivered on stdin by `agentic_run`; `opencode run`
        // reads its message from piped stdin, so no positional message is
        // appended here. a70: when a resume handle is set (the implementer's
        // AskUser answer), continue that session with `--session <id>`.
        if let Some(sid) = ctx.resume_session_id {
            self.apply_resume(&mut cmd, sid);
        }
        cmd
    }

    fn apply_model_selection(&self, cmd: &mut Command, model: Option<&ResolvedModel>) {
        if let Some(m) = model {
            cmd.arg("--model")
                .arg(format!("{}/{}", m.provider.as_str(), m.model));
            // A supplied key rides the subprocess env; `opencode.json` carries
            // only the `{env:...}` reference (see `provider_block`). No key →
            // set nothing (opencode uses its own auth). The secret reaches the
            // subprocess, where the same-uid model can read it — a documented
            // opt-in exposure.
            if !m.api_key.is_empty() {
                cmd.env(OPENCODE_PROVIDER_KEY_ENV, &m.api_key);
            }
        }
        // No `ANTHROPIC_*` env — that is the claude strategy's mechanism;
        // opencode reads the provider config from `opencode.json` (written in
        // `build_command`) AND the `--model <provider>/<model>` selection.
    }

    /// `opencode run --session <id>` continues an existing session (a70). The
    /// id is captured from opencode's session store after the first run.
    ///
    /// NOTE: opencode is NOT installed in this sandbox, so the a70 integration
    /// spike (tasks 1.1/1.3) could not be run live; this flag follows
    /// opencode's documented `run` interface. The scoped session-DELETE is
    /// deliberately left as the defaulted no-op ([`CliStrategy::delete_session`])
    /// rather than guessing opencode's on-disk store layout — the surgical
    /// delete path is wired the moment that layout is confirmed (the same
    /// call-site seam a60 left for the opencode roles). No opencode sessions
    /// are created here, so the no-op leaks nothing in practice.
    fn apply_resume(&self, cmd: &mut Command, handle: &str) -> bool {
        cmd.arg("--session").arg(handle);
        true
    }
}

/// Filename of the MCP config the [`AntigravityStrategy`] writes into the
/// workspace. The Antigravity CLI (`agy`) reads MCP servers from an
/// `mcp_config.json` with the standard `mcpServers` schema (the same shape
/// Gemini CLI used). Note: the integration spike found the installed `agy`
/// discovers this file from its global config dir (`~/.gemini/config/`), NOT
/// the project root — so the end-to-end role wiring that points `agy` at this
/// workspace copy (e.g. via the CLI's config-dir resolution) is a follow-up,
/// the same call-site step a60 left for the opencode roles. This strategy
/// writes the file per the a69 contract AND unit-tests its shape.
const ANTIGRAVITY_MCP_CONFIG_FILENAME: &str = "mcp_config.json";

/// Filename of the per-run Antigravity settings the [`AntigravityStrategy`]
/// writes into the workspace, carrying the read-only tool restriction
/// (a56 sandbox → `agy` permissions). A tangible artifact mirroring a60's
/// `opencode.json` `permission` block; the runtime backstop for any escaped
/// write is the `WritePolicy::None` post-hoc revert (see the type docs).
const ANTIGRAVITY_SETTINGS_FILENAME: &str = "agy_settings.json";

/// The Antigravity CLI's default model (a69). `agy` itself drives
/// `gemini-3-pro`; the strategy selects it via `--model` when a role resolves
/// no explicit model, so a Google/Gemini-family model is always chosen.
const ANTIGRAVITY_DEFAULT_MODEL: &str = "gemini-3-pro";

/// `agy` persists one SQLite transcript per conversation under
/// `~/.gemini/antigravity-cli/conversations/<id>.db` (a70 spike, confirmed
/// against the installed `agy` 1.0.6 — NOT `~/.antigravity`, which does not
/// exist). A matching `~/.gemini/antigravity-cli/brain/<id>/` directory holds
/// the same conversation's working state, keyed by the SAME id.
const ANTIGRAVITY_STORE_SUBDIR: &str = ".gemini/antigravity-cli";

/// The `agy` (Antigravity) CLI strategy (a69) — the third [`CliStrategy`],
/// for Google's Antigravity CLI, the successor to the sunset Gemini CLI. A
/// role whose model provider resolves to `antigravity` (a55's `provider →
/// CLI` rule for [`crate::config::LlmProvider::Google`], OR an explicit
/// registry `cli: antigravity`) runs agentically through `agy` instead of
/// erroring with "no registered strategy".
///
/// The invocation is `agy -p "" --model <model>` (capture). The empty `-p`
/// value satisfies `agy`'s required print-mode flag while the prompt is
/// delivered on stdin (the integration spike confirmed `agy` reads the
/// stdin prompt and that a non-empty `-p` value would be treated as a SECOND
/// prompt; [`agentic_run`] already pipes the prompt on stdin). The model is
/// selected via `--model <model>` (default `gemini-3-pro`).
///
/// Auth (a69): the strategy sets `AV_API_KEY` from the resolved model's
/// `api_key` when one is configured, AND sets NO `ANTHROPIC_*` (the claude
/// strategy's mechanism). In practice the registry resolves Google models
/// with an empty key (a003-faithful — like Ollama), so `agy` authenticates
/// from its own OAuth login / credential store (the spike confirmed the host
/// login); the `AV_API_KEY` path covers operators who provide an explicit
/// Antigravity API key. The key is NEVER written into any workspace file.
///
/// MCP (a69): writes `mcp_config.json` (`mcpServers` schema: per-server
/// `command`/`args`/`env` incl. `ORCH_MCP_ROLE`, local stdio) so the role's
/// `submit_*` tool is reachable — the same submission contract a56 requires
/// of the claude path. It writes NEITHER `.mcp.json` (claude) NOR
/// `opencode.json` (opencode).
///
/// Read-only sandbox (a69): for a read-only role (a56 sandbox: allow
/// Read/Glob/Grep; deny Write/Edit/Bash) the strategy appends `--sandbox`
/// (the OS-level Terminal Sandbox) AND emits a tool restriction (read tools +
/// the role's `submit_*` tool allowed; shell/write/edit denied). Because the
/// exact non-interactive deny mechanism is best-effort (the spike found `agy`
/// auto-runs read/terminal tools in `-p` mode), a read-only `agy` role does
/// NOT rely on the restriction alone: the existing `WritePolicy::None`
/// post-hoc enforcement (non-empty `git status --porcelain` → `git reset
/// --hard HEAD` + fail) is the backstop.
///
/// `agy` is capture-mode only; the streaming-JSON event path stays claude-
/// specific (`agy`'s `--stream` emits SSE, a different format).
pub struct AntigravityStrategy {
    pub command: String,
    pub args: Vec<String>,
}

impl AntigravityStrategy {
    pub fn new(command: String, args: Vec<String>) -> Self {
        Self { command, args }
    }

    /// Whether the a56 sandbox is read-only: none of Write/Edit/Bash is in
    /// the allowed-tools list. Read-only roles get `--sandbox` AND the
    /// deny-shell/write/edit tool restriction.
    fn is_read_only(allowed_tools: &[String]) -> bool {
        let allows = |name: &str| allowed_tools.iter().any(|t| t.eq_ignore_ascii_case(name));
        !(allows("Write") || allows("Edit") || allows("Bash"))
    }

    /// The MCP child's `env` map for `mcp_config.json`
    /// (`mcpServers.<server>.env`). Mirrors [`OpencodeStrategy::mcp_environment`]
    /// AND `ClaudeCliExecutor::write_mcp_config`: always the workspace; the role
    /// (as both `ORCH_MCP_CHANGE` submission key AND `ORCH_MCP_ROLE`) when set;
    /// the daemon control-socket vars when the parent carries them.
    fn mcp_environment(ctx: &BuildContext<'_>) -> serde_json::Value {
        let mut env = serde_json::Map::new();
        env.insert(
            crate::mcp_askuser_server::ENV_WORKSPACE.to_string(),
            serde_json::Value::String(ctx.workspace.to_string_lossy().into_owned()),
        );
        if let Some(role) = ctx.mcp_role {
            env.insert(
                crate::mcp_askuser_server::ENV_CHANGE.to_string(),
                serde_json::Value::String(role.to_string()),
            );
            env.insert(
                crate::mcp_askuser_server::ENV_ROLE.to_string(),
                serde_json::Value::String(role.to_string()),
            );
        }
        if let Ok(socket) = std::env::var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET) {
            env.insert(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET.to_string(),
                serde_json::Value::String(socket),
            );
            let basename = std::env::var(crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME)
                .unwrap_or_else(|_| {
                    ctx.workspace
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown_workspace")
                        .to_string()
                });
            env.insert(
                crate::mcp_askuser_server::ENV_WORKSPACE_BASENAME.to_string(),
                serde_json::Value::String(basename),
            );
        }
        serde_json::Value::Object(env)
    }

    /// Assemble the `mcp_config.json` value: the `mcpServers` block with the
    /// orchestrator MCP child (`command` string + `args` array +
    /// `env` incl. `ORCH_MCP_ROLE`, local stdio). a003: NO credential is ever
    /// written here (the resolved `api_key` goes to `AV_API_KEY` env, not a
    /// file).
    fn mcp_config_value(ctx: &BuildContext<'_>) -> Result<serde_json::Value> {
        let exe = std::env::current_exe()
            .context("resolving current autocoder binary path for antigravity MCP config")?;
        let mut server = serde_json::Map::new();
        server.insert(
            "command".to_string(),
            serde_json::Value::String(exe.to_string_lossy().into_owned()),
        );
        server.insert(
            "args".to_string(),
            serde_json::json!(["mcp-ask-user-server"]),
        );
        server.insert("env".to_string(), Self::mcp_environment(ctx));

        let mut servers = serde_json::Map::new();
        servers.insert(
            crate::mcp_askuser_server::SERVER_NAME.to_string(),
            serde_json::Value::Object(server),
        );
        let mut config = serde_json::Map::new();
        config.insert("mcpServers".to_string(), serde_json::Value::Object(servers));
        Ok(serde_json::Value::Object(config))
    }

    /// Map a56's allowed-tools list onto Antigravity's read-only tool
    /// restriction: the read tools (whatever is in `allowed_tools`) plus the
    /// role's `submit_*` MCP tool are `allow`ed; shell/write/edit (`Bash` /
    /// `Write` / `Edit`) are `deny`ed unless explicitly allowed. `toolPermission:
    /// deny` makes deny the default so nothing outside the allow-list runs
    /// unprompted in non-interactive mode. Returned as a value so the writer
    /// AND the unit test share one source of truth (mirrors a60's
    /// `OpencodeStrategy::permission_block`).
    fn tool_restriction(allowed_tools: &[String], mcp_role: Option<&str>) -> serde_json::Value {
        let allows = |name: &str| allowed_tools.iter().any(|t| t.eq_ignore_ascii_case(name));
        // Allow the read tools the sandbox granted, plus the role's submit_* tool.
        let mut allow: Vec<String> = allowed_tools.to_vec();
        if let Some(role) = mcp_role
            && let Some(tool) = crate::mcp_askuser_server::submission_tool_name_for_role(role)
        {
            allow.push(crate::mcp_askuser_server::qualified_tool_name(tool));
        }
        // Deny the mutating/shell tools that are NOT in the allow-list.
        let deny: Vec<String> = ["Bash", "Write", "Edit"]
            .iter()
            .filter(|t| !allows(t))
            .map(|t| t.to_string())
            .collect();
        serde_json::json!({
            "sandbox": true,
            "toolPermission": "deny",
            "permissions": { "allow": allow, "deny": deny },
        })
    }

    /// Write `<workspace>/mcp_config.json`. `pub(crate)` so callers wiring
    /// `agy` end-to-end can reuse the exact shape; returns the path.
    pub(crate) fn write_mcp_config(ctx: &BuildContext<'_>) -> Result<PathBuf> {
        let value = Self::mcp_config_value(ctx)?;
        let path = ctx.workspace.join(ANTIGRAVITY_MCP_CONFIG_FILENAME);
        let raw = serde_json::to_string_pretty(&value)?;
        std::fs::write(&path, raw)
            .with_context(|| format!("writing antigravity mcp config {}", path.display()))?;
        Ok(path)
    }

    /// Write `<workspace>/agy_settings.json` carrying the read-only tool
    /// restriction. Returns the path.
    pub(crate) fn write_settings(ctx: &BuildContext<'_>) -> Result<PathBuf> {
        let value = Self::tool_restriction(ctx.allowed_tools, ctx.mcp_role);
        let path = ctx.workspace.join(ANTIGRAVITY_SETTINGS_FILENAME);
        let raw = serde_json::to_string_pretty(&value)?;
        std::fs::write(&path, raw)
            .with_context(|| format!("writing antigravity settings {}", path.display()))?;
        Ok(path)
    }
}

impl CliStrategy for AntigravityStrategy {
    fn build_command(&self, ctx: &BuildContext<'_>) -> Command {
        // Write the workspace MCP config (mcpServers). Best-effort: a write
        // failure is logged but does not abort argv assembly.
        if let Err(e) = Self::write_mcp_config(ctx) {
            tracing::warn!(
                workspace = %ctx.workspace.display(),
                "failed to write antigravity mcp_config.json (run continues): {e:#}"
            );
        }
        let read_only = Self::is_read_only(ctx.allowed_tools);
        if read_only && let Err(e) = Self::write_settings(ctx) {
            tracing::warn!(
                workspace = %ctx.workspace.display(),
                "failed to write antigravity settings (run continues): {e:#}"
            );
        }

        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            // Print (single-shot) mode. The empty value satisfies `agy`'s
            // required print flag; the prompt is delivered on stdin by
            // `agentic_run` (a non-empty value would be treated as a SECOND
            // prompt — see the spike notes). `--resume` is the claude
            // streaming-recovery mechanism and is intentionally ignored.
            .arg("-p")
            .arg("");
        // a70: when a resume handle is set (the implementer's AskUser answer),
        // continue that conversation with `--conversation <id>`.
        if let Some(sid) = ctx.resume_session_id {
            self.apply_resume(&mut cmd, sid);
        }
        if read_only {
            // The OS-level Terminal Sandbox; the tool restriction + the
            // `WritePolicy::None` post-hoc revert are the deny backstops.
            cmd.arg("--sandbox");
        }
        cmd
    }

    fn apply_model_selection(&self, cmd: &mut Command, model: Option<&ResolvedModel>) {
        let model_id = model
            .map(|m| m.model.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(ANTIGRAVITY_DEFAULT_MODEL);
        cmd.arg("--model").arg(model_id);
        // a69: Antigravity's auth env. Set ONLY from an explicitly-resolved
        // key; the registry resolves Google models with an empty key
        // (a003-faithful), so production runs authenticate from `agy`'s own
        // OAuth login store. NO `ANTHROPIC_*` (the claude mechanism).
        if let Some(m) = model
            && !m.api_key.is_empty()
        {
            cmd.env("AV_API_KEY", &m.api_key);
        }
    }

    /// `agy --conversation <id>` resumes a previous conversation by id (a70
    /// spike: `agy --help` lists `--conversation` for "Resume a previous
    /// conversation by ID"). Delivered alongside the `-p ""` print flag so the
    /// answer arrives on stdin into the resumed conversation.
    fn apply_resume(&self, cmd: &mut Command, handle: &str) -> bool {
        cmd.arg("--conversation").arg(handle);
        true
    }

    /// The conversations directory holding one `<id>.db` per conversation.
    /// Antigravity keys its store by conversation id, NOT by workspace, so the
    /// directory is workspace-independent.
    fn session_store_dir(&self, ctx: SessionStoreCtx<'_>) -> Option<PathBuf> {
        Some(ctx.home.join(ANTIGRAVITY_STORE_SUBDIR).join("conversations"))
    }

    /// Remove ONLY this conversation's records — `conversations/<id>.db` AND
    /// the matching `brain/<id>/` directory (both keyed by the same id) —
    /// leaving `settings.json`, `oauth_creds.json`, `GEMINI.md`, the generated
    /// `mcp_config.json`, AND every other conversation intact.
    fn delete_session(&self, ctx: SessionStoreCtx<'_>, handle: &str) -> Result<bool> {
        // a70 hardening: a handle with a separator or `..` would let the
        // `join(..)` below escape the store (Rust does not normalize `..`),
        // and the `brain` `remove_dir_all` would then recursively delete an
        // arbitrary directory. An empty handle would point `brain` at the
        // whole `brain/` dir. Refuse any such handle rather than traverse.
        if reject_unsafe_session_handle(handle) {
            return Ok(false);
        }
        let store = ctx.home.join(ANTIGRAVITY_STORE_SUBDIR);
        let mut removed = delete_session_file(&store.join("conversations").join(format!("{handle}.db")))?;
        let brain = store.join("brain").join(handle);
        match std::fs::remove_dir_all(&brain) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("pruning conversation brain dir {}", brain.display()));
            }
        }
        Ok(removed)
    }
}

/// Resolve a role's strategy from the model's provider via a55's
/// `provider → default CLI` rule ([`crate::config::default_cli_for`]).
///
/// Forward-looking API: the agentic roles that resolve a model per-role
/// (changes 4–8) call this; this change registers the rule + the `claude`
/// strategy AND exercises both via tests.
#[allow(dead_code)]
pub fn strategy_for_provider(
    provider: crate::config::LlmProvider,
    command: String,
    args: Vec<String>,
) -> Result<Box<dyn CliStrategy>> {
    strategy_for_cli(crate::config::default_cli_for(provider), command, args)
}

/// Resolve the strategy for a specific CLI. `claude` (a56), `opencode`
/// (a60) AND `antigravity` (a69) are registered; each maps to a real
/// strategy with no subprocess spawned at resolution time. The `Result` is
/// retained so a future CLI can land an error arm without changing call sites.
#[allow(dead_code)]
pub fn strategy_for_cli(
    cli: crate::config::CliKind,
    command: String,
    args: Vec<String>,
) -> Result<Box<dyn CliStrategy>> {
    match cli {
        crate::config::CliKind::Claude => Ok(Box::new(ClaudeStrategy::new(command, args))),
        crate::config::CliKind::Opencode => Ok(Box::new(OpencodeStrategy::new(command, args))),
        // a69: the Antigravity (`agy`) strategy for Google/Gemini models.
        crate::config::CliKind::Antigravity => {
            Ok(Box::new(AntigravityStrategy::new(command, args)))
        }
    }
}

/// Startup WARN for a role that resolves to a [`CliStrategy`] AND carries a
/// configured `api_key`. A supplied key is now PASSED to the wrapped CLI so the
/// CLI uses it — but because the sandboxed model shares the CLI's process and
/// uid, the key reaches a place the model can read (claude/`agy` via the
/// subprocess env, opencode via an `{env:...}` reference resolved from it). This
/// returns the one-line WARN the daemon logs exactly once at startup so the
/// operator opts into that exposure knowingly; omitting the key uses the CLI's
/// own login instead (no credential reaches the subprocess). Returns `None`
/// when no key is configured (`has_key == false`) — the no-exposure default.
///
/// Roles that use autocoder's in-process HTTP path (e.g. the `oneshot`
/// reviewer's `LlmClient`) resolve and use their key inside the daemon process
/// AND must NOT call this — their key never reaches a subprocess. Separated from
/// the logging site (`cli::run` startup) as a pure decision so tests assert the
/// disposition without a daemon, mirroring
/// [`crate::code_reviewer::startup_reviewer_kind_decision`].
pub fn cli_role_key_exposure_warning(role_label: &str, has_key: bool) -> Option<String> {
    has_key.then(|| {
        format!(
            "role `{role_label}` has a configured `api_key`: it is passed to the wrapped \
             CLI so the CLI uses that key, AND because the sandboxed model shares the \
             CLI's process it can read the key — a deliberate opt-in exposure. Omit \
             `api_key` from this role to authenticate from the CLI's own login instead \
             (no credential then reaches the subprocess)."
        )
    })
}

/// Everything [`agentic_run`] needs for one run. Most call sites set only
/// a handful of fields; the rest carry safe per-caller defaults that
/// preserve each pre-refactor path's exact behavior.
pub struct AgenticRunOpts<'a> {
    pub workspace: &'a Path,
    /// Log identifier (the change name, or a synthetic name for non-change
    /// flows). Used only by streaming mode to compute the structured-log
    /// path.
    pub change: &'a str,
    pub strategy: &'a dyn CliStrategy,
    pub prompt: &'a str,
    pub sandbox: SandboxConfig,
    pub model: Option<&'a ResolvedModel>,
    pub output_mode: OutputMode,
    pub timeout: Duration,
    /// Daemon paths (for the structured-log path AND the busy-marker
    /// sidecar). `None` for the audits, which capture-only and write no
    /// sidecar.
    pub paths: Option<&'a Arc<crate::paths::DaemonPaths>>,
    pub settings_dir: Option<&'a Path>,
    /// Append the autocoder MCP provided-tool names to `--allowedTools`.
    pub include_autocoder_tools: bool,
    /// Emit `--verbose --output-format stream-json` even in capture mode
    /// (the recovery turn emits stream-json but reads it at exit). Ignored
    /// in streaming mode, which always emits the flags.
    pub emit_stream_json_in_capture: bool,
    /// `--resume <session_id>` for the recovery turn's session reuse.
    pub resume_session_id: Option<&'a str>,
    /// Write the busy-marker subprocess-PID sidecar (the executor paths,
    /// so stuck-state recovery can `killpg` the child's group). Audits
    /// do not.
    pub track_subprocess_marker: bool,
    /// Spawn via the ETXTBSY-retry helper (the audits, which race parallel
    /// test fixtures writing sibling scripts). The executor uses a plain
    /// spawn.
    pub etxtbsy_retry_spawn: bool,
    /// a006: the OS-level sandbox to wrap this spawn in. `enforce == false`
    /// (the default) skips the OS layer entirely (test fixtures); production
    /// call sites set an enforced [`crate::sandbox::RunSandbox`] so EVERY role
    /// is wrapped and no role can opt out. When enforced with no available
    /// mechanism AND no operator opt-in, the spawn fails closed.
    pub os_sandbox: crate::sandbox::RunSandbox,
}

/// The directory the per-run OS-sandbox `--settings` file is written into.
///
/// Defaults to the run workspace's `.git` directory. The OS sandbox (a006)
/// gives the child a private `/tmp` (systemd `PrivateTmp=yes`, bwrap
/// `--tmpfs /tmp`) and, for read-only roles, a masked `$HOME` — so a settings
/// file written to the host temp dir is invisible inside the child's mount
/// namespace and the wrapped CLI fails with `Settings file not found:
/// /tmp/...`. The workspace is the one path every mechanism binds into the
/// namespace, and `.git` rides along inside it.
///
/// `.git` (rather than the workspace root) is deliberate: it is reachable by
/// the sandboxed CLI, but git never stages (`add -A`), reports (`status`), or
/// `clean`s files under `.git`, so this purely per-run file (regenerated each
/// run, RAII-deleted on exit) can never leak into a PR, trip the dirty-
/// workspace check, or linger as gitignored litter — and it needs no
/// `.git/info/exclude` entry. Keeping it out of the working tree also matches
/// the a16 rule that daemon bookkeeping never lives in the managed repo tree.
///
/// Tests pass an explicit `settings_dir` (a per-test `TempDir`) to override.
fn sandbox_settings_dir(settings_dir: Option<&Path>, workspace: &Path) -> PathBuf {
    match settings_dir {
        Some(dir) => dir.to_path_buf(),
        None => workspace.join(".git"),
    }
}

/// The daemon control-socket path to bind into the OS sandbox, so the
/// per-execution MCP child can `connect()` to relay outcomes / submissions.
///
/// Read from the same `ORCH_DAEMON_CONTROL_SOCKET` env var that is forwarded
/// into the child. The socket otherwise lives outside the sandbox namespace
/// whenever it sits in a masked location — under `/tmp` (the sandbox's private
/// `/tmp`) or a masked `$HOME` (read-only roles) — so without an explicit bind
/// the relay's `connect()` fails and the run times out instead of recording
/// its outcome. `None` when the var is unset/empty (tests, or a run with no
/// relay), in which case no bind is added.
fn sandbox_control_socket_bind() -> Option<PathBuf> {
    control_socket_from_env(std::env::var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET).ok())
}

/// Pure core of [`sandbox_control_socket_bind`]: a present, non-empty value
/// becomes the bind path; absent/empty yields `None`. Split out so it is
/// testable without mutating the process environment.
fn control_socket_from_env(val: Option<String>) -> Option<PathBuf> {
    val.filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// Spawn the wrapped CLI, write `prompt` on its stdin, wait with the
/// configured timeout, AND return the unified outcome. See the module
/// docs for the behavior contract.
pub async fn agentic_run(opts: AgenticRunOpts<'_>) -> Result<AgenticRunOutcome> {
    // a006 fail-closed gate (task 4.1): when the OS sandbox is enforced but no
    // mechanism is available AND the operator has not opted into unsandboxed
    // operation, refuse to spawn — BEFORE writing any settings or building the
    // command. `None` here means the OS layer is not enforced for this run.
    let spawn_plan = if opts.os_sandbox.enforce {
        Some(
            crate::sandbox::decide_spawn(
                opts.os_sandbox.mechanism,
                opts.os_sandbox.allow_unsandboxed,
            )
            .context("OS-level sandbox mechanism gate")?,
        )
    } else {
        None
    };

    // a006 engine_deny (task 5.2): extend the per-invocation read-deny set to
    // every registered CLI store (self included) so the agent's `Read`/`Bash`
    // tools are denied those paths at the CLI permission layer. Supplied
    // per-invocation through the settings file below — never by mutating the
    // operator's global CLI config.
    let mut disallowed_read_paths = opts.sandbox.disallowed_read_paths.clone();
    if opts.os_sandbox.enforce {
        disallowed_read_paths.extend(opts.os_sandbox.engine_deny_paths());
    }
    let resolved_sandbox = crate::config::ResolvedSandbox {
        allowed_tools: opts.sandbox.allowed_tools.clone(),
        disallowed_bash_patterns: opts.sandbox.disallowed_bash_patterns.clone(),
        disallowed_read_paths,
    };
    let settings_dir = sandbox_settings_dir(opts.settings_dir, opts.workspace);
    let (settings_path, _settings_guard) = crate::audits::write_sandbox_settings(
        &resolved_sandbox,
        Some(&settings_dir),
        opts.sandbox.deny_writes,
    )
    .context("generating sandbox settings file")?;

    let streaming = matches!(opts.output_mode, OutputMode::Streaming);
    let emit_stream_json = streaming || opts.emit_stream_json_in_capture;

    // The command is pure argv assembly (no IO), so it can be rebuilt on
    // each ETXTBSY retry attempt. The settings file is written exactly
    // once, above.
    let build = || {
        let ctx = BuildContext {
            settings_path: &settings_path,
            allowed_tools: &opts.sandbox.allowed_tools,
            include_autocoder_tools: opts.include_autocoder_tools,
            emit_stream_json,
            resume_session_id: opts.resume_session_id,
            workspace: opts.workspace,
            model: opts.model,
            // The submission roles that drive opencode (reviewer a58,
            // contradiction check a59) currently write their own `.mcp.json`
            // via `write_mcp_config` and key the role there; threading the
            // role through to the opencode strategy's `opencode.json` writer
            // is the call-site change those roles make when they opt into
            // opencode end-to-end. This change registers the strategy AND
            // exposes the seam (`BuildContext::mcp_role`); it does not modify
            // a58/a59, so the production build leaves it `None`.
            mcp_role: None,
        };
        let mut inner_cmd = opts.strategy.build_command(&ctx);
        opts.strategy.apply_model_selection(&mut inner_cmd, opts.model);

        // a014 (task 3.1): layer the operator's captured, credential-filtered
        // login-shell environment onto the strategy command so shell-init-
        // activated toolchains (pyenv/rbenv/poetry/nvm) are usable in the
        // subprocess. Applied to the INNER command BEFORE the OS-sandbox wrapper
        // so it flows through every mechanism uniformly (systemd `--setenv`,
        // bwrap inheritance). `apply_captured_env` never overrides a variable
        // the strategy/model already set — the run-set value wins on conflict —
        // and is a no-op for an empty (degraded / not-yet-captured) capture.
        crate::agent_env::apply_captured_env(
            &mut inner_cmd,
            &crate::agent_env::current_captured_env(),
        );

        // a006 (tasks 2.1–2.5, 3.1): wrap the strategy command in the OS-level
        // sandbox via the resolved mechanism. The wrapper preserves stdio +
        // process-group + timeout/kill behavior unchanged — the `--pipe` /
        // bwrap pass-through keeps streaming-JSON and capture modes intact.
        // `Unsandboxed` (operator opt-in) and the not-enforced path spawn the
        // strategy command directly.
        let mut cmd = match spawn_plan {
            Some(crate::sandbox::SpawnPlan::Wrap(mechanism)) => {
                let inner = crate::sandbox::InnerCommand::from_command(&inner_cmd);
                // a013: the program (resolved + bound under an allowlist policy
                // so the wrapped CLI execs under a masked home) drives the plan.
                let mut plan = opts.os_sandbox.build_plan(opts.workspace, &inner.program);
                // Bind the daemon control socket into the namespace so the
                // per-execution MCP child can connect() to relay outcomes /
                // submissions — even when the socket lives under /tmp or a
                // masked home. Without this the relay times out: the run does
                // the work but is never recorded.
                if let Some(sock) = sandbox_control_socket_bind() {
                    plan.extra_ro_paths.push(sock);
                }
                crate::sandbox::wrap_command(mechanism, &plan, &inner)
            }
            _ => inner_cmd,
        };
        cmd.current_dir(opts.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so stuck-state recovery can `killpg` the
            // whole subprocess tree with one signal. `process_group(0)` is
            // stable Rust.
            .process_group(0);
        cmd
    };

    let mut child = if opts.etxtbsy_retry_spawn {
        crate::audits::spawn_with_etxtbsy_retry(build)
            .await
            .context("spawning agentic-run subprocess")?
    } else {
        build().spawn().context("spawning agentic-run subprocess")?
    };

    // Record the spawned child's PID to a sidecar so the busy-marker
    // stuck-state recovery has a kill target covering the child's process
    // group. The guard cleans the file on every exit path.
    let _subprocess_marker_guard = if opts.track_subprocess_marker {
        match (opts.paths, child.id()) {
            (Some(paths), Some(pid)) => {
                if let Err(e) =
                    crate::busy_marker::write_subprocess_marker(paths, opts.workspace, pid)
                {
                    tracing::warn!(
                        workspace = %opts.workspace.display(),
                        pid,
                        "failed to write subprocess sidecar marker (run continues): {e:#}"
                    );
                    None
                } else {
                    Some(SubprocessMarkerGuard {
                        paths: paths.clone(),
                        workspace: opts.workspace.to_path_buf(),
                    })
                }
            }
            _ => None,
        }
    } else {
        None
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(opts.prompt.as_bytes()).await;
    }
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    if streaming {
        // Streaming requires the daemon paths for the structured-log path.
        // Production always provides them; fall back to capture if absent.
        if let Some(paths) = opts.paths {
            run_streaming(
                child,
                stdout_pipe,
                stderr_pipe,
                paths,
                opts.workspace,
                opts.change,
                opts.prompt,
                opts.timeout,
            )
            .await
        } else {
            run_capture(child, stdout_pipe, stderr_pipe, opts.timeout).await
        }
    } else {
        run_capture(child, stdout_pipe, stderr_pipe, opts.timeout).await
    }
}

/// Resolve the home directory used to locate a strategy's session store:
/// the explicit `override_home` (tests) else `$HOME`.
fn resolve_session_home(override_home: Option<&Path>) -> Option<PathBuf> {
    override_home
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

/// The set of session-record stems (filenames without extension) currently in
/// `dir`. A missing directory yields the empty set. Used to attribute the
/// session a run created by diffing this snapshot before/after the run.
fn snapshot_session_stems(dir: &Path) -> std::collections::BTreeSet<String> {
    let mut set = std::collections::BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                set.insert(stem.to_string());
            }
        }
    }
    set
}

/// Run a session AND manage its lifecycle (a70). Wraps [`agentic_run`] with
/// the session-hygiene the daemon owns:
///
/// 1. Snapshot the strategy's session-store directory before the run.
/// 2. Run it.
/// 3. Attribute the created session handle — the streamed `session_id` for a
///    `claude` run, else the single new entry that appeared in the store — AND
///    record it on [`AgenticRunOutcome::session_handle`].
/// 4. When `prune` is set (single-shot roles, which never resume), delete that
///    one session record via the strategy's scoped [`CliStrategy::delete_session`].
///    The implementer passes `prune = false` and prunes at its terminal
///    outcome instead, because it may retain the session across an AskUser.
///
/// `home_override` points the store resolution at a test home; production
/// passes `None` (resolves `$HOME`). A strategy with no known store
/// ([`CliStrategy::session_store_dir`] → `None`) is a no-op for capture AND
/// prune — the run still completes normally.
pub async fn agentic_run_with_session(
    opts: AgenticRunOpts<'_>,
    prune: bool,
    home_override: Option<&Path>,
) -> Result<AgenticRunOutcome> {
    let strategy = opts.strategy;
    let workspace = opts.workspace.to_path_buf();
    // A resume run continues an existing session, so no NEW store entry
    // appears; fall back to the resumed id so the handle survives a
    // resume-then-AskUser-again sequence.
    let resume_id = opts.resume_session_id.map(str::to_string);
    let home = resolve_session_home(home_override);
    let store_dir = home.as_ref().and_then(|h| {
        strategy.session_store_dir(SessionStoreCtx {
            home: h,
            workspace: &workspace,
        })
    });
    let before = store_dir
        .as_ref()
        .map(|d| snapshot_session_stems(d))
        .unwrap_or_default();

    let mut outcome = agentic_run(opts).await?;

    // Attribute the created session: a streamed `session_id` is authoritative;
    // otherwise the lone new store entry. >1 new entries (concurrent writers)
    // or 0 leave the handle `None` rather than guess.
    let handle = outcome
        .session_id
        .clone()
        .or_else(|| {
            store_dir.as_ref().and_then(|d| {
                let after = snapshot_session_stems(d);
                let mut fresh = after.difference(&before);
                match (fresh.next(), fresh.next()) {
                    (Some(only), None) => Some(only.clone()),
                    _ => None,
                }
            })
        })
        .or(resume_id);
    outcome.session_handle = handle.clone();

    if prune && let (Some(h), Some(home)) = (handle, home.as_ref()) {
        match strategy.delete_session(
            SessionStoreCtx {
                home,
                workspace: &workspace,
            },
            &h,
        ) {
            Ok(removed) => {
                tracing::debug!(
                    session = %h,
                    removed,
                    "pruned single-shot agentic session record"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session = %h,
                    "failed to prune agentic session record (run continues): {e:#}"
                );
            }
        }
    }

    Ok(outcome)
}

/// Capture path: wait for child exit (or timeout) then read stdout +
/// stderr in one shot. No structured log is written.
async fn run_capture(
    mut child: tokio::process::Child,
    mut stdout_pipe: Option<tokio::process::ChildStdout>,
    mut stderr_pipe: Option<tokio::process::ChildStderr>,
    timeout: Duration,
) -> Result<AgenticRunOutcome> {
    let sleeper = tokio::time::sleep(timeout);
    tokio::pin!(sleeper);

    let exit_status: Option<std::io::Result<std::process::ExitStatus>> = tokio::select! {
        biased;
        () = &mut sleeper => None,
        res = child.wait() => Some(res),
    };

    match exit_status {
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Ok(AgenticRunOutcome {
                timed_out: true,
                exit_status: None,
                stdout: String::new(),
                stderr: "timeout".to_string(),
                ..Default::default()
            })
        }
        Some(Err(e)) => Err(e).context("waiting on agentic-run child process"),
        Some(Ok(status)) => {
            let mut stdout_text = String::new();
            if let Some(ref mut p) = stdout_pipe {
                let _ = p.read_to_string(&mut stdout_text).await;
            }
            let mut stderr_text = String::new();
            if let Some(ref mut p) = stderr_pipe {
                let _ = p.read_to_string(&mut stderr_text).await;
            }
            Ok(AgenticRunOutcome {
                timed_out: false,
                exit_status: Some(status),
                stdout: stdout_text,
                stderr: stderr_text,
                ..Default::default()
            })
        }
    }
}

/// Streaming path: open the structured log writer, spawn one task that
/// reads stdout line-by-line and dispatches parsed events to the log + one
/// task that reads stderr into the writer's buffer, then race
/// `child.wait()` against the timeout. On timeout-kill the partial action
/// stream is already on disk; the writer is `finalize`d unconditionally.
#[allow(clippy::too_many_arguments)]
async fn run_streaming(
    mut child: tokio::process::Child,
    stdout_pipe: Option<tokio::process::ChildStdout>,
    stderr_pipe: Option<tokio::process::ChildStderr>,
    paths: &crate::paths::DaemonPaths,
    workspace: &Path,
    change: &str,
    prompt: &str,
    timeout: Duration,
) -> Result<AgenticRunOutcome> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let log_path = crate::executor::claude_cli::run_log_path(paths, workspace, change);
    let writer = match event_log::open(&log_path) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            tracing::warn!(
                log_file = %log_path.display(),
                "could not open structured log; falling back to capture: {e:#}"
            );
            return run_capture(child, stdout_pipe, stderr_pipe, timeout).await;
        }
    };
    if let Err(e) = writer.write_prompt(prompt) {
        tracing::warn!(
            log_file = %log_path.display(),
            "writing prompt header to structured log failed: {e:#}"
        );
    }

    // Stdout reader: parse one JSON event per line; dispatch each to the
    // structured log. Accumulate the raw lines too so callers' `stdout`
    // still reflects what was emitted (sentinel extraction, heuristics).
    let stdout_writer = writer.clone();
    let stdout_handle: tokio::task::JoinHandle<String> = match stdout_pipe {
        Some(pipe) => tokio::spawn(async move {
            let mut buf = String::new();
            let mut reader = BufReader::new(pipe).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        buf.push_str(&line);
                        buf.push('\n');
                        dispatch_event_to_log(&stdout_writer, &line);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("stdout reader error: {e}");
                        break;
                    }
                }
            }
            buf
        }),
        None => tokio::spawn(async { String::new() }),
    };

    // Stderr reader: stream bytes into the writer's buffer so the STDERR
    // section's annotation reflects the true byte count.
    let stderr_writer = writer.clone();
    let stderr_handle: tokio::task::JoinHandle<String> = match stderr_pipe {
        Some(mut pipe) => tokio::spawn(async move {
            let mut buf = String::new();
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        let _ = stderr_writer.append_stderr(&chunk[..n]);
                    }
                    Err(e) => {
                        tracing::warn!("stderr reader error: {e}");
                        break;
                    }
                }
            }
            buf
        }),
        None => tokio::spawn(async { String::new() }),
    };

    let sleeper = tokio::time::sleep(timeout);
    tokio::pin!(sleeper);

    let exit_status: Option<std::io::Result<std::process::ExitStatus>> = tokio::select! {
        biased;
        () = &mut sleeper => None,
        res = child.wait() => Some(res),
    };

    let timed_out = exit_status.is_none();
    let status_opt: Option<std::process::ExitStatus> = match exit_status {
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            None
        }
        Some(Err(e)) => return Err(e).context("waiting on agentic-run child process"),
        Some(Ok(s)) => Some(s),
    };

    // The reader tasks return when their pipe hits EOF (the child closed
    // its end). After `child.wait()` / `start_kill()` the child is reaped;
    // awaiting the readers is safe.
    let stdout_text = stdout_handle.await.unwrap_or_default();
    let stderr_text = stderr_handle.await.unwrap_or_default();

    // Flush the structured log AFTER readers finished so the FINAL ANSWER
    // section reflects whatever set_final_answer captured.
    if let Err(e) = writer.finalize() {
        tracing::warn!(
            log_file = %log_path.display(),
            "finalizing structured log failed: {e:#}"
        );
    }
    let final_answer = writer.final_answer();
    let session_id = writer.session_id();

    Ok(AgenticRunOutcome {
        timed_out,
        exit_status: status_opt,
        stdout: stdout_text,
        stderr: if timed_out && stderr_text.is_empty() {
            "timeout".to_string()
        } else {
            stderr_text
        },
        final_answer,
        session_id,
        streamed_log: true,
        // Populated by `agentic_run_with_session` (it owns the home/store
        // resolution); the bare `agentic_run` path leaves it `None`.
        session_handle: None,
    })
}

/// RAII guard that removes the subprocess-PID sidecar when dropped, so the
/// next iteration's busy-marker recovery only sees a sidecar when an actual
/// orphan exists (the daemon crashed before Drop ran).
struct SubprocessMarkerGuard {
    paths: Arc<crate::paths::DaemonPaths>,
    workspace: std::path::PathBuf,
}

impl Drop for SubprocessMarkerGuard {
    fn drop(&mut self) {
        crate::busy_marker::remove_subprocess_marker(&self.paths, &self.workspace);
    }
}

// ---------------------------------------------------------------------------
// Streaming-JSON event dispatch (moved from `executor::claude_cli`).
// ---------------------------------------------------------------------------

/// Parse a stdout line as a JSON event and append a corresponding
/// ACTIONS-section line (or, for the `result` event, capture the final
/// answer in the writer's buffer). Malformed JSON lands as `[raw]`;
/// unknown event types as `[unknown:<type>]` — neither aborts the
/// stream-reader loop.
fn dispatch_event_to_log(writer: &StructuredLogWriter, line: &str) {
    if line.is_empty() {
        return;
    }
    match json_event::parse_event_line(line) {
        Ok(event) => dispatch_parsed_event(writer, event),
        Err(e) => {
            tracing::warn!("claude stream-json: malformed line, recording as [raw]: {e}");
            let _ = writer.append_action(ActionKind::Raw, line);
        }
    }
}

fn dispatch_parsed_event(writer: &StructuredLogWriter, event: JsonEvent) {
    match event {
        JsonEvent::System { content } => {
            // Init metadata isn't actionable for operators; suppress from
            // the action stream. We DO capture the session_id so the
            // recovery loop can `claude --resume <session_id>`.
            if let Some(id) = content.get("session_id").and_then(|v| v.as_str())
                && !id.is_empty()
            {
                writer.set_session_id(id.to_string());
            }
        }
        JsonEvent::Assistant { content_blocks } => {
            for block in content_blocks {
                match block {
                    AssistantBlock::Text { text } => {
                        for line in wrap_assistant_text(&text) {
                            let _ = writer.append_action(ActionKind::Assistant, &line);
                        }
                    }
                    AssistantBlock::ToolUse {
                        tool_name,
                        tool_input,
                    } => {
                        let summary = format_tool_input_summary(&tool_input);
                        let content = if summary.is_empty() {
                            tool_name
                        } else {
                            format!("{tool_name} {summary}")
                        };
                        let _ = writer.append_action(ActionKind::ToolUse, &content);
                    }
                }
            }
        }
        JsonEvent::User { content_blocks } => {
            for block in content_blocks {
                match block {
                    UserBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        if is_error {
                            let msg: String = content.chars().take(200).collect();
                            let _ = writer.append_action(
                                ActionKind::Unknown("tool_result:error".into()),
                                &msg,
                            );
                        } else {
                            let line = format!("({n} bytes returned)", n = content.len());
                            let _ = writer.append_action(ActionKind::ToolResult, &line);
                        }
                    }
                }
            }
        }
        JsonEvent::Result { final_text, .. } => {
            let _ = writer.set_final_answer(final_text);
        }
        JsonEvent::Unknown { event_type, raw } => {
            let body = serde_json::to_string(&raw).unwrap_or_default();
            let _ = writer.append_action(ActionKind::Unknown(event_type), &body);
        }
    }
}

/// Wrap assistant text at ~80 columns on whitespace boundaries; long
/// single-line runs (URLs, code) get returned as a single chunk to avoid
/// mid-token splits.
fn wrap_assistant_text(text: &str) -> Vec<String> {
    const WIDTH: usize = 80;
    let mut out: Vec<String> = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in para.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.len() + 1 + word.len() <= WIDTH {
                current.push(' ');
                current.push_str(word);
            } else {
                out.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Format a `tool_input` JSON value into a one-line summary suitable for a
/// `[tool_use]` log line. Truncates at ~200 chars to keep the log readable.
fn format_tool_input_summary(input: &serde_json::Value) -> String {
    let raw = match input {
        serde_json::Value::Object(map) => {
            if let Some(p) = map.get("file_path").and_then(|v| v.as_str()) {
                p.to_string()
            } else if let Some(p) = map.get("path").and_then(|v| v.as_str()) {
                p.to_string()
            } else if let Some(c) = map.get("command").and_then(|v| v.as_str()) {
                c.to_string()
            } else if let Some(p) = map.get("pattern").and_then(|v| v.as_str()) {
                p.to_string()
            } else {
                serde_json::to_string(input).unwrap_or_default()
            }
        }
        _ => serde_json::to_string(input).unwrap_or_default(),
    };
    if raw.chars().count() > 200 {
        let mut truncated: String = raw.chars().take(200).collect();
        truncated.push('…');
        truncated
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CliKind, LlmProvider};
    use std::collections::HashMap;

    /// Env vars explicitly set on the command via `.env()`.
    fn envs(cmd: &Command) -> HashMap<String, String> {
        cmd.as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect()
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Regression: the OS-sandbox `--settings` file must default to the run
    /// workspace's `.git` dir (reachable inside the sandbox, but never staged,
    /// reported, or cleaned by git), never the host temp dir. Under the OS
    /// sandbox the child gets a private `/tmp`, so a settings file in the host
    /// temp dir is invisible in the namespace and the CLI dies with "Settings
    /// file not found".
    #[test]
    fn sandbox_settings_dir_defaults_to_git_dir_not_temp() {
        let workspace = Path::new("/cache/workspaces/example");
        // No override → the workspace's `.git` (bound into the child namespace).
        assert_eq!(sandbox_settings_dir(None, workspace), workspace.join(".git"));
        // And explicitly NOT the host temp dir that caused the original bug.
        assert_ne!(sandbox_settings_dir(None, workspace), std::env::temp_dir());
        // An explicit override (the per-test TempDir path) still wins.
        let override_dir = Path::new("/tmp/per-test-override");
        assert_eq!(
            sandbox_settings_dir(Some(override_dir), workspace).as_path(),
            override_dir
        );
    }

    /// Regression: the daemon control socket is threaded into the sandbox plan
    /// (so the MCP relay can reach it) when configured, and omitted otherwise.
    #[test]
    fn control_socket_bind_resolves_from_env_value() {
        assert_eq!(
            control_socket_from_env(Some("/tmp/1000-runtime/autocoder/control.sock".to_string())),
            Some(PathBuf::from("/tmp/1000-runtime/autocoder/control.sock"))
        );
        // Unset or empty → no bind added.
        assert_eq!(control_socket_from_env(None), None);
        assert_eq!(control_socket_from_env(Some(String::new())), None);
    }

    fn ctx<'a>(
        settings: &'a Path,
        allowed: &'a [String],
        include_autocoder_tools: bool,
        emit_stream_json: bool,
        resume: Option<&'a str>,
    ) -> BuildContext<'a> {
        BuildContext {
            settings_path: settings,
            allowed_tools: allowed,
            include_autocoder_tools,
            emit_stream_json,
            resume_session_id: resume,
            workspace: Path::new("/tmp"),
            mcp_role: None,
            model: None,
        }
    }

    // 5.3: no model → none of the ANTHROPIC_* vars set.
    #[test]
    fn claude_strategy_no_model_sets_no_anthropic_env() {
        let strat = ClaudeStrategy::new("claude".into(), Vec::new());
        let allowed = vec!["Read".to_string()];
        let mut cmd = strat.build_command(&ctx(
            Path::new("/tmp/s.json"),
            &allowed,
            false,
            false,
            None,
        ));
        strat.apply_model_selection(&mut cmd, None);
        let e = envs(&cmd);
        assert!(!e.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!e.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(!e.contains_key("ANTHROPIC_MODEL"));
    }

    // a003 / task 3.2: a resolved model sets the endpoint + model env
    // (ANTHROPIC_BASE_URL / ANTHROPIC_MODEL) but NO ANTHROPIC_AUTH_TOKEN —
    // the api_key is a credential the subprocess never receives. claude
    // authenticates from its own login. (Supersedes a56's 5.3, which set all
    // three.)
    #[test]
    fn claude_strategy_without_key_sets_endpoint_and_model_no_credential() {
        let strat = ClaudeStrategy::new("claude".into(), Vec::new());
        let model = ResolvedModel {
            provider: LlmProvider::Anthropic,
            model: "claude-opus-4-8".into(),
            api_base_url: "https://example.invalid/api".into(),
            api_key: String::new(), // no key → claude uses its own login
        };
        let allowed: Vec<String> = vec![];
        let mut cmd = strat.build_command(&ctx(
            Path::new("/tmp/s.json"),
            &allowed,
            false,
            false,
            None,
        ));
        strat.apply_model_selection(&mut cmd, Some(&model));
        let e = envs(&cmd);
        assert_eq!(
            e.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://example.invalid/api")
        );
        assert_eq!(e.get("ANTHROPIC_MODEL").map(String::as_str), Some("claude-opus-4-8"));
        // No key supplied → no credential reaches the subprocess (the safe,
        // no-exposure default; claude authenticates from its own login/store).
        assert!(
            !e.contains_key("ANTHROPIC_API_KEY"),
            "no key → no ANTHROPIC_API_KEY"
        );
        assert!(!e.contains_key("ANTHROPIC_AUTH_TOKEN"));
    }

    // The claude strategy reproduces the pre-refactor executor streaming
    // invocation exactly (the "byte-identical command" scenario).
    #[test]
    fn claude_strategy_reproduces_streaming_invocation() {
        let strat = ClaudeStrategy::new("claude".into(), Vec::new());
        let allowed = vec!["Read".to_string(), "Write".to_string()];
        let cmd = strat.build_command(&ctx(
            Path::new("/tmp/s.json"),
            &allowed,
            true,
            true,
            None,
        ));
        let combined = build_allowed_tools_value(&allowed, true);
        assert_eq!(cmd.as_std().get_program().to_string_lossy(), "claude");
        assert_eq!(
            args(&cmd),
            vec![
                "--settings".to_string(),
                "/tmp/s.json".into(),
                "--allowedTools".into(),
                combined,
                "--permission-mode".into(),
                "acceptEdits".into(),
                "--verbose".into(),
                "--output-format".into(),
                "stream-json".into(),
            ]
        );
    }

    // The recovery invocation: `--resume` after `acceptEdits`, before the
    // stream-json flags, AND no auto-appended autocoder MCP tools.
    #[test]
    fn claude_strategy_recovery_invocation_has_resume_and_plain_allowed_tools() {
        let strat = ClaudeStrategy::new("claude".into(), Vec::new());
        let allowed = vec!["Read".to_string()];
        let cmd = strat.build_command(&ctx(
            Path::new("/tmp/s.json"),
            &allowed,
            false,
            true,
            Some("sess-123"),
        ));
        let a = args(&cmd);
        let pos = |s: &str| a.iter().position(|x| x == s).expect("arg present");
        assert!(pos("acceptEdits") < pos("--resume"));
        assert!(pos("--resume") < pos("--verbose"));
        assert_eq!(a[pos("--resume") + 1], "sess-123");
        // Plain join — the autocoder MCP tools are NOT appended in recovery.
        assert_eq!(a[pos("--allowedTools") + 1], "Read");
    }

    // Anthropic resolves to the claude strategy.
    #[test]
    fn strategy_for_provider_anthropic_resolves_claude() {
        assert!(strategy_for_provider(LlmProvider::Anthropic, "claude".into(), Vec::new()).is_ok());
    }

    // a60 / task 4.1: the non-Anthropic providers resolve (via a55's
    // `provider → CLI` rule) to a working `OpencodeStrategy` — NOT the
    // pre-a60 "no registered strategy" error — AND it builds an `opencode
    // run` invocation.
    #[test]
    fn strategy_for_provider_non_claude_resolves_opencode() {
        for p in [LlmProvider::OpenAiCompatible, LlmProvider::Ollama] {
            let strat = strategy_for_provider(p, "opencode".into(), Vec::new())
                .expect("non-anthropic provider resolves to the opencode strategy (a60)");
            let allowed = vec!["Read".to_string()];
            let tmp = tempfile::tempdir().unwrap();
            let bctx = BuildContext {
                workspace: tmp.path(),
                ..ctx(Path::new("/tmp/s.json"), &allowed, false, false, None)
            };
            let cmd = strat.build_command(&bctx);
            assert_eq!(cmd.as_std().get_program().to_string_lossy(), "opencode");
            assert_eq!(args(&cmd), vec!["run".to_string()]);
        }
    }

    // a60 / task 4.1: explicit `cli: opencode` (registry override) resolves
    // to the opencode strategy.
    #[test]
    fn strategy_for_cli_opencode_resolves() {
        assert!(strategy_for_cli(CliKind::Opencode, "opencode".into(), Vec::new()).is_ok());
    }

    #[test]
    fn build_allowed_tools_value_appends_mcp_tools_only_when_requested() {
        let allowed = vec!["Read".to_string(), "Edit".to_string()];
        let plain = build_allowed_tools_value(&allowed, false);
        assert_eq!(plain, "Read,Edit");
        let with_mcp = build_allowed_tools_value(&allowed, true);
        assert!(with_mcp.starts_with("Read,Edit,"));
        for tool in crate::mcp_askuser_server::PROVIDED_TOOL_NAMES {
            assert!(
                with_mcp.contains(&crate::mcp_askuser_server::qualified_tool_name(tool)),
                "{tool} must be auto-appended: {with_mcp}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // a60: OpencodeStrategy.
    // -----------------------------------------------------------------------

    fn read_opencode_json(workspace: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(workspace.join("opencode.json"))
            .expect("opencode.json was written");
        serde_json::from_str(&raw).expect("opencode.json is valid JSON")
    }

    // a60 / task 4.2: the strategy writes `opencode.json` with the `mcp`
    // block (`type: local`, the MCP-child command, env incl. ORCH_MCP_ROLE)
    // AND writes NO `.mcp.json`.
    #[test]
    fn opencode_strategy_writes_opencode_json_with_mcp_block_and_no_dot_mcp_json() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()];
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = OpencodeStrategy::new("opencode".into(), Vec::new());
        let _ = strat.build_command(&bctx);

        assert!(
            tmp.path().join("opencode.json").exists(),
            "opencode.json must be written into the workspace"
        );
        assert!(
            !tmp.path().join(".mcp.json").exists(),
            "the opencode strategy must NOT write .mcp.json (that is the claude format)"
        );

        let v = read_opencode_json(tmp.path());
        let server = &v["mcp"][crate::mcp_askuser_server::SERVER_NAME];
        assert_eq!(server["type"], "local");
        assert_eq!(server["enabled"], true);
        let command = server["command"].as_array().expect("command is an array");
        assert_eq!(
            command.last().and_then(|v| v.as_str()),
            Some("mcp-ask-user-server"),
            "MCP child launches the autocoder mcp-ask-user-server subcommand"
        );
        let env = &server["environment"];
        assert_eq!(env[crate::mcp_askuser_server::ENV_ROLE], "reviewer");
        assert_eq!(env[crate::mcp_askuser_server::ENV_CHANGE], "reviewer");
        assert!(
            env[crate::mcp_askuser_server::ENV_WORKSPACE].is_string(),
            "the workspace env var is always written"
        );
    }

    // a60 / task 4.2: with no role, no submission env is advertised (no
    // ORCH_MCP_ROLE / ORCH_MCP_CHANGE).
    #[test]
    fn opencode_strategy_omits_role_env_when_no_role() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string()];
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: None,
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = OpencodeStrategy::new("opencode".into(), Vec::new());
        let _ = strat.build_command(&bctx);

        let v = read_opencode_json(tmp.path());
        let env = &v["mcp"][crate::mcp_askuser_server::SERVER_NAME]["environment"];
        assert!(env.get(crate::mcp_askuser_server::ENV_ROLE).is_none());
        assert!(env.get(crate::mcp_askuser_server::ENV_CHANGE).is_none());
    }

    // a60 / task 4.3 + a003 / task 3.1: model selection targets the configured
    // provider — `--model <provider>/<model>` + the opencode.json provider
    // entry (model + base URL) — AND sets none of the ANTHROPIC_* env vars.
    // a003: the keyed model's `api_key` is NEVER written into opencode.json
    // (the provider `options` carry the base URL but no `apiKey`), and the key
    // value appears nowhere in the file.
    #[test]
    fn opencode_strategy_without_key_writes_no_api_key() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string()];
        let model = ResolvedModel {
            provider: LlmProvider::OpenAiCompatible,
            model: "gpt-4o-mini".into(),
            api_base_url: "https://api.example.invalid/v1".into(),
            api_key: String::new(), // no key → opencode uses its own auth
        };
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            model: Some(&model),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = OpencodeStrategy::new("opencode".into(), Vec::new());
        let mut cmd = strat.build_command(&bctx);
        strat.apply_model_selection(&mut cmd, Some(&model));

        let a = args(&cmd);
        let pos = a.iter().position(|x| x == "--model").expect("--model present");
        assert_eq!(a[pos + 1], "openai_compatible/gpt-4o-mini");

        let v = read_opencode_json(tmp.path());
        // The MCP, permission, and provider-base-URL blocks are all present.
        assert!(v["mcp"][crate::mcp_askuser_server::SERVER_NAME].is_object());
        assert!(v["permission"].is_object());
        let provider = &v["provider"]["openai_compatible"];
        assert_eq!(provider["options"]["baseURL"], "https://api.example.invalid/v1");
        // No key supplied → no `apiKey` reference at all (opencode self-auths).
        assert!(
            provider["options"].get("apiKey").is_none(),
            "no key → no apiKey in opencode.json"
        );
        assert!(
            provider["models"]["gpt-4o-mini"].is_object(),
            "the resolved model is registered under the provider"
        );

        let e = envs(&cmd);
        assert!(
            !e.contains_key("AUTOCODER_OPENCODE_API_KEY"),
            "no key → no key env var"
        );
        assert!(!e.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!e.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(!e.contains_key("ANTHROPIC_MODEL"));
    }

    // a60 / task 4.3: an Ollama model (no api key) omits the apiKey option.
    #[test]
    fn opencode_strategy_ollama_provider_omits_api_key() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string()];
        let model = ResolvedModel {
            provider: LlmProvider::Ollama,
            model: "qwen2.5-coder".into(),
            api_base_url: "http://localhost:11434".into(),
            api_key: String::new(),
        };
        let bctx = BuildContext {
            workspace: tmp.path(),
            model: Some(&model),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = OpencodeStrategy::new("opencode".into(), Vec::new());
        let mut cmd = strat.build_command(&bctx);
        strat.apply_model_selection(&mut cmd, Some(&model));

        let a = args(&cmd);
        let pos = a.iter().position(|x| x == "--model").expect("--model present");
        assert_eq!(a[pos + 1], "ollama/qwen2.5-coder");

        let v = read_opencode_json(tmp.path());
        let options = &v["provider"]["ollama"]["options"];
        assert_eq!(options["baseURL"], "http://localhost:11434");
        assert!(
            options.get("apiKey").is_none(),
            "ollama does not authenticate; apiKey must be omitted"
        );
    }

    // a60 / task 4.4: a read-only role's Write/Edit/Bash are denied via the
    // generated permission config; the role's MCP tool is still exposed.
    #[test]
    fn opencode_strategy_readonly_denies_write_edit_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()];
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = OpencodeStrategy::new("opencode".into(), Vec::new());
        let _ = strat.build_command(&bctx);

        let v = read_opencode_json(tmp.path());
        let perm = &v["permission"];
        assert_eq!(perm["edit"], "deny", "Write/Edit denied for a read-only role");
        assert_eq!(perm["bash"], "deny", "Bash denied for a read-only role");
        assert_eq!(perm["webfetch"], "deny");
        // The role's submission tool stays reachable via the mcp block.
        assert_eq!(
            v["mcp"][crate::mcp_askuser_server::SERVER_NAME]["environment"]
                [crate::mcp_askuser_server::ENV_ROLE],
            "reviewer"
        );
    }

    // a60 / task 4.4 (converse): a write-enabled sandbox allows edit + bash.
    #[test]
    fn opencode_strategy_write_sandbox_allows_edit_and_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = crate::config::default_allowed_tools();
        let bctx = BuildContext {
            workspace: tmp.path(),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = OpencodeStrategy::new("opencode".into(), Vec::new());
        let _ = strat.build_command(&bctx);

        let v = read_opencode_json(tmp.path());
        assert_eq!(v["permission"]["edit"], "allow");
        assert_eq!(v["permission"]["bash"], "allow");
    }

    // a60 / task 4.5: an opencode role runs through `agentic_run` in capture
    // mode — stdout/stderr read at exit, NO streaming-JSON parse (no
    // final_answer / session_id / structured log).
    #[tokio::test]
    async fn opencode_role_runs_through_agentic_run_in_capture_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // Stub `opencode`: drain stdin (the piped prompt), print a line,
        // exit 0. Stands in for the real binary so the capture path runs.
        let stub = tmp.path().join("opencode_stub.sh");
        std::fs::write(&stub, "#!/bin/sh\ncat >/dev/null\necho 'opencode stub done'\n").unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let strat = OpencodeStrategy::new(stub.to_string_lossy().into_owned(), Vec::new());
        let outcome = agentic_run(AgenticRunOpts {
            workspace: tmp.path(),
            change: "reviewer",
            strategy: &strat,
            prompt: "review this change",
            sandbox: SandboxConfig {
                allowed_tools: vec!["Read".to_string()],
                disallowed_bash_patterns: Vec::new(),
                disallowed_read_paths: Vec::new(),
                deny_writes: true,
            },
            model: None,
            output_mode: OutputMode::Capture,
            timeout: std::time::Duration::from_secs(30),
            paths: None,
            settings_dir: Some(tmp.path()),
            include_autocoder_tools: true,
            emit_stream_json_in_capture: false,
            resume_session_id: None,
            track_subprocess_marker: false,
            etxtbsy_retry_spawn: false,
            // Unenforced: this test exercises the inner capture path, not the
            // OS layer (no mechanism is runnable in CI).
            os_sandbox: crate::sandbox::RunSandbox::default(),
        })
        .await
        .expect("agentic_run completes for the opencode stub");

        assert!(!outcome.timed_out);
        assert!(
            outcome.stdout.contains("opencode stub done"),
            "capture mode reads stdout at exit: {:?}",
            outcome.stdout
        );
        assert!(
            outcome.final_answer.is_none(),
            "capture mode does NOT parse a streaming-JSON final_answer"
        );
        assert!(
            outcome.session_id.is_none(),
            "capture mode does NOT parse a streaming-JSON session_id"
        );
        assert!(
            !outcome.streamed_log,
            "capture mode does NOT write the streaming structured log"
        );
        // The strategy wrote opencode.json (not .mcp.json) for the run.
        assert!(tmp.path().join("opencode.json").exists());
        assert!(!tmp.path().join(".mcp.json").exists());
    }

    // -----------------------------------------------------------------------
    // a69: AntigravityStrategy (the `agy` CLI).
    // -----------------------------------------------------------------------

    fn read_antigravity_mcp_config(workspace: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(workspace.join("mcp_config.json"))
            .expect("mcp_config.json was written");
        serde_json::from_str(&raw).expect("mcp_config.json is valid JSON")
    }

    // a69 / task 4.1: a Google-provider model (a55's `provider → CLI` rule)
    // AND an explicit `cli: antigravity` both resolve to AntigravityStrategy —
    // NOT a "no registered strategy" error — AND it builds `agy -p ""`
    // selecting the model via `--model` (default `gemini-3-pro`).
    #[test]
    fn strategy_for_provider_google_resolves_antigravity() {
        // a55 provider path: default_cli_for(Google) == Antigravity.
        let strat = strategy_for_provider(LlmProvider::Google, "agy".into(), Vec::new())
            .expect("Google provider resolves to the antigravity strategy (a69)");
        // Explicit `cli: antigravity` path.
        assert!(strategy_for_cli(CliKind::Antigravity, "agy".into(), Vec::new()).is_ok());

        let allowed = vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()];
        let tmp = tempfile::tempdir().unwrap();
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let mut cmd = strat.build_command(&bctx);
        strat.apply_model_selection(&mut cmd, None);
        assert_eq!(cmd.as_std().get_program().to_string_lossy(), "agy");
        let a = args(&cmd);
        // `-p ""` print mode (the prompt arrives on stdin), `--model
        // gemini-3-pro` (default), `--sandbox` (read-only role).
        let p = a.iter().position(|x| x == "-p").expect("-p present");
        assert_eq!(a[p + 1], "", "the -p value is empty; the prompt is piped on stdin");
        let m = a.iter().position(|x| x == "--model").expect("--model present");
        assert_eq!(a[m + 1], "gemini-3-pro", "default model when none is resolved");
        assert!(a.iter().any(|x| x == "--sandbox"), "a read-only role gets --sandbox");
    }

    // a69 / task 4.1 (model selection): a resolved model is selected via
    // `--model <model>`.
    #[test]
    fn antigravity_apply_model_selection_uses_resolved_model() {
        let strat = AntigravityStrategy::new("agy".into(), Vec::new());
        let model = ResolvedModel {
            provider: LlmProvider::Google,
            model: "gemini-3-pro-preview".into(),
            api_base_url: String::new(),
            api_key: String::new(),
        };
        let mut cmd = Command::new("agy");
        strat.apply_model_selection(&mut cmd, Some(&model));
        let a = args(&cmd);
        let m = a.iter().position(|x| x == "--model").expect("--model present");
        assert_eq!(a[m + 1], "gemini-3-pro-preview");
        // No key configured → no AV_API_KEY (agy uses its OAuth login store).
        assert!(!envs(&cmd).contains_key("AV_API_KEY"));
    }

    // a69 / task 4.2: writes `mcp_config.json` with the `mcpServers` entry
    // (command/args + env incl. ORCH_MCP_ROLE, local stdio) AND writes NEITHER
    // `.mcp.json` (claude) NOR `opencode.json` (opencode).
    #[test]
    fn antigravity_writes_mcp_config_with_role_env_and_no_other_config() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()];
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = AntigravityStrategy::new("agy".into(), Vec::new());
        let _ = strat.build_command(&bctx);

        assert!(tmp.path().join("mcp_config.json").exists());
        assert!(!tmp.path().join(".mcp.json").exists(), "no claude .mcp.json");
        assert!(!tmp.path().join("opencode.json").exists(), "no opencode.json");

        let v = read_antigravity_mcp_config(tmp.path());
        let server = &v["mcpServers"][crate::mcp_askuser_server::SERVER_NAME];
        assert!(server["command"].is_string(), "command is a string path (local stdio)");
        let cmdargs = server["args"].as_array().expect("args is an array");
        assert_eq!(
            cmdargs.last().and_then(|v| v.as_str()),
            Some("mcp-ask-user-server"),
            "MCP child launches the autocoder mcp-ask-user-server subcommand"
        );
        let env = &server["env"];
        assert_eq!(env[crate::mcp_askuser_server::ENV_ROLE], "reviewer");
        assert_eq!(env[crate::mcp_askuser_server::ENV_CHANGE], "reviewer");
        assert!(env[crate::mcp_askuser_server::ENV_WORKSPACE].is_string());
    }

    // a69 / task 4.2 (no role): with no role, no submission env is advertised.
    #[test]
    fn antigravity_omits_role_env_when_no_role() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string()];
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: None,
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = AntigravityStrategy::new("agy".into(), Vec::new());
        let _ = strat.build_command(&bctx);
        let v = read_antigravity_mcp_config(tmp.path());
        let env = &v["mcpServers"][crate::mcp_askuser_server::SERVER_NAME]["env"];
        assert!(env.get(crate::mcp_askuser_server::ENV_ROLE).is_none());
        assert!(env.get(crate::mcp_askuser_server::ENV_CHANGE).is_none());
    }

    // a69 / task 4.3: a resolved model with a key sets `AV_API_KEY` AND NONE
    // of the `ANTHROPIC_*` env vars; the key never lands in any workspace file.
    #[test]
    fn antigravity_auth_env_av_api_key_no_anthropic() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string()];
        let model = ResolvedModel {
            provider: LlmProvider::Google,
            model: "gemini-3-pro".into(),
            api_base_url: String::new(),
            api_key: "av-secret-sentinel".into(),
        };
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            model: Some(&model),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = AntigravityStrategy::new("agy".into(), Vec::new());
        let mut cmd = strat.build_command(&bctx);
        strat.apply_model_selection(&mut cmd, Some(&model));
        let e = envs(&cmd);
        assert_eq!(
            e.get("AV_API_KEY").map(String::as_str),
            Some("av-secret-sentinel")
        );
        assert!(!e.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!e.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(!e.contains_key("ANTHROPIC_MODEL"));
        // a003: the key is NEVER written into any workspace file (mcp_config /
        // settings carry no credential).
        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let raw = std::fs::read_to_string(&path).unwrap_or_default();
                assert!(
                    !raw.contains("av-secret-sentinel"),
                    "a003: the api_key leaked into workspace file {}",
                    path.display()
                );
            }
        }
    }

    // a69 / task 4.4: a read-only role's generated tool restriction allows the
    // read tools + the role's `submit_*` tool AND denies shell/write/edit.
    #[test]
    fn antigravity_readonly_restriction_allows_read_and_submit_denies_write_edit_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()];
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = AntigravityStrategy::new("agy".into(), Vec::new());
        let _ = strat.build_command(&bctx);

        let raw = std::fs::read_to_string(tmp.path().join("agy_settings.json"))
            .expect("agy_settings.json written for a read-only role");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let allow: Vec<String> = v["permissions"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        for t in ["Read", "Glob", "Grep"] {
            assert!(allow.iter().any(|a| a == t), "read tool {t} allowed: {allow:?}");
        }
        let submit = crate::mcp_askuser_server::qualified_tool_name(
            crate::mcp_askuser_server::submission_tool_name_for_role("reviewer").unwrap(),
        );
        assert!(
            allow.contains(&submit),
            "the role's submit_* tool {submit} is allowed: {allow:?}"
        );
        let deny: Vec<String> = v["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        for t in ["Write", "Edit", "Bash"] {
            assert!(deny.iter().any(|d| d == t), "shell/write/edit tool {t} denied: {deny:?}");
        }
    }

    // a69 / task 4.4 (converse): a write-enabled role gets no `--sandbox` and
    // writes no read-only settings file (it may write/edit/run).
    #[test]
    fn antigravity_write_role_no_sandbox_no_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = crate::config::default_allowed_tools();
        let bctx = BuildContext {
            workspace: tmp.path(),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let strat = AntigravityStrategy::new("agy".into(), Vec::new());
        let cmd = strat.build_command(&bctx);
        assert!(
            !args(&cmd).iter().any(|x| x == "--sandbox"),
            "a write-enabled role gets no --sandbox"
        );
        assert!(!tmp.path().join("agy_settings.json").exists());
    }

    // a69 (spec scenario "Capture mode only"): an agy role runs through
    // `agentic_run` in capture mode — stdout read at exit, NO streaming-JSON
    // parse (no final_answer / session_id / structured log).
    #[tokio::test]
    async fn antigravity_role_runs_through_agentic_run_in_capture_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join("agy_stub.sh");
        std::fs::write(&stub, "#!/bin/sh\ncat >/dev/null\necho 'agy stub done'\n").unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let strat = AntigravityStrategy::new(stub.to_string_lossy().into_owned(), Vec::new());
        let outcome = agentic_run(AgenticRunOpts {
            workspace: tmp.path(),
            change: "reviewer",
            strategy: &strat,
            prompt: "review this change",
            sandbox: SandboxConfig {
                allowed_tools: vec!["Read".to_string()],
                disallowed_bash_patterns: Vec::new(),
                disallowed_read_paths: Vec::new(),
                deny_writes: true,
            },
            model: None,
            output_mode: OutputMode::Capture,
            timeout: std::time::Duration::from_secs(30),
            paths: None,
            settings_dir: Some(tmp.path()),
            include_autocoder_tools: true,
            emit_stream_json_in_capture: false,
            resume_session_id: None,
            track_subprocess_marker: false,
            etxtbsy_retry_spawn: false,
            os_sandbox: crate::sandbox::RunSandbox::default(),
        })
        .await
        .expect("agentic_run completes for the agy stub");

        assert!(!outcome.timed_out);
        assert!(
            outcome.stdout.contains("agy stub done"),
            "capture mode reads stdout at exit: {:?}",
            outcome.stdout
        );
        assert!(outcome.final_answer.is_none(), "capture mode parses no final_answer");
        assert!(outcome.session_id.is_none(), "capture mode parses no session_id");
        assert!(!outcome.streamed_log, "capture mode writes no streaming log");
        // The agy strategy wrote mcp_config.json — NOT .mcp.json / opencode.json.
        assert!(tmp.path().join("mcp_config.json").exists());
        assert!(!tmp.path().join(".mcp.json").exists());
        assert!(!tmp.path().join("opencode.json").exists());
    }

    // a69 / task 4.5: a read-only agy run that escapes a write (non-empty
    // post-run `git status --porcelain`) is caught by the `WritePolicy::None`
    // backstop — `detect_write_policy_violation` flags it (the run fails) AND
    // `git reset --hard HEAD` + `git clean -fd` revert it. CLI-agnostic: the
    // enforcement runs on git status after ANY strategy's run.
    #[tokio::test]
    async fn antigravity_escaped_write_caught_by_write_policy_none_backstop() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command as StdCommand;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // A committed baseline so `git status` is clean before the run.
        let git = |args: &[&str]| {
            StdCommand::new("git")
                .args(args)
                .current_dir(ws)
                .output()
                .expect("git runs")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t.t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(ws.join("baseline.txt"), "baseline\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "baseline"]);

        // An agy stub standing in for a read-only role that nonetheless writes
        // a stray file (the synthetic escaped write the spike probes).
        let stub = ws.join("agy_stub.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\ncat >/dev/null\necho INTRUDER > escaped.txt\necho done\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let strat = AntigravityStrategy::new(stub.to_string_lossy().into_owned(), Vec::new());
        let _ = agentic_run(AgenticRunOpts {
            workspace: ws,
            change: "reviewer",
            strategy: &strat,
            prompt: "review",
            sandbox: SandboxConfig {
                allowed_tools: vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()],
                disallowed_bash_patterns: Vec::new(),
                disallowed_read_paths: Vec::new(),
                deny_writes: true,
            },
            model: None,
            output_mode: OutputMode::Capture,
            timeout: std::time::Duration::from_secs(30),
            paths: None,
            settings_dir: Some(ws),
            include_autocoder_tools: true,
            emit_stream_json_in_capture: false,
            resume_session_id: None,
            track_subprocess_marker: false,
            etxtbsy_retry_spawn: false,
            os_sandbox: crate::sandbox::RunSandbox::default(),
        })
        .await
        .expect("agentic_run completes for the agy stub");

        // The escaped write landed.
        assert!(ws.join("escaped.txt").exists(), "the stub wrote escaped.txt");
        let entries = crate::git::status_entries(ws).expect("git status");
        // WritePolicy::None enforcement: a non-empty status is a violation
        // (the run fails) regardless of which CLI produced it.
        assert!(
            crate::audits::scheduler::detect_write_policy_violation(
                crate::audits::WritePolicy::None,
                &entries,
            )
            .is_some(),
            "a non-empty post-run status must trip the WritePolicy::None violation (run failure)"
        );
        // The revert the enforcement performs.
        crate::git::reset_hard_head(ws).unwrap();
        crate::git::clean_force(ws).unwrap();
        let porcelain = crate::git::status_porcelain(ws).unwrap();
        assert!(
            porcelain.trim().is_empty(),
            "the escaped write must be reverted; got: {porcelain}"
        );
        assert!(
            !ws.join("escaped.txt").exists(),
            "escaped.txt must not persist into the workspace"
        );
    }

    // -----------------------------------------------------------------------
    // a003: credentials never reach the model.
    // -----------------------------------------------------------------------

    /// The sentinel a strategy has no legitimate reason to ever emit.
    const KEY_SENTINEL: &str = "SENTINEL-API-KEY-MUST-NOT-LEAK-9f3c";

    /// Build a keyed [`ResolvedModel`] for `provider` carrying [`KEY_SENTINEL`].
    fn sentinel_model(provider: LlmProvider) -> ResolvedModel {
        ResolvedModel {
            provider,
            model: "the-model".into(),
            api_base_url: "https://api.example.invalid/v1".into(),
            api_key: KEY_SENTINEL.into(),
        }
    }

    /// Drive one strategy with the keyed model AND assert the sentinel appears
    /// in NO file the strategy wrote into the workspace. A supplied key MAY
    /// reach the subprocess env (the documented opt-in residual), but it must
    /// NEVER be written raw into a committable workspace file (opencode.json
    /// holds only an `{env:...}` reference).
    fn assert_no_raw_key_in_workspace_file(strat: &dyn CliStrategy, provider: LlmProvider) {
        let tmp = tempfile::tempdir().unwrap();
        let model = sentinel_model(provider);
        let allowed = vec!["Read".to_string()];
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            model: Some(&model),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let mut cmd = strat.build_command(&bctx);
        strat.apply_model_selection(&mut cmd, Some(&model));

        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let raw = std::fs::read_to_string(&path).unwrap_or_default();
                assert!(
                    !raw.contains(KEY_SENTINEL),
                    "workspace file {} leaked the RAW api_key for provider {provider:?}",
                    path.display()
                );
            }
        }
    }

    // Across EVERY registered CliStrategy, no file written into the workspace
    // carries the RAW api_key (claude passes it via env; opencode.json holds an
    // `{env:...}` reference, never the secret).
    #[test]
    fn no_strategy_writes_raw_key_to_workspace_file() {
        assert_no_raw_key_in_workspace_file(
            &ClaudeStrategy::new("claude".into(), Vec::new()),
            LlmProvider::Anthropic,
        );
        assert_no_raw_key_in_workspace_file(
            &OpencodeStrategy::new("opencode".into(), Vec::new()),
            LlmProvider::OpenAiCompatible,
        );
    }

    // A CLI role configured with an api_key produces exactly one startup WARN
    // (the key is passed to the CLI AND readable by the sandboxed model — an
    // opt-in exposure) AND the strategy passes the key. No key → no WARN.
    #[test]
    fn cli_role_with_key_warns_exposure_and_strategy_passes_it() {
        // With a key: exactly one WARN, naming the role AND the exposure.
        let role = "executor.change_internal_contradiction_check_llm";
        let msg = cli_role_key_exposure_warning(role, true)
            .expect("a keyed CLI role must produce exactly one WARN");
        assert!(msg.contains(role), "the WARN names the role: {msg}");
        assert!(
            msg.to_ascii_lowercase().contains("exposure")
                || msg.to_ascii_lowercase().contains("read the key"),
            "the WARN explains the exposure: {msg}"
        );
        assert!(msg.contains("api_key"), "the WARN names the field: {msg}");

        // No key → no WARN (the no-exposure default).
        assert!(
            cli_role_key_exposure_warning(role, false).is_none(),
            "a role with no configured key must not warn"
        );

        // The claude strategy now PASSES a supplied key via ANTHROPIC_API_KEY
        // (NOT the legacy ANTHROPIC_AUTH_TOKEN).
        let strat = ClaudeStrategy::new("claude".into(), Vec::new());
        let model = sentinel_model(LlmProvider::Anthropic);
        let allowed: Vec<String> = vec![];
        let mut cmd =
            strat.build_command(&ctx(Path::new("/tmp/s.json"), &allowed, false, false, None));
        strat.apply_model_selection(&mut cmd, Some(&model));
        let e = envs(&cmd);
        assert_eq!(
            e.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some(KEY_SENTINEL),
            "claude passes a supplied key via ANTHROPIC_API_KEY"
        );
        assert!(!e.contains_key("ANTHROPIC_AUTH_TOKEN"));
    }

    // The opencode strategy references a supplied key via `{env:...}` in
    // opencode.json (NEVER the raw secret) AND sets the secret on the subprocess
    // env, where opencode interpolates it (verified live by the bogus-key probe).
    #[test]
    fn opencode_passes_supplied_key_via_env_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let strat = OpencodeStrategy::new("opencode".into(), Vec::new());
        let model = sentinel_model(LlmProvider::OpenAiCompatible);
        let allowed = vec!["Read".to_string()];
        let bctx = BuildContext {
            workspace: tmp.path(),
            mcp_role: Some("reviewer"),
            model: Some(&model),
            ..ctx(Path::new("/tmp/s.json"), &allowed, true, false, None)
        };
        let mut cmd = strat.build_command(&bctx);
        strat.apply_model_selection(&mut cmd, Some(&model));

        let oc = std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap();
        assert!(
            oc.contains("{env:AUTOCODER_OPENCODE_API_KEY}"),
            "opencode.json carries the env reference: {oc}"
        );
        assert!(
            !oc.contains(KEY_SENTINEL),
            "opencode.json must NOT carry the raw secret: {oc}"
        );
        assert_eq!(
            envs(&cmd)
                .get("AUTOCODER_OPENCODE_API_KEY")
                .map(String::as_str),
            Some(KEY_SENTINEL),
            "the secret rides the subprocess env"
        );
    }

    // -----------------------------------------------------------------------
    // a70: session resume + scoped delete + single-shot prune.
    // -----------------------------------------------------------------------

    /// The claude project-hash maps every non-[alnum-] char to `-` (so `/`,
    /// `.`, `_` all collapse), matching the `~/.claude/projects/` directory
    /// naming the integration spike observed against a live store.
    #[test]
    fn claude_project_hash_collapses_non_alnum() {
        assert_eq!(
            claude_project_hash(Path::new("/home/u/.cache/ws/github_com_x-y")),
            "-home-u--cache-ws-github-com-x-y"
        );
    }

    /// a70 §2.1: each strategy's native headless-resume injects its own flag
    /// (`claude --resume`, `opencode --session`, `agy --conversation`).
    #[test]
    fn strategies_apply_native_resume_flag() {
        let mut c = Command::new("claude");
        assert!(ClaudeStrategy::new("claude".into(), vec![]).apply_resume(&mut c, "sid"));
        let a = args(&c);
        assert_eq!(a[a.iter().position(|x| x == "--resume").unwrap() + 1], "sid");

        let mut o = Command::new("opencode");
        assert!(OpencodeStrategy::new("opencode".into(), vec![]).apply_resume(&mut o, "osid"));
        let a = args(&o);
        assert_eq!(a[a.iter().position(|x| x == "--session").unwrap() + 1], "osid");

        let mut g = Command::new("agy");
        assert!(AntigravityStrategy::new("agy".into(), vec![]).apply_resume(&mut g, "gsid"));
        let a = args(&g);
        assert_eq!(a[a.iter().position(|x| x == "--conversation").unwrap() + 1], "gsid");
    }

    /// a70 scenario "The prune is surgical": the claude scoped delete removes
    /// ONLY `<store>/<handle>.jsonl`, leaving sibling sessions AND the
    /// settings / credentials / memory files intact. Re-deleting is idempotent.
    #[test]
    fn claude_delete_session_is_surgical() {
        let home = tempfile::tempdir().unwrap();
        let workspace = Path::new("/some/workspace/repo");
        let store = home
            .path()
            .join(".claude/projects")
            .join(claude_project_hash(workspace));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("target.jsonl"), "{}").unwrap();
        std::fs::write(store.join("other.jsonl"), "{}").unwrap();
        let claude_dir = home.path().join(".claude");
        std::fs::write(claude_dir.join("settings.json"), "{}").unwrap();
        std::fs::write(claude_dir.join(".credentials.json"), "{}").unwrap();
        std::fs::write(claude_dir.join("CLAUDE.md"), "memory").unwrap();

        let strat = ClaudeStrategy::new("claude".into(), vec![]);
        let ctx = SessionStoreCtx {
            home: home.path(),
            workspace,
        };
        assert!(strat.delete_session(ctx, "target").unwrap());
        assert!(!store.join("target.jsonl").exists(), "named session is gone");
        assert!(store.join("other.jsonl").exists(), "sibling session survives");
        assert!(claude_dir.join("settings.json").exists());
        assert!(claude_dir.join(".credentials.json").exists());
        assert!(claude_dir.join("CLAUDE.md").exists());
        assert!(
            !strat.delete_session(ctx, "target").unwrap(),
            "re-delete is idempotent (Ok(false))"
        );
    }

    /// a70: the antigravity scoped delete removes the conversation `.db` AND
    /// its `brain/<id>/` dir (both keyed by the conversation id), leaving
    /// other conversations AND settings / oauth creds intact.
    #[test]
    fn antigravity_delete_session_removes_db_and_brain_only() {
        let home = tempfile::tempdir().unwrap();
        let store = home.path().join(".gemini/antigravity-cli");
        std::fs::create_dir_all(store.join("conversations")).unwrap();
        std::fs::create_dir_all(store.join("brain/target")).unwrap();
        std::fs::create_dir_all(store.join("brain/other")).unwrap();
        std::fs::create_dir_all(home.path().join(".gemini")).unwrap();
        std::fs::write(store.join("conversations/target.db"), "x").unwrap();
        std::fs::write(store.join("conversations/other.db"), "x").unwrap();
        std::fs::write(store.join("brain/target/state"), "x").unwrap();
        std::fs::write(home.path().join(".gemini/settings.json"), "{}").unwrap();
        std::fs::write(home.path().join(".gemini/oauth_creds.json"), "{}").unwrap();

        let strat = AntigravityStrategy::new("agy".into(), vec![]);
        let ctx = SessionStoreCtx {
            home: home.path(),
            workspace: Path::new("/ws"),
        };
        assert!(strat.delete_session(ctx, "target").unwrap());
        assert!(!store.join("conversations/target.db").exists());
        assert!(!store.join("brain/target").exists());
        assert!(store.join("conversations/other.db").exists());
        assert!(store.join("brain/other").exists());
        assert!(home.path().join(".gemini/settings.json").exists());
        assert!(home.path().join(".gemini/oauth_creds.json").exists());
    }

    /// a70 hardening: the handle-safety guard rejects every path-traversal
    /// shape (separators, a `..` component, interior NUL, empty) while
    /// accepting the real handle shapes (a `claude` UUID, an `antigravity`
    /// conversation id, a store-diff filename stem — `.`-containing stems
    /// without a `..` are fine).
    #[test]
    fn session_handle_is_safe_rejects_traversal_handles() {
        // Real handles pass.
        assert!(session_handle_is_safe(
            "0e8c2a1b-7d4f-4c3a-9b2e-1f6a5d3c2b10"
        ));
        assert!(session_handle_is_safe("created-by-run"));
        assert!(session_handle_is_safe("v1.2.3")); // a single `.` is not `..`
        // Traversal / separator shapes are rejected.
        assert!(!session_handle_is_safe(""));
        assert!(!session_handle_is_safe(".."));
        assert!(!session_handle_is_safe("../escape"));
        assert!(!session_handle_is_safe("../../.ssh/authorized_keys"));
        assert!(!session_handle_is_safe("a/b"));
        assert!(!session_handle_is_safe("a\\b"));
        assert!(!session_handle_is_safe("foo..bar"));
        assert!(!session_handle_is_safe("with\0nul"));
    }

    /// a70 hardening: the claude scoped delete REFUSES a handle that would
    /// escape the store via `..` (`dir.join("../escape.jsonl")` resolves to a
    /// sibling of the store dir). The planted out-of-store file survives AND
    /// the call reports nothing removed.
    #[test]
    fn claude_delete_session_refuses_traversal_handle() {
        let home = tempfile::tempdir().unwrap();
        let workspace = Path::new("/some/workspace/repo");
        let store = home
            .path()
            .join(".claude/projects")
            .join(claude_project_hash(workspace));
        std::fs::create_dir_all(&store).unwrap();
        // `dir.join("../escape.jsonl")` → `<projects>/escape.jsonl`, a sibling
        // of the per-project store dir. Without the guard the surgical delete
        // would remove it.
        let escape_target = store.parent().unwrap().join("escape.jsonl");
        std::fs::write(&escape_target, "victim").unwrap();

        let strat = ClaudeStrategy::new("claude".into(), vec![]);
        let ctx = SessionStoreCtx {
            home: home.path(),
            workspace,
        };
        assert!(
            !strat.delete_session(ctx, "../escape").unwrap(),
            "an unsafe handle removes nothing (Ok(false))"
        );
        assert!(
            escape_target.exists(),
            "the out-of-store file survives — no traversal occurred"
        );
    }

    /// a70 hardening: the antigravity scoped delete REFUSES a traversal handle
    /// for BOTH the `<id>.db` file AND the `brain/<id>/` `remove_dir_all` path
    /// (the recursive delete is the more dangerous of the two). The planted
    /// out-of-store db file AND brain directory both survive.
    #[test]
    fn antigravity_delete_session_refuses_traversal_handle() {
        let home = tempfile::tempdir().unwrap();
        let store = home.path().join(".gemini/antigravity-cli");
        std::fs::create_dir_all(store.join("conversations")).unwrap();
        std::fs::create_dir_all(store.join("brain")).unwrap();
        // db: `conversations/../victim.db` → `<store>/victim.db`.
        let db_target = store.join("victim.db");
        std::fs::write(&db_target, "victim").unwrap();
        // brain: `brain/../victim` → `<store>/victim/` (would be recursively
        // wiped by `remove_dir_all` without the guard).
        let brain_target = store.join("victim");
        std::fs::create_dir_all(&brain_target).unwrap();
        std::fs::write(brain_target.join("important"), "victim").unwrap();

        let strat = AntigravityStrategy::new("agy".into(), vec![]);
        let ctx = SessionStoreCtx {
            home: home.path(),
            workspace: Path::new("/ws"),
        };
        assert!(
            !strat.delete_session(ctx, "../victim").unwrap(),
            "an unsafe handle removes nothing (Ok(false))"
        );
        assert!(db_target.exists(), "out-of-store db file survives");
        assert!(
            brain_target.join("important").exists(),
            "out-of-store brain dir survives the would-be recursive delete"
        );
    }

    /// a70 §4.1 / scenario "A single-shot agentic role prunes its session on
    /// completion": a `prune = true` run deletes the session record it
    /// created (captured by store-diff) while a sibling session AND an
    /// out-of-store settings sentinel survive (surgical scope).
    #[tokio::test]
    async fn agentic_run_with_session_prunes_single_shot_session() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = home
            .path()
            .join(".claude/projects")
            .join(claude_project_hash(ws.path()));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("preexisting.jsonl"), "{}").unwrap();
        std::fs::write(home.path().join(".claude/settings.json"), "{}").unwrap();

        // A stub claude that creates a NEW session record in the store (what
        // the real CLI does), drains the prompt, exits 0.
        let created = store.join("created-by-run.jsonl");
        let stub = ws.path().join("claude_stub.sh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\ncat >/dev/null\necho '{{}}' > '{}'\necho done\n",
                created.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let strat = ClaudeStrategy::new(stub.to_string_lossy().into_owned(), vec![]);
        let outcome = agentic_run_with_session(
            AgenticRunOpts {
                workspace: ws.path(),
                change: "audit",
                strategy: &strat,
                prompt: "do the thing",
                sandbox: SandboxConfig {
                    allowed_tools: vec!["Read".into()],
                    disallowed_bash_patterns: vec![],
                    disallowed_read_paths: vec![],
                    deny_writes: true,
                },
                model: None,
                output_mode: OutputMode::Capture,
                timeout: std::time::Duration::from_secs(30),
                paths: None,
                settings_dir: Some(ws.path()),
                include_autocoder_tools: false,
                emit_stream_json_in_capture: false,
                resume_session_id: None,
                track_subprocess_marker: false,
                etxtbsy_retry_spawn: false,
                os_sandbox: crate::sandbox::RunSandbox::default(),
            },
            true,
            Some(home.path()),
        )
        .await
        .expect("run completes");

        assert_eq!(
            outcome.session_handle.as_deref(),
            Some("created-by-run"),
            "the created session's handle is captured from the store diff"
        );
        assert!(!created.exists(), "the created session record is pruned");
        assert!(
            store.join("preexisting.jsonl").exists(),
            "sibling session survives the surgical prune"
        );
        assert!(
            home.path().join(".claude/settings.json").exists(),
            "settings survive the surgical prune"
        );
    }
}
