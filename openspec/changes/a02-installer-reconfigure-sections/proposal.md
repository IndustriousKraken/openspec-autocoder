## Why

The install wizard today is an all-or-nothing artifact. First run prompts for repository URL, branches, poll interval, GitHub token env var, chatops backend + channel, reviewer provider + model, audit cadences. After that, `autocoder install` exits silently if config exists — there is no way to re-run a subset. An operator who wants to change just one section (e.g., switch a single audit from `disabled` to `weekly`) must either edit `config.yaml` by hand (no validation, no scaffolding for new field shapes) or wipe and re-do the entire wizard.

Editing YAML by hand is the right answer for advanced operators. For everyone else, a `--reconfigure <section>` flag that re-prompts only one section AND patches the existing config in place is the missing middle ground. It's the install wizard fulfilling its role as a config-help tool — re-runnable, scoped, and respecting prior choices.

## What Changes

**`InstallArgs` gains `--reconfigure <section>`.** Accepted values: `audits`, `reviewer`, `chatops`. The flag is mutually exclusive with `--non-interactive` and with the prefill flags (`--repo-url`, `--token-env-var`, etc.) — reconfigure is by definition interactive and section-scoped.

**`--reconfigure` repurposes the existing-install detection from `a01`.** The flag SHALL only operate against a detected existing install:

- Server mode: read the `--config <path>` from `systemctl show autocoder.service`. Fall back to `/etc/autocoder/config.yaml` if the unit-probe returns no path.
- Dev mode: fall back to `~/.config/autocoder/config.yaml` (no probe).
- If neither resolves to an existing file, the flag exits non-zero with a "no existing install to reconfigure" diagnostic and a pointer at `install.sh` for first-time setup.

**Per-section behavior:**

- **`--reconfigure audits`**: re-runs the audit-walkthrough prompts (the same `run_audit_prompts` the first-run wizard uses). Loads the current cadences from the existing config, displays each as the default, and prompts for each audit individually. Applies the operator's answers via in-place YAML patch: parse the existing `config.yaml`, replace ONLY the `audits.defaults.*` subtree, write back. Comments outside the patched subtree are preserved (serde_yaml round-trips values but not comments; the patch operates on the parsed structure but the surrounding lines are read-modify-write at the line level for top-level keys we don't touch). On error (parse failure, unexpected schema), the wizard refuses the patch and leaves the file unchanged.

- **`--reconfigure reviewer`** and **`--reconfigure chatops`**: re-run the relevant subsection of the wizard, generate the proposed new YAML subtree, AND show the operator a unified diff against the current config before applying. If the operator declines, the file is unchanged. If they accept, the patch lands. The diff-confirm shape (instead of audits' immediate-write shape) reflects that these sections are more likely to carry hand-edited comments, custom keys (e.g. `api_base_url` overrides for OpenRouter), and care about exact formatting — surfacing the diff lets the operator catch losses before they happen.

**Per-section excluded knobs.** Some fields are explicitly NOT in the reconfigure surface:

- **`repositories`**: use `autocoder reload` (hot-applies add/remove without daemon restart). Reconfigure deliberately does not grow into this space.
- **`paths.*`**: relocating data directories is a destructive operation that needs explicit operator action AND a daemon restart. Out of scope for the wizard.
- **`executor.*`**: every executor knob requires a restart (it's the only block that does, per the `reload` requirements). Reconfigure stays in the hot-applicable space.
- **`audits.settings.*.prompt_path` and `audits.settings.*.extra.*`**: advanced per-audit overrides. Wizard handles only `audits.defaults.*` cadences. Operators editing prompts or thresholds edit YAML directly.

**Restart guidance.** After a successful patch, the wizard prints whether the change is hot-applicable (`audits`, `reviewer`, `chatops` all are) AND the command to apply it: `sudo -u autocoder autocoder reload` — same socket the `reload` subcommand uses. No automatic reload — the operator decides when to apply.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one ADDED requirement: `Install wizard --reconfigure flag re-runs one section against an existing install`. Names the three accepted section values, the mutual-exclusion with `--non-interactive`, the per-section behavior (in-place patch for audits; diff-confirm for reviewer / chatops), the excluded knobs, and the post-patch restart guidance.
  - `project-documentation` — one ADDED requirement: `DEPLOYMENT.md documents the --reconfigure verb and its section scope`. Documents the verb, the three values, the exclude-list rationale (repos use `reload`; executor needs restart), and the expected workflow (`autocoder install --reconfigure audits` → answer prompts → `autocoder reload`).
- **Affected code:**
  - `autocoder/src/cli/install.rs`:
    - Add `pub reconfigure: Option<ReconfigureSection>` to `InstallArgs` (mutually exclusive with `--non-interactive` and the prefill flags via clap's `conflicts_with`).
    - New enum `pub enum ReconfigureSection { Audits, Reviewer, Chatops }`.
    - New entry point `pub async fn execute_reconfigure(args: InstallArgs, ...)` that takes the existing-install path (from `a01`'s probe OR the default-path fallback) and dispatches to the per-section handler.
    - New helpers: `reconfigure_audits(existing_config: &Config) -> Result<Config>` (re-prompts via `run_audit_prompts`, returns the patched config); `reconfigure_reviewer(existing_config: &Config) -> Result<Config>`; `reconfigure_chatops(existing_config: &Config) -> Result<Config>`.
    - New helper `apply_in_place_patch(config_path: &Path, new_config: &Config) -> Result<()>` for audits' immediate-write path. Reads the existing YAML, parses, replaces only the relevant subtree, serializes back, atomic temp-file-then-rename. Preserves the file's existing top-level key order where possible.
    - New helper `confirm_diff_and_apply(config_path: &Path, new_config: &Config, io: &mut dyn WizardIo) -> Result<bool>` for reviewer / chatops' diff-confirm path. Generates the proposed new YAML, computes a unified diff against current, prints it, prompts `Apply this patch? [y/N]`. Returns the operator's answer; only writes on accept.
    - The diff is computed via the `similar` crate (or `imara-diff`); pick whichever is already a transitive dep, else the smaller and add-version per `check-current-versions-not-training`.
  - `docs/DEPLOYMENT.md` — extend the section landing in `a01` with a `--reconfigure` paragraph documenting the three values, the per-section behavior, and the post-patch `autocoder reload` step.
  - `docs/CLI.md` — extend the `## \`install\`` section (or add one if missing) to document the `--reconfigure` flag.
- **Operator-visible behavior:**
  - `autocoder install --reconfigure audits` re-prompts cadences with the current values as defaults, writes the new cadences to `audits.defaults.*` in place, prints `Run \`sudo -u autocoder autocoder reload\` to apply.`
  - `autocoder install --reconfigure reviewer` re-prompts provider + model + api-key-source, generates the proposed `reviewer:` block, shows a unified diff, prompts for confirmation. Same shape for `--reconfigure chatops`.
  - `autocoder install --reconfigure audits` against a host with no existing install exits non-zero with the diagnostic from above.
- **Breaking:** no. The new flag is additive. Existing `autocoder install` invocations (with or without `--upgrade`) behave identically to pre-spec.
- **Acceptance:** `cargo test` passes; `openspec validate a02-installer-reconfigure-sections --strict` passes. Three new integration tests, one per section, exercise the full re-prompt → patch path against a temp config + a `ScriptedIo` answer queue.
