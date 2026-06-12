# orchestrator-cli — delta for install-wizard-issues-lane

## ADDED Requirements

### Requirement: Install wizard configures the issues lane
The `autocoder install` wizard SHALL prompt operators about the issues lane during first-time install, after the periodic-audits prompts AND before the config-assembly step, as a single yes/no gate defaulting to NO. Because enabling the lane changes daemon behavior autonomously — per-iteration unit selection becomes `issues > changes > audits`, AND with `features.scout.include_issues` enabled the bot triages open GitHub issues read-only into chatops candidates a maintainer promotes with `send it` — the gate is an explicit opt-in, NOT a default-on feature, AND the prompt body SHALL state these effects so the operator decides informed rather than toggling blind. The wizard SHALL write `features.issues.enabled: true` to config.yaml ONLY when the operator opts in; declining SHALL write no `features.issues` entry, matching the schema's default-off representation. The non-interactive mode SHALL mirror the gate with a `--issues-lane <enabled|disabled>` flag whose default (`disabled`) matches the conservative interactive default, so IaC scripts that predate the flag continue to produce a lane-off install without behavior change.

#### Scenario: Default interactive path leaves the issues lane off
- **WHEN** an operator runs `autocoder install` AND accepts the issues-lane default (bare-Enter on the gate → no)
- **THEN** the rendered config.yaml contains no `features.issues` entry
- **AND** the issues lane is off (the schema's default-off representation)

#### Scenario: Operator opts in interactively
- **WHEN** the operator answers `y` to the issues-lane gate
- **THEN** the rendered config.yaml contains `features.issues.enabled: true`

#### Scenario: Non-interactive default leaves the issues lane off
- **WHEN** an operator runs `autocoder install --non-interactive` with all required flags AND no `--issues-lane` flag
- **THEN** the rendered config.yaml contains no `features.issues` entry
- **AND** IaC scripts that pre-date this flag produce the same lane-off install as before

#### Scenario: Non-interactive enable
- **WHEN** an operator runs `autocoder install --non-interactive --issues-lane enabled` with all other required flags
- **THEN** the rendered config.yaml contains `features.issues.enabled: true`

#### Scenario: Non-interactive explicit disable
- **WHEN** an operator passes `--issues-lane disabled`
- **THEN** the rendered config.yaml contains no `features.issues` entry (same as the default)
