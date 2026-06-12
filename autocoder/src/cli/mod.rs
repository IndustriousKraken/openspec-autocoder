//! CLI argument parsing + dispatch. Each subcommand's execute function lives
//! in its own submodule.

use crate::config;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Resolve a `DaemonPaths` from env vars only (no config file). Used by
/// CLI subcommands that talk to the running daemon (or run standalone)
/// AND don't take a `--config` flag. The env-driven resolution mirrors
/// the daemon's own startup priority order minus the config override:
/// AUTOCODER_*_DIR → systemd dirs → XDG defaults → hard fallback.
pub fn resolve_paths_from_env() -> Result<crate::paths::DaemonPaths> {
    let cfg = config::Config {
        repositories: vec![],
        executor: config::ExecutorConfig {
            kind: config::ExecutorKind::ClaudeCli,
            implementer_cli: None,
            command: String::new(),
            timeout_secs: 60,
            sandbox: None,
            agent_env: None,
            implementer_prompt_path: None,
            changelog_stylist_prompt_path: None,
            perma_stuck_after_failures: None,
            max_changes_per_pr: None,
            startup_jitter_max_secs: None,
            inter_iteration_jitter_pct: None,
            max_auto_revisions_per_pr: 5,
            max_revise_triggers_per_pr: 10,
            wipe_drain_timeout_secs: config::default_wipe_drain_timeout_secs(),
            output_format: config::default_output_format(),
            log_retention_days: config::default_log_retention_days(),
            busy_marker_stale_threshold_secs: None,
            change_internal_contradiction_check: config::ContradictionCheckMode::Disabled,
            change_internal_contradiction_check_prompt_path: None,
            change_internal_contradiction_check_llm: None,
            change_canonical_contradiction_check:
                crate::config::ContradictionCheckMode::Disabled,
            change_canonical_contradiction_check_prompt_path: None,
            change_canonical_contradiction_check_llm: None,
            code_implements_spec_check:
                crate::config::ContradictionCheckMode::Disabled,
            code_implements_spec_check_prompt_path: None,
            code_implements_spec_check_llm: None,
            verifier_gate_retries: crate::config::default_verifier_gate_retries(),
            implementer: None,
            changelog_stylist: None,
            implementer_revision: None,
            audit_triage: None,
            chat_request_triage: None,
        },
        github: config::GithubConfig {
            token_env: "GITHUB_TOKEN".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
            command_authorization: Default::default(),
        },
        reviewer: None,
        chatops: None,
        audits: None,
        paths: config::DaemonPathsConfig::default(),
        cache: config::CacheConfig::default(),
        features: config::FeaturesConfig::default(),
        canonical_rag: None,
        models: None,
    };
    crate::paths::resolve_daemon_paths(&cfg)
}

pub mod audit;
pub mod changelog;
pub mod check_config;
pub mod doctor;
pub mod inspect;
pub mod install;
pub mod pkg_manager;
pub mod reload;
pub mod rewind;
pub mod run;
pub mod sync_specs;
pub mod sync_specs_deps;

#[derive(Parser, Debug)]
#[command(name = "autocoder")]
#[command(version = env!("AUTOCODER_VERSION"))]
#[command(about = "Autonomous AI code-writer driven by OpenSpec changes", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum InspectSubcommand {
    /// Query the running daemon's canonical-spec RAG store and render the
    /// hits as a human-readable table. Wraps the `query_canonical_specs`
    /// control-socket action.
    Rag {
        /// Workspace basename (e.g. `github_com_owner_repo`) OR repo
        /// URL. When omitted, the daemon's single configured workspace
        /// is used; if there are zero OR multiple, the command exits
        /// non-zero with the available basenames listed.
        #[arg(long)]
        workspace: Option<String>,

        /// The query text to send to the RAG store.
        #[arg(long)]
        query: String,

        /// Top-K results to request. Defaults to the daemon's
        /// `canonical_rag.top_k` when omitted.
        #[arg(long)]
        top_k: Option<u32>,

        /// Render the first 500 characters of each hit's
        /// `requirement_body` below the table.
        #[arg(long, default_value_t = false)]
        show_bodies: bool,

        /// Print the raw control-socket response JSON to stdout
        /// instead of the formatted table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Pretty-print a per-change stream log with tool calls grouped AND
    /// query/result pairs aligned. Reads `<logs_dir>/runs/<basename>/<change>.stream.log`.
    Log {
        /// Workspace basename or URL; see `inspect rag --workspace`.
        #[arg(long)]
        workspace: Option<String>,

        /// The change name (matches `<change>.stream.log` in the
        /// workspace's runs directory).
        change: String,

        /// Cap rendered tool-call event groups at N. Default 30;
        /// `--limit 0` means unlimited.
        #[arg(long)]
        limit: Option<u32>,

        /// Print the parsed event stream as a JSON array instead of
        /// the formatted output.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Aggregate stats from a per-change stream log (tool-call counts,
    /// duration, `query_canonical_specs` distribution).
    ToolUsage {
        /// Workspace basename or URL; see `inspect rag --workspace`.
        #[arg(long)]
        workspace: Option<String>,

        /// The change name (matches `<change>.stream.log` in the
        /// workspace's runs directory).
        change: String,

        /// Print the aggregated stats as a structured JSON object
        /// instead of the formatted output.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum AuditSubcommand {
    /// Trigger an audit for a workspace. With the daemon running, the
    /// CLI sends a `queue_audit` action via the control socket so the
    /// next polling iteration runs the audit. Without the daemon, the
    /// audit module is invoked directly against the workspace and
    /// findings print to stdout.
    Run {
        /// Path to the workspace directory.
        #[arg(long)]
        workspace: PathBuf,

        /// Audit type name (e.g. `security_bug_audit`). The exact
        /// `audit_type` slug — substring matching is reserved for the
        /// chatops verb.
        #[arg(long)]
        audit: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the autocoder daemon. Polls every configured repository on its
    /// interval, processes ready OpenSpec changes, and opens monolithic PRs.
    Run {
        /// Path to the YAML configuration file. When omitted, the path is
        /// discovered from the installed systemd unit's `ExecStart`, then
        /// from the default locations (`/etc/autocoder/config.yaml`, then
        /// `~/.config/autocoder/config.yaml`).
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Run the dependency preflight on demand and print the full report:
    /// every required dependency (`openspec`, `git`, a usable sandbox
    /// mechanism) AND every dependency implied by the active configuration.
    /// Exits non-zero when a required dependency is missing or unusable.
    Doctor {
        /// Path to the YAML configuration file whose strategies/features
        /// drive the configuration-implied checks. When omitted, the same
        /// discovery as `run` is used; if no config is found the required
        /// dependencies are still checked.
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Validate a config file against this binary's schema, without
    /// running the daemon. Exits 0 on a clean config, 1 on
    /// warnings-only (typically unset env vars referenced by `*_env`
    /// fields), 2 on at least one hard error. Use this as a CI gate
    /// AND as the preflight before `update.sh` swaps a new binary into
    /// place.
    CheckConfig(check_config::CheckConfigArgs),

    /// Internal: stdio MCP server exposing the `ask_user` tool. Launched
    /// by the wrapped CLI agent (via the workspace's `.mcp.json` config),
    /// NOT invoked directly by humans.
    #[command(hide = true)]
    McpAskUserServer,

    /// Reload the running daemon's hot-applicable config sections (github,
    /// reviewer, chatops) from the on-disk YAML the daemon was launched
    /// with. Connects to the daemon's control socket; exits non-zero if
    /// the daemon is not running or the new YAML fails validation.
    Reload,

    /// First-run wizard. Collects the minimum configuration an operator
    /// needs (one repo URL, a GitHub PAT, optional chatops + reviewer),
    /// writes config.yaml + secrets.env, and on server mode renders +
    /// enables a systemd unit. Idempotent: re-running against an existing
    /// config prints a status line and exits 0.
    Install(install::InstallArgs),

    /// Rebuild every canonical spec under `openspec/specs/` from the
    /// archived change history under `openspec/changes/archive/`. The
    /// rebuild iterates archives chronologically and replays each via
    /// `openspec archive` so canonical state is exactly what it would be
    /// if every archive had synced correctly the first time. See the
    /// "Rebuilding canonical specs" section of the README for the
    /// operator's perspective.
    SyncSpecs {
        /// Path to the workspace (the directory containing
        /// `openspec/changes/archive/`).
        #[arg(long)]
        workspace: PathBuf,

        /// Run the full rebuild. There is no incremental mode; this
        /// flag exists for clarity and future-proofing. Defaults to
        /// true.
        #[arg(long, default_value_t = true)]
        rebuild: bool,

        /// SIGTERM the running executor subprocess (if any) before
        /// starting the rebuild. Without this flag the CLI waits
        /// politely for the current iteration to finish. No-op when
        /// no daemon is running on the workspace.
        #[arg(long, default_value_t = false)]
        immediate: bool,
    },

    /// On-demand audit triggers (chatops-on-demand-audit-trigger). The
    /// `run` subcommand queues an audit for the daemon's next polling
    /// iteration when the daemon is reachable, OR invokes the audit
    /// module directly against the named workspace when no daemon is
    /// running (useful for prompt-template iteration).
    Audit {
        #[command(subcommand)]
        command: AuditSubcommand,
    },

    /// Harvest a release-notes changelog from the OpenSpec archive of a
    /// workspace. Walks `openspec/changes/archive/`, finds archive
    /// directories added within a tag range, and renders markdown (default)
    /// or JSON to stdout. Pure-data extractor: no LLM, no mutation, no
    /// daemon work. Same archive contents + same tag range produce the
    /// same output every invocation.
    Changelog(changelog::ChangelogArgs),

    /// Operator-friendly diagnostic surface that wraps the existing
    /// log + control-socket primitives. Three subsubcommands: `rag`
    /// queries the canonical RAG store; `log` pretty-prints a stream
    /// log; `tool-usage` aggregates stats from a stream log.
    Inspect {
        #[command(subcommand)]
        command: InspectSubcommand,
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
            let resolved = run::resolve_run_config_path(config)?;
            let cfg = config::Config::load_from(&resolved)?;
            run::execute(cfg, resolved).await
        }
        Command::Doctor { config } => doctor::execute(config).await,
        Command::CheckConfig(args) => check_config::execute(args).await,
        Command::Install(args) => install::execute(args).await,
        Command::Reload => reload::execute().await,
        Command::McpAskUserServer => crate::mcp_askuser_server::run(),
        Command::SyncSpecs {
            workspace,
            rebuild,
            immediate,
        } => {
            sync_specs::execute(sync_specs::SyncSpecsArgs {
                workspace,
                rebuild,
                immediate,
            })
            .await
        }
        Command::Changelog(args) => changelog::execute(args).await,
        Command::Audit { command } => match command {
            AuditSubcommand::Run { workspace, audit } => {
                audit::execute(workspace, audit).await
            }
        },
        Command::Inspect { command } => inspect::dispatch(command).await,
        Command::Rewind {
            changes,
            config: config_path,
            hard,
            repo,
        } => {
            let cfg = config::Config::load_from(&config_path)?;
            rewind::execute(
                cfg.repositories,
                cfg.github,
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
