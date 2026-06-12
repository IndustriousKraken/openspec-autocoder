# Tasks

## 1. Non-interactive flag

- [x] 1.1 Add an `IssuesLaneArg` enum (`Enabled` | `Disabled`, default `Disabled`) to `cli/install.rs`, mirroring the `LlmDrivenAuditsArg` pattern (clap `ValueEnum`, `Default`).
- [x] 1.2 Add `issues_lane: Option<IssuesLaneArg>` to `InstallArgs` (the `#[arg(long = "issues-lane")]` non-interactive flag) and thread it into `WizardPrefill`.
- [x] 1.3 Resolve the prefill to a bool in the non-interactive answer builder (mirroring `resolve_non_interactive_audits`): `Some(Enabled) → true`, `Some(Disabled)`/`None → false`. No new validation is required (a `ValueEnum` rejects bad values at parse time); confirm `validate_non_interactive` needs no change.

## 2. Interactive gate

- [x] 2.1 Add an `issues_enabled: bool` field to `WizardAnswers`.
- [x] 2.2 In the interactive wizard flow, after the periodic-audits prompts and before config assembly, add a single yes/no gate defaulting to NO (reuse the existing `confirm(..., false)` helper). The prompt body states: enabling makes per-iteration unit selection `issues > changes > audits`, AND that with `features.scout.include_issues` on the bot triages open GitHub issues read-only into chatops candidates a maintainer promotes with `send it`.

## 3. Config assembly

- [x] 3.1 In the config-assembly step, set `cfg.features.issues.enabled = answers.issues_enabled`. When `false`, ensure no `features.issues` entry is serialized (rely on the `skip_serializing_if`/default-omission behavior already used for the audits block); when `true`, the `features.issues.enabled: true` line is present.

## 4. Tests (`cli/install.rs`)

- [x] 4.1 Interactive default → rendered config has no `features.issues` entry / lane off.
- [x] 4.2 Interactive opt-in (`y`) → rendered config has `features.issues.enabled: true`.
- [x] 4.3 `--non-interactive` with no `--issues-lane` → lane off (IaC-compatibility).
- [x] 4.4 `--non-interactive --issues-lane enabled` → `features.issues.enabled: true`.
- [x] 4.5 `--non-interactive --issues-lane disabled` → lane off.

## 5. Documentation

- [x] 5.1 README: add the issues-lane gate + `--issues-lane` flag to the install/wizard section (alongside the periodic-audits paragraphs).
- [x] 5.2 `docs/CLI.md`: document the `--issues-lane` flag under `install`.
- [x] 5.3 `docs/CONFIG.md`: cross-reference `features.issues` (config.example.yaml already carries the field documentation).

## 6. Acceptance

- [x] 6.1 `cargo test` passes.
- [x] 6.2 `openspec validate install-wizard-issues-lane --strict` passes.
