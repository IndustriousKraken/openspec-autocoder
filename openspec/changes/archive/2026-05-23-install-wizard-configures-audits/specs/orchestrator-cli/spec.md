## ADDED Requirements

### Requirement: Install wizard configures periodic audits
The `autocoder install` wizard SHALL prompt operators about periodic audits during first-time install, after the reviewer prompt and before the config-assembly step. The wizard offers a three-tier UX: (1) inline prompt for `spec_sync_audit` with default ON at daily cadence (cheap, defensive, no LLM cost); (2) a single yes/no gate for the LLM-driven audits (default no — operators who don't want a tour answer once and move on); (3) a fast-path "enable all five at recommended cadences" question for operators who answered yes to the gate, with per-audit walk-through as the fallback when the fast path is declined. The non-interactive mode SHALL mirror this with flags whose defaults match the conservative interactive defaults so existing IaC scripts that don't know about the new flags continue to work without behavior change.

#### Scenario: Default interactive path enables spec_sync_audit only
- **WHEN** an operator runs `autocoder install` AND accepts
  every audit-related default (bare-Enter on the spec-sync
  cadence prompt → `daily`; bare-Enter on the LLM-driven
  gate → `no`)
- **THEN** the wizard writes `audits.defaults.spec_sync_audit: daily`
  to config.yaml AND no other audit entries
- **AND** the operator's total interaction with the audits
  section is two prompts (cadence + gate)

#### Scenario: Operator declines spec_sync_audit
- **WHEN** the operator answers `n` (never) to the spec-sync
  cadence prompt
- **THEN** the wizard skips the LLM-driven-audits gate
  AND any subsequent per-audit prompts
- **AND** the rendered config.yaml omits the `audits:`
  block entirely (matching the `Option<AuditsConfig>`
  schema's `None` representation)

#### Scenario: Fast-path enables all six audits
- **WHEN** the operator chose a non-disabled cadence for
  spec-sync AND answered `y` to the LLM-driven-audits gate
  AND accepted the fast-path default `Y` on the "enable all
  five with recommended cadences" prompt
- **THEN** config.yaml contains all six audits at their
  recommended cadences:
  - `spec_sync_audit`: per the operator's spec-sync answer
  - `architecture_brightline`: weekly
  - `drift_audit`: weekly
  - `missing_tests_audit`: monthly
  - `security_bug_audit`: weekly
  - `architecture_consultative`: monthly
- **AND** total wizard interaction in this branch is three
  prompts (spec-sync cadence + LLM gate + fast-path
  acceptance)

#### Scenario: Individual cadence walk-through after declining fast-path
- **WHEN** the operator answered `y` to the LLM-driven gate
  AND `n` to the fast-path prompt
- **THEN** the wizard prompts for each of the five LLM-driven
  audits individually: slug + description + cadence choice
  (with the recommended cadence as the default)
- **AND** each audit's chosen cadence appears in
  `audits.defaults` UNLESS the operator chose `never`
  (those audits are omitted)
- **AND** the resulting config.yaml's audit count matches
  the operator's non-disabled choices (spec-sync + each LLM
  audit the operator did NOT decline)

#### Scenario: Non-interactive defaults match conservative interactive defaults
- **WHEN** an operator runs `autocoder install --non-interactive`
  with all the existing-spec's required flags AND NO new
  `--audits-*` flags
- **THEN** config.yaml contains exactly
  `audits.defaults.spec_sync_audit: daily` (the
  conservative default matching the interactive default-default)
- **AND** existing IaC scripts (Ansible playbooks, cloud-init,
  etc.) that pre-date this change continue to produce a
  working install without surprise behavior change

#### Scenario: Non-interactive recommended preset
- **WHEN** an operator runs
  `autocoder install --non-interactive --audits-llm-driven recommended`
  with all other required flags
- **THEN** config.yaml contains all six audits at their
  recommended cadences (same as the interactive fast-path)
- **AND** no per-audit `--audit-<slug>` flag is required

#### Scenario: Non-interactive per-audit override within recommended preset
- **WHEN** the operator passes
  `--audits-llm-driven recommended --audit-security-bug-audit disabled`
- **THEN** four of the five LLM-driven audits get their
  recommended cadences AND `security_bug_audit` is omitted
  from config.yaml (treated as disabled)
- **AND** spec-sync follows its own `--audits-spec-sync`
  flag (or default `daily` if unset)

#### Scenario: --audits-llm-driven none master switch overrides per-audit flags
- **WHEN** the operator passes
  `--audits-llm-driven none --audit-architecture-brightline weekly`
- **THEN** architecture_brightline is NOT enabled (the
  master switch wins)
- **AND** the rendered config.yaml has no
  architecture_brightline entry
- **AND** the wizard emits a one-line stdout note explaining
  that the per-audit flag was overridden by the master
  switch (so IaC logs distinguish "operator opted-out
  explicitly" from "operator forgot to set the flag")

#### Scenario: Audit description rendering
- **WHEN** the wizard prompts for any audit's cadence
- **THEN** the prompt body includes the audit's
  `description()` string (a one-line operator-facing
  description, ≤ 80 chars, from the `Audit` trait)
- **AND** the description is enough for an operator to
  recognize the audit in subsequent chatops alerts or
  config.yaml lines without needing to consult external
  documentation
