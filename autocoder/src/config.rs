use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A secret value sourced from EITHER an environment variable name (bare
/// YAML string) OR an inline value (`{ value: "..." }` object). Used for
/// any config field that carries a credential.
///
/// Parsing relies on `#[serde(untagged)]`: a YAML string deserializes to
/// `EnvVar(name)`; a YAML mapping with a `value` key deserializes to
/// `Inline { value }`. Any other shape produces a deserialize error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SecretSource {
    /// Bare string: names an environment variable holding the secret.
    EnvVar(String),
    /// `{ value: "..." }`: the secret value itself, verbatim.
    Inline { value: String },
}

impl SecretSource {
    /// Read the secret. For `EnvVar`, reads the named env var and errors if
    /// unset, naming both the env var and the originating config field. For
    /// `Inline`, returns the value verbatim.
    pub fn resolve(&self, field_label: &str) -> Result<String> {
        match self {
            Self::EnvVar(name) => std::env::var(name).map_err(|_| {
                anyhow!("secret env var `{name}` for `{field_label}` is not set")
            }),
            Self::Inline { value } => Ok(value.clone()),
        }
    }

    /// Source description for startup logs. NEVER returns the secret value.
    pub fn describe(&self, field_label: &str) -> String {
        match self {
            Self::EnvVar(name) => format!("env var {name}"),
            Self::Inline { .. } => format!("inline ({field_label})"),
        }
    }

    /// True when this source is an inline value (used to detect "both forms
    /// set" precedence warnings at startup).
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repositories: Vec<RepositoryConfig>,
    pub executor: ExecutorConfig,
    pub github: GithubConfig,
    #[serde(default)]
    pub reviewer: Option<ReviewerConfig>,
    #[serde(default)]
    pub chatops: Option<ChatOpsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub url: String,
    #[serde(default)]
    pub local_path: Option<PathBuf>,
    pub base_branch: String,
    pub agent_branch: String,
    pub poll_interval_sec: u64,
    #[serde(default)]
    pub chatops_channel_id: Option<String>,
    /// Per-repo upper bound on the number of archived changes committed
    /// in one iteration's PR. When unset, falls back to
    /// `executor.max_changes_per_pr` and finally to a global default of
    /// `3`. A configured value of `0` is a misconfiguration and is
    /// clamped to `1` with a WARN log at startup. See
    /// `Config::resolved_max_changes_per_pr` for the resolved value.
    #[serde(default)]
    pub max_changes_per_pr: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    pub kind: ExecutorKind,
    #[serde(default = "default_executor_command")]
    pub command: String,
    #[serde(default = "default_executor_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub sandbox: Option<ExecutorSandboxConfig>,
    /// Optional path to a custom implementer prompt template. When unset,
    /// the binary uses the template embedded at compile time from
    /// `prompts/implementer.md`. The file must contain the literal
    /// `{{change_body}}` placeholder which is replaced with the output of
    /// `openspec instructions apply` for each change.
    #[serde(default)]
    pub implementer_prompt_path: Option<PathBuf>,
    /// Number of consecutive Failed outcomes for a single change before
    /// autocoder marks it perma-stuck (writes `.perma-stuck.json` in the
    /// change directory, posts a chatops alert, and excludes the change
    /// from `list_pending` until the marker is removed manually). When
    /// unset, defaults to 2. A configured value of 0 is a misconfiguration
    /// and is clamped to 1 with a WARN log at startup.
    #[serde(default)]
    pub perma_stuck_after_failures: Option<u32>,
    /// Global default for the per-iteration commit cap. Per-repository
    /// `RepositoryConfig::max_changes_per_pr` takes precedence. When both
    /// are unset, the global default of `3` applies. A configured value
    /// of `0` is clamped to `1` with a WARN log at startup.
    #[serde(default)]
    pub max_changes_per_pr: Option<u32>,
    /// Upper bound (in seconds) on the random sleep each polling task
    /// performs before its first iteration. Each task independently draws
    /// a value uniformly from `[0, startup_jitter_max_secs]` at spawn
    /// time. Staggers a fleet of concurrent `git fetch` operations so an
    /// IDS does not see a synchronized burst. `0` disables the startup
    /// jitter entirely. When unset, the effective default is `30`.
    #[serde(default)]
    pub startup_jitter_max_secs: Option<u64>,
    /// Percent (0..=100) of `poll_interval_sec` used as a uniform random
    /// offset on every inter-iteration sleep. Each task's sleep is drawn
    /// from `[interval - interval*pct/100, interval + interval*pct/100]`.
    /// Prevents long-term re-synchronization of multiple tasks. `0`
    /// produces exact intervals. When unset, the effective default is
    /// `10`. Values above 100 are clamped to 100 (the negative offset
    /// could otherwise exceed the interval and would saturate at zero).
    #[serde(default)]
    pub inter_iteration_jitter_pct: Option<u8>,
}

impl ExecutorConfig {
    /// Effective perma-stuck threshold. `None` → 2 (the default). Any
    /// configured value is clamped to `>=1` so the agent always gets at
    /// least one attempt. Callers that want the raw configured value
    /// (e.g. to warn about a zero) read `perma_stuck_after_failures`
    /// directly.
    pub fn perma_stuck_threshold(&self) -> u32 {
        self.perma_stuck_after_failures.unwrap_or(2).max(1)
    }

    /// Effective startup jitter ceiling (seconds). Unset → `30`.
    pub fn startup_jitter_max_secs(&self) -> u64 {
        self.startup_jitter_max_secs.unwrap_or(30)
    }

    /// Effective inter-iteration jitter percentage. Unset → `10`. Clamped
    /// to `100` so a negative offset cannot exceed the base interval (the
    /// arithmetic would otherwise saturate at zero and waste resolution).
    pub fn inter_iteration_jitter_pct(&self) -> u8 {
        self.inter_iteration_jitter_pct.unwrap_or(10).min(100)
    }
}

/// Per-iteration tool-use restrictions for the wrapped agent CLI. When
/// absent, restrictive safe defaults apply (see `default_allowed_tools`,
/// `default_disallowed_bash_patterns`, `default_disallowed_read_paths`).
/// Each field can be overridden independently; omitted fields keep their
/// safe defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorSandboxConfig {
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_bash_patterns: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_read_paths: Option<Vec<String>>,
}

/// The fully-resolved sandbox after per-field defaulting. Used by the
/// executor at spawn time.
#[derive(Debug, Clone)]
pub struct ResolvedSandbox {
    pub allowed_tools: Vec<String>,
    pub disallowed_bash_patterns: Vec<String>,
    pub disallowed_read_paths: Vec<String>,
}

impl ResolvedSandbox {
    /// Resolve a configured sandbox (or absence) into the values that will
    /// be passed to the wrapped CLI. Each field falls back to its safe
    /// default when unset in the operator's config.
    pub fn resolve(cfg: Option<&ExecutorSandboxConfig>) -> Self {
        let allowed_tools = cfg
            .and_then(|c| c.allowed_tools.clone())
            .unwrap_or_else(default_allowed_tools);
        let disallowed_bash_patterns = cfg
            .and_then(|c| c.disallowed_bash_patterns.clone())
            .unwrap_or_else(default_disallowed_bash_patterns);
        let disallowed_read_paths = cfg
            .and_then(|c| c.disallowed_read_paths.clone())
            .unwrap_or_else(default_disallowed_read_paths);
        Self {
            allowed_tools,
            disallowed_bash_patterns,
            disallowed_read_paths,
        }
    }
}

pub fn default_allowed_tools() -> Vec<String> {
    ["Read", "Write", "Edit", "Glob", "Grep", "Bash"]
        .into_iter()
        .map(String::from)
        .collect()
}

pub fn default_disallowed_bash_patterns() -> Vec<String> {
    [
        "curl:*",
        "wget:*",
        "nc:*",
        "ncat:*",
        "netcat:*",
        "ssh:*",
        "scp:*",
        "sftp:*",
        "rsync:*",
        "git push:*",
        "git remote *",
        "git fetch *://*",
        // Defense in depth against the "lazy archive" failure mode. The
        // structural check in polling_loop::detect_lazy_archive is the
        // real protection (catches bare `git mv` archive renames too).
        "openspec archive:*",
        "openspec unarchive:*",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub fn default_disallowed_read_paths() -> Vec<String> {
    [
        "/home/*/.ssh/**",
        "/home/*/.claude/**",
        "/etc/shadow",
        "/etc/ssl/private/**",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    ClaudeCli,
}

fn default_executor_command() -> String {
    "claude".to_string()
}

fn default_executor_timeout() -> u64 {
    1800
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    #[serde(default = "default_github_token_env")]
    pub token_env: String,
    #[serde(default)]
    pub token: Option<SecretSource>,
    #[serde(default)]
    pub owner_tokens: Option<HashMap<String, SecretSource>>,
    /// When set, autocoder operates in fork-PR mode: the agent branch is
    /// pushed to `git@github.com:<fork_owner>/<repo>.git` (a fork owned
    /// by this handle), and PRs are opened as cross-repository PRs with
    /// `head` formatted as `<fork_owner>:<agent_branch>`. The fork must
    /// be pre-created; autocoder verifies its existence at startup.
    #[serde(default)]
    pub fork_owner: Option<String>,
}

fn default_github_token_env() -> String {
    "GITHUB_TOKEN".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerConfig {
    #[serde(default)]
    pub enabled: bool,
    pub provider: ReviewerProvider,
    pub model: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<SecretSource>,
    #[serde(default)]
    pub api_base_url: Option<String>,
    #[serde(default)]
    pub prompt_template_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerProvider {
    Anthropic,
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatOpsProvider {
    Slack,
    Discord,
    Teams,
    Mattermost,
    Matrix,
}

impl ChatOpsProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Teams => "teams",
            Self::Mattermost => "mattermost",
            Self::Matrix => "matrix",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatOpsConfig {
    pub provider: ChatOpsProvider,
    pub default_channel_id: String,
    #[serde(default)]
    pub notifications: Option<NotificationsConfig>,
    #[serde(default)]
    pub slack: Option<SlackProviderConfig>,
    #[serde(default)]
    pub discord: Option<DiscordProviderConfig>,
    #[serde(default)]
    pub teams: Option<TeamsProviderConfig>,
    #[serde(default)]
    pub mattermost: Option<MattermostProviderConfig>,
    #[serde(default)]
    pub matrix: Option<MatrixProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackProviderConfig {
    #[serde(default)]
    pub bot_token_env: Option<String>,
    #[serde(default)]
    pub bot_token: Option<SecretSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordProviderConfig {
    pub bot_token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamsProviderConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret_env: String,
    pub team_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MattermostProviderConfig {
    pub server_url: String,
    pub access_token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixProviderConfig {
    pub homeserver_url: String,
    pub access_token_env: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    #[serde(default = "default_true")]
    pub start_work: bool,
    #[serde(default = "default_true")]
    pub failure_alerts: bool,
    #[serde(default = "default_true")]
    pub pr_opened: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            start_work: true,
            failure_alerts: true,
            pr_opened: true,
        }
    }
}

impl NotificationsConfig {
    /// Resolve the effective `start_work` flag given the (optional) ChatOps
    /// config: defaults to `true` when no `notifications:` block was set, and
    /// honors the explicit value otherwise.
    pub fn start_work_enabled(chatops: Option<&ChatOpsConfig>) -> bool {
        chatops
            .and_then(|s| s.notifications.as_ref())
            .map(|n| n.start_work)
            .unwrap_or(true)
    }

    /// Resolve the effective `failure_alerts` flag given the (optional) ChatOps
    /// config: defaults to `true` when no `notifications:` block was set, and
    /// honors the explicit value otherwise.
    pub fn failure_alerts_enabled(chatops: Option<&ChatOpsConfig>) -> bool {
        chatops
            .and_then(|s| s.notifications.as_ref())
            .map(|n| n.failure_alerts)
            .unwrap_or(true)
    }

    /// Resolve the effective `pr_opened` flag given the (optional) ChatOps
    /// config: defaults to `true` when no `notifications:` block was set,
    /// and honors the explicit value otherwise.
    pub fn pr_opened_enabled(chatops: Option<&ChatOpsConfig>) -> bool {
        chatops
            .and_then(|s| s.notifications.as_ref())
            .map(|n| n.pr_opened)
            .unwrap_or(true)
    }
}

impl RepositoryConfig {
    /// Resolve the ChatOps channel to use for this repo: explicit per-repo
    /// `chatops_channel_id` if set, otherwise the global default.
    pub fn chatops_channel<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.chatops_channel_id.as_deref().unwrap_or(fallback)
    }

    /// Resolve the effective `max_changes_per_pr` for this repository.
    /// Lookup order: per-repo override → executor-level default → hardcoded
    /// `3`. Any configured value is clamped to `>= 1`. Callers that want
    /// to warn about a configured `0` read the raw fields directly.
    pub fn max_changes_per_pr(&self, executor: &ExecutorConfig) -> u32 {
        const DEFAULT: u32 = 3;
        let chosen = self
            .max_changes_per_pr
            .or(executor.max_changes_per_pr)
            .unwrap_or(DEFAULT);
        chosen.max(1)
    }
}

impl Config {
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(yaml: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        (dir, path)
    }

    const VALID_TWO_REPO_YAML: &str = r#"
repositories:
  - url: "git@github.com:owner/repo-a.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
  - url: "git@github.com:owner/repo-b.git"
    local_path: /tmp/workspaces/repo-b
    base_branch: dev
    agent_branch: agent-q
    poll_interval_sec: 1800
executor:
  kind: claude_cli
  command: claude
  timeout_secs: 1800
github:
  token_env: GITHUB_TOKEN
"#;

    /// Parses the actual `config.example.yaml` file shipped at the repo
    /// root. This guards against the example drifting out of sync with the
    /// parser — operators who `cp config.example.yaml config.yaml` should
    /// always end up with a parseable file.
    #[test]
    fn config_example_yaml_parses() {
        let example_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("manifest dir has a parent")
            .join("config.example.yaml");
        assert!(
            example_path.exists(),
            "config.example.yaml must exist at {}",
            example_path.display()
        );
        let cfg = Config::load_from(&example_path)
            .expect("config.example.yaml must be parseable as Config");
        // Single-repo by default per the design.
        assert_eq!(cfg.repositories.len(), 1);
        assert_eq!(cfg.repositories[0].base_branch, "main");
        assert_eq!(cfg.repositories[0].agent_branch, "agent-q");
        // Reviewer and ChatOps blocks are commented out by default.
        assert!(cfg.reviewer.is_none(), "reviewer must be off by default");
        assert!(cfg.chatops.is_none(), "chatops must be off by default");
    }

    #[test]
    fn loads_example() {
        let (_dir, path) = write_config(VALID_TWO_REPO_YAML);
        let cfg = Config::load_from(&path).expect("config should parse");
        assert_eq!(cfg.repositories.len(), 2);
        assert_eq!(cfg.repositories[0].url, "git@github.com:owner/repo-a.git");
        assert_eq!(cfg.repositories[0].poll_interval_sec, 300);
        assert!(cfg.repositories[0].local_path.is_none());
        assert_eq!(
            cfg.repositories[1].local_path.as_deref(),
            Some(Path::new("/tmp/workspaces/repo-b"))
        );
        assert_eq!(cfg.executor.kind, ExecutorKind::ClaudeCli);
        assert_eq!(cfg.executor.command, "claude");
        assert_eq!(cfg.executor.timeout_secs, 1800);
        assert_eq!(cfg.github.token_env, "GITHUB_TOKEN");
    }

    #[test]
    fn applies_defaults_for_executor_and_github() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        assert_eq!(cfg.executor.command, "claude");
        assert_eq!(cfg.executor.timeout_secs, 1800);
        assert_eq!(cfg.github.token_env, "GITHUB_TOKEN");
    }

    #[test]
    fn rejects_unknown_field() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    typo_field: oops
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("should reject unknown field");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("typo_field") || msg.to_lowercase().contains("unknown"),
            "error should mention unknown field; got: {msg}"
        );
    }

    #[test]
    fn missing_config_path_errors_with_path_in_message() {
        // 13.1.2 attestation: orchestrator-cli baseline says missing config
        // "exits with a non-zero status code AND stderr contains a single
        // error line naming the offending file path". Config::load_from is
        // the only step in the dispatch chain that reads the file; if it
        // returns an Err whose message names the path, anyhow's `main`
        // formatting will print that to stderr and the process will exit
        // non-zero (a Result::Err from `main`).
        let path = Path::new("/nonexistent/orchestrator-test-config.yaml");
        let err = Config::load_from(path).expect_err("missing path must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("/nonexistent/orchestrator-test-config.yaml"),
            "error must name the offending path; got: {msg}"
        );
    }

    #[test]
    fn loads_with_reviewer() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  api_base_url: https://api.anthropic.com
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with reviewer should parse");
        let rv = cfg.reviewer.expect("reviewer block should be present");
        assert!(rv.enabled);
        assert_eq!(rv.provider, ReviewerProvider::Anthropic);
        assert_eq!(rv.model, "claude-sonnet-4-6");
        assert_eq!(rv.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(rv.api_base_url.as_deref(), Some("https://api.anthropic.com"));
        assert!(rv.prompt_template_path.is_none());
    }

    #[test]
    fn reviewer_disabled_by_default() {
        // Absent block parses to None — opt-in semantics.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.reviewer.is_none());
    }

    #[test]
    fn reviewer_openai_compatible_provider() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  provider: openai_compatible
  model: gpt-4o
  api_key_env: OPENAI_API_KEY
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let rv = cfg.reviewer.unwrap();
        assert_eq!(rv.provider, ReviewerProvider::OpenAiCompatible);
        assert!(!rv.enabled); // default false when omitted
    }

    #[test]
    fn loads_with_chatops_slack() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    chatops_channel_id: C01234OVERRIDE
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Slack);
        assert_eq!(co.default_channel_id, "C0DEFAULT");
        let slack = co.slack.expect("slack sub-block present");
        assert_eq!(slack.bot_token_env.as_deref(), Some("SLACK_BOT_TOKEN"));
        assert_eq!(
            cfg.repositories[0].chatops_channel_id.as_deref(),
            Some("C01234OVERRIDE")
        );
    }

    #[test]
    fn loads_with_chatops_discord() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: discord
  default_channel_id: "123456789012345678"
  discord:
    bot_token_env: DISCORD_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Discord);
        let d = co.discord.expect("discord sub-block");
        assert_eq!(d.bot_token_env, "DISCORD_BOT_TOKEN");
    }

    #[test]
    fn loads_with_chatops_teams() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: teams
  default_channel_id: "19:abc@thread.tacv2"
  teams:
    tenant_id: "11111111-2222-3333-4444-555555555555"
    client_id: "66666666-7777-8888-9999-aaaaaaaaaaaa"
    client_secret_env: TEAMS_CLIENT_SECRET
    team_id: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Teams);
        let t = co.teams.expect("teams sub-block");
        assert_eq!(t.tenant_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(t.client_id, "66666666-7777-8888-9999-aaaaaaaaaaaa");
        assert_eq!(t.client_secret_env, "TEAMS_CLIENT_SECRET");
        assert_eq!(t.team_id, "bbbbbbbb-cccc-dddd-eeee-ffffffffffff");
    }

    #[test]
    fn loads_with_chatops_mattermost() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: mattermost
  default_channel_id: c1abcd
  mattermost:
    server_url: "https://mattermost.example.com"
    access_token_env: MATTERMOST_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Mattermost);
        let m = co.mattermost.expect("mattermost sub-block");
        assert_eq!(m.server_url, "https://mattermost.example.com");
        assert_eq!(m.access_token_env, "MATTERMOST_TOKEN");
    }

    #[test]
    fn loads_with_chatops_matrix() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: matrix
  default_channel_id: "!abc:server.tld"
  matrix:
    homeserver_url: "https://matrix.example.com"
    access_token_env: MATRIX_ACCESS_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops block present");
        assert_eq!(co.provider, ChatOpsProvider::Matrix);
        let m = co.matrix.expect("matrix sub-block");
        assert_eq!(m.homeserver_url, "https://matrix.example.com");
        assert_eq!(m.access_token_env, "MATRIX_ACCESS_TOKEN");
    }

    #[test]
    fn rejects_unknown_chatops_provider() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: irc
  default_channel_id: general-channel
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("unknown chatops.provider must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("irc") || msg.to_lowercase().contains("variant"),
            "error should reject unknown variant; got: {msg}"
        );
    }

    #[test]
    fn repo_overrides_channel() {
        let repo_with_override = RepositoryConfig {
            url: "x".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: Some("C_REPO_LEVEL".into()),
            max_changes_per_pr: None,
        };
        assert_eq!(repo_with_override.chatops_channel("C_DEFAULT"), "C_REPO_LEVEL");

        let repo_default = RepositoryConfig {
            url: "x".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
        };
        assert_eq!(repo_default.chatops_channel("C_DEFAULT"), "C_DEFAULT");
    }

    #[test]
    fn chatops_block_absent_parses_to_none() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.chatops.is_none());
    }

    #[test]
    fn sandbox_absent_uses_defaults() {
        let resolved = ResolvedSandbox::resolve(None);
        assert_eq!(resolved.allowed_tools, default_allowed_tools());
        assert_eq!(
            resolved.disallowed_bash_patterns,
            default_disallowed_bash_patterns()
        );
        assert_eq!(
            resolved.disallowed_read_paths,
            default_disallowed_read_paths()
        );
        // Defense-in-depth: WebFetch and WebSearch are NOT in the defaults.
        assert!(!resolved.allowed_tools.iter().any(|t| t == "WebFetch"));
        assert!(!resolved.allowed_tools.iter().any(|t| t == "WebSearch"));
        // Spot-check that curl is denied.
        assert!(
            resolved
                .disallowed_bash_patterns
                .iter()
                .any(|p| p.starts_with("curl"))
        );
    }

    #[test]
    fn sandbox_default_blocks_openspec_archive() {
        let patterns = default_disallowed_bash_patterns();
        assert!(
            patterns.contains(&"openspec archive:*".to_string()),
            "default sandbox must deny `openspec archive`"
        );
        assert!(
            patterns.contains(&"openspec unarchive:*".to_string()),
            "default sandbox must deny `openspec unarchive`"
        );
    }

    #[test]
    fn sandbox_partial_override_uses_defaults_per_field() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  sandbox:
    allowed_tools: [Read, Write]
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("partial sandbox should parse");
        let resolved = ResolvedSandbox::resolve(cfg.executor.sandbox.as_ref());
        // Operator's allowed_tools wins.
        assert_eq!(
            resolved.allowed_tools,
            vec!["Read".to_string(), "Write".to_string()]
        );
        // Other fields fall back to safe defaults.
        assert_eq!(
            resolved.disallowed_bash_patterns,
            default_disallowed_bash_patterns()
        );
        assert_eq!(
            resolved.disallowed_read_paths,
            default_disallowed_read_paths()
        );
    }

    #[test]
    fn sandbox_full_override_uses_operator_values_only() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  sandbox:
    allowed_tools: [Read]
    disallowed_bash_patterns: ["custom-pat:*"]
    disallowed_read_paths: ["/custom/path/**"]
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("full sandbox should parse");
        let resolved = ResolvedSandbox::resolve(cfg.executor.sandbox.as_ref());
        assert_eq!(resolved.allowed_tools, vec!["Read".to_string()]);
        assert_eq!(
            resolved.disallowed_bash_patterns,
            vec!["custom-pat:*".to_string()]
        );
        assert_eq!(
            resolved.disallowed_read_paths,
            vec!["/custom/path/**".to_string()]
        );
    }

    #[test]
    fn loads_fork_owner() {
        let yaml = r#"
repositories:
  - url: "git@github.com:upstream/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  fork_owner: machine-user-handle
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with fork_owner should parse");
        assert_eq!(cfg.github.fork_owner.as_deref(), Some("machine-user-handle"));
    }

    #[test]
    fn fork_owner_absent_defaults_to_none() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.github.fork_owner.is_none());
    }

    #[test]
    fn loads_with_owner_tokens() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
  owner_tokens:
    rabbeverly: PERSONAL_GH_TOKEN
    my-org-a: ORG_A_GH_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with owner_tokens should parse");
        let map = cfg
            .github
            .owner_tokens
            .expect("owner_tokens block should be present");
        match map.get("rabbeverly").unwrap() {
            SecretSource::EnvVar(name) => assert_eq!(name, "PERSONAL_GH_TOKEN"),
            _ => panic!("expected env-var source for rabbeverly"),
        }
        match map.get("my-org-a").unwrap() {
            SecretSource::EnvVar(name) => assert_eq!(name, "ORG_A_GH_TOKEN"),
            _ => panic!("expected env-var source for my-org-a"),
        }
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn owner_tokens_optional() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token_env: GITHUB_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config without owner_tokens should parse");
        assert!(cfg.github.owner_tokens.is_none());
    }

    #[test]
    fn secret_source_parses_bare_string_as_env_var() {
        let s: SecretSource = serde_yaml::from_str("MY_VAR").unwrap();
        match s {
            SecretSource::EnvVar(name) => assert_eq!(name, "MY_VAR"),
            _ => panic!("bare string must parse as EnvVar"),
        }
    }

    #[test]
    fn secret_source_parses_object_as_inline() {
        let s: SecretSource = serde_yaml::from_str("value: \"abc123\"").unwrap();
        match s {
            SecretSource::Inline { value } => assert_eq!(value, "abc123"),
            _ => panic!("`{{value: ...}}` must parse as Inline"),
        }
    }

    #[test]
    fn secret_source_resolve_env_var_set() {
        // SAFETY: unique env var name per test, no parallel mutator.
        unsafe { std::env::set_var("AUTOCODER_TEST_SECRET_RESOLVE_SET", "x") };
        let s = SecretSource::EnvVar("AUTOCODER_TEST_SECRET_RESOLVE_SET".into());
        assert_eq!(s.resolve("test.field").unwrap(), "x");
        unsafe { std::env::remove_var("AUTOCODER_TEST_SECRET_RESOLVE_SET") };
    }

    #[test]
    fn secret_source_resolve_env_var_unset_names_field() {
        unsafe { std::env::remove_var("AUTOCODER_TEST_SECRET_RESOLVE_UNSET") };
        let s = SecretSource::EnvVar("AUTOCODER_TEST_SECRET_RESOLVE_UNSET".into());
        let err = s.resolve("my.field.label").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("AUTOCODER_TEST_SECRET_RESOLVE_UNSET"),
            "error must name env var; got: {msg}"
        );
        assert!(
            msg.contains("my.field.label"),
            "error must name field label; got: {msg}"
        );
    }

    #[test]
    fn secret_source_resolve_inline() {
        let s = SecretSource::Inline {
            value: "verbatim".into(),
        };
        assert_eq!(s.resolve("any.label").unwrap(), "verbatim");
    }

    #[test]
    fn secret_source_describe_redacts_inline_value() {
        let inline = SecretSource::Inline {
            value: "super-secret-token-xyz".into(),
        };
        let desc = inline.describe("github.token");
        assert!(
            !desc.contains("super-secret-token-xyz"),
            "describe must NEVER expose the inline value; got: {desc}"
        );
        assert_eq!(desc, "inline (github.token)");

        let env = SecretSource::EnvVar("MY_VAR".into());
        assert_eq!(env.describe("anything"), "env var MY_VAR");
    }

    #[test]
    fn loads_github_token_inline() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  token:
    value: "ghp_inlinepat"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config with inline github.token should parse");
        match cfg.github.token.unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "ghp_inlinepat"),
            _ => panic!("expected inline source"),
        }
        // token_env default still present:
        assert_eq!(cfg.github.token_env, "GITHUB_TOKEN");
    }

    #[test]
    fn loads_owner_tokens_mixed_env_and_inline() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github:
  owner_tokens:
    org-with-env-var: ORG_ENV_VAR
    org-with-inline:
      value: "ghp_inlinevalue"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("mixed owner_tokens should parse");
        let map = cfg.github.owner_tokens.expect("present");
        match map.get("org-with-env-var").unwrap() {
            SecretSource::EnvVar(n) => assert_eq!(n, "ORG_ENV_VAR"),
            _ => panic!("env-var entry mis-parsed"),
        }
        match map.get("org-with-inline").unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "ghp_inlinevalue"),
            _ => panic!("inline entry mis-parsed"),
        }
    }

    #[test]
    fn loads_slack_inline_bot_token() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
    bot_token:
      value: "xoxb-inline"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("inline slack bot_token should parse");
        let co = cfg.chatops.unwrap();
        let slack = co.slack.unwrap();
        match slack.bot_token.unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "xoxb-inline"),
            _ => panic!("expected inline slack bot token"),
        }
        assert_eq!(slack.bot_token_env.as_deref(), Some("SLACK_BOT_TOKEN"));
    }

    #[test]
    fn loads_reviewer_inline_api_key_without_env_name() {
        // The point of the fix: with `api_key` inline set, `api_key_env`
        // is not required in YAML.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key:
    value: "sk-ant-inline-only"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path)
            .expect("reviewer with inline api_key and no api_key_env should parse");
        let rv = cfg.reviewer.unwrap();
        assert!(rv.api_key_env.is_none());
        assert!(rv.api_key.is_some());
    }

    #[test]
    fn loads_slack_inline_bot_token_without_env_name() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token:
      value: "xoxb-inline-only"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path)
            .expect("slack with inline bot_token and no bot_token_env should parse");
        let co = cfg.chatops.unwrap();
        let slack = co.slack.unwrap();
        assert!(slack.bot_token_env.is_none());
        assert!(slack.bot_token.is_some());
    }

    #[test]
    fn loads_reviewer_inline_api_key() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
reviewer:
  enabled: true
  provider: anthropic
  model: claude-sonnet-4-6
  api_key_env: ANTHROPIC_API_KEY
  api_key:
    value: "sk-ant-inline"
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("inline reviewer api_key should parse");
        let rv = cfg.reviewer.unwrap();
        match rv.api_key.unwrap() {
            SecretSource::Inline { value } => assert_eq!(value, "sk-ant-inline"),
            _ => panic!("expected inline reviewer key"),
        }
        // api_key_env still present:
        assert_eq!(rv.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn loads_notifications_block() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    start_work: false
    failure_alerts: true
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        let co = cfg.chatops.expect("chatops present");
        let n = co.notifications.clone().expect("notifications present");
        assert!(!n.start_work);
        assert!(n.failure_alerts);
        assert!(!NotificationsConfig::start_work_enabled(Some(&co)));
        assert!(NotificationsConfig::failure_alerts_enabled(Some(&co)));
    }

    #[test]
    fn notifications_partial_populated_defaults_other_to_true() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    start_work: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        let co = cfg.chatops.expect("chatops present");
        let n = co.notifications.expect("notifications present");
        assert!(!n.start_work);
        assert!(n.failure_alerts, "omitted field must default to true");
    }

    #[test]
    fn notifications_rejects_unknown_field() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    start_work: true
    typo_field: oops
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path)
            .expect_err("unknown field in notifications must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("typo_field") || msg.to_lowercase().contains("unknown"),
            "error should mention unknown field; got: {msg}"
        );
    }

    #[test]
    fn pr_opened_default_is_true_when_block_absent() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops present");
        assert!(NotificationsConfig::pr_opened_enabled(Some(&co)));
        assert!(NotificationsConfig::pr_opened_enabled(None));
    }

    #[test]
    fn pr_opened_default_is_true_when_field_absent() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    start_work: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops present");
        let n = co.notifications.clone().expect("notifications present");
        assert!(n.pr_opened, "field defaults to true when omitted");
        assert!(NotificationsConfig::pr_opened_enabled(Some(&co)));
    }

    #[test]
    fn pr_opened_explicit_false_disables() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
  notifications:
    pr_opened: false
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        let co = cfg.chatops.expect("chatops present");
        let n = co.notifications.clone().expect("notifications present");
        assert!(!n.pr_opened);
        assert!(!NotificationsConfig::pr_opened_enabled(Some(&co)));
    }

    #[test]
    fn notifications_absent_block_defaults_both_true() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
chatops:
  provider: slack
  default_channel_id: C0DEFAULT
  slack:
    bot_token_env: SLACK_BOT_TOKEN
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).expect("config should parse");
        let co = cfg.chatops.expect("chatops present");
        assert!(co.notifications.is_none(), "block must be absent");
        // Helpers must default to true when block omitted.
        assert!(NotificationsConfig::start_work_enabled(Some(&co)));
        assert!(NotificationsConfig::failure_alerts_enabled(Some(&co)));
        // Helpers must also default to true when chatops itself is None.
        assert!(NotificationsConfig::start_work_enabled(None));
        assert!(NotificationsConfig::failure_alerts_enabled(None));
    }

    #[test]
    fn executor_perma_stuck_default_is_two() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.executor.perma_stuck_after_failures.is_none());
        assert_eq!(cfg.executor.perma_stuck_threshold(), 2);
    }

    #[test]
    fn executor_perma_stuck_clamps_zero_to_one() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  perma_stuck_after_failures: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.perma_stuck_after_failures, Some(0));
        assert_eq!(
            cfg.executor.perma_stuck_threshold(),
            1,
            "zero must clamp to one"
        );
    }

    #[test]
    fn executor_perma_stuck_accepts_custom_value() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  perma_stuck_after_failures: 5
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.perma_stuck_after_failures, Some(5));
        assert_eq!(cfg.executor.perma_stuck_threshold(), 5);
    }

    #[test]
    fn max_changes_per_pr_global_default_is_3() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.repositories[0].max_changes_per_pr.is_none());
        assert!(cfg.executor.max_changes_per_pr.is_none());
        assert_eq!(cfg.repositories[0].max_changes_per_pr(&cfg.executor), 3);
    }

    #[test]
    fn max_changes_per_pr_executor_fallback_applies() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  max_changes_per_pr: 2
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.max_changes_per_pr, Some(2));
        assert_eq!(cfg.repositories[0].max_changes_per_pr(&cfg.executor), 2);
    }

    #[test]
    fn max_changes_per_pr_per_repo_override_takes_precedence() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    max_changes_per_pr: 5
executor:
  kind: claude_cli
  max_changes_per_pr: 2
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.repositories[0].max_changes_per_pr, Some(5));
        assert_eq!(cfg.executor.max_changes_per_pr, Some(2));
        assert_eq!(
            cfg.repositories[0].max_changes_per_pr(&cfg.executor),
            5,
            "per-repo override must win over executor-level"
        );
    }

    #[test]
    fn max_changes_per_pr_zero_clamps_to_1() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
    max_changes_per_pr: 0
executor:
  kind: claude_cli
  max_changes_per_pr: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        // Raw configured values preserved so the WARN log can name them.
        assert_eq!(cfg.repositories[0].max_changes_per_pr, Some(0));
        assert_eq!(cfg.executor.max_changes_per_pr, Some(0));
        // Effective cap is clamped.
        assert_eq!(cfg.repositories[0].max_changes_per_pr(&cfg.executor), 1);
    }

    #[test]
    fn startup_jitter_default_is_30() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.executor.startup_jitter_max_secs.is_none());
        assert_eq!(cfg.executor.startup_jitter_max_secs(), 30);
    }

    #[test]
    fn startup_jitter_explicit_zero_is_zero() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  startup_jitter_max_secs: 0
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.startup_jitter_max_secs, Some(0));
        assert_eq!(cfg.executor.startup_jitter_max_secs(), 0);
    }

    #[test]
    fn inter_iteration_jitter_default_is_10() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.executor.inter_iteration_jitter_pct.is_none());
        assert_eq!(cfg.executor.inter_iteration_jitter_pct(), 10);
    }

    #[test]
    fn inter_iteration_jitter_above_100_is_clamped() {
        // u8 fits up to 255; values above 100 must clamp to 100 so the
        // negative offset cannot exceed the base interval.
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: claude_cli
  inter_iteration_jitter_pct: 250
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.executor.inter_iteration_jitter_pct, Some(250));
        assert_eq!(cfg.executor.inter_iteration_jitter_pct(), 100);
    }

    #[test]
    fn rejects_unknown_executor_kind() {
        let yaml = r#"
repositories:
  - url: "git@github.com:owner/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 60
executor:
  kind: gpt_cli
github: {}
"#;
        let (_dir, path) = write_config(yaml);
        let err = Config::load_from(&path).expect_err("unknown executor kind should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("gpt_cli") || msg.to_lowercase().contains("variant"),
            "error should reject unknown variant; got: {msg}"
        );
    }
}
