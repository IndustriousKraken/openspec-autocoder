use anyhow::Result;
use clap::Parser;

mod agent_env;
mod agentic_run;
mod alert_state;
mod alert_state_migration;
mod alerts;
mod attribution;
mod audits;
mod busy_marker;
mod changelog_requests;
mod changelog_triage;
mod chatops;
mod cli;
mod code_implements_spec;
mod code_review_suggestion;
mod code_reviewer;
mod config;
mod control_socket;
mod daemon;
mod dependency_preflight;
mod executor;
mod failure_state;
mod forge;
mod git;
mod github_credentials;
// a007: `github.rs` moved into the `forge` module (`forge::github`) as the
// `GithubForge` REST layer. This crate-root alias preserves every existing
// `crate::github::*` path so the extraction stays behavior-preserving — the
// REST code now physically lives inside `src/forge/`, the single source of
// truth, while call sites that have not yet been routed through the `Forge`
// trait keep compiling unchanged.
pub(crate) use forge::github;
mod ignore_for_queue;
mod iteration_pending;
mod lanes;
mod llm;
mod log_retention;
mod mcp_askuser_server;
mod migration;
mod openspec_archive;
mod outcome_store;
mod paths;
mod perma_stuck;
mod polling;
mod polling_loop;
mod preflight;
mod prompts;
mod proposal_requests;
mod queue;
mod rag;
mod recovery_classification;
mod revisions;
mod sandbox;
mod spec_revision;
mod spec_root;
mod spec_storage_routing;
mod state;
mod submission_store;
#[cfg(test)]
mod testing;
mod tool_probe;
mod verifier_gate;
mod workspace;
mod workspace_cache;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = cli::Cli::parse();
    cli::dispatch(cli).await
}
