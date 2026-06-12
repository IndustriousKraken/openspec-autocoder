## Why

The issues lane — the curated entry AND the public GitHub-issue ingestion — is gated by `features.issues.enabled`, OFF by default. Off-by-default is correct: unlike the chatops-verb features (inert until invoked), enabling the lane changes daemon behavior autonomously — per-iteration lane precedence (`issues > changes > audits`) AND, with `features.scout.include_issues` on, read-only LLM triage of UNTRUSTED public issue bodies that posts candidates to chatops every pass. That is automatic token spend and untrusted-input processing an operator should opt into.

But the flag is undiscoverable. `config.example.yaml` omitted it entirely, and the install wizard never mentions it — so an operator who wants the lane has no signpost and reaches for the wrong lever (e.g. assigning the issue to the bot account, which the lane never consults). The fix is discoverability, not flipping the default: document the flag (done in `config.example.yaml`) AND surface it as an explicit opt-in in the install wizard, mirroring the periodic-audits gate.

## What Changes

**Interactive gate.** The `autocoder install` wizard prompts about the issues lane during first-time install — a single yes/no gate, default NO — placed after the periodic-audits prompts and before config assembly. The prompt body states what enabling does (lane precedence; with `features.scout.include_issues` on, read-only triage of open GitHub issues into chatops candidates that a maintainer promotes with `send it`) so the choice is informed rather than a blind toggle.

**Non-interactive flag.** Non-interactive mode mirrors the gate with `--issues-lane <enabled|disabled>`, default `disabled`, so IaC scripts that predate the flag keep producing the same (lane-off) install with no surprise behavior change.

**Config assembly.** The wizard writes `features.issues.enabled: true` only when the operator opts in; declining (or omitting the flag) writes no `features.issues` entry, matching the default-off representation.

## Impact

- **Affected specs:** `orchestrator-cli` — ADD `Install wizard configures the issues lane`.
- **Affected code:** the install wizard (`cli/install.rs`) — the interactive gate, the `--issues-lane` non-interactive flag plus its validation/resolution, AND the config-assembly step that writes `features.issues.enabled`.
- **Operator-visible behavior:** first-time `autocoder install` asks whether to enable the issues lane (default no); `--issues-lane enabled` enables it non-interactively; declining or omitting leaves it off. No change to an existing install on binary upgrade (the wizard short-circuits when `config.yaml` already exists).
- **Docs:** README install section, `docs/CLI.md` (`install` flags), AND `docs/CONFIG.md` (`features.issues`) updated; `config.example.yaml` already documents the flag.
- **Non-goals:** no change to the lane's runtime behavior, the ingestion/promotion flow, or the default (`features.issues.enabled` stays `false`); `--reconfigure` is NOT extended with an `issues` section in this change (an operator toggles it post-install by editing `config.yaml` and restarting — `features.*` is restart-required, not hot-reloadable).
- **Dependencies:** builds on `Install wizard configures periodic audits` (the gate pattern) AND the issues-lane requirements (`Issues lane for corrections`, `Hybrid issue ingestion with maintainer promotion`). No unmerged dependencies.
- **Acceptance:** `cargo test` passes; `openspec validate install-wizard-issues-lane --strict` passes. Tests: the interactive default leaves the lane off AND writes no `features.issues` entry; opting in writes `features.issues.enabled: true`; `--non-interactive` with no `--issues-lane` leaves it off; `--issues-lane enabled` enables it; `--issues-lane disabled` leaves it off.
