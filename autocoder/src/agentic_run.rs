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
//! model's provider can pick the `claude` CLI today and `opencode` later
//! without role code changing. This change registers only the
//! [`ClaudeStrategy`]; a provider that resolves to any other CLI returns
//! a clear "strategy not yet implemented" error
//! ([`strategy_for_provider`]).
//!
//! The refactor is behavior-neutral: the executor keeps streaming-JSON +
//! MCP + the recovery/session-reuse path; each audit keeps simple-capture
//! + no-MCP + its read-only tool list + its ETXTBSY retry.

use anyhow::{Context, Result, anyhow};
use std::path::Path;
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
    /// Carried for completeness of the resolved tuple AND for strategy
    /// dispatch (the strategy is selected from the provider before this is
    /// constructed); `apply_model_selection` itself does not read it.
    #[allow(dead_code)]
    pub provider: crate::config::LlmProvider,
    pub model: String,
    pub api_base_url: String,
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
/// settings file has already been written by [`agentic_run`]; the strategy
/// only assembles argv.
pub struct BuildContext<'a> {
    pub settings_path: &'a Path,
    pub allowed_tools: &'a [String],
    /// Append the autocoder MCP provided-tool names to `--allowedTools`
    /// (the executor's main path does this so the agent may call the
    /// `ask_user` / `outcome_*` / `query_canonical_specs` MCP tools).
    pub include_autocoder_tools: bool,
    /// Emit `--verbose --output-format stream-json` on the command.
    pub emit_stream_json: bool,
    /// `--resume <session_id>` for the recovery turn.
    pub resume_session_id: Option<&'a str>,
}

/// Abstracts CLI invocation so a model's provider can determine the CLI
/// without role code changing. Two jobs: build the invocation (binary,
/// flags, allowed-tools/settings format) AND translate a [`ResolvedModel`]
/// into the CLI's model-selection mechanism.
pub trait CliStrategy: Send + Sync {
    fn build_command(&self, ctx: &BuildContext<'_>) -> Command;
    fn apply_model_selection(&self, cmd: &mut Command, model: Option<&ResolvedModel>);
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
/// selection sets `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` /
/// `ANTHROPIC_MODEL` ONLY when a model is configured; with no model it
/// sets none of them (the executor's current CLI-default behavior).
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
            cmd.arg("--resume").arg(sid);
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
            cmd.env("ANTHROPIC_AUTH_TOKEN", &m.api_key);
            cmd.env("ANTHROPIC_MODEL", &m.model);
        }
        // model: None → set nothing; the CLI uses its own default model.
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

/// Resolve the strategy for a specific CLI. This change registers only
/// `claude`; any other CLI returns a clear error naming the CLI (its
/// strategy lands with a later change) AND no subprocess is spawned.
#[allow(dead_code)]
pub fn strategy_for_cli(
    cli: crate::config::CliKind,
    command: String,
    args: Vec<String>,
) -> Result<Box<dyn CliStrategy>> {
    match cli {
        crate::config::CliKind::Claude => Ok(Box::new(ClaudeStrategy::new(command, args))),
        other => Err(anyhow!(
            "agentic-run strategy not yet implemented for CLI `{}`; only `claude` is registered in this change",
            other.as_str()
        )),
    }
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
}

/// Spawn the wrapped CLI, write `prompt` on its stdin, wait with the
/// configured timeout, AND return the unified outcome. See the module
/// docs for the behavior contract.
pub async fn agentic_run(opts: AgenticRunOpts<'_>) -> Result<AgenticRunOutcome> {
    let resolved_sandbox = crate::config::ResolvedSandbox {
        allowed_tools: opts.sandbox.allowed_tools.clone(),
        disallowed_bash_patterns: opts.sandbox.disallowed_bash_patterns.clone(),
        disallowed_read_paths: opts.sandbox.disallowed_read_paths.clone(),
    };
    let (settings_path, _settings_guard) = crate::audits::write_sandbox_settings(
        &resolved_sandbox,
        opts.settings_dir,
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
        };
        let mut cmd = opts.strategy.build_command(&ctx);
        opts.strategy.apply_model_selection(&mut cmd, opts.model);
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

    // 5.3: a resolved model → all three ANTHROPIC_* vars set from the tuple.
    #[test]
    fn claude_strategy_with_model_sets_all_three_anthropic_env() {
        let strat = ClaudeStrategy::new("claude".into(), Vec::new());
        let model = ResolvedModel {
            provider: LlmProvider::Anthropic,
            model: "claude-opus-4-8".into(),
            api_base_url: "https://example.invalid/api".into(),
            api_key: "sk-test".into(),
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
        assert_eq!(e.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str), Some("sk-test"));
        assert_eq!(e.get("ANTHROPIC_MODEL").map(String::as_str), Some("claude-opus-4-8"));
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

    // 5.4: a provider resolving to a CLI with no registered strategy errors,
    // naming the CLI; the only registered strategy is `claude`.
    #[test]
    fn strategy_for_provider_anthropic_resolves_claude() {
        assert!(strategy_for_provider(LlmProvider::Anthropic, "claude".into(), Vec::new()).is_ok());
    }

    #[test]
    fn strategy_for_provider_non_claude_errors_naming_cli() {
        for p in [LlmProvider::OpenAiCompatible, LlmProvider::Ollama] {
            let err = strategy_for_provider(p, "opencode".into(), Vec::new())
                .err()
                .expect("non-claude provider has no registered strategy yet");
            assert!(
                format!("{err:#}").contains("opencode"),
                "error must name the CLI: {err:#}"
            );
        }
    }

    #[test]
    fn strategy_for_cli_opencode_errors_naming_cli() {
        let err = strategy_for_cli(CliKind::Opencode, "opencode".into(), Vec::new())
            .err()
            .expect("opencode strategy is not registered in this change");
        assert!(format!("{err:#}").contains("opencode"));
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
}
