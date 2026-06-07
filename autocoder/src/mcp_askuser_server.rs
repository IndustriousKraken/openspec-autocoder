//! Minimal stdio MCP server exposing two tools (a21):
//! - `ask_user(question)` — writes a marker file the parent autocoder
//!   process picks up after the wrapped agent exits.
//! - `query_canonical_specs(query, top_k?)` — relays the request to the
//!   daemon via a Unix-domain control socket and returns ranked
//!   canonical-spec chunks for the wrapped agent's query.
//!
//! Launched by `claude-cli` (or any MCP-compatible CLI agent) as a child
//! process via the workspace's `.mcp.json` configuration written by
//! `ClaudeCliExecutor` at run time.
//!
//! Protocol: JSON-RPC 2.0 over stdio with newline-delimited messages.
//! Only the subset needed by Claude Code's MCP client is implemented:
//! `initialize`, `tools/list`, `tools/call`.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Env vars autocoder sets in the MCP server child's environment.
pub const ENV_WORKSPACE: &str = "ORCH_MCP_WORKSPACE";
pub const ENV_CHANGE: &str = "ORCH_MCP_CHANGE";
/// Path to the daemon's control socket. Set when canonical_rag is
/// configured; absent → the `query_canonical_specs` tool returns
/// `{ hits: [], error_hint: "rag not configured for this execution" }`.
pub const ENV_CONTROL_SOCKET: &str = "ORCH_DAEMON_CONTROL_SOCKET";
/// Sanitized workspace basename routed into the control-socket request
/// so the daemon's handler can look up the right `CanonicalRagStore`.
pub const ENV_WORKSPACE_BASENAME: &str = "ORCH_MCP_WORKSPACE_BASENAME";
/// Role name the per-execution MCP child serves (a56). Written into
/// `.mcp.json` by `ClaudeCliExecutor::write_mcp_config` when a role is
/// supplied; the child then advertises only that role's `submit_*` tool
/// alongside the common tools. Absent → no submission tool is advertised.
pub const ENV_ROLE: &str = "ORCH_MCP_ROLE";

/// The MCP server name registered in `.mcp.json`'s `mcpServers` key.
/// MUST match the key used in `ClaudeCliExecutor::write_mcp_config`.
/// Claude CLI exposes MCP tools to the agent as `mcp__<server>__<tool>`,
/// so changing this string changes the agent-visible tool names AND
/// requires updating any operator-configured `allowed_tools` entries
/// that referenced the old name.
pub const SERVER_NAME: &str = "ask_user";

/// Canonical list of tools this MCP server provides via its `tools/list`
/// response. MUST be kept in sync with the response body in
/// `handle_request`'s `"tools/list"` arm. Used by
/// `crate::agentic_run::build_allowed_tools_value` to auto-include these
/// tools in the `--allowedTools` argument passed to Claude CLI, so operators
/// don't have to enumerate them in `executor.sandbox.allowed_tools` —
/// they're part of the daemon's contract with the agent (not operator-
/// configurable surface). When adding a new MCP tool, add its name HERE
/// in addition to the `tools/list` response; the auto-allow path picks
/// it up on the next polling iteration.
pub const PROVIDED_TOOL_NAMES: &[&str] = &[
    "ask_user",
    "query_canonical_specs",
    "outcome_success",              // added by a27a0 (PR #73)
    "outcome_spec_needs_revision",  // added by a27a0 (PR #73)
    "outcome_request_iteration",    // added by a27a1 (PR #74)
];

/// Format a tool name as Claude CLI's `--allowedTools` expects:
/// `mcp__<server>__<tool>`. Reused by the executor's argv builder.
pub fn qualified_tool_name(tool: &str) -> String {
    format!("mcp__{SERVER_NAME}__{tool}")
}

// ---------------------------------------------------------------------------
// Outcome-tool `description` text (a44).
//
// These three constants are the single source of truth for the `description`
// field of each outcome tool in the `tools/list` response below. They are the
// PRIMARY surface for shaping agent behaviour: the agent reads them to decide
// how to use each tool, so they SHALL stay operationally focused (what to do,
// what content to produce) and SHALL NOT carry narrative history about prior
// failure modes or legacy mechanisms.
//
// The canonical executor requirement "MCP outcome-tool description fields
// encourage substantive content AND drop narrative history" governs this
// content as design intent: each description directs the agent what to do AND
// what content to produce, and carries no narrative history about prior failure
// modes or superseded mechanisms. That fitness is verified by review AND the
// drift audit's semantic judgment — NOT by a unit test asserting substrings of
// the prose (per the project-documentation requirement "Tests assert behavior
// or derivation, never message wording"). The only test over these descriptions
// is structural: `each_outcome_tool_advertised_with_nonempty_description`
// asserts the served `tools/list` carries a non-empty description per tool.

/// `description` for the `outcome_success` tool. Content intent is governed by
/// the executor requirement above (review + drift audit), not a substring test.
pub(crate) const OUTCOME_SUCCESS_DESCRIPTION: &str = "Signal successful completion of the implementation run. Pass `final_answer` with a substantive end-of-run summary (10-20 lines: what you implemented, test counts, clippy + `openspec validate` results, judgment calls, follow-ups). This text becomes the per-change body of the PR's `## Agent implementation notes` section AND is the reviewer's primary surface. Call once on the success path before exiting.";

/// `description` for the `outcome_request_iteration` tool. Content intent is
/// governed by the executor requirement above (review + drift audit).
pub(crate) const OUTCOME_REQUEST_ITERATION_DESCRIPTION: &str = "Signal that you completed some tasks but want another iteration to finish the rest. NOT for unimplementable tasks (use `outcome_spec_needs_revision` for those). The cumulative completed/remaining lists carry forward across iterations; the reason field documents the concrete blocker. Input is schema-validated at the MCP layer; empty arrays AND placeholder-shaped strings (e.g. `<concrete blocker>`) are rejected with a tool error you can correct AND retry in the same session.";

/// `description` for the `outcome_spec_needs_revision` tool. Content intent is
/// governed by the executor requirement above (review + drift audit).
pub(crate) const OUTCOME_SPEC_NEEDS_REVISION_DESCRIPTION: &str = "Signal that tasks.md names one or more tasks the agent cannot complete in this sandbox. Input is schema-validated at the MCP layer; placeholder-shaped strings (e.g. `<id-from-tasks-md>`) are rejected with a tool error you can correct AND retry in the same session.";

/// 10-second timeout for the control-socket round trip (read + write).
const CONTROL_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the stdio MCP server until stdin closes. Returns Ok on a clean
/// shutdown (EOF on stdin) or Err on a protocol/IO failure.
pub fn run() -> Result<()> {
    let workspace = std::env::var(ENV_WORKSPACE)
        .with_context(|| format!("missing {ENV_WORKSPACE} in MCP server env"))?;
    let change = std::env::var(ENV_CHANGE)
        .with_context(|| format!("missing {ENV_CHANGE} in MCP server env"))?;
    let marker_path = PathBuf::from(&workspace)
        .join("openspec/changes")
        .join(&change)
        .join(".askuser-pending.json");

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .context("reading from stdin")?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                emit_error(&mut writer, None, -32700, &format!("parse error: {e}"))?;
                continue;
            }
        };
        handle_request(&mut writer, &marker_path, req)?;
    }
    Ok(())
}

fn handle_request<W: Write>(
    writer: &mut W,
    marker_path: &std::path::Path,
    req: JsonRpcRequest,
) -> Result<()> {
    let id = req.id.clone();
    match req.method.as_str() {
        "initialize" => {
            let result = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "autocoder-mcp",
                    "version": env!("AUTOCODER_VERSION"),
                }
            });
            emit_result(writer, id, result)?;
        }
        "notifications/initialized" => {
            // Notification — no response expected.
        }
        "tools/list" => {
            let base = serde_json::json!({
                "tools": [
                    {
                        "name": "ask_user",
                        "description": "Ask the human operator a question when you cannot proceed without their input. After calling this tool, stop further changes; autocoder will deliver the human's answer in a subsequent invocation.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "description": "A clear, self-contained question to ask the human."
                                }
                            },
                            "required": ["question"]
                        }
                    },
                    {
                        "name": "query_canonical_specs",
                        "description": "Retrieve canonical-spec chunks for a query string via semantic similarity. Use this when you're working on a capability whose canonical contract matters. Returns ranked excerpts, not whole files; cheap to call as often as useful.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": {
                                    "type": "string",
                                    "description": "A search string describing what canonical-spec context you want (a requirement title, a problem you're solving, a keyword)."
                                },
                                "top_k": {
                                    "type": "integer",
                                    "description": "Optional maximum number of chunks to return. Defaults to the daemon's configured top_k (typically 10)."
                                }
                            },
                            "required": ["query"]
                        }
                    },
                    {
                        "name": "outcome_success",
                        "description": OUTCOME_SUCCESS_DESCRIPTION,
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "final_answer": {
                                    "type": "string",
                                    "description": "Optional end-of-run summary text (the agent's `result`-event content). When omitted, an empty string is recorded."
                                }
                            }
                        }
                    },
                    {
                        "name": "outcome_request_iteration",
                        "description": OUTCOME_REQUEST_ITERATION_DESCRIPTION,
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "completed_tasks": {
                                    "type": "array",
                                    "description": "Cumulative across iterations — every task id (e.g. \"1.2\") that has been completed so far, including those from prior iterations. Non-empty.",
                                    "items": { "type": "string" },
                                    "minItems": 1
                                },
                                "remaining_tasks": {
                                    "type": "array",
                                    "description": "Task ids still pending after this iteration. Non-empty.",
                                    "items": { "type": "string" },
                                    "minItems": 1
                                },
                                "reason": {
                                    "type": "string",
                                    "description": "Concrete one-line blocker — what specifically prevented you from finishing this iteration. No placeholder-shaped strings."
                                }
                            },
                            "required": ["completed_tasks", "remaining_tasks", "reason"]
                        }
                    },
                    {
                        "name": "outcome_spec_needs_revision",
                        "description": OUTCOME_SPEC_NEEDS_REVISION_DESCRIPTION,
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "unimplementable_tasks": {
                                    "type": "array",
                                    "description": "Non-empty list of tasks that cannot run in this sandbox.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "task_id": {
                                                "type": "string",
                                                "description": "Exact id from tasks.md, e.g. \"6.4\"."
                                            },
                                            "task_text": {
                                                "type": "string",
                                                "description": "Verbatim text of the unimplementable task."
                                            },
                                            "reason": {
                                                "type": "string",
                                                "description": "One-line explanation of why the task cannot run in this sandbox."
                                            }
                                        },
                                        "required": ["task_id", "task_text", "reason"]
                                    },
                                    "minItems": 1
                                },
                                "revision_suggestion": {
                                    "type": "string",
                                    "description": "Concrete edit the operator can make to tasks.md to make the spec verifiable."
                                }
                            },
                            "required": ["unimplementable_tasks", "revision_suggestion"]
                        }
                    }
                ]
            });
            // a56: advertise the per-role `submit_*` tool when this child
            // serves a role that has a registered submission tool,
            // alongside the common tools. The concrete per-role tools land
            // with their consuming changes (4/5/6/8); none is registered in
            // this change, so the framework adds nothing yet.
            let mut tools = base
                .get("tools")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default();
            if let Some(role) = std::env::var(ENV_ROLE).ok().filter(|s| !s.is_empty())
                && let Some(tool) = submission_tool_for_role(&role)
            {
                tools.push(tool);
            }
            let result = serde_json::json!({ "tools": tools });
            emit_result(writer, id, result)?;
        }
        "tools/call" => {
            let params = req
                .params
                .ok_or_else(|| anyhow!("tools/call missing params"))?;
            let call: ToolCallParams = serde_json::from_value(params)
                .map_err(|e| anyhow!("tools/call params decode: {e}"))?;
            match call.name.as_str() {
                "ask_user" => {
                    let question = call
                        .arguments
                        .get("question")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .ok_or_else(|| {
                            anyhow!("ask_user: missing string `question` argument")
                        })?;
                    write_marker(marker_path, &question)?;
                    let result = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": "Your question has been delivered to the human operator. autocoder will resume you with their answer in a subsequent invocation. Stop further changes now."
                            }
                        ],
                        "isError": false
                    });
                    emit_result(writer, id, result)?;
                }
                "query_canonical_specs" => {
                    let query_str = match call
                        .arguments
                        .get("query")
                        .and_then(|v| v.as_str())
                    {
                        Some(s) => s.to_string(),
                        None => {
                            emit_error(
                                writer,
                                id,
                                -32602,
                                "query_canonical_specs: missing string `query` argument",
                            )?;
                            return Ok(());
                        }
                    };
                    let top_k = call.arguments.get("top_k").and_then(|v| v.as_u64());
                    let payload = handle_query_canonical_specs(&query_str, top_k);
                    let result = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": serde_json::to_string(&payload)
                                    .unwrap_or_else(|_| "{}".into()),
                            }
                        ],
                        "isError": false,
                        "structuredContent": payload,
                    });
                    emit_result(writer, id, result)?;
                }
                "outcome_success" => {
                    let final_answer = match call.arguments.get("final_answer") {
                        Some(v) if v.is_string() => {
                            Some(v.as_str().unwrap().to_string())
                        }
                        Some(v) if v.is_null() => None,
                        None => None,
                        Some(_) => {
                            emit_error(
                                writer,
                                id,
                                -32602,
                                "outcome_success: `final_answer` must be a string when present",
                            )?;
                            return Ok(());
                        }
                    };
                    let outcome_payload = serde_json::json!({
                        "type": "success",
                        "final_answer": final_answer,
                    });
                    match relay_record_outcome(&outcome_payload) {
                        Ok(()) => {
                            let text = "Outcome recorded: success.";
                            let result = serde_json::json!({
                                "content": [
                                    { "type": "text", "text": text }
                                ],
                                "isError": false,
                                "structuredContent": { "ok": true }
                            });
                            emit_result(writer, id, result)?;
                        }
                        Err(e) => {
                            emit_error(
                                writer,
                                id,
                                -32603,
                                &format!(
                                    "outcome_success: control-socket relay failed: {e}"
                                ),
                            )?;
                        }
                    }
                }
                "outcome_request_iteration" => {
                    match validate_request_iteration_args(&call.arguments) {
                        Ok(payload) => match relay_record_outcome(&payload) {
                            Ok(()) => {
                                let text = "Outcome recorded: iteration_request.";
                                let result = serde_json::json!({
                                    "content": [
                                        { "type": "text", "text": text }
                                    ],
                                    "isError": false,
                                    "structuredContent": { "ok": true }
                                });
                                emit_result(writer, id, result)?;
                            }
                            Err(e) => {
                                emit_error(
                                    writer,
                                    id,
                                    -32603,
                                    &format!(
                                        "outcome_request_iteration: control-socket relay failed: {e}"
                                    ),
                                )?;
                            }
                        },
                        Err(msg) => {
                            emit_error(writer, id, -32602, &msg)?;
                        }
                    }
                }
                "outcome_spec_needs_revision" => {
                    match validate_spec_needs_revision_args(&call.arguments) {
                        Ok(payload) => match relay_record_outcome(&payload) {
                            Ok(()) => {
                                let text = "Outcome recorded: spec_needs_revision.";
                                let result = serde_json::json!({
                                    "content": [
                                        { "type": "text", "text": text }
                                    ],
                                    "isError": false,
                                    "structuredContent": { "ok": true }
                                });
                                emit_result(writer, id, result)?;
                            }
                            Err(e) => {
                                emit_error(
                                    writer,
                                    id,
                                    -32603,
                                    &format!(
                                        "outcome_spec_needs_revision: control-socket relay failed: {e}"
                                    ),
                                )?;
                            }
                        },
                        Err(msg) => {
                            emit_error(writer, id, -32602, &msg)?;
                        }
                    }
                }
                // a57 `submit_findings` (advisory audits), a58 `submit_review`
                // (agentic reviewer), a59 `submit_contradictions` (the [in]
                // gate), a62 `submit_canon_contradictions` (the [canon] gate),
                // AND a63 `submit_verdict` (the [out] gate) share the per-role
                // relay path: the role selects the daemon-side schema, the
                // tool's full arguments object IS the submission payload, AND a
                // rejection comes back as a correctable tool error.
                tool @ ("submit_findings"
                | "submit_review"
                | "submit_contradictions"
                | "submit_canon_contradictions"
                | "submit_canon_internal_contradictions"
                | "submit_verdict") => {
                    let role = std::env::var(ENV_ROLE)
                        .ok()
                        .filter(|s| !s.is_empty());
                    let Some(role) = role else {
                        emit_error(
                            writer,
                            id,
                            -32601,
                            &format!("{tool}: no ORCH_MCP_ROLE set; this MCP child advertises no submission tool"),
                        )?;
                        return Ok(());
                    };
                    let payload = call.arguments.clone();
                    match relay_submission(&role, &payload) {
                        Ok(()) => {
                            let text = match tool {
                                "submit_review" => {
                                    "Review submitted and recorded as this PR's verdict. You may stop now."
                                }
                                "submit_contradictions"
                                | "submit_canon_contradictions"
                                | "submit_canon_internal_contradictions" => {
                                    "Contradictions submitted and recorded as this check's result. You may stop now."
                                }
                                "submit_verdict" => {
                                    "Verdict submitted and recorded as this check's result. You may stop now."
                                }
                                _ => {
                                    "Findings submitted and recorded as this audit's result. You may stop now."
                                }
                            };
                            let result = serde_json::json!({
                                "content": [
                                    { "type": "text", "text": text }
                                ],
                                "isError": false,
                                "structuredContent": { "ok": true }
                            });
                            emit_result(writer, id, result)?;
                        }
                        Err(e) => {
                            // A schema rejection (or transport failure) is
                            // surfaced as a correctable tool error so the
                            // agent can fix its payload AND resubmit in the
                            // same session.
                            emit_error(
                                writer,
                                id,
                                -32602,
                                &format!("{tool} rejected: {e}"),
                            )?;
                        }
                    }
                }
                other => {
                    emit_error(
                        writer,
                        id,
                        -32601,
                        &format!("unknown tool `{other}`"),
                    )?;
                }
            }
        }
        other => {
            emit_error(writer, id, -32601, &format!("method not found: {other}"))?;
        }
    }
    Ok(())
}

/// Build the `query_canonical_specs` tool result payload. Fail-open:
/// every error path returns `{ hits: [], error_hint: "..." }` so the
/// agent can fall back to its non-RAG behaviour gracefully.
fn handle_query_canonical_specs(
    query: &str,
    top_k: Option<u64>,
) -> serde_json::Value {
    let socket_path = match std::env::var(ENV_CONTROL_SOCKET) {
        Ok(s) => s,
        Err(_) => {
            return serde_json::json!({
                "hits": [],
                "error_hint": "rag not configured for this execution",
            });
        }
    };
    let workspace_basename = std::env::var(ENV_WORKSPACE_BASENAME).unwrap_or_default();
    let mut request = serde_json::json!({
        "action": "query_canonical_specs",
        "workspace_basename": workspace_basename,
        "query": query,
    });
    if let Some(k) = top_k {
        request["top_k"] = serde_json::json!(k);
    }
    match relay_to_control_socket(Path::new(&socket_path), &request) {
        Ok(value) => {
            // Pass through `hits` and `error_hint` from the daemon's
            // response verbatim — the daemon's fail-open contract is
            // already the right shape for the tool result.
            let hits = value
                .get("hits")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            let mut out = serde_json::json!({ "hits": hits });
            if let Some(hint) = value.get("error_hint").and_then(|h| h.as_str()) {
                out["error_hint"] = serde_json::json!(hint);
            }
            out
        }
        Err(e) => serde_json::json!({
            "hits": [],
            "error_hint": format!("control socket unreachable: {e}"),
        }),
    }
}

/// Detect un-substituted `<placeholder>` text. Matches the daemon-side
/// detector in `claude_cli.rs` (kept duplicate to keep the MCP server
/// self-contained; both layers must agree on the rejection rule). The
/// regex is intentionally narrow — leading lowercase letter, then
/// `[a-z0-9 _-]` — so legitimate angle-bracket content like `<HTML>` or
/// `<MyType>` does not false-positive.
fn contains_placeholder_marker(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let start = i + 1;
        if start >= bytes.len() {
            return false;
        }
        let first = bytes[start];
        if !first.is_ascii_lowercase() {
            i += 1;
            continue;
        }
        let mut j = start + 1;
        let mut closed = false;
        while j < bytes.len() {
            let b = bytes[j];
            if b == b'>' {
                closed = true;
                break;
            }
            let ok = b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b' '
                || b == b'_'
                || b == b'-';
            if !ok {
                break;
            }
            j += 1;
        }
        if closed {
            return true;
        }
        i += 1;
    }
    false
}

/// Validate the `outcome_spec_needs_revision` tool arguments AT THE
/// MCP LAYER. Per the spec deltas, returns `Err(<message>)` on any
/// schema violation; the control socket is NOT contacted. On success,
/// returns the variant-tagged outcome payload ready to ship to the
/// daemon's `record_outcome` action.
pub(crate) fn validate_spec_needs_revision_args(
    args: &serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let tasks_val = args.get("unimplementable_tasks").ok_or_else(|| {
        "outcome_spec_needs_revision: missing required field `unimplementable_tasks`"
            .to_string()
    })?;
    let tasks_array = tasks_val.as_array().ok_or_else(|| {
        "outcome_spec_needs_revision: `unimplementable_tasks` must be an array"
            .to_string()
    })?;
    if tasks_array.is_empty() {
        return Err("outcome_spec_needs_revision: `unimplementable_tasks` must be a non-empty array".to_string());
    }
    let mut validated_tasks: Vec<serde_json::Value> = Vec::with_capacity(tasks_array.len());
    for (i, entry) in tasks_array.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            format!(
                "outcome_spec_needs_revision: `unimplementable_tasks[{i}]` must be an object"
            )
        })?;
        for field in ["task_id", "task_text", "reason"] {
            let v = obj.get(field).ok_or_else(|| {
                format!(
                    "outcome_spec_needs_revision: `unimplementable_tasks[{i}].{field}` is missing"
                )
            })?;
            let s = v.as_str().ok_or_else(|| {
                format!(
                    "outcome_spec_needs_revision: `unimplementable_tasks[{i}].{field}` must be a string"
                )
            })?;
            if s.is_empty() {
                return Err(format!(
                    "outcome_spec_needs_revision: `unimplementable_tasks[{i}].{field}` must be a non-empty string"
                ));
            }
            if contains_placeholder_marker(s) {
                return Err(format!(
                    "outcome_spec_needs_revision: `unimplementable_tasks[{i}].{field}` contains placeholder-shaped text (`<...>`). Substitute concrete values from tasks.md AND retry."
                ));
            }
        }
        validated_tasks.push(entry.clone());
    }
    let revision_val = args.get("revision_suggestion").ok_or_else(|| {
        "outcome_spec_needs_revision: missing required field `revision_suggestion`".to_string()
    })?;
    let revision_str = revision_val.as_str().ok_or_else(|| {
        "outcome_spec_needs_revision: `revision_suggestion` must be a string".to_string()
    })?;
    if revision_str.is_empty() {
        return Err(
            "outcome_spec_needs_revision: `revision_suggestion` must be a non-empty string".to_string(),
        );
    }
    if contains_placeholder_marker(revision_str) {
        return Err(
            "outcome_spec_needs_revision: `revision_suggestion` contains placeholder-shaped text (`<...>`). Substitute a concrete edit AND retry."
                .to_string(),
        );
    }
    Ok(serde_json::json!({
        "type": "spec_needs_revision",
        "unimplementable_tasks": validated_tasks,
        "revision_suggestion": revision_str,
    }))
}

/// Validate the `outcome_request_iteration` tool arguments at the MCP
/// layer. Returns `Err(<message>)` on any schema violation; the control
/// socket is NOT contacted on failure. On success, returns the variant-
/// tagged outcome payload ready to ship to the daemon's `record_outcome`
/// action.
pub(crate) fn validate_request_iteration_args(
    args: &serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    fn validate_string_array(
        args: &serde_json::Value,
        field: &str,
    ) -> std::result::Result<Vec<String>, String> {
        let val = args.get(field).ok_or_else(|| {
            format!("outcome_request_iteration: missing required field `{field}`")
        })?;
        let array = val.as_array().ok_or_else(|| {
            format!("outcome_request_iteration: `{field}` must be an array")
        })?;
        if array.is_empty() {
            return Err(format!(
                "outcome_request_iteration: `{field}` must be a non-empty array"
            ));
        }
        let mut out: Vec<String> = Vec::with_capacity(array.len());
        for (i, entry) in array.iter().enumerate() {
            let s = entry.as_str().ok_or_else(|| {
                format!(
                    "outcome_request_iteration: `{field}[{i}]` must be a string"
                )
            })?;
            if s.is_empty() {
                return Err(format!(
                    "outcome_request_iteration: `{field}[{i}]` must be a non-empty string"
                ));
            }
            if contains_placeholder_marker(s) {
                return Err(format!(
                    "outcome_request_iteration: `{field}[{i}]` contains placeholder-shaped text (`<...>`). Substitute concrete values AND retry."
                ));
            }
            out.push(s.to_string());
        }
        Ok(out)
    }

    let completed_tasks = validate_string_array(args, "completed_tasks")?;
    let remaining_tasks = validate_string_array(args, "remaining_tasks")?;
    let reason_val = args.get("reason").ok_or_else(|| {
        "outcome_request_iteration: missing required field `reason`".to_string()
    })?;
    let reason_str = reason_val.as_str().ok_or_else(|| {
        "outcome_request_iteration: `reason` must be a string".to_string()
    })?;
    if reason_str.is_empty() {
        return Err(
            "outcome_request_iteration: `reason` must be a non-empty string".to_string(),
        );
    }
    if contains_placeholder_marker(reason_str) {
        return Err(
            "outcome_request_iteration: `reason` contains placeholder-shaped text (`<...>`). Substitute a concrete blocker AND retry."
                .to_string(),
        );
    }
    Ok(serde_json::json!({
        "type": "iteration_request",
        "completed_tasks": completed_tasks,
        "remaining_tasks": remaining_tasks,
        "reason": reason_str,
    }))
}

/// Relay a validated outcome payload to the daemon via the existing
/// control-socket transport using a new `record_outcome` action. Reads
/// the routing keys from env vars set by `ClaudeCliExecutor::write_mcp_config`.
/// Returns `Err(<message>)` on socket-absent OR transport failure; the
/// caller maps to JSON-RPC `-32603` (internal error).
fn relay_record_outcome(outcome: &serde_json::Value) -> Result<()> {
    let socket_path = std::env::var(ENV_CONTROL_SOCKET).map_err(|_| {
        anyhow!(
            "{ENV_CONTROL_SOCKET} not set; outcome tools require the daemon's control socket"
        )
    })?;
    let workspace_basename = std::env::var(ENV_WORKSPACE_BASENAME).map_err(|_| {
        anyhow!("{ENV_WORKSPACE_BASENAME} not set; cannot route outcome")
    })?;
    let change = std::env::var(ENV_CHANGE).map_err(|_| {
        anyhow!("{ENV_CHANGE} not set; cannot route outcome")
    })?;
    let request = serde_json::json!({
        "action": "record_outcome",
        "workspace_basename": workspace_basename,
        "change": change,
        "outcome": outcome,
    });
    let resp = relay_to_control_socket(Path::new(&socket_path), &request)?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("daemon rejected record_outcome");
        return Err(anyhow!("{err}"));
    }
    Ok(())
}

/// Relay a schema-validated submission payload to the daemon via the
/// control-socket `record_submission` action (a56), paralleling
/// [`relay_record_outcome`]. Reads the routing keys (workspace basename,
/// change) from env vars set by `ClaudeCliExecutor::write_mcp_config`. A
/// daemon rejection (e.g. schema-invalid) is returned as `Err(<reason>)`
/// so the calling `submit_*` tool can surface it to the agent as a
/// correctable tool error AND let it retry in the same session.
///
/// a57: the advisory audits' `submit_findings` tool is the first consumer.
fn relay_submission(role: &str, payload: &serde_json::Value) -> Result<()> {
    let socket_path = std::env::var(ENV_CONTROL_SOCKET).map_err(|_| {
        anyhow!("{ENV_CONTROL_SOCKET} not set; submit tools require the daemon's control socket")
    })?;
    let workspace_basename = std::env::var(ENV_WORKSPACE_BASENAME)
        .map_err(|_| anyhow!("{ENV_WORKSPACE_BASENAME} not set; cannot route submission"))?;
    let change = std::env::var(ENV_CHANGE)
        .map_err(|_| anyhow!("{ENV_CHANGE} not set; cannot route submission"))?;
    let request = serde_json::json!({
        "action": "record_submission",
        "workspace_basename": workspace_basename,
        "change": change,
        "role": role,
        "payload": payload,
    });
    let resp = relay_to_control_socket(Path::new(&socket_path), &request)?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("daemon rejected record_submission");
        return Err(anyhow!("{err}"));
    }
    Ok(())
}

/// The advisory-audit roles whose per-execution MCP child advertises the
/// `submit_findings` tool (a57). The executor (`implementer`) AND the
/// specs-writing audits (`missing_tests`, `security_bug`) are deliberately
/// absent: the executor reports via the `outcome_*` tools AND the
/// specs-writing audits produce on-disk proposals, not findings.
pub const ADVISORY_AUDIT_ROLES: &[&str] =
    &["drift_audit", "architecture_consultative", "documentation_audit"];

/// Name of the per-role structured-submission (`submit_*`) tool a role
/// advertises, or `None` when the role has no registered submission tool.
/// Used both by the `tools/list` advertisement AND by the daemon-side
/// allowed-tools assembly (so the qualified `mcp__ask_user__submit_findings`
/// name is auto-allowed for the advisory audits). The concrete tools added
/// by later changes (`submit_review`, `submit_contradictions`, …) extend
/// this lookup.
pub fn submission_tool_name_for_role(role: &str) -> Option<&'static str> {
    if ADVISORY_AUDIT_ROLES.contains(&role) {
        Some("submit_findings")
    } else if role == crate::code_reviewer::REVIEWER_ROLE {
        // a58: the agentic reviewer returns its verdict via `submit_review`.
        Some("submit_review")
    } else if role == crate::preflight::change_contradiction::CONTRADICTION_CHECK_ROLE {
        // a59: the contradiction check returns its findings via
        // `submit_contradictions`.
        Some("submit_contradictions")
    } else if role == crate::preflight::canon_contradiction::CANON_CONTRADICTION_CHECK_ROLE {
        // a62: the `[canon]` gate returns its findings via
        // `submit_canon_contradictions`.
        Some("submit_canon_contradictions")
    } else if role == crate::code_implements_spec::CODE_IMPLEMENTS_SPEC_ROLE {
        // a63: the `[out]` gate returns its verdict via `submit_verdict`.
        Some("submit_verdict")
    } else if role == crate::audits::canon_contradiction::CanonContradictionAudit::TYPE {
        // a75: the canon-internal contradiction audit returns its findings
        // via `submit_canon_internal_contradictions` (symmetric capability+
        // title pair on both sides).
        Some("submit_canon_internal_contradictions")
    } else {
        None
    }
}

/// Look up the per-role structured-submission (`submit_*`) tool definition
/// for the `tools/list` response. a57 registers `submit_findings` for the
/// three advisory-audit roles, each with the role's audit-specific finding
/// schema; every other role returns `None`.
fn submission_tool_for_role(role: &str) -> Option<serde_json::Value> {
    // a58: the agentic reviewer's `submit_review` tool has a verdict +
    // concerns shape distinct from the advisory audits' `submit_findings`.
    if role == crate::code_reviewer::REVIEWER_ROLE {
        return Some(submit_review_tool());
    }
    // a59: the contradiction check's `submit_contradictions` tool has a
    // requirement-pair shape distinct from the audits AND the reviewer.
    if role == crate::preflight::change_contradiction::CONTRADICTION_CHECK_ROLE {
        return Some(submit_contradictions_tool());
    }
    // a62: the `[canon]` gate's `submit_canon_contradictions` tool names the
    // conflicting canonical requirement (capability + title), distinct from
    // the `[in]` gate's within-change requirement pair.
    if role == crate::preflight::canon_contradiction::CANON_CONTRADICTION_CHECK_ROLE {
        return Some(submit_canon_contradictions_tool());
    }
    // a63: the `[out]` gate's `submit_verdict` tool returns the
    // code-implements-spec verdict (implemented | gaps_found + per-gap detail),
    // distinct from every other role's submission shape.
    if role == crate::code_implements_spec::CODE_IMPLEMENTS_SPEC_ROLE {
        return Some(submit_verdict_tool());
    }
    // a75: the canon-internal contradiction audit's
    // `submit_canon_internal_contradictions` tool is symmetric — both sides
    // name a canonical requirement (capability + title) — distinct from
    // a62's `submit_canon_contradictions`, which names a change requirement
    // against a canonical one.
    if role == crate::audits::canon_contradiction::CanonContradictionAudit::TYPE {
        return Some(submit_canon_internal_contradictions_tool());
    }
    let finding_schema = match role {
        "drift_audit" => serde_json::json!({
            "type": "object",
            "properties": {
                "capability": { "type": "string" },
                "requirement": { "type": "string" },
                "severity": { "type": "string", "enum": ["low", "medium", "high"] },
                "code_anchors": { "type": "array", "items": { "type": "string" } },
                "divergence": { "type": "string" }
            },
            "required": ["capability", "requirement", "severity", "divergence"]
        }),
        "architecture_consultative" => serde_json::json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string" },
                "body": { "type": "string" },
                "anchor": { "type": "string" },
                "severity": { "type": "string", "enum": ["low", "medium"] }
            },
            "required": ["subject", "body", "anchor", "severity"]
        }),
        "documentation_audit" => serde_json::json!({
            "type": "object",
            "properties": {
                "category": { "type": "string", "enum": ["coverage", "stale_reference", "organization"] },
                "severity": { "type": "string", "enum": ["low", "medium"] },
                "anchor": { "type": "string" },
                "body": { "type": "string" }
            },
            "required": ["category", "severity", "anchor", "body"]
        }),
        _ => return None,
    };
    // The architecture audit caps the array at 5 entries; the registered
    // schema rejects a submission with more, surfacing it to the agent as
    // a correctable tool error.
    let mut findings_schema = serde_json::json!({
        "type": "array",
        "items": finding_schema,
    });
    if role == "architecture_consultative" {
        findings_schema["maxItems"] = serde_json::json!(5);
    }
    Some(serde_json::json!({
        "name": "submit_findings",
        "description": "Return this audit's findings to the daemon as the audit result. Call exactly once when your analysis is complete, passing a `findings` array shaped per this tool's schema (an empty array means \"no findings\"). The daemon validates the payload against the audit's finding schema; a schema violation comes back as a correctable tool error you can fix AND resubmit in the same session.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "findings": findings_schema
            },
            "required": ["findings"]
        }
    }))
}

/// The `submit_review` tool definition advertised for the reviewer role
/// (a58). The `verdict` enum AND `concerns` shape are the operator-visible
/// schema; the daemon-side validator
/// ([`crate::code_reviewer::payload_to_review_result`]) additionally
/// enforces the "non-empty `actionable_request` when `should_request_revision`"
/// cross-field rule that basic JSON Schema cannot express, surfacing a
/// violation as a correctable tool error.
fn submit_review_tool() -> serde_json::Value {
    serde_json::json!({
        "name": "submit_review",
        "description": "Return this code review's verdict to the daemon as the review result. Call exactly once when your analysis is complete. Pass `verdict` (Approve | Block), a `summary`, AND a `concerns` array (empty means \"no concerns\"). Each concern that should drive a revision MUST set `should_request_revision: true` with a non-empty `actionable_request`. Set `security_critical: true` on any finding that is a credential/secret/key exposure (a key, token, or secret written where it could be committed or exposed), a hardcoded secret, OR an injection vulnerability — these are Block-class; the daemon escalates the verdict to `Block` from that signal even if you returned `Approve`. The daemon validates the payload; a schema violation comes back as a correctable tool error you can fix AND resubmit in the same session.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "enum": ["Approve", "Block"] },
                "summary": { "type": "string" },
                "concerns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "detail": { "type": "string" },
                            "anchor": { "type": "string" },
                            "should_request_revision": { "type": "boolean" },
                            "actionable_request": { "type": ["string", "null"] },
                            "security_critical": { "type": "boolean" }
                        },
                        "required": ["title", "detail", "anchor", "should_request_revision"]
                    }
                }
            },
            "required": ["verdict", "summary", "concerns"]
        }
    })
}

/// The `submit_contradictions` tool definition advertised for the
/// contradiction-check role (a59). Each entry names the two conflicting
/// requirements AND a one-line summary of why they cannot both hold; an
/// empty `contradictions` array means "no contradictions found". The
/// daemon-side validator
/// ([`crate::preflight::change_contradiction::payload_to_contradictions`])
/// deserializes the payload into the finding shape, surfacing a malformed
/// payload as a correctable tool error.
fn submit_contradictions_tool() -> serde_json::Value {
    serde_json::json!({
        "name": "submit_contradictions",
        "description": "Return the change-internal contradictions you found to the daemon as this check's result. Call exactly once when your analysis is complete, passing a `contradictions` array (an empty array means \"no contradictions found\"). Each entry names the two conflicting requirements AND a one-line summary of why they cannot both hold simultaneously. The daemon validates the payload; a schema violation comes back as a correctable tool error you can fix AND resubmit in the same session.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "contradictions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "requirement_a": { "type": "string" },
                            "requirement_b": { "type": "string" },
                            "summary": { "type": "string" }
                        },
                        "required": ["requirement_a", "requirement_b", "summary"]
                    }
                }
            },
            "required": ["contradictions"]
        }
    })
}

/// The `submit_canon_contradictions` tool definition advertised for the
/// canon-contradiction-check role — the `[canon]` gate (a62). Each entry names
/// the requirement IN THE CHANGE, the canonical capability AND requirement it
/// conflicts with, AND a one-line summary; an empty `contradictions` array
/// means "no contradictions found". The daemon-side validator
/// ([`crate::preflight::canon_contradiction::payload_to_canon_contradictions`])
/// deserializes the payload into the finding shape, surfacing a malformed
/// payload as a correctable tool error.
fn submit_canon_contradictions_tool() -> serde_json::Value {
    serde_json::json!({
        "name": "submit_canon_contradictions",
        "description": "Return the change-vs-canonical contradictions you found to the daemon as this check's result. Call exactly once when your analysis is complete, passing a `contradictions` array (an empty array means \"no contradictions found\"). Each entry names the requirement IN THIS CHANGE (`change_requirement`), the canonical capability slug AND requirement title it conflicts with (`canonical_capability`, `canonical_requirement`), AND a one-line `summary` of why the two cannot both hold. The daemon validates the payload; a schema violation comes back as a correctable tool error you can fix AND resubmit in the same session.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "contradictions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "change_requirement": { "type": "string" },
                            "canonical_capability": { "type": "string" },
                            "canonical_requirement": { "type": "string" },
                            "summary": { "type": "string" }
                        },
                        "required": ["change_requirement", "canonical_capability", "canonical_requirement", "summary"]
                    }
                }
            },
            "required": ["contradictions"]
        }
    })
}

/// The `submit_canon_internal_contradictions` tool definition advertised
/// for the canon-internal contradiction audit role (a75). Each entry names
/// BOTH conflicting canonical requirements by capability AND title plus a
/// one-line `summary`; an empty `contradictions` array means a clean canon.
/// The schema is symmetric (both sides canonical), distinguishing it from
/// a62's `submit_canon_contradictions`. The daemon-side validator
/// ([`crate::audits::canon_contradiction::payload_to_contradictions`])
/// deserializes the payload into the finding shape, surfacing a malformed
/// payload as a correctable tool error.
fn submit_canon_internal_contradictions_tool() -> serde_json::Value {
    serde_json::json!({
        "name": "submit_canon_internal_contradictions",
        "description": "Return the canon-internal contradictions you found to the daemon as this audit's result. Call exactly once when your analysis is complete, passing a `contradictions` array (an empty array means \"no contradictions found — clean canon\"). Each entry names BOTH conflicting canonical requirements by capability slug AND requirement title (`capability_a`/`requirement_a` and `capability_b`/`requirement_b`), AND a one-line `summary` of why the two cannot both hold. The daemon validates the payload; a schema violation comes back as a correctable tool error you can fix AND resubmit in the same session.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "contradictions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "capability_a": { "type": "string" },
                            "requirement_a": { "type": "string" },
                            "capability_b": { "type": "string" },
                            "requirement_b": { "type": "string" },
                            "summary": { "type": "string" }
                        },
                        "required": ["capability_a", "requirement_a", "capability_b", "requirement_b", "summary"]
                    }
                }
            },
            "required": ["contradictions"]
        }
    })
}

/// The `submit_verdict` tool definition advertised for the code-implements-spec
/// role — the `[out]` gate (a63). The verdict is `implemented` (the change's
/// requirements AND scenarios are satisfied) or `gaps_found` (one or more are
/// not). Each gap names the `requirement`, an optional `scenario`, a `status`
/// of `missing` or `partial`, AND concrete `evidence`. The daemon-side
/// validator ([`crate::code_implements_spec::payload_to_verification`])
/// additionally enforces the "non-empty `gaps` when `verdict: gaps_found`"
/// cross-field rule that basic JSON Schema cannot express, surfacing a
/// violation as a correctable tool error.
fn submit_verdict_tool() -> serde_json::Value {
    serde_json::json!({
        "name": "submit_verdict",
        "description": "Return your code-implements-spec verdict to the daemon as this check's result. Call exactly once when your analysis is complete. Pass `verdict` (implemented | gaps_found), a `summary`, AND a `gaps` array. Use `implemented` with an empty `gaps` array when every requirement AND scenario is satisfied; use `gaps_found` with a NON-EMPTY `gaps` array otherwise. Each gap names the `requirement`, an optional `scenario` (or null), a `status` (missing | partial), AND concrete `evidence`. The daemon validates the payload; a schema violation comes back as a correctable tool error you can fix AND resubmit in the same session.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "enum": ["implemented", "gaps_found"] },
                "summary": { "type": "string" },
                "gaps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "requirement": { "type": "string" },
                            "scenario": { "type": ["string", "null"] },
                            "status": { "type": "string", "enum": ["missing", "partial"] },
                            "evidence": { "type": "string" }
                        },
                        "required": ["requirement", "status", "evidence"]
                    }
                }
            },
            "required": ["verdict", "summary", "gaps"]
        }
    })
}

/// Open a connection to the daemon's control socket, send `request`
/// followed by a newline, and read the single-line JSON response. Both
/// halves are bounded by `CONTROL_SOCKET_TIMEOUT`.
fn relay_to_control_socket(
    socket: &Path,
    request: &serde_json::Value,
) -> Result<serde_json::Value> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to control socket at {}", socket.display()))?;
    stream.set_read_timeout(Some(CONTROL_SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_SOCKET_TIMEOUT))?;
    let mut stream = stream;
    let raw = serde_json::to_string(request)?;
    stream.write_all(raw.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    let value: serde_json::Value = serde_json::from_str(buf.trim())
        .with_context(|| format!("decoding control-socket response: {buf:?}"))?;
    Ok(value)
}

fn write_marker(marker_path: &std::path::Path, question: &str) -> Result<()> {
    let parent = marker_path
        .parent()
        .ok_or_else(|| anyhow!("marker path has no parent: {}", marker_path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let payload = serde_json::json!({ "question": question });
    let tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    serde_json::to_writer_pretty(&tmp, &payload)
        .context("serializing askuser marker")?;
    tmp.persist(marker_path)
        .map_err(|e| anyhow!("persisting marker file {}: {e}", marker_path.display()))?;
    Ok(())
}

fn emit_result<W: Write>(
    writer: &mut W,
    id: Option<serde_json::Value>,
    result: serde_json::Value,
) -> Result<()> {
    if id.is_none() {
        return Ok(()); // notification — no response
    }
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    write_message(writer, &resp)
}

fn emit_error<W: Write>(
    writer: &mut W,
    id: Option<serde_json::Value>,
    code: i64,
    message: &str,
) -> Result<()> {
    if id.is_none() {
        return Ok(());
    }
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    });
    write_message(writer, &resp)
}

fn write_message<W: Write>(writer: &mut W, value: &serde_json::Value) -> Result<()> {
    let line = serde_json::to_string(value)?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    id: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    // Env-var-touching tests serialize via `crate::testing::ENV_LOCK`
    // (a27a2 unified the per-module locks into a single process-wide
    // lock so cross-module tests cannot race).
    use crate::testing::ENV_LOCK;

    /// Drive the server's `handle_request` with a sequence of synthetic
    /// JSON-RPC messages and return everything written to the response
    /// buffer.
    fn run_with(
        marker_path: &std::path::Path,
        messages: &[&str],
    ) -> Vec<serde_json::Value> {
        let mut output = Vec::<u8>::new();
        for line in messages {
            let req: JsonRpcRequest = serde_json::from_str(line).unwrap();
            handle_request(&mut output, marker_path, req).unwrap();
        }
        std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    // a56: roles with no registered `submit_*` tool resolve to None. (The
    // `reviewer` role registered `submit_review` in a58 AND
    // `contradiction_check` registered `submit_contradictions` in a59, so
    // neither is in this list.)
    #[test]
    fn submission_tool_for_role_returns_none_until_a_role_registers() {
        for role in ["scout", "whatever", "implementer"] {
            assert!(
                submission_tool_for_role(role).is_none(),
                "role `{role}` must have no registered submit tool yet"
            );
        }
    }

    // a56: with `ORCH_MCP_ROLE` set to a role with no registered `submit_*`
    // tool (the `implementer`), `tools/list` still advertises the common
    // tools AND advertises no submission tool.
    #[test]
    fn tools_list_with_role_env_advertises_common_tools_and_no_submission_tool() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: env writes are serialized via ENV_LOCK.
        unsafe {
            std::env::set_var(ENV_ROLE, "implementer");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
        );
        // Clear the env var BEFORE asserting so a failed assert cannot leak
        // it into a sibling test.
        unsafe {
            std::env::remove_var(ENV_ROLE);
        }
        let tools = resps[0]["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"query_canonical_specs"), "common tools present: {names:?}");
        assert!(names.contains(&"ask_user"), "common tools present: {names:?}");
        assert!(
            !names.iter().any(|n| n.starts_with("submit_")),
            "no submission tool is wired in this change: {names:?}"
        );
    }

    // a57: `submit_findings` is advertised for each of the three advisory
    // roles, carrying that role's audit-specific finding schema alongside
    // the common tools.
    #[test]
    fn submit_findings_advertised_for_each_advisory_role() {
        let _guard = ENV_LOCK.lock().unwrap();
        for (role, required_field) in [
            ("drift_audit", "capability"),
            ("architecture_consultative", "subject"),
            ("documentation_audit", "category"),
        ] {
            // SAFETY: env writes are serialized via ENV_LOCK.
            unsafe {
                std::env::set_var(ENV_ROLE, role);
            }
            let dir = TempDir::new().unwrap();
            let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
            let resps = run_with(
                &marker,
                &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
            );
            unsafe {
                std::env::remove_var(ENV_ROLE);
            }
            let tools = resps[0]["result"]["tools"].as_array().expect("tools array");
            let submit = tools
                .iter()
                .find(|t| t["name"] == "submit_findings")
                .unwrap_or_else(|| panic!("role `{role}` must advertise submit_findings"));
            // Common tools coexist with the role's submission tool.
            let names: Vec<&str> =
                tools.iter().filter_map(|t| t["name"].as_str()).collect();
            assert!(names.contains(&"query_canonical_specs"), "role `{role}`: {names:?}");
            assert!(names.contains(&"ask_user"), "role `{role}`: {names:?}");
            // The advertised schema requires the role's distinguishing field.
            let props = &submit["inputSchema"]["properties"]["findings"]["items"]["properties"];
            assert!(
                props.get(required_field).is_some(),
                "role `{role}`: finding schema must define `{required_field}`: {submit}"
            );
            let required: Vec<&str> = submit["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert_eq!(required, vec!["findings"], "role `{role}` top-level required");
        }
    }

    // a57: the architecture role's findings schema caps the array at 5.
    #[test]
    fn submit_findings_architecture_schema_caps_array_at_five() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_ROLE, "architecture_consultative");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
        );
        unsafe {
            std::env::remove_var(ENV_ROLE);
        }
        let tools = resps[0]["result"]["tools"].as_array().unwrap();
        let submit = tools
            .iter()
            .find(|t| t["name"] == "submit_findings")
            .unwrap();
        assert_eq!(submit["inputSchema"]["properties"]["findings"]["maxItems"], 5);
    }

    // a57: `submit_findings` is NOT advertised for the executor or the
    // specs-writing audits.
    #[test]
    fn submit_findings_absent_for_non_advisory_roles() {
        let _guard = ENV_LOCK.lock().unwrap();
        for role in ["implementer", "missing_tests", "security_bug"] {
            unsafe {
                std::env::set_var(ENV_ROLE, role);
            }
            let dir = TempDir::new().unwrap();
            let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
            let resps = run_with(
                &marker,
                &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
            );
            unsafe {
                std::env::remove_var(ENV_ROLE);
            }
            let tools = resps[0]["result"]["tools"].as_array().unwrap();
            assert!(
                !tools.iter().any(|t| t["name"] == "submit_findings"),
                "role `{role}` must NOT advertise submit_findings"
            );
        }
    }

    #[test]
    fn submission_tool_name_for_role_covers_only_advisory_roles() {
        assert_eq!(submission_tool_name_for_role("drift_audit"), Some("submit_findings"));
        assert_eq!(
            submission_tool_name_for_role("architecture_consultative"),
            Some("submit_findings")
        );
        assert_eq!(
            submission_tool_name_for_role("documentation_audit"),
            Some("submit_findings")
        );
        // a58: the reviewer role gets `submit_review`, not `submit_findings`.
        assert_eq!(submission_tool_name_for_role("reviewer"), Some("submit_review"));
        // a59: the contradiction check gets `submit_contradictions`.
        assert_eq!(
            submission_tool_name_for_role("contradiction_check"),
            Some("submit_contradictions")
        );
        // a62: the `[canon]` gate gets `submit_canon_contradictions`.
        assert_eq!(
            submission_tool_name_for_role("canon_contradiction_check"),
            Some("submit_canon_contradictions")
        );
        // a75: the canon-internal contradiction AUDIT gets the symmetric
        // `submit_canon_internal_contradictions` (distinct from a62's
        // change-vs-canon tool).
        assert_eq!(
            submission_tool_name_for_role("canon_contradiction_audit"),
            Some("submit_canon_internal_contradictions")
        );
        assert_eq!(submission_tool_name_for_role("implementer"), None);
        assert_eq!(submission_tool_name_for_role("missing_tests"), None);
        assert_eq!(submission_tool_name_for_role("security_bug"), None);
    }

    // a58 (4.6): `submit_review` is advertised ONLY when
    // `ORCH_MCP_ROLE = reviewer`, with the verdict enum + concerns schema,
    // alongside the common tools — AND NOT for any other role.
    #[test]
    fn submit_review_advertised_only_for_reviewer_role() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Reviewer role: submit_review present with the verdict enum.
        unsafe {
            std::env::set_var(ENV_ROLE, "reviewer");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
        );
        unsafe {
            std::env::remove_var(ENV_ROLE);
        }
        let tools = resps[0]["result"]["tools"].as_array().expect("tools array");
        let submit = tools
            .iter()
            .find(|t| t["name"] == "submit_review")
            .expect("reviewer role must advertise submit_review");
        let verdict_enum = submit["inputSchema"]["properties"]["verdict"]["enum"]
            .as_array()
            .expect("verdict enum present");
        let labels: Vec<&str> = verdict_enum.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(labels, vec!["Approve", "Block"], "verdict enum is Approve|Block");
        // submit_findings is NOT present for the reviewer.
        assert!(
            !tools.iter().any(|t| t["name"] == "submit_findings"),
            "reviewer must not advertise submit_findings"
        );
        // Common tools coexist.
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"ask_user"), "common tools present: {names:?}");

        // Other roles: submit_review is absent.
        for role in ["implementer", "drift_audit", "missing_tests", "security_bug"] {
            assert!(
                submission_tool_for_role(role)
                    .map(|t| t["name"] != "submit_review")
                    .unwrap_or(true),
                "role `{role}` must NOT advertise submit_review"
            );
        }
    }

    // a59 (3.6): `submit_contradictions` is advertised ONLY when
    // `ORCH_MCP_ROLE = contradiction_check`, with the requirement-pair
    // schema, alongside the common tools — AND NOT for any other role.
    #[test]
    fn submit_contradictions_advertised_only_for_contradiction_check_role() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_ROLE, "contradiction_check");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
        );
        unsafe {
            std::env::remove_var(ENV_ROLE);
        }
        let tools = resps[0]["result"]["tools"].as_array().expect("tools array");
        let submit = tools
            .iter()
            .find(|t| t["name"] == "submit_contradictions")
            .expect("contradiction_check role must advertise submit_contradictions");
        // The advertised schema requires the requirement-pair fields.
        let item_props =
            &submit["inputSchema"]["properties"]["contradictions"]["items"]["properties"];
        for field in ["requirement_a", "requirement_b", "summary"] {
            assert!(
                item_props.get(field).is_some(),
                "contradiction item schema must define `{field}`: {submit}"
            );
        }
        let required: Vec<&str> = submit["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["contradictions"], "top-level required");
        // submit_findings AND submit_review are NOT present for this role.
        assert!(
            !tools.iter().any(|t| t["name"] == "submit_findings"),
            "contradiction_check must not advertise submit_findings"
        );
        assert!(
            !tools.iter().any(|t| t["name"] == "submit_review"),
            "contradiction_check must not advertise submit_review"
        );
        // Common tools coexist.
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"ask_user"), "common tools present: {names:?}");
        assert!(
            names.contains(&"query_canonical_specs"),
            "common tools present: {names:?}"
        );

        // Other roles: submit_contradictions is absent.
        for role in ["implementer", "reviewer", "drift_audit", "missing_tests"] {
            assert!(
                submission_tool_for_role(role)
                    .map(|t| t["name"] != "submit_contradictions")
                    .unwrap_or(true),
                "role `{role}` must NOT advertise submit_contradictions"
            );
        }
    }

    // a62 (4.6): `submit_canon_contradictions` is advertised ONLY when
    // `ORCH_MCP_ROLE = canon_contradiction_check`, with the change-vs-canonical
    // schema (each item names the conflicting canonical requirement), alongside
    // the common tools — AND NOT for any other role.
    #[test]
    fn submit_canon_contradictions_advertised_only_for_canon_check_role() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_ROLE, "canon_contradiction_check");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
        );
        unsafe {
            std::env::remove_var(ENV_ROLE);
        }
        let tools = resps[0]["result"]["tools"].as_array().expect("tools array");
        let submit = tools
            .iter()
            .find(|t| t["name"] == "submit_canon_contradictions")
            .expect("canon_contradiction_check role must advertise submit_canon_contradictions");
        // The advertised schema requires the change-vs-canonical fields — each
        // finding names the conflicting canonical requirement.
        let item_props =
            &submit["inputSchema"]["properties"]["contradictions"]["items"]["properties"];
        for field in [
            "change_requirement",
            "canonical_capability",
            "canonical_requirement",
            "summary",
        ] {
            assert!(
                item_props.get(field).is_some(),
                "canon contradiction item schema must define `{field}`: {submit}"
            );
        }
        let required: Vec<&str> = submit["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["contradictions"], "top-level required");
        // The within-change `submit_contradictions`, `submit_findings`, AND
        // `submit_review` are NOT present for this role.
        assert!(
            !tools.iter().any(|t| t["name"] == "submit_contradictions"),
            "canon_contradiction_check must not advertise the within-change submit_contradictions"
        );
        assert!(
            !tools.iter().any(|t| t["name"] == "submit_findings"),
            "canon_contradiction_check must not advertise submit_findings"
        );
        assert!(
            !tools.iter().any(|t| t["name"] == "submit_review"),
            "canon_contradiction_check must not advertise submit_review"
        );
        // Common tools coexist — including `query_canonical_specs`, the RAG
        // retrieval tool the gate uses when a21's canonical_rag is enabled.
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"ask_user"), "common tools present: {names:?}");
        assert!(
            names.contains(&"query_canonical_specs"),
            "query_canonical_specs (a21 RAG) must be advertised as a common tool: {names:?}"
        );

        // Other roles: submit_canon_contradictions is absent.
        for role in [
            "implementer",
            "reviewer",
            "contradiction_check",
            "drift_audit",
            "missing_tests",
        ] {
            assert!(
                submission_tool_for_role(role)
                    .map(|t| t["name"] != "submit_canon_contradictions")
                    .unwrap_or(true),
                "role `{role}` must NOT advertise submit_canon_contradictions"
            );
        }
    }

    // a75 (task 6.6): `submit_canon_internal_contradictions` is advertised
    // ONLY when `ORCH_MCP_ROLE = canon_contradiction_audit`, with the
    // SYMMETRIC schema (both sides name a canonical requirement by
    // capability + title), alongside the common tools — AND NOT for any
    // other role (notably NOT for a62's `canon_contradiction_check`).
    #[test]
    fn submit_canon_internal_contradictions_advertised_only_for_audit_role() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_ROLE, "canon_contradiction_audit");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
        );
        unsafe {
            std::env::remove_var(ENV_ROLE);
        }
        let tools = resps[0]["result"]["tools"].as_array().expect("tools array");
        let submit = tools
            .iter()
            .find(|t| t["name"] == "submit_canon_internal_contradictions")
            .expect("audit role must advertise submit_canon_internal_contradictions");
        // The advertised schema is symmetric: both sides name a canonical
        // requirement by capability + title.
        let item_props =
            &submit["inputSchema"]["properties"]["contradictions"]["items"]["properties"];
        for field in [
            "capability_a",
            "requirement_a",
            "capability_b",
            "requirement_b",
            "summary",
        ] {
            assert!(
                item_props.get(field).is_some(),
                "canon-internal contradiction item schema must define `{field}`: {submit}"
            );
        }
        let required: Vec<&str> = submit["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["contradictions"], "top-level required");
        // The a62 change-vs-canon tool, `submit_findings`, AND `submit_review`
        // are NOT present for this role.
        assert!(
            !tools.iter().any(|t| t["name"] == "submit_canon_contradictions"),
            "the audit role must not advertise a62's change-vs-canon submit_canon_contradictions"
        );
        assert!(
            !tools.iter().any(|t| t["name"] == "submit_findings"),
            "canon_contradiction_audit must not advertise submit_findings"
        );
        // Common tools coexist — including `query_canonical_specs`, the RAG
        // retrieval tool the audit uses when a21's canonical_rag is enabled.
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"ask_user"), "common tools present: {names:?}");
        assert!(
            names.contains(&"query_canonical_specs"),
            "query_canonical_specs (a21 RAG) must be advertised as a common tool: {names:?}"
        );

        // Other roles: submit_canon_internal_contradictions is absent.
        for role in [
            "implementer",
            "reviewer",
            "canon_contradiction_check",
            "drift_audit",
            "documentation_audit",
        ] {
            assert!(
                submission_tool_for_role(role)
                    .map(|t| t["name"] != "submit_canon_internal_contradictions")
                    .unwrap_or(true),
                "role `{role}` must NOT advertise submit_canon_internal_contradictions"
            );
        }
    }

    // a63 (task 4.6): `submit_verdict` is advertised ONLY when the MCP child
    // serves the `code_implements_spec` role (the `[out]` gate) — never for any
    // other role — AND carries the verdict + gaps schema alongside the common
    // tools.
    #[test]
    fn submit_verdict_advertised_only_for_code_implements_spec_role() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_ROLE, "code_implements_spec");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#],
        );
        unsafe {
            std::env::remove_var(ENV_ROLE);
        }
        let tools = resps[0]["result"]["tools"].as_array().expect("tools array");
        let submit = tools
            .iter()
            .find(|t| t["name"] == "submit_verdict")
            .expect("code_implements_spec role must advertise submit_verdict");
        // The advertised schema enforces the verdict enum AND the gap shape.
        let verdict_enum = &submit["inputSchema"]["properties"]["verdict"]["enum"];
        let variants: Vec<&str> = verdict_enum
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(variants, vec!["implemented", "gaps_found"], "verdict enum");
        let gap_props = &submit["inputSchema"]["properties"]["gaps"]["items"]["properties"];
        for field in ["requirement", "scenario", "status", "evidence"] {
            assert!(
                gap_props.get(field).is_some(),
                "gap item schema must define `{field}`: {submit}"
            );
        }
        let required: Vec<&str> = submit["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["verdict", "summary", "gaps"], "top-level required");
        // No OTHER role's submission tool leaks into this role.
        for other in [
            "submit_findings",
            "submit_review",
            "submit_contradictions",
            "submit_canon_contradictions",
        ] {
            assert!(
                !tools.iter().any(|t| t["name"] == other),
                "code_implements_spec must not advertise {other}"
            );
        }
        // Common tools coexist.
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"ask_user"), "common tools present: {names:?}");

        // Other roles: submit_verdict is absent.
        for role in [
            "implementer",
            "reviewer",
            "contradiction_check",
            "canon_contradiction_check",
            "drift_audit",
        ] {
            assert!(
                submission_tool_for_role(role)
                    .map(|t| t["name"] != "submit_verdict")
                    .unwrap_or(true),
                "role `{role}` must NOT advertise submit_verdict"
            );
        }
    }

    // a57: a `submit_findings` tool call relays the payload to the daemon
    // via the `record_submission` control-socket action, tagged with the
    // child's role.
    #[test]
    fn submit_findings_relays_via_socket() {
        let _g = ENV_LOCK.lock().unwrap();
        let socket_dir = TempDir::new().unwrap();
        let socket_path = socket_dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let received: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let received_clone = received.clone();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            std::io::BufRead::read_line(&mut reader, &mut buf).unwrap();
            let req: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            *received_clone.lock().unwrap() = Some(req);
            let response = serde_json::json!({"ok": true});
            let mut s = serde_json::to_string(&response).unwrap();
            s.push('\n');
            stream.write_all(s.as_bytes()).unwrap();
        });
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, socket_path.to_string_lossy().to_string());
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "drift_audit");
            std::env::set_var(ENV_ROLE, "drift_audit");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"submit_findings","arguments":{"findings":[{"capability":"orchestrator-cli","requirement":"Polling loop","severity":"high","code_anchors":["src/x.rs:1"],"divergence":"spec X, code Y"}]}}}"#],
        );
        handle.join().unwrap();
        assert_eq!(resps[0]["result"]["isError"], false, "resp: {:?}", resps[0]);
        let recv = received.lock().unwrap().take().unwrap();
        assert_eq!(recv["action"], "record_submission");
        assert_eq!(recv["workspace_basename"], "test-ws");
        assert_eq!(recv["change"], "drift_audit");
        assert_eq!(recv["role"], "drift_audit");
        assert_eq!(recv["payload"]["findings"][0]["capability"], "orchestrator-cli");
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
            std::env::remove_var(ENV_ROLE);
        }
    }

    // a57: a schema rejection from the daemon comes back as a correctable
    // tool error (-32602), not a silent success.
    #[test]
    fn submit_findings_schema_rejection_surfaces_as_tool_error() {
        let _g = ENV_LOCK.lock().unwrap();
        let socket_dir = TempDir::new().unwrap();
        let socket_path = socket_dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            std::io::BufRead::read_line(&mut reader, &mut buf).unwrap();
            // Daemon rejects (e.g. >5 architecture findings).
            let response = serde_json::json!({"ok": false, "error": "findings array exceeds the cap of 5"});
            let mut s = serde_json::to_string(&response).unwrap();
            s.push('\n');
            stream.write_all(s.as_bytes()).unwrap();
        });
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, socket_path.to_string_lossy().to_string());
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "architecture_consultative");
            std::env::set_var(ENV_ROLE, "architecture_consultative");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":43,"method":"tools/call","params":{"name":"submit_findings","arguments":{"findings":[]}}}"#],
        );
        handle.join().unwrap();
        assert_eq!(resps[0]["error"]["code"], -32602, "resp: {:?}", resps[0]);
        let msg = resps[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("cap of 5"), "error must carry the daemon's reason: {msg}");
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
            std::env::remove_var(ENV_ROLE);
        }
    }

    // a69 / task 4.6: the submission contract holds for the roles `agy`
    // serves (here `reviewer` / `submit_review`) — a schema-invalid payload
    // comes back as a CORRECTABLE tool error (-32602) the agent can fix AND
    // resubmit in the SAME session. CLI-agnostic: the relay/validation lives
    // here, not in any strategy, so it applies identically to the antigravity
    // path. Mirrors `submit_findings_schema_rejection_surfaces_as_tool_error`,
    // adding the corrected-resubmit-succeeds half.
    #[test]
    fn submit_review_rejection_is_correctable_and_resubmit_succeeds_in_session() {
        let _g = ENV_LOCK.lock().unwrap();
        let socket_dir = TempDir::new().unwrap();
        let socket_path = socket_dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = std::thread::spawn(move || {
            // Each submission is one connection; the relay reads the response
            // until EOF, so the stream must be CLOSED (dropped) after the
            // reply — hence the per-connection blocks.
            // First submission: the daemon rejects (schema-invalid).
            {
                let (mut s, _) = listener.accept().unwrap();
                let mut r = std::io::BufReader::new(s.try_clone().unwrap());
                let mut buf = String::new();
                std::io::BufRead::read_line(&mut r, &mut buf).unwrap();
                let mut rej = serde_json::to_string(
                    &serde_json::json!({"ok": false, "error": "schema invalid: missing 'verdict'"}),
                )
                .unwrap();
                rej.push('\n');
                s.write_all(rej.as_bytes()).unwrap();
            }
            // Second (corrected) submission in the same session: accepted.
            {
                let (mut s, _) = listener.accept().unwrap();
                let mut r = std::io::BufReader::new(s.try_clone().unwrap());
                let mut buf = String::new();
                std::io::BufRead::read_line(&mut r, &mut buf).unwrap();
                let mut acc = serde_json::to_string(&serde_json::json!({"ok": true})).unwrap();
                acc.push('\n');
                s.write_all(acc.as_bytes()).unwrap();
            }
        });
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, socket_path.to_string_lossy().to_string());
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "reviewer");
            std::env::set_var(ENV_ROLE, "reviewer");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"submit_review","arguments":{"approved":false}}}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"submit_review","arguments":{"approved":true,"summary":"ok"}}}"#,
            ],
        );
        handle.join().unwrap();
        // The rejection is a correctable tool error carrying the daemon reason.
        assert_eq!(resps[0]["error"]["code"], -32602, "resp0: {:?}", resps[0]);
        let msg = resps[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("missing 'verdict'"), "carries the daemon reason: {msg}");
        // The corrected resubmit succeeds within the SAME session (a result,
        // no error).
        assert!(
            resps[1].get("error").is_none(),
            "the corrected resubmit must succeed: {:?}",
            resps[1]
        );
        assert!(resps[1]["result"].is_object(), "resp1 has a result: {:?}", resps[1]);
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
            std::env::remove_var(ENV_ROLE);
        }
    }

    #[test]
    fn initialize_returns_capabilities() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#],
        );
        assert_eq!(resps.len(), 1);
        assert_eq!(resps[0]["id"], 1);
        assert_eq!(resps[0]["result"]["serverInfo"]["name"], "autocoder-mcp");
        assert!(resps[0]["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn provided_tool_names_matches_tools_list_response() {
        // Regression: when a27a0/a27a1 added outcome_* tools to the
        // tools/list response, PROVIDED_TOOL_NAMES wasn't updated, so the
        // executor's auto-allow path didn't include them — every
        // outcome-tool call hit Claude CLI's permission gate AND failed
        // with `permission denied`. This caused a30-release-glibc-floor
        // to perma-stuck across 3 iterations. This test fails the build
        // if the two sources drift again.
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#],
        );
        let tools = resps[0]["result"]["tools"].as_array().unwrap();
        let advertised: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();

        // Every advertised tool MUST be in PROVIDED_TOOL_NAMES so the
        // executor's auto-allow path includes it in --allowedTools.
        for name in &advertised {
            assert!(
                PROVIDED_TOOL_NAMES.contains(name),
                "tool `{name}` advertised in tools/list but missing from PROVIDED_TOOL_NAMES — add it to the const so the executor auto-allows it"
            );
        }

        // Every name in PROVIDED_TOOL_NAMES MUST actually exist in
        // tools/list — listing a non-existent tool in --allowedTools is
        // harmless but suggests stale const.
        for name in PROVIDED_TOOL_NAMES {
            assert!(
                advertised.contains(name),
                "PROVIDED_TOOL_NAMES contains `{name}` but it's not in tools/list — either remove it from the const OR add it to the tools/list response"
            );
        }
    }

    #[test]
    fn tools_list_advertises_both_tools() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#],
        );
        let tools = resps[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"ask_user"));
        assert!(names.contains(&"query_canonical_specs"));
        let rag_tool = tools
            .iter()
            .find(|t| t["name"] == "query_canonical_specs")
            .unwrap();
        assert!(rag_tool["inputSchema"]["properties"]["query"].is_object());
        let required: Vec<&str> = rag_tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["query"]);
    }

    #[test]
    fn tools_call_ask_user_writes_marker_file() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/feature/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"What should we name the project?"}}}"#],
        );
        assert_eq!(resps[0]["id"], 3);
        assert_eq!(resps[0]["result"]["isError"], false);

        assert!(marker.is_file(), "marker file must be written");
        let contents: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        assert_eq!(contents["question"], "What should we name the project?");
    }

    #[test]
    fn tools_call_unknown_tool_returns_error() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"banana","arguments":{}}}"#],
        );
        assert_eq!(resps[0]["id"], 4);
        let err = &resps[0]["error"];
        assert_eq!(err["code"], -32601);
        assert!(err["message"].as_str().unwrap().contains("banana"));
    }

    #[test]
    fn notifications_initialized_emits_no_response() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#],
        );
        assert!(resps.is_empty(), "notifications must not produce responses");
    }

    #[test]
    fn unknown_method_returns_error_response() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#],
        );
        assert_eq!(resps[0]["error"]["code"], -32601);
    }

    #[test]
    fn query_canonical_specs_env_absent_returns_not_configured_hint() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"query_canonical_specs","arguments":{"query":"audit cadence"}}}"#],
        );
        let structured = &resps[0]["result"]["structuredContent"];
        assert!(structured["hits"].as_array().unwrap().is_empty());
        assert_eq!(
            structured["error_hint"].as_str().unwrap(),
            "rag not configured for this execution"
        );
    }

    #[test]
    fn query_canonical_specs_relays_via_socket() {
        let _g = ENV_LOCK.lock().unwrap();
        let socket_dir = TempDir::new().unwrap();
        let socket_path = socket_dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        // Spawn a thread that answers ONE request with a canned response
        // and exits.
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            std::io::BufRead::read_line(&mut reader, &mut buf).unwrap();
            // Echo what we got plus a fixed hits array.
            let response = serde_json::json!({
                "ok": true,
                "hits": [
                    {"capability": "audits", "requirement_title": "Audit cadence",
                     "requirement_body": "...", "scenario_titles": [], "relevance_score": 0.9}
                ],
            });
            let mut s = serde_json::to_string(&response).unwrap();
            s.push('\n');
            stream.write_all(s.as_bytes()).unwrap();
        });
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, socket_path.to_string_lossy().to_string());
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"query_canonical_specs","arguments":{"query":"audit cadence","top_k":3}}}"#],
        );
        handle.join().unwrap();
        let structured = &resps[0]["result"]["structuredContent"];
        let hits = structured["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["capability"], "audits");
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
        }
    }

    // ----- a27a0: outcome tools -----

    #[test]
    fn tools_list_advertises_outcome_tools_with_schemas() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":99,"method":"tools/list"}"#],
        );
        let tools = resps[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> =
            tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"outcome_success"), "names: {names:?}");
        assert!(
            names.contains(&"outcome_spec_needs_revision"),
            "names: {names:?}"
        );
        let success_tool = tools
            .iter()
            .find(|t| t["name"] == "outcome_success")
            .unwrap();
        // outcome_success: no required fields, but `final_answer` is documented.
        assert!(
            success_tool["inputSchema"]["properties"]["final_answer"].is_object()
        );

        let revision_tool = tools
            .iter()
            .find(|t| t["name"] == "outcome_spec_needs_revision")
            .unwrap();
        let required: Vec<&str> = revision_tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"unimplementable_tasks"));
        assert!(required.contains(&"revision_suggestion"));
    }

    #[test]
    fn validate_spec_needs_revision_accepts_valid_input() {
        let args = serde_json::json!({
            "unimplementable_tasks": [
                {"task_id": "6.4", "task_text": "Manual: SSH...", "reason": "no SSH"}
            ],
            "revision_suggestion": "Replace 6.4 with a mocked unit test"
        });
        let out = validate_spec_needs_revision_args(&args).unwrap();
        assert_eq!(out["type"], "spec_needs_revision");
        assert_eq!(out["revision_suggestion"], "Replace 6.4 with a mocked unit test");
    }

    #[test]
    fn validate_spec_needs_revision_rejects_missing_revision_suggestion() {
        let args = serde_json::json!({
            "unimplementable_tasks": [
                {"task_id": "6.4", "task_text": "t", "reason": "r"}
            ]
        });
        let err = validate_spec_needs_revision_args(&args).unwrap_err();
        assert!(
            err.contains("revision_suggestion"),
            "err should name field: {err}"
        );
        assert!(err.contains("missing"), "err should say missing: {err}");
    }

    #[test]
    fn validate_spec_needs_revision_rejects_empty_task_array() {
        let args = serde_json::json!({
            "unimplementable_tasks": [],
            "revision_suggestion": "r"
        });
        let err = validate_spec_needs_revision_args(&args).unwrap_err();
        assert!(err.contains("non-empty"), "err: {err}");
    }

    #[test]
    fn validate_spec_needs_revision_rejects_missing_required_subfield() {
        let args = serde_json::json!({
            "unimplementable_tasks": [
                {"task_id": "6.4", "task_text": "t"}
            ],
            "revision_suggestion": "r"
        });
        let err = validate_spec_needs_revision_args(&args).unwrap_err();
        assert!(err.contains("reason"), "err should name `reason`: {err}");
    }

    #[test]
    fn validate_spec_needs_revision_rejects_wrong_type_field() {
        let args = serde_json::json!({
            "unimplementable_tasks": [
                {"task_id": 6, "task_text": "t", "reason": "r"}
            ],
            "revision_suggestion": "r"
        });
        let err = validate_spec_needs_revision_args(&args).unwrap_err();
        assert!(err.contains("task_id"), "err: {err}");
        assert!(
            err.contains("must be a string"),
            "err should mention type: {err}"
        );
    }

    #[test]
    fn validate_spec_needs_revision_rejects_empty_string_field() {
        let args = serde_json::json!({
            "unimplementable_tasks": [
                {"task_id": "", "task_text": "t", "reason": "r"}
            ],
            "revision_suggestion": "r"
        });
        let err = validate_spec_needs_revision_args(&args).unwrap_err();
        assert!(err.contains("non-empty"), "err: {err}");
    }

    #[test]
    fn validate_spec_needs_revision_rejects_placeholder_in_task_id() {
        let args = serde_json::json!({
            "unimplementable_tasks": [
                {"task_id": "<id-from-tasks-md>", "task_text": "t", "reason": "r"}
            ],
            "revision_suggestion": "r"
        });
        let err = validate_spec_needs_revision_args(&args).unwrap_err();
        assert!(err.contains("placeholder"), "err: {err}");
        assert!(err.contains("task_id"), "err: {err}");
    }

    #[test]
    fn validate_spec_needs_revision_rejects_placeholder_in_revision_suggestion() {
        let args = serde_json::json!({
            "unimplementable_tasks": [
                {"task_id": "6.4", "task_text": "t", "reason": "r"}
            ],
            "revision_suggestion": "<concrete edit>"
        });
        let err = validate_spec_needs_revision_args(&args).unwrap_err();
        assert!(err.contains("placeholder"), "err: {err}");
        assert!(err.contains("revision_suggestion"), "err: {err}");
    }

    #[test]
    fn placeholder_marker_detector_accepts_legitimate_brackets() {
        // Mirrors the daemon-side detector's positive/negative cases.
        for s in &["<HTML>", "<MyType>", "<3>", "no brackets at all"] {
            assert!(
                !contains_placeholder_marker(s),
                "did not expect `{s}` to match"
            );
        }
        for s in &[
            "<id-from-tasks-md>",
            "<verbatim quote>",
            "<one-line why>",
        ] {
            assert!(contains_placeholder_marker(s), "expected `{s}` to match");
        }
    }

    #[test]
    fn outcome_success_with_non_string_final_answer_returns_invalid_params() {
        // Test by invoking tools/call directly through run_with;
        // outcome_success accepts no final_answer or a string. A non-string
        // (here a number) must produce JSON-RPC -32602.
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"outcome_success","arguments":{"final_answer":42}}}"#],
        );
        assert_eq!(resps[0]["error"]["code"], -32602);
    }

    #[test]
    fn outcome_success_socket_absent_returns_internal_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":101,"method":"tools/call","params":{"name":"outcome_success","arguments":{"final_answer":"done"}}}"#],
        );
        // -32603 = internal error (relay failure).
        assert_eq!(resps[0]["error"]["code"], -32603);
    }

    #[test]
    fn outcome_success_relays_via_socket() {
        let _g = ENV_LOCK.lock().unwrap();
        let socket_dir = TempDir::new().unwrap();
        let socket_path = socket_dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let received: Arc<Mutex<Option<serde_json::Value>>> =
            Arc::new(Mutex::new(None));
        let received_clone = received.clone();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            std::io::BufRead::read_line(&mut reader, &mut buf).unwrap();
            let req: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            *received_clone.lock().unwrap() = Some(req);
            let response = serde_json::json!({"ok": true});
            let mut s = serde_json::to_string(&response).unwrap();
            s.push('\n');
            stream.write_all(s.as_bytes()).unwrap();
        });
        unsafe {
            std::env::set_var(
                ENV_CONTROL_SOCKET,
                socket_path.to_string_lossy().to_string(),
            );
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "a30-foo");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":102,"method":"tools/call","params":{"name":"outcome_success","arguments":{"final_answer":"all done"}}}"#],
        );
        handle.join().unwrap();
        assert_eq!(resps[0]["result"]["isError"], false);
        let recv = received.lock().unwrap().take().unwrap();
        assert_eq!(recv["action"], "record_outcome");
        assert_eq!(recv["workspace_basename"], "test-ws");
        assert_eq!(recv["change"], "a30-foo");
        assert_eq!(recv["outcome"]["type"], "success");
        assert_eq!(recv["outcome"]["final_answer"], "all done");
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
        }
    }

    #[test]
    fn outcome_spec_needs_revision_relays_via_socket_on_valid_input() {
        let _g = ENV_LOCK.lock().unwrap();
        let socket_dir = TempDir::new().unwrap();
        let socket_path = socket_dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let received: Arc<Mutex<Option<serde_json::Value>>> =
            Arc::new(Mutex::new(None));
        let received_clone = received.clone();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            std::io::BufRead::read_line(&mut reader, &mut buf).unwrap();
            let req: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            *received_clone.lock().unwrap() = Some(req);
            let response = serde_json::json!({"ok": true});
            let mut s = serde_json::to_string(&response).unwrap();
            s.push('\n');
            stream.write_all(s.as_bytes()).unwrap();
        });
        unsafe {
            std::env::set_var(
                ENV_CONTROL_SOCKET,
                socket_path.to_string_lossy().to_string(),
            );
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "a30-foo");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":103,"method":"tools/call","params":{"name":"outcome_spec_needs_revision","arguments":{"unimplementable_tasks":[{"task_id":"6.4","task_text":"Manual: SSH...","reason":"no SSH"}],"revision_suggestion":"Mock systemctl status"}}}"#],
        );
        handle.join().unwrap();
        assert_eq!(resps[0]["result"]["isError"], false);
        let recv = received.lock().unwrap().take().unwrap();
        assert_eq!(recv["action"], "record_outcome");
        assert_eq!(recv["outcome"]["type"], "spec_needs_revision");
        assert_eq!(
            recv["outcome"]["unimplementable_tasks"][0]["task_id"],
            "6.4"
        );
        assert_eq!(recv["outcome"]["revision_suggestion"], "Mock systemctl status");
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
        }
    }

    #[test]
    fn outcome_spec_needs_revision_placeholder_returns_invalid_params_without_relay() {
        let _g = ENV_LOCK.lock().unwrap();
        // Pointing the env at a nonexistent socket: if the validator
        // FAILS to short-circuit, the relay will try to connect AND
        // produce a -32603 error. We assert -32602 (validation), proving
        // the control socket was NOT contacted.
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, "/nonexistent/control.sock");
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "a30-foo");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":104,"method":"tools/call","params":{"name":"outcome_spec_needs_revision","arguments":{"unimplementable_tasks":[{"task_id":"<id-from-tasks-md>","task_text":"<verbatim quote>","reason":"<one-line why>"}],"revision_suggestion":"<concrete edit>"}}}"#],
        );
        assert_eq!(resps[0]["error"]["code"], -32602);
        let msg = resps[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("placeholder"), "msg: {msg}");
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
        }
    }

    #[test]
    fn outcome_spec_needs_revision_socket_unreachable_returns_internal_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, "/nonexistent/control.sock");
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "a30-foo");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":105,"method":"tools/call","params":{"name":"outcome_spec_needs_revision","arguments":{"unimplementable_tasks":[{"task_id":"6.4","task_text":"t","reason":"r"}],"revision_suggestion":"s"}}}"#],
        );
        assert_eq!(resps[0]["error"]["code"], -32603);
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
        }
    }

    // ----- a27a1: outcome_request_iteration -----

    #[test]
    fn tools_list_advertises_outcome_request_iteration() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":200,"method":"tools/list"}"#],
        );
        let tools = resps[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> =
            tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(
            names.contains(&"outcome_request_iteration"),
            "names: {names:?}"
        );
        let tool = tools
            .iter()
            .find(|t| t["name"] == "outcome_request_iteration")
            .unwrap();
        let required: Vec<&str> = tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"completed_tasks"));
        assert!(required.contains(&"remaining_tasks"));
        assert!(required.contains(&"reason"));
        // minItems: 1 documented for both arrays.
        assert_eq!(
            tool["inputSchema"]["properties"]["completed_tasks"]["minItems"], 1
        );
        assert_eq!(
            tool["inputSchema"]["properties"]["remaining_tasks"]["minItems"], 1
        );
    }

    #[test]
    fn validate_request_iteration_accepts_valid_input() {
        let args = serde_json::json!({
            "completed_tasks": ["1", "2"],
            "remaining_tasks": ["3"],
            "reason": "task 3 needs a refactor I want to plan more carefully"
        });
        let out = validate_request_iteration_args(&args).unwrap();
        assert_eq!(out["type"], "iteration_request");
        assert_eq!(out["completed_tasks"][0], "1");
        assert_eq!(out["remaining_tasks"][0], "3");
        assert_eq!(
            out["reason"],
            "task 3 needs a refactor I want to plan more carefully"
        );
    }

    #[test]
    fn validate_request_iteration_rejects_empty_completed_tasks() {
        let args = serde_json::json!({
            "completed_tasks": [],
            "remaining_tasks": ["3"],
            "reason": "r"
        });
        let err = validate_request_iteration_args(&args).unwrap_err();
        assert!(err.contains("completed_tasks"), "err: {err}");
        assert!(err.contains("non-empty"), "err: {err}");
    }

    #[test]
    fn validate_request_iteration_rejects_empty_remaining_tasks() {
        let args = serde_json::json!({
            "completed_tasks": ["1"],
            "remaining_tasks": [],
            "reason": "r"
        });
        let err = validate_request_iteration_args(&args).unwrap_err();
        assert!(err.contains("remaining_tasks"), "err: {err}");
        assert!(err.contains("non-empty"), "err: {err}");
    }

    #[test]
    fn validate_request_iteration_rejects_empty_reason() {
        let args = serde_json::json!({
            "completed_tasks": ["1"],
            "remaining_tasks": ["3"],
            "reason": ""
        });
        let err = validate_request_iteration_args(&args).unwrap_err();
        assert!(err.contains("reason"), "err: {err}");
        assert!(err.contains("non-empty"), "err: {err}");
    }

    #[test]
    fn validate_request_iteration_rejects_placeholder_in_reason() {
        let args = serde_json::json!({
            "completed_tasks": ["1"],
            "remaining_tasks": ["3"],
            "reason": "<concrete blocker>"
        });
        let err = validate_request_iteration_args(&args).unwrap_err();
        assert!(err.contains("placeholder"), "err: {err}");
        assert!(err.contains("reason"), "err: {err}");
    }

    #[test]
    fn validate_request_iteration_rejects_placeholder_in_completed_tasks_element() {
        let args = serde_json::json!({
            "completed_tasks": ["1", "<task-id>"],
            "remaining_tasks": ["3"],
            "reason": "r"
        });
        let err = validate_request_iteration_args(&args).unwrap_err();
        assert!(err.contains("placeholder"), "err: {err}");
        assert!(err.contains("completed_tasks"), "err: {err}");
    }

    #[test]
    fn validate_request_iteration_rejects_placeholder_in_remaining_tasks_element() {
        let args = serde_json::json!({
            "completed_tasks": ["1"],
            "remaining_tasks": ["<task-id>"],
            "reason": "r"
        });
        let err = validate_request_iteration_args(&args).unwrap_err();
        assert!(err.contains("placeholder"), "err: {err}");
        assert!(err.contains("remaining_tasks"), "err: {err}");
    }

    #[test]
    fn outcome_request_iteration_invalid_input_returns_invalid_params() {
        // The MCP layer must short-circuit on validation failure WITHOUT
        // contacting the control socket. Pointing ENV_CONTROL_SOCKET at a
        // nonexistent path verifies the short-circuit: if the validator
        // FAILED to fire, the relay would attempt a connection AND produce
        // -32603. We assert -32602, which proves the socket wasn't touched.
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, "/nonexistent/control.sock");
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "a30-foo");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":201,"method":"tools/call","params":{"name":"outcome_request_iteration","arguments":{"completed_tasks":["1"],"remaining_tasks":["3"],"reason":"<concrete blocker>"}}}"#],
        );
        assert_eq!(resps[0]["error"]["code"], -32602);
        let msg = resps[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("placeholder"), "msg: {msg}");
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
        }
    }

    #[test]
    fn outcome_request_iteration_relays_via_socket_on_valid_input() {
        let _g = ENV_LOCK.lock().unwrap();
        let socket_dir = TempDir::new().unwrap();
        let socket_path = socket_dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let received: Arc<Mutex<Option<serde_json::Value>>> =
            Arc::new(Mutex::new(None));
        let received_clone = received.clone();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            std::io::BufRead::read_line(&mut reader, &mut buf).unwrap();
            let req: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            *received_clone.lock().unwrap() = Some(req);
            let response = serde_json::json!({"ok": true});
            let mut s = serde_json::to_string(&response).unwrap();
            s.push('\n');
            stream.write_all(s.as_bytes()).unwrap();
        });
        unsafe {
            std::env::set_var(
                ENV_CONTROL_SOCKET,
                socket_path.to_string_lossy().to_string(),
            );
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "a30-foo");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":202,"method":"tools/call","params":{"name":"outcome_request_iteration","arguments":{"completed_tasks":["1","2"],"remaining_tasks":["3"],"reason":"task 3 needs a refactor"}}}"#],
        );
        handle.join().unwrap();
        assert_eq!(resps[0]["result"]["isError"], false);
        let recv = received.lock().unwrap().take().unwrap();
        assert_eq!(recv["action"], "record_outcome");
        assert_eq!(recv["workspace_basename"], "test-ws");
        assert_eq!(recv["change"], "a30-foo");
        assert_eq!(recv["outcome"]["type"], "iteration_request");
        assert_eq!(recv["outcome"]["completed_tasks"][0], "1");
        assert_eq!(recv["outcome"]["completed_tasks"][1], "2");
        assert_eq!(recv["outcome"]["remaining_tasks"][0], "3");
        assert_eq!(recv["outcome"]["reason"], "task 3 needs a refactor");
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
        }
    }

    #[test]
    fn outcome_request_iteration_socket_unreachable_returns_internal_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, "/nonexistent/control.sock");
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
            std::env::set_var(ENV_CHANGE, "a30-foo");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":203,"method":"tools/call","params":{"name":"outcome_request_iteration","arguments":{"completed_tasks":["1"],"remaining_tasks":["3"],"reason":"r"}}}"#],
        );
        assert_eq!(resps[0]["error"]["code"], -32603);
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
            std::env::remove_var(ENV_CHANGE);
        }
    }

    // ----- end a27a1 -----

    // ----- end a27a0 -----

    #[test]
    fn query_canonical_specs_socket_unreachable_returns_hint() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(ENV_CONTROL_SOCKET, "/nonexistent/control.sock");
            std::env::set_var(ENV_WORKSPACE_BASENAME, "test-ws");
        }
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"query_canonical_specs","arguments":{"query":"x"}}}"#],
        );
        let structured = &resps[0]["result"]["structuredContent"];
        assert!(structured["hits"].as_array().unwrap().is_empty());
        let hint = structured["error_hint"].as_str().unwrap();
        assert!(
            hint.contains("control socket unreachable"),
            "hint should name socket-unreachable; got: {hint}"
        );
        unsafe {
            std::env::remove_var(ENV_CONTROL_SOCKET);
            std::env::remove_var(ENV_WORKSPACE_BASENAME);
        }
    }

    // ----- a48: outcome-tool descriptions are served, non-empty -----

    /// Structural behavior test (a48, replacing the a44 substring-marker
    /// contract): drive the server's `tools/list` response in-process and
    /// assert each outcome tool is advertised with a non-empty
    /// `description`. This checks the served structure — that the
    /// descriptions exist and are populated — not any hand-authored
    /// wording of their prose. The descriptions' operational fitness is
    /// design intent verified by the drift audit (per the executor
    /// requirement "MCP outcome-tool description fields encourage
    /// substantive content..." AND the project-documentation requirement
    /// "Tests assert behavior or derivation, never message wording").
    #[test]
    fn each_outcome_tool_advertised_with_nonempty_description() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("openspec/changes/x/.askuser-pending.json");
        let resps = run_with(
            &marker,
            &[r#"{"jsonrpc":"2.0","id":300,"method":"tools/list"}"#],
        );
        let tools = resps[0]["result"]["tools"].as_array().unwrap();

        for tool in [
            "outcome_success",
            "outcome_request_iteration",
            "outcome_spec_needs_revision",
        ] {
            let tool_obj = tools
                .iter()
                .find(|t| t["name"] == tool)
                .unwrap_or_else(|| panic!("tools/list missing tool `{tool}`"));
            let description = tool_obj["description"].as_str().unwrap_or_else(|| {
                panic!("tool `{tool}` description is not a string in tools/list")
            });
            assert!(
                !description.trim().is_empty(),
                "tool `{tool}` must be advertised with a non-empty description"
            );
        }
    }

    // ----- end a48 -----
}
