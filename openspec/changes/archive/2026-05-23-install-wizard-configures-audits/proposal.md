## Why

The existing install wizard (shipped by `install-via-autocoder-subcommand`) prompts operators through the minimum needed for a first-run config: repo URL, branches, GitHub PAT, chatops backend, reviewer. It deliberately doesn't ask about audits — those are opt-in via the `audits:` config block, and the example YAML documents them. The thinking at the time was "keep the wizard minimum so first-time operators ship fast."

Real-world consequence: most operators never enable audits, because reading the config-reference docs and editing `audits.defaults` manually is a step that happens once-someday-after-things-are-working — which often means never. The most useful audit (`spec_sync_audit`) is cheap and defensive — no LLM cost, no risk of bad changes, just keeps the canonical specs in sync. Leaving it off-by-default for new installs means new operators silently accumulate drift starting from day one.

Additionally, the LLM-driven audits (architecture_brightline, drift, missing_tests, security_bug, architecture_consultative) have real value but real token cost. Operators want to make explicit choices about which to run and at what cadence — and the install moment is the right time to ask, when the operator already has the context loaded. Asking later means they've moved on and the decision keeps getting deferred.

Three things this change wants:

1. **`spec_sync_audit` enabled by default** at daily cadence (asked inline with a one-line description and an easy "n" override). Cheap, defensive, recommended.
2. **A single yes/no gate for the LLM-driven audits** so operators who don't want a tour can answer "n" and move on without clicking through six separate prompts.
3. **An "all enabled with recommended cadences" fast path** for operators who DO want the full audit suite, with sensible per-audit defaults.

The non-interactive mode (`--non-interactive` with flags) needs to support the same configuration so IaC users (Ansible, cloud-init) can pre-declare audit choices.

## What Changes

**New wizard step**, inserted after the existing reviewer prompt and before config-write:

```
Periodic audits — autocoder ships several optional audits that run on a configurable cadence.

  spec_sync_audit (cheap, no LLM cost, recommended ON)
    Backfills drift between archived changes and canonical openspec/specs/ files.

  Enable spec_sync_audit? Cadence: [d]aily (default) / [w]eekly / [m]onthly / [n]ever
  > d

  Enable the LLM-driven audits? [y/N]
    These call the agent CLI and have token cost. Includes:
      - architecture_brightline (file-size / module-size guidelines)
      - drift_audit (spec ↔ code drift)
      - missing_tests_audit (proposes test coverage for untested branches)
      - security_bug_audit (proposes fixes for likely security bugs)
      - architecture_consultative (advisory architecture findings)
  > n
```

If the operator answers `n` to "enable LLM-driven audits", the wizard writes only the `spec_sync_audit` entry under `audits.defaults` (per the operator's cadence choice above) and skips the other prompts. The remaining audits stay disabled by default and the operator can enable them later by editing config.yaml.

If the operator answers `y`, the wizard offers a fast path first:

```
  > y
  Enable all five with recommended cadences? [Y/n]
    Recommended cadences:
      architecture_brightline:     weekly
      drift_audit:                 weekly
      missing_tests_audit:         monthly
      security_bug_audit:          weekly
      architecture_consultative:   monthly
  > Y
```

If the operator answers `Y` (the fast path), the wizard writes all five at the recommended cadences and is done. If `n`, the wizard walks each audit individually with a one-line description and a cadence choice (with the recommended cadence shown as the default):

```
  architecture_brightline (file-size / module-size guidelines)
  Cadence: [d]aily / [w]eekly (default) / [m]onthly / [n]ever
  > [Enter for default]
```

**Each audit's prompt** includes:
- The slug (so the operator recognizes it when they see it in alerts or config.yaml later)
- A one-line description (taken from the audit's `Audit::description()` method, which this change adds if it doesn't already exist as a contract)
- Cadence options: `disabled` / `daily` / `weekly` / `monthly`, with the recommended default shown
- An "every-N-days" advanced option is NOT in the wizard; operators wanting that edit config.yaml after install

**Non-interactive mode** adds new flags to mirror the prompts:

- `--audits-spec-sync <disabled|daily|weekly|monthly>` (default `daily`)
- `--audits-llm-driven <none|recommended|all-disabled>` (the gate; defaults `none`)
- When `--audits-llm-driven recommended` is passed, all five LLM audits get their recommended cadences (same defaults as the interactive fast path)
- Individual per-audit flags `--audit-architecture-brightline <cadence>` etc. for operators who want to override one cadence within the recommended set

If `--non-interactive` is passed without ANY `--audits-*` flag, the audits default to: `spec_sync_audit: daily`, all others `disabled`. This matches the conservative interactive default and means existing IaC scripts that don't know about the new flags continue to work.

**Config writing**: the wizard appends to the assembled `Config` struct's `audits.defaults` map per the operator's choices. The example YAML's commented `audits:` block is replaced with an active block containing exactly the operator's selections — other audit slugs do NOT appear (so the file stays scannable). Operators who want to see all available audits + their `extra` knobs can refer to `config.example.yaml` which the install script downloads alongside.

**`Audit::description()` contract.** The wizard reads a one-line description per audit to render in the prompts. If the `Audit` trait doesn't already have this method, this change adds it as a required trait method with a `&'static str` return. Existing audits get a description string added to their impl. Future audits get the same.

## Impact

- Affected specs: `orchestrator-cli` — one ADDED requirement establishing the audit-prompts contract in the install wizard.
- Affected code:
  - `autocoder/src/audits/mod.rs` — add `description()` to the `Audit` trait if missing.
  - Each registered audit's impl (`brightline.rs`, `drift.rs`, `missing_tests.rs`, `security_bug.rs`, `architecture_consultative.rs`, `spec_sync.rs`) — add a `description()` returning the one-liner.
  - `autocoder/src/cli/install.rs` — new wizard step between the reviewer prompt and config-write; new `InstallArgs` flags for non-interactive mode.
  - `autocoder/src/cli/install.rs::tests` — new tests for the audit-prompt branches (spec-sync only, fast-path recommended, individual cadence per audit, non-interactive variants).
  - README — small note in the install section pointing at the new wizard step. The existing config-reference docs already explain audits in detail; the install section just references them.
- Operator-visible behavior:
  - First-time install gains 1–7 new prompts depending on answers. Fast path (skip LLM-driven audits) is two questions: spec-sync cadence + "y/n LLM-driven audits."
  - Non-interactive scripts that don't know about the new flags get the conservative default (spec_sync_audit: daily; everything else disabled) — non-breaking.
- Breaking: no.
- Acceptance: `cargo test` passes (the new wizard tests). `openspec validate install-wizard-configures-audits --strict` passes. A test that scripts "spec-sync daily, LLM-driven y, all-recommended Y" asserts the resulting `config.yaml` has all six audits with their recommended cadences.
