## 1. Audit the schema and confirm the missing-fields inventory

- [ ] 1.1 Re-walk `autocoder/src/config.rs` against `config.example.yaml` and confirm the field inventory in proposal.md is exhaustive. Specifically: enumerate every `pub` field on `Config`, `RepositoryConfig`, `ExecutorConfig`, `ExecutorSandboxConfig`, `GithubConfig`, `ReviewerConfig`, `ChatOpsConfig`, `NotificationsConfig`, `AuditsConfig`, `AuditSettings`, plus the registered audits' `extra` knobs (`brightline::SETTINGS_KEY_FILE_LINES`, `dependency_update::SETTINGS_KEY_MAX_APPROVALS`, `dependency_update::SETTINGS_KEY_FORK_REMOTE`, plus any others). If new omissions surface, add them to the gap-closing list before §2.

## 2. Fill in the example

- [ ] 2.1 Under `repositories[]:` add a commented block illustrating per-repo overrides: `chatops_channel_id`, `max_changes_per_pr`, and `audits:` (per-repo cadence map keyed by audit type slug). Each commented field carries a one-line description.
- [ ] 2.2 Under `executor:` add commented entries for `implementer_prompt_path`, `perma_stuck_after_failures` (default 2), `max_changes_per_pr` (global default 3), `startup_jitter_max_secs` (default 30), and `inter_iteration_jitter_pct` (default 10).
- [ ] 2.3 Under `github:` add a commented `recreate_fork_on_reinit: false` entry. Cross-reference the README section so an operator who reads only the example knows where to learn about the destructive semantic.
- [ ] 2.4 Under `chatops.notifications:` add `pr_opened: true` alongside the existing `start_work` and `failure_alerts` (all default `true`; commented).
- [ ] 2.5 Add a new top-level `audits:` block after `chatops:`. Include:
  - A multi-line header comment naming all 5 (or 6 once `architecture_consultative` ships) registered audits and explaining the cadence vocabulary (`disabled`, `daily`, `every-N-days`, `weekly`, `monthly`, `quarterly`).
  - A commented `defaults:` map showing each audit at a plausible cadence (e.g., `weekly` for brightline; `daily` for dependency_update_triage; `weekly` for drift; etc.). Each line is commented so the default-empty `audits:` block keeps everything Disabled by default.
  - A commented `settings:` block with one entry per audit demonstrating `prompt_path`, `notify_on_clean`, and the audit-specific `extra` keys (file_lines_threshold, max_approvals_per_run, fork_remote_name).
- [ ] 2.6 Walk the resulting file top-to-bottom and confirm every field name from §1.1's inventory appears at least once (as an active key or in a comment). Note any field that didn't land — fix.

## 3. Coverage test

- [ ] 3.1 Add a unit test `example_yaml_mentions_every_top_level_field` in `autocoder/src/config.rs::tests`. The test:
  - Resolves `config.example.yaml` via `Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.example.yaml")` (the autocoder crate lives one level below the repo root).
  - Reads the file as a string. If reading fails (file missing), the test panics with a clear "config.example.yaml not found at <path>" message so the operator knows what to fix.
  - Maintains a const-array of field names from the schema (top-level: `repositories`, `executor`, `github`, `reviewer`, `chatops`, `audits`; nested: `local_path`, `base_branch`, `agent_branch`, `poll_interval_sec`, `chatops_channel_id`, `max_changes_per_pr`, `command`, `timeout_secs`, `sandbox`, `implementer_prompt_path`, `perma_stuck_after_failures`, `startup_jitter_max_secs`, `inter_iteration_jitter_pct`, `allowed_tools`, `disallowed_bash_patterns`, `disallowed_read_paths`, `token_env`, `token`, `owner_tokens`, `fork_owner`, `recreate_fork_on_reinit`, `enabled`, `provider`, `model`, `api_key_env`, `api_key`, `api_base_url`, `bot_token_env`, `bot_token`, `default_channel_id`, `notifications`, `start_work`, `failure_alerts`, `pr_opened`, `defaults`, `settings`, `prompt_path`, `notify_on_clean`, `extra`).
  - Asserts each field name appears as a substring in the example. On miss, the test message names the missing field AND points the operator at `config.example.yaml` and `config.rs` so they know to update both.
- [ ] 3.2 Add a doc comment on the test explaining its purpose: catches new configurable fields added without corresponding example coverage. When extending the schema, update BOTH the example AND the field-name list in this test.

## 4. README pointer

- [ ] 4.1 In README's "Configuration Reference" section, add a one-line note: "`config.example.yaml` ships annotated comments for every field documented below; copy it as a starting point for your own `config.yaml`." This makes the example file's role explicit.

## 5. Verification

- [ ] 5.1 `cargo test` passes (the new coverage test is the main signal).
- [ ] 5.2 `openspec validate example-config-covers-every-field --strict` passes.
- [ ] 5.3 Spot-check: paste the un-commented version of the example into a `config.yaml`, run `autocoder run --config <path> --dry-run` (or whatever check is closest available) and confirm the YAML parses cleanly. If `--dry-run` doesn't exist, at minimum: write a small test that calls `Config::load_from` on an uncommented copy of the example and asserts it parses.
