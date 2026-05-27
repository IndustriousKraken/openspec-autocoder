## ADDED Requirements

### Requirement: Install wizard `--reconfigure` flag re-runs one section against an existing install
`autocoder install` SHALL accept a `--reconfigure <section>` flag whose value is one of `audits`, `reviewer`, or `chatops`. The flag SHALL operate only against a detected existing install (located via the `a01` systemd probe OR the default-config-path fallback). The flag SHALL be mutually exclusive with `--non-interactive` AND with every prefill flag (`--repo-url`, `--token-env-var`, `--chatops-backend`, etc.); reconfigure is interactive and section-scoped by definition.

Per-section behavior:

- **`--reconfigure audits`** SHALL re-prompt every audit cadence with the operator's current cadence as the default, then patch ONLY the `audits.defaults.*` subtree of the existing `config.yaml` in place via atomic temp-file-then-rename. The patch overwrites the file; YAML comments outside the audits subtree are not preserved because `serde_yaml` does not round-trip comments.
- **`--reconfigure reviewer`** AND **`--reconfigure chatops`** SHALL re-prompt the relevant section, then show the operator a unified diff between the current `config.yaml` and the proposed new YAML AND prompt `Apply this patch? [y/N]`. The patch is applied only on `y/Y`; any other answer (including the default) leaves the file unchanged.

After a successful patch, the subcommand SHALL print restart guidance naming `sudo -u autocoder autocoder reload` as the apply step. The wizard SHALL NOT auto-reload — the operator decides when to apply.

The following knobs SHALL NOT be accessible via `--reconfigure`:

- `repositories` (use `autocoder reload`, which hot-applies add/remove without a daemon restart)
- `paths.*` (relocating data directories is destructive and restart-required)
- `executor.*` (the only block that requires a daemon restart)
- `audits.settings.*.prompt_path` and `audits.settings.*.extra.*` (advanced overrides; edit YAML directly)

#### Scenario: `--reconfigure audits` re-prompts cadences and patches in place
- **WHEN** the operator runs `autocoder install --reconfigure audits` against an existing server-mode install whose `audits.defaults.drift_audit` is `weekly`
- **THEN** the wizard prompts for each audit's cadence with the existing value as the displayed default
- **AND** if the operator answers `monthly` for `drift_audit`, the patched config has `audits.defaults.drift_audit: monthly`
- **AND** other top-level keys in `config.yaml` (`github`, `repositories`, `executor`, etc.) parse to the same values they had pre-patch
- **AND** the file is written via atomic temp-file-then-rename, preserving the existing mode and owner
- **AND** the wizard prints `Patched audits.defaults.* in <path>. To apply: sudo -u autocoder autocoder reload`

#### Scenario: `--reconfigure reviewer` shows a diff and applies only on confirmation
- **WHEN** the operator runs `autocoder install --reconfigure reviewer` against an existing install whose `reviewer.provider` is `anthropic` AND `reviewer.model` is `claude-sonnet-4-6`
- **AND** the operator answers `openai_compatible` for provider AND `grok-3` for model
- **THEN** the wizard generates the proposed full YAML
- **AND** prints a unified diff between the current file and the proposed file
- **AND** prompts `Apply this patch? [y/N]`
- **AND** if the operator answers `y`, the file is overwritten via atomic temp-file-then-rename
- **AND** if the operator answers `n` (or presses Enter to accept the default), the file is unchanged AND the wizard prints `no changes made`

#### Scenario: `--reconfigure` against a host with no existing install exits non-zero
- **WHEN** the operator runs `autocoder install --reconfigure audits` AND neither the systemd probe NOR `<default-config-dir>/config.yaml` resolves to an existing file
- **THEN** the subcommand exits non-zero
- **AND** the error message reads `no existing install detected; run install.sh for first-time setup`
- **AND** no file is created

#### Scenario: `--reconfigure` is mutually exclusive with `--non-interactive`
- **WHEN** the operator runs `autocoder install --reconfigure audits --non-interactive`
- **THEN** clap rejects the invocation at argument-parse time
- **AND** the error message names both flags AND the conflict
- **AND** no file is created or modified

#### Scenario: `--reconfigure repositories` is rejected (excluded from the surface)
- **WHEN** the operator runs `autocoder install --reconfigure repositories`
- **THEN** clap rejects the value with the standard `possible values: audits, reviewer, chatops` message
- **AND** the wizard does NOT prompt and does NOT modify any file
- **AND** the operator workflow for repository changes (`autocoder reload`) is documented in the help text or docs

#### Scenario: Probe-resolved config path is honored over default
- **WHEN** the systemd probe (from `a01`) reports an existing unit with `--config /home/autocoder/autocoder/config.yaml`
- **AND** the operator runs `autocoder install --reconfigure audits`
- **THEN** the wizard reads from AND writes to `/home/autocoder/autocoder/config.yaml`, NOT the default `/etc/autocoder/config.yaml`
- **AND** the operator's existing config location is respected throughout the reconfigure flow

#### Scenario: Reconfigure handlers are testable via ScriptedIo
- **WHEN** the reconfigure tests run under `cargo test`
- **THEN** each test uses a `ScriptedIo` impl with a pre-loaded answer queue
- **AND** the `apply_in_place_patch` and `confirm_diff_and_apply` helpers are exercised against temp files
- **AND** the recorded calls assert what was prompted AND what was written
- **AND** no test invokes systemctl, useradd, or any other OS-mutating action
