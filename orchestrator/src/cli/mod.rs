//! CLI argument parsing + dispatch. Each subcommand's execute function lives
//! in its own submodule.

use crate::config;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub mod rewind;
pub mod run;

#[derive(Parser, Debug)]
#[command(name = "orchestrator")]
#[command(about = "Autonomous OpenSpec CI/CD orchestrator", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the orchestrator daemon. Polls every configured repository on its
    /// interval, processes ready OpenSpec changes, and opens monolithic PRs.
    Run {
        /// Path to the YAML configuration file.
        #[arg(long)]
        config: PathBuf,
    },

    /// Recover from a failed PR or bad implementation by unarchiving named
    /// changes and resetting the agent branch.
    Rewind {
        /// One or more change names to unarchive.
        #[arg(required = true)]
        changes: Vec<String>,

        /// Path to the YAML configuration file.
        #[arg(long)]
        config: PathBuf,

        /// Skip the confirmation prompt and force-delete the agent branch
        /// locally and remotely.
        #[arg(long, default_value_t = false)]
        hard: bool,

        /// Repository URL or short-name (basename without .git). Required
        /// when config has multiple repositories.
        #[arg(long)]
        repo: Option<String>,
    },
}

pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Run { config } => {
            let cfg = config::Config::load_from(&config)?;
            run::execute(cfg).await
        }
        Command::Rewind {
            changes,
            config: config_path,
            hard,
            repo,
        } => {
            let cfg = config::Config::load_from(&config_path)?;
            rewind::execute(
                cfg.repositories,
                rewind::RewindArgs {
                    changes,
                    hard,
                    repo,
                },
            )
            .await
        }
    }
}
