## Why

autocoder today processes only the OpenSpec change queue. There's no facility for "periodic, repository-wide audits" — dependency triage, drift detection, missing-tests review, architecture commentary. Each of those is a sibling task: runs on a cadence, may emit chatops findings, may write new OpenSpec changes, but is structurally distinct from the implementer-drives-a-change flow.

Rather than special-case each one into the polling loop, this change adds a single audit framework: cadence config, scheduler integration, per-audit sandbox profile, post-hoc diff check, audit-run logging, and a trait that concrete audits implement. The framework ships with one concrete audit (architecture-brightline, pure-code, no LLM) so the change is fully runnable; subsequent audits plug into the same framework.

## What Changes

- **ADDED capability:** `orchestrator-cli` gains "Periodic audit framework" requirements: cadence schema, scheduler integration, state file format, sandbox profile selection, audit-run logging, default prompts mechanism, and chatops output formatting helpers.
- **Config:** new `audits:` block at the top level with:
  - `audits.defaults.<audit_type>: Cadence` — global default (`disabled` | `daily` | `every-N-days` | `weekly` | `monthly` | `quarterly`).
  - Per-repo override: `repositories[].audits.<audit_type>: Cadence`.
  - Default for every audit is `disabled`. Operators opt in.
- **Code:**
  - `Audit` trait: `audit_type() -> &'static str`, `requires_head_change() -> bool`, `run(&self, ctx: AuditContext) -> Result<AuditOutcome>`.
  - `AuditOutcome`: `NoFindings | Reported(Vec<Finding>) | SpecsWritten(Vec<String>)`.
  - `AuditScheduler`: invoked per iteration AFTER `recreate_branch` AND BEFORE `list_pending`, so any specs an audit writes are picked up in the same pass's queue walk.
  - State file: `<workspace>/.audit-state.json` recording `last_run_at` + `last_run_sha` per audit type. The change-guard (`requires_head_change`) suppresses re-runs when HEAD hasn't moved.
  - Sandbox profile selection: each audit declares `WritePolicy::{None, OpenSpecOnly, Approved}`. Audits with `WritePolicy::None` run with a Read+Glob+Grep+Bash-only sandbox AND a post-hoc `git status --porcelain` check (any non-empty diff after the audit means failure + chatops alert + state file unchanged so it re-runs next time, allowing operator intervention).
  - Audit-run log per invocation at `/tmp/autocoder/logs/<workspace-basename>/audits/<audit_type>-<timestamp>.log` containing the prompt (if any) and the raw audit output.
- **Concrete audit bundled:** `architecture-brightline`. Pure-code metrics; no LLM. Surface: file line counts, module pub-item counts, identical function signatures across modules, dead public-API items. Output: chatops post listing findings (or "no findings"). `requires_head_change = true`. `WritePolicy::None`.
- **Default prompts mechanism:** for future LLM-based audits. Each audit type with an LLM prompt SHALL have an embedded default template at compile time AND accept `audits.<audit_type>.prompt_path: Option<PathBuf>` to override. (No LLM audits ship in this change; the mechanism is laid out for the next audit specs to use.)

## Impact

- Affected specs: `orchestrator-cli` (ADDED requirements only; no existing requirement is modified).
- Affected code: `autocoder/src/audits/{mod.rs, scheduler.rs, state.rs, brightline.rs}` (new module), `autocoder/src/config.rs` (audit config schema), `autocoder/src/polling_loop.rs` (one scheduler call per iteration), `autocoder/src/cli/run.rs` (audit registry wiring).
- Behavior change: with default config (all audits `disabled`), zero behavioral difference. Operators opt in explicitly.
- Budget: brightline runs in milliseconds, no Claude tokens. Other audits, when added, declare their own cost characteristics.
- Breaking: no. New optional config block; absent block = all audits disabled.
