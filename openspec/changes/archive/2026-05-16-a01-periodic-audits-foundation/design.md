## Decisions

### Trait + registry, not per-audit hardcoding
A `pub trait Audit` decouples scheduling/sandbox/logging from audit-specific logic. The registry is a `Vec<Arc<dyn Audit>>` built at startup. Adding an audit = one trait impl + one `register` call. Avoids special-cases sprawled across the polling loop.

### Audits run BEFORE list_pending, not after
A spec-writing audit (security, missing-tests) that creates `openspec/changes/<name>/` and commits should have its output picked up by the same iteration's `walk_queue`. Running audits before `list_pending` (after `recreate_branch`, so the working tree is on a clean agent-q) means the new change directory is visible to `list_pending` and immediately enters the implementation flow. Net effect: one PR contains both the spec creation commit and the implementation commit, not two PRs across two iterations.

Report-only audits (drift, brightline, architecture-consultative) don't write to the workspace, so this ordering is invisible to them.

### State file at `.audit-state.json` (separate from `.alert-state.json`)
Different cadence (audits run every N days; alerts throttle on 24h windows). Different schema. Different lifecycle. Keep them apart so neither has to grow union types.

### `requires_head_change` is a per-audit declaration, not a flag in state
The dependency audit returns `requires_head_change = false`: package registries update independently of HEAD. Every other audit returns `true`: there's nothing to analyze if the code hasn't changed. The framework asks the audit, not the state file.

### Three WritePolicy levels, not a boolean
- `None`: report-only audits (brightline, drift, architecture-consultative). Sandbox blocks Write/Edit. Post-hoc diff check enforces "no writes". Any leak means the audit fails for that iteration.
- `OpenSpecOnly`: spec-writing audits (security, missing-tests). Sandbox allows Write/Edit. Post-hoc check rejects diffs whose paths are not under `openspec/changes/`.
- `Approved`: full write access. Reserved for future audits with broader scope; not used by any audit landing in the foundation.

Belt-and-suspenders: the sandbox declares the tool restrictions to the CLI; the post-hoc diff check catches anything that slips through (creative bash use, etc.).

### Audit-run logs are per-invocation, not per-audit-type
Each audit run gets a fresh timestamped log file at `/tmp/autocoder/logs/<basename>/audits/<type>-<timestamp>.log`. Operators tracking down "why did the consultative audit say X this week" can read the exact raw output. Rotation/cleanup is not in this change — operators handle it via the same mechanism they use for `/tmp/autocoder/logs/<basename>/<change>.log`.

### Default-off, opt-in per audit
Existing patterns (start_work, failure_alerts, pr_opened) all default ON. Audits flip that because (a) some are expensive in Claude tokens, (b) noise tolerance varies wildly across operators, (c) operators who deploy autocoder will see the new `audits:` config block in the README and choose explicitly.

### Cadence schema is enum, not duration
`disabled | daily | every-N-days(u32) | weekly | monthly | quarterly`. Free-form `Duration` invites typos and operator confusion ("is 86400s a day?"). The enum is parseable from short YAML strings: `"disabled"`, `"daily"`, `"every-3-days"`, `"weekly"`, `"monthly"`, `"quarterly"`.

### Chatops findings format
A `Finding` has `severity: enum`, `subject: String`, `body_excerpt: String`, `anchor: Option<String>` (file:line if available). Chatops output renders as a compact bullet list with a one-line summary header. Long findings truncate to a configurable excerpt length (default 200 chars per finding); the full content lives in the audit-run log.

## Open Questions

None blocking. Cost characteristics of LLM audits will be observed once those land; the framework doesn't need to model cost ahead of time.
