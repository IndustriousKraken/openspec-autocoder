## 1. `Audit::description()` contract

- [x] 1.1 In `autocoder/src/audits/mod.rs`, add a required method `fn description(&self) -> &'static str` to the `Audit` trait. The method returns a one-line operator-facing description suitable for inline rendering in the install wizard (target ≤ 80 chars).
- [x] 1.2 If the trait method already exists (verify by grep first), skip §1.1 and use the existing method. This change does NOT introduce a new trait method if there's already an equivalent.
- [x] 1.3 Add a `description()` impl to each registered audit:
  - `architecture_brightline`: `"file-size / module-size guidelines (architecture brightline)"`
  - `architecture_consultative`: `"advisory architecture findings via LLM consultation"`
  - `drift_audit`: `"spec ↔ code drift detection (warns when reality outgrows the spec)"`
  - `missing_tests_audit`: `"proposes test coverage for untested branches"`
  - `security_bug_audit`: `"proposes fixes for likely security bugs"`
  - `spec_sync_audit`: `"backfills drift between archived changes and canonical openspec/specs/ files"`
- [x] 1.4 Trait-level test: build a registry containing every registered audit, iterate, assert each `description()` returns a non-empty string and is under 80 chars.

## 2. Wizard step: spec_sync_audit inline prompt

- [x] 2.1 In `autocoder/src/cli/install.rs`'s wizard flow, AFTER the reviewer prompt and BEFORE the config-assembly step, add a section titled "Periodic audits."
- [x] 2.2 First question: `Enable spec_sync_audit? Cadence: [d]aily (default) / [w]eekly / [m]onthly / [n]ever`. Default on bare-Enter is `daily`. Capture the answer as a `Cadence` value (using the existing `Cadence::parse` from config.rs).
- [x] 2.3 If the operator chose `n` for spec-sync, the wizard skips the LLM-driven-audits prompt entirely and writes no audits to config.yaml. (Skipping spec-sync also skips the LLM gate — if you don't want the cheap defensive one, you almost certainly don't want the LLM-driven ones either.)

## 3. Wizard step: LLM-driven audits gate

- [x] 3.1 If spec-sync was enabled, second question: `Enable the LLM-driven audits? [y/N]` with the list of audit slugs shown in the prompt body. Default on bare-Enter is `no`.
- [x] 3.2 If `no`: no further audit prompts. Write only the spec-sync entry to `audits.defaults`.
- [x] 3.3 If `yes`: proceed to §4 (fast-path / individual).

## 4. Wizard step: fast-path vs individual

- [x] 4.1 Third question (LLM-driven enabled): `Enable all five with recommended cadences? [Y/n]` showing the recommended cadences inline (architecture_brightline: weekly, drift_audit: weekly, missing_tests_audit: monthly, security_bug_audit: weekly, architecture_consultative: monthly).
- [x] 4.2 If `Y` (default on bare-Enter): write all five with their recommended cadences. No further prompts. Total wizard interaction: 3 questions.
- [x] 4.3 If `n`: walk each audit individually. For each: print `<slug> (<description>)`, then `Cadence: [d]aily / [w]eekly / [m]onthly / [n]ever (recommended: <rec>)`. Default on bare-Enter is the recommended cadence. Operator answering `n` (never) leaves that audit disabled. Total wizard interaction in this branch: 3 + 5 = 8 questions.

## 5. Non-interactive mode flags

- [x] 5.1 Add to `InstallArgs`: `--audits-spec-sync <disabled|daily|weekly|monthly>` (default `daily`); `--audits-llm-driven <none|recommended|all-disabled>` (default `none`); per-audit `--audit-<slug> <cadence>` flags (e.g. `--audit-architecture-brightline weekly`).
- [x] 5.2 Non-interactive resolution: if `--audits-llm-driven recommended` is set, every LLM-driven audit gets its recommended cadence UNLESS a more-specific `--audit-<slug>` flag overrides. If `--audits-llm-driven none`, all LLM-driven audits stay disabled regardless of any `--audit-<slug>` flags (the flag is the explicit master switch). If `--audits-llm-driven all-disabled`, same as `none` but the wizard prints to stdout that this was explicitly opt-out (distinguishable in IaC logs from "operator just didn't pass the flag").
- [x] 5.3 Backwards compatibility: a non-interactive invocation that doesn't pass ANY `--audits-*` flag gets the conservative default — spec-sync daily, everything else disabled. Existing IaC scripts that don't know about the new flags continue to work without surprise behavior changes.

## 6. Config assembly

- [x] 6.1 The `assemble_config(answers) -> Config` function (from the existing install wizard) gains an `audits` field on `WizardAnswers`. Populates `Config.audits.defaults` from the answers' resolved cadences. Audits with cadence `Disabled` are omitted from the map (so the rendered config.yaml only lists enabled audits — keeps the file scannable).
- [x] 6.2 `audits.settings` stays empty in the wizard's output. Operators wanting `prompt_path` / `notify_on_clean` / `extra` overrides edit config.yaml after install. `config.example.yaml` documents the schema.
- [x] 6.3 If NO audits end up enabled (operator answered `n` to spec-sync), the `audits:` block is omitted entirely from the rendered config.yaml. This matches the schema's `Option<AuditsConfig>` shape and keeps config.yaml minimal for operators who want zero audits.

## 7. Tests

- [x] 7.1 `wizard_audits_default_path_enables_spec_sync_only` — script all default answers; assert config.yaml has `audits.defaults.spec_sync_audit: daily` AND no other audit entries.
- [x] 7.2 `wizard_audits_fast_path_enables_all_six` — script "spec-sync daily, LLM y, fast-path Y"; assert all six audits in config.yaml with their recommended cadences.
- [x] 7.3 `wizard_audits_per_audit_cadence_choices_respected` — script "spec-sync weekly, LLM y, fast-path n, then explicit cadences per audit"; assert each cadence matches the operator's input.
- [x] 7.4 `wizard_audits_decline_spec_sync_skips_all_audit_prompts` — script "spec-sync n"; assert no further audit prompts were emitted (using ScriptedIo's read-count assertion) AND config.yaml has no `audits:` block.
- [x] 7.5 `non_interactive_no_audit_flags_enables_spec_sync_daily_default` — `--non-interactive` with all required existing flags but no `--audits-*`; assert config.yaml matches §7.1.
- [x] 7.6 `non_interactive_audits_llm_driven_recommended_enables_all_six` — `--non-interactive --audits-llm-driven recommended`; assert config.yaml matches §7.2.
- [x] 7.7 `non_interactive_per_audit_flag_overrides_recommended` — `--non-interactive --audits-llm-driven recommended --audit-security-bug-audit disabled`; assert security_bug is absent from config.yaml while the other four LLM-driven audits stay at their recommended cadences.
- [x] 7.8 `non_interactive_llm_driven_none_overrides_per_audit_flags` — `--non-interactive --audits-llm-driven none --audit-architecture-brightline weekly`; assert architecture_brightline is still disabled (master switch wins).

## 8. README documentation

- [x] 8.1 In the install section (under "Quick install"), add a one-paragraph note that the wizard now prompts about periodic audits, that `spec_sync_audit` is recommended on by default, and that the LLM-driven audits are gated behind one yes/no question for operators who want to defer. Cross-reference the "Configuration Reference" audits table for cadence and `extra`-knob documentation.
- [x] 8.2 In the "Configuration Reference" audits section, add a one-line note that operators who installed via the wizard already have their cadence choices applied; this section is for operators editing config.yaml directly or onboarded via source build.

## 9. Spec delta

- [x] 9.1 Add the ADDED requirement under `orchestrator-cli` titled "Install wizard configures periodic audits" per the proposal. Scenarios cover: default path (spec-sync only), fast-path (all six), individual cadences (per-audit choices), declining spec-sync (no audit block), non-interactive defaults, non-interactive recommended-with-per-audit-override, master-switch precedence.

## 10. Verification

- [x] 10.1 `cargo test` passes.
- [x] 10.2 `openspec validate install-wizard-configures-audits --strict` passes.
