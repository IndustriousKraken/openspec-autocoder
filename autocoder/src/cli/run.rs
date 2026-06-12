//! `autocoder run` — daemon entry point. Spawns one polling task per
//! configured repository and waits for shutdown signal (SIGINT/SIGTERM) or
//! all tasks to finish.

use crate::audits::{
    AuditRegistry,
    architecture_consultative::ArchitectureConsultativeAudit,
    brightline::ArchitectureBrightlineAudit,
    canon_consolidation::CanonConsolidationAudit,
    canon_contradiction::CanonContradictionAudit,
    documentation_audit::DocumentationAudit,
    drift::DriftAudit,
    missing_tests::MissingTestsAudit, security_bug::SecurityBugAudit,
};
use crate::chatops;
use crate::code_reviewer::CodeReviewer;
use crate::config::{
    AuditSettings, AuditsConfig, Config, ContradictionCheckMode, ExecutorKind, GithubConfig,
    NotificationsConfig, RepositoryConfig, clamp_max_audits_per_iteration,
    validate_audit_type_names,
};
use crate::control_socket::{
    self, CacheHolder, ChatOpsHolder, ChatOpsSlot, ControlState, GithubHolder, RepoTaskHandle,
    RepoTaskMap, ReviewerHolder, SpawnOutcome, SpawnRepoFn,
};
use crate::executor::{Executor, claude_cli::ClaudeCliExecutor};
use crate::github::parse_repo_url;
use crate::github_credentials::resolve_token_with_source;
use crate::{alert_state_migration, git, migration, paths, polling_loop, workspace};
use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub async fn execute(mut cfg: Config, config_path: PathBuf) -> Result<()> {
    // Resolve the daemon paths via the env-driven resolver, then wrap
    // in an `Arc<DaemonPaths>` AND thread the value explicitly into
    // every consumer (per the canonical `Production paths SHALL be
    // threaded` requirement). The single `Arc` is constructed here
    // exactly once per daemon process AND handed by clone into the
    // top-level orchestrator types (`ClaudeCliExecutor`, `ControlState`,
    // `polling_loop::run`).
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
    let daemon_paths: Arc<paths::DaemonPaths> = Arc::new(daemon_paths);

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

    // Migrate any pre-existing workspace-rooted `.alert-state.json`
    // files into the new `<state_dir>/alert-state/<basename>.json`
    // layout (per `a16`). Errors are per-repo AND non-fatal; the
    // marker is only written when every repo's outcome was clean. A
    // failed repo retries on the next daemon start.
    if let Err(e) = alert_state_migration::migrate_alert_state_from_workspace(
        &daemon_paths,
        &cfg.repositories,
        &cfg.github,
    ) {
        tracing::error!(
            "alert-state migration: scan errored (daemon continues): {e:#}"
        );
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

    // a011: comprehensive dependency preflight — extends the historical
    // openspec-only check to the full set (openspec, git, a usable sandbox
    // mechanism, every configured strategy's CLI, scout/RAG backends),
    // reporting ALL missing dependencies together and aborting startup only
    // when a required one is missing or unusable.
    crate::dependency_preflight::run_startup_preflight(&cfg)?;

    // Tool-capability probe: the agentic gates/reviewer require their model to
    // emit tool calls (Read the change, then call a submit_* MCP tool). Probe
    // each agentic registry endpoint once and WARN if it cannot — so a toolless
    // model (older family, abliterated template) is surfaced now, not via a
    // cryptic fail-closed hold mid-run. Best-effort; never blocks startup.
    crate::tool_probe::run_tool_capability_preflight(&cfg).await;

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
    // NOTE: fork-existence setup is deliberately deferred until AFTER the
    // chatops backend is initialized (see below), so a per-repo fork-setup
    // failure can be alerted through chatops rather than aborting startup
    // (`fork-setup-failure-degrades-gracefully`).

    let executor: Arc<dyn Executor> = match cfg.executor.kind {
        ExecutorKind::ClaudeCli => Arc::new(
            ClaudeCliExecutor::from_config(&cfg.executor, daemon_paths.clone())
                .context("initializing ClaudeCliExecutor from config")?,
        ),
    };

    // a006: detect the OS-level sandbox mechanism once at startup AND seed the
    // daemon-global sandbox context. After this, every `agentic_run` spawn is
    // gated + wrapped (the executor, every audit, the agentic reviewer, the
    // contradiction checks). With no mechanism AND no opt-in, agentic runs
    // fail closed at the spawn seam; with the explicit opt-in, they proceed
    // unsandboxed and the loud WARN below fires.
    let sandbox_mechanism = crate::sandbox::detect_mechanism();
    let sandbox_global = cfg.executor.sandbox.as_ref();
    let allow_unsandboxed = sandbox_global.map(|s| s.allow_unsandboxed).unwrap_or(false);
    let global_sandbox_toggles = crate::config::SandboxToggles {
        os_hide: sandbox_global.and_then(|s| s.os_hide).unwrap_or(true),
        engine_deny: sandbox_global.and_then(|s| s.engine_deny).unwrap_or(true),
        strict_mode: sandbox_global.and_then(|s| s.strict_mode).unwrap_or(false),
        mask_add: sandbox_global.and_then(|s| s.mask_add.clone()).unwrap_or_default(),
        mask_remove: sandbox_global.and_then(|s| s.mask_remove.clone()).unwrap_or_default(),
    };
    match sandbox_mechanism {
        Some(m) => tracing::info!(
            mechanism = m.as_str(),
            "OS-level agentic sandbox active (every agentic subprocess is kernel-wrapped)"
        ),
        None if allow_unsandboxed => {}
        None => tracing::info!(
            "no OS sandbox mechanism (systemd-run / bwrap) detected; agentic runs will fail closed unless executor.sandbox.allow_unsandboxed is set"
        ),
    }
    if let Some(warn) =
        crate::sandbox::startup_unsandboxed_warning(sandbox_mechanism, allow_unsandboxed)
    {
        tracing::warn!("{warn}");
    }
    crate::sandbox::init_global(sandbox_mechanism, allow_unsandboxed, global_sandbox_toggles);

    // a014: capture the operator's ACTIVATED login-shell environment and seed
    // it (credential-filtered) for every agentic subprocess, so shell-init-
    // activated toolchains (pyenv/rbenv/poetry/nvm) are usable — not merely
    // present on disk under a013's exposed home. Best-effort and time-bounded:
    // a failed/partial capture degrades to the base environment and never
    // aborts startup. The credential filter (defaults + operator edits) keeps
    // shell-exported secrets AND provider API keys out of the subprocess.
    let agent_env_cfg = cfg.executor.agent_env.clone().unwrap_or_default();
    if agent_env_cfg.capture_enabled() {
        let filter = crate::agent_env::CredentialFilter::from_edits(
            agent_env_cfg.exclude_add.as_ref(),
            agent_env_cfg.exclude_remove.as_ref(),
        );
        let captured = crate::agent_env::capture_login_shell(&filter).await;
        if captured.is_empty() {
            tracing::warn!(
                "a014: login-shell environment capture produced nothing; agentic \
                 subprocesses run against the daemon's base environment (toolchains \
                 activated only by shell init may not resolve — see `autocoder doctor`)"
            );
        } else {
            tracing::info!(
                propagated = captured.len(),
                excluded = captured.excluded_count(),
                "a014: captured operator login-shell environment for agentic subprocesses \
                 (credential variables withheld)"
            );
        }
        crate::agent_env::init_captured_env(captured);
    } else {
        tracing::info!(
            "a014: login-shell environment capture disabled (executor.agent_env.capture: \
             false); agentic subprocesses use the daemon's base environment"
        );
    }

    // Build the change-internal contradiction pre-flight context
    // (a19). Disabled by default → no LLM client built, no context
    // produced; the polling loop short-circuits at the
    // `change_contradiction::current()` read. Enabled-without-LLM-config
    // already failed validation above, so an Enabled config here is
    // guaranteed to carry an LLM block.
    let contradiction_ctx: Option<
        Arc<crate::preflight::change_contradiction::ContradictionCheckCtx>,
    > = if matches!(
        cfg.executor.change_internal_contradiction_check,
        ContradictionCheckMode::Enabled
    ) {
        let llm_cfg = cfg
            .executor
            .change_internal_contradiction_check_llm
            .as_ref()
            .expect("validate_config guarantees the llm block is set when enabled");
        // a59: resolve the model into the a56 `(provider, model, base, key)`
        // tuple the `claude` CLI strategy translates into `ANTHROPIC_*`. The
        // contradiction check now runs agentically through `agentic_run`
        // rather than over HTTP, so no `LlmClient` is built.
        let model = crate::llm::resolve_contradiction_check_model(llm_cfg)
            .context("resolving contradiction-check model from config")?;
        // a003: the contradiction check runs agentically (a59) through a CLI
        // strategy, which authenticates from its own login — so a configured
        // `api_key` is unused. Warn once at startup; the strategy ignores it.
        if let Some(warn) = crate::agentic_run::cli_role_key_exposure_warning(
            "executor.change_internal_contradiction_check_llm",
            !model.api_key.is_empty(),
        ) {
            tracing::warn!("{warn}");
        }
        let prompt_template = crate::preflight::change_contradiction::load_prompt_template(
            cfg.executor
                .change_internal_contradiction_check_prompt_path
                .as_deref(),
        )
        .context("loading change-contradiction-check prompt template")?;
        tracing::info!(
            provider = ?llm_cfg.provider,
            model = llm_cfg.model.as_str(),
            "change-internal contradiction pre-flight enabled (a19; agentic transport a59)"
        );
        let attribution =
            crate::attribution::AttributionSurface::attribution(llm_cfg);
        Some(Arc::new(
            crate::preflight::change_contradiction::ContradictionCheckCtx {
                command: crate::config::resolve_cli_command(
                    &cfg.executor.command,
                    crate::config::default_cli_for(model.provider),
                ),
                model,
                prompt_template,
                attribution: Some(attribution),
                retries: cfg.executor.verifier_gate_retries,
                #[cfg(test)]
                test_submission: None,
            },
        ))
    } else {
        tracing::info!(
            "change-internal contradiction pre-flight disabled (a19; opt-in via executor.change_internal_contradiction_check)"
        );
        None
    };

    // Build the change-vs-canonical contradiction pre-flight context — the
    // `[canon]` gate (a62). Disabled by default → no context produced; the
    // polling loop short-circuits at the `canon_contradiction::current()`
    // read. Enabled-without-LLM-config already failed validation above.
    let canon_contradiction_ctx: Option<
        Arc<crate::preflight::canon_contradiction::CanonContradictionCheckCtx>,
    > = if matches!(
        cfg.executor.change_canonical_contradiction_check,
        ContradictionCheckMode::Enabled
    ) {
        let llm_cfg = cfg
            .executor
            .change_canonical_contradiction_check_llm
            .as_ref()
            .expect("validate_config guarantees the llm block is set when enabled");
        let model = crate::llm::resolve_canon_contradiction_check_model(llm_cfg)
            .context("resolving canon-contradiction-check model from config")?;
        // a003: agentic CLI gate (a62) — a configured `api_key` is unused (the
        // CLI authenticates itself). Warn once at startup; the strategy ignores it.
        if let Some(warn) = crate::agentic_run::cli_role_key_exposure_warning(
            "executor.change_canonical_contradiction_check_llm",
            !model.api_key.is_empty(),
        ) {
            tracing::warn!("{warn}");
        }
        let prompt_template = crate::preflight::canon_contradiction::load_prompt_template(
            cfg.executor
                .change_canonical_contradiction_check_prompt_path
                .as_deref(),
        )
        .context("loading change-vs-canonical-check prompt template")?;
        tracing::info!(
            provider = ?llm_cfg.provider,
            model = llm_cfg.model.as_str(),
            "change-vs-canonical contradiction pre-flight enabled (the [canon] gate, a62)"
        );
        let attribution = crate::attribution::AttributionSurface::attribution(llm_cfg);
        Some(Arc::new(
            crate::preflight::canon_contradiction::CanonContradictionCheckCtx {
                command: crate::config::resolve_cli_command(
                    &cfg.executor.command,
                    crate::config::default_cli_for(model.provider),
                ),
                model,
                prompt_template,
                attribution: Some(attribution),
                retries: cfg.executor.verifier_gate_retries,
                #[cfg(test)]
                test_submission: None,
            },
        ))
    } else {
        tracing::info!(
            "change-vs-canonical contradiction pre-flight disabled (the [canon] gate, a62; opt-in via executor.change_canonical_contradiction_check)"
        );
        None
    };

    // Build the code-implements-spec verification context — the `[out]` gate
    // (a63). Disabled by default → no context produced; the polling loop
    // short-circuits at the `code_implements_spec::current()` read post-
    // executor. Enabled-without-LLM-config already failed validation above.
    let code_implements_spec_ctx: Option<
        Arc<crate::code_implements_spec::CodeImplementsSpecCheckCtx>,
    > = if matches!(
        cfg.executor.code_implements_spec_check,
        ContradictionCheckMode::Enabled
    ) {
        let llm_cfg = cfg
            .executor
            .code_implements_spec_check_llm
            .as_ref()
            .expect("validate_config guarantees the llm block is set when enabled");
        let model = crate::llm::resolve_code_implements_spec_check_model(llm_cfg)
            .context("resolving code-implements-spec-check model from config")?;
        // a003: agentic CLI gate (a63) — a configured `api_key` is unused (the
        // CLI authenticates itself). Warn once at startup; the strategy ignores it.
        if let Some(warn) = crate::agentic_run::cli_role_key_exposure_warning(
            "executor.code_implements_spec_check_llm",
            !model.api_key.is_empty(),
        ) {
            tracing::warn!("{warn}");
        }
        let prompt_template = crate::code_implements_spec::load_prompt_template(
            cfg.executor
                .code_implements_spec_check_prompt_path
                .as_deref(),
        )
        .context("loading code-implements-spec-check prompt template")?;
        tracing::info!(
            provider = ?llm_cfg.provider,
            model = llm_cfg.model.as_str(),
            "code-implements-spec verification enabled (the [out] gate, a63)"
        );
        let attribution = crate::attribution::AttributionSurface::attribution(llm_cfg);
        Some(Arc::new(
            crate::code_implements_spec::CodeImplementsSpecCheckCtx {
                command: crate::config::resolve_cli_command(
                    &cfg.executor.command,
                    crate::config::default_cli_for(model.provider),
                ),
                model,
                prompt_template,
                attribution: Some(attribution),
                retries: cfg.executor.verifier_gate_retries,
                #[cfg(test)]
                test_submission: None,
            },
        ))
    } else {
        tracing::info!(
            "code-implements-spec verification disabled (the [out] gate, a63; opt-in via executor.code_implements_spec_check)"
        );
        None
    };

    // Build the issues-lane context — the second work lane (a009). OFF by
    // default: a context is produced ONLY when `features.issues.enabled`,
    // so the polling pass short-circuits at the `lanes::gate::current()`
    // read AND `issues/<slug>/` directories are not worked.
    let issues_ctx: Option<Arc<crate::lanes::gate::IssuesLaneContext>> =
        if cfg.features.issues.enabled {
            tracing::info!(
                "issues lane enabled (a009; precedence issues > changes > audits)"
            );
            Some(Arc::new(crate::lanes::gate::IssuesLaneContext {
                prompt_path: cfg.features.issues.prompt_path.clone(),
                // Hybrid PUBLIC ingestion (a010) is gated behind the
                // existing scout issue-read opt-in. With the issues lane on
                // AND `features.scout.include_issues` true, the bot triages
                // reported GitHub issues read-only AND posts candidates.
                ingest: cfg.features.scout.include_issues,
            }))
        } else {
            None
        };

    let reviewer_initial: Option<Arc<CodeReviewer>> = match cfg.reviewer.as_ref() {
        Some(rcfg) if rcfg.enabled => {
            let r = CodeReviewer::from_config(rcfg)
                .context("initializing code reviewer from config")?;
            // a64: when the effective kind is `agentic` but the resolved
            // reviewer CLI is unavailable on this host, degrade to the
            // `oneshot` HTTP path for the boot (one loud WARN logged inside).
            let r = crate::code_reviewer::apply_startup_cli_fallback(r);
            tracing::info!(
                provider = ?rcfg.provider,
                model = rcfg.model.as_str(),
                kind = ?r.kind(),
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

    // Fork-PR mode: ensure each repository has a reachable fork BEFORE
    // spawning its polling task. Per `fork-setup-failure-degrades-gracefully`,
    // a per-repo failure no longer aborts startup — the repository is skipped
    // for the process lifetime (no polling task) AND a chatops alert is
    // emitted. The chatops backend was initialized above, so the alert is
    // deliverable. The daemon stays up serving every other repository AND
    // chatops, even when every configured repository fails fork setup.
    let fork_setup_failures = ensure_forks_exist(&cfg.github, &cfg.repositories).await;
    let skip_fork_urls: std::collections::HashSet<String> = fork_setup_failures
        .iter()
        .map(|f| f.upstream_url.clone())
        .collect();
    if !fork_setup_failures.is_empty() {
        tracing::warn!(
            count = fork_setup_failures.len(),
            "fork-PR mode: {} repository(ies) skipped for the process lifetime after \
             fork setup failed; the daemon and other repositories keep running",
            fork_setup_failures.len()
        );
        for f in &fork_setup_failures {
            tracing::error!(
                url = %f.upstream_url,
                fork_url = f.fork_url.as_deref().unwrap_or(""),
                "fork setup failed (repository skipped for the process lifetime): {}",
                f.cause
            );
        }
    }
    alert_fork_setup_failures(chatops_initial.as_ref(), &fork_setup_failures).await;

    // Hot-swappable holders. The control socket swaps into these on
    // `autocoder reload`; the polling loops read snapshots once per pass.
    let github_holder: GithubHolder = Arc::new(ArcSwap::from_pointee(cfg.github.clone()));
    let reviewer_holder: ReviewerHolder = Arc::new(ArcSwap::from_pointee(reviewer_initial));
    let chatops_holder: ChatOpsHolder = Arc::new(ArcSwap::from_pointee(chatops_initial));
    let cache_holder: CacheHolder = Arc::new(ArcSwap::from_pointee(cfg.cache.clone()));

    // a65: one-time startup notice when the workspace cache is unbounded
    // (`cache.workspaces_max_gb` unset, the default). Surfaces the
    // unbounded-growth failure mode — and the field that bounds it —
    // before it can wedge a disk. `execute` runs exactly once per daemon
    // boot, so this WARN is emitted at most once per process lifetime.
    if let Some(notice) =
        crate::config::workspace_cache_unbounded_notice(cfg.cache.workspaces_max_gb)
    {
        tracing::warn!("{notice}");
    }

    for repo in &cfg.repositories {
        let derived = workspace::resolve_path(&daemon_paths, repo);
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

    // a006: per-repository relaxed-sandbox-posture WARN. Naming each
    // credential-protection toggle that is OFF for the repo (per-repo override
    // or global). Both ON (the secure default) emits nothing — loosening is
    // explicit and logged so the operator notices.
    for repo in &cfg.repositories {
        if let Some(warn) = repo.relaxed_sandbox_warning(cfg.executor.sandbox.as_ref()) {
            tracing::warn!(url = %repo.url, "{warn}");
        }
    }

    // Per-PR automatic-revision cap. Values above the ceiling are clamped
    // down in `max_auto_revisions_per_pr_clamped()`; we WARN once here so
    // the operator notices the bogus value.
    if cfg.executor.max_auto_revisions_per_pr > crate::config::MAX_AUTO_REVISIONS_PER_PR_CEILING {
        tracing::warn!(
            configured = cfg.executor.max_auto_revisions_per_pr,
            ceiling = crate::config::MAX_AUTO_REVISIONS_PER_PR_CEILING,
            "executor.max_auto_revisions_per_pr is set above the ceiling; clamping (a runaway reviewer-driven revision chain would otherwise burn tokens — fix your config)"
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
    registry.register(Arc::new(DocumentationAudit::new(
        &audit_settings,
        &cfg.executor,
    )));
    registry.register(Arc::new(CanonContradictionAudit::new(
        &audit_settings,
        &cfg.executor,
        &daemon_paths,
    )));
    // a76: the canon-consolidation audit drafts a `consolidate-` change
    // (OpenSpecOnly) merging redundant requirements — the overlap twin of
    // the contradiction audit's conflict scan.
    registry.register(Arc::new(CanonConsolidationAudit::new(
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
    let revision_cap = cfg.executor.max_auto_revisions_per_pr_clamped();
    let human_revise_cap = cfg.executor.max_revise_triggers_per_pr;
    let startup_jitter_max_secs = cfg.executor.startup_jitter_max_secs();
    let inter_iteration_jitter_pct = cfg.executor.inter_iteration_jitter_pct();
    let spawn_repo = build_spawn_repo_fn(SpawnDeps {
        paths: daemon_paths.clone(),
        executor: executor.clone(),
        github_holder: github_holder.clone(),
        reviewer_holder: reviewer_holder.clone(),
        chatops_holder: chatops_holder.clone(),
        cache_holder: cache_holder.clone(),
        stuck_threshold_secs,
        perma_stuck_threshold,
        executor_max_changes_per_pr,
        revision_cap,
        human_revise_cap,
        startup_jitter_max_secs,
        inter_iteration_jitter_pct,
        audit_registry: audit_registry.clone(),
        audits_cfg: audits_cfg_arc.clone(),
        audit_settings: audit_settings_arc.clone(),
        contradiction_ctx: contradiction_ctx.clone(),
        canon_contradiction_ctx: canon_contradiction_ctx.clone(),
        code_implements_spec_ctx: code_implements_spec_ctx.clone(),
        issues_ctx: issues_ctx.clone(),
        global_cancel: cancel.clone(),
        task_map: task_map.clone(),
        task_map_changed: task_map_changed.clone(),
    });

    for repo in cfg.repositories.iter().cloned() {
        if skip_fork_urls.contains(&repo.url) {
            // Fork setup failed for this repository at startup; skip it for
            // the process lifetime (no polling task). The chatops alert was
            // already emitted above. Other repositories continue normally.
            continue;
        }
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
    let canonical_rag_registry = crate::rag::CanonicalRagRegistry::new();
    // Publish the registry + config to the process-global so the polling
    // loop's RAG hooks AND the per-execution MCP child can both reach
    // them without threading every-call-site plumbing. Set BEFORE the
    // control socket binds so the daemon's `query_canonical_specs`
    // handler observes a consistent view.
    if let Some(rag_cfg) = cfg.canonical_rag.as_ref().filter(|c| c.is_active()) {
        crate::rag::set_shared(canonical_rag_registry.clone(), rag_cfg.clone());
        // Set the control-socket env var so `ClaudeCliExecutor::write_mcp_config`
        // picks it up when writing the per-execution `.mcp.json`.
        let socket = crate::control_socket::socket_path(&daemon_paths);
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
        }
    }
    // Per-execution outcome store (a27a0). Always constructed regardless
    // of whether canonical_rag is configured; the per-execution MCP
    // child uses the same control socket for `record_outcome` AND the
    // classifier drains via `consume_outcome` after subprocess exit.
    // The control-socket env var is required for the MCP child to relay
    // outcome tools, independent of canonical_rag. Set it unconditionally
    // if not already set by the canonical_rag block above.
    if std::env::var(crate::mcp_askuser_server::ENV_CONTROL_SOCKET).is_err() {
        let socket = crate::control_socket::socket_path(&daemon_paths);
        // SAFETY: daemon startup is single-threaded at this point; we
        // are the sole writer to the process env.
        unsafe {
            std::env::set_var(
                crate::mcp_askuser_server::ENV_CONTROL_SOCKET,
                socket.as_os_str(),
            );
        }
    }
    let control_state = ControlState {
        github: github_holder.clone(),
        reviewer: reviewer_holder.clone(),
        chatops: chatops_holder.clone(),
        cache: cache_holder.clone(),
        last_config: Arc::new(ArcSwap::from_pointee(cfg.clone())),
        config_path,
        repo_tasks: task_map.clone(),
        repo_tasks_changed: task_map_changed.clone(),
        spawn_repo: spawn_repo.clone(),
        canonical_rag_registry: canonical_rag_registry.clone(),
        outcome_store: crate::outcome_store::OutcomeStore::new(),
        submission_store: crate::submission_store::SubmissionStore::new(),
        paths: daemon_paths.clone(),
    };
    // a57: register the advisory audits' `submit_findings` payload schemas
    // on the shared submission store BEFORE the listener starts handling
    // `record_submission`, so the MCP child's submissions are validated
    // against the role's finding schema.
    crate::audits::register_submission_schemas(&control_state.submission_store);
    // a58: register the agentic reviewer's `submit_review` payload schema on
    // the same store so the reviewer MCP child's submissions are validated.
    crate::code_reviewer::register_reviewer_submission_schema(
        &control_state.submission_store,
    );
    // a59: register the contradiction check's `submit_contradictions` payload
    // schema on the same store so its MCP child's submissions are validated.
    crate::preflight::change_contradiction::register_contradiction_submission_schema(
        &control_state.submission_store,
    );
    // a62: register the `[canon]` gate's `submit_canon_contradictions` payload
    // schema on the same store so its MCP child's submissions are validated.
    crate::preflight::canon_contradiction::register_canon_contradiction_submission_schema(
        &control_state.submission_store,
    );
    // a63: register the `[out]` gate's `submit_verdict` payload schema on the
    // same store so its MCP child's submissions are validated.
    crate::code_implements_spec::register_code_implements_spec_submission_schema(
        &control_state.submission_store,
    );
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
        daemon_paths.clone(),
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
    daemon_paths: Arc<paths::DaemonPaths>,
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
        crate::chatops::operator_commands::OperatorCommandDispatcher::new(&daemon_paths)
            .with_audit_types(
                audit_registry
                    .known_type_names()
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )
            .with_chatops(slot.backend.clone())
            .with_brownfield_enabled(cfg.features.brownfield.enabled)
            .with_scout_enabled(cfg.features.scout.enabled)
            .with_brownfield_survey_enabled(cfg.features.brownfield_survey.enabled)
            .with_workspace_resolver({
                let task_map_for_resolver = task_map.clone();
                let paths_for_resolver = daemon_paths.clone();
                move |url: &str| -> Option<std::path::PathBuf> {
                    let guard = task_map_for_resolver.lock().unwrap();
                    guard
                        .get(url)
                        .map(|h| crate::workspace::resolve_path(&paths_for_resolver, &h.config.load_full()))
                }
            }),
    );
    let task_map_for_provider = task_map.clone();
    let repos: Arc<dyn crate::chatops::operator_commands::RepoIdentityProvider> =
        Arc::new(crate::chatops::TaskMapRepoIdentities::new(daemon_paths.clone(), move || {
            let guard = task_map_for_provider.lock().unwrap();
            guard
                .values()
                .map(|h| h.config.load_full().as_ref().clone())
                .collect()
        }));
    let allowed_arc = Arc::new(allowed);

    let backend = slot.backend.clone();
    match backend
        .start_inbound_listener(daemon_paths, dispatcher, repos, allowed_arc, cancel)
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
    paths: Arc<paths::DaemonPaths>,
    executor: Arc<dyn Executor>,
    github_holder: GithubHolder,
    reviewer_holder: ReviewerHolder,
    chatops_holder: ChatOpsHolder,
    cache_holder: CacheHolder,
    stuck_threshold_secs: u64,
    perma_stuck_threshold: u32,
    executor_max_changes_per_pr: Option<u32>,
    revision_cap: u32,
    human_revise_cap: u32,
    startup_jitter_max_secs: u64,
    inter_iteration_jitter_pct: u8,
    audit_registry: Arc<AuditRegistry>,
    audits_cfg: Option<Arc<AuditsConfig>>,
    audit_settings: Arc<HashMap<String, AuditSettings>>,
    contradiction_ctx:
        Option<Arc<crate::preflight::change_contradiction::ContradictionCheckCtx>>,
    canon_contradiction_ctx:
        Option<Arc<crate::preflight::canon_contradiction::CanonContradictionCheckCtx>>,
    code_implements_spec_ctx:
        Option<Arc<crate::code_implements_spec::CodeImplementsSpecCheckCtx>>,
    issues_ctx: Option<Arc<crate::lanes::gate::IssuesLaneContext>>,
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
        if !repo_passes_startup_check(&deps.paths, &repo, &github_snap) {
            return SpawnOutcome::StartupCheckFailed;
        }
        let child_cancel = deps.global_cancel.child_token();
        let config_holder: Arc<ArcSwap<RepositoryConfig>> =
            Arc::new(ArcSwap::from_pointee(repo));
        let cancel_for_task = child_cancel.clone();
        let config_for_task = config_holder.clone();
        // Identity sentinel for the task's exit-path self-removal: the same
        // outer `Arc` the handle holds (config swaps via the inner ArcSwap do
        // NOT change this pointer), so the removal can confirm the entry under
        // this URL is still ours before removing it.
        let config_for_removal = config_holder.clone();
        let map_for_task = deps.task_map.clone();
        let map_changed_for_task = deps.task_map_changed.clone();
        let url_for_task = url.clone();
        let executor_for_task = deps.executor.clone();
        let github_for_task = deps.github_holder.clone();
        let reviewer_for_task = deps.reviewer_holder.clone();
        let chatops_for_task = deps.chatops_holder.clone();
        let cache_for_task = deps.cache_holder.clone();
        let stuck = deps.stuck_threshold_secs;
        let perma = deps.perma_stuck_threshold;
        let exec_max = deps.executor_max_changes_per_pr;
        let revision_cap_for_task = deps.revision_cap;
        let human_revise_cap_for_task = deps.human_revise_cap;
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
        let pending_brownfield_requests: Arc<
            std::sync::Mutex<
                std::collections::VecDeque<crate::control_socket::BrownfieldRequest>,
            >,
        > = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let pending_brownfield_requests_for_task = pending_brownfield_requests.clone();
        let pending_scout_requests: Arc<
            std::sync::Mutex<
                std::collections::VecDeque<crate::control_socket::ScoutRequest>,
            >,
        > = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let pending_scout_requests_for_task = pending_scout_requests.clone();
        let pending_spec_it_requests: Arc<
            std::sync::Mutex<
                std::collections::VecDeque<crate::control_socket::SpecItRequest>,
            >,
        > = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let pending_spec_it_requests_for_task = pending_spec_it_requests.clone();
        let pending_sync_upstream_requests: Arc<
            std::sync::Mutex<
                std::collections::VecDeque<crate::control_socket::SyncUpstreamRequest>,
            >,
        > = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let pending_sync_upstream_requests_for_task = pending_sync_upstream_requests.clone();
        let pending_brownfield_survey_requests: Arc<
            std::sync::Mutex<
                std::collections::VecDeque<crate::control_socket::BrownfieldSurveyRequest>,
            >,
        > = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let pending_brownfield_survey_requests_for_task =
            pending_brownfield_survey_requests.clone();
        let pending_brownfield_batch_requests: Arc<
            std::sync::Mutex<
                std::collections::VecDeque<crate::control_socket::BrownfieldBatchRequest>,
            >,
        > = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let pending_brownfield_batch_requests_for_task =
            pending_brownfield_batch_requests.clone();
        let iteration_cancel: Arc<std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>> =
            Arc::new(std::sync::Mutex::new(None));
        let iteration_cancel_for_task = iteration_cancel.clone();
        let iteration_drained: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
        let iteration_drained_for_task = iteration_drained.clone();
        let contradiction_ctx_for_task = deps.contradiction_ctx.clone();
        let canon_contradiction_ctx_for_task = deps.canon_contradiction_ctx.clone();
        let code_implements_spec_ctx_for_task = deps.code_implements_spec_ctx.clone();
        let issues_ctx_for_task = deps.issues_ctx.clone();
        let paths_for_task = deps.paths.clone();
        let join: JoinHandle<()> = tokio::spawn(async move {
            let fut = polling_loop::run(
                paths_for_task,
                config_for_task,
                executor_for_task,
                github_for_task,
                reviewer_for_task,
                chatops_for_task,
                cache_for_task,
                stuck,
                perma,
                exec_max,
                revision_cap_for_task,
                human_revise_cap_for_task,
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
                pending_brownfield_requests_for_task,
                pending_scout_requests_for_task,
                pending_spec_it_requests_for_task,
                pending_sync_upstream_requests_for_task,
                pending_brownfield_survey_requests_for_task,
                pending_brownfield_batch_requests_for_task,
                iteration_cancel_for_task,
                iteration_drained_for_task,
                cancel_for_task,
            );
            // Nest the verifier-gate scopes around the polling future: the two
            // pre-executor gates (a59 `[in]`, a62 `[canon]`) AND the
            // post-executor gate (a63 `[out]`). Each gate reads its own
            // task-local via `current()`; any may be `None` (disabled)
            // independently.
            let in_scoped =
                crate::preflight::change_contradiction::scope(contradiction_ctx_for_task, fut);
            let canon_scoped = crate::preflight::canon_contradiction::scope(
                canon_contradiction_ctx_for_task,
                in_scoped,
            );
            let out_scoped = crate::code_implements_spec::scope(
                code_implements_spec_ctx_for_task,
                canon_scoped,
            );
            // Issues lane gate (a009): bind the `features.issues` context
            // for the whole polling future; the pass reads it via
            // `lanes::gate::current()`. `None` (disabled) → lane inactive.
            crate::lanes::gate::scope(issues_ctx_for_task, out_scoped).await;
            {
                let mut guard = map_for_task.lock().unwrap();
                // Identity-guarded self-removal: remove ONLY the entry this task
                // owns (its config `Arc` sentinel). Today the spawn path refuses
                // to re-insert a still-present URL (`contains_key` →
                // AlreadyPresent), so a fresh handle never coexists with a
                // not-yet-exited cancelled task — but guarding locally keeps this
                // correct if that non-local invariant ever changes: a cancelled
                // task must never clobber a freshly respawned handle.
                if guard
                    .get(&url_for_task)
                    .is_some_and(|h| Arc::ptr_eq(&h.config, &config_for_removal))
                {
                    guard.remove(&url_for_task);
                }
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
                    pending_brownfield_requests,
                    pending_scout_requests,
                    pending_spec_it_requests,
                    pending_sync_upstream_requests,
                    pending_brownfield_survey_requests,
                    pending_brownfield_batch_requests,
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
    let version = env!("AUTOCODER_VERSION");
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

/// Resolve the daemon's config-file path when `autocoder run` is invoked
/// without an explicit `--config` (a011 task 3). An explicit path always wins
/// AND skips the systemd-unit lookup. Otherwise the installed
/// `autocoder.service` unit's `ExecStart` is parsed for the config argument
/// the daemon is launched with; absent a unit (or a recorded path), the
/// existing default-path resolution applies.
pub fn resolve_run_config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    resolve_run_config_path_inner(
        explicit,
        probe_unit_exec_start().as_deref(),
        &default_config_candidates(),
        &|p| p.exists(),
    )
}

/// Pure resolver behind [`resolve_run_config_path`]: every environment fact is
/// injected so the precedence (explicit → unit → defaults) is unit-testable.
fn resolve_run_config_path_inner(
    explicit: Option<PathBuf>,
    exec_start: Option<&str>,
    default_candidates: &[PathBuf],
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> Result<PathBuf> {
    // 1. An explicitly provided path always wins and never consults the unit.
    if let Some(p) = explicit {
        return Ok(p);
    }
    // 2. Discover the path the unit launches the daemon with.
    if let Some(es) = exec_start
        && let Some(p) = config_path_from_exec_start(es)
    {
        tracing::info!(path = %p.display(), "config path discovered from systemd unit ExecStart");
        return Ok(p);
    }
    // 3. Fall back to the existing default-path resolution.
    for cand in default_candidates {
        if exists(cand) {
            return Ok(cand.clone());
        }
    }
    Err(anyhow!(
        "no config path provided, none recorded in the systemd unit's ExecStart, \
         and no config file at the default locations ({}). \
         Pass `autocoder run --config <path>`.",
        default_candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The default config-file locations, checked in order: the server-mode path,
/// then the dev-mode XDG path.
fn default_config_candidates() -> Vec<PathBuf> {
    let mut cands = vec![PathBuf::from(crate::cli::install::DEFAULT_SERVER_CONFIG_PATH)];
    if let Some(home) = std::env::var_os("HOME") {
        cands.push(PathBuf::from(home).join(".config/autocoder/config.yaml"));
    }
    cands
}

/// Parse a systemd `ExecStart=` value for the daemon's config path. Matches
/// both `--config <file>` / `--config=<file>` AND `--config-dir <dir>` /
/// `--config-dir=<dir>` (from which the file is `<dir>/config.yaml`). Returns
/// `None` when neither flag carries a value.
pub fn config_path_from_exec_start(exec_start: &str) -> Option<PathBuf> {
    let tokens: Vec<&str> = exec_start.split_whitespace().collect();
    let mut iter = tokens.iter().peekable();
    while let Some(tok) = iter.next() {
        // `--config-dir <dir>` → `<dir>/config.yaml`.
        if *tok == "--config-dir" {
            if let Some(next) = iter.peek()
                && !next.starts_with("--")
            {
                return Some(PathBuf::from(next).join("config.yaml"));
            }
        } else if let Some(rest) = tok.strip_prefix("--config-dir=") {
            return Some(PathBuf::from(rest).join("config.yaml"));
        // `--config <file>` → `<file>` (checked after `--config-dir` so the
        // longer flag is not shadowed).
        } else if *tok == "--config" {
            if let Some(next) = iter.peek()
                && !next.starts_with("--")
            {
                return Some(PathBuf::from(next));
            }
        } else if let Some(rest) = tok.strip_prefix("--config=") {
            return Some(PathBuf::from(rest));
        }
    }
    None
}

/// Probe the installed `autocoder.service` unit for its `ExecStart=` line.
/// Returns `None` on any failure (no systemd, unit not found) — the resolver
/// then falls through to the default-path resolution.
fn probe_unit_exec_start() -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(["show", "autocoder.service", "-p", "ExecStart"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("ExecStart=").map(|r| r.to_string()))
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

/// One repository whose startup fork setup failed. Carries enough to
/// identify the repository in a chatops alert AND to log the precise cause.
/// Per `fork-setup-failure-degrades-gracefully`, a failure here no longer
/// aborts startup — the repository is skipped for the process lifetime and
/// an alert is emitted instead.
#[derive(Debug, Clone)]
pub struct ForkSetupFailure {
    /// The configured upstream repository URL.
    pub upstream_url: String,
    /// The derived fork URL, when derivation succeeded (`None` when the fork
    /// URL itself could not be derived).
    pub fork_url: Option<String>,
    /// Human-readable cause (HTTP status + body snippet, reachability
    /// timeout, PAT-routing failure, unsupported URL scheme, …).
    pub cause: String,
}

/// Fork-setup primitives the startup routine depends on, behind a trait so
/// tests can inject scripted reachability + creation outcomes without real
/// network (or a 60-second wall-clock wait). The production impl is
/// [`GitForkOps`].
#[async_trait::async_trait]
trait ForkOps: Send + Sync {
    /// True when `git ls-remote <fork_url> HEAD` succeeds right now.
    fn fork_reachable(&self, fork_url: &str) -> bool;
    /// Issue the fork-creation POST; `Ok(())` on 2xx.
    async fn create_fork(
        &self,
        upstream_owner: &str,
        upstream_repo: &str,
        token: &str,
    ) -> Result<()>;
}

/// Production [`ForkOps`]: real `git ls-remote` probe + real GitHub
/// fork-creation POST.
struct GitForkOps;

#[async_trait::async_trait]
impl ForkOps for GitForkOps {
    fn fork_reachable(&self, fork_url: &str) -> bool {
        crate::git::ls_remote_head(fork_url).is_ok()
    }
    async fn create_fork(
        &self,
        upstream_owner: &str,
        upstream_repo: &str,
        token: &str,
    ) -> Result<()> {
        crate::github::create_fork(upstream_owner, upstream_repo, token).await
    }
}

/// Reachability budget for a freshly-created fork: poll for up to this long.
const FORK_REACHABILITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Interval between reachability probes while waiting on a new fork.
const FORK_REACHABILITY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// When fork-PR mode is active, ensure each configured repository has a
/// reachable fork at the derived URL. Missing forks are created via the
/// GitHub REST API, then probed via `git ls-remote` with a 60-second
/// timeout.
///
/// Per `fork-setup-failure-degrades-gracefully`, a per-repo failure no
/// longer aborts startup: this returns one [`ForkSetupFailure`] per
/// repository whose fork could not be set up (creation non-2xx, fork not
/// reachable within the timeout, unroutable PAT, or underivable fork URL).
/// The caller skips those repositories for the process lifetime and emits a
/// chatops alert for each (see [`alert_fork_setup_failures`]); every other
/// repository is served normally. The daemon NEVER exits for a per-repo
/// fork-setup failure, even when every configured repository fails. When
/// `github.fork_owner` is unset (direct-push mode) the returned vec is
/// always empty.
pub async fn ensure_forks_exist(
    github: &GithubConfig,
    repos: &[RepositoryConfig],
) -> Vec<ForkSetupFailure> {
    ensure_forks_exist_with(
        github,
        repos,
        &GitForkOps,
        FORK_REACHABILITY_TIMEOUT,
        FORK_REACHABILITY_POLL_INTERVAL,
    )
    .await
}

/// Inner driver for [`ensure_forks_exist`] with the fork primitives AND the
/// reachability timings injected, so the per-repo skip/alert decision is
/// unit-testable without network or a 60-second wall-clock wait.
async fn ensure_forks_exist_with(
    github: &GithubConfig,
    repos: &[RepositoryConfig],
    ops: &dyn ForkOps,
    reachability_timeout: std::time::Duration,
    poll_interval: std::time::Duration,
) -> Vec<ForkSetupFailure> {
    let Some(fork_owner) = github.fork_owner.as_deref() else {
        return Vec::new();
    };
    let mut failures: Vec<ForkSetupFailure> = Vec::new();
    for repo in repos {
        let fork_url = match crate::github::derive_fork_url(&repo.url, fork_owner) {
            Ok(u) => u,
            Err(e) => {
                failures.push(ForkSetupFailure {
                    upstream_url: repo.url.clone(),
                    fork_url: None,
                    cause: format!("cannot derive fork URL: {e:#}"),
                });
                continue;
            }
        };
        // Quick probe: if the fork is already there, do nothing.
        if ops.fork_reachable(&fork_url) {
            continue;
        }
        // Missing fork → POST to GitHub.
        let (upstream_owner, upstream_repo) = match parse_repo_url(&repo.url) {
            Ok(t) => t,
            Err(e) => {
                failures.push(ForkSetupFailure {
                    upstream_url: repo.url.clone(),
                    fork_url: Some(fork_url),
                    cause: format!("cannot parse upstream URL: {e:#}"),
                });
                continue;
            }
        };
        let token = match resolve_token_with_source(github, &upstream_owner) {
            Ok((tok, _src)) => tok,
            Err(e) => {
                failures.push(ForkSetupFailure {
                    upstream_url: repo.url.clone(),
                    fork_url: Some(fork_url),
                    cause: format!("cannot resolve PAT for fork creation: {e:#}"),
                });
                continue;
            }
        };
        tracing::info!(
            "creating fork for {} → {fork_url}",
            repo.url
        );
        if let Err(e) = ops.create_fork(&upstream_owner, &upstream_repo, &token).await {
            failures.push(ForkSetupFailure {
                upstream_url: repo.url.clone(),
                fork_url: Some(fork_url),
                cause: format!("fork creation POST failed: {e:#}"),
            });
            continue;
        }
        // Poll until reachable, up to the configured timeout.
        let deadline = std::time::Instant::now() + reachability_timeout;
        let mut reachable = false;
        tracing::info!(
            "waiting for fork `{fork_url}` to become reachable (up to {}s)",
            reachability_timeout.as_secs()
        );
        loop {
            if ops.fork_reachable(&fork_url) {
                reachable = true;
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(poll_interval).await;
        }
        if reachable {
            tracing::info!(
                "created fork {fork_url} from upstream {}",
                repo.url
            );
        } else {
            failures.push(ForkSetupFailure {
                upstream_url: repo.url.clone(),
                fork_url: Some(fork_url.clone()),
                cause: format!(
                    "fork creation succeeded but `{fork_url}` was not reachable within {}s",
                    reachability_timeout.as_secs()
                ),
            });
        }
    }
    failures
}

/// Format the chatops alert text for a repository whose startup fork setup
/// failed. Identifies the repository AND carries a brief remedy hint
/// (ensure the fork exists/reachable, then restart or reload). Pure so the
/// message shape is unit-testable without a chatops backend.
pub fn fork_setup_failure_alert_message(failure: &ForkSetupFailure) -> String {
    let fork_part = match failure.fork_url.as_deref() {
        Some(fork) => format!(" (expected fork `{fork}`)"),
        None => String::new(),
    };
    format!(
        "⚠️ autocoder: fork setup failed for `{}`{} — {}. This repository is \
         skipped for the process lifetime; the daemon, other repositories, and \
         chatops keep running. Remedy: ensure the fork exists and is reachable, \
         then restart autocoder (or run `reload`).",
        failure.upstream_url, fork_part, failure.cause
    )
}

/// Emit one chatops alert per failed fork setup through the standard
/// outbound notification path. Best-effort: each delivery failure is logged
/// at WARN and never blocks startup. With no chatops backend configured,
/// each skip is logged at WARN so the operator still has a journalctl
/// signal. Independent of `notifications.*` flags — a skipped repository is
/// a daemon-lifecycle event, like the startup version notification.
pub async fn alert_fork_setup_failures(
    chatops: Option<&ChatOpsSlot>,
    failures: &[ForkSetupFailure],
) {
    for failure in failures {
        let msg = fork_setup_failure_alert_message(failure);
        match chatops {
            Some(slot) => {
                if let Err(e) = slot
                    .backend
                    .post_notification(&slot.default_channel_id, &msg)
                    .await
                {
                    tracing::warn!(
                        url = %failure.upstream_url,
                        "fork-setup-failure chatops alert failed to deliver: {e}"
                    );
                }
            }
            None => {
                tracing::warn!(
                    url = %failure.upstream_url,
                    "fork setup failed and no chatops backend is configured to alert \
                     (repository skipped for the process lifetime): {}",
                    failure.cause
                );
            }
        }
    }
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
pub fn repo_passes_startup_check(
    paths: &paths::DaemonPaths,
    repo: &RepositoryConfig,
    github: &GithubConfig,
) -> bool {
    let workspace_path = workspace::resolve_path(paths, repo);
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
    if let Err(e) = workspace::ensure_initialized(paths, &workspace_path, &repo.url, fork_arg) {
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
    fn config_path_from_exec_start_parses_config_file() {
        let es = "{ path=/usr/local/bin/autocoder ; argv[]=/usr/local/bin/autocoder run --config /etc/autocoder/config.yaml ; ignore_errors=no }";
        assert_eq!(
            config_path_from_exec_start(es),
            Some(PathBuf::from("/etc/autocoder/config.yaml"))
        );
    }

    #[test]
    fn config_path_from_exec_start_parses_config_dir() {
        let es = "argv[]=/usr/local/bin/autocoder run --config-dir /home/ac/conf";
        assert_eq!(
            config_path_from_exec_start(es),
            Some(PathBuf::from("/home/ac/conf/config.yaml"))
        );
        // `=`-joined form too.
        assert_eq!(
            config_path_from_exec_start("autocoder run --config-dir=/srv/ac"),
            Some(PathBuf::from("/srv/ac/config.yaml"))
        );
    }

    #[test]
    fn config_path_from_exec_start_none_without_flag() {
        assert_eq!(config_path_from_exec_start("autocoder run"), None);
        // `--config` with no value falls through to None.
        assert_eq!(config_path_from_exec_start("autocoder run --config --verbose"), None);
    }

    #[test]
    fn resolve_config_explicit_path_wins_and_skips_unit() {
        // Even with a unit recording a different path, an explicit path wins
        // AND the unit is not consulted (the explicit branch returns first).
        let got = resolve_run_config_path_inner(
            Some(PathBuf::from("/explicit/config.yaml")),
            Some("autocoder run --config /unit/config.yaml"),
            &[PathBuf::from("/default/config.yaml")],
            &|_| true,
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/explicit/config.yaml"));
    }

    #[test]
    fn resolve_config_discovered_from_unit() {
        let got = resolve_run_config_path_inner(
            None,
            Some("autocoder run --config /unit/config.yaml"),
            &[PathBuf::from("/default/config.yaml")],
            &|_| true,
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/unit/config.yaml"));
    }

    #[test]
    fn resolve_config_falls_back_to_default_without_unit() {
        let got = resolve_run_config_path_inner(
            None,
            None,
            &[PathBuf::from("/default/config.yaml")],
            &|p| p == std::path::Path::new("/default/config.yaml"),
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/default/config.yaml"));
    }

    #[test]
    fn resolve_config_errors_when_nothing_resolves() {
        let err = resolve_run_config_path_inner(
            None,
            None,
            &[PathBuf::from("/default/config.yaml")],
            &|_| false,
        )
        .expect_err("no config anywhere must error");
        assert!(format!("{err}").contains("--config"), "{err}");
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
        RepositoryConfig { forge: None,
            url: format!("git@github.com:fixture/{}.git", local.file_name().unwrap().to_string_lossy()),
            local_path: Some(local),
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
            sandbox: None,
        }
    }

    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Env-var mutation is global; serialize the startup-validation tests
    /// that touch real env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn repo(url: &str) -> RepositoryConfig {
        RepositoryConfig { forge: None,
            url: url.into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
            sandbox: None,
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
            command_authorization: Default::default(),
        };
        // No fork_owner means the function returns no failures without
        // probing anything (direct-push mode).
        let repos = vec![repo("git@github.com:any/repo.git")];
        let failures = ensure_forks_exist(&github, &repos).await;
        assert!(
            failures.is_empty(),
            "direct-push mode skips fork probing; got: {failures:?}"
        );
    }

    #[tokio::test]
    async fn ensure_forks_exist_records_failure_on_unsupported_url_scheme() {
        // Non-github URL combined with fork-PR mode → derive_fork_url
        // rejects → recorded as a per-repo failure (NOT a fatal error).
        // Per `fork-setup-failure-degrades-gracefully` the daemon does not
        // abort; the repo is skipped and an alert is emitted by the caller.
        let github = GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: Some("machine-user".into()),
            recreate_fork_on_reinit: false,
            command_authorization: Default::default(),
        };
        let repos = vec![repo("ssh://git@github.com/upstream/repo.git")];
        let failures = ensure_forks_exist(&github, &repos).await;
        assert_eq!(failures.len(), 1, "one repo, one failure");
        assert_eq!(failures[0].upstream_url, "ssh://git@github.com/upstream/repo.git");
        assert!(
            failures[0].cause.contains("derive fork URL"),
            "cause must explain the derivation failure; got: {}",
            failures[0].cause
        );
    }

    // ====================================================================
    // fork-setup-failure-degrades-gracefully: per-repo skip + chatops alert
    // ====================================================================

    /// Injectable [`ForkOps`] with scripted reachability + creation outcomes,
    /// so the per-repo skip/alert decision is exercised without real network
    /// or the 60-second reachability wait.
    struct FakeForkOps {
        /// Fork URLs reachable right now (probe → true).
        reachable: Mutex<std::collections::HashSet<String>>,
        /// Upstream `owner/repo` keys whose fork-creation POST returns non-2xx.
        create_fails_for: std::collections::HashSet<String>,
        /// Upstream `owner/repo` → fork URL inserted into `reachable` on a
        /// successful create (models a fork that becomes reachable after POST).
        make_reachable_on_create: std::collections::HashMap<String, String>,
        /// Record of every `owner/repo` a create POST was attempted for.
        created: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl ForkOps for FakeForkOps {
        fn fork_reachable(&self, fork_url: &str) -> bool {
            self.reachable.lock().unwrap().contains(fork_url)
        }
        async fn create_fork(
            &self,
            upstream_owner: &str,
            upstream_repo: &str,
            _token: &str,
        ) -> Result<()> {
            let key = format!("{upstream_owner}/{upstream_repo}");
            self.created.lock().unwrap().push(key.clone());
            if self.create_fails_for.contains(&key) {
                return Err(anyhow!("simulated non-2xx (403 Forbidden) for {key}"));
            }
            if let Some(fork) = self.make_reachable_on_create.get(&key) {
                self.reachable.lock().unwrap().insert(fork.clone());
            }
            Ok(())
        }
    }

    /// A `ChatOpsBackend` that records every `post_notification` call, so the
    /// fork-setup alert path is asserted without a live chat provider.
    struct RecordingChatOps {
        posts: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl crate::chatops::ChatOpsBackend for RecordingChatOps {
        fn provider_name(&self) -> &'static str {
            "recording"
        }
        fn is_experimental(&self) -> bool {
            true
        }
        async fn post_question(
            &self,
            _channel: &str,
            _change: &str,
            _question: &str,
        ) -> Result<String> {
            unreachable!()
        }
        async fn poll_thread_for_human_reply(
            &self,
            _channel: &str,
            _handle: &str,
        ) -> Result<Option<crate::chatops::HumanReply>> {
            unreachable!()
        }
        async fn post_notification(&self, channel: &str, text: &str) -> Result<()> {
            self.posts
                .lock()
                .unwrap()
                .push((channel.to_string(), text.to_string()));
            Ok(())
        }
    }

    /// A `GithubConfig` in fork-PR mode whose token resolves inline (no env),
    /// so the create path is reachable in tests.
    fn fork_github(fork_owner: &str) -> GithubConfig {
        GithubConfig {
            token_env: "AUTOCODER_TEST_FORK_TOKEN_ENV_DEFINITELY_UNSET".into(),
            token: Some(crate::config::SecretSource::Inline {
                value: "inline-fork-pat".into(),
            }),
            owner_tokens: None,
            fork_owner: Some(fork_owner.into()),
            recreate_fork_on_reinit: false,
            command_authorization: Default::default(),
        }
    }

    fn recording_slot(channel: &str) -> (Arc<RecordingChatOps>, ChatOpsSlot) {
        let backend = Arc::new(RecordingChatOps {
            posts: Mutex::new(Vec::new()),
        });
        let slot = ChatOpsSlot {
            backend: backend.clone(),
            default_channel_id: channel.into(),
            start_work_enabled: true,
            failure_alerts_enabled: true,
            pr_opened_enabled: true,
        };
        (backend, slot)
    }

    /// 3.3 (happy path): every fork already exists → no creation POSTs and no
    /// failures, so every repository spawns normally.
    #[tokio::test]
    async fn fork_setup_all_forks_already_exist_no_creation_no_failures() {
        let github = fork_github("mu");
        let repos = vec![
            repo("git@github.com:orgA/a.git"),
            repo("git@github.com:orgB/b.git"),
        ];
        let ops = FakeForkOps {
            reachable: Mutex::new(
                ["git@github.com:mu/a.git", "git@github.com:mu/b.git"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            ),
            create_fails_for: std::collections::HashSet::new(),
            make_reachable_on_create: std::collections::HashMap::new(),
            created: Mutex::new(Vec::new()),
        };
        let failures = ensure_forks_exist_with(
            &github,
            &repos,
            &ops,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(failures.is_empty(), "all reachable → no failures: {failures:?}");
        assert!(
            ops.created.lock().unwrap().is_empty(),
            "no fork-creation POST issued when every fork already exists"
        );
    }

    /// 3.3 (happy path): a missing fork is created and becomes reachable →
    /// recorded as success (no failure), and the create POST was issued.
    #[tokio::test]
    async fn fork_setup_created_and_reachable_spawns_normally() {
        let github = fork_github("mu");
        let repos = vec![repo("git@github.com:orgA/a.git")];
        let mut make = std::collections::HashMap::new();
        make.insert("orgA/a".to_string(), "git@github.com:mu/a.git".to_string());
        let ops = FakeForkOps {
            reachable: Mutex::new(std::collections::HashSet::new()),
            create_fails_for: std::collections::HashSet::new(),
            make_reachable_on_create: make,
            created: Mutex::new(Vec::new()),
        };
        let failures = ensure_forks_exist_with(
            &github,
            &repos,
            &ops,
            std::time::Duration::from_millis(500),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(
            failures.is_empty(),
            "fork created and reachable → no failure: {failures:?}"
        );
        assert_eq!(
            ops.created.lock().unwrap().as_slice(),
            &["orgA/a".to_string()],
            "the create POST must have been issued for the missing fork"
        );
    }

    /// 3.1: one repo's fork setup fails (creation POST non-2xx) AND another's
    /// succeeds → only the failed repo is in the skip set, exactly one alert
    /// fires for it, and the routine returns normally (no fatal error).
    #[tokio::test]
    async fn fork_setup_one_fails_one_succeeds_skips_and_alerts_only_failed() {
        let github = fork_github("mu");
        let repos = vec![
            repo("git@github.com:orgA/a.git"), // already reachable → success
            repo("git@github.com:orgB/b.git"), // create POST fails → failure
        ];
        let ops = FakeForkOps {
            reachable: Mutex::new(
                ["git@github.com:mu/a.git".to_string()].into_iter().collect(),
            ),
            create_fails_for: ["orgB/b".to_string()].into_iter().collect(),
            make_reachable_on_create: std::collections::HashMap::new(),
            created: Mutex::new(Vec::new()),
        };
        let failures = ensure_forks_exist_with(
            &github,
            &repos,
            &ops,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert_eq!(failures.len(), 1, "only the failed repo is recorded");
        assert_eq!(failures[0].upstream_url, "git@github.com:orgB/b.git");

        // The skip set (built exactly as `execute` builds it) excludes the
        // reachable repo and includes only the failed one.
        let skip: std::collections::HashSet<String> =
            failures.iter().map(|f| f.upstream_url.clone()).collect();
        assert!(
            !skip.contains("git@github.com:orgA/a.git"),
            "reachable repo must NOT be skipped (its polling task spawns)"
        );
        assert!(
            skip.contains("git@github.com:orgB/b.git"),
            "failed repo must be skipped for the process lifetime"
        );

        // Exactly one chatops alert fires, naming the failed repo + a remedy.
        let (backend, slot) = recording_slot("C123");
        alert_fork_setup_failures(Some(&slot), &failures).await;
        let posts = backend.posts.lock().unwrap();
        assert_eq!(posts.len(), 1, "one alert per failed repo");
        assert_eq!(posts[0].0, "C123", "alert posts to the default channel");
        assert!(
            posts[0].1.contains("orgB/b.git"),
            "alert must identify the failed repo: {}",
            posts[0].1
        );
        let lc = posts[0].1.to_lowercase();
        assert!(
            lc.contains("restart") || lc.contains("reload"),
            "alert must carry a remedy hint: {}",
            posts[0].1
        );
    }

    /// 3.2: every repo's fork setup fails → the routine still returns normally
    /// (the daemon does NOT exit non-zero) with one alert per failed repo.
    #[tokio::test]
    async fn fork_setup_every_repo_fails_returns_with_one_alert_each() {
        let github = fork_github("mu");
        let repos = vec![
            repo("git@github.com:orgA/a.git"),
            repo("git@github.com:orgB/b.git"),
        ];
        let ops = FakeForkOps {
            reachable: Mutex::new(std::collections::HashSet::new()),
            create_fails_for: ["orgA/a".to_string(), "orgB/b".to_string()]
                .into_iter()
                .collect(),
            make_reachable_on_create: std::collections::HashMap::new(),
            created: Mutex::new(Vec::new()),
        };
        // Returns normally even when every repository fails fork setup.
        let failures = ensure_forks_exist_with(
            &github,
            &repos,
            &ops,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert_eq!(failures.len(), 2, "every repo failed");

        let (backend, slot) = recording_slot("C9");
        alert_fork_setup_failures(Some(&slot), &failures).await;
        assert_eq!(
            backend.posts.lock().unwrap().len(),
            2,
            "one alert per failed repo"
        );
    }

    /// The reachability-timeout branch: create succeeds but the fork never
    /// becomes reachable within the budget → recorded as a failure naming the
    /// fork URL and the timeout.
    #[tokio::test]
    async fn fork_setup_created_but_unreachable_times_out_to_failure() {
        let github = fork_github("mu");
        let repos = vec![repo("git@github.com:orgA/a.git")];
        let ops = FakeForkOps {
            reachable: Mutex::new(std::collections::HashSet::new()),
            create_fails_for: std::collections::HashSet::new(),
            // never becomes reachable after create
            make_reachable_on_create: std::collections::HashMap::new(),
            created: Mutex::new(Vec::new()),
        };
        let failures = ensure_forks_exist_with(
            &github,
            &repos,
            &ops,
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].cause.contains("not reachable within"),
            "timeout cause expected; got: {}",
            failures[0].cause
        );
        assert_eq!(
            failures[0].fork_url.as_deref(),
            Some("git@github.com:mu/a.git")
        );
    }

    #[test]
    fn fork_setup_failure_alert_message_names_repo_and_remedy() {
        let f = ForkSetupFailure {
            upstream_url: "git@github.com:up/repo.git".into(),
            fork_url: Some("git@github.com:mu/repo.git".into()),
            cause: "fork creation POST failed: 403".into(),
        };
        let msg = fork_setup_failure_alert_message(&f);
        assert!(msg.contains("git@github.com:up/repo.git"), "names upstream: {msg}");
        assert!(msg.contains("git@github.com:mu/repo.git"), "names fork: {msg}");
        assert!(msg.contains("403"), "carries the cause: {msg}");
        assert!(msg.contains("skipped"), "states the repo is skipped: {msg}");
        assert!(
            msg.contains("reload") || msg.contains("restart"),
            "carries a remedy hint: {msg}"
        );
    }

    /// With no chatops backend configured, the alert path is a no-op (it logs
    /// at WARN) and must never panic — startup keeps running regardless.
    #[tokio::test]
    async fn alert_fork_setup_failures_no_backend_is_noop() {
        let f = ForkSetupFailure {
            upstream_url: "git@github.com:up/repo.git".into(),
            fork_url: None,
            cause: "cannot derive fork URL: unsupported scheme".into(),
        };
        alert_fork_setup_failures(None, &[f]).await;
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
            command_authorization: Default::default(),
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
            command_authorization: Default::default(),
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
            command_authorization: Default::default(),
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
            command_authorization: Default::default(),
        };
        let (_td, test_paths) = crate::testing::test_daemon_paths();
        assert!(
            repo_passes_startup_check(&test_paths, &dirty_repo, &direct_push_github),
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

    #[test]
    fn autocoder_version_env_is_non_empty() {
        let v = env!("AUTOCODER_VERSION");
        assert!(
            !v.is_empty(),
            "AUTOCODER_VERSION must be non-empty (build.rs fallback should always produce a value)"
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
            calls[0].text.contains(&format!("autocoder v{}", env!("AUTOCODER_VERSION"))),
            "message must name AUTOCODER_VERSION: {}",
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
            command_authorization: Default::default(),
        };
        let (_td, test_paths) = crate::testing::test_daemon_paths();
        assert!(repo_passes_startup_check(&test_paths, &clean_repo, &direct_push_github),
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
            () = ctrl_c => {
                // a39: flag the daemon-shutdown path BEFORE cancelling
                // child tasks. The shutdown SIGTERM cascade reaches the
                // executor subprocess (systemd cgroup kill / the
                // process-group setup), killing it by signal 15, AND the
                // classifier's SIGTERM check (`signal() == Some(15)`)
                // must observe the flag as `true` so the resulting
                // outcome is `Aborted` (no counter bump) rather than
                // `Failed`. Order is load-bearing per the spec.
                crate::daemon::request_shutdown();
                tracing::info!("received SIGINT; shutting down");
            }
            () = terminate => {
                crate::daemon::request_shutdown();
                tracing::info!("received SIGTERM; shutting down");
            }
        }
        cancel.cancel();
    });
}
