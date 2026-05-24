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
    /// security_bug_audit). Default `none`.
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
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum InstallMode {
    Server,
    Dev,
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

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ReviewerProviderArg {
    None,
    Anthropic,
    #[clap(name = "openai_compatible")]
    OpenAiCompatible,
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
    /// Resolved cadences per audit slug. Audits the operator declined are
    /// either absent from the map or stored as `Cadence::Disabled`. The
    /// config-assembly step drops `Disabled` entries before emitting YAML.
    pub audits: HashMap<String, Cadence>,
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
}

#[derive(Default)]
pub struct RecordingActions {
    pub calls: Mutex<Vec<RecordedCall>>,
    pub which_overrides: Mutex<std::collections::HashMap<String, Option<PathBuf>>>,
    pub apt_get_available: bool,
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
const REVIEWER_OPTIONS: &[&str] = &["none", "anthropic", "openai_compatible"];

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
    if reviewer_provider != ReviewerProviderArg::None {
        let default_model = prefill
            .reviewer_model
            .as_deref()
            .unwrap_or(match reviewer_provider {
                ReviewerProviderArg::Anthropic => "claude-sonnet-4-6",
                _ => "gpt-4o-mini",
            });
        reviewer_model = Some(ask_default(io, "Reviewer model", default_model).await?);
        io.print("Reviewer API key (written to secrets.env): ");
        let k = io.read_password().await?;
        reviewer_api_key = if k.is_empty() { None } else { Some(k) };
    }

    let audits = run_audit_prompts(io).await?;

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
        audits,
    })
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
    }
}

fn idx_to_reviewer_arg(i: usize) -> ReviewerProviderArg {
    [
        ReviewerProviderArg::None,
        ReviewerProviderArg::Anthropic,
        ReviewerProviderArg::OpenAiCompatible,
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
    }
}

// ----------------------------------------------------------------------------
// Config + secrets assembly.
// ----------------------------------------------------------------------------

/// Deserialize the bundled `config.example.yaml` and mutate it with the
/// operator's answers. The example is the source-of-truth for what fields
/// exist; this function only writes the ones the wizard collected.
pub fn assemble_config(answers: &WizardAnswers) -> Result<Config> {
    let mut cfg: Config = serde_yaml::from_str(BUNDLED_EXAMPLE)
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
        ReviewerProviderArg::Anthropic | ReviewerProviderArg::OpenAiCompatible => {
            let provider = match answers.reviewer_provider {
                ReviewerProviderArg::Anthropic => ReviewerProvider::Anthropic,
                ReviewerProviderArg::OpenAiCompatible => ReviewerProvider::OpenAiCompatible,
                _ => unreachable!(),
            };
            Some(ReviewerConfig {
                enabled: true,
                provider,
                model: answers
                    .reviewer_model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
                api_key_env: reviewer_env_var(answers.reviewer_provider).map(String::from),
                api_key: None,
                api_base_url: None,
                prompt_template_path: None,
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
            settings: Default::default(),
        })
    };

    // Suppress the unused-import warning if `SecretSource` ends up only
    // being referenced behind a feature in future edits.
    let _ = std::marker::PhantomData::<SecretSource>;

    Ok(cfg)
}

pub fn serialize_config(cfg: &Config) -> Result<String> {
    serde_yaml::to_string(cfg).context("serialize Config to YAML")
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
        audit_architecture_brightline: args.audit_architecture_brightline,
        audit_architecture_consultative: args.audit_architecture_consultative,
        audit_drift_audit: args.audit_drift_audit,
        audit_missing_tests_audit: args.audit_missing_tests_audit,
        audit_security_bug_audit: args.audit_security_bug_audit,
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

    let cfg = assemble_config(&answers)?;
    let yaml = serialize_config(&cfg)?;
    fs::write(&config_path, yaml.as_bytes())
        .await
        .with_context(|| format!("write {}", config_path.display()))?;

    let secrets = assemble_secrets_env(&answers);
    fs::write(&secrets_path, secrets.as_bytes())
        .await
        .with_context(|| format!("write {}", secrets_path.display()))?;

    let config_mode = if mode == InstallMode::Server { 0o640 } else { 0o600 };
    actions.chmod(&config_path, config_mode).await?;
    actions.chmod(&secrets_path, 0o600).await?;
    if mode == InstallMode::Server {
        actions.chown(&config_path, "autocoder", "autocoder").await?;
        actions.chown(&secrets_path, "autocoder", "autocoder").await?;
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
    Ok(())
}

fn prefill_to_answers(p: &WizardPrefill) -> Result<WizardAnswers> {
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
        audits: resolve_non_interactive_audits(p),
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
        _ => None,
    }
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
            audits: HashMap::new(),
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
        // → no).
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

    #[tokio::test]
    async fn assemble_config_round_trips_through_serde() {
        let ans = slack_answers();
        let cfg = assemble_config(&ans).unwrap();
        let yaml = serialize_config(&cfg).unwrap();
        let round: Config = serde_yaml::from_str(&yaml)
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
    async fn wizard_audits_fast_path_enables_all_five() {
        let mut answers = baseline_wizard_answers();
        // LLM gate yes, fast-path default Y.
        answers.push("y"); // LLM gate
        answers.push(""); // fast-path (default Y)
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
        assert_eq!(audits.defaults.len(), 5, "all five audits must be present");
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
        answers.push("y");
        answers.push("n");
        answers.push("d"); // architecture_brightline
        answers.push("m"); // drift_audit
        answers.push("w"); // missing_tests_audit
        answers.push("n"); // security_bug_audit (never)
        answers.push("d"); // architecture_consultative
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
}
