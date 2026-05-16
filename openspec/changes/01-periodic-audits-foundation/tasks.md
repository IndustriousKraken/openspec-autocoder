## 1. Config schema

- [ ] 1.1 In `autocoder/src/config.rs`, add `pub audits: Option<AuditsConfig>` to `Config` (top-level).
- [ ] 1.2 Define `AuditsConfig { defaults: HashMap<String, Cadence>, settings: HashMap<String, AuditSettings> }`. `defaults` keys are audit type names; values are `Cadence`. `settings` carries per-audit knobs (`prompt_path`, `notify_on_clean`).
- [ ] 1.3 Define `pub audits: Option<HashMap<String, Cadence>>` on `RepositoryConfig` for per-repo overrides.
- [ ] 1.4 Define `Cadence` enum with variants `Disabled, Daily, EveryNDays(u32), Weekly, Monthly, Quarterly`. Custom serde deserialization from the literal strings listed in the spec. Reject `every-0-days` and negative N at deserialize time with a clear error.
- [ ] 1.5 Helper: `pub fn resolved_cadence(repo: &RepositoryConfig, audits_cfg: Option<&AuditsConfig>, audit_type: &str) -> Cadence` — per-repo override → global default → `Disabled`.
- [ ] 1.6 Validation: at config load, every audit type name appearing in `audits.defaults` or `repositories[].audits` must match a name in the audit registry. Mismatched names exit non-zero at startup with a list of known names.
- [ ] 1.7 Tests in `config::tests`:
  - `cadence_parses_each_string_form`
  - `cadence_every_n_days_rejects_zero`
  - `cadence_every_n_days_rejects_negative`
  - `audits_unknown_type_fails_at_load`
  - `per_repo_audit_overrides_global_default`
  - `audit_absent_from_both_resolves_to_disabled`

## 2. State file

- [ ] 2.1 New module `autocoder/src/audits/state.rs`. Define `AuditState { runs: HashMap<String, AuditRunEntry> }` and `AuditRunEntry { last_run_at: DateTime<Utc>, last_run_sha: Option<String>, last_outcome: AuditOutcomeKind }`.
- [ ] 2.2 Path: `<workspace>/.audit-state.json`. Add `.audit-state.json` to `.git/info/exclude` via `workspace::ensure_git_info_excluded` so it doesn't trip the dirty check.
- [ ] 2.3 Atomic save (write-to-temp + rename, mirroring `alert_state::save`). Idempotent.
- [ ] 2.4 `load_or_default`: returns `Default` on missing file OR unparseable file (logs WARN on unparseable; missing is silent).
- [ ] 2.5 Tests:
  - `state_save_load_round_trip`
  - `state_load_handles_missing_file`
  - `state_load_handles_corrupt_file_with_warning`

## 3. Audit trait + outcome types

- [ ] 3.1 New module `autocoder/src/audits/mod.rs`. Define:
  ```rust
  #[async_trait]
  pub trait Audit: Send + Sync {
      fn audit_type(&self) -> &'static str;
      fn requires_head_change(&self) -> bool;
      fn write_policy(&self) -> WritePolicy;
      async fn run(&self, ctx: &AuditContext) -> Result<AuditOutcome>;
  }

  pub enum WritePolicy { None, OpenSpecOnly, Approved }

  pub enum AuditOutcome {
      NoFindings,
      Reported(Vec<Finding>),
      SpecsWritten(Vec<String>),
  }

  pub enum AuditOutcomeKind { NoFindings, Reported, SpecsWritten }

  pub struct Finding {
      pub severity: Severity,
      pub subject: String,
      pub body: String,
      pub anchor: Option<String>,
  }

  pub enum Severity { Low, Medium, High }

  pub struct AuditContext<'a> {
      pub workspace: &'a Path,
      pub repo: &'a RepositoryConfig,
      pub chatops_ctx: Option<&'a ChatOpsContext>,
      pub log_writer: AuditLogWriter,
      // Future audits may need a github client here; add per-audit when needed.
  }
  ```
- [ ] 3.2 `AuditLogWriter`: append-only writer that hands the audit a `Write` trait object backed by the per-invocation log file. Auto-creates `/tmp/autocoder/logs/<basename>/audits/` directory.
- [ ] 3.3 `AuditRegistry`: holds `Vec<Arc<dyn Audit>>`. Built once at startup. `audits.iter()` iterated by the scheduler.

## 4. Scheduler integration

- [ ] 4.1 New module `autocoder/src/audits/scheduler.rs`. Function:
  ```rust
  pub async fn run_due_audits(
      registry: &AuditRegistry,
      workspace: &Path,
      repo: &RepositoryConfig,
      audits_cfg: Option<&AuditsConfig>,
      audit_settings: &HashMap<String, AuditSettings>,
      chatops_ctx: Option<&ChatOpsContext>,
  ) -> Result<()>;
  ```
- [ ] 4.2 Algorithm per audit:
  1. Resolve effective cadence via `config::resolved_cadence(...)`. Disabled → skip.
  2. Load `AuditState`. If `last_run_at + cadence_interval > now`, skip.
  3. If `requires_head_change && last_run_sha == current_head_sha`, skip.
  4. Open the audit-run log writer.
  5. Run the audit's `run(&ctx)`.
  6. Enforce write policy via `git status --porcelain`; if violation, revert (`git reset --hard HEAD + git clean -fd` for OpenSpecOnly violations; `git reset --hard HEAD` for None violations), post chatops alert, do NOT update state.
  7. On success: dispatch `AuditOutcome` (chatops post for Reported, log for SpecsWritten), update state.
- [ ] 4.3 Call site in `polling_loop::run_pass_through_commits`: insert `run_due_audits(...).await?;` AFTER `recreate_branch` AND BEFORE `list_pending`.
- [ ] 4.4 New `AlertCategory::AuditWritePolicyViolation` with label "audit attempted disallowed write" — see `alert_state.rs` plumbing.

## 5. Architecture-brightline audit

- [ ] 5.1 New module `autocoder/src/audits/brightline.rs`. Implements `Audit` with `audit_type() = "architecture_brightline"`, `requires_head_change() = true`, `write_policy() = WritePolicy::None`.
- [ ] 5.2 Metrics (language-agnostic via simple line/regex scanning, NOT a full parser):
  - **File size:** count newlines in every tracked file under `src/`/`lib/`/`app/`/etc. (heuristic: ignore vendored / `node_modules` / `target` / `vendor` directories). Threshold: configurable (`audit_settings.architecture_brightline.file_lines_threshold`), default `800`.
  - **Function/method signature duplicate detection:** simple regex per file: `^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\s*\([^)]*\)` for Rust; `^\s*(?:async\s+)?def\s+(\w+)\s*\([^)]*\):` for Python; `^\s*(?:public\s+)?(?:async\s+)?\w+\s+(\w+)\s*\([^)]*\)` for C# (simplified). Cross-file collision = finding.
  - **(Other metrics added incrementally; this spec doesn't require an exhaustive list, only "at least file-size and signature-duplicate detection ship in this change".)**
- [ ] 5.3 Returns `AuditOutcome::Reported(findings)`. Empty findings is a successful "no findings" outcome.
- [ ] 5.4 Tests `audits::brightline::tests`:
  - `file_size_metric_flags_long_files`
  - `file_size_metric_respects_threshold_override`
  - `file_size_metric_ignores_excluded_dirs` (node_modules, target, vendor)
  - `signature_duplicate_metric_flags_cross_file_collisions_rust`
  - `signature_duplicate_metric_ignores_tests_module`
  - `audit_returns_no_findings_on_clean_codebase`
  - `audit_returns_findings_for_known_violations`

## 6. Default audit settings + registry wiring

- [ ] 6.1 `AuditSettings` struct: `{ prompt_path: Option<PathBuf>, notify_on_clean: bool, extra: HashMap<String, serde_yaml::Value> }`. The `extra` field allows per-audit knobs (like brightline's `file_lines_threshold`) without bloating the top-level schema.
- [ ] 6.2 In `cli/run.rs::run_command`, after config load, build the `AuditRegistry` with `Arc::new(brightline::ArchitectureBrightlineAudit::new(&audit_settings))`. Wire it through to each polling task.

## 7. Chatops output formatting

- [ ] 7.1 In `audits/scheduler.rs`, helper `format_findings_message(repo_url, audit_type, findings, per_finding_max_chars)`. Renders the header + bullet list with severity glyphs (low: `•`, medium: `⚠`, high: `🔴`). Truncates per finding.
- [ ] 7.2 Helper `format_clean_message(repo_url, audit_type)`: `"✅ \`<repo>\`: <audit_type> — no findings"`.

## 8. Audit-run log + scheduling tests

- [ ] 8.1 `audits::scheduler::tests::audit_due_when_cadence_elapsed`
- [ ] 8.2 `audits::scheduler::tests::audit_skipped_when_cadence_not_elapsed`
- [ ] 8.3 `audits::scheduler::tests::audit_skipped_when_requires_head_change_and_sha_matches`
- [ ] 8.4 `audits::scheduler::tests::audit_runs_when_requires_head_change_but_sha_differs`
- [ ] 8.5 `audits::scheduler::tests::audit_disabled_cadence_never_runs`
- [ ] 8.6 `audits::scheduler::tests::write_policy_none_post_hoc_diff_triggers_revert_and_alert`
- [ ] 8.7 `audits::scheduler::tests::write_policy_openspec_only_rejects_diff_outside_changes`
- [ ] 8.8 `audits::scheduler::tests::audit_failure_does_not_update_state_and_does_not_abort_iteration`
- [ ] 8.9 `audits::scheduler::tests::reported_findings_post_to_chatops_with_format`
- [ ] 8.10 `audits::scheduler::tests::reported_no_findings_silent_unless_notify_on_clean`
- [ ] 8.11 `audits::scheduler::tests::specs_written_outcome_logs_info_no_chatops`
- [ ] 8.12 `audits::scheduler::tests::audit_run_log_written_per_invocation`

## 9. Documentation

- [ ] 9.1 README "Periodic audits" — new subsection under "Operating Notes". Document: the `audits:` config block, the cadence enum, the registered audit type names, the default-off / opt-in pattern, the audit-run log location, the WritePolicy enforcement semantics.
- [ ] 9.2 README "Config reference" — under `audits` block table, document each field and the cadence syntax.

## 10. Verification

- [ ] 10.1 `cargo test` passes.
- [ ] 10.2 `openspec validate periodic-audits-foundation --strict` passes.
- [ ] 10.3 Manual check: with default config (no `audits:` block), daemon behavior is identical to today.
