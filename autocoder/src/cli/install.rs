//! `autocoder install` — interactive (or non-interactive) first-run wizard
//! and idempotent re-install entry point. install.sh swaps the binary and
//! execs this subcommand; everything OS-mutating (`useradd`, `systemctl`,
//! `apt-get install`, the claude installer subprocess) happens here behind
//! the [`SystemActions`] trait so `cargo test` can drive the whole flow with
//! a recording mock.
//!
//! The bundled `config.example.yaml` is the source of truth for what fields
//! exist; the wizard deserializes a copy and mutates it. No string-splicing
//! or sed against YAML — serde does the round trip.
use crate::config::{
    AuditsConfig, Cadence, ChatOpsConfig, ChatOpsProvider, Config, GithubConfig, ReviewerConfig,
    ReviewerProvider, SecretSource, SlackProviderConfig,
};
use std::collections::HashMap;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::{Args, ValueEnum};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::fs;

const BUNDLED_EXAMPLE: &str = include_str!("../../../config.example.yaml");
const SYSTEMD_UNIT: &str = include_str!("./install_systemd.service");
const SERVER_CONFIG_DIR: &str = "/etc/autocoder";
const SERVER_BINARY_PATH: &str = "/usr/local/bin/autocoder";
const CLAUDE_INSTALL_URL: &str = "https://claude.ai/install.sh";

#[derive(Args, Debug, Clone, Default)]
pub struct InstallArgs {
    /// Install mode. Default: `server` on Linux with systemd, `dev` otherwise.
    #[arg(long, value_enum)]
    pub mode: Option<InstallMode>,

    /// Override the config directory (default `/etc/autocoder/` for server,
    /// `~/.config/autocoder/` for dev).
    #[arg(long)]
    pub config_dir: Option<PathBuf>,

    /// Run end-to-end with no stdin reads. Required values must be supplied
    /// via the flags below.
    #[arg(long, default_value_t = false)]
    pub non_interactive: bool,

    /// Skip the existing-config short-circuit and re-run the wizard.
    #[arg(long, default_value_t = false)]
    pub upgrade: bool,

    /// Re-prompt one section of the install wizard against an existing
    /// install and patch the existing `config.yaml` in place. Accepted
    /// values: `audits`, `reviewer`, `chatops`. See [`ReconfigureSection`]
    /// for the rationale on what is excluded.
    #[arg(
        long,
        value_enum,
        conflicts_with_all = [
            "non_interactive",
            "repo_url",
            "base_branch",
            "agent_branch",
            "poll_interval_sec",
            "token_env_var",
            "chatops_backend",
            "chatops_channel_id",
            "reviewer_provider",
            "reviewer_model",
            "audits_llm_driven",
            "audit_architecture_brightline",
            "audit_architecture_consultative",
            "audit_drift_audit",
            "audit_missing_tests_audit",
            "audit_security_bug_audit",
            "audit_documentation_audit",
        ]
    )]
    pub reconfigure: Option<ReconfigureSection>,

    // ---------- wizard pre-fill / non-interactive answers ----------
    #[arg(long)]
    pub repo_url: Option<String>,
    #[arg(long)]
    pub base_branch: Option<String>,
    #[arg(long)]
    pub agent_branch: Option<String>,
    #[arg(long)]
    pub poll_interval_sec: Option<u64>,
    #[arg(long)]
    pub token_env_var: Option<String>,
    #[arg(long, value_enum)]
    pub chatops_backend: Option<ChatOpsBackendArg>,
    #[arg(long)]
    pub chatops_channel_id: Option<String>,
    #[arg(long, value_enum)]
    pub reviewer_provider: Option<ReviewerProviderArg>,
    #[arg(long)]
    pub reviewer_model: Option<String>,

    // ---------- audits ----------
    /// Master switch for the LLM-driven audits (architecture_brightline,
    /// architecture_consultative, drift_audit, missing_tests_audit,
    /// security_bug_audit, documentation_audit). Default `none`.
    #[arg(long, value_enum)]
    pub audits_llm_driven: Option<LlmDrivenAuditsArg>,

    /// Override the cadence for `architecture_brightline`. Only consulted
    /// when `--audits-llm-driven recommended`. Ignored under `none` /
    /// `all-disabled` (the master switch wins).
    #[arg(long, value_enum)]
    pub audit_architecture_brightline: Option<AuditCadenceArg>,
    #[arg(long, value_enum)]
    pub audit_architecture_consultative: Option<AuditCadenceArg>,
    #[arg(long, value_enum)]
    pub audit_drift_audit: Option<AuditCadenceArg>,
    #[arg(long, value_enum)]
    pub audit_missing_tests_audit: Option<AuditCadenceArg>,
    #[arg(long, value_enum)]
    pub audit_security_bug_audit: Option<AuditCadenceArg>,
    #[arg(long, value_enum)]
    pub audit_documentation_audit: Option<AuditCadenceArg>,

    // ---------- canonical-spec RAG (a21) ----------
    /// Canonical-spec RAG provider in non-interactive mode. `none`
    /// (default when omitted) writes no `canonical_rag:` block.
    #[arg(long, value_enum)]
    pub rag_provider: Option<RagProviderArg>,
    /// Embedding provider base URL. Required when `--rag-provider` is
    /// `ollama` or `openai_compatible`.
    #[arg(long)]
    pub rag_base_url: Option<String>,
    /// Embedding model identifier. Optional; defaults to
    /// `nomic-embed-text` for ollama AND has no default for
    /// openai_compatible (required).
    #[arg(long)]
    pub rag_model: Option<String>,
    /// Env-var name carrying the API key for `openai_compatible`. Required
    /// for that provider in non-interactive mode unless `--rag-api-key`
    /// is set inline.
    #[arg(long)]
    pub rag_api_key_env: Option<String>,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum InstallMode {
    Server,
    Dev,
}

/// Which slice of `config.yaml` the operator wants to re-prompt for via
/// `autocoder install --reconfigure <section>`. The wizard intentionally
/// excludes several knobs from this surface:
///
/// - `repositories`: add/remove flows hot-apply via `autocoder reload`; the
///   reconfigure verb deliberately does not grow into that space.
/// - `paths.*`: relocating the daemon data directories is a destructive
///   operation that needs explicit operator action AND a daemon restart.
/// - `executor.*`: every executor knob requires a restart; reconfigure
///   stays in the hot-applicable space.
/// - `audits.settings.*.prompt_path` and `audits.settings.*.extra.*`:
///   advanced per-audit overrides. The wizard handles only the
///   `audits.defaults.*` cadences; operators editing prompts or
///   thresholds edit YAML directly.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ReconfigureSection {
    Audits,
    Reviewer,
    Chatops,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ChatOpsBackendArg {
    None,
    Slack,
    Discord,
    Teams,
    Mattermost,
    Matrix,
}

/// Canonical-spec RAG provider in non-interactive mode (a21).
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq, Default)]
pub enum RagProviderArg {
    /// No `canonical_rag:` block; default behaviour (RAG disabled).
    #[default]
    None,
    /// Ollama (local or remote). Requires `--rag-base-url`.
    Ollama,
    /// OpenAI-compatible endpoint (Voyage, OpenRouter, llama.cpp, etc.).
    /// Requires `--rag-base-url` AND `--rag-api-key-env`.
    #[clap(name = "openai_compatible")]
    OpenaiCompatible,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ReviewerProviderArg {
    None,
    Anthropic,
    #[clap(name = "openai_compatible")]
    OpenAiCompatible,
    /// a37: local Ollama for the reviewer. No `api_key` is collected
    /// (Ollama does not authenticate); the wizard prompts for
    /// `api_base_url` and `model` only.
    Ollama,
}

/// Master switch for the LLM-driven audits in non-interactive mode. Mirrors
/// the interactive "Enable the LLM-driven audits? [y/N]" gate; the `recommended`
/// variant additionally accepts the fast-path defaults inline.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum LlmDrivenAuditsArg {
    /// Same as the default: no LLM-driven audits enabled.
    None,
    /// Enable every LLM-driven audit at its recommended cadence. Individual
    /// `--audit-<slug>` flags can override one cadence each.
    Recommended,
    /// Same as `none`, but prints a one-line acknowledgement so IaC logs
    /// can distinguish "operator opted out explicitly" from "operator did
    /// not pass the flag at all".
    AllDisabled,
}

/// Cadence choice for an individual audit. Mirrors `Cadence` minus the
/// `every-N-days` advanced form (operators wanting that edit config.yaml
/// post-install).
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum AuditCadenceArg {
    Disabled,
    Daily,
    Weekly,
    Monthly,
}

impl AuditCadenceArg {
    pub fn to_cadence(self) -> Cadence {
        match self {
            Self::Disabled => Cadence::Disabled,
            Self::Daily => Cadence::Daily,
            Self::Weekly => Cadence::Weekly,
            Self::Monthly => Cadence::Monthly,
        }
    }
}

/// Slugs of the audits the wizard knows about. The order is the order they
/// appear in the per-audit walk-through.
pub const LLM_DRIVEN_SLUGS: &[(&str, Cadence)] = &[
    ("architecture_brightline", Cadence::Weekly),
    ("drift_audit", Cadence::Weekly),
    ("missing_tests_audit", Cadence::Monthly),
    ("security_bug_audit", Cadence::Weekly),
    ("architecture_consultative", Cadence::Monthly),
    ("documentation_audit", Cadence::Monthly),
];

/// One-line operator-facing description per known audit slug. Mirrors each
/// audit impl's `Audit::description()` so the wizard does not need to
/// instantiate concrete audits to render the prompts.
fn audit_description(slug: &str) -> &'static str {
    match slug {
        "architecture_brightline" => {
            "file-size / module-size guidelines (architecture brightline)"
        }
        "architecture_consultative" => "advisory architecture findings via LLM consultation",
        "drift_audit" => "spec ↔ code drift detection (warns when reality outgrows the spec)",
        "missing_tests_audit" => "proposes test coverage for untested branches",
        "security_bug_audit" => "proposes fixes for likely security bugs",
        "documentation_audit" => {
            "documentation coverage / stale-reference / organization audit (LLM-driven)"
        }
        _ => "",
    }
}

fn cadence_label(c: Cadence) -> &'static str {
    match c {
        Cadence::Disabled => "disabled",
        Cadence::Daily => "daily",
        Cadence::Weekly => "weekly",
        Cadence::Monthly => "monthly",
        Cadence::Quarterly => "quarterly",
        Cadence::EveryNDays(_) => "every-n-days",
    }
}

/// Pre-fill values surfaced from `InstallArgs` to the wizard. In interactive
/// mode each `Some(...)` becomes the prompt default. In non-interactive mode
/// a missing required field is a fatal error.
#[derive(Debug, Default, Clone)]
pub struct WizardPrefill {
    pub repo_url: Option<String>,
    pub base_branch: Option<String>,
    pub agent_branch: Option<String>,
    pub poll_interval_sec: Option<u64>,
    pub token_env_var: Option<String>,
    pub chatops_backend: Option<ChatOpsBackendArg>,
    pub chatops_channel_id: Option<String>,
    pub reviewer_provider: Option<ReviewerProviderArg>,
    pub reviewer_model: Option<String>,
    pub audits_llm_driven: Option<LlmDrivenAuditsArg>,
    pub audit_architecture_brightline: Option<AuditCadenceArg>,
    pub audit_architecture_consultative: Option<AuditCadenceArg>,
    pub audit_drift_audit: Option<AuditCadenceArg>,
    pub audit_missing_tests_audit: Option<AuditCadenceArg>,
    pub audit_security_bug_audit: Option<AuditCadenceArg>,
    pub audit_documentation_audit: Option<AuditCadenceArg>,
    pub rag_provider: Option<RagProviderArg>,
    pub rag_base_url: Option<String>,
    pub rag_model: Option<String>,
    pub rag_api_key_env: Option<String>,
}

/// Final wizard output: everything the operator confirmed or accepted as
/// default. `assemble_config` + `assemble_secrets_env` consume this verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WizardAnswers {
    pub repo_url: String,
    pub base_branch: String,
    pub agent_branch: String,
    pub poll_interval_sec: u64,
    pub token_env_var: String,
    pub github_pat: Option<String>,
    pub chatops_backend: ChatOpsBackendArg,
    pub chatops_channel_id: Option<String>,
    pub chatops_token: Option<String>,
    pub reviewer_provider: ReviewerProviderArg,
    pub reviewer_model: Option<String>,
    pub reviewer_api_key: Option<String>,
    /// a37: reviewer base URL captured for `ollama` (and overridable for
    /// the other providers via reconfigure). `None` for the default
    /// hosted-API choice with `anthropic` / `openai_compatible`.
    pub reviewer_api_base_url: Option<String>,
    /// Resolved cadences per audit slug. Audits the operator declined are
    /// either absent from the map or stored as `Cadence::Disabled`. The
    /// config-assembly step drops `Disabled` entries before emitting YAML.
    pub audits: HashMap<String, Cadence>,
    /// Canonical-spec RAG block (a21). `None` → no block written.
    pub canonical_rag: Option<RagAnswers>,
}

/// Wizard-resolved canonical-spec RAG settings. `None` from the wizard
/// means the operator declined RAG OR the wizard fell through to disable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RagAnswers {
    pub provider: RagProviderArg,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
}

// Write `contents` to `path` such that the file never exists on disk with a
// mode wider than `mode`. On a fresh create the `mode` is applied in the same
// syscall that creates the file. If the file already exists (re-install), it
// is chmod'd down to `mode` BEFORE the truncate-and-rewrite, so the
// truncated-but-not-yet-rewritten state is also not world-readable.
#[cfg(unix)]
fn write_file_with_mode(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .mode(mode)
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    f.write_all(contents)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_file_with_mode(path: &Path, contents: &[u8], _mode: u32) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

// ----------------------------------------------------------------------------
// SystemActions — the OS-mutating surface. Production impl shells out;
// tests use RecordingActions to assert the orchestration without touching
// the real host.
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubprocessOutcome {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdUnitProbe {
    pub load_state: LoadState,
    pub fragment_path: Option<PathBuf>,
    pub exec_start_config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    Loaded,
    NotFound,
    Other(String),
}

#[async_trait]
pub trait SystemActions: Send + Sync {
    async fn which(&self, command: &str) -> Option<PathBuf>;
    async fn run_subprocess(&self, cmd: &str, args: &[&str]) -> Result<SubprocessOutcome>;
    async fn create_user(&self, name: &str, home_dir: &Path, shell: &str) -> Result<()>;
    async fn chown(&self, path: &Path, owner: &str, group: &str) -> Result<()>;
    async fn chmod(&self, path: &Path, mode: u32) -> Result<()>;
    async fn apt_install(&self, packages: &[&str]) -> Result<()>;
    async fn daemon_reload(&self) -> Result<()>;
    async fn enable_systemd_unit(&self, name: &str) -> Result<()>;
    async fn start_systemd_unit(&self, name: &str) -> Result<()>;
    async fn probe_systemd_unit(&self, unit_name: &str) -> Result<SystemdUnitProbe>;
}

pub struct RealSystemActions;

#[async_trait]
impl SystemActions for RealSystemActions {
    async fn which(&self, command: &str) -> Option<PathBuf> {
        let out = tokio::process::Command::new("which")
            .arg(command)
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(PathBuf::from(s)) }
    }

    async fn run_subprocess(&self, cmd: &str, args: &[&str]) -> Result<SubprocessOutcome> {
        let out = tokio::process::Command::new(cmd)
            .args(args)
            .output()
            .await
            .with_context(|| format!("failed to run `{cmd}`"))?;
        Ok(SubprocessOutcome {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }

    async fn create_user(&self, name: &str, home_dir: &Path, shell: &str) -> Result<()> {
        if self.which(&format!("id")).await.is_some() {
            let check = tokio::process::Command::new("id")
                .arg(name)
                .output()
                .await
                .ok();
            if let Some(o) = check {
                if o.status.success() {
                    return Ok(());
                }
            }
        }
        let status = tokio::process::Command::new("useradd")
            .args([
                "--system",
                "--home-dir",
                home_dir.to_str().unwrap_or("/var/lib/autocoder"),
                "--create-home",
                "--shell",
                shell,
                name,
            ])
            .status()
            .await
            .with_context(|| format!("failed to spawn useradd for `{name}`"))?;
        if !status.success() {
            bail!("useradd exited non-zero for user `{name}`");
        }
        Ok(())
    }

    async fn chown(&self, path: &Path, owner: &str, group: &str) -> Result<()> {
        let status = tokio::process::Command::new("chown")
            .arg(format!("{owner}:{group}"))
            .arg(path)
            .status()
            .await
            .context("failed to spawn chown")?;
        if !status.success() {
            bail!("chown failed for {}", path.display());
        }
        Ok(())
    }

    async fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
        let status = tokio::process::Command::new("chmod")
            .arg(format!("{mode:o}"))
            .arg(path)
            .status()
            .await
            .context("failed to spawn chmod")?;
        if !status.success() {
            bail!("chmod failed for {}", path.display());
        }
        Ok(())
    }

    async fn apt_install(&self, packages: &[&str]) -> Result<()> {
        if !Path::new("/etc/debian_version").exists() {
            return Ok(());
        }
        let mut args = vec!["install", "-y"];
        args.extend_from_slice(packages);
        let status = tokio::process::Command::new("apt-get")
            .args(&args)
            .status()
            .await
            .context("failed to spawn apt-get")?;
        if !status.success() {
            bail!("apt-get install failed for {packages:?}");
        }
        Ok(())
    }

    async fn daemon_reload(&self) -> Result<()> {
        let status = tokio::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .status()
            .await
            .context("systemctl daemon-reload")?;
        if !status.success() {
            bail!("systemctl daemon-reload failed");
        }
        Ok(())
    }

    async fn enable_systemd_unit(&self, name: &str) -> Result<()> {
        let status = tokio::process::Command::new("systemctl")
            .args(["enable", name])
            .status()
            .await
            .context("systemctl enable")?;
        if !status.success() {
            bail!("systemctl enable {name} failed");
        }
        Ok(())
    }

    async fn start_systemd_unit(&self, name: &str) -> Result<()> {
        let status = tokio::process::Command::new("systemctl")
            .args(["start", name])
            .status()
            .await
            .context("systemctl start")?;
        if !status.success() {
            bail!("systemctl start {name} failed");
        }
        Ok(())
    }

    async fn probe_systemd_unit(&self, unit_name: &str) -> Result<SystemdUnitProbe> {
        let out = tokio::process::Command::new("systemctl")
            .args([
                "show",
                unit_name,
                "-p",
                "LoadState",
                "-p",
                "FragmentPath",
                "-p",
                "ExecStart",
            ])
            .output()
            .await;
        let stdout = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            // Treat any failure to invoke or run systemctl as "no unit found";
            // we don't want a non-systemd host to fail the install.
            _ => {
                return Ok(SystemdUnitProbe {
                    load_state: LoadState::NotFound,
                    fragment_path: None,
                    exec_start_config_path: None,
                });
            }
        };
        Ok(parse_systemctl_show(&stdout))
    }
}

/// Parse the stdout of `systemctl show <unit> -p LoadState -p FragmentPath -p ExecStart`.
/// systemd emits one `KEY=VALUE` line per requested property. Unknown / unset
/// values come back as empty strings. The `ExecStart=` value is a structured
/// `{ path=... ; argv[]=... ; ... }` block; we scan it for the first
/// `--config <path>` pair.
pub(crate) fn parse_systemctl_show(stdout: &str) -> SystemdUnitProbe {
    let mut load_state = LoadState::NotFound;
    let mut fragment_path: Option<PathBuf> = None;
    let mut exec_start_config_path: Option<PathBuf> = None;

    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("LoadState=") {
            load_state = match rest.trim() {
                "loaded" => LoadState::Loaded,
                "not-found" => LoadState::NotFound,
                other => LoadState::Other(other.to_string()),
            };
        } else if let Some(rest) = line.strip_prefix("FragmentPath=") {
            let trimmed = rest.trim();
            fragment_path = if trimmed.is_empty() {
                None
            } else {
                Some(PathBuf::from(trimmed))
            };
        } else if let Some(rest) = line.strip_prefix("ExecStart=") {
            // Only set exec_start_config_path from the first ExecStart line
            // that yields one. systemd may emit multiple ExecStart= entries
            // when there are multiple ExecStart directives in the unit, but
            // the wizard cares about the first one that actually launches
            // autocoder.
            if exec_start_config_path.is_none() {
                exec_start_config_path = extract_config_arg(rest);
            }
        }
    }

    SystemdUnitProbe { load_state, fragment_path, exec_start_config_path }
}

/// Scan a systemd `ExecStart=` value for the first `--config <path>` token.
/// Returns `None` when `--config` is absent OR when the next token is another
/// `--<flag>` (operator wrote `--config` with no value).
fn extract_config_arg(exec_start: &str) -> Option<PathBuf> {
    let tokens: Vec<&str> = exec_start.split_whitespace().collect();
    let mut iter = tokens.iter().peekable();
    while let Some(tok) = iter.next() {
        if *tok == "--config" {
            if let Some(next) = iter.peek() {
                if next.starts_with("--") {
                    return None;
                }
                return Some(PathBuf::from(*next));
            }
            return None;
        }
        if let Some(rest) = tok.strip_prefix("--config=") {
            return Some(PathBuf::from(rest));
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedCall {
    Which(String),
    RunSubprocess { cmd: String, args: Vec<String> },
    CreateUser { name: String, home_dir: PathBuf, shell: String },
    Chown { path: PathBuf, owner: String, group: String },
    Chmod { path: PathBuf, mode: u32 },
    AptInstall(Vec<String>),
    DaemonReload,
    EnableSystemdUnit(String),
    StartSystemdUnit(String),
    ProbeSystemdUnit(String),
}

#[derive(Default)]
pub struct RecordingActions {
    pub calls: Mutex<Vec<RecordedCall>>,
    pub which_overrides: Mutex<std::collections::HashMap<String, Option<PathBuf>>>,
    pub apt_get_available: bool,
    pub probe_systemd_unit_responses: Mutex<HashMap<String, SystemdUnitProbe>>,
}

impl RecordingActions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_which(self, command: &str, result: Option<PathBuf>) -> Self {
        self.which_overrides
            .lock()
            .unwrap()
            .insert(command.to_string(), result);
        self
    }
    pub fn with_apt(mut self, available: bool) -> Self {
        self.apt_get_available = available;
        self
    }
    pub fn with_probe_response(self, unit_name: &str, probe: SystemdUnitProbe) -> Self {
        self.probe_systemd_unit_responses
            .lock()
            .unwrap()
            .insert(unit_name.to_string(), probe);
        self
    }
    pub fn record(&self, call: RecordedCall) {
        self.calls.lock().unwrap().push(call);
    }
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl SystemActions for RecordingActions {
    async fn which(&self, command: &str) -> Option<PathBuf> {
        self.record(RecordedCall::Which(command.to_string()));
        if let Some(over) = self.which_overrides.lock().unwrap().get(command) {
            return over.clone();
        }
        if command == "apt-get" {
            return if self.apt_get_available {
                Some(PathBuf::from("/usr/bin/apt-get"))
            } else {
                None
            };
        }
        None
    }
    async fn run_subprocess(&self, cmd: &str, args: &[&str]) -> Result<SubprocessOutcome> {
        self.record(RecordedCall::RunSubprocess {
            cmd: cmd.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        });
        Ok(SubprocessOutcome { status: 0, stdout: String::new(), stderr: String::new() })
    }
    async fn create_user(&self, name: &str, home_dir: &Path, shell: &str) -> Result<()> {
        self.record(RecordedCall::CreateUser {
            name: name.to_string(),
            home_dir: home_dir.to_path_buf(),
            shell: shell.to_string(),
        });
        Ok(())
    }
    async fn chown(&self, path: &Path, owner: &str, group: &str) -> Result<()> {
        self.record(RecordedCall::Chown {
            path: path.to_path_buf(),
            owner: owner.to_string(),
            group: group.to_string(),
        });
        Ok(())
    }
    async fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
        self.record(RecordedCall::Chmod { path: path.to_path_buf(), mode });
        Ok(())
    }
    async fn apt_install(&self, packages: &[&str]) -> Result<()> {
        self.record(RecordedCall::AptInstall(
            packages.iter().map(|s| s.to_string()).collect(),
        ));
        Ok(())
    }
    async fn daemon_reload(&self) -> Result<()> {
        self.record(RecordedCall::DaemonReload);
        Ok(())
    }
    async fn enable_systemd_unit(&self, name: &str) -> Result<()> {
        self.record(RecordedCall::EnableSystemdUnit(name.to_string()));
        Ok(())
    }
    async fn start_systemd_unit(&self, name: &str) -> Result<()> {
        self.record(RecordedCall::StartSystemdUnit(name.to_string()));
        Ok(())
    }
    async fn probe_systemd_unit(&self, unit_name: &str) -> Result<SystemdUnitProbe> {
        self.record(RecordedCall::ProbeSystemdUnit(unit_name.to_string()));
        if let Some(p) = self
            .probe_systemd_unit_responses
            .lock()
            .unwrap()
            .get(unit_name)
        {
            return Ok(p.clone());
        }
        Ok(SystemdUnitProbe {
            load_state: LoadState::NotFound,
            fragment_path: None,
            exec_start_config_path: None,
        })
    }
}

// ----------------------------------------------------------------------------
// WizardIo — operator-facing prompts. Production impl reads stdin; tests use
// ScriptedIo to drive a pre-loaded answer queue and assert on the prompt
// stream the wizard emitted.
// ----------------------------------------------------------------------------

#[async_trait]
pub trait WizardIo: Send {
    async fn read_line(&mut self) -> Result<String>;
    async fn read_password(&mut self) -> Result<String>;
    fn print(&mut self, s: &str);
    async fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool>;
    async fn choose(
        &mut self,
        prompt: &str,
        options: &[&str],
        default_idx: usize,
    ) -> Result<usize>;
}

pub struct StdioWizardIo;

#[async_trait]
impl WizardIo for StdioWizardIo {
    async fn read_line(&mut self) -> Result<String> {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).context("stdin read_line")?;
        Ok(s.trim_end_matches(['\n', '\r']).to_string())
    }
    async fn read_password(&mut self) -> Result<String> {
        // Not using a true silent read here to avoid adding `rpassword`.
        // The wizard prints a "(input not echoed on a real terminal)" hint
        // and falls back to the same stdin line read. Production operators
        // should run install via the bootstrap, which uses a TTY where the
        // upstream rpassword dep would normally suppress echo — left as a
        // future enhancement.
        self.read_line().await
    }
    fn print(&mut self, s: &str) {
        print!("{s}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    async fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        self.print(&format!("{prompt} {hint} "));
        let line = self.read_line().await?;
        let line = line.trim().to_lowercase();
        if line.is_empty() {
            return Ok(default);
        }
        Ok(matches!(line.as_str(), "y" | "yes"))
    }
    async fn choose(
        &mut self,
        prompt: &str,
        options: &[&str],
        default_idx: usize,
    ) -> Result<usize> {
        self.print(&format!("{prompt}\n"));
        for (i, o) in options.iter().enumerate() {
            self.print(&format!("  [{}] {}\n", i + 1, o));
        }
        self.print(&format!("Choice [default {}]: ", default_idx + 1));
        let line = self.read_line().await?;
        let line = line.trim();
        if line.is_empty() {
            return Ok(default_idx);
        }
        let n: usize = line
            .parse()
            .with_context(|| format!("expected a number 1..={}", options.len()))?;
        if n < 1 || n > options.len() {
            bail!("choice {n} out of range 1..={}", options.len());
        }
        Ok(n - 1)
    }
}

#[derive(Default)]
pub struct ScriptedIo {
    pub answers: std::collections::VecDeque<String>,
    pub passwords: std::collections::VecDeque<String>,
    pub output: Vec<u8>,
}

impl ScriptedIo {
    pub fn new(answers: Vec<&str>) -> Self {
        Self {
            answers: answers.into_iter().map(String::from).collect(),
            passwords: std::collections::VecDeque::new(),
            output: Vec::new(),
        }
    }
    pub fn with_passwords(mut self, pws: Vec<&str>) -> Self {
        self.passwords = pws.into_iter().map(String::from).collect();
        self
    }
    pub fn output_str(&self) -> String {
        String::from_utf8_lossy(&self.output).to_string()
    }
}

#[async_trait]
impl WizardIo for ScriptedIo {
    async fn read_line(&mut self) -> Result<String> {
        self.answers
            .pop_front()
            .ok_or_else(|| anyhow!("ScriptedIo exhausted; wizard tried to read another line"))
    }
    async fn read_password(&mut self) -> Result<String> {
        self.passwords
            .pop_front()
            .or_else(|| self.answers.pop_front())
            .ok_or_else(|| anyhow!("ScriptedIo exhausted; wizard tried to read a password"))
    }
    fn print(&mut self, s: &str) {
        use std::io::Write;
        let _ = self.output.write_all(s.as_bytes());
    }
    async fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        self.print(prompt);
        self.print(if default { " [Y/n] " } else { " [y/N] " });
        let line = self.read_line().await.unwrap_or_default();
        let line = line.trim().to_lowercase();
        if line.is_empty() {
            return Ok(default);
        }
        Ok(matches!(line.as_str(), "y" | "yes"))
    }
    async fn choose(
        &mut self,
        prompt: &str,
        options: &[&str],
        default_idx: usize,
    ) -> Result<usize> {
        self.print(prompt);
        self.print("\n");
        for (i, o) in options.iter().enumerate() {
            self.print(&format!("  [{}] {}\n", i + 1, o));
        }
        let line = self.read_line().await.unwrap_or_default();
        let line = line.trim();
        if line.is_empty() {
            return Ok(default_idx);
        }
        let n: usize = line.parse().context("non-numeric choice")?;
        if n < 1 || n > options.len() {
            bail!("choice {n} out of range");
        }
        Ok(n - 1)
    }
}

// ----------------------------------------------------------------------------
// Wizard flow.
// ----------------------------------------------------------------------------

const CHATOPS_OPTIONS: &[&str] = &["none", "slack", "discord", "teams", "mattermost", "matrix"];
const REVIEWER_OPTIONS: &[&str] = &["none", "anthropic", "openai_compatible", "ollama"];

pub async fn run_wizard(
    io: &mut dyn WizardIo,
    _mode: InstallMode,
    prefill: &WizardPrefill,
) -> Result<WizardAnswers> {
    let repo_url = ask_required(io, "Repository URL (e.g. git@github.com:org/repo.git): ", prefill.repo_url.as_deref()).await?;
    let base_branch = ask_default(io, "Base branch", prefill.base_branch.as_deref().unwrap_or("main")).await?;
    let agent_branch = ask_default(io, "Agent branch", prefill.agent_branch.as_deref().unwrap_or("agent-q")).await?;
    let poll_interval_sec: u64 = ask_default(
        io,
        "Poll interval (seconds)",
        &prefill.poll_interval_sec.unwrap_or(300).to_string(),
    )
    .await?
    .parse()
    .context("poll interval must be an integer")?;
    let token_env_var = ask_default(
        io,
        "GitHub PAT env var name",
        prefill.token_env_var.as_deref().unwrap_or("GITHUB_TOKEN"),
    )
    .await?;

    io.print(&format!(
        "GitHub PAT value (will be written to secrets.env as {token_env_var}=...): "
    ));
    let github_pat = io.read_password().await?;
    let github_pat = if github_pat.is_empty() { None } else { Some(github_pat) };

    let default_chatops_idx = chatops_arg_to_idx(prefill.chatops_backend.unwrap_or(ChatOpsBackendArg::None));
    let chatops_idx = io.choose("ChatOps backend", CHATOPS_OPTIONS, default_chatops_idx).await?;
    let chatops_backend = idx_to_chatops_arg(chatops_idx);

    let mut chatops_channel_id: Option<String> = None;
    let mut chatops_token: Option<String> = None;
    if chatops_backend != ChatOpsBackendArg::None {
        let cid = ask_default(
            io,
            "ChatOps channel id",
            prefill.chatops_channel_id.as_deref().unwrap_or(""),
        )
        .await?;
        chatops_channel_id = if cid.is_empty() { None } else { Some(cid) };
        io.print(&format!(
            "ChatOps bot token for {} (written to secrets.env): ",
            chatops_backend_label(chatops_backend)
        ));
        let tok = io.read_password().await?;
        chatops_token = if tok.is_empty() { None } else { Some(tok) };
    }

    let default_reviewer_idx = reviewer_arg_to_idx(prefill.reviewer_provider.unwrap_or(ReviewerProviderArg::None));
    let reviewer_idx = io.choose("Reviewer provider", REVIEWER_OPTIONS, default_reviewer_idx).await?;
    let reviewer_provider = idx_to_reviewer_arg(reviewer_idx);

    let mut reviewer_model: Option<String> = None;
    let mut reviewer_api_key: Option<String> = None;
    let mut reviewer_api_base_url: Option<String> = None;
    if reviewer_provider != ReviewerProviderArg::None {
        let default_model = prefill
            .reviewer_model
            .as_deref()
            .unwrap_or(match reviewer_provider {
                ReviewerProviderArg::Anthropic => "claude-sonnet-4-6",
                ReviewerProviderArg::Ollama => "qwen2.5-coder:32b",
                _ => "gpt-4o-mini",
            });
        reviewer_model = Some(ask_default(io, "Reviewer model", default_model).await?);
        if reviewer_provider == ReviewerProviderArg::Ollama {
            // Ollama: prompt for base URL only (no api_key — Ollama
            // does not authenticate; the per-provider auth-semantics
            // validator at config-load REJECTS a configured key).
            let base = ask_default(
                io,
                "Reviewer Ollama base URL",
                "http://localhost:11434",
            )
            .await?;
            reviewer_api_base_url = Some(base);
        } else {
            io.print("Reviewer API key (written to secrets.env): ");
            let k = io.read_password().await?;
            reviewer_api_key = if k.is_empty() { None } else { Some(k) };
        }
    }

    let audits = run_audit_prompts(io).await?;
    let canonical_rag = run_rag_prompts(io, prefill).await?;

    Ok(WizardAnswers {
        repo_url,
        base_branch,
        agent_branch,
        poll_interval_sec,
        token_env_var,
        github_pat,
        chatops_backend,
        chatops_channel_id,
        chatops_token,
        reviewer_provider,
        reviewer_model,
        reviewer_api_key,
        reviewer_api_base_url,
        audits,
        canonical_rag,
    })
}

/// Canonical-spec RAG graduated-path prompt (a21). Probes localhost
/// Ollama; on hit, offers it. On miss, presents the four-option menu:
/// docker quick-start / remote Ollama / OpenAI-compatible / disable.
async fn run_rag_prompts(
    io: &mut dyn WizardIo,
    _prefill: &WizardPrefill,
) -> Result<Option<RagAnswers>> {
    io.print(
        "\nCanonical-spec RAG (a21): retrieval-augmented context for the implementer.\n",
    );
    io.print(
        "When enabled, the daemon embeds your canonical specs and the agent can query them.\n",
    );
    if !io.confirm("Configure canonical-specs RAG?", true).await? {
        io.print("Skipping RAG configuration. Re-enable later via `canonical_rag:` block in config.yaml.\n");
        return Ok(None);
    }
    let localhost_url = "http://localhost:11434";
    let detected = probe_ollama(localhost_url).await;
    if detected {
        io.print("Detected Ollama on http://localhost:11434.\n");
        let model = ask_default(io, "Embedding model", "nomic-embed-text").await?;
        return Ok(Some(RagAnswers {
            provider: RagProviderArg::Ollama,
            base_url: localhost_url.to_string(),
            model,
            api_key_env: None,
        }));
    }
    io.print("Ollama not detected on http://localhost:11434.\n");
    let options = [
        "Install local Ollama via docker (we ship a compose file)",
        "Point at a remote Ollama instance",
        "Point at an OpenAI-compatible embeddings endpoint",
        "Disable RAG (you can enable later in config.yaml)",
    ];
    let choice = io.choose("Choose RAG option", &options, 3).await?;
    match choice {
        0 => {
            io.print(
                "Docker compose file will be copied to your config directory at install time.\n",
            );
            io.print(
                "After install, run: docker compose -f <config_dir>/ollama-docker-compose.yml up -d\n",
            );
            Ok(Some(RagAnswers {
                provider: RagProviderArg::Ollama,
                base_url: localhost_url.to_string(),
                model: "nomic-embed-text".to_string(),
                api_key_env: None,
            }))
        }
        1 => {
            let base_url = ask_required(io, "Ollama base URL (e.g. http://gpu-host:11434)", None).await?;
            let model = ask_default(io, "Embedding model", "nomic-embed-text").await?;
            Ok(Some(RagAnswers {
                provider: RagProviderArg::Ollama,
                base_url,
                model,
                api_key_env: None,
            }))
        }
        2 => {
            let base_url = ask_required(io, "OpenAI-compatible base URL (e.g. https://api.voyageai.com/v1)", None).await?;
            let model = ask_required(io, "Embedding model name", None).await?;
            let api_key_env = ask_default(io, "Env var holding the API key", "RAG_API_KEY").await?;
            Ok(Some(RagAnswers {
                provider: RagProviderArg::OpenaiCompatible,
                base_url,
                model,
                api_key_env: Some(api_key_env),
            }))
        }
        _ => {
            io.print("Disabling RAG. Enable later via the `canonical_rag:` block in config.yaml.\n");
            Ok(None)
        }
    }
}

/// HTTP probe: GET `<base>/api/tags` with a 2-second timeout. Returns
/// `true` on 200 OK. Any other status, network failure, or timeout is
/// `false`. Used by the install wizard's detection step.
async fn probe_ollama(base_url: &str) -> bool {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(&url).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Walk the operator through the periodic-audit prompts. See the
/// `Install wizard configures periodic audits` requirement for the full
/// three-tier UX. Returns a map of audit slug → resolved cadence; absent
/// or `Disabled` entries are dropped before YAML emit.
async fn run_audit_prompts(io: &mut dyn WizardIo) -> Result<HashMap<String, Cadence>> {
    let mut audits: HashMap<String, Cadence> = HashMap::new();

    io.print("\nPeriodic audits\n");
    io.print(
        "  autocoder ships several optional audits that run on a configurable cadence.\n",
    );

    io.print("\n  LLM-driven audits (call the agent CLI; have token cost). Includes:\n");
    for (slug, _) in LLM_DRIVEN_SLUGS {
        io.print(&format!("    - {slug} ({})\n", audit_description(slug)));
    }
    let enable_llm = io
        .confirm("\n  Enable the LLM-driven audits?", false)
        .await?;
    if !enable_llm {
        return Ok(audits);
    }

    io.print("\n  Recommended cadences:\n");
    for (slug, rec) in LLM_DRIVEN_SLUGS {
        io.print(&format!("    {slug}: {}\n", cadence_label(*rec)));
    }
    let fast_path = io
        .confirm("\n  Enable all five with recommended cadences?", true)
        .await?;
    if fast_path {
        for (slug, rec) in LLM_DRIVEN_SLUGS {
            audits.insert((*slug).to_string(), *rec);
        }
        return Ok(audits);
    }

    // Walk each LLM-driven audit individually.
    for (slug, rec) in LLM_DRIVEN_SLUGS {
        io.print(&format!("\n  {slug} ({})\n", audit_description(slug)));
        let label = format!(
            "  Cadence (recommended: {})",
            cadence_label(*rec),
        );
        let chosen = ask_audit_cadence(io, &label, *rec, "never").await?;
        if chosen != Cadence::Disabled {
            audits.insert((*slug).to_string(), chosen);
        }
    }
    Ok(audits)
}

/// Prompt for a cadence using the wizard's compact `[d]aily / [w]eekly /
/// [m]onthly / [n]ever` shorthand. Bare-Enter accepts `default`. The
/// `never_label` lets the caller print `never` (the wizard's operator-
/// facing word for `Cadence::Disabled`).
async fn ask_audit_cadence(
    io: &mut dyn WizardIo,
    prompt: &str,
    default: Cadence,
    never_label: &str,
) -> Result<Cadence> {
    let default_letter = match default {
        Cadence::Daily => "d",
        Cadence::Weekly => "w",
        Cadence::Monthly => "m",
        Cadence::Disabled => "n",
        // No wizard prompt should use these as defaults; map to weekly so
        // the cadence-key letter stays meaningful if a caller does anyway.
        Cadence::Quarterly | Cadence::EveryNDays(_) => "w",
    };
    io.print(&format!(
        "{prompt} [d]aily / [w]eekly / [m]onthly / [{never_label}n]ever (default {default_letter}): ",
    ));
    let line = io.read_line().await?;
    let trimmed = line.trim().to_lowercase();
    if trimmed.is_empty() {
        return Ok(default);
    }
    match trimmed.as_str() {
        "d" | "daily" => Ok(Cadence::Daily),
        "w" | "weekly" => Ok(Cadence::Weekly),
        "m" | "monthly" => Ok(Cadence::Monthly),
        "n" | "never" | "disabled" => Ok(Cadence::Disabled),
        other => bail!(
            "unrecognized cadence `{other}`; expected one of d / w / m / n (or daily/weekly/monthly/never)"
        ),
    }
}

async fn ask_required(io: &mut dyn WizardIo, prompt: &str, prefill: Option<&str>) -> Result<String> {
    loop {
        if let Some(p) = prefill {
            io.print(&format!("{prompt}[{p}]: "));
        } else {
            io.print(prompt);
        }
        let line = io.read_line().await?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        if let Some(p) = prefill {
            return Ok(p.to_string());
        }
        io.print("value required\n");
    }
}

async fn ask_default(io: &mut dyn WizardIo, label: &str, default: &str) -> Result<String> {
    io.print(&format!("{label} [{default}]: "));
    let line = io.read_line().await?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn chatops_arg_to_idx(a: ChatOpsBackendArg) -> usize {
    match a {
        ChatOpsBackendArg::None => 0,
        ChatOpsBackendArg::Slack => 1,
        ChatOpsBackendArg::Discord => 2,
        ChatOpsBackendArg::Teams => 3,
        ChatOpsBackendArg::Mattermost => 4,
        ChatOpsBackendArg::Matrix => 5,
    }
}

fn idx_to_chatops_arg(i: usize) -> ChatOpsBackendArg {
    [
        ChatOpsBackendArg::None,
        ChatOpsBackendArg::Slack,
        ChatOpsBackendArg::Discord,
        ChatOpsBackendArg::Teams,
        ChatOpsBackendArg::Mattermost,
        ChatOpsBackendArg::Matrix,
    ][i]
}

fn reviewer_arg_to_idx(a: ReviewerProviderArg) -> usize {
    match a {
        ReviewerProviderArg::None => 0,
        ReviewerProviderArg::Anthropic => 1,
        ReviewerProviderArg::OpenAiCompatible => 2,
        ReviewerProviderArg::Ollama => 3,
    }
}

fn idx_to_reviewer_arg(i: usize) -> ReviewerProviderArg {
    [
        ReviewerProviderArg::None,
        ReviewerProviderArg::Anthropic,
        ReviewerProviderArg::OpenAiCompatible,
        ReviewerProviderArg::Ollama,
    ][i]
}

fn chatops_backend_label(b: ChatOpsBackendArg) -> &'static str {
    match b {
        ChatOpsBackendArg::None => "none",
        ChatOpsBackendArg::Slack => "slack",
        ChatOpsBackendArg::Discord => "discord",
        ChatOpsBackendArg::Teams => "teams",
        ChatOpsBackendArg::Mattermost => "mattermost",
        ChatOpsBackendArg::Matrix => "matrix",
    }
}

fn chatops_env_var(b: ChatOpsBackendArg) -> Option<&'static str> {
    match b {
        ChatOpsBackendArg::None => None,
        ChatOpsBackendArg::Slack => Some("SLACK_BOT_TOKEN"),
        ChatOpsBackendArg::Discord => Some("DISCORD_BOT_TOKEN"),
        ChatOpsBackendArg::Teams => Some("TEAMS_CLIENT_SECRET"),
        ChatOpsBackendArg::Mattermost => Some("MATTERMOST_TOKEN"),
        ChatOpsBackendArg::Matrix => Some("MATRIX_ACCESS_TOKEN"),
    }
}

fn reviewer_env_var(p: ReviewerProviderArg) -> Option<&'static str> {
    match p {
        ReviewerProviderArg::None => None,
        ReviewerProviderArg::Anthropic => Some("ANTHROPIC_API_KEY"),
        ReviewerProviderArg::OpenAiCompatible => Some("OPENAI_API_KEY"),
        // a37: Ollama does not authenticate; the per-provider auth
        // validator rejects a configured api_key/api_key_env at
        // config-load. Returning None here keeps the secrets.env path
        // (which only writes when the env var name is `Some`) inert.
        ReviewerProviderArg::Ollama => None,
    }
}

// ----------------------------------------------------------------------------
// Config + secrets assembly.
// ----------------------------------------------------------------------------

/// Resolve XDG-derived defaults for the daemon data paths under
/// `$HOME` (or whatever `$HOME` is set to in the calling environment).
/// Used by dev-mode install to write explicit `paths:` values into the
/// generated `config.yaml` so operators can see exactly where their
/// state is being written without re-deriving the XDG layout.
pub fn xdg_paths_for_dev_mode() -> crate::config::DaemonPathsConfig {
    let home = std::env::var("HOME").ok().unwrap_or_else(|| ".".to_string());
    let home = PathBuf::from(home);
    let xdg_state = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    let xdg_cache = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    let xdg_runtime = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("{uid}-runtime"))
        });
    crate::config::DaemonPathsConfig {
        state_dir: Some(xdg_state.join("autocoder")),
        cache_dir: Some(xdg_cache.join("autocoder")),
        logs_dir: Some(xdg_state.join("autocoder/logs")),
        runtime_dir: Some(xdg_runtime.join("autocoder")),
    }
}

/// Deserialize the bundled `config.example.yaml` and mutate it with the
/// operator's answers. The example is the source-of-truth for what fields
/// exist; this function only writes the ones the wizard collected.
pub fn assemble_config(answers: &WizardAnswers) -> Result<Config> {
    let mut cfg: Config = serde_yml::from_str(BUNDLED_EXAMPLE)
        .context("bundled config.example.yaml failed to deserialize")?;
    let repo = cfg
        .repositories
        .get_mut(0)
        .ok_or_else(|| anyhow!("bundled config.example.yaml has no repositories[]"))?;
    repo.url = answers.repo_url.clone();
    repo.base_branch = answers.base_branch.clone();
    repo.agent_branch = answers.agent_branch.clone();
    repo.poll_interval_sec = answers.poll_interval_sec;
    repo.chatops_channel_id = answers.chatops_channel_id.clone();

    cfg.github = GithubConfig {
        token_env: answers.token_env_var.clone(),
        token: None,
        owner_tokens: None,
        fork_owner: None,
        recreate_fork_on_reinit: false,
    };

    cfg.chatops = match answers.chatops_backend {
        ChatOpsBackendArg::None => None,
        ChatOpsBackendArg::Slack => Some(ChatOpsConfig {
            provider: ChatOpsProvider::Slack,
            default_channel_id: answers.chatops_channel_id.clone().unwrap_or_default(),
            notifications: None,
            slack: Some(SlackProviderConfig {
                bot_token_env: Some("SLACK_BOT_TOKEN".to_string()),
                bot_token: None,
                app_token_env: None,
                app_token: None,
                listen_channels: Vec::new(),
                dedup_cache_capacity: crate::config::default_dedup_cache_capacity(),
                dedup_cache_ttl_secs: crate::config::default_dedup_cache_ttl_secs(),
            }),
            discord: None,
            teams: None,
            mattermost: None,
            matrix: None,
        }),
        // Experimental backends: wizard captures the channel and token but
        // leaves the provider-specific sub-blocks unset (the operator can
        // hand-edit if needed). The serialize round-trip would surface a
        // missing required field for those backends; flag a clear error
        // pointing the operator at the README for now.
        other => bail!(
            "chatops backend `{}` is experimental and not supported by the wizard yet; pick `none` or `slack`",
            chatops_backend_label(other)
        ),
    };

    cfg.reviewer = match answers.reviewer_provider {
        ReviewerProviderArg::None => None,
        ReviewerProviderArg::Anthropic
        | ReviewerProviderArg::OpenAiCompatible
        | ReviewerProviderArg::Ollama => {
            let provider = match answers.reviewer_provider {
                ReviewerProviderArg::Anthropic => ReviewerProvider::Anthropic,
                ReviewerProviderArg::OpenAiCompatible => ReviewerProvider::OpenAiCompatible,
                ReviewerProviderArg::Ollama => ReviewerProvider::Ollama,
                _ => unreachable!(),
            };
            // a37: Ollama needs `api_base_url` (REQUIRED by config-load
            // validation) AND NO `api_key_env` (Ollama does not
            // authenticate; configuring a key fails config-load).
            let is_ollama = answers.reviewer_provider == ReviewerProviderArg::Ollama;
            Some(ReviewerConfig {
                enabled: true,
                provider,
                model: answers
                    .reviewer_model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
                api_key_env: if is_ollama {
                    None
                } else {
                    reviewer_env_var(answers.reviewer_provider).map(String::from)
                },
                api_key: None,
                api_base_url: answers.reviewer_api_base_url.clone(),
                prompt_template_path: None,
                code_review: None,
                auto_revise: false,
                prompt_budget_chars: 2_000_000,
                mode: crate::config::ReviewerMode::Bundled,
                max_code_reviews_per_pr: None,
                suggest_rereview_threshold: None,
                skip_spec_only_prs: false,
            })
        }
    };

    // Audits: drop `Disabled` entries; if no audits enabled at all, the
    // `audits:` block is omitted entirely (matching the `Option<AuditsConfig>`
    // schema). `settings` stays empty — operators wanting `prompt_path` /
    // `notify_on_clean` / `extra` overrides edit config.yaml after install.
    let enabled: HashMap<String, Cadence> = answers
        .audits
        .iter()
        .filter(|(_, c)| c.is_enabled())
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    cfg.audits = if enabled.is_empty() {
        None
    } else {
        Some(AuditsConfig {
            defaults: enabled,
            ..AuditsConfig::default()
        })
    };

    // Canonical-spec RAG (a21).
    cfg.canonical_rag = answers.canonical_rag.as_ref().map(|r| {
        crate::config::CanonicalRagConfig {
            enabled: true,
            provider: match r.provider {
                RagProviderArg::Ollama => crate::config::RagProvider::Ollama,
                RagProviderArg::OpenaiCompatible => {
                    crate::config::RagProvider::OpenAiCompatible
                }
                RagProviderArg::None => crate::config::RagProvider::Ollama, // unreachable
            },
            model: r.model.clone(),
            api_base_url: r.base_url.clone(),
            api_key_env: r.api_key_env.clone(),
            api_key: None,
            top_k: crate::config::default_rag_top_k(),
            chunk_strategy: crate::config::ChunkStrategy::default(),
            reembed_on_archive: crate::config::default_reembed_on_archive(),
        }
    });

    // Suppress the unused-import warning if `SecretSource` ends up only
    // being referenced behind a feature in future edits.
    let _ = std::marker::PhantomData::<SecretSource>;

    Ok(cfg)
}

pub fn serialize_config(cfg: &Config) -> Result<String> {
    serde_yml::to_string(cfg).context("serialize Config to YAML")
}

pub fn assemble_secrets_env(answers: &WizardAnswers) -> String {
    let mut out = String::new();
    if let Some(pat) = &answers.github_pat {
        out.push_str(&format!("{}={}\n", answers.token_env_var, pat));
    }
    if let (Some(env), Some(val)) = (chatops_env_var(answers.chatops_backend), &answers.chatops_token) {
        out.push_str(&format!("{env}={val}\n"));
    }
    if let (Some(env), Some(val)) = (
        reviewer_env_var(answers.reviewer_provider),
        &answers.reviewer_api_key,
    ) {
        out.push_str(&format!("{env}={val}\n"));
    }
    out
}

// ----------------------------------------------------------------------------
// Entry point.
// ----------------------------------------------------------------------------

pub async fn execute(args: InstallArgs) -> Result<()> {
    let actions: Box<dyn SystemActions> = Box::new(RealSystemActions);
    let mut io: Box<dyn WizardIo> = Box::new(StdioWizardIo);
    execute_inner(
        args,
        &mut *io,
        actions.as_ref(),
        PathBuf::from("/etc/systemd/system"),
    )
    .await
}

pub(crate) async fn execute_inner(
    args: InstallArgs,
    io: &mut dyn WizardIo,
    actions: &dyn SystemActions,
    systemd_unit_dir: PathBuf,
) -> Result<()> {
    let mode = resolve_mode(args.mode);
    let config_dir = resolve_config_dir(args.config_dir.clone(), mode);
    let config_path = config_dir.join("config.yaml");
    let secrets_path = config_dir.join("secrets.env");

    // --reconfigure short-circuits the entire fresh-install flow: the
    // operator explicitly asked to re-prompt one section of an existing
    // install. The dispatch resolves the existing-config path via the
    // a01 systemd probe (or the default-path fallback), parses the
    // existing config, re-prompts the chosen section, and patches the
    // file in place (audits) or with diff-confirm (reviewer / chatops).
    if let Some(section) = args.reconfigure {
        return execute_reconfigure(&args, section, io, actions, mode).await;
    }

    // 0. Systemd-probe existing-install detection. Operators who built from
    //    source and wrote their own systemd unit are invisible to the
    //    default-path config.yaml check below; the probe finds them.
    //    Skipped in dev mode (no systemd unit by definition) and when
    //    --upgrade is set (operator explicitly opted into the wizard rerun).
    if mode == InstallMode::Server && !args.upgrade {
        let probe = actions.probe_systemd_unit("autocoder.service").await?;
        match probe.load_state {
            LoadState::Loaded => match &probe.exec_start_config_path {
                Some(unit_config_path) => {
                    if unit_config_path.exists() {
                        print_existing_install_verbs(unit_config_path);
                        return Ok(());
                    }
                    let frag = probe
                        .fragment_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<unknown>".to_string());
                    bail!(
                        "autocoder.service is loaded (unit file: {frag}) but its \
                         --config path {missing} does not exist on disk. \
                         Either restore the config file from backup, or remove \
                         the unit file (`sudo rm {frag} && sudo systemctl daemon-reload`) \
                         and re-run install.sh to start fresh.",
                        missing = unit_config_path.display(),
                    );
                }
                None => {
                    let frag = probe
                        .fragment_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<unknown>".to_string());
                    eprintln!(
                        "WARN: autocoder.service is loaded (unit: {frag}) but its \
                         ExecStart has no --config <path> flag; falling through to \
                         the default-path config.yaml check."
                    );
                }
            },
            LoadState::NotFound | LoadState::Other(_) => {
                // Fall through to the default-path check.
            }
        }
    }

    // 1. Idempotency check.
    if config_path.exists() && !args.upgrade {
        println!(
            "autocoder is already configured at {}; the new binary is installed. No wizard work needed.",
            config_path.display()
        );
        return Ok(());
    }
    if config_path.exists() && args.upgrade {
        println!(
            "autocoder install --upgrade: binary already swapped by install.sh; existing config at {} unchanged.",
            config_path.display()
        );
        return Ok(());
    }

    // 2. Non-interactive validation up-front so we fail fast.
    let prefill = WizardPrefill {
        repo_url: args.repo_url.clone(),
        base_branch: args.base_branch.clone(),
        agent_branch: args.agent_branch.clone(),
        poll_interval_sec: args.poll_interval_sec,
        token_env_var: args.token_env_var.clone(),
        chatops_backend: args.chatops_backend,
        chatops_channel_id: args.chatops_channel_id.clone(),
        reviewer_provider: args.reviewer_provider,
        reviewer_model: args.reviewer_model.clone(),
        audits_llm_driven: args.audits_llm_driven,
        audit_documentation_audit: args.audit_documentation_audit,
        audit_architecture_brightline: args.audit_architecture_brightline,
        audit_architecture_consultative: args.audit_architecture_consultative,
        audit_drift_audit: args.audit_drift_audit,
        audit_missing_tests_audit: args.audit_missing_tests_audit,
        audit_security_bug_audit: args.audit_security_bug_audit,
        rag_provider: args.rag_provider,
        rag_base_url: args.rag_base_url.clone(),
        rag_model: args.rag_model.clone(),
        rag_api_key_env: args.rag_api_key_env.clone(),
    };
    if args.non_interactive {
        validate_non_interactive(&prefill)?;
    }

    // 3. Optional: server-mode system user + system packages + claude CLI.
    if mode == InstallMode::Server {
        actions
            .create_user("autocoder", Path::new("/var/lib/autocoder"), "/usr/sbin/nologin")
            .await?;
    }

    if actions.which("apt-get").await.is_some() {
        let should_install = if args.non_interactive {
            true
        } else {
            io.confirm("Install system dependencies via apt-get (git, ca-certificates)?", true).await?
        };
        if should_install {
            actions.apt_install(&["git", "ca-certificates"]).await?;
        }
    }

    if actions.which("claude").await.is_none() {
        let should_install = if args.non_interactive {
            true
        } else {
            io.confirm("Install Claude Code CLI now?", true).await?
        };
        if should_install {
            actions.run_subprocess("bash", &["-c", &format!("curl -fsSL {CLAUDE_INSTALL_URL} | bash")]).await?;
        }
    }

    // 4. Collect answers — either via the wizard or directly from prefill.
    let answers = if args.non_interactive {
        prefill_to_answers(&prefill)?
    } else {
        run_wizard(io, mode, &prefill).await?
    };

    // 5. Generate + persist artifacts.
    fs::create_dir_all(&config_dir)
        .await
        .with_context(|| format!("create_dir_all {}", config_dir.display()))?;

    let mut cfg = assemble_config(&answers)?;
    // Dev-mode install: write the XDG-derived paths into the
    // generated `config.yaml` so operators can see exactly where
    // their state is being written (the values would otherwise be
    // resolved at startup via the env-var / XDG-default chain).
    // Server-mode install leaves `paths:` absent — the rendered
    // systemd unit's StateDirectory/CacheDirectory/etc. populate
    // $STATE_DIRECTORY family at unit-start time and the daemon's
    // resolver picks them up automatically.
    if mode == InstallMode::Dev {
        cfg.paths = xdg_paths_for_dev_mode();
    }
    let yaml = serialize_config(&cfg)?;
    let config_mode = if mode == InstallMode::Server { 0o640 } else { 0o600 };
    write_file_with_mode(&config_path, yaml.as_bytes(), config_mode)
        .with_context(|| format!("write {}", config_path.display()))?;

    let secrets = assemble_secrets_env(&answers);
    write_file_with_mode(&secrets_path, secrets.as_bytes(), 0o600)
        .with_context(|| format!("write {}", secrets_path.display()))?;

    actions.chmod(&config_path, config_mode).await?;
    actions.chmod(&secrets_path, 0o600).await?;
    if mode == InstallMode::Server {
        actions.chown(&config_path, "autocoder", "autocoder").await?;
        actions.chown(&secrets_path, "autocoder", "autocoder").await?;
    }

    // Canonical-spec RAG (a21): if the operator chose the docker
    // quick-start path AND a wired ollama compose file ships with the
    // binary, copy it to the config dir AND print the operator's next
    // step. We detect "docker quick-start path" by the wizard's choice
    // of localhost ollama with the default model.
    if let Some(rag) = answers.canonical_rag.as_ref()
        && rag.provider == RagProviderArg::Ollama
        && rag.base_url == "http://localhost:11434"
    {
        let dst = config_dir.join("ollama-docker-compose.yml");
        if !dst.exists() {
            const BUNDLED_OLLAMA_COMPOSE: &str =
                include_str!("../../../install/ollama-docker-compose.yml");
            if let Err(e) = fs::write(&dst, BUNDLED_OLLAMA_COMPOSE).await {
                eprintln!(
                    "WARN: could not copy ollama-docker-compose.yml to {}: {e}",
                    dst.display()
                );
            } else {
                println!(
                    "Copied bundled ollama-docker-compose.yml to {}",
                    dst.display()
                );
                println!(
                    "Start it with: docker compose -f {} up -d",
                    dst.display()
                );
            }
        }
    }

    // 6. Systemd unit (server mode).
    if mode == InstallMode::Server {
        fs::create_dir_all(&systemd_unit_dir)
            .await
            .with_context(|| format!("create_dir_all {}", systemd_unit_dir.display()))?;
        let unit_path = systemd_unit_dir.join("autocoder.service");
        fs::write(&unit_path, SYSTEMD_UNIT.as_bytes())
            .await
            .with_context(|| format!("write {}", unit_path.display()))?;
        actions.chmod(&unit_path, 0o644).await?;
        actions.daemon_reload().await?;
        actions.enable_systemd_unit("autocoder").await?;
        let start_now = if args.non_interactive {
            true
        } else {
            io.confirm("Start autocoder.service now?", true).await?
        };
        if start_now {
            actions.start_systemd_unit("autocoder").await?;
        }
    }

    // 7. Post-install summary.
    println!("autocoder install complete.");
    println!("  config:  {}", config_path.display());
    println!("  secrets: {}", secrets_path.display());
    match mode {
        InstallMode::Server => {
            println!("  service: systemctl status autocoder    journalctl -u autocoder -f");
            println!("  binary:  {SERVER_BINARY_PATH}");
        }
        InstallMode::Dev => {
            println!(
                "  run with: autocoder run --config {}",
                config_path.display()
            );
        }
    }
    if actions.which("claude").await.is_some() && std::env::var_os("CLAUDE_AUTH_NOTICE").is_none() {
        println!("  reminder: run `claude auth login` once as the runtime user before starting.");
    }
    println!("  add more repositories by editing config.yaml and running `autocoder reload`.");

    Ok(())
}

/// Print the three-verb status block shown when an existing install is
/// detected via the systemd probe. Wired into the post-detection short-circuit
/// in `execute_inner`. The `--reconfigure` and `update.sh` hints reference
/// follow-on changes (`a02`, `a04`); the verbs are correct regardless of
/// whether those changes have merged yet.
fn print_existing_install_verbs(config_path: &Path) {
    let config_dir = config_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<config-dir>".to_string());
    println!(
        "autocoder is already installed (config: {}).",
        config_path.display()
    );
    println!();
    println!("To update the binary:        ./update.sh        (or wire into cron)");
    println!("To reconfigure a section:    autocoder install --reconfigure <audits|reviewer|chatops>");
    println!("To wipe and reinstall:       sudo rm -rf {config_dir} && ./install.sh");
    println!();
    println!("No changes made.");
}

fn resolve_mode(explicit: Option<InstallMode>) -> InstallMode {
    if let Some(m) = explicit {
        return m;
    }
    if cfg!(target_os = "linux") && Path::new("/run/systemd/system").exists() {
        InstallMode::Server
    } else {
        InstallMode::Dev
    }
}

fn resolve_config_dir(explicit: Option<PathBuf>, mode: InstallMode) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    match mode {
        InstallMode::Server => PathBuf::from(SERVER_CONFIG_DIR),
        InstallMode::Dev => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".config/autocoder")
        }
    }
}

fn validate_non_interactive(p: &WizardPrefill) -> Result<()> {
    let required_msg = "in --non-interactive mode the following flags are required: \
        --repo-url, --token-env-var, --chatops-backend, --reviewer-provider \
        (chatops/reviewer may be `none`)";
    if p.repo_url.is_none() {
        bail!("missing --repo-url; {required_msg}");
    }
    if p.token_env_var.is_none() {
        bail!("missing --token-env-var; {required_msg}");
    }
    if p.chatops_backend.is_none() {
        bail!("missing --chatops-backend; {required_msg}");
    }
    if p.reviewer_provider.is_none() {
        bail!("missing --reviewer-provider; {required_msg}");
    }
    // Canonical-spec RAG (a21): when `--rag-provider` is set to a
    // non-`none` provider, the corresponding base-url (and api-key-env
    // for openai_compatible) MUST be present.
    match p.rag_provider.unwrap_or(RagProviderArg::None) {
        RagProviderArg::None => {}
        RagProviderArg::Ollama => {
            if p.rag_base_url.is_none() {
                bail!(
                    "missing --rag-base-url; required when --rag-provider=ollama"
                );
            }
        }
        RagProviderArg::OpenaiCompatible => {
            if p.rag_base_url.is_none() {
                bail!(
                    "missing --rag-base-url; required when --rag-provider=openai_compatible"
                );
            }
            if p.rag_api_key_env.is_none() {
                bail!(
                    "missing --rag-api-key-env; required when --rag-provider=openai_compatible"
                );
            }
        }
    }
    Ok(())
}

fn prefill_to_answers(p: &WizardPrefill) -> Result<WizardAnswers> {
    let canonical_rag = match p.rag_provider.unwrap_or(RagProviderArg::None) {
        RagProviderArg::None => None,
        provider => Some(RagAnswers {
            provider,
            base_url: p
                .rag_base_url
                .clone()
                .ok_or_else(|| anyhow!("--rag-base-url required when --rag-provider is set"))?,
            model: p
                .rag_model
                .clone()
                .unwrap_or_else(|| match provider {
                    RagProviderArg::Ollama => "nomic-embed-text".to_string(),
                    _ => String::new(),
                }),
            api_key_env: p.rag_api_key_env.clone(),
        }),
    };
    Ok(WizardAnswers {
        repo_url: p.repo_url.clone().ok_or_else(|| anyhow!("--repo-url required"))?,
        base_branch: p.base_branch.clone().unwrap_or_else(|| "main".to_string()),
        agent_branch: p.agent_branch.clone().unwrap_or_else(|| "agent-q".to_string()),
        poll_interval_sec: p.poll_interval_sec.unwrap_or(300),
        token_env_var: p.token_env_var.clone().ok_or_else(|| anyhow!("--token-env-var required"))?,
        github_pat: None,
        chatops_backend: p.chatops_backend.unwrap_or(ChatOpsBackendArg::None),
        chatops_channel_id: p.chatops_channel_id.clone(),
        chatops_token: None,
        reviewer_provider: p.reviewer_provider.unwrap_or(ReviewerProviderArg::None),
        reviewer_model: p.reviewer_model.clone(),
        reviewer_api_key: None,
        reviewer_api_base_url: None,
        audits: resolve_non_interactive_audits(p),
        canonical_rag,
    })
}

/// Resolve audit cadences from `--audits-*` flags. Implements the precedence
/// documented in the change spec:
/// - `--audits-llm-driven` is the master switch. `none` (default) keeps every
///   LLM-driven audit disabled regardless of any per-audit overrides;
///   `all-disabled` behaves identically but prints a one-line acknowledgement
///   to stdout (so IaC logs can distinguish explicit opt-out from "operator
///   forgot to pass the flag"); `recommended` enables every LLM-driven audit
///   at its recommended cadence, with per-audit `--audit-<slug>` flags
///   overriding individual cadences.
pub(crate) fn resolve_non_interactive_audits(p: &WizardPrefill) -> HashMap<String, Cadence> {
    let mut out: HashMap<String, Cadence> = HashMap::new();
    let llm = p.audits_llm_driven.unwrap_or(LlmDrivenAuditsArg::None);
    match llm {
        LlmDrivenAuditsArg::None => {
            // Master switch off — per-audit flags ignored.
        }
        LlmDrivenAuditsArg::AllDisabled => {
            println!(
                "audits: --audits-llm-driven all-disabled — every LLM-driven audit \
                 left disabled (per-audit flags are ignored under the master switch)."
            );
        }
        LlmDrivenAuditsArg::Recommended => {
            for (slug, rec) in LLM_DRIVEN_SLUGS {
                let override_arg = lookup_per_audit_override(p, slug);
                let resolved = override_arg.map(AuditCadenceArg::to_cadence).unwrap_or(*rec);
                if resolved != Cadence::Disabled {
                    out.insert((*slug).to_string(), resolved);
                }
            }
        }
    }
    out
}

fn lookup_per_audit_override(p: &WizardPrefill, slug: &str) -> Option<AuditCadenceArg> {
    match slug {
        "architecture_brightline" => p.audit_architecture_brightline,
        "architecture_consultative" => p.audit_architecture_consultative,
        "drift_audit" => p.audit_drift_audit,
        "missing_tests_audit" => p.audit_missing_tests_audit,
        "security_bug_audit" => p.audit_security_bug_audit,
        "documentation_audit" => p.audit_documentation_audit,
        _ => None,
    }
}

// ----------------------------------------------------------------------------
// --reconfigure: re-prompt one section of an existing install.
// ----------------------------------------------------------------------------

const RECONFIGURE_NO_INSTALL_HINT: &str =
    "no existing install detected; run install.sh for first-time setup";

/// Default server-mode config path used as the systemd-probe fallback when
/// the unit isn't loaded (or has no `--config` flag).
pub(crate) const DEFAULT_SERVER_CONFIG_PATH: &str = "/etc/autocoder/config.yaml";

/// Resolve the path to the existing `config.yaml` for `--reconfigure`.
///
/// Resolution priority:
///
/// 1. `args.config_dir.join("config.yaml")` if the override exists.
/// 2. Server mode: probe `autocoder.service`; if loaded AND its
///    `exec_start_config_path` exists on disk, use it.
/// 3. Server mode: fall back to `/etc/autocoder/config.yaml`.
/// 4. Dev mode: `~/.config/autocoder/config.yaml`.
///
/// Bails with `RECONFIGURE_NO_INSTALL_HINT` if none of the above resolves
/// to a file that exists on disk.
pub(crate) async fn resolve_existing_config_path(
    args: &InstallArgs,
    actions: &dyn SystemActions,
    mode: InstallMode,
) -> Result<PathBuf> {
    if let Some(dir) = args.config_dir.as_ref() {
        let candidate = dir.join("config.yaml");
        if candidate.exists() {
            return Ok(candidate);
        }
        bail!(RECONFIGURE_NO_INSTALL_HINT);
    }

    match mode {
        InstallMode::Server => {
            let probe = actions.probe_systemd_unit("autocoder.service").await?;
            if matches!(probe.load_state, LoadState::Loaded)
                && let Some(p) = probe.exec_start_config_path.as_ref()
                && p.exists()
            {
                return Ok(p.clone());
            }
            let fallback = PathBuf::from(DEFAULT_SERVER_CONFIG_PATH);
            if fallback.exists() {
                return Ok(fallback);
            }
            bail!(RECONFIGURE_NO_INSTALL_HINT);
        }
        InstallMode::Dev => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let candidate = home.join(".config/autocoder/config.yaml");
            if candidate.exists() {
                return Ok(candidate);
            }
            bail!(RECONFIGURE_NO_INSTALL_HINT);
        }
    }
}

/// Per-section dispatcher invoked from `execute_inner` when
/// `args.reconfigure` is `Some`. Resolves the existing config path,
/// parses it, calls the section-specific re-prompt helper, and applies
/// the result (audits → in-place patch; reviewer / chatops → diff-confirm).
pub(crate) async fn execute_reconfigure(
    args: &InstallArgs,
    section: ReconfigureSection,
    io: &mut dyn WizardIo,
    actions: &dyn SystemActions,
    mode: InstallMode,
) -> Result<()> {
    let config_path = resolve_existing_config_path(args, actions, mode).await?;
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read existing config at {}", config_path.display()))?;
    let existing: Config = serde_yml::from_str(&raw)
        .with_context(|| format!("parse existing config at {}", config_path.display()))?;

    match section {
        ReconfigureSection::Audits => {
            let new_config = reconfigure_audits(&existing, io).await?;
            apply_in_place_patch(&config_path, &new_config)?;
            print_restart_guidance(&config_path, section);
        }
        ReconfigureSection::Reviewer => {
            let new_config = reconfigure_reviewer(&existing, io).await?;
            let applied =
                confirm_diff_and_apply(&config_path, &new_config, io).await?;
            if applied {
                print_restart_guidance(&config_path, section);
            } else {
                io.print("no changes made\n");
            }
        }
        ReconfigureSection::Chatops => {
            let new_config = reconfigure_chatops(&existing, io).await?;
            let applied =
                confirm_diff_and_apply(&config_path, &new_config, io).await?;
            if applied {
                print_restart_guidance(&config_path, section);
            } else {
                io.print("no changes made\n");
            }
        }
    }
    Ok(())
}

fn section_label(section: ReconfigureSection) -> &'static str {
    match section {
        ReconfigureSection::Audits => "audits.defaults.*",
        ReconfigureSection::Reviewer => "reviewer:",
        ReconfigureSection::Chatops => "chatops:",
    }
}

fn print_restart_guidance(config_path: &Path, section: ReconfigureSection) {
    println!(
        "Patched {} in {}.\nTo apply: sudo -u autocoder autocoder reload",
        section_label(section),
        config_path.display()
    );
}

/// Re-prompt the audit cadences with the operator's current values shown
/// as defaults, returning a clone of `existing` with the updated
/// `audits.defaults`. Decline (`never`) drops the slug from the map.
pub(crate) async fn reconfigure_audits(
    existing: &Config,
    io: &mut dyn WizardIo,
) -> Result<Config> {
    let current: HashMap<String, Cadence> = existing
        .audits
        .as_ref()
        .map(|a| a.defaults.clone())
        .unwrap_or_default();

    io.print("\nReconfigure audit cadences\n");
    io.print(
        "  Each prompt's default is the existing cadence. Pick `n` to disable.\n",
    );

    let mut updated: HashMap<String, Cadence> = HashMap::new();
    for (slug, rec) in LLM_DRIVEN_SLUGS {
        let existing_cadence = current
            .get(*slug)
            .copied()
            .unwrap_or(Cadence::Disabled);
        io.print(&format!("\n  {slug} ({})\n", audit_description(slug)));
        let label = format!("  Cadence (recommended: {})", cadence_label(*rec));
        let chosen = ask_audit_cadence(io, &label, existing_cadence, "never").await?;
        if chosen != Cadence::Disabled {
            updated.insert((*slug).to_string(), chosen);
        }
    }

    let mut new_config = existing.clone();
    if updated.is_empty() {
        new_config.audits = None;
    } else {
        let mut audits = existing.audits.clone().unwrap_or_default();
        audits.defaults = updated;
        new_config.audits = Some(audits);
    }
    Ok(new_config)
}

/// Re-prompt the reviewer block (provider + model + api-key env-var) with
/// existing values as defaults. Returns a clone of `existing` with the
/// updated `reviewer:` block.
pub(crate) async fn reconfigure_reviewer(
    existing: &Config,
    io: &mut dyn WizardIo,
) -> Result<Config> {
    let current_provider_arg = match existing.reviewer.as_ref().map(|r| r.provider) {
        Some(ReviewerProvider::Anthropic) => ReviewerProviderArg::Anthropic,
        Some(ReviewerProvider::OpenAiCompatible) => ReviewerProviderArg::OpenAiCompatible,
        Some(ReviewerProvider::Ollama) => ReviewerProviderArg::Ollama,
        None => ReviewerProviderArg::None,
    };

    io.print("\nReconfigure reviewer\n");
    let idx = io
        .choose(
            "Reviewer provider",
            REVIEWER_OPTIONS,
            reviewer_arg_to_idx(current_provider_arg),
        )
        .await?;
    let provider_arg = idx_to_reviewer_arg(idx);

    let mut new_config = existing.clone();
    new_config.reviewer = match provider_arg {
        ReviewerProviderArg::None => None,
        ReviewerProviderArg::Anthropic
        | ReviewerProviderArg::OpenAiCompatible
        | ReviewerProviderArg::Ollama => {
            let provider = match provider_arg {
                ReviewerProviderArg::Anthropic => ReviewerProvider::Anthropic,
                ReviewerProviderArg::OpenAiCompatible => ReviewerProvider::OpenAiCompatible,
                ReviewerProviderArg::Ollama => ReviewerProvider::Ollama,
                _ => unreachable!(),
            };
            let default_model = existing
                .reviewer
                .as_ref()
                .map(|r| r.model.clone())
                .unwrap_or_else(|| match provider_arg {
                    ReviewerProviderArg::Anthropic => "claude-sonnet-4-6".to_string(),
                    ReviewerProviderArg::Ollama => "qwen2.5-coder:32b".to_string(),
                    _ => "gpt-4o-mini".to_string(),
                });
            let model = ask_default(io, "Reviewer model", &default_model).await?;
            // a37: Ollama branch — prompt for `api_base_url` (REQUIRED
            // by config-load) AND NO `api_key_env` (config-load REJECTS
            // a key for ollama).
            let (api_key_env, api_base_url) = if provider_arg == ReviewerProviderArg::Ollama
            {
                let existing_base = existing
                    .reviewer
                    .as_ref()
                    .and_then(|r| r.api_base_url.clone())
                    .unwrap_or_else(|| "http://localhost:11434".to_string());
                let base = ask_default(io, "Reviewer Ollama base URL", &existing_base).await?;
                (None, Some(base))
            } else {
                let default_env = existing
                    .reviewer
                    .as_ref()
                    .and_then(|r| r.api_key_env.clone())
                    .or_else(|| reviewer_env_var(provider_arg).map(String::from))
                    .unwrap_or_default();
                let api_key_env_raw = ask_default(io, "Reviewer API key env var", &default_env).await?;
                let key_env = if api_key_env_raw.is_empty() {
                    None
                } else {
                    Some(api_key_env_raw)
                };
                let existing_base = existing
                    .reviewer
                    .as_ref()
                    .and_then(|r| r.api_base_url.clone());
                (key_env, existing_base)
            };
            // Preserve all other reviewer fields (inline `api_key`,
            // `prompt_template_path`, etc.) from the existing config —
            // only provider/model/api_key_env/api_base_url are
            // reconfigured here.
            let mut reviewer = existing.reviewer.clone().unwrap_or_else(|| ReviewerConfig {
                enabled: true,
                provider,
                model: model.clone(),
                api_key_env: api_key_env.clone(),
                api_key: None,
                api_base_url: api_base_url.clone(),
                prompt_template_path: None,
                code_review: None,
                auto_revise: false,
                prompt_budget_chars: 2_000_000,
                mode: crate::config::ReviewerMode::Bundled,
                max_code_reviews_per_pr: None,
                suggest_rereview_threshold: None,
                skip_spec_only_prs: false,
            });
            reviewer.provider = provider;
            reviewer.model = model;
            reviewer.api_key_env = api_key_env;
            // For ollama, clear any pre-existing inline `api_key` (the
            // validator would reject it). For other providers, leave
            // inline `api_key` untouched (the operator may have set
            // it deliberately).
            if provider_arg == ReviewerProviderArg::Ollama {
                reviewer.api_key = None;
            }
            reviewer.api_base_url = api_base_url;
            Some(reviewer)
        }
    };
    Ok(new_config)
}

/// Re-prompt the chatops block (provider + default channel id) with
/// existing values as defaults. Returns a clone of `existing` with the
/// updated `chatops:` block (or absent, if the operator picks `none`).
pub(crate) async fn reconfigure_chatops(
    existing: &Config,
    io: &mut dyn WizardIo,
) -> Result<Config> {
    let current_backend_arg = match existing.chatops.as_ref().map(|c| c.provider) {
        Some(ChatOpsProvider::Slack) => ChatOpsBackendArg::Slack,
        Some(ChatOpsProvider::Discord) => ChatOpsBackendArg::Discord,
        Some(ChatOpsProvider::Teams) => ChatOpsBackendArg::Teams,
        Some(ChatOpsProvider::Mattermost) => ChatOpsBackendArg::Mattermost,
        Some(ChatOpsProvider::Matrix) => ChatOpsBackendArg::Matrix,
        None => ChatOpsBackendArg::None,
    };

    io.print("\nReconfigure chatops\n");
    let idx = io
        .choose(
            "ChatOps backend",
            CHATOPS_OPTIONS,
            chatops_arg_to_idx(current_backend_arg),
        )
        .await?;
    let backend_arg = idx_to_chatops_arg(idx);

    let mut new_config = existing.clone();
    if backend_arg == ChatOpsBackendArg::None {
        new_config.chatops = None;
        return Ok(new_config);
    }
    let default_channel = existing
        .chatops
        .as_ref()
        .map(|c| c.default_channel_id.clone())
        .unwrap_or_default();
    let channel = ask_default(io, "ChatOps default channel id", &default_channel).await?;

    // Build the new ChatOpsConfig. If the operator kept the same provider,
    // preserve all unchanged fields (provider-specific tokens, notification
    // settings, etc.); otherwise start from the wizard's slack default
    // (only slack is implemented end-to-end in the wizard today).
    let preserved = existing
        .chatops
        .as_ref()
        .filter(|c| {
            ChatOpsBackendArg::from_provider(c.provider) == backend_arg
        })
        .cloned();
    let mut chatops = match preserved {
        Some(mut existing_chatops) => {
            existing_chatops.default_channel_id = channel.clone();
            existing_chatops
        }
        None => {
            if backend_arg != ChatOpsBackendArg::Slack {
                bail!(
                    "chatops backend `{}` is experimental and not supported by the wizard yet; pick `none` or `slack`",
                    chatops_backend_label(backend_arg)
                );
            }
            ChatOpsConfig {
                provider: ChatOpsProvider::Slack,
                default_channel_id: channel.clone(),
                notifications: None,
                slack: Some(SlackProviderConfig {
                    bot_token_env: Some("SLACK_BOT_TOKEN".to_string()),
                    bot_token: None,
                    app_token_env: None,
                    app_token: None,
                    listen_channels: Vec::new(),
                    dedup_cache_capacity: crate::config::default_dedup_cache_capacity(),
                    dedup_cache_ttl_secs: crate::config::default_dedup_cache_ttl_secs(),
                }),
                discord: None,
                teams: None,
                mattermost: None,
                matrix: None,
            }
        }
    };
    chatops.default_channel_id = channel;
    new_config.chatops = Some(chatops);
    Ok(new_config)
}

impl ChatOpsBackendArg {
    fn from_provider(p: ChatOpsProvider) -> Self {
        match p {
            ChatOpsProvider::Slack => Self::Slack,
            ChatOpsProvider::Discord => Self::Discord,
            ChatOpsProvider::Teams => Self::Teams,
            ChatOpsProvider::Mattermost => Self::Mattermost,
            ChatOpsProvider::Matrix => Self::Matrix,
        }
    }
}

/// Serialize `new_config` and atomically replace `config_path`. On unix,
/// the new file inherits the prior file's mode and owner where stat
/// allows. `serde_yml` does not preserve comments; the wizard-generated
/// audits block carries none so this is acceptable for `--reconfigure
/// audits` and the operator confirms the diff explicitly for reviewer /
/// chatops.
pub(crate) fn apply_in_place_patch(config_path: &Path, new_config: &Config) -> Result<()> {
    let yaml = serialize_config(new_config)?;
    let parent = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path {} has no parent", config_path.display()))?;
    let tmp = parent.join(format!(
        ".{}.reconfigure.tmp",
        config_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("config.yaml")
    ));

    let prior_mode = prior_file_mode(config_path);

    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(yaml.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }

    std::fs::rename(&tmp, config_path).with_context(|| {
        format!("rename {} -> {}", tmp.display(), config_path.display())
    })?;

    #[cfg(unix)]
    if let Some(mode) = prior_mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(config_path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("restore mode on {}", config_path.display()))?;
    }
    let _ = prior_mode;
    Ok(())
}

#[cfg(unix)]
fn prior_file_mode(p: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).ok().map(|m| m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn prior_file_mode(_p: &Path) -> Option<u32> {
    None
}

/// Compute a unified diff between the on-disk YAML and the serialized
/// `new_config`, print it via `io`, and prompt `Apply this patch? [y/N]`.
/// On accept, writes the patch via [`apply_in_place_patch`] and returns
/// `Ok(true)`. On decline, the file is unchanged and returns `Ok(false)`.
pub(crate) async fn confirm_diff_and_apply(
    config_path: &Path,
    new_config: &Config,
    io: &mut dyn WizardIo,
) -> Result<bool> {
    let current = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let new_yaml = serialize_config(new_config)?;
    let diff = similar::TextDiff::from_lines(&current, &new_yaml);
    let unified = diff
        .unified_diff()
        .header("current", "proposed")
        .to_string();
    io.print("\n");
    if unified.trim().is_empty() {
        io.print("No changes between current and proposed config.\n");
    } else {
        io.print(&unified);
        if !unified.ends_with('\n') {
            io.print("\n");
        }
    }
    let accept = io.confirm("Apply this patch?", false).await?;
    if !accept {
        return Ok(false);
    }
    apply_in_place_patch(config_path, new_config)?;
    Ok(true)
}

// ----------------------------------------------------------------------------
// Tests.
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn baseline_answers() -> WizardAnswers {
        WizardAnswers {
            repo_url: "git@github.com:acme/widgets.git".to_string(),
            base_branch: "main".to_string(),
            agent_branch: "agent-q".to_string(),
            poll_interval_sec: 300,
            token_env_var: "GITHUB_TOKEN".to_string(),
            github_pat: Some("ghp_test".to_string()),
            chatops_backend: ChatOpsBackendArg::None,
            chatops_channel_id: None,
            chatops_token: None,
            reviewer_provider: ReviewerProviderArg::None,
            reviewer_model: None,
            reviewer_api_key: None,
            reviewer_api_base_url: None,
            audits: HashMap::new(),
            canonical_rag: None,
        }
    }

    fn slack_answers() -> WizardAnswers {
        WizardAnswers {
            chatops_backend: ChatOpsBackendArg::Slack,
            chatops_channel_id: Some("C0123456789".to_string()),
            chatops_token: Some("xoxb-test".to_string()),
            ..baseline_answers()
        }
    }

    fn reviewer_answers() -> WizardAnswers {
        WizardAnswers {
            reviewer_provider: ReviewerProviderArg::Anthropic,
            reviewer_model: Some("claude-sonnet-4-6".to_string()),
            reviewer_api_key: Some("sk-ant-test".to_string()),
            ..baseline_answers()
        }
    }

    #[tokio::test]
    async fn wizard_collects_minimum_essential_fields() {
        // answers in order: repo, base, agent, poll, token env, PAT,
        // chatops choice "1" (none), reviewer choice "1" (none),
        // then the default-path audit answer (bare-Enter on the LLM gate
        // → no), then bare-Enter on the canonical-RAG gate (a21) → no.
        let mut io = ScriptedIo::new(vec![
            "git@github.com:acme/widgets.git",
            "main",
            "agent-q",
            "300",
            "GITHUB_TOKEN",
            "ghp_test",
            "1",
            "1",
            "",
            "n",
        ]);
        let prefill = WizardPrefill::default();
        let ans = run_wizard(&mut io, InstallMode::Dev, &prefill).await.unwrap();
        assert_eq!(ans.repo_url, "git@github.com:acme/widgets.git");
        assert_eq!(ans.base_branch, "main");
        assert_eq!(ans.agent_branch, "agent-q");
        assert_eq!(ans.poll_interval_sec, 300);
        assert_eq!(ans.token_env_var, "GITHUB_TOKEN");
        assert_eq!(ans.chatops_backend, ChatOpsBackendArg::None);
        assert_eq!(ans.reviewer_provider, ReviewerProviderArg::None);

        let cfg = assemble_config(&ans).unwrap();
        assert_eq!(cfg.repositories[0].url, "git@github.com:acme/widgets.git");
        assert_eq!(cfg.repositories[0].base_branch, "main");
        assert_eq!(cfg.repositories[0].agent_branch, "agent-q");
        assert_eq!(cfg.repositories[0].poll_interval_sec, 300);
        assert_eq!(cfg.github.token_env, "GITHUB_TOKEN");
        assert!(cfg.chatops.is_none());
        assert!(cfg.reviewer.is_none());
    }

    fn ni_args(tmp: &TempDir) -> InstallArgs {
        InstallArgs {
            mode: Some(InstallMode::Dev),
            config_dir: Some(tmp.path().to_path_buf()),
            non_interactive: true,
            repo_url: Some("git@github.com:acme/widgets.git".to_string()),
            token_env_var: Some("GITHUB_TOKEN".to_string()),
            chatops_backend: Some(ChatOpsBackendArg::None),
            reviewer_provider: Some(ReviewerProviderArg::None),
            ..InstallArgs::default()
        }
    }

    #[tokio::test]
    async fn existing_config_skips_wizard() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.yaml");
        std::fs::write(&cfg_path, "# placeholder\n").unwrap();
        let mut io = ScriptedIo::new(vec![]);
        let actions = RecordingActions::new();
        let args = InstallArgs {
            mode: Some(InstallMode::Dev),
            config_dir: Some(tmp.path().to_path_buf()),
            ..InstallArgs::default()
        };
        let r = execute_inner(args, &mut io, &actions, tmp.path().to_path_buf()).await;
        assert!(r.is_ok(), "expected Ok, got {r:?}");
        assert!(io.answers.is_empty(), "no answers should have been consumed");
    }

    #[tokio::test]
    async fn server_mode_calls_expected_system_actions_in_order() {
        let tmp = TempDir::new().unwrap();
        let systemd_dir = tmp.path().join("systemd");
        let actions = RecordingActions::new()
            .with_which("apt-get", None)
            .with_which("claude", Some(PathBuf::from("/usr/local/bin/claude")));
        let mut io = ScriptedIo::new(vec![]);
        let args = InstallArgs {
            mode: Some(InstallMode::Server),
            config_dir: Some(tmp.path().join("conf")),
            non_interactive: true,
            repo_url: Some("git@github.com:acme/widgets.git".to_string()),
            token_env_var: Some("GITHUB_TOKEN".to_string()),
            chatops_backend: Some(ChatOpsBackendArg::None),
            reviewer_provider: Some(ReviewerProviderArg::None),
            ..InstallArgs::default()
        };
        execute_inner(args, &mut io, &actions, systemd_dir).await.unwrap();
        let calls = actions.calls();
        // Find the indices of the milestone calls and assert ordering.
        let pos_create = calls.iter().position(
            |c| matches!(c, RecordedCall::CreateUser { name, .. } if name == "autocoder"),
        );
        let pos_reload = calls.iter().position(|c| matches!(c, RecordedCall::DaemonReload));
        let pos_enable = calls.iter().position(
            |c| matches!(c, RecordedCall::EnableSystemdUnit(n) if n == "autocoder"),
        );
        let pos_start = calls.iter().position(
            |c| matches!(c, RecordedCall::StartSystemdUnit(n) if n == "autocoder"),
        );
        let pc = pos_create.expect("create_user missing");
        let pr = pos_reload.expect("daemon_reload missing");
        let pe = pos_enable.expect("enable_systemd_unit missing");
        let ps = pos_start.expect("start_systemd_unit missing");
        assert!(pc < pr && pr < pe && pe < ps, "server-mode call ordering broken: {calls:?}");
    }

    #[tokio::test]
    async fn dev_mode_does_not_call_useradd_or_systemctl() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new()
            .with_which("apt-get", None)
            .with_which("claude", Some(PathBuf::from("/usr/local/bin/claude")));
        let mut io = ScriptedIo::new(vec![]);
        let args = ni_args(&tmp);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf()).await.unwrap();
        let calls = actions.calls();
        for c in &calls {
            match c {
                RecordedCall::CreateUser { .. }
                | RecordedCall::DaemonReload
                | RecordedCall::EnableSystemdUnit(_)
                | RecordedCall::StartSystemdUnit(_) => panic!("unexpected server-mode call: {c:?}"),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn non_interactive_succeeds_with_all_required_flags() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = ni_args(&tmp);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf()).await.unwrap();
        assert!(io.answers.is_empty(), "no answers should have been consumed");
    }

    #[tokio::test]
    async fn non_interactive_errors_on_missing_required_flag() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new();
        let mut io = ScriptedIo::new(vec![]);
        let args = InstallArgs {
            mode: Some(InstallMode::Dev),
            config_dir: Some(tmp.path().to_path_buf()),
            non_interactive: true,
            // missing --repo-url
            token_env_var: Some("GITHUB_TOKEN".to_string()),
            chatops_backend: Some(ChatOpsBackendArg::None),
            reviewer_provider: Some(ReviewerProviderArg::None),
            ..InstallArgs::default()
        };
        let err =
            execute_inner(args, &mut io, &actions, tmp.path().to_path_buf()).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("--repo-url"), "error should name --repo-url: {msg}");
    }

    #[tokio::test]
    async fn chatops_choice_writes_secrets_and_config_references_env_var() {
        let ans = slack_answers();
        let cfg = assemble_config(&ans).unwrap();
        let yaml = serialize_config(&cfg).unwrap();
        assert!(yaml.contains("bot_token_env: SLACK_BOT_TOKEN"), "YAML missing slack env-var ref:\n{yaml}");
        let env = assemble_secrets_env(&ans);
        assert!(env.contains("SLACK_BOT_TOKEN=xoxb-test"), "secrets.env missing slack token:\n{env}");
        assert!(env.contains("GITHUB_TOKEN=ghp_test"), "secrets.env missing github token:\n{env}");
    }

    #[tokio::test]
    async fn reviewer_choice_writes_api_key_and_config_picks_provider() {
        let ans = reviewer_answers();
        let cfg = assemble_config(&ans).unwrap();
        let yaml = serialize_config(&cfg).unwrap();
        assert!(yaml.contains("provider: anthropic"), "YAML missing reviewer provider:\n{yaml}");
        assert!(yaml.contains("model: claude-sonnet-4-6"), "YAML missing reviewer model:\n{yaml}");
        let env = assemble_secrets_env(&ans);
        assert!(env.contains("ANTHROPIC_API_KEY=sk-ant-test"), "secrets.env missing reviewer key:\n{env}");
    }

    #[test]
    fn systemd_unit_declares_four_standard_directories() {
        // The bundled unit template must declare the four
        // *Directory=autocoder directives so systemd auto-creates
        // /var/lib/autocoder, /var/cache/autocoder, /var/log/autocoder,
        // and /run/autocoder owned by the service user and populates
        // $STATE_DIRECTORY, $CACHE_DIRECTORY, $LOGS_DIRECTORY, and
        // $RUNTIME_DIRECTORY for the daemon's path resolver to pick
        // up.
        for directive in [
            "StateDirectory=autocoder",
            "CacheDirectory=autocoder",
            "LogsDirectory=autocoder",
            "RuntimeDirectory=autocoder",
        ] {
            assert!(
                SYSTEMD_UNIT.contains(directive),
                "rendered unit must contain `{directive}`:\n{SYSTEMD_UNIT}"
            );
        }
    }

    #[test]
    fn dev_mode_assemble_writes_xdg_paths_into_config() {
        // Set HOME to a fixture path; the dev-mode install renders
        // the XDG defaults into the generated config so operators
        // see the resolved values without re-deriving them.
        let prior_home = std::env::var_os("HOME");
        let prior_state = std::env::var_os("XDG_STATE_HOME");
        let prior_cache = std::env::var_os("XDG_CACHE_HOME");
        let prior_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe {
            std::env::set_var("HOME", "/home/fixture");
            std::env::remove_var("XDG_STATE_HOME");
            std::env::remove_var("XDG_CACHE_HOME");
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let paths = xdg_paths_for_dev_mode();
        assert_eq!(
            paths.state_dir,
            Some(PathBuf::from("/home/fixture/.local/state/autocoder"))
        );
        assert_eq!(
            paths.cache_dir,
            Some(PathBuf::from("/home/fixture/.cache/autocoder"))
        );
        assert_eq!(
            paths.logs_dir,
            Some(PathBuf::from("/home/fixture/.local/state/autocoder/logs"))
        );
        assert!(paths.runtime_dir.is_some());
        unsafe {
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            if let Some(v) = prior_state {
                std::env::set_var("XDG_STATE_HOME", v);
            }
            if let Some(v) = prior_cache {
                std::env::set_var("XDG_CACHE_HOME", v);
            }
            if let Some(v) = prior_runtime {
                std::env::set_var("XDG_RUNTIME_DIR", v);
            }
        }
    }

    #[tokio::test]
    async fn assemble_config_round_trips_through_serde() {
        let ans = slack_answers();
        let cfg = assemble_config(&ans).unwrap();
        let yaml = serialize_config(&cfg).unwrap();
        let round: Config = serde_yml::from_str(&yaml)
            .expect("round-trip YAML should deserialize");
        assert_eq!(round.repositories[0].url, cfg.repositories[0].url);
        assert_eq!(round.github.token_env, cfg.github.token_env);
        assert_eq!(round.chatops.is_some(), cfg.chatops.is_some());
    }

    #[tokio::test]
    async fn apt_install_skipped_on_non_debian() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new()
            .with_which("apt-get", None)
            .with_which("claude", Some(PathBuf::from("/usr/local/bin/claude")));
        let mut io = ScriptedIo::new(vec![]);
        let args = ni_args(&tmp);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf()).await.unwrap();
        let calls = actions.calls();
        let saw_apt = calls.iter().any(|c| matches!(c, RecordedCall::AptInstall(_)));
        assert!(!saw_apt, "expected zero apt_install calls, got {calls:?}");
    }

    #[tokio::test]
    async fn claude_install_skipped_when_already_present() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new()
            .with_which("apt-get", None)
            .with_which("claude", Some(PathBuf::from("/usr/local/bin/claude")));
        let mut io = ScriptedIo::new(vec![]);
        let args = ni_args(&tmp);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf()).await.unwrap();
        let calls = actions.calls();
        let saw_claude_install = calls.iter().any(|c| {
            matches!(c, RecordedCall::RunSubprocess { cmd, args }
                if cmd == "bash" && args.iter().any(|a| a.contains("claude.ai/install.sh")))
        });
        assert!(
            !saw_claude_install,
            "claude installer should not run when `claude` is already on PATH; calls={calls:?}"
        );
    }

    // ----- audits ---------------------------------------------------------

    /// Build the minimum prompt-answer queue for the existing pre-audit
    /// wizard prompts (repo / branches / poll / token env / PAT / chatops
    /// "none" / reviewer "none"). Audit-prompt answers are appended by the
    /// individual tests.
    fn baseline_wizard_answers() -> Vec<&'static str> {
        vec![
            "git@github.com:acme/widgets.git",
            "main",
            "agent-q",
            "300",
            "GITHUB_TOKEN",
            "ghp_test",
            "1",
            "1",
        ]
    }

    #[tokio::test]
    async fn wizard_audits_default_path_declines_all_audits() {
        let mut answers = baseline_wizard_answers();
        // Bare-Enter on LLM gate → no.
        answers.push("");
        // RAG gate (a21) bare-Enter → no.
        answers.push("n");
        let mut io = ScriptedIo::new(answers);
        let ans = run_wizard(&mut io, InstallMode::Dev, &WizardPrefill::default())
            .await
            .unwrap();
        assert!(
            ans.audits.is_empty(),
            "default path declines all audits; got {:?}",
            ans.audits
        );

        let cfg = assemble_config(&ans).unwrap();
        assert!(cfg.audits.is_none(), "no enabled audits → no audits: block");
        let yaml = serialize_config(&cfg).unwrap();
        for (slug, _) in LLM_DRIVEN_SLUGS {
            assert!(
                !yaml.contains(&format!("{slug}:")),
                "YAML must not list {slug}:\n{yaml}"
            );
        }
    }

    #[tokio::test]
    async fn wizard_audits_fast_path_enables_all_llm_driven() {
        let mut answers = baseline_wizard_answers();
        // LLM gate yes, fast-path default Y.
        answers.push("y"); // LLM gate
        answers.push(""); // fast-path (default Y)
        // RAG gate (a21) → no.
        answers.push("n");
        let mut io = ScriptedIo::new(answers);
        let ans = run_wizard(&mut io, InstallMode::Dev, &WizardPrefill::default())
            .await
            .unwrap();
        for (slug, rec) in LLM_DRIVEN_SLUGS {
            assert_eq!(
                ans.audits.get(*slug),
                Some(rec),
                "{slug} should be set to {:?}",
                rec
            );
        }
        let cfg = assemble_config(&ans).unwrap();
        let audits = cfg.audits.as_ref().expect("audits block present");
        assert_eq!(
            audits.defaults.len(),
            LLM_DRIVEN_SLUGS.len(),
            "every LLM-driven audit slug must be present"
        );
    }

    #[tokio::test]
    async fn wizard_audits_per_audit_cadence_choices_respected() {
        let mut answers = baseline_wizard_answers();
        // LLM y, fast-path n, per-audit walk:
        //   architecture_brightline: daily
        //   drift_audit: monthly
        //   missing_tests_audit: weekly
        //   security_bug_audit: never
        //   architecture_consultative: daily
        //   documentation_audit: monthly
        answers.push("y");
        answers.push("n");
        answers.push("d"); // architecture_brightline
        answers.push("m"); // drift_audit
        answers.push("w"); // missing_tests_audit
        answers.push("n"); // security_bug_audit (never)
        answers.push("d"); // architecture_consultative
        answers.push("m"); // documentation_audit
        // RAG gate (a21) → no.
        answers.push("n");
        let mut io = ScriptedIo::new(answers);
        let ans = run_wizard(&mut io, InstallMode::Dev, &WizardPrefill::default())
            .await
            .unwrap();
        assert_eq!(ans.audits.get("architecture_brightline"), Some(&Cadence::Daily));
        assert_eq!(ans.audits.get("drift_audit"), Some(&Cadence::Monthly));
        assert_eq!(ans.audits.get("missing_tests_audit"), Some(&Cadence::Weekly));
        // never → omitted from the map entirely.
        assert!(ans.audits.get("security_bug_audit").is_none());
        assert_eq!(
            ans.audits.get("architecture_consultative"),
            Some(&Cadence::Daily)
        );
        assert_eq!(ans.audits.get("documentation_audit"), Some(&Cadence::Monthly));
    }

    fn ni_args_audits(tmp: &TempDir) -> InstallArgs {
        InstallArgs {
            mode: Some(InstallMode::Dev),
            config_dir: Some(tmp.path().to_path_buf()),
            non_interactive: true,
            repo_url: Some("git@github.com:acme/widgets.git".to_string()),
            token_env_var: Some("GITHUB_TOKEN".to_string()),
            chatops_backend: Some(ChatOpsBackendArg::None),
            reviewer_provider: Some(ReviewerProviderArg::None),
            ..InstallArgs::default()
        }
    }

    fn load_yaml(tmp: &TempDir) -> String {
        std::fs::read_to_string(tmp.path().join("config.yaml")).unwrap()
    }

    #[tokio::test]
    async fn non_interactive_no_audit_flags_enables_no_audits() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = ni_args_audits(&tmp);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let yaml = load_yaml(&tmp);
        for (slug, _) in LLM_DRIVEN_SLUGS {
            assert!(
                !yaml.contains(&format!("{slug}:")),
                "{slug} must NOT be in conservative default yaml:\n{yaml}"
            );
        }
    }

    #[tokio::test]
    async fn non_interactive_audits_llm_driven_recommended_enables_all_five() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = InstallArgs {
            audits_llm_driven: Some(LlmDrivenAuditsArg::Recommended),
            ..ni_args_audits(&tmp)
        };
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let yaml = load_yaml(&tmp);
        for (slug, rec) in LLM_DRIVEN_SLUGS {
            let expected = format!("{slug}: {}", cadence_label(*rec));
            assert!(
                yaml.contains(&expected),
                "yaml must include `{expected}`:\n{yaml}"
            );
        }
    }

    #[tokio::test]
    async fn non_interactive_per_audit_flag_overrides_recommended() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = InstallArgs {
            audits_llm_driven: Some(LlmDrivenAuditsArg::Recommended),
            audit_security_bug_audit: Some(AuditCadenceArg::Disabled),
            ..ni_args_audits(&tmp)
        };
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let yaml = load_yaml(&tmp);
        assert!(
            !yaml.contains("security_bug_audit:"),
            "security_bug_audit must be omitted when disabled via override:\n{yaml}"
        );
        for (slug, rec) in LLM_DRIVEN_SLUGS {
            if *slug == "security_bug_audit" {
                continue;
            }
            let expected = format!("{slug}: {}", cadence_label(*rec));
            assert!(yaml.contains(&expected), "missing `{expected}`:\n{yaml}");
        }
    }

    #[tokio::test]
    async fn non_interactive_llm_driven_none_overrides_per_audit_flags() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = InstallArgs {
            audits_llm_driven: Some(LlmDrivenAuditsArg::None),
            audit_architecture_brightline: Some(AuditCadenceArg::Weekly),
            ..ni_args_audits(&tmp)
        };
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let yaml = load_yaml(&tmp);
        assert!(
            !yaml.contains("architecture_brightline:"),
            "master switch `none` must override per-audit flag:\n{yaml}"
        );
    }

    // ----- secrets.env permissions ---------------------------------------

    /// Fresh install: secrets.env must end at mode 0o600 even though the
    /// `RecordingActions` mock turns the post-write `chmod` into a no-op.
    /// Passing this test proves the file was *created* with 0o600, not
    /// merely chmod'd to 0o600 after-the-fact.
    #[cfg(unix)]
    #[tokio::test]
    async fn secrets_env_is_created_with_0600_before_any_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = ni_args(&tmp);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();

        let secrets_path = tmp.path().join("secrets.env");
        let secrets_mode = std::fs::metadata(&secrets_path)
            .expect("secrets.env should exist after install")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            secrets_mode, 0o600,
            "secrets.env must be created with mode 0o600; got {secrets_mode:o}"
        );

        // Dev-mode config.yaml should also be born at 0o600.
        let config_path = tmp.path().join("config.yaml");
        let config_mode = std::fs::metadata(&config_path)
            .expect("config.yaml should exist after install")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            config_mode, 0o600,
            "dev-mode config.yaml must be created with mode 0o600; got {config_mode:o}"
        );
    }

    /// Operator-rerun regression: a pre-existing world-readable
    /// secrets.env (e.g. orphaned by a botched prior install that crashed
    /// after writing secrets.env but before writing config.yaml) must end
    /// up at 0o600 after the install reruns over it. `write_file_with_mode`
    /// should tighten the file's permissions BEFORE truncating it.
    #[cfg(unix)]
    #[tokio::test]
    async fn reinstall_over_world_readable_secrets_env_ends_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        // Pre-create a world-readable secrets.env to simulate a prior
        // botched install. Leave config.yaml absent so the install path
        // does not short-circuit on the existing-config check.
        let secrets_path = tmp.path().join("secrets.env");
        std::fs::write(&secrets_path, b"GITHUB_TOKEN=stale\n").unwrap();
        std::fs::set_permissions(&secrets_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&secrets_path).unwrap().permissions().mode() & 0o777,
            0o644,
            "preconditions: secrets.env should start at 0o644"
        );

        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = ni_args(&tmp);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();

        let mode = std::fs::metadata(&secrets_path)
            .expect("secrets.env should still exist after install")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "re-install over a world-readable secrets.env must tighten to 0o600; got {mode:o}"
        );
    }

    /// Direct unit test on the helper: covers the truncate-after-chmod
    /// invariant in isolation. If the file existed wider than `mode`, the
    /// helper must chmod it down BEFORE truncating, so there is no window
    /// where the file is empty-but-still-world-readable.
    #[cfg(unix)]
    #[test]
    fn write_file_with_mode_tightens_existing_file_before_truncating() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("secret");
        std::fs::write(&p, b"old\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_file_with_mode(&p, b"new\n", 0o600).unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "helper must end at 0o600 over a pre-existing 0o644 file");
        assert_eq!(std::fs::read(&p).unwrap(), b"new\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_file_with_mode_creates_new_file_with_requested_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("secret");

        write_file_with_mode(&p, b"hello\n", 0o600).unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fresh-create mode must equal requested mode");
        assert_eq!(std::fs::read(&p).unwrap(), b"hello\n");
    }

    // ----- systemd probe parsing -----------------------------------------

    #[test]
    fn parse_systemctl_show_loaded_unit_with_config_flag() {
        let fixture = "\
LoadState=loaded
FragmentPath=/etc/systemd/system/autocoder.service
ExecStart={ path=/usr/local/bin/autocoder ; argv[]=/usr/local/bin/autocoder run --config /home/autocoder/autocoder/config.yaml ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }
";
        let probe = parse_systemctl_show(fixture);
        assert_eq!(probe.load_state, LoadState::Loaded);
        assert_eq!(
            probe.fragment_path,
            Some(PathBuf::from("/etc/systemd/system/autocoder.service"))
        );
        assert_eq!(
            probe.exec_start_config_path,
            Some(PathBuf::from("/home/autocoder/autocoder/config.yaml"))
        );
    }

    #[test]
    fn parse_systemctl_show_loaded_unit_without_config_flag() {
        let fixture = "\
LoadState=loaded
FragmentPath=/etc/systemd/system/autocoder.service
ExecStart={ path=/usr/local/bin/autocoder ; argv[]=/usr/local/bin/autocoder run ; ignore_errors=no }
";
        let probe = parse_systemctl_show(fixture);
        assert_eq!(probe.load_state, LoadState::Loaded);
        assert!(probe.fragment_path.is_some());
        assert_eq!(probe.exec_start_config_path, None);
    }

    #[test]
    fn parse_systemctl_show_not_found_unit() {
        let fixture = "\
LoadState=not-found
FragmentPath=
ExecStart=
";
        let probe = parse_systemctl_show(fixture);
        assert_eq!(probe.load_state, LoadState::NotFound);
        assert_eq!(probe.fragment_path, None);
        assert_eq!(probe.exec_start_config_path, None);
    }

    #[test]
    fn parse_systemctl_show_masked_unit() {
        let fixture = "\
LoadState=masked
FragmentPath=
ExecStart=
";
        let probe = parse_systemctl_show(fixture);
        assert_eq!(probe.load_state, LoadState::Other("masked".to_string()));
        assert_eq!(probe.fragment_path, None);
        assert_eq!(probe.exec_start_config_path, None);
    }

    #[test]
    fn parse_systemctl_show_config_equals_form() {
        // `--config=path` (single-token form) is also a legitimate way to
        // write the flag; the parser must accept it.
        let fixture = "\
LoadState=loaded
FragmentPath=/etc/systemd/system/autocoder.service
ExecStart={ path=/usr/local/bin/autocoder ; argv[]=/usr/local/bin/autocoder run --config=/srv/autocoder/config.yaml ; ignore_errors=no }
";
        let probe = parse_systemctl_show(fixture);
        assert_eq!(
            probe.exec_start_config_path,
            Some(PathBuf::from("/srv/autocoder/config.yaml"))
        );
    }

    #[test]
    fn parse_systemctl_show_config_flag_with_no_value_returns_none() {
        // Operator wrote `--config --other-flag` — no value. Must not
        // misinterpret `--other-flag` as the config path.
        let fixture = "\
LoadState=loaded
FragmentPath=/etc/systemd/system/autocoder.service
ExecStart={ argv[]=/usr/local/bin/autocoder run --config --verbose ; ignore_errors=no }
";
        let probe = parse_systemctl_show(fixture);
        assert_eq!(probe.exec_start_config_path, None);
    }

    // ----- detect_existing_install execute_inner integration -------------

    fn loaded_probe(config_path: &Path) -> SystemdUnitProbe {
        SystemdUnitProbe {
            load_state: LoadState::Loaded,
            fragment_path: Some(PathBuf::from("/etc/systemd/system/autocoder.service")),
            exec_start_config_path: Some(config_path.to_path_buf()),
        }
    }

    fn server_ni_args(config_dir: PathBuf) -> InstallArgs {
        InstallArgs {
            mode: Some(InstallMode::Server),
            config_dir: Some(config_dir),
            non_interactive: true,
            repo_url: Some("git@github.com:acme/widgets.git".to_string()),
            token_env_var: Some("GITHUB_TOKEN".to_string()),
            chatops_backend: Some(ChatOpsBackendArg::None),
            reviewer_provider: Some(ReviewerProviderArg::None),
            ..InstallArgs::default()
        }
    }

    #[tokio::test]
    async fn detect_existing_install_loaded_unit_with_existing_config_short_circuits() {
        let tmp = TempDir::new().unwrap();
        let unit_config = tmp.path().join("home-autocoder-config.yaml");
        std::fs::write(&unit_config, b"# placeholder\n").unwrap();

        let actions = RecordingActions::new()
            .with_probe_response("autocoder.service", loaded_probe(&unit_config));
        let mut io = ScriptedIo::new(vec![]);
        let args = server_ni_args(tmp.path().join("fresh-config-dir"));
        let r = execute_inner(args, &mut io, &actions, tmp.path().join("systemd")).await;
        assert!(r.is_ok(), "expected Ok, got {r:?}");

        let calls = actions.calls();
        // Probe was invoked exactly once with the correct unit name.
        let probe_calls: Vec<_> = calls
            .iter()
            .filter(|c| matches!(c, RecordedCall::ProbeSystemdUnit(n) if n == "autocoder.service"))
            .collect();
        assert_eq!(probe_calls.len(), 1, "probe called once: {calls:?}");

        // No mutation-side-effect calls fired.
        for c in &calls {
            match c {
                RecordedCall::CreateUser { .. }
                | RecordedCall::AptInstall(_)
                | RecordedCall::DaemonReload
                | RecordedCall::EnableSystemdUnit(_)
                | RecordedCall::StartSystemdUnit(_)
                | RecordedCall::Chown { .. }
                | RecordedCall::Chmod { .. } => {
                    panic!("unexpected side-effect call after probe short-circuit: {c:?}");
                }
                _ => {}
            }
        }

        // The fresh-config-dir was NOT created (no wizard run, no config
        // write).
        assert!(
            !tmp.path().join("fresh-config-dir").join("config.yaml").exists(),
            "wizard must not have written config.yaml after short-circuit"
        );
    }

    #[tokio::test]
    async fn detect_existing_install_loaded_unit_with_missing_config_errors() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.yaml");

        let actions = RecordingActions::new()
            .with_probe_response("autocoder.service", loaded_probe(&missing));
        let mut io = ScriptedIo::new(vec![]);
        let args = server_ni_args(tmp.path().join("conf"));
        let err = execute_inner(args, &mut io, &actions, tmp.path().join("systemd"))
            .await
            .expect_err("expected broken-install error");
        let msg = format!("{err}");
        assert!(
            msg.contains("/etc/systemd/system/autocoder.service"),
            "error must name FragmentPath: {msg}"
        );
        assert!(
            msg.contains(&missing.display().to_string()),
            "error must name missing config path: {msg}"
        );
        assert!(
            msg.contains("restore") && msg.contains("rm "),
            "error must hint at remediations: {msg}"
        );
    }

    #[tokio::test]
    async fn detect_existing_install_not_found_falls_through_to_default_path() {
        let tmp = TempDir::new().unwrap();
        // No probe response → default-fixture is NotFound.
        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = server_ni_args(tmp.path().join("conf"));
        execute_inner(args, &mut io, &actions, tmp.path().join("systemd"))
            .await
            .unwrap();
        let calls = actions.calls();
        // Probe was invoked.
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, RecordedCall::ProbeSystemdUnit(n) if n == "autocoder.service")),
            "probe must run in server mode: {calls:?}"
        );
        // ... and the wizard proceeded to write the config.
        assert!(
            tmp.path().join("conf").join("config.yaml").exists(),
            "wizard must have written config.yaml when probe found no unit"
        );
    }

    #[tokio::test]
    async fn detect_existing_install_not_found_with_default_path_config_hits_idempotency() {
        let tmp = TempDir::new().unwrap();
        let conf = tmp.path().join("conf");
        std::fs::create_dir_all(&conf).unwrap();
        std::fs::write(conf.join("config.yaml"), b"# placeholder\n").unwrap();
        // No probe response → NotFound; default-path config exists → should
        // hit the existing idempotency short-circuit.
        let actions = RecordingActions::new();
        let mut io = ScriptedIo::new(vec![]);
        let args = server_ni_args(conf.clone());
        execute_inner(args, &mut io, &actions, tmp.path().join("systemd"))
            .await
            .unwrap();
        let calls = actions.calls();
        for c in &calls {
            match c {
                RecordedCall::CreateUser { .. }
                | RecordedCall::AptInstall(_)
                | RecordedCall::DaemonReload
                | RecordedCall::EnableSystemdUnit(_)
                | RecordedCall::StartSystemdUnit(_) => {
                    panic!("unexpected wizard call after default-path idempotency exit: {c:?}");
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn detect_existing_install_dev_mode_skips_probe() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new().with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = ni_args(&tmp);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let calls = actions.calls();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, RecordedCall::ProbeSystemdUnit(_))),
            "dev-mode install must NOT invoke probe_systemd_unit: {calls:?}"
        );
    }

    #[tokio::test]
    async fn detect_existing_install_loaded_unit_no_config_flag_falls_through() {
        let tmp = TempDir::new().unwrap();
        // Loaded unit but no exec_start_config_path (parser couldn't extract
        // it). Should fall through to the default-path check and proceed
        // with the wizard since no config.yaml exists there.
        let probe = SystemdUnitProbe {
            load_state: LoadState::Loaded,
            fragment_path: Some(PathBuf::from("/etc/systemd/system/autocoder.service")),
            exec_start_config_path: None,
        };
        let actions = RecordingActions::new()
            .with_probe_response("autocoder.service", probe)
            .with_which("claude", Some(PathBuf::from("/c")));
        let mut io = ScriptedIo::new(vec![]);
        let args = server_ni_args(tmp.path().join("conf"));
        execute_inner(args, &mut io, &actions, tmp.path().join("systemd"))
            .await
            .unwrap();
        // Wizard proceeded — config.yaml exists at the default path.
        assert!(
            tmp.path().join("conf").join("config.yaml").exists(),
            "wizard must have written config.yaml after fall-through with WARN"
        );
    }

    // ----- --reconfigure -------------------------------------------------

    /// Build a fixture `config.yaml` with audits/reviewer/chatops set so
    /// reconfigure tests have realistic state to mutate.
    fn fixture_install_yaml() -> String {
        // Assemble via the wizard's `assemble_config` so the YAML stays in
        // sync with whatever fields the bundled example carries.
        let ans = WizardAnswers {
            chatops_backend: ChatOpsBackendArg::Slack,
            chatops_channel_id: Some("C0123456789".to_string()),
            chatops_token: Some("xoxb-test".to_string()),
            reviewer_provider: ReviewerProviderArg::Anthropic,
            reviewer_model: Some("claude-sonnet-4-6".to_string()),
            reviewer_api_key: Some("sk-ant-test".to_string()),
            audits: {
                let mut m = HashMap::new();
                m.insert("drift_audit".to_string(), Cadence::Weekly);
                m
            },
            ..baseline_answers()
        };
        let cfg = assemble_config(&ans).expect("fixture assemble_config");
        serialize_config(&cfg).expect("fixture serialize_config")
    }

    fn write_fixture_config(tmp: &TempDir) -> PathBuf {
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, fixture_install_yaml()).unwrap();
        p
    }

    // --- 2.2 resolve_existing_config_path -----------------------------------

    #[tokio::test]
    async fn resolve_existing_config_path_dev_uses_home_default() {
        let tmp = TempDir::new().unwrap();
        let prior_home = std::env::var_os("HOME");
        let home = tmp.path().to_path_buf();
        let cfg_dir = home.join(".config/autocoder");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.yaml"), b"# dev fixture\n").unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let args = InstallArgs::default();
        let actions = RecordingActions::new();
        let got = resolve_existing_config_path(&args, &actions, InstallMode::Dev)
            .await
            .unwrap();
        assert_eq!(got, cfg_dir.join("config.yaml"));
        unsafe {
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        // Dev mode must not invoke the systemd probe.
        for c in actions.calls() {
            assert!(
                !matches!(c, RecordedCall::ProbeSystemdUnit(_)),
                "dev mode must skip probe; saw {c:?}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_existing_config_path_server_probe_loaded_with_path() {
        let tmp = TempDir::new().unwrap();
        let unit_config = tmp.path().join("custom-config.yaml");
        std::fs::write(&unit_config, b"# unit config\n").unwrap();
        let actions = RecordingActions::new().with_probe_response(
            "autocoder.service",
            loaded_probe(&unit_config),
        );
        let args = InstallArgs::default();
        let got = resolve_existing_config_path(&args, &actions, InstallMode::Server)
            .await
            .unwrap();
        assert_eq!(got, unit_config);
    }

    #[tokio::test]
    async fn resolve_existing_config_path_server_probe_not_found_no_default_bails() {
        let actions = RecordingActions::new(); // default probe: NotFound
        let args = InstallArgs::default();
        // /etc/autocoder/config.yaml almost certainly does not exist in
        // the sandbox. Assert the bail message rather than the path.
        let err = resolve_existing_config_path(&args, &actions, InstallMode::Server)
            .await
            .expect_err("expected bail when no probe + no default");
        assert!(
            format!("{err}").contains("no existing install detected"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_existing_config_path_honors_config_dir_override() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.yaml"), b"# override\n").unwrap();
        let actions = RecordingActions::new();
        let args = InstallArgs {
            config_dir: Some(tmp.path().to_path_buf()),
            ..InstallArgs::default()
        };
        let got = resolve_existing_config_path(&args, &actions, InstallMode::Server)
            .await
            .unwrap();
        assert_eq!(got, tmp.path().join("config.yaml"));
        // Override must short-circuit before the probe runs.
        for c in actions.calls() {
            assert!(
                !matches!(c, RecordedCall::ProbeSystemdUnit(_)),
                "config_dir override must skip probe; saw {c:?}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_existing_config_path_override_missing_bails() {
        let tmp = TempDir::new().unwrap();
        let actions = RecordingActions::new();
        let args = InstallArgs {
            config_dir: Some(tmp.path().to_path_buf()),
            ..InstallArgs::default()
        };
        let err = resolve_existing_config_path(&args, &actions, InstallMode::Server)
            .await
            .expect_err("expected bail when override path is empty");
        assert!(
            format!("{err}").contains("no existing install detected"),
            "unexpected error: {err}"
        );
    }

    // --- 4.4 per-section helpers --------------------------------------------

    #[tokio::test]
    async fn reconfigure_audits_updates_defaults_and_drops_disabled() {
        let raw = fixture_install_yaml();
        let existing: Config = serde_yml::from_str(&raw).unwrap();
        // LLM_DRIVEN_SLUGS order is: architecture_brightline, drift_audit,
        // missing_tests_audit, security_bug_audit, architecture_consultative,
        // documentation_audit. The wizard re-prompts each in order.
        let mut io = ScriptedIo::new(vec![
            "w", // architecture_brightline -> weekly
            "m", // drift_audit -> monthly (was weekly)
            "n", // missing_tests_audit -> never (drop)
            "d", // security_bug_audit -> daily
            "",  // architecture_consultative -> default (existing = disabled)
            "m", // documentation_audit -> monthly
        ]);
        let new_cfg = reconfigure_audits(&existing, &mut io).await.unwrap();
        let defaults = new_cfg
            .audits
            .as_ref()
            .expect("audits block present")
            .defaults
            .clone();
        assert_eq!(defaults.get("architecture_brightline"), Some(&Cadence::Weekly));
        assert_eq!(defaults.get("drift_audit"), Some(&Cadence::Monthly));
        assert!(!defaults.contains_key("missing_tests_audit"));
        assert_eq!(defaults.get("security_bug_audit"), Some(&Cadence::Daily));
        assert!(!defaults.contains_key("architecture_consultative"));
        assert_eq!(defaults.get("documentation_audit"), Some(&Cadence::Monthly));
    }

    #[tokio::test]
    async fn reconfigure_audits_all_never_drops_block() {
        let raw = fixture_install_yaml();
        let existing: Config = serde_yml::from_str(&raw).unwrap();
        let mut io = ScriptedIo::new(vec!["n", "n", "n", "n", "n", "n"]);
        let new_cfg = reconfigure_audits(&existing, &mut io).await.unwrap();
        assert!(new_cfg.audits.is_none(), "no audits enabled → block omitted");
    }

    #[tokio::test]
    async fn reconfigure_reviewer_switches_provider_and_model() {
        let raw = fixture_install_yaml();
        let existing: Config = serde_yml::from_str(&raw).unwrap();
        let mut io = ScriptedIo::new(vec![
            "3",                  // provider choice: openai_compatible (index 2 + 1)
            "grok-3",             // model
            "OPENAI_API_KEY",     // env var (bare-Enter would accept the existing default)
        ]);
        let new_cfg = reconfigure_reviewer(&existing, &mut io).await.unwrap();
        let r = new_cfg.reviewer.expect("reviewer block present");
        assert_eq!(r.provider, ReviewerProvider::OpenAiCompatible);
        assert_eq!(r.model, "grok-3");
        assert_eq!(r.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[tokio::test]
    async fn reconfigure_reviewer_pick_none_clears_block() {
        let raw = fixture_install_yaml();
        let existing: Config = serde_yml::from_str(&raw).unwrap();
        let mut io = ScriptedIo::new(vec!["1"]); // "none"
        let new_cfg = reconfigure_reviewer(&existing, &mut io).await.unwrap();
        assert!(new_cfg.reviewer.is_none());
    }

    #[tokio::test]
    async fn reconfigure_chatops_updates_channel_id() {
        let raw = fixture_install_yaml();
        let existing: Config = serde_yml::from_str(&raw).unwrap();
        let mut io = ScriptedIo::new(vec![
            "2",            // slack (idx 1 + 1)
            "C9999999999",  // new channel id
        ]);
        let new_cfg = reconfigure_chatops(&existing, &mut io).await.unwrap();
        let c = new_cfg.chatops.expect("chatops block present");
        assert_eq!(c.provider, ChatOpsProvider::Slack);
        assert_eq!(c.default_channel_id, "C9999999999");
        // Existing slack sub-block (with bot_token_env) preserved.
        let slack = c.slack.expect("slack sub-block preserved");
        assert_eq!(slack.bot_token_env.as_deref(), Some("SLACK_BOT_TOKEN"));
    }

    #[tokio::test]
    async fn reconfigure_chatops_pick_none_drops_block() {
        let raw = fixture_install_yaml();
        let existing: Config = serde_yml::from_str(&raw).unwrap();
        let mut io = ScriptedIo::new(vec!["1"]); // "none"
        let new_cfg = reconfigure_chatops(&existing, &mut io).await.unwrap();
        assert!(new_cfg.chatops.is_none());
    }

    // --- 5.2 apply_in_place_patch -------------------------------------------

    #[test]
    fn apply_in_place_patch_updates_audits_subtree_only() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = write_fixture_config(&tmp);
        let raw_before = std::fs::read_to_string(&cfg_path).unwrap();
        let mut new_cfg: Config = serde_yml::from_str(&raw_before).unwrap();
        // Pre-condition: existing config carries drift_audit=weekly.
        {
            let defaults = new_cfg
                .audits
                .as_ref()
                .map(|a| a.defaults.clone())
                .unwrap_or_default();
            assert_eq!(defaults.get("drift_audit"), Some(&Cadence::Weekly));
        }
        // Mutate the audits subtree only.
        let mut audits = new_cfg.audits.clone().unwrap_or_default();
        audits
            .defaults
            .insert("drift_audit".to_string(), Cadence::Monthly);
        new_cfg.audits = Some(audits);

        apply_in_place_patch(&cfg_path, &new_cfg).unwrap();

        let raw_after = std::fs::read_to_string(&cfg_path).unwrap();
        let parsed: Config = serde_yml::from_str(&raw_after).unwrap();
        // Audits update landed.
        let defaults = parsed
            .audits
            .as_ref()
            .map(|a| a.defaults.clone())
            .unwrap_or_default();
        assert_eq!(defaults.get("drift_audit"), Some(&Cadence::Monthly));
        // Other top-level keys still parse to their prior values.
        let before_parsed: Config = serde_yml::from_str(&raw_before).unwrap();
        assert_eq!(parsed.github.token_env, before_parsed.github.token_env);
        assert_eq!(parsed.repositories.len(), before_parsed.repositories.len());
        assert_eq!(parsed.chatops.is_some(), before_parsed.chatops.is_some());
        assert_eq!(parsed.reviewer.is_some(), before_parsed.reviewer.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn apply_in_place_patch_preserves_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let cfg_path = write_fixture_config(&tmp);
        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let raw_before = std::fs::read_to_string(&cfg_path).unwrap();
        let cfg: Config = serde_yml::from_str(&raw_before).unwrap();
        apply_in_place_patch(&cfg_path, &cfg).unwrap();

        let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "patch must preserve the pre-existing file mode");
    }

    // --- 6.3 confirm_diff_and_apply -----------------------------------------

    #[tokio::test]
    async fn confirm_diff_and_apply_accept_writes_and_prints_headers() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = write_fixture_config(&tmp);
        let raw = std::fs::read_to_string(&cfg_path).unwrap();
        let mut new_cfg: Config = serde_yml::from_str(&raw).unwrap();
        // Mutate reviewer model so the diff is non-empty.
        if let Some(r) = new_cfg.reviewer.as_mut() {
            r.model = "grok-3".to_string();
        }

        let mut io = ScriptedIo::new(vec!["y"]);
        let applied = confirm_diff_and_apply(&cfg_path, &new_cfg, &mut io)
            .await
            .unwrap();
        assert!(applied);
        let out = io.output_str();
        assert!(out.contains("current"), "diff must print `current` header:\n{out}");
        assert!(out.contains("proposed"), "diff must print `proposed` header:\n{out}");
        assert!(out.contains("+"), "diff must show added line:\n{out}");
        assert!(out.contains("-"), "diff must show removed line:\n{out}");

        let parsed_after: Config =
            serde_yml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            parsed_after.reviewer.as_ref().unwrap().model,
            "grok-3"
        );
    }

    #[tokio::test]
    async fn confirm_diff_and_apply_decline_leaves_file_unchanged() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = write_fixture_config(&tmp);
        let raw_before = std::fs::read_to_string(&cfg_path).unwrap();

        let mut new_cfg: Config = serde_yml::from_str(&raw_before).unwrap();
        if let Some(r) = new_cfg.reviewer.as_mut() {
            r.model = "grok-3".to_string();
        }

        for answer in ["n", "", "q"] {
            let mut io = ScriptedIo::new(vec![answer]);
            let applied = confirm_diff_and_apply(&cfg_path, &new_cfg, &mut io)
                .await
                .unwrap();
            assert!(!applied, "answer `{answer}` should not apply");
            let raw_after = std::fs::read_to_string(&cfg_path).unwrap();
            assert_eq!(
                raw_after, raw_before,
                "decline must leave file unchanged (answer={answer})"
            );
        }
    }

    // --- 3.2 / 3.3 execute_reconfigure integration --------------------------

    fn dev_reconfigure_args(tmp: &TempDir, section: ReconfigureSection) -> InstallArgs {
        InstallArgs {
            mode: Some(InstallMode::Dev),
            config_dir: Some(tmp.path().to_path_buf()),
            reconfigure: Some(section),
            ..InstallArgs::default()
        }
    }

    #[tokio::test]
    async fn execute_reconfigure_audits_in_place_patch_and_restart_guidance() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = write_fixture_config(&tmp);
        let mut io = ScriptedIo::new(vec![
            "w", // architecture_brightline
            "m", // drift_audit
            "n", // missing_tests_audit
            "d", // security_bug_audit
            "",  // architecture_consultative
            "m", // documentation_audit
        ]);
        let actions = RecordingActions::new();
        let args = dev_reconfigure_args(&tmp, ReconfigureSection::Audits);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let parsed: Config =
            serde_yml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        let defaults = parsed
            .audits
            .as_ref()
            .map(|a| a.defaults.clone())
            .unwrap_or_default();
        assert_eq!(defaults.get("drift_audit"), Some(&Cadence::Monthly));
        assert_eq!(defaults.get("security_bug_audit"), Some(&Cadence::Daily));
    }

    #[tokio::test]
    async fn execute_reconfigure_reviewer_decline_leaves_file_unchanged() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = write_fixture_config(&tmp);
        let raw_before = std::fs::read_to_string(&cfg_path).unwrap();
        // Provider -> openai_compatible, model -> grok-3, env var bare-Enter,
        // then decline at the diff prompt.
        let mut io = ScriptedIo::new(vec!["3", "grok-3", "", "n"]);
        let actions = RecordingActions::new();
        let args = dev_reconfigure_args(&tmp, ReconfigureSection::Reviewer);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let raw_after = std::fs::read_to_string(&cfg_path).unwrap();
        assert_eq!(raw_before, raw_after, "decline must leave file unchanged");
        let out = io.output_str();
        assert!(out.contains("no changes made"), "expected `no changes made`:\n{out}");
    }

    #[tokio::test]
    async fn execute_reconfigure_chatops_accept_applies_patch() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = write_fixture_config(&tmp);
        // slack -> slack, channel C9999999999, accept the diff.
        let mut io = ScriptedIo::new(vec!["2", "C9999999999", "y"]);
        let actions = RecordingActions::new();
        let args = dev_reconfigure_args(&tmp, ReconfigureSection::Chatops);
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let parsed: Config =
            serde_yml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            parsed.chatops.as_ref().unwrap().default_channel_id,
            "C9999999999"
        );
    }

    #[tokio::test]
    async fn execute_reconfigure_no_existing_install_bails() {
        let tmp = TempDir::new().unwrap();
        // No config.yaml under the override dir.
        let mut io = ScriptedIo::new(vec![]);
        let actions = RecordingActions::new();
        let args = dev_reconfigure_args(&tmp, ReconfigureSection::Audits);
        let err = execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .expect_err("expected bail with no install detected");
        let msg = format!("{err}");
        assert!(
            msg.contains("no existing install detected"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("install.sh"),
            "error should hint at install.sh: {msg}"
        );
    }

    #[tokio::test]
    async fn execute_reconfigure_honors_probe_resolved_path_over_default() {
        // Server mode without a --config-dir override: the probe returns a
        // custom path that exists; the reconfigure flow must read AND write
        // there (not the /etc default).
        let tmp = TempDir::new().unwrap();
        let probe_cfg = tmp.path().join("probe-config.yaml");
        std::fs::write(&probe_cfg, fixture_install_yaml()).unwrap();
        let actions = RecordingActions::new().with_probe_response(
            "autocoder.service",
            loaded_probe(&probe_cfg),
        );
        let mut io = ScriptedIo::new(vec![
            "w", // architecture_brightline
            "m", // drift_audit
            "n", // missing_tests_audit
            "d", // security_bug_audit
            "",  // architecture_consultative
            "m", // documentation_audit
        ]);
        let args = InstallArgs {
            mode: Some(InstallMode::Server),
            reconfigure: Some(ReconfigureSection::Audits),
            ..InstallArgs::default()
        };
        execute_inner(args, &mut io, &actions, tmp.path().to_path_buf())
            .await
            .unwrap();
        let parsed: Config =
            serde_yml::from_str(&std::fs::read_to_string(&probe_cfg).unwrap()).unwrap();
        let defaults = parsed
            .audits
            .as_ref()
            .map(|a| a.defaults.clone())
            .unwrap_or_default();
        assert_eq!(defaults.get("drift_audit"), Some(&Cadence::Monthly));
    }

    // --- 7.3 clap mutual-exclusion and unknown-value rejection --------------

    #[test]
    fn reconfigure_rejects_repositories_value() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrapper {
            #[command(flatten)]
            inst: InstallArgs,
        }
        let err = Wrapper::try_parse_from(["test", "--reconfigure", "repositories"])
            .expect_err("clap must reject `repositories` value");
        let msg = format!("{err}");
        assert!(
            msg.contains("audits") && msg.contains("reviewer") && msg.contains("chatops"),
            "clap usage error should list valid values: {msg}"
        );
    }

    #[test]
    fn reconfigure_conflicts_with_non_interactive() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrapper {
            #[command(flatten)]
            inst: InstallArgs,
        }
        let err = Wrapper::try_parse_from([
            "test",
            "--reconfigure",
            "audits",
            "--non-interactive",
        ])
        .expect_err("clap must reject the combination");
        let msg = format!("{err}");
        assert!(
            msg.contains("non-interactive") || msg.contains("non_interactive"),
            "clap error should name --non-interactive: {msg}"
        );
    }

    #[tokio::test]
    async fn wizard_rag_decline_writes_no_block() {
        let mut answers = baseline_wizard_answers();
        answers.push(""); // audits gate bare-Enter → no
        answers.push("n"); // RAG gate
        let mut io = ScriptedIo::new(answers);
        let ans = run_wizard(&mut io, InstallMode::Dev, &WizardPrefill::default())
            .await
            .unwrap();
        assert!(ans.canonical_rag.is_none());
        let cfg = assemble_config(&ans).unwrap();
        assert!(cfg.canonical_rag.is_none());
    }

    #[tokio::test]
    async fn wizard_rag_disable_option_writes_no_block() {
        let mut answers = baseline_wizard_answers();
        answers.push(""); // audits gate bare-Enter → no
        answers.push("y"); // RAG gate: yes, configure
        // localhost ollama probe will fail in test env so the four-option
        // menu fires; choose option 4 (disable).
        answers.push("4");
        let mut io = ScriptedIo::new(answers);
        let ans = run_wizard(&mut io, InstallMode::Dev, &WizardPrefill::default())
            .await
            .unwrap();
        assert!(ans.canonical_rag.is_none());
    }

    /// a37: reviewer Ollama choice exercises the bare-base-URL + no-api-key
    /// branch. Mirrors the `wizard_rag_*` shape: feed the scripted answers,
    /// run the wizard, assert the resolved `WizardAnswers` carries the
    /// ollama provider AND the captured base URL, AND that
    /// `assemble_config` produces a `reviewer:` block with the matching
    /// provider, NO api_key_env, AND the bare base URL.
    #[tokio::test]
    async fn wizard_reviewer_ollama_collects_base_url_and_no_api_key() {
        let mut answers: Vec<&'static str> = vec![
            "git@github.com:acme/widgets.git",
            "main",
            "agent-q",
            "300",
            "GITHUB_TOKEN",
            "ghp_test",
            "1", // chatops: none
            "4", // reviewer: ollama (1-indexed: none=1, anthropic=2, openai_compatible=3, ollama=4)
            "qwen2.5-coder:32b", // reviewer model
            "http://10.42.11.10:11434", // reviewer Ollama base URL (overrides default)
            "", // audits LLM gate bare-Enter → no
            "n", // RAG gate
        ];
        let _ = &mut answers;
        let mut io = ScriptedIo::new(answers);
        let ans = run_wizard(&mut io, InstallMode::Dev, &WizardPrefill::default())
            .await
            .unwrap();
        assert_eq!(ans.reviewer_provider, ReviewerProviderArg::Ollama);
        assert_eq!(ans.reviewer_model.as_deref(), Some("qwen2.5-coder:32b"));
        assert!(
            ans.reviewer_api_key.is_none(),
            "ollama path must NOT collect an api_key"
        );
        assert_eq!(
            ans.reviewer_api_base_url.as_deref(),
            Some("http://10.42.11.10:11434")
        );

        let cfg = assemble_config(&ans).unwrap();
        let rv = cfg.reviewer.expect("reviewer block present");
        assert_eq!(rv.provider, ReviewerProvider::Ollama);
        assert_eq!(rv.model, "qwen2.5-coder:32b");
        assert_eq!(
            rv.api_base_url.as_deref(),
            Some("http://10.42.11.10:11434")
        );
        assert!(rv.api_key_env.is_none(), "no api_key_env for ollama");
        assert!(rv.api_key.is_none(), "no inline api_key for ollama");

        // secrets.env MUST NOT carry a reviewer key for the ollama path.
        let secrets = assemble_secrets_env(&ans);
        assert!(
            !secrets.contains("ANTHROPIC_API_KEY") && !secrets.contains("OPENAI_API_KEY"),
            "no reviewer key should leak into secrets.env: {secrets}"
        );
    }

    #[tokio::test]
    async fn wizard_rag_docker_option_writes_localhost_ollama() {
        let mut answers = baseline_wizard_answers();
        answers.push(""); // audits gate bare-Enter → no
        answers.push("y"); // RAG gate
        answers.push("1"); // docker option
        let mut io = ScriptedIo::new(answers);
        let ans = run_wizard(&mut io, InstallMode::Dev, &WizardPrefill::default())
            .await
            .unwrap();
        let rag = ans.canonical_rag.expect("docker option writes block");
        assert_eq!(rag.provider, RagProviderArg::Ollama);
        assert_eq!(rag.base_url, "http://localhost:11434");
        assert_eq!(rag.model, "nomic-embed-text");
    }

    #[test]
    fn non_interactive_rag_provider_without_base_url_fails() {
        let p = WizardPrefill {
            repo_url: Some("git@github.com:acme/x.git".into()),
            token_env_var: Some("GITHUB_TOKEN".into()),
            chatops_backend: Some(ChatOpsBackendArg::None),
            reviewer_provider: Some(ReviewerProviderArg::None),
            rag_provider: Some(RagProviderArg::Ollama),
            ..WizardPrefill::default()
        };
        let err = validate_non_interactive(&p).expect_err("missing base url fails");
        let msg = format!("{err}");
        assert!(msg.contains("rag-base-url"), "msg should name flag: {msg}");
    }

    #[test]
    fn non_interactive_rag_ollama_full_flags_pass() {
        let p = WizardPrefill {
            repo_url: Some("git@github.com:acme/x.git".into()),
            token_env_var: Some("GITHUB_TOKEN".into()),
            chatops_backend: Some(ChatOpsBackendArg::None),
            reviewer_provider: Some(ReviewerProviderArg::None),
            rag_provider: Some(RagProviderArg::Ollama),
            rag_base_url: Some("http://gpu-host:11434".into()),
            ..WizardPrefill::default()
        };
        validate_non_interactive(&p).expect("full flags should pass");
        let ans = prefill_to_answers(&p).unwrap();
        let rag = ans.canonical_rag.unwrap();
        assert_eq!(rag.provider, RagProviderArg::Ollama);
        assert_eq!(rag.base_url, "http://gpu-host:11434");
        assert_eq!(rag.model, "nomic-embed-text");
    }

    #[test]
    fn non_interactive_rag_openai_requires_api_key_env() {
        let p = WizardPrefill {
            repo_url: Some("git@github.com:acme/x.git".into()),
            token_env_var: Some("GITHUB_TOKEN".into()),
            chatops_backend: Some(ChatOpsBackendArg::None),
            reviewer_provider: Some(ReviewerProviderArg::None),
            rag_provider: Some(RagProviderArg::OpenaiCompatible),
            rag_base_url: Some("https://api.voyageai.com/v1".into()),
            ..WizardPrefill::default()
        };
        let err = validate_non_interactive(&p)
            .expect_err("missing api-key-env should fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("rag-api-key-env"),
            "msg should name flag: {msg}"
        );
    }

    #[test]
    fn reconfigure_conflicts_with_prefill_flag() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrapper {
            #[command(flatten)]
            inst: InstallArgs,
        }
        let err = Wrapper::try_parse_from([
            "test",
            "--reconfigure",
            "reviewer",
            "--repo-url",
            "git@github.com:acme/x.git",
        ])
        .expect_err("clap must reject reconfigure + prefill");
        let msg = format!("{err}");
        assert!(
            msg.contains("repo-url") || msg.contains("repo_url"),
            "clap error should name --repo-url: {msg}"
        );
    }
}
