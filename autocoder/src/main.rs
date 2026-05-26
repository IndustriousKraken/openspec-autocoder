use anyhow::Result;
use clap::Parser;

mod alert_state;
mod alerts;
mod audits;
mod busy_marker;
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
mod mcp_askuser_server;
mod perma_stuck;
mod polling_loop;
mod queue;
mod revisions;
mod spec_revision;
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
