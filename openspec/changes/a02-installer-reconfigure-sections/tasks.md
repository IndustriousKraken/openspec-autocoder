## 1. Args + enum

- [ ] 1.1 In `autocoder/src/cli/install.rs`, add to `InstallArgs`:
  ```rust
  #[arg(long, value_enum, conflicts_with_all = ["non_interactive", "repo_url", "base_branch", "agent_branch", "poll_interval_sec", "token_env_var", "chatops_backend", "chatops_channel_id", "reviewer_provider", "reviewer_model", "audits_llm_driven", "audit_architecture_brightline", "audit_architecture_consultative", "audit_drift_audit", "audit_missing_tests_audit", "audit_security_bug_audit"])]
  pub reconfigure: Option<ReconfigureSection>,
  ```
- [ ] 1.2 Define the enum:
  ```rust
  #[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
  pub enum ReconfigureSection {
      Audits,
      Reviewer,
      Chatops,
  }
  ```

## 2. Locate existing config for reconfigure

- [ ] 2.1 New function `resolve_existing_config_path(args: &InstallArgs, actions: &dyn SystemActions, mode: InstallMode) -> Result<PathBuf>`:
  - Server mode: invoke `probe_systemd_unit("autocoder.service")` (the surface from `a01`). If the probe returns `LoadState::Loaded` AND an `exec_start_config_path` that exists, use it. Else fall back to `/etc/autocoder/config.yaml`.
  - Dev mode: use `~/.config/autocoder/config.yaml`.
  - Honor `args.config_dir` override if set (`<override>/config.yaml`).
  - If no existing config is found, bail with `no existing install detected; run install.sh for first-time setup`.
- [ ] 2.2 Tests:
  - Probe returns loaded unit with valid path → returns that path.
  - Probe returns not-found AND `/etc/autocoder/config.yaml` exists → returns that path.
  - Probe returns not-found AND default path missing → bails.
  - Dev mode → returns home-config path without invoking the probe.

## 3. Dispatcher and per-section handlers

- [ ] 3.1 In `execute_inner`, after the existing-install detection from `a01`, branch on `args.reconfigure`. If `Some(section)`, dispatch to `execute_reconfigure(args, section, io, actions, ...)` and return its result. The pre-existing `args.upgrade` and idempotency-exit paths only apply when `reconfigure` is `None`.
- [ ] 3.2 New function `execute_reconfigure(args, section, io, actions, ...) -> Result<()>`:
  - Resolve the existing-config path via `resolve_existing_config_path`.
  - Parse the existing `config.yaml` into a `Config` via `serde_yaml`.
  - Match on `section`:
    - `Audits` → call `reconfigure_audits(&existing, io).await?` → call `apply_in_place_patch(&config_path, &new_config)?` → print restart guidance.
    - `Reviewer` → call `reconfigure_reviewer(&existing, io).await?` → call `confirm_diff_and_apply(&config_path, &new_config, io).await?` → if accepted, print restart guidance; if declined, print `no changes made`.
    - `Chatops` → analogous to Reviewer.
- [ ] 3.3 Restart guidance text:
  ```
  Patched audits.defaults.* in <config-path>.
  To apply: sudo -u autocoder autocoder reload
  ```
  (Substitute `audits.defaults.*` with `reviewer:` or `chatops:` for the other sections.)

## 4. Per-section re-prompt helpers

- [ ] 4.1 `reconfigure_audits(existing: &Config, io: &mut dyn WizardIo) -> Result<Config>`:
  - Extract current cadences from `existing.audits.defaults` for each known audit slug. Format as the default value in each prompt.
  - Call the existing `run_audit_prompts` (or a refactored version that accepts pre-fills). Operator's answers replace the existing values; declined audits (operator picks `disabled`) flip cadence to disabled.
  - Return a clone of `existing` with the updated `audits.defaults` map.
- [ ] 4.2 `reconfigure_reviewer(existing: &Config, io: &mut dyn WizardIo) -> Result<Config>`:
  - Re-prompt provider, model, api-key source (env-var name or inline). Use the existing values as defaults.
  - Return a clone of `existing` with the updated `reviewer:` block.
- [ ] 4.3 `reconfigure_chatops(existing: &Config, io: &mut dyn WizardIo) -> Result<Config>`:
  - Re-prompt provider (slack / discord / teams / mattermost / matrix / none), default channel id, the bot-token source. Use existing values as defaults.
  - Return a clone of `existing` with the updated `chatops:` block. If provider is `none`, the block is removed entirely.
- [ ] 4.4 Tests: each helper takes a fixture `Config`, an empty or scripted `WizardIo`, and asserts the returned Config has the expected fields.

## 5. In-place patch (audits)

- [ ] 5.1 `apply_in_place_patch(config_path: &Path, new_config: &Config) -> Result<()>`:
  - Serialize `new_config` to a full YAML string via `serialize_config`.
  - Atomic write: write to `<config_path>.tmp`, fsync, rename over `<config_path>`. On a server-mode config this respects the existing mode (0640) and owner (root:autocoder); check via stat after rename and `chmod` / `chown` back if needed.
  - The patch overwrites the entire file rather than splice-editing. Comments in the YAML ARE lost on round-trip — `serde_yaml` does not preserve them. The audits section in the wizard-generated YAML carries no comments, so this is acceptable for `--reconfigure audits`.
- [ ] 5.2 Tests: write a fixture config, call patch with a modified audits subtree, re-parse, assert the audits values updated AND other top-level keys still parse to expected values.

## 6. Diff-confirm (reviewer / chatops)

- [ ] 6.1 Pick a diff crate: check whether `similar` is already a transitive dep via `cargo tree`. If not, prefer `similar` over `imara-diff` for richer formatting. Pin version per `check-current-versions-not-training`.
- [ ] 6.2 `confirm_diff_and_apply(config_path: &Path, new_config: &Config, io: &mut dyn WizardIo) -> Result<bool>`:
  - Read current YAML from disk (raw string, no parse).
  - Serialize `new_config` to a full YAML string.
  - Compute a unified diff: `similar::TextDiff::from_lines(&current, &new).unified_diff().header("current", "proposed")`.
  - Print the diff.
  - Prompt `Apply this patch? [y/N]`. Default no.
  - On y/Y: call `apply_in_place_patch(config_path, new_config)?`, return `Ok(true)`. On any other answer: return `Ok(false)`.
- [ ] 6.3 Tests:
  - ScriptedIo answers `y` → patch applied; file matches expected new content.
  - ScriptedIo answers (default / `n` / `q` / `<empty>`) → patch NOT applied; file unchanged.
  - Diff output contains both `current` and `proposed` headers AND the expected `+` / `-` lines.

## 7. Exclude-list enforcement

- [ ] 7.1 `ReconfigureSection::Repositories` is NOT in the enum (the value isn't valid). clap rejects `--reconfigure repos` at the argument level with the standard "possible values: audits, reviewer, chatops" message.
- [ ] 7.2 Add a documentation comment to the `ReconfigureSection` enum naming the excluded sections (`repositories` → `autocoder reload`; `paths.*` → destructive, restart-required; `executor.*` → restart-required; `audits.settings.*.prompt_path` and `audits.settings.*.extra.*` → edit YAML).
- [ ] 7.3 Test: `cargo test` confirms `autocoder install --reconfigure repositories` exits non-zero with a clap usage error.

## 8. CLI.md + DEPLOYMENT.md

- [ ] 8.1 In `docs/CLI.md`, document the `--reconfigure` flag under a new `## \`install\`` heading (or extend an existing one): names the three accepted values, the mutual-exclusion with `--non-interactive`, the per-section behavior, the post-patch `reload` step.
- [ ] 8.2 In `docs/DEPLOYMENT.md`, in the section landing in `a01` (`Switching from source-build to binary updates`), add a paragraph about the `--reconfigure` verb: explains it as the "edit one section without re-doing the whole wizard" tool AND points at the `audits` example as the most common use.
- [ ] 8.3 Also add a brief mention in `docs/CONFIG.md` near the `audits.defaults.*` table noting that operators can re-prompt via `autocoder install --reconfigure audits` as an alternative to editing YAML.

## 9. Spec deltas

- [ ] 9.1 `openspec/changes/a02-installer-reconfigure-sections/specs/orchestrator-cli/spec.md` ADDs one requirement covering the `--reconfigure` flag, its three accepted sections, the per-section behavior (in-place patch for audits; diff-confirm for reviewer / chatops), the exclude-list, mutual-exclusion with `--non-interactive`, and the post-patch restart guidance.
- [ ] 9.2 `openspec/changes/a02-installer-reconfigure-sections/specs/project-documentation/spec.md` ADDs one requirement covering the docs surface (CLI.md `install` entry, DEPLOYMENT.md `--reconfigure` paragraph, CONFIG.md cross-link).

## 10. Verification

- [ ] 10.1 `cargo test` passes (new + existing).
- [ ] 10.2 `openspec validate a02-installer-reconfigure-sections --strict` passes.
- [ ] 10.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
