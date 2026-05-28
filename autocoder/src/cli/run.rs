//! `autocoder run` — daemon entry point. Spawns one polling task per
//! configured repository and waits for shutdown signal (SIGINT/SIGTERM) or
//! all tasks to finish.

use crate::audits::{
    AuditRegistry,
    architecture_consultative::ArchitectureConsultativeAudit,
    brightline::ArchitectureBrightlineAudit,
    drift::DriftAudit,
    missing_tests::MissingTestsAudit, security_bug::SecurityBugAudit,
};
use crate::chatops;
use crate::code_reviewer::CodeReviewer;
use crate::config::{
    AuditSettings, AuditsConfig, Config, ExecutorKind, GithubConfig, NotificationsConfig,
    RepositoryConfig, clamp_max_audits_per_iteration, validate_audit_type_names,
};
use crate::control_socket::{
    self, ChatOpsHolder, ChatOpsSlot, ControlState, GithubHolder, RepoTaskHandle, RepoTaskMap,
    ReviewerHolder, SpawnOutcome, SpawnRepoFn,
};
use crate::executor::{Executor, claude_cli::ClaudeCliExecutor};
use crate::github::parse_repo_url;
use crate::github_credentials::resolve_token_with_source;
use crate::{git, migration, paths, polling_loop, workspace};
use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub async fn execute(mut cfg: Config, config_path: PathBuf) -> Result<()> {
    // Resolve + install the daemon-paths global BEFORE any callsite
    // that reads workspace / control-socket / log / state paths. The
    // resolution order (config → AUTOCODER_*_DIR → systemd → XDG →
    // hard fallback) is owned by `paths::resolve_daemon_paths`. After
    // the install, every callsite that previously read
    // `<system-temp>/autocoder/...` paths reads the resolved locations
    // (state on /var/lib, cache on /var/cache, etc.).
    let daemon_paths = paths::resolve_daemon_paths(&cfg)
        .context("resolving daemon data paths")?;
    paths::ensure_directories(&daemon_paths)
        .context("creating daemon data directories")?;
    tracing::info!(
        state = %daemon_paths.state.display(),
        cache = %daemon_paths.cache.display(),
        logs = %daemon_paths.logs.display(),
        runtime = %daemon_paths.runtime.display(),
        "daemon paths resolved"
    );
    paths::install_global(daemon_paths.clone())
        .context("installing global daemon paths")?;

    // Migrate any legacy /tmp paths into the new layout. Logged but
    // never fatal — operators see ERROR lines in journalctl and can
    // clean up any orphan /tmp entries manually. If the marker file
    // is missing AND every entry succeeded, the marker is written so
    // subsequent startups skip the scan.
    match migration::migrate_legacy_tmp_paths(&daemon_paths) {
        Ok(report) => {
            tracing::info!(
                workspaces_moved = report.workspaces_moved,
                state_files_moved = report.state_files_moved,
                log_files_moved = report.log_files_moved,
                errors = report.errors.len(),
                "legacy /tmp migration: scan complete"
            );
            for e in &report.errors {
                tracing::error!("legacy /tmp migration: {e}");
            }
        }
        Err(e) => {
            tracing::error!(
                "legacy /tmp migration: scan failed (daemon continues): {e:#}"
            );
        }
    }

    // Reload audit cadence state from `<state_dir>/audit-state/` so
    // a daemon restart respects on-disk `last_run_at` timestamps and
    // does NOT re-fire audits whose cadence has not elapsed.
    // Currently advisory — the in-memory map is populated at
    // iteration start by `AuditState::load_or_default(workspace)`
    // reading the workspace-local file, which now lives on disk via
    // the workspace move to `cache_dir`. The aggregated
    // `<state_dir>/audit-state/<audit-type>.json` files are a parallel
    // store; reload makes them available for any future cadence
    // resolver that prefers the daemon-global view over the
    // workspace-local view.
    match crate::audits::state::reload_from_disk(&daemon_paths.state) {
        Ok(map) => {
            tracing::info!(
                count = map.len(),
                "audit-state reload: loaded entries from <state_dir>/audit-state/"
            );
        }
        Err(e) => {
            tracing::warn!("audit-state reload failed (daemon continues): {e:#}");
        }
    }

    openspec_preflight()?;

    // Shared startup-time validation: schema, token-route, workspace
    // collision, audit slug, path collision, secret source. Errors block
    // startup with an aggregated message; warnings log and continue.
    // The same `validate_config` function powers `autocoder check-config`
    // so the two surfaces never drift.
    let report = crate::config::validate_config(&cfg);
    for w in &report.warnings {
        tracing::warn!(
            category = w.category.slug(),
            pointer = w.config_pointer.as_deref().unwrap_or(""),
            "{}",
            w.message
        );
    }
    if report.has_errors() {
        let mut msg = format!(
            "startup config validation failed with {} error(s):",
            report.errors.len()
        );
        for e in &report.errors {
            msg.push_str("\n  - ");
            msg.push_str(e.category.slug());
            msg.push_str(": ");
            msg.push_str(&e.message);
        }
        return Err(anyhow!(msg));
    }

    // Per-repo startup info log naming the resolved token source. Preserved
    // from the pre-validate_config validate_github_token_routes path so
    // operators still see one info line per repo at startup.
    for repo in &cfg.repositories {
        let owner = match parse_repo_url(&repo.url) {
            Ok((o, _r)) => o,
            Err(_) => continue,
        };
        if let Ok((_value, source_desc)) = resolve_token_with_source(&cfg.github, &owner) {
            tracing::info!(
                "repository {} will use GitHub token from {}",
                repo.url,
                source_desc
            );
        }
    }
    // Precedence advisory: if `github.token` is inline AND `github.token_env`
    // is also set, the inline value wins; tell the operator their env var is
    // being ignored on this field.
    if cfg
        .github
        .token
        .as_ref()
        .map(|s| s.is_inline())
        .unwrap_or(false)
        && std::env::var(&cfg.github.token_env).is_ok()
    {
        tracing::warn!(
            "github.token (inline) takes precedence; env var `{}` is being ignored for the global GitHub token",
            cfg.github.token_env
        );
    }

    if cfg.github.recreate_fork_on_reinit && cfg.github.fork_owner.is_none() {
        tracing::info!(
            "github.recreate_fork_on_reinit is true but fork_owner is unset; flag will have no effect"
        );
    }
    ensure_forks_exist(&cfg.github, &cfg.repositories).await?;

    let executor: Arc<dyn Executor> = match cfg.executor.kind {
        ExecutorKind::ClaudeCli => Arc::new(
            ClaudeCliExecutor::from_config(&cfg.executor)
                .context("initializing ClaudeCliExecutor from config")?,
        ),
    };

    let reviewer_initial: Option<Arc<CodeReviewer>> = match cfg.reviewer.as_ref() {
        Some(rcfg) if rcfg.enabled => {
            let r = CodeReviewer::from_config(rcfg)
                .context("initializing code reviewer from config")?;
            tracing::info!(
                provider = ?rcfg.provider,
                model = rcfg.model.as_str(),
                "code reviewer enabled"
            );
            Some(Arc::new(r))
        }
        _ => {
            tracing::info!("code reviewer disabled (no reviewer block, or enabled: false)");
            None
        }
    };

    let chatops_initial: Option<ChatOpsSlot> = match cfg.chatops.as_ref() {
        Some(co) => {
            let backend = chatops::from_config(co)
                .await
                .context("initializing chatops backend from config")?;
            emit_chatops_startup_log(backend.provider_name(), backend.is_experimental());
            Some(ChatOpsSlot {
                backend,
                default_channel_id: co.default_channel_id.clone(),
                start_work_enabled: NotificationsConfig::start_work_enabled(Some(co)),
                failure_alerts_enabled: NotificationsConfig::failure_alerts_enabled(Some(co)),
                pr_opened_enabled: NotificationsConfig::pr_opened_enabled(Some(co)),
            })
        }
        None => {
            tracing::info!("ChatOps escalation disabled (no chatops: config block)");
            None
        }
    };

    // Lifecycle signal: post a one-line version + repo-count notification
    // before any polling task starts. Independent of `notifications.*`
    // flags. Suppressed (with a journalctl-bound INFO log) when no chatops
    // backend is configured.
    dispatch_startup_notification(chatops_initial.as_ref(), cfg.repositories.len()).await;

    // Hot-swappable holders. The control socket swaps into these on
    // `autocoder reload`; the polling loops read snapshots once per pass.
    let github_holder: GithubHolder = Arc::new(ArcSwap::from_pointee(cfg.github.clone()));
    let reviewer_holder: ReviewerHolder = Arc::new(ArcSwap::from_pointee(reviewer_initial));
    let chatops_holder: ChatOpsHolder = Arc::new(ArcSwap::from_pointee(chatops_initial));

    for repo in &cfg.repositories {
        let derived = workspace::resolve_path(repo);
        tracing::info!(
            url = repo.url.as_str(),
            workspace = %derived.display(),
            poll_interval_sec = repo.poll_interval_sec,
            "configured repository"
        );
    }

    let cancel = CancellationToken::new();

    // Log-retention pass: prune per-change logs older than the
    // configured window whose corresponding change directory is no
    // longer active. Runs immediately at startup, then once per day
    // until shutdown. A misconfigured retention_days above the ceiling
    // is clamped in `Config::load_from` already; this WARN surfaces
    // the original-vs-clamped delta to the operator at startup.
    let raw_retention = cfg.executor.log_retention_days;
    if raw_retention > crate::config::LOG_RETENTION_DAYS_CEILING {
        tracing::warn!(
            configured = raw_retention,
            ceiling = crate::config::LOG_RETENTION_DAYS_CEILING,
            "executor.log_retention_days is above the ceiling; clamped to ceiling"
        );
    }
    let retention_cfg = crate::log_retention::RetentionConfig {
        days: cfg.executor.log_retention_days,
    };
    let _retention_handle = crate::log_retention::spawn_periodic(
        daemon_paths.logs.clone(),
        daemon_paths.cache.join("workspaces"),
        retention_cfg,
        cancel.clone(),
    );

    // Busy-marker stuck threshold for the LIVE-PID branch: how long a
    // live in-flight iteration is allowed to hold the marker before
    // the next pass treats it as stuck and SIGTERMs the process
    // group. Sourced from `executor.busy_marker_stale_threshold_secs`
    // (default 600s, max 7200s clamped) per
    // `a08-busy-marker-recovery-semantics`. Decoupled from
    // `executor.timeout_secs` — raising the executor timeout for one
    // legitimately long-running change does NOT delay stale-marker
    // recovery on unrelated iterations.
    //
    // Dead-PID markers are recovered IMMEDIATELY regardless of this
    // value; the dead-pid branch in `busy_marker::try_acquire_with`
    // does not consult an age gate.
    let stuck_threshold_secs: u64 = cfg.executor.busy_marker_stale_threshold_secs();
    let timeout_secs = cfg.executor.timeout_secs;
    match crate::config::busy_marker_threshold_startup_log(
        cfg.executor.busy_marker_stale_threshold_secs,
        stuck_threshold_secs,
        timeout_secs,
    ) {
        crate::config::BusyMarkerThresholdStartupLog::Migration {
            new_threshold_secs,
            pre_spec_implicit_threshold_secs,
            timeout_secs,
        } => {
            tracing::info!(
                new_threshold_secs,
                pre_spec_implicit_threshold_secs,
                timeout_secs,
                "busy marker stale threshold is now {new_threshold_secs}s (was implicit \
                 {pre_spec_implicit_threshold_secs}s via timeout_secs+10min). Pre-spec \
                 operators raising timeout_secs no longer see proportional recovery \
                 delays. Set executor.busy_marker_stale_threshold_secs explicitly to \
                 override."
            );
        }
        crate::config::BusyMarkerThresholdStartupLog::Regular {
            timeout_secs,
            busy_marker_stale_threshold_secs,
        } => {
            tracing::info!(
                timeout_secs,
                busy_marker_stale_threshold_secs,
                "executor timeout: {timeout_secs}s; busy_marker_stale_threshold: \
                 {busy_marker_stale_threshold_secs}s"
            );
        }
    }

    // Perma-stuck consecutive-failure threshold. `perma_stuck_threshold`
    // clamps a misconfigured 0 to 1 internally; we WARN once here so the
    // operator notices their config is bogus.
    if cfg.executor.perma_stuck_after_failures == Some(0) {
        tracing::warn!(
            "executor.perma_stuck_after_failures is set to 0; clamping to 1 (a zero threshold would mark every change perma-stuck before the first attempt — fix your config)"
        );
    }
    let perma_stuck_threshold: u32 = cfg.executor.perma_stuck_threshold();

    // Per-PR change cap. Misconfigured `0` is clamped to `1` inside
    // `RepositoryConfig::max_changes_per_pr`; we WARN once here at startup
    // so the operator notices.
    if cfg.executor.max_changes_per_pr == Some(0) {
        tracing::warn!(
            "executor.max_changes_per_pr is set to 0; clamping to 1 (each PR would ship zero commits otherwise — fix your config)"
        );
    }
    for (idx, repo) in cfg.repositories.iter().enumerate() {
        if repo.max_changes_per_pr == Some(0) {
            tracing::warn!(
                idx = idx,
                url = %repo.url,
                "repositories[{idx}].max_changes_per_pr is set to 0; clamping to 1"
            );
        }
    }

    // Per-PR revision cap. Values above the ceiling are clamped down in
    // `max_revisions_per_pr_clamped()`; we WARN once here so the operator
    // notices the bogus value.
    if cfg.executor.max_revisions_per_pr > crate::config::MAX_REVISIONS_PER_PR_CEILING {
        tracing::warn!(
            configured = cfg.executor.max_revisions_per_pr,
            ceiling = crate::config::MAX_REVISIONS_PER_PR_CEILING,
            "executor.max_revisions_per_pr is set above the ceiling; clamping (a runaway revision loop would otherwise burn tokens — fix your config)"
        );
    }

    // Build the audit registry once at startup. Operators wire the
    // architecture-brightline audit by listing its slug under
    // `audits.defaults` (and optionally setting `extra` knobs under
    // `audits.settings.architecture_brightline`); the cadence resolver
    // returns `Disabled` for absent entries so the registry can stay
    // populated without forcing any audit to run.
    let audit_settings: HashMap<String, AuditSettings> = cfg
        .audits
        .as_ref()
        .map(|a| a.settings.clone())
        .unwrap_or_default();
    let mut registry = AuditRegistry::new();
    registry.register(Arc::new(ArchitectureBrightlineAudit::new(&audit_settings)));
    registry.register(Arc::new(DriftAudit::new(&audit_settings, &cfg.executor)));
    registry.register(Arc::new(MissingTestsAudit::new(
        &audit_settings,
        &cfg.executor,
    )));
    registry.register(Arc::new(SecurityBugAudit::new(
        &audit_settings,
        &cfg.executor,
    )));
    registry.register(Arc::new(ArchitectureConsultativeAudit::new(
        &audit_settings,
        &cfg.executor,
    )));
    // Validate every audit type name in the operator's config is in the
    // registry. A typo here means the audit will silently never run, so
    // we fail fast at startup with the list of known names.
    validate_audit_type_names(&cfg, &registry.known_type_names())?;
    // Clamp `audits.max_audits_per_iteration` against the registry size.
    // The clamp lives at startup (not in `Config::load_from`) because the
    // ceiling depends on how many audits are registered, which the
    // config loader doesn't know.
    let registry_count = registry.len();
    if let Some(audits) = cfg.audits.as_mut() {
        let (clamped, _) =
            clamp_max_audits_per_iteration(audits.max_audits_per_iteration, registry_count);
        audits.max_audits_per_iteration = clamped;
    }
    let audit_type_list = registry.known_type_names().join(", ");
    let max_audits_per_iteration = cfg
        .audits
        .as_ref()
        .map(|a| a.max_audits_per_iteration)
        .unwrap_or_else(crate::config::default_max_audits_per_iteration);
    tracing::info!(
        max_per_iteration = max_audits_per_iteration,
        "audits configured: {audit_type_list}; max_per_iteration={max_audits_per_iteration}"
    );
    let audits_cfg_arc: Option<Arc<AuditsConfig>> = cfg.audits.clone().map(Arc::new);
    let audit_registry: Arc<AuditRegistry> = Arc::new(registry);
    let audit_settings_arc: Arc<HashMap<String, AuditSettings>> =
        Arc::new(audit_settings);

    let task_map: RepoTaskMap = Arc::new(Mutex::new(HashMap::new()));
    let task_map_changed = Arc::new(tokio::sync::Notify::new());
    let executor_max_changes_per_pr = cfg.executor.max_changes_per_pr;
    let revision_cap = cfg.executor.max_revisions_per_pr_clamped();
    let startup_jitter_max_secs = cfg.executor.startup_jitter_max_secs();
    let inter_iteration_jitter_pct = cfg.executor.inter_iteration_jitter_pct();
    let spawn_repo = build_spawn_repo_fn(SpawnDeps {
        executor: executor.clone(),
        github_holder: github_holder.clone(),
        reviewer_holder: reviewer_holder.clone(),
        chatops_holder: chatops_holder.clone(),
        stuck_threshold_secs,
        perma_stuck_threshold,
        executor_max_changes_per_pr,
        revision_cap,
        startup_jitter_max_secs,
        inter_iteration_jitter_pct,
        audit_registry: audit_registry.clone(),
        audits_cfg: audits_cfg_arc.clone(),
        audit_settings: audit_settings_arc.clone(),
        global_cancel: cancel.clone(),
        task_map: task_map.clone(),
        task_map_changed: task_map_changed.clone(),
    });

    for repo in cfg.repositories.iter().cloned() {
        match spawn_repo(repo) {
            SpawnOutcome::Spawned => {}
            SpawnOutcome::AlreadyPresent => {
                // Cannot happen at startup (map is empty) but log defensively.
                tracing::warn!("startup: spawn helper reported duplicate URL — ignoring");
            }
            SpawnOutcome::StartupCheckFailed => {
                // Per orchestrator-cli baseline: a repo whose workspace
                // fails the startup check is skipped for the remainder
                // of the process lifetime. Other repos continue.
            }
        }
    }

    // Spawn the control-socket listener as a sibling task. It shares the
    // same cancellation token as the polling tasks.
    let control_state = ControlState {
        github: github_holder.clone(),
        reviewer: reviewer_holder.clone(),
        chatops: chatops_holder.clone(),
        last_config: Arc::new(ArcSwap::from_pointee(cfg.clone())),
        config_path,
        repo_tasks: task_map.clone(),
        repo_tasks_changed: task_map_changed.clone(),
        spawn_repo: spawn_repo.clone(),
    };
    let listener_cancel = cancel.clone();
    let control_handle: JoinHandle<()> = tokio::spawn(async move {
        if let Err(e) = control_socket::listen(control_state, listener_cancel).await {
            tracing::error!("control socket listener exited: {e:#}");
        }
    });

    // Spawn the inbound ChatOps listener (Slack Socket Mode) as a
    // sibling task. The listener is only started when:
    //   - chatops is configured, AND
    //   - the chosen backend supports `start_inbound_listener`, AND
    //   - the backend has the provider-specific inbound credential
    //     (Slack: `app_token`).
    //
    // Returns a (potentially empty) Vec of JoinHandles so the
    // shutdown path awaits the listener before exiting.
    let inbound_handles = spawn_inbound_listener(
        &cfg,
        chatops_holder.clone(),
        task_map.clone(),
        audit_registry.clone(),
        cancel.clone(),
    )
    .await;

    spawn_signal_handler(cancel.clone());

    // The polling tasks loop until the global cancellation token fires
    // (or the per-repo token from a reload-induced cancel). Wait for the
    // global cancel, then drain the task map and await every polling
    // task. The wrapper inside the spawn closure removes its own entry
    // on exit, so by draining the map first we take ownership of the
    // JoinHandles before any wrapper races us for the lock.
    cancel.cancelled().await;
    let handles: Vec<JoinHandle<()>> = {
        let mut guard = task_map.lock().unwrap();
        guard.drain().map(|(_, h)| h.join).collect()
    };
    for h in handles {
        if let Err(e) = h.await {
            tracing::error!("polling task panicked: {e}");
        }
    }
    if let Err(e) = control_handle.await {
        tracing::error!("control socket task panicked: {e}");
    }
    for h in inbound_handles {
        if let Err(e) = h.await {
            tracing::error!("chatops inbound listener task panicked: {e}");
        }
    }

    tracing::info!("shutdown complete");
    Ok(())
}

/// Spawn the chatops inbound listener if the active backend supports
/// it and the operator has supplied the inbound credential. Builds
/// the channel allowlist from the union of every
/// `repositories[].chatops_channel_id`, `chatops.default_channel_id`,
/// and `chatops.slack.listen_channels`. Returns one or zero
/// JoinHandles (so the shutdown path can `await` them uniformly).
///
/// WARN-and-skip cases:
///   - no chatops config block → silent skip (already logged
///     elsewhere at startup);
///   - chatops configured but no Slack `app_token` → WARN + skip;
///   - app_token present but allowlist empty → WARN + spawn anyway
///     (the listener will drop every command silently; the operator
///     may reload-add channels later).
async fn spawn_inbound_listener(
    cfg: &Config,
    chatops_holder: ChatOpsHolder,
    task_map: RepoTaskMap,
    audit_registry: Arc<AuditRegistry>,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let slot_arc = chatops_holder.load_full();
    let slot = match slot_arc.as_ref() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let chatops_cfg = match cfg.chatops.as_ref() {
        Some(c) => c,
        None => return Vec::new(),
    };
    let slack_sub = chatops_cfg.slack.as_ref();
    let has_app_token = slack_sub
        .map(|s| s.app_token.is_some() || s.app_token_env.is_some())
        .unwrap_or(false);
    if !has_app_token {
        tracing::warn!(
            "chatops inbound listener not started: chatops.slack.app_token not configured. \
             Operator commands like '@<bot> status <repo>' will not receive replies. \
             See README \"ChatOps operator commands → Setup\" for setup."
        );
        return Vec::new();
    }

    // Build the channel allowlist:
    //   union(
    //     every repositories[].chatops_channel_id,
    //     chatops.default_channel_id (if set),
    //     chatops.slack.listen_channels,
    //   )
    let mut allowed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for repo in &cfg.repositories {
        if let Some(c) = repo.chatops_channel_id.as_deref()
            && !c.is_empty()
        {
            allowed.insert(c.to_string());
        }
    }
    if !chatops_cfg.default_channel_id.is_empty() {
        allowed.insert(chatops_cfg.default_channel_id.clone());
    }
    if let Some(sub) = slack_sub {
        for c in &sub.listen_channels {
            if !c.is_empty() {
                allowed.insert(c.clone());
            }
        }
    }
    if allowed.is_empty() {
        tracing::warn!(
            "chatops inbound listener: no channels in allowlist. The bot will be \
             connected but will silently drop every command. Configure at least one \
             chatops_channel_id on a repository, or set chatops.slack.listen_channels."
        );
    }

    let dispatcher = Arc::new(
        crate::chatops::operator_commands::OperatorCommandDispatcher::new()
            .with_audit_types(
                audit_registry
                    .known_type_names()
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )
            .with_chatops(slot.backend.clone()),
    );
    let task_map_for_provider = task_map.clone();
    let repos: Arc<dyn crate::chatops::operator_commands::RepoIdentityProvider> =
        Arc::new(crate::chatops::TaskMapRepoIdentities::new(move || {
            let guard = task_map_for_provider.lock().unwrap();
            guard
                .values()
                .map(|h| h.config.load_full().as_ref().clone())
                .collect()
        }));
    let allowed_arc = Arc::new(allowed);

    let backend = slot.backend.clone();
    match backend
        .start_inbound_listener(dispatcher, repos, allowed_arc, cancel)
        .await
    {
        Ok(h) => vec![h],
        Err(e) => {
            tracing::warn!("chatops inbound listener could not start: {e:#}");
            Vec::new()
        }
    }
}

/// Dependencies the daemon captures into the spawn closure so the reload
/// handler can launch new polling tasks without re-deriving them.
struct SpawnDeps {
    executor: Arc<dyn Executor>,
    github_holder: GithubHolder,
    reviewer_holder: ReviewerHolder,
    chatops_holder: ChatOpsHolder,
    stuck_threshold_secs: u64,
    perma_stuck_threshold: u32,
    executor_max_changes_per_pr: Option<u32>,
    revision_cap: u32,
    startup_jitter_max_secs: u64,
    inter_iteration_jitter_pct: u8,
    audit_registry: Arc<AuditRegistry>,
    audits_cfg: Option<Arc<AuditsConfig>>,
    audit_settings: Arc<HashMap<String, AuditSettings>>,
    global_cancel: CancellationToken,
    task_map: RepoTaskMap,
    task_map_changed: Arc<tokio::sync::Notify>,
}

/// Build a `SpawnRepoFn` that runs the repo's startup check, then spawns
/// the per-repo polling task with a fresh child cancellation token and a
/// new `Arc<ArcSwap<RepositoryConfig>>` holder. The spawned task removes
/// its own map entry on exit so the next reload sees an absent URL.
fn build_spawn_repo_fn(deps: SpawnDeps) -> SpawnRepoFn {
    Arc::new(move |repo: RepositoryConfig| {
        let url = repo.url.clone();
        // Fast-path duplicate check before doing the (potentially slow)
        // startup check. Re-checked under the lock below to close the
        // race window between this and the insert.
        {
            let guard = deps.task_map.lock().unwrap();
            if guard.contains_key(&url) {
                return SpawnOutcome::AlreadyPresent;
            }
        }
        // Startup check uses the live github config (post-reload it may
        // differ from what was on disk at process start).
        let github_snap = deps.github_holder.load_full();
        if !repo_passes_startup_check(&repo, &github_snap) {
            return SpawnOutcome::StartupCheckFailed;
        }
        let child_cancel = deps.global_cancel.child_token();
        let config_holder: Arc<ArcSwap<RepositoryConfig>> =
            Arc::new(ArcSwap::from_pointee(repo));
        let cancel_for_task = child_cancel.clone();
        let config_for_task = config_holder.clone();
        let map_for_task = deps.task_map.clone();
        let map_changed_for_task = deps.task_map_changed.clone();
        let url_for_task = url.clone();
        let executor_for_task = deps.executor.clone();
        let github_for_task = deps.github_holder.clone();
        let reviewer_for_task = deps.reviewer_holder.clone();
        let chatops_for_task = deps.chatops_holder.clone();
        let stuck = deps.stuck_threshold_secs;
        let perma = deps.perma_stuck_threshold;
        let exec_max = deps.executor_max_changes_per_pr;
        let revision_cap_for_task = deps.revision_cap;
        let startup_jitter = deps.startup_jitter_max_secs;
        let iter_jitter = deps.inter_iteration_jitter_pct;
        let registry_for_task = deps.audit_registry.clone();
        let audits_cfg_for_task = deps.audits_cfg.clone();
        let audit_settings_for_task = deps.audit_settings.clone();
        let pending_rebuild: Arc<std::sync::atomic::AtomicBool> =
            Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pending_rebuild_for_task = pending_rebuild.clone();
        let pending_triages: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let pending_triages_for_task = pending_triages.clone();
        let pending_audit_runs: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let pending_audit_runs_for_task = pending_audit_runs.clone();
        let pending_proposal_requests: Arc<
            std::sync::Mutex<Vec<crate::control_socket::ProposalRequest>>,
        > = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pending_proposal_requests_for_task = pending_proposal_requests.clone();
        let pending_changelog_requests: Arc<
            std::sync::Mutex<Vec<crate::control_socket::ChangelogRequest>>,
        > = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pending_changelog_requests_for_task = pending_changelog_requests.clone();
        let iteration_cancel: Arc<std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>> =
            Arc::new(std::sync::Mutex::new(None));
        let iteration_cancel_for_task = iteration_cancel.clone();
        let iteration_drained: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
        let iteration_drained_for_task = iteration_drained.clone();
        let join: JoinHandle<()> = tokio::spawn(async move {
            polling_loop::run(
                config_for_task,
                executor_for_task,
                github_for_task,
                reviewer_for_task,
                chatops_for_task,
                stuck,
                perma,
                exec_max,
                revision_cap_for_task,
                startup_jitter,
                iter_jitter,
                registry_for_task,
                audits_cfg_for_task,
                audit_settings_for_task,
                pending_rebuild_for_task,
                pending_triages_for_task,
                pending_audit_runs_for_task,
                pending_proposal_requests_for_task,
                pending_changelog_requests_for_task,
                iteration_cancel_for_task,
                iteration_drained_for_task,
                cancel_for_task,
            )
            .await;
            {
                let mut guard = map_for_task.lock().unwrap();
                guard.remove(&url_for_task);
            }
            map_changed_for_task.notify_waiters();
        });
        let inserted = {
            let mut guard = deps.task_map.lock().unwrap();
            if guard.contains_key(&url) {
                // Lost the race against another spawn. Cancel ours and
                // report the URL as already present.
                child_cancel.cancel();
                return SpawnOutcome::AlreadyPresent;
            }
            guard.insert(
                url,
                RepoTaskHandle {
                    cancel: child_cancel,
                    config: config_holder,
                    join,
                    pending_rebuild,
                    pending_triages,
                    pending_audit_runs,
                    pending_proposal_requests,
                    pending_changelog_requests,
                    iteration_cancel,
                    iteration_drained,
                },
            );
            true
        };
        if inserted {
            deps.task_map_changed.notify_waiters();
        }
        SpawnOutcome::Spawned
    })
}

/// Format the startup version notification text. Lives in its own helper
/// so unit tests can assert the message shape without invoking the
/// async dispatch path.
pub fn startup_version_message(version: &str, repo_count: usize) -> String {
    format!(
        "🆙 autocoder v{version} started — {repo_count} repository(ies) configured"
    )
}

/// Fire the daemon-lifecycle startup notification exactly once per boot.
/// When a chatops backend is configured, post a one-line `🆙` notification
/// to the resolved default channel. When not, emit a journalctl-bound
/// INFO line so operators still have a startup-version signal in logs.
///
/// This is independent of `chatops.notifications.*` flags (those gate
/// per-change signals; the startup line is a daemon-lifecycle signal).
/// A `post_notification` failure is logged at WARN level and does NOT
/// block the daemon from proceeding to the polling loop.
pub async fn dispatch_startup_notification(
    chatops: Option<&ChatOpsSlot>,
    repo_count: usize,
) {
    let version = env!("CARGO_PKG_VERSION");
    let msg = startup_version_message(version, repo_count);
    match chatops {
        Some(slot) => {
            if let Err(e) = slot
                .backend
                .post_notification(&slot.default_channel_id, &msg)
                .await
            {
                tracing::warn!("startup version notification failed: {e}");
            }
        }
        None => {
            tracing::info!(
                "startup version: v{version}; {repo_count} repositories"
            );
        }
    }
}

/// Emit the one-shot startup log line for the active ChatOps backend.
/// Experimental backends get a `warn`-level line containing `"EXPERIMENTAL"`
/// and `"best-effort"`; Slack (and any future official backend) gets an
/// `info`-level line without those markers.
pub fn emit_chatops_startup_log(provider: &str, experimental: bool) {
    if experimental {
        tracing::warn!(
            "EXPERIMENTAL: ChatOps escalation enabled via {provider} — best-effort support, may break without notice, no API-stability guarantees"
        );
    } else {
        tracing::info!("ChatOps escalation enabled via {provider} (officially supported)");
    }
}

/// Verify the `openspec` binary is reachable before the polling loop
/// starts. A failed preflight aborts daemon startup so misconfigured
/// deployments fail loudly instead of looping forever producing nothing.
pub fn openspec_preflight() -> Result<()> {
    openspec_preflight_with("openspec")
}

/// Internal preflight that takes the binary name as an argument so tests
/// can target a name guaranteed to be absent.
fn openspec_preflight_with(bin: &str) -> Result<()> {
    match std::process::Command::new(bin).arg("--version").output() {
        Ok(out) if out.status.success() => {
            tracing::info!(
                version = %String::from_utf8_lossy(&out.stdout).trim(),
                "openspec preflight passed"
            );
            Ok(())
        }
        Ok(out) => {
            let stderr_tail: String =
                String::from_utf8_lossy(&out.stderr).chars().take(200).collect();
            Err(anyhow!(
                "openspec preflight failed: `{bin} --version` exited {code:?}. stderr: {stderr_tail}",
                code = out.status.code(),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "openspec preflight failed: `{bin}` binary not found on PATH. \
             Install openspec and ensure the systemd unit's PATH covers its install directory."
        )),
        Err(e) => Err(anyhow!(
            "openspec preflight failed: spawning `{bin} --version` errored: {e}"
        )),
    }
}

/// Resolve a GitHub PAT route for every configured repository before any
/// polling task spawns. Returns `Err` aggregating every failure when one
/// or more repos have no routable token; on success, emits one info log
/// per repo naming the env var (never the token value) that will be used.
pub fn validate_github_token_routes(
    github: &GithubConfig,
    repos: &[RepositoryConfig],
) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    for repo in repos {
        let owner = match parse_repo_url(&repo.url) {
            Ok((o, _r)) => o,
            Err(e) => {
                failures.push(format!("repo `{}`: {e:#}", repo.url));
                continue;
            }
        };
        match resolve_token_with_source(github, &owner) {
            Ok((_value, source_desc)) => {
                tracing::info!(
                    "repository {} will use GitHub token from {}",
                    repo.url,
                    source_desc
                );
            }
            Err(e) => {
                failures.push(format!("repo `{}`: {e:#}", repo.url));
            }
        }
    }
    if !failures.is_empty() {
        return Err(anyhow!(
            "GitHub token routing failed for {} repository(ies):\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        ));
    }
    // Precedence warning: if `github.token` is inline AND the env var named
    // by `github.token_env` is also set, the inline value wins; tell the
    // operator their env var is being ignored on this field.
    if github
        .token
        .as_ref()
        .map(|s| s.is_inline())
        .unwrap_or(false)
        && std::env::var(&github.token_env).is_ok()
    {
        tracing::warn!(
            "github.token (inline) takes precedence; env var `{}` is being ignored for the global GitHub token",
            github.token_env
        );
    }
    Ok(())
}

/// When fork-PR mode is active, ensure each configured repository has a
/// reachable fork at the derived URL. Missing forks are created via the
/// GitHub REST API, then probed via `git ls-remote` with a 60-second
/// timeout. Aggregates failures into a single startup error.
pub async fn ensure_forks_exist(
    github: &GithubConfig,
    repos: &[RepositoryConfig],
) -> Result<()> {
    let Some(fork_owner) = github.fork_owner.as_deref() else {
        return Ok(());
    };
    let mut failures: Vec<String> = Vec::new();
    for repo in repos {
        let fork_url = match crate::github::derive_fork_url(&repo.url, fork_owner) {
            Ok(u) => u,
            Err(e) => {
                failures.push(format!("repo `{}`: {e:#}", repo.url));
                continue;
            }
        };
        // Quick probe: if the fork is already there, do nothing.
        if crate::git::ls_remote_head(&fork_url).is_ok() {
            continue;
        }
        // Missing fork → POST to GitHub.
        let (upstream_owner, upstream_repo) = match parse_repo_url(&repo.url) {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("repo `{}`: {e:#}", repo.url));
                continue;
            }
        };
        let token = match resolve_token_with_source(github, &upstream_owner) {
            Ok((tok, _src)) => tok,
            Err(e) => {
                failures.push(format!("repo `{}`: cannot resolve PAT for fork creation: {e:#}", repo.url));
                continue;
            }
        };
        tracing::info!(
            "creating fork for {} → {fork_url}",
            repo.url
        );
        if let Err(e) =
            crate::github::create_fork(&upstream_owner, &upstream_repo, &token).await
        {
            failures.push(format!(
                "repo `{}`: fork creation POST failed: {e:#}",
                repo.url
            ));
            continue;
        }
        // Poll until reachable, up to 60 seconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut reachable = false;
        tracing::info!(
            "waiting for fork `{fork_url}` to become reachable (up to 60s)"
        );
        while std::time::Instant::now() < deadline {
            if crate::git::ls_remote_head(&fork_url).is_ok() {
                reachable = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if reachable {
            tracing::info!(
                "created fork {fork_url} from upstream {}",
                repo.url
            );
        } else {
            failures.push(format!(
                "repo `{}`: fork creation succeeded but `{fork_url}` was not reachable within 60s",
                repo.url
            ));
        }
    }
    if !failures.is_empty() {
        return Err(anyhow!(
            "fork-PR mode: {} repository(ies) could not be set up under `{fork_owner}`:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        ));
    }
    Ok(())
}

/// Initialize the workspace and check for a dirty working tree. Returns
/// `true` if the repository is healthy and a polling task should be spawned;
/// `false` (with a logged error) if the workspace is dirty or cannot be
/// initialized.
///
/// TODO(a14): a future spec could extend mid-iteration's
/// `classify_recovery_failure` to startup too, so a transient
/// `Could not resolve host` at boot waits for the next iteration instead
/// of skipping the repo for the daemon's lifetime. For now startup keeps
/// its conservative skip-for-lifetime contract: any failure here removes
/// the repo from the polling set until the operator restarts the daemon.
pub fn repo_passes_startup_check(repo: &RepositoryConfig, github: &GithubConfig) -> bool {
    let workspace_path = workspace::resolve_path(repo);
    let fork_url = match github.fork_owner.as_deref() {
        Some(owner) => match crate::github::derive_fork_url(&repo.url, owner) {
            Ok(u) => Some(u),
            Err(e) => {
                tracing::error!(
                    url = repo.url.as_str(),
                    "cannot derive fork URL for fork-PR mode: {e:#}; this repository is skipped for the process lifetime"
                );
                return false;
            }
        },
        None => None,
    };
    // Recreate-fork mode + absent workspace: defer all init to the first
    // polling iteration. The recreate path runs async (`DELETE /repos/...`
    // + `POST /repos/.../forks`) and we can't call async work from the
    // sync spawn closure that wraps this function. The polling iteration
    // has its own async context and its own failure-alert plumbing.
    let defer_init =
        github.recreate_fork_on_reinit && fork_url.is_some() && !workspace_path.exists();
    if defer_init {
        tracing::info!(
            url = repo.url.as_str(),
            workspace = %workspace_path.display(),
            "deferring workspace init to first polling iteration \
             (recreate_fork_on_reinit + absent workspace)"
        );
        return true;
    }
    let fork_arg = fork_url
        .as_deref()
        .map(|u| (u, repo.agent_branch.as_str()));
    if let Err(e) = workspace::ensure_initialized(&workspace_path, &repo.url, fork_arg) {
        tracing::error!(
            url = repo.url.as_str(),
            workspace = %workspace_path.display(),
            "workspace initialization failed; this repository is skipped for the process lifetime: {e:#}"
        );
        return false;
    }
    match git::status_porcelain(&workspace_path) {
        Ok(s) if s.is_empty() => true,
        Ok(dirty) => {
            let dirty_count = dirty.lines().count();
            tracing::warn!(
                url = repo.url.as_str(),
                workspace = %workspace_path.display(),
                "workspace dirty at startup ({dirty_count} entries); attempting recovery (git reset --hard origin/{} + git clean -fd)",
                repo.base_branch
            );
            // Best-effort: ignore checkout failures (might already be on
            // base, or HEAD might be detached). The reset + clean are what
            // actually clear the dirty state.
            let _ = git::checkout(&workspace_path, &repo.base_branch);
            if let Err(e) = git::reset_hard_to_remote(&workspace_path, &repo.base_branch) {
                tracing::error!(
                    url = repo.url.as_str(),
                    "recovery `git reset --hard origin/{}` failed: {e:#}; skipping this repository for the process lifetime",
                    repo.base_branch
                );
                return false;
            }
            if let Err(e) = git::clean_force(&workspace_path) {
                tracing::error!(
                    url = repo.url.as_str(),
                    "recovery `git clean -fd` failed: {e:#}; skipping this repository for the process lifetime"
                );
                return false;
            }
            match git::status_porcelain(&workspace_path) {
                Ok(s) if s.is_empty() => {
                    tracing::info!(
                        url = repo.url.as_str(),
                        "workspace recovered; proceeding to normal polling"
                    );
                    true
                }
                _ => {
                    tracing::error!(
                        url = repo.url.as_str(),
                        "workspace still dirty after recovery; skipping this repository for the process lifetime"
                    );
                    false
                }
            }
        }
        Err(e) => {
            tracing::error!(
                url = repo.url.as_str(),
                "could not run git status on workspace: {e:#}; skipping this repository for the process lifetime"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn run_git(path: &Path, args: &[&str]) {
        let st = Command::new("git").args(args).current_dir(path).status().unwrap();
        assert!(st.success(), "git {args:?} failed");
    }

    #[test]
    fn preflight_errors_when_openspec_binary_missing() {
        let err = openspec_preflight_with("openspec-definitely-not-installed-on-this-host")
            .expect_err("missing binary must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("openspec"), "error must name openspec: {msg}");
        assert!(
            msg.contains("PATH") || msg.contains("not found"),
            "error must hint at PATH/install: {msg}"
        );
    }

    #[test]
    fn preflight_errors_when_binary_exits_nonzero() {
        // `false` always exits 1. Path differs by platform (/bin/false on
        // Linux, /usr/bin/false on macOS) — pick whichever exists so the
        // test runs on both.
        let false_bin = ["/bin/false", "/usr/bin/false"]
            .iter()
            .copied()
            .find(|p| std::path::Path::new(p).exists())
            .expect("a `false` binary must exist for this test");
        let err = openspec_preflight_with(false_bin).expect_err("nonzero exit must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("exited"), "error must mention exit code: {msg}");
    }

    /// Build a remote + workspace clone pair. The workspace has `origin`
    /// pointing at the remote, so `git fetch` succeeds during the startup
    /// check.
    fn workspace_pair() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let remote = dir.path().join("remote");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "-q", "-b", "main"]);
        run_git(&remote, &["config", "user.email", "test@example.com"]);
        run_git(&remote, &["config", "user.name", "test"]);
        std::fs::write(remote.join("README.md"), "x").unwrap();
        run_git(&remote, &["add", "README.md"]);
        run_git(&remote, &["commit", "-q", "-m", "initial"]);

        let parent = workspace.parent().unwrap();
        let st = Command::new("git")
            .args(["clone", "-q", remote.to_string_lossy().as_ref(),
                   workspace.to_string_lossy().as_ref()])
            .current_dir(parent)
            .status()
            .unwrap();
        assert!(st.success(), "clone failed");
        run_git(&workspace, &["config", "user.email", "test@example.com"]);
        run_git(&workspace, &["config", "user.name", "test"]);
        (dir, workspace)
    }

    fn dirty_workspace_fixture() -> (TempDir, PathBuf) {
        let (dir, path) = workspace_pair();
        // Untracked file → status --porcelain non-empty → dirty.
        std::fs::write(path.join("LEFTOVER.txt"), "stale\n").unwrap();
        (dir, path)
    }

    fn clean_workspace_fixture() -> (TempDir, PathBuf) {
        workspace_pair()
    }

    fn cfg_with(local: PathBuf) -> RepositoryConfig {
        RepositoryConfig {
            url: format!("git@github.com:fixture/{}.git", local.file_name().unwrap().to_string_lossy()),
            local_path: Some(local),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Env-var mutation is global; serialize the startup-validation tests
    /// that touch real env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn repo(url: &str) -> RepositoryConfig {
        RepositoryConfig {
            url: url.into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    #[tokio::test]
    async fn ensure_forks_exist_skipped_in_direct_push_mode() {
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        // No repos to validate; no fork_owner means the function returns Ok
        // without probing anything.
        let repos = vec![repo("git@github.com:any/repo.git")];
        ensure_forks_exist(&github, &repos)
            .await
            .expect("direct-push mode skips fork probing");
    }

    #[tokio::test]
    async fn ensure_forks_exist_errors_on_unsupported_url_scheme() {
        // Non-github URL combined with fork-PR mode → derive_fork_url
        // rejects → validation aggregates the failure.
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: Some("machine-user".into()),
            recreate_fork_on_reinit: false,
        };
        let repos = vec![repo("ssh://git@github.com/upstream/repo.git")];
        let err = ensure_forks_exist(&github, &repos)
            .await
            .expect_err("unsupported URL scheme must error in fork mode");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("upstream/repo.git"),
            "error must name the offending URL; got: {msg}"
        );
    }

    #[test]
    fn startup_fails_when_no_token_route() {
        // Two repos: one has a matching owner_tokens entry whose env var
        // is set; the other has no entry AND `token_env`'s named env var
        // is unset. The aggregated error must name the unmappable repo.
        let _g = ENV_LOCK.lock().unwrap();
        let covered_var = "AUTOCODER_TEST_STARTUP_COVERED";
        let fallback_var = "AUTOCODER_TEST_STARTUP_FALLBACK_UNSET";
        unsafe {
            std::env::set_var(covered_var, "ok");
            std::env::remove_var(fallback_var);
        }

        let mut map = HashMap::new();
        map.insert(
            "covered-org".into(),
            crate::config::SecretSource::EnvVar(covered_var.into()),
        );
        let github = GithubConfig {
            token_env: fallback_var.into(),
            token: None,
            owner_tokens: Some(map),
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let repos = vec![
            repo("git@github.com:covered-org/repo-a.git"),
            repo("git@github.com:other-org/repo-b.git"),
        ];

        let err = validate_github_token_routes(&github, &repos)
            .expect_err("must fail when a repo has no route");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("other-org/repo-b.git"),
            "error must name the unmappable repo URL; got: {msg}"
        );
        assert!(
            msg.contains(fallback_var),
            "error must name the unset fallback env var; got: {msg}"
        );
        assert!(
            !msg.contains("covered-org/repo-a.git"),
            "error must not include the successfully-routed repo; got: {msg}"
        );

        unsafe { std::env::remove_var(covered_var) };
    }

    #[test]
    fn startup_passes_with_inline_owner_token_and_no_env() {
        // No env vars set for either the owner-specific source or the
        // fallback; both routes resolved entirely via inline values.
        let _g = ENV_LOCK.lock().unwrap();
        let mut map = HashMap::new();
        map.insert(
            "fixture-org".into(),
            crate::config::SecretSource::Inline {
                value: "inline-org-pat".into(),
            },
        );
        let github = GithubConfig {
            token_env: "AUTOCODER_TEST_INLINE_ROUTE_FALLBACK_NEVER_SET".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "inline-fallback-pat".into(),
            }),
            owner_tokens: Some(map),
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        let repos = vec![
            repo("git@github.com:fixture-org/repo.git"),    // owner_tokens hit
            repo("git@github.com:uncovered-org/repo.git"),  // fallback to github.token inline
        ];
        validate_github_token_routes(&github, &repos)
            .expect("both repos should resolve via inline sources");
    }

    #[test]
    fn startup_passes_when_every_repo_has_a_route() {
        let _g = ENV_LOCK.lock().unwrap();
        let personal_var = "AUTOCODER_TEST_STARTUP_PERSONAL";
        let fallback_var = "AUTOCODER_TEST_STARTUP_FALLBACK_SET";
        unsafe {
            std::env::set_var(personal_var, "personal-secret");
            std::env::set_var(fallback_var, "fallback-secret");
        }

        let mut map = HashMap::new();
        map.insert(
            "rabbeverly".into(),
            crate::config::SecretSource::EnvVar(personal_var.into()),
        );
        let github = GithubConfig {
            token_env: fallback_var.into(),
            token: None,
            owner_tokens: Some(map),
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };

        let repos = vec![
            repo("git@github.com:rabbeverly/personal-repo.git"),
            repo("git@github.com:some-org/work-repo.git"),
        ];

        validate_github_token_routes(&github, &repos)
            .expect("both repos should resolve: one via owner_tokens, one via fallback");

        unsafe {
            std::env::remove_var(personal_var);
            std::env::remove_var(fallback_var);
        }
    }

    /// A workspace dirty at startup (residue from a prior failed run) is
    /// auto-recovered via `git reset --hard origin/<base>` + `git clean
    /// -fd`. After recovery the workspace is clean and the startup check
    /// returns true.
    #[test]
    fn dirty_workspace_recovers_at_startup() {
        let (_dirty, dirty_path) = dirty_workspace_fixture();
        // Sanity: fixture really is dirty before the check.
        let before = git::status_porcelain(&dirty_path).unwrap();
        assert!(!before.is_empty(), "fixture must start dirty");

        let dirty_repo = cfg_with(dirty_path.clone());
        let direct_push_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        assert!(
            repo_passes_startup_check(&dirty_repo, &direct_push_github),
            "dirty workspace must auto-recover and pass the startup check"
        );

        // After recovery the workspace is clean.
        let after = git::status_porcelain(&dirty_path).unwrap();
        assert!(after.is_empty(), "workspace must be clean after recovery, got: {after}");
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn startup_logs_info_for_slack() {
        emit_chatops_startup_log("slack", false);
        assert!(logs_contain("ChatOps escalation enabled via slack"));
        assert!(logs_contain("officially supported"));
        assert!(!logs_contain("EXPERIMENTAL"));
        assert!(!logs_contain("best-effort"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn startup_logs_experimental_warning_for_discord() {
        emit_chatops_startup_log("discord", true);
        assert!(logs_contain("EXPERIMENTAL"));
        assert!(logs_contain("best-effort"));
        assert!(logs_contain("discord"));
    }

    #[test]
    fn startup_version_message_includes_version_and_repo_count() {
        let msg = startup_version_message("1.2.3", 3);
        assert!(msg.starts_with("🆙 "), "must start with the 🆙 prefix: {msg}");
        assert!(msg.contains("autocoder v1.2.3"), "must contain version: {msg}");
        assert!(
            msg.contains("3 repository(ies) configured"),
            "must contain repo count: {msg}"
        );
    }

    #[tokio::test]
    async fn dispatch_startup_notification_posts_one_message_when_chatops_configured() {
        let backend = Arc::new(crate::audits::test_support::RecordingBackend::new());
        let slot = ChatOpsSlot {
            backend: backend.clone() as Arc<dyn crate::chatops::ChatOpsBackend>,
            default_channel_id: "C_DEFAULT".to_string(),
            start_work_enabled: false,
            failure_alerts_enabled: false,
            pr_opened_enabled: false,
        };
        dispatch_startup_notification(Some(&slot), 3).await;
        let calls = backend.calls();
        assert_eq!(calls.len(), 1, "exactly one post_notification call expected");
        assert_eq!(calls[0].channel, "C_DEFAULT");
        assert!(
            calls[0].text.contains(&format!("autocoder v{}", env!("CARGO_PKG_VERSION"))),
            "message must name CARGO_PKG_VERSION: {}",
            calls[0].text
        );
        assert!(
            calls[0].text.contains("3 repository(ies) configured"),
            "message must include repo count: {}",
            calls[0].text
        );
        assert!(
            calls[0].text.starts_with("🆙 "),
            "message must begin with the 🆙 prefix: {}",
            calls[0].text
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn dispatch_startup_notification_logs_info_when_no_chatops() {
        dispatch_startup_notification(None, 2).await;
        assert!(
            logs_contain("startup version"),
            "INFO log must mention startup version"
        );
        assert!(
            logs_contain("2 repositories"),
            "INFO log must include repo count"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn dispatch_startup_notification_failure_is_non_fatal() {
        let backend = Arc::new(crate::audits::test_support::RecordingBackend::failing(
            "simulated chatops failure",
        ));
        let slot = ChatOpsSlot {
            backend: backend.clone() as Arc<dyn crate::chatops::ChatOpsBackend>,
            default_channel_id: "C_DEFAULT".to_string(),
            start_work_enabled: false,
            failure_alerts_enabled: false,
            pr_opened_enabled: false,
        };
        dispatch_startup_notification(Some(&slot), 1).await;
        assert!(
            logs_contain("startup version notification failed"),
            "WARN log must name the failure"
        );
    }

    #[test]
    fn clean_workspace_still_passes_startup() {
        let (_clean, clean_path) = clean_workspace_fixture();
        let clean_repo = cfg_with(clean_path);
        let direct_push_github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        };
        assert!(repo_passes_startup_check(&clean_repo, &direct_push_github),
            "clean workspace must pass startup check");
    }
}

fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };

        #[cfg(unix)]
        let terminate = async {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(e) => {
                    tracing::warn!("could not install SIGTERM handler: {e}");
                    std::future::pending::<()>().await;
                }
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            () = ctrl_c => tracing::info!("received SIGINT; shutting down"),
            () = terminate => tracing::info!("received SIGTERM; shutting down"),
        }
        cancel.cancel();
    });
}
