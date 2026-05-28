use anyhow::Result;
use clap::Parser;

mod alert_state;
mod alerts;
mod audits;
mod busy_marker;
mod changelog_requests;
mod changelog_triage;
mod chatops;
mod cli;
mod code_reviewer;
mod config;
mod control_socket;
mod executor;
mod failure_state;
mod git;
mod github;
mod github_credentials;
mod llm;
mod log_retention;
mod mcp_askuser_server;
mod migration;
mod openspec_archive;
mod paths;
mod perma_stuck;
mod polling_loop;
mod proposal_requests;
mod queue;
mod recovery_classification;
mod revisions;
mod spec_revision;
#[cfg(test)]
mod testing;
mod workspace;

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
