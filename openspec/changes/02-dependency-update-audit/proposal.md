## Why

Dependabot opens PRs against the upstream repository, but in fork-PR mode (and often even in direct-push mode) the autocoder bot lacks rights to approve/merge on upstream. The operator typically still has to triage each Dependabot PR by hand. With dozens of repos under autocoder's care, this becomes the single largest source of unattended manual work.

This audit narrows the problem: instead of trying to merge upstream PRs (which often fails for auth reasons), it lists Dependabot PRs on the bot's own fork, validates each diff against a strict "safe shape" filter (version-string bumps only, no script/URL changes), and approves the safe ones. The operator merges upstream when convenient; the bot's approval signals "we've sanity-checked this on our fork".

## What Changes

- **ADDED capability:** `orchestrator-cli` gains a "Dependency update triage audit" requirement.
- **Audit:** registered as `dependency_update_triage`. `requires_head_change() = false` (registries update independently of HEAD). `WritePolicy::None` (no workspace writes; the audit only interacts with GitHub via API).
- **Algorithm per run:**
  1. List open PRs on the configured fork remote (when `github.fork_owner` is set) whose author is `dependabot[bot]` OR `dependabot-preview[bot]`. When `fork_owner` is unset, fall back to listing on the upstream repo (operators with full upstream rights).
  2. For each PR, fetch the unified diff via GitHub API.
  3. Apply the "safe shape" filter:
     - Every modified file is a known manifest type (e.g. `Cargo.toml`, `package.json`, `package-lock.json`, `requirements.txt`, `pyproject.toml`, `*.csproj`, `packages.lock.json`, `go.sum`, `go.mod`, `Gemfile.lock`, etc.).
     - Within each manifest, only version-string fields change; no new package entries, no removed entries, no URL/registry field changes.
     - For lockfiles, only hash + version fields change.
     - The PR adds no `postinstall`, `preinstall`, `prepublish`, `scripts.*` entries that didn't exist before.
  4. Safe PRs receive a GitHub `APPROVE` review with a one-line body identifying the audit. Unsafe PRs get a chatops post with the reason ("contains new package", "modifies non-manifest file", etc.) so the operator knows to look.
- **Per-audit knob:** `audits.dependency_update_triage.max_approvals_per_run: u32` (default `5`). Caps the number of approvals per audit invocation; remaining safe PRs wait until the next run.
- **Per-audit knob:** `audits.dependency_update_triage.fork_remote_name: String` (default `"fork"`). Matches the existing fork-PR mode remote name.

## Impact

- Affected specs: `orchestrator-cli` (one ADDED requirement).
- Affected code: `autocoder/src/audits/dependency_update.rs` (new), `autocoder/src/github.rs` (helpers for listing Dependabot PRs and submitting approval reviews if not already present).
- Cost: each run = 1 list-PRs call + N diff-fetch calls + M approval calls. Cheap compared to LLM audits.
- Operator-visible behavior: with the audit configured at e.g. `daily`, each day's first qualifying iteration approves up to `max_approvals_per_run` Dependabot PRs on the fork and reports any unsafe ones via chatops.
- Foundation dependency: requires `periodic-audits-foundation` to be applied first (provides the registry, scheduler, cadence parsing, etc.).
- Breaking: no. Default cadence `disabled` per the foundation's opt-in pattern.
