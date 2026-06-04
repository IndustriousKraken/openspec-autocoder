# Operating Notes

This document is organized so day-to-day workflow content comes first, recovery flows next, configuration reference in the middle, and automatic-mechanism internals at the end. Use your markdown viewer's outline pane to jump to a section.

- **Day-to-day operations** — periodic audits, on-demand audit triggers, PR-comment revisions.
- **Recovery workflows** — perma-stuck changes, spec-needs-revision, queue-blocking policy, `rewind`.
- **Operating the daemon** — runtime config reload, rebuilding canonical specs, workspace paths, multi-repo setup, polling cadence, queue order, startup preflight, fork-recreation policy.
- **Internals & automatic recovery** — workspace-deleted recovery, partial-clone self-heal, dirty-workspace auto-recovery, busy marker, per-change run log shape, pre-flight checks, self-heal for already-implemented changes, skipping iterations while a PR is open, migrations.

## Periodic audits

Beyond the OpenSpec change queue, autocoder runs a periodic-audit framework: a set of registered audits that fire on per-audit cadences, write per-invocation logs, and (depending on the audit) post chatops findings or write new OpenSpec changes that feed back into the queue.

The framework is **default-off**. With no `audits:` block in the config, every registered audit's effective cadence resolves to `disabled` and the daemon behaves exactly as it did before the framework existed. Operators opt in explicitly per audit.

**Registered audit type names:**

| Slug | What it does | LLM | Default cadence | WritePolicy |
|---|---|---|---|---|
| `architecture_brightline` | Pure-code metrics — file size, duplicate signatures across files. Surfaces oversize files and accidental copies. | No | `disabled` (opt-in via `audits.defaults` or per-repo) | `None` (read-only) |
| `drift_audit` | Invokes the wrapped agent CLI (typically `claude`) with a read-only sandbox (`Read`, `Glob`, `Grep`, `Bash`) and a drift-detection prompt. The agent compares each requirement in `openspec/specs/<capability>/spec.md` against observable code behavior and emits structured findings. Triggers on HEAD change at the configured cadence. Purely **advisory** — never modifies code or specs; the operator decides whether each finding becomes a code-fix change, a spec-fix change, or is dismissed. | Yes | `disabled` (opt-in via `audits.defaults` or per-repo) | `None` (read-only; sandbox blocks `Write`/`Edit`, post-hoc diff check reverts any sneaky writes) |
| `missing_tests_audit` | Invokes the wrapped agent CLI with a sandbox that allows `Write` and `Edit` under `openspec/changes/` only, plus the read tools. The agent surveys the source tree, identifies uncovered error paths / branches without assertions / obvious edge cases, and creates up to `max_proposals_per_run` (default `2`) new OpenSpec change directories under `openspec/changes/tests-*` proposing tests to fill those gaps. The audit validates each new change via `openspec validate --strict`, rejects invalid ones (deletes the directory), and commits the valid ones to the agent branch as `audit: missing-tests proposals (N change(s))`. Returns `AuditOutcome::SpecsWritten(names)`; per `a12-changes-have-precedence-over-audits`, the new changes wait for the NEXT iteration's `walk_queue` (audits run AFTER the pending queue walk), so the audit's creation commits ship in iteration N's PR and the implementer's commits ship in iteration N+1's PR. **Additive only:** the prompt forbids deleting or modifying existing tests (except factually broken ones). All produced changes use the `tests-` prefix so operators recognize audit-produced work at a glance. Triggers on HEAD change at the configured cadence. | Yes | `disabled` (opt-in via `audits.defaults` or per-repo) | `OpenSpecOnly` (sandbox allows `Write`/`Edit`; post-hoc diff check reverts anything outside `openspec/changes/`) |
| `security_bug_audit` | Invokes the wrapped agent CLI with the same `OpenSpecOnly` sandbox as `missing_tests_audit`, but with a security-and-bug-detection prompt. The agent surveys the source tree for high-confidence security issues (injection, auth/authz mistakes, hard-coded secrets, unsafe deserialization, missing input validation, race conditions, resource leaks) and likely bugs (off-by-one, wrong operator, mishandled None/null, missing error propagation, panicking on attacker-controlled input). For each confirmed finding it creates an OpenSpec change directory under `openspec/changes/fix-*` (bug fixes) or `openspec/changes/secure-*` (security hardening), each describing the fix the implementer should make. Up to `max_proposals_per_run` (default `2`) per invocation. The audit validates each new change via `openspec validate --strict`, rejects invalid ones, and commits the valid ones as `audit: security-bug proposals (N change(s))`. Returns `AuditOutcome::SpecsWritten(names)`; per `a12-changes-have-precedence-over-audits`, the new changes wait for the NEXT iteration's `walk_queue` (audits run AFTER the pending queue walk), so the audit's creation commits ship in iteration N's PR and the implementer + reviewer pipeline catches any LLM mistakes in iteration N+1's PR. The prompt aggressively filters low-confidence findings — a false positive becomes wasted implementer work. **Operator warning:** this audit can be noisy in early iterations on an unfamiliar codebase. Monitor the first few invocations and tighten the prompt (or disable the audit) if the false-positive rate is high. Triggers on HEAD change at the configured cadence. | Yes | `disabled` (opt-in via `audits.defaults` or per-repo) | `OpenSpecOnly` (sandbox allows `Write`/`Edit`; post-hoc diff check reverts anything outside `openspec/changes/`) |
| `architecture_consultative` | Invokes the wrapped agent CLI with a read-only sandbox (`Read`, `Glob`, `Grep`, `Bash`) and a *consultative* architecture prompt. The agent surveys the codebase and emits 0-5 anchored observations phrased as questions — "Should X be its own module?", "Is the boundary between Y and Z still meaningful?" — each tied to a specific `file:line` range and a one-paragraph body of context. Purely **advisory**: the operator decides which (if any) questions are worth acting on. The prompt's anti-pattern list specifically forbids the failure modes consultative LLMs are prone to: do NOT suggest microservices, do NOT suggest a different language, do NOT suggest new infrastructure dependencies, do NOT suggest team-of-50 patterns (event sourcing, CQRS, hexagonal overlays), do NOT suggest stylistic refactorings, and do NOT suggest changes that would add more code than they remove. The prompt is language-agnostic and explicitly tolerates polyglot codebases. The audit returns `Err` if the agent emits more than 5 findings — silent truncation would obscure prompt misbehavior. **Cadence intent:** designed for `monthly` or `quarterly` cadence; daily/weekly invocations produce noise. **Operator guidance on noise:** if the audit output is too noisy, tighten the prompt (override at `audits.settings.architecture_consultative.prompt_path`) before reaching for `disabled` — the anti-pattern list exists specifically to mitigate common LLM failure modes, so if output still misfires, the prompt is where to fix it. Triggers on HEAD change at the configured cadence. | Yes | `disabled` (opt-in via `audits.defaults` or per-repo) | `None` (read-only; sandbox blocks `Write`/`Edit`, post-hoc diff check reverts any sneaky writes) |
| `documentation_audit` | Invokes the wrapped agent CLI with a read-only sandbox (`Read`, `Glob`, `Grep`, `Bash`) and a three-check documentation prompt. The audit detects three orthogonal classes of documentation defect: **coverage** (code or canonical-spec features with operator-visible surface area that user-facing docs do not mention), **stale references** (docs that name code symbols, CLI verbs, config fields, or chatops verbs that no longer exist), and **organization** (oversize READMEs, docs pages over the configured length without a TOC, user-driving content buried below admin material, capabilities mentioned only in CHANGELOG, two pages covering the same topic without cross-linking). Findings ship as `AuditOutcome::Reported`; severities are `low` or `medium` only (the audit deliberately does not emit `high` — documentation drift is rarely emergency-grade). Operators act on findings via `@<bot> send it` in the threaded notification, which (as of a43) produces a spec-only PR: the triage agent captures the doc fixes as `tasks.md` items in a new `openspec/changes/<slug>/` change rather than editing docs directly, and the implementer makes the edits on the next iteration after the operator merges the spec PR. `extra` knobs: `readme_max_lines` (default `200`) and `page_max_lines_without_toc` (default `500`). **Default cadence in the install wizard's fast-path: `monthly`** — docs drift on a slower timescale than code; weekly is overkill for most repos. Triggers on HEAD change at the configured cadence. | Yes | `monthly` (fast-path recommendation; `disabled` until enabled) | `None` (read-only; sandbox blocks `Write`/`Edit`, post-hoc diff check reverts any sneaky writes) |

Each audit declares a `WritePolicy`:

- **`None`** — sandbox blocks `Write`/`Edit`; after `run()` returns the framework runs `git status --porcelain -uall` and asserts the workspace is clean. Any unexpected diff is treated as failure: the state file is NOT updated (so the cadence retriggers on the next iteration), the diff is reverted via `git reset --hard HEAD` + `git clean -fd`, and a throttled chatops alert under the `audit_write_policy_violation` category is posted.
- **`OpenSpecOnly`** — sandbox allows `Write`/`Edit`; after `run()` returns every modified or new path must begin with `openspec/changes/`. A diff outside that prefix triggers the same failure handling.
- **`Approved`** — full write access. Reserved for future audits with broader scope; not used by any audit shipped today.

**Cadence configuration:**

```yaml
audits:
  defaults:
    architecture_brightline: weekly      # disabled | daily | every-N-days | weekly | monthly | quarterly
    drift_audit: weekly                  # spec/code alignment audit; HEAD-change gated
    missing_tests_audit: weekly          # propose OpenSpec changes to fill test-coverage gaps; HEAD-change gated
    security_bug_audit: weekly           # propose OpenSpec changes for confirmed security issues and bugs; HEAD-change gated
    architecture_consultative: monthly   # consultative LLM architecture read; HEAD-change gated; recommended monthly/quarterly
    documentation_audit: monthly         # docs coverage / stale-reference / organization audit; HEAD-change gated; install fast-path default
  settings:
    architecture_brightline:
      notify_on_clean: false             # silence is success; set true for an explicit ✅ post each clean run
      extra:
        file_lines_threshold: 800        # override the brightline default (800)
    drift_audit:
      prompt_path: null                  # path to a markdown file overriding the embedded default prompt; null → embedded prompt
      notify_on_clean: false             # true → post a brief "no findings" chatops message on clean runs
    missing_tests_audit:
      prompt_path: null                  # path overriding the embedded prompts/missing-tests-audit.md; null → embedded prompt
      notify_on_clean: false             # missing-tests is a spec-writing audit (SpecsWritten outcome is silent regardless); this only affects the rare error case
      extra:
        max_proposals_per_run: 2         # cap on the number of new openspec/changes/tests-* directories created per invocation (default 2)
    security_bug_audit:
      prompt_path: null                  # path overriding the embedded prompts/security-bug-audit.md; null → embedded prompt
      notify_on_clean: false             # security-bug is a spec-writing audit (SpecsWritten outcome is silent regardless); this only affects the rare error case
      extra:
        max_proposals_per_run: 2         # cap on the number of new openspec/changes/fix-*|secure-* directories created per invocation (default 2)
    architecture_consultative:
      prompt_path: null                  # path overriding the embedded prompts/architecture-consultative.md; null → embedded prompt. If the audit's output is noisy, tighten the prompt here before disabling the audit.
      notify_on_clean: false             # true → post a brief "no findings" chatops message when the agent emits zero questions
    documentation_audit:
      prompt_path: null                  # path overriding the embedded prompts/documentation-audit.md; null → embedded prompt
      notify_on_clean: false             # true → post a brief "no findings" chatops message when the audit returns zero findings
      extra:
        readme_max_lines: 200            # threshold for the "README too long" organization finding
        page_max_lines_without_toc: 500  # threshold for the "page too long without TOC" finding

repositories:
  - url: "git@github.com:my-org/repo.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300
    audits:
      architecture_brightline: every-3-days   # per-repo override of the global default
```

Per-repo entries under `repositories[].audits` override the corresponding `audits.defaults` entry for that repository only. An audit name that does not match a registered slug fails config validation at startup with a list of the known names; this prevents typos silently disabling an audit.

**When audits fire:** Each polling iteration, after the pending queue walk completes AND BEFORE the push+PR step. The pending queue walk runs FIRST, then the audit phase runs on whatever budget is left. This ordering prevents an audit storm — many `requires_head_change` audits becoming eligible simultaneously after a HEAD change — from monopolizing the daemon for hours and blocking pending changes from reaching the implementer.

**Per-iteration audit bound (`audits.max_audits_per_iteration`).** Even with the change-precedence ordering above, an iteration in which many audits are eligible at once can still get bogged down running them back-to-back. The audit framework therefore caps the number of audits that run per iteration. The default is `1` — even when 5 audits become eligible after a HEAD change unblocks every `requires_head_change` audit, only the first (in declaration order: `architecture_brightline`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`, `architecture_consultative`) runs this iteration; the rest defer to subsequent iterations. With a 5-minute poll interval, a flood of 5 eligible audits drains in roughly 25 minutes of elapsed wall-clock — staggered across iterations that also continue to process pending changes — instead of one iteration running all five sequentially. Override with `audits.max_audits_per_iteration: N` for faster drainage during onboarding or after a major refactor (e.g. `3` is a typical onboarding value); the trade-off is longer per-iteration wall-clock. Values above the number of registered audits clamp at the registry count with a startup WARN log. Value `0` is permitted and disables audits behaviourally (every iteration skips the audit phase — useful for diagnostics or temporary silencing). **On-demand queued runs count against the bound:** if an operator queues several audits via `@<bot> audit <name>`, they drain one per iteration at the default `1` — the queued audits run first within the iteration (preserving their priority over cadence-driven runs), but each one consumes a slot, so an operator queuing 3 audits sees one run per iteration over the next 3 polling cycles. The bound is named in the startup log line `audits configured: <list>; max_per_iteration=<N>`.

**Audit-to-implementation delay (one iteration).** A spec-writing audit (`AuditOutcome::SpecsWritten(...)`) creates `openspec/changes/<name>/` AND commits it on the agent branch, but the new pending changes do NOT feed THIS iteration's queue walk — it already completed before the audit ran. They sit on disk as pending and are picked up by the NEXT iteration's `list_pending`. The operator-visible effect: the audit's creation commits ship in iteration N's PR (just the new proposal directories); the implementer's commits for those generated changes ship in iteration N+1's PR. The two phases become separable PRs — reviewers see proposal contents before implementation and can `@<bot> revise <text>` the proposals before the implementer runs in the next iteration.

**`requires_head_change` semantics:** Audits that compute over the codebase (like `architecture_brightline`, `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`, and `documentation_audit`) declare `requires_head_change = true`; the scheduler skips them when the base-branch HEAD SHA matches the recorded `last_run_sha`, regardless of cadence. Audits whose inputs are external (package registries, GitHub PR lists) return `false` and run on cadence alone.

**Audit-run logs:** Every invocation (success, failure, violation) writes a timestamped log file at:

```
/tmp/autocoder/logs/<workspace-basename>/audits/<audit_type>-<UTC-RFC3339-with-Z>.log
```

The log contains: the audit type, workspace path, start/end timestamps, the resolved cadence, the last-run record (if any), the prompt (for LLM audits), the raw audit output, every finding's full body, and the final `AuditOutcome` variant. The directory is created on first use. Cleanup is operator-driven (same model as the per-change run logs).

**State file:** Per-workspace audit run state lives at `<workspace>/.audit-state.json`. The file is autocoder bookkeeping and is registered in `.git/info/exclude` at workspace init so it does not trip the pre-pass dirty check. Missing/unparseable file → "no audits have ever run" (every audit is eligible on its next due iteration). Lost state safely re-runs all audits on schedule.

**Outcome dispatch:**

- `AuditOutcome::Reported(findings)` → chatops post with header `📋 <repo>: <audit_type> — N finding(s)` and a bullet list of severity-glyphed subjects (low: `•`, medium: `⚠`, high: `🔴`). Default per-finding excerpt is 200 chars; full bodies live in the audit-run log. Empty findings vector is silent unless `notify_on_clean: true`.
- `AuditOutcome::SpecsWritten(names)` → one `🔍 <repo-url>: <audit_type> created proposal \`<change-slug>\` — <first line of ## Why>` chatops post per validated change (see [CHATOPS.md → Proposal-created audit notifications](CHATOPS.md#progress-notifications)). The notification fires AFTER `openspec validate --strict` passes for each proposal AND BEFORE the audit's `git commit` ships it, so operators see provenance for the `🚀 starting work on …` line that follows on the next polling iteration. The framework also logs an info line naming each created change. The notification is always sent (not gated by `notify_on_clean`); brightline + the advisory `Reported`-only audits never fire it.
- `AuditOutcome::NoFindings` → silent.

**Failure modes:**

- An audit returning `Err` is logged at ERROR; the state file is NOT updated for that audit; the iteration continues to the remaining audits and then to the push+PR step. Other audits in the registry still run.
- A WritePolicy violation is treated the same way (state untouched), additionally reverts the workspace and posts the throttled `audit_write_policy_violation` chatops alert.

**Workspace-validity gate.** Every audit (LLM-driven and pure-data) verifies the workspace is valid before doing any work — "valid" means the workspace directory exists AND it contains a `.git/` subdirectory. When the check fails, the audit returns `AuditOutcome::WorkspaceUnavailable` immediately: no file IO, no LLM call, no `fs::create_dir_all`, and no state mutation. The scheduler logs a single INFO line `audit skipped: workspace not in a valid state` naming the audit, the workspace path, and the reason, and does NOT update the audit's cadence-state file (skipped runs do not consume cadence — the next iteration's cadence check re-evaluates and may try again if the workspace has become valid). No chatops notification fires for a skipped audit; the iteration-level `WorkspaceInitFailure` alert is the operator-facing signal of the upstream problem, and per-audit skip notifications would just flood the channel.

The polling iteration also gates the entire audit scheduler on `ensure_initialized` success: if workspace init failed for the iteration, the scheduler is not invoked at all. The per-audit gate catches the rarer case where the workspace becomes invalid mid-iteration; the iteration-level gate catches the common case where the workspace was invalid at iteration start. Both gates together close the upstream gap where an audit's `fs::create_dir_all` could create the workspace's parent directories without a real clone — leaving behind a broken state that future iterations could not recover from.

**Acting on findings: the audit → review → `send it` → spec-PR → merge → implementer loop.** When an audit's findings post to chatops via the threaded path, autocoder stamps an audit-thread state file (`<system-temp>/autocoder/audit-threads/<thread_ts>.json`) keyed by the Slack thread's `thread_ts`. An operator reviewing the findings has three options:

1. **Ignore.** The thread state file expires after 7 days and is pruned automatically.
2. **Triage by hand.** SSH to the workspace, edit code or write a new `openspec/changes/<slug>/` proposal, push and PR like normal.
3. **`@<bot> send it`** posted as a reply inside the audit's thread. The dispatcher validates the thread (tracked, fresh, status `open` or `triage-failed`), submits a `trigger_audit_action` to the daemon, and flips the state to `triage-pending`. The next polling iteration drains the triage queue: the executor runs in triage mode against the findings, explores the codebase, classifies each finding as quick-fix or spec-worthy, and writes a new `openspec/changes/<slug>/` proposal capturing the work as `tasks.md` items. As of a43 autocoder opens **one spec-only PR**: it discards any code-path write the agent made (restoring those paths, logging a WARN, and posting a chatops warning that names what was dropped) so the PR diff is spec-only. If the agent produced only code and no spec, NO PR opens and the state flips to `triage-failed`. Otherwise the state flips to `acted` after the spec PR lands, and the code fixes are written by the standard implementer on the next iteration once the operator merges the spec PR. (The `architecture_brightline` "Mark as intentional" verdict is the sole exception: its `.brightline-ignore` write ships directly in a single PR — see [CHATOPS.md → `send it`](CHATOPS.md#acting-on-audit-findings-send-it).) See [CHATOPS.md → Acting on audit findings](CHATOPS.md#acting-on-audit-findings-send-it) for the full operator-facing surface.

The triage-spawned spec PR is a normal autocoder-opened PR that participates in the existing PR-comment revision loop (see [Revising an open PR via comment](#revising-an-open-pr-via-comment) below). If the agent over-promoted a finding or under-specified the work, `@<bot> revise <text>` on the spec PR gets revisions through the standard channel — the same channel the spec-driven workflow uses for correcting any other autocoder-opened PR. Because the PR diff is spec-only by construction, revisions stay scoped to the proposal.

**Symmetry with `propose`.** The `send it` flow is "act on what the audit found." The companion `@<bot> propose <repo> <free-form text>` verb (see [CHATOPS.md → Chat-driven proposals: `propose`](CHATOPS.md#chat-driven-proposals-propose)) is "act on what I'm asking for." It reuses the same triage-mode plumbing — explore + classify + write-spec — and the same spec-only-PR shape (a43), but accepts the operator's free-form description as the input instead of an audit's findings. The chat-triage prompt adds one classification step ahead of explore: a request that reads as a QUESTION gets a thread reply (no PR), a DIRECTIVE gets the standard spec-only-PR output (with code fixes captured as `tasks.md` items for the implementer), an AMBIGUOUS request escalates via `ask_user`. The resulting spec PR goes through the same revision loop as `send it` PRs.

**`documentation_audit` — prompt structure, the three checks, and acting on findings.** Following the same pattern as `architecture_consultative` and `drift_audit`: `documentation_audit` invokes the wrapped agent CLI with the read-only `Read`/`Glob`/`Grep`/`Bash` sandbox and an embedded prompt (`prompts/documentation-audit.md`, overridable via `audits.settings.documentation_audit.prompt_path`). The audit's driver pre-gathers every canonical spec under `openspec/specs/<cap>/spec.md`, `README.md`, every `docs/*.md` file, AND a YAML block carrying the operator's configured organizational thresholds (`readme_max_lines`, `page_max_lines_without_toc`) — all concatenated into the prompt with `## File: <path>` headers so the LLM has the inputs in context without spending tool calls to read them. The three checks fire in one LLM call: **coverage** (every operator-visible feature in the canonical specs surfaces in user-facing docs — `@<bot>` verbs, config keys, CLI flags, file paths the operator interacts with), **stale references** (every docs reference to a code symbol / CLI verb / config field exists in the current source), and **organization** (READMEs over `readme_max_lines`, docs pages over `page_max_lines_without_toc` without a TOC, user-driving content buried below admin material, etc.). Findings ship via the standard threaded-notification path with a `📚 documentation_audit on <repo>: <N> finding(s)` top-line. Operators act on findings via `@<bot> send it` in the audit's thread; the triage executor produces a **docs-fix PR** (changes to `README.md` and `docs/*.md`), NOT a spec PR — documentation is not OpenSpec material. The docs-fix PR participates in the standard `@<bot> revise <text>` revision loop the same way every other autocoder-opened PR does.

### `.brightline-ignore` — silencing intentional duplications {#brightline-ignore}

The `architecture_brightline` audit's duplicate-signature check is structural: any function/method whose normalized signature appears in N+ files trips it. Some duplications are deliberate (example sites mirroring a production API, generated scaffolding, multi-platform protocol implementations). To silence those without suppressing the audit entirely, drop a `.brightline-ignore` file at the workspace root.

**Location.** `<workspace_root>/.brightline-ignore`. Per-workspace, never global — different repos have different intentional-duplication patterns.

**YAML schema.** All four fields per entry are required; malformed entries WARN and are skipped:

```yaml
ignore:
  - file: examples/site-a/auth.ts            # workspace-relative path (exact match)
    function: handleAuthCallback              # function/method name (exact match)
    signature_match: "async function handleAuthCallback(req"   # substring of the signature line
    reason: "All example sites implement the same auth contract; intentional"
  - file: examples/site-b/auth.ts
    function: handleAuthCallback
    signature_match: "async function handleAuthCallback(req"
    reason: "All example sites implement the same auth contract; intentional"
```

Anchors are `file + function + signature_match` — NEVER line numbers, which shift on every edit and would rot every entry within days.

**Match-suppression rule.** A duplicate-signature finding is suppressed in full when EVERY constituent site matches an ignore entry. A partial match (some sites match, some don't) emits the finding with the unmatched sites only, plus a `(N suppressed by .brightline-ignore)` tail in the subject so operators see at a glance that the finding is partially silenced. No match → the finding emits unchanged.

**Stale-entry handling.** Each brightline run validates every loaded entry against the current workspace state. An entry is stale when (a) the named file doesn't exist, (b) the file no longer contains a function with the named name, OR (c) the function's signature line no longer contains `signature_match`. Stale entries surface as additional findings; the brightline chatops top-line gains a trailing `; <K> stale ignore entries to clean up` clause when `K > 0`, and the threaded body lists each stale entry with its `file + function + reason`. The audit does NOT modify `.brightline-ignore` on disk — brightline declares `WritePolicy::None` and the cleanup is purely informational. The operator removes stale entries manually (or via `@<bot> send it` on a future audit run, since the triage handler permits `.brightline-ignore` writes).

**`send it` integration.** When an operator runs `@<bot> send it` on a brightline-finding thread, the triage LLM classifies each duplicate-signature finding as one of:

- **Fix** — refactor the duplication out via a quick fix.
- **Spec-worthy** — write a proposal under `openspec/changes/<slug>/`.
- **Mark as intentional** — add an entry to `.brightline-ignore` for each constituent site of the finding (the LLM populates `reason` from its judgment; operators revise via the standard PR-comment revision loop if the reason is off).

A `Mark as intentional` triage produces a diff that touches ONLY `.brightline-ignore`. The triage handler enforces this scope: a brightline triage diff that mixes `.brightline-ignore` writes with arbitrary code edits is rejected (the offending paths are named in the rejection chatops reply and the thread's status flips to `triage-failed` so the operator can retry).

### On-demand audit triggers

Cadence-based scheduling fires audits on `daily`/`weekly`/`monthly` intervals, which suits steady-state operation but not the production-readiness workflow ("run an architecture audit now, fix what it surfaces, run a security audit now, iterate"). On-demand triggers complement the cadence: a `@<bot> audit <substring> <repo>` chatops verb (see [CHATOPS.md → On-demand audit: `audit`](CHATOPS.md#on-demand-audit-audit)) and an `autocoder audit run --workspace <path> --audit <name>` CLI subcommand (see [CLI Reference → audit run](CLI.md#audit-run)) both append an audit-type to a per-repo `pending_audit_runs` queue. At the start of each polling iteration's audit phase, the scheduler drains the queue and runs each queued audit unconditionally — cadence and `requires_head_change` are bypassed for queued runs. After the queued runs, the cadence-driven sweep proceeds normally, skipping any audit that already ran via the queue this iteration so the same audit cannot run twice in one pass.

**Cadence interaction rule.** A queued audit's `last_run_at` state is updated on success, so the next cadence-scheduled fire shifts forward by the cadence interval from the on-demand timestamp. Concretely: if `security_bug_audit` is configured `monthly` and an operator triggers it on-demand today, the next cadence-driven fire is one month from today (not one month from the original schedule). The trade-off favors not double-running audits soon after an on-demand fire; operators who want to bypass cadence entirely can keep triggering on-demand.

**De-duplication.** If the same audit-type appears in `pending_audit_runs` more than once before a single iteration fires (operator typo, double-click on a chatops command), the duplicate entries collapse to one run. The audit fires once per iteration, not once per queue entry.

**Audits configured with `cadence: disabled` can still be triggered on-demand.** The on-demand path is independent of the cadence machinery; an operator who configured an audit `disabled` can still run it ad-hoc via chatops or CLI without changing the YAML. The audit's `last_run_at` is still updated, but with no cadence interval the "next scheduled fire" remains in the past — the audit stays effectively disabled for cadence-driven scheduling.

**ETA in the bot ack.** The chatops verb's reply names the resolved audit-type, the repo URL, and an ETA derived from the repo's `poll_interval_sec` (`~Nm` rounded to minutes). When the daemon reports `seconds_until_next_iteration < 30`, the ETA reads `imminently` instead.

**Standalone CLI mode.** When no daemon is running, `autocoder audit run` invokes the audit module directly against the named workspace and prints findings to stdout. This bypasses the daemon's scheduler entirely (no `pending_audit_runs` queue is involved) and is intended for prompt-template iteration during audit-prompt development — edit `prompts/<audit>.md`, run the CLI, observe, iterate.

## Revising an open PR via comment

autocoder treats a PR comment of the form `@<bot> revise <free-text>` as a
revision request against the agent branch the PR was opened from. On the
next polling iteration, the daemon:

1. Fetches the comment, parses the revision text (everything after `revise`).
2. Re-invokes the executor in revision mode with the original change
   material, the current PR diff, and the operator's text.
3. On `Completed`: commits the workspace, force-pushes (`--force-with-lease`)
   to the agent branch, and posts a reply comment starting with
   `✅ Revision applied:`. The PR's diff updates in place; no PR close /
   re-open is required.
4. On `Failed`: posts `✗ Revision attempt failed: <reason>`. The PR is
   unchanged; the operator can reply with another `@<bot> revise ...` to
   retry or close the PR.
5. On `AskUser` (executor needs clarification): no commit, no reply.
   The question is escalated via the existing ChatOps channel; once the
   operator answers in that thread, the next polling iteration resumes
   the revision against the same trigger comment.

The trigger pattern is strict: the comment body's first non-whitespace
token must be `@<bot>` (case-insensitive on the username) and the next
token must be `revise` (case-insensitive). Comments like `@<bot> looks
good` are conversational and are ignored. Anyone with GitHub write access
to the repo can post a revision — the trust boundary matches the existing
ChatOps channel.

**Revision cap (automatic only).** Each PR has a per-PR cap on
**automatic** revisions (default `5`; configurable via
`executor.max_auto_revisions_per_pr`, hard-clamped at `20`; the legacy
key `executor.max_revisions_per_pr` is still accepted as an alias). The
cap counts ONLY reviewer-marked automatic revisions (the
`<!-- reviewer-revision -->` comments) — **human `@<bot> revise` requests
are never capped and always process.** When an automatic revision would
exceed the cap, the daemon posts a one-time decline comment starting with
`🛑 Revision cap reached` AND a ChatOps notification, then silently
ignores subsequent automatic triggers on that PR (human triggers continue
to process). Close + re-open or merge as-is to reset the cap.

**State persistence.** Per-PR state (last-seen-timestamp,
automatic-revision count, cap-decline flag) lives at
`<workspace>/.autocoder/revisions/<pr-number>.json`. Files for
closed/merged PRs are pruned automatically at iteration start.

**Disabling.** Set `executor.max_auto_revisions_per_pr: 0` to opt out of
the PR-comment revision channel entirely.

### Reviewer-initiated revisions (cross-reference)

The same revision dispatcher described above also processes
`<!-- reviewer-revision -->`-marked comments posted by the code-quality
reviewer when `reviewer.auto_revise: true` (fired on actionable concerns
regardless of verdict). These reviewer-marked comments are the
**automatic** revisions bounded by the per-PR
`executor.max_auto_revisions_per_pr` cap; they share the same per-PR state
file (`<workspace>/.autocoder/revisions/<pr-number>.json`). A human
`@<bot> revise ...` request does NOT consume this automatic budget — it
always processes regardless of how many automatic revisions have run.

See [Reviewer-initiated revisions on actionable concerns](CODE-REVIEW.md#reviewer-initiated-revisions-on-actionable-concerns)
for the full reviewer-side flow, the per-concern decision the reviewer
makes, and the operator-template migration steps for sites that have
overridden the default reviewer prompt.

## Perma-stuck change detection

When an agent fails the same change two iterations in a row, autocoder marks it perma-stuck: writes a `.perma-stuck.json` marker inside the change directory, posts a chatops alert, and excludes the change from `list_pending` on every subsequent pass until the marker is removed manually. The threshold is `executor.perma_stuck_after_failures` (default `2`, minimum `1`).

What counts as a failure:

- The executor returns `Failed`.
- The executor returns `Completed` but did not modify the workspace (no-op completion).
- The executor returns `Completed` but only renamed the change directory into `archive/` (lazy archive).

What does NOT count (transient infrastructure problems):

- Workspace init / clone / fetch failure.
- `openspec` preflight failure.
- GitHub API transport errors.
- A busy-marker stuck-state that skipped the iteration entirely.

Per-repo counter state lives at `<workspace>/.failure-state.json` (registered in `.git/info/exclude` at workspace init so it never trips the pre-pass dirty check). Successfully archiving a change clears its counter entry; the next failure starts fresh from `1`.

The marker file at `<workspace>/openspec/changes/<change>/.perma-stuck.json` has the schema:

```json
{
  "change": "<change-name>",
  "consecutive_failures": 2,
  "last_reason": "...",
  "marked_stuck_at": "RFC 3339 UTC timestamp",
  "operator_action": "Delete this file to retry the change."
}
```

The chatops alert names the repo, change, count, and a truncated `last_reason`, plus the marker file path. It is subject to the same 24-hour throttle as the predictable-failure alerts: repeat fix-test-fail cycles do not spam the channel. When no chatops backend is configured, the marker is still written and the change is still excluded — an ERROR log is the operator's only signal.

To clear the marker: delete the file. The change re-enters `list_pending` on the next poll. If the underlying problem is not fixed, the change will fail twice more and be marked perma-stuck again (with the 24-hour alert throttle suppressing duplicate notifications inside the window).

**Queue-blocking behavior (a18).** A `.perma-stuck.json` marker does more than exclude the affected change: it ALSO halts the queue walk for subsequent pending changes in the same repository. This is the same blocking semantic that already applies to `.needs-spec-revision.json` and AskUser (`.question.json`) markers — the four marker categories that gate the queue are enumerated under [Queue-blocking policy](#queue-blocking-policy). The reason: stacked changes (the common autocoder pattern) frequently reference symbols a prior change introduces, so blasting through the queue after a perma-stuck would burn tokens against changes that cannot land.

**Escape hatch: `ignore-and-continue`.** When the operator knows a particular perma-stuck (or needs-spec-revision) change is independent of its siblings — they happen to be on the same queue but don't depend on each other — they can run `@<bot> ignore-and-continue <repo> <change>` to stamp `.ignore-for-queue.json` alongside the underlying marker. The change stays excluded from `list_pending` (it's still broken), but siblings resume processing. Reverse with `@<bot> clear-ignore <repo> <change>`. Resolving the original problem with `@<bot> clear-perma-stuck <repo> <change>` removes BOTH files automatically. See [CHATOPS.md → operator recovery commands](CHATOPS.md#operator-recovery-commands) for verb syntax and example replies.

See also [Spec marked as needing revision](#spec-marked-as-needing-revision) — its sibling pattern for the case where the operator (not the agent) is the one with work to do.

## Spec marked as needing revision

Sibling pattern to [Perma-stuck change detection](OPERATIONS.md#perma-stuck-change-detection). Where perma-stuck signals "the agent kept failing on this change," needs-spec-revision signals "the spec is asking the agent to do something it cannot do." Both are operator-action states; both are cleared by deleting the marker file.

**What triggers it.** Three independent code paths can write this marker:

1. **Agent-detected unimplementable tasks.** Before doing any work, the agent scans `tasks.md` for tasks that require capabilities outside its sandbox: `sudo` on a real host, missing CLI tools, real GitHub tag pushes, browser interactions, VM/container spin-up, smoke tests on specific hardware or OS versions, manual external observation. If any task matches, the agent emits an `=== AUTOCODER-OUTCOME ===` block flagging the unimplementable tasks and exits without modifying the workspace. autocoder writes `<workspace>/openspec/changes/<change>/.needs-spec-revision.json` with `unimplementable_tasks` populated and halts the queue walk.

2. **Pre-flight spec-delta archivability check (a17).** Before invoking the executor, autocoder parses each `specs/<capability>/spec.md` in the change and verifies every `## ADDED Requirements` / `## MODIFIED Requirements` / `## REMOVED Requirements` / `## RENAMED Requirements` block's `### Requirement:` headers against the canonical `openspec/specs/<capability>/spec.md`. The four delta kinds enforce: ADDED title must NOT exist in canonical (catching duplicate-add); MODIFIED title MUST exist (catching the a07 class of bug where an invented title was used); REMOVED title MUST exist; RENAMED `from:` title MUST exist, `to:` title MUST NOT exist. On any precondition violation, autocoder writes the marker with `unarchivable_deltas` populated, posts the chatops alert, and halts the queue — the executor is never invoked. **Principal cost savings:** no LLM call against a change whose deltas would abort `openspec archive` later anyway. The marker's `revision_suggestion` is auto-generated and names exactly which deltas need to be fixed.

3. **Pre-flight change-internal contradiction check (a19; opt-in).** Where `a17` catches structural defects, the contradiction check catches semantic ones: a change whose requirements are individually well-formed AND archivable but contradict each other (e.g. ADDED A "all secrets in env vars" + ADDED B "API key in `config.yaml`"). The check runs a configurable LLM against the change's concatenated spec-delta files (small input → small cost, ~$0.01 per check at current pricing). Non-empty findings write the marker with `revision_suggestion` populated from the contradictions narrative — `unarchivable_deltas` AND `unimplementable_tasks` are left empty for this case because the issue is semantic, not mechanical. The check is **disabled by default**; operators trading the small per-change LLM cost for the catch enable it via `executor.change_internal_contradiction_check: enabled` AND `executor.change_internal_contradiction_check_llm`. Failures (network, parse, malformed response) fail OPEN — log a WARN AND proceed to the executor; the daemon does NOT gate work on a failed check. See [Pre-flight checks](#pre-flight-checks) for the full layered design.

All three code paths share the same `AlertCategory::SpecNeedsRevision` throttle (24-hour, same as perma-stuck) and the same operator-clears-the-marker recovery shape. The marker schema accommodates any (or several) populations: `unimplementable_tasks` for the agent-detected path, `unarchivable_deltas` for the a17 pre-flight path, AND a free-form `revision_suggestion` carrying the contradictions narrative for the a19 path.

The agent does NOT auto-edit `tasks.md`. The flag-and-stop contract preserves the project invariant that no AI process edits its own marching orders without human review.

**The marker file** at `<workspace>/openspec/changes/<change>/.needs-spec-revision.json` has the schema:

```json
{
  "change": "<change-name>",
  "marked_at": "RFC 3339 UTC timestamp",
  "unimplementable_tasks": [
    {"task_id": "5.2", "task_text": "...", "reason": "..."}
  ],
  "unarchivable_deltas": [
    {"capability": "code-reviewer", "kind": "Modified", "header": "Reviewer prompt budget is operator-configurable", "reason": "header not found in canonical openspec/specs/code-reviewer/spec.md (this is the a07-style bug; check spelling AND capitalization)"}
  ],
  "revision_suggestion": "free-form text describing what to change (auto-generated for the pre-flight path)",
  "operator_action": "Edit openspec/changes/<change>/(tasks.md OR specs/<capability>/spec.md), commit + push, then clear this marker (via @<bot> clear-revision <repo> <change> or by deleting the file directly)."
}
```

`unimplementable_tasks` and `unarchivable_deltas` are both optional (each elided from the JSON when empty). Pre-spec markers with only `unimplementable_tasks` continue to deserialize unchanged.

The marker is registered in `.git/info/exclude` at workspace init so it does not trip the pre-pass dirty check and survives `git clean -fd` during per-iteration recovery (same treatment as `.perma-stuck.json`).

**The chatops alert** lists each flagged task's id + text, the agent's revision suggestion, an operator-action checklist, and the marker file path + the per-change run log path. It is gated on `failure_alerts_enabled` and subject to the standard 24-hour per-category throttle.

**Operator workflow.**

1. Read the chatops alert. The flagged tasks and the agent's revision suggestion are in the body; the run log is named for deeper diagnosis if needed.
2. Edit `openspec/changes/<change>/tasks.md` to remove or revise the flagged tasks. Commit + push to the base branch.
3. Delete the marker file: `rm openspec/changes/<change>/.needs-spec-revision.json`. The next iteration picks the change back up.

**False-positive escape hatch.** If you review the flagged tasks and decide the agent was overly conservative, delete the marker WITHOUT editing `tasks.md`. The change re-enters `list_pending` on the next iteration. If the agent flags the same task again, you can add a comment in `tasks.md` near it explaining why it's implementable (e.g. naming a tool path or workflow that resolves the concern), or update the implementer prompt template via a follow-up change to relax the relevant pattern.

The marker is operator-cleared, not auto-cleared. autocoder does not remove it on the next iteration even when the spec has been revised — same rationale as the perma-stuck marker: the operator's audit trail is clearer when "did the issue actually get fixed?" requires an explicit human action.

## Queue-blocking policy

autocoder treats the following per-change marker categories as queue-blocking — when any change in `openspec/changes/<slug>/` has one of these markers AND does NOT also have `.ignore-for-queue.json`, the polling loop halts the queue walk for the iteration:

1. **`.question.json` (AskUser waiting).** The agent has posted a question to the operator and is awaiting a reply. Resumed via `@<bot> send it` or by replying in the chatops thread; cleared when the resume completes.
2. **`.needs-spec-revision.json`.** Either the agent flagged unimplementable tasks OR the a17 pre-flight check rejected an unarchivable spec delta. Cleared via `@<bot> clear-revision <repo> <change>` (or by deleting the file directly).
3. **`.perma-stuck.json`.** The change has hit `executor.perma_stuck_after_failures` consecutive failures. Cleared via `@<bot> clear-perma-stuck <repo> <change>` (which also removes any accompanying `.ignore-for-queue.json`).
4. **Future extension markers.** Any new operator-action category future specs add SHOULD be added to this list AND honor the `.ignore-for-queue.json` downgrade contract.

**Downgrade marker.** `.ignore-for-queue.json` accompanies any of the above and downgrades the change's blocking effect from "halt subsequent pending changes" to "still excluded from `list_pending` but siblings proceed." It is the operator's explicit "I know this one's broken; skip it AND keep going with the rest" signal — see [Perma-stuck change detection](#perma-stuck-change-detection) for the operator workflow and [CHATOPS.md](CHATOPS.md#operator-recovery-commands) for the verb syntax.

## Recovering from a bad run

The `rewind` subcommand discards the in-flight agent branch and re-queues one or more archived changes. See [CLI Reference → rewind](CLI.md#rewind) below.

## Runtime control: live config reload

A running daemon exposes a Unix-domain control socket at `<system-temp>/autocoder/control/control.sock` (typically `/tmp/autocoder/control/control.sock` on Linux). The file is created on startup with mode `0600` and owned by the user running the daemon — only that user can connect. The socket file is removed at shutdown.

The `autocoder reload` subcommand connects to the socket, sends `{"action":"reload"}`, and prints the daemon's response. The daemon re-reads the YAML config from the same path it was launched with, validates it (parse + workspace-collision + token-route checks), and either rejects the request or hot-applies the safe subset of changes.

What gets hot-applied:

- `github` — per-owner tokens, default `token_env`, `fork_owner`. Applied at the next iteration boundary for each repository.
- `reviewer` — provider, model, API key, prompt template. In-flight reviews finish with the previous reviewer; subsequent reviews use the new one.
- `chatops` — backend selection, default channel, notification flags. In-flight notifications finish with the previous backend; subsequent ones use the new one.
- `repositories` — adding, removing, or modifying repositories in the list. New entries are spawned as fresh polling tasks (workspace setup, dirty-check, busy-marker — same as daemon startup). Removed entries get their per-repo cancellation token fired; the running task finishes its in-flight iteration normally (including push + PR) and exits at the next inter-poll sleep boundary. Modified entries hot-swap an `Arc<ArcSwap<RepositoryConfig>>` holder so the next iteration of that task reads the new `base_branch`, `agent_branch`, `poll_interval_sec`, `chatops_channel_id`, `local_path`, or `max_changes_per_pr`. The reload handler diffs the new list against the current task set by `url` — that field is the identity key. Changing the `url` of an existing entry is treated as `remove old_url + add new_url`. Reordering the list has no effect.

What requires a full restart:

- `executor` — only one executor instance exists, shared across tasks. Changes to `executor:` fields are reported under `requires_restart`.

Response shape on success:

```json
{
  "ok": true,
  "applied": ["github", "reviewer", "repositories"],
  "requires_restart": ["executor"],
  "unchanged": ["chatops"],
  "repositories_delta": {
    "added": ["git@github.com:owner/repo-c.git"],
    "removed": ["git@github.com:owner/repo-a.git"],
    "changed": ["git@github.com:owner/repo-b.git"]
  }
}
```

`repositories_delta` is always present (the three arrays can each be empty) so client tooling has a consistent shape to parse. An entry only appears under one of `added` / `removed` / `changed` per reload.

Validation rejection is non-disruptive: if the new YAML fails to parse or fails semantic validation, the daemon continues running with the previous in-memory config. The response is `{"ok": false, "error": "<message>"}` naming the failure, and the CLI exits non-zero. If the daemon is not running (or is running under a different user), the CLI prints an error naming the expected socket path and hinting at the cause.

### Adding a repository at runtime

To add a repository without restarting the daemon:

1. Edit `config.yaml` (the path the daemon was launched with) and append the new entry under `repositories:`. Set its `url`, `base_branch`, `agent_branch`, and `poll_interval_sec` as usual.
2. Run `sudo -u autocoder autocoder reload` from the same host. The CLI prints the daemon's response.
3. Verify the response includes the new URL under `repositories_delta.added` and `"repositories"` appears in `applied`. The polling task is now running; it does workspace initialization on its first pass.

The reverse (remove a repository) works the same way: delete the entry, reload, and the new URL appears under `repositories_delta.removed`. The cancelled task finishes its current iteration before exiting, so a removal during an active push or PR step completes cleanly.

### In-flight iteration safety

A repo cancelled mid-iteration finishes its in-flight pass normally. The cancellation check sits in the inter-poll `tokio::select!`, so the next poll never starts after the cancel — but the current one runs to completion. A modify-in-place is observed at the *next* iteration; the current iteration uses the old snapshot. Both rules eliminate mid-iteration tearing of `RepositoryConfig` fields.

If you remove a repo and re-add it (or change a setting) before the previous task has fully exited (e.g. it is mid-push when the reload lands), the response logs a WARN and reports the URL as unchanged for that reload. Run `autocoder reload` again after a brief wait; the second reload sees the URL as absent and re-adds it cleanly.

## Rebuilding canonical specs from archive history

`openspec/specs/<capability>/spec.md` is rebuilt by the host's openspec install whenever an archived change has the `openspec sync` workflow enabled at archive time. When a repository was archived from a host without that workflow (or before that workflow existed), the canonical specs drift from what the archive history actually says. Symptoms: the archive contains 30 `## ADDED Requirements` blocks, but the canonical spec is missing 25 of them.

autocoder ships a full rebuild path for that case. Incremental backfill is intentionally unsupported — when drift is mid-history (an earlier change was never synced but later changes were), re-applying the skipped change onto the current canonical produces an incorrect end state. Full rebuild from scratch is the only safe answer.

**When to use rebuild.** When you onboard a repo that was archive-driven from a host without `openspec sync`, when `git diff openspec/specs/` after a successful archive shows nothing despite the change adding requirements, or when `openspec list` and the on-disk canonical specs disagree on capability content.

**CLI invocation** (against a local clone — no daemon required):

```bash
autocoder sync-specs --rebuild --workspace /path/to/repo
```

This iterates every archived change in chronological order, replays it via `openspec archive`, and preserves each archive's original date prefix via in-place rename. The CLI prints a summary listing successful and failed changes plus a modified-vs-unchanged tally for every canonical spec file. Exit code is non-zero if any archive failed to re-archive.

**Chatops invocation** (for daemon-managed repos):

```
@<bot> rebuild-specs <repo-substring>
```

This submits a `RebuildSpecs` action to the control socket, which sets a `pending_rebuild` flag on the named repo's polling task. The next iteration runs the rebuild instead of the normal queue walk. The rebuild's commits land on the agent branch via the existing push + PR flow; the PR title is `spec rebuild: <N> capability(ies) rebuilt from archive history` so operators can recognize it at a glance.

When the rebuild iteration finishes, the bot posts one of three chatops messages:

- `✓ rebuild complete for <repo>: PR <url> opened — <N> capability(ies) updated from <M> archived change(s)` (success with drift)
- `✓ rebuild complete for <repo>: no drift detected, canonical specs already in sync` (success no drift)
- `⚠️ rebuild for <repo> completed with <N> failure(s); ...` (partial failure)

The completion notification fires regardless of `chatops.notifications.pr_opened` or `failure_alerts` — it is the operator's direct response to a command they issued, so they always get the completion signal.

**The `--immediate` flag** (CLI only — never exposed via chatops):

```bash
autocoder sync-specs --rebuild --immediate --workspace /path/to/repo
```

Without `--immediate`, the CLI waits politely for the current iteration to release the busy marker before starting. With `--immediate`, the CLI sends `SIGTERM` to the executor subprocess (via the busy marker's recorded PID), waits up to 30 seconds for cleanup, and runs the rebuild even if the iteration was mid-flight. The cancelled iteration's partial workspace state is cleaned up by the rebuild's first dirty-workspace recovery pass.

Chatops deliberately does NOT support `--immediate`: killing a running executor mid-iteration is a foot-loaded gun that should require SSH access. Operators wanting `--immediate` SSH to the daemon host and run the CLI.

**What rebuild discards** — a caveat. The rebuild is "what would canonical look like if every archive had synced correctly the first time." It does NOT preserve:

- `## Purpose` paragraphs hand-edited into canonical specs without an archived change introducing them. New capability spec files openspec creates from scratch get a placeholder Purpose (`TBD - created by archiving change <X>. Update Purpose after archive.`); operators replace those manually after the rebuild PR merges.
- `### Requirement:` entries hand-added to canonical without an archive source. Anything not in the archive history is gone after rebuild.

Review the rebuild PR's diff before merging; treat it like any other autocoder PR.

## Workspace path derivation

If a repository entry omits `local_path`, the workspace path is derived deterministically from the URL:

1. Strip the protocol prefix (`git@`, `ssh://`, `https://`, `http://`).
2. Strip a trailing `.git`.
3. Replace any character that is not ASCII alphanumeric, `_`, or `-` with `_`.
4. Prepend `/tmp/workspaces/`.

`git@github.com:owner/repo.git` and `https://github.com/owner/repo.git` both map to `/tmp/workspaces/github_com_owner_repo`. At startup, autocoder runs a collision check: if two configured repositories resolve to the same workspace path (whether by derivation or by explicit `local_path`), the process exits non-zero before spawning any polling tasks. Set `local_path` explicitly to disambiguate.

## Multi-repo setup

`repositories:` accepts any number of entries. autocoder spawns one polling task per entry, each on its own `poll_interval_sec`. Per-repo state is fully independent: an iteration failure on repo A does not affect repo B; a ChatOps escalation on repo A blocks A's pending queue but does not touch B.

```yaml
repositories:
  - url: "git@github.com:my-org/auth-service.git"
    base_branch: main
    agent_branch: agent-q
    poll_interval_sec: 300

  - url: "git@github.com:my-org/web-dashboard.git"
    base_branch: dev
    agent_branch: agent-q
    poll_interval_sec: 3600
```

## Polling cadence and your firewall

When autocoder spawns ≥5 polling tasks at process start, the simultaneous `git fetch` operations from a single source IP can look like a port scan or scraper to network IDS — one operator reported their IDS killing SSH connections the moment the daemon tried to poll 8–9 repos at once. Even without an IDS, tasks that all share the same `poll_interval_sec` (e.g. the default `300`) drift only marginally across iterations because `git fetch` dominates each iteration's wall-clock, so they tend to re-cluster over time.

Two defaults defuse this:

- `executor.startup_jitter_max_secs` (default `30`) — each task waits a uniformly-random `[0, 30]` seconds before its first iteration, smearing the first round of fetches across a 30 s window.
- `executor.inter_iteration_jitter_pct` (default `10`) — each inter-iteration sleep is `poll_interval_sec ± 10%`, so tasks that briefly synchronize drift apart again on the next cycle.

Both jitters cost almost nothing in wall-clock and respect SIGTERM/SIGINT (cancellation is observed within 200 ms during either sleep). Operators on isolated networks who prefer deterministic timing can set both to `0`. Operators who want a wider window — say, after seeing IDS alerts even with the defaults — can raise `startup_jitter_max_secs` to something like `120` or `300`.

## Queue order

Pending changes are processed in ascending entry-name order (UTF-8 byte order, which is alphabetical for ASCII names). Operators with stacked dependencies — i.e. change N+1 depends on change N — encode order explicitly by prefixing change names with a letter+number tag: `a01-rename-foo`, `a02-extract-bar`, `a03-wire-baz`. The prefix is the operator's contract for "this change depends on the previous in sequence." For a second unrelated stack, use a different letter group (`b01-`, `b02-`). For unrelated single changes, no prefix is needed; alphabetical order is arbitrary but deterministic.

Note: OpenSpec rejects change names that start with a digit. Plain `01-`/`02-` prefixes will fail at the prompt-building step (`openspec instructions apply --change <name>` returns "Invalid change name"). Always start with a letter.

Each iteration commits at most `max_changes_per_pr` archived changes (default `3`); any remaining pending changes wait for the next iteration. The cap is configurable per repository, or globally via `executor.max_changes_per_pr`. A long queue therefore ships as several reviewable PRs over time rather than one large PR.

A change that fails (or escalates to chatops) halts the queue walk for that iteration; remaining pending changes wait for the next iteration. This preserves the stacked-dependency assumption behind authoring-order processing: change N+1 may depend on change N having succeeded, so the bot does not attempt N+1 while N is unfixed. A persistently-failing change accumulates failure-counter increments and hits perma-stuck (default after 2 consecutive failures), at which point it drops out of `list_pending` and the queue resumes at N+1.

## Startup preflight

At startup, `autocoder run` invokes `openspec --version` once. If the binary is not on the daemon's PATH or exits non-zero, the daemon exits non-zero before any polling task is spawned. The stderr message names the failure (binary not found, non-zero exit code, etc.). This means a misconfigured deployment surfaces at startup rather than producing empty iterations.

If you see `openspec preflight failed: binary not found on PATH`, add the install directory to the systemd unit's `Environment="PATH=..."` line (see [Deployment](DEPLOYMENT.md)).

## Fork recreation on workspace reinitialization

The default workspace-deleted recovery (see [Workspace directory deleted](#workspace-directory-deleted)) preserves whatever state lives on the fork. That is the right behavior when you have open PRs from that fork — losing their head refs would close the PRs. But the same preservation is a liability when the fork has accumulated stale branches no one cares about, or when the fork's state is genuinely worthless and you'd rather start from a pristine mirror of upstream.

Set `github.recreate_fork_on_reinit: true` to opt in to the destructive recovery path. When that flag is enabled AND fork-PR mode is active AND the workspace directory is absent at iteration start, autocoder:

1. Calls `DELETE /repos/<fork_owner>/<repo>` against the GitHub API to delete the fork.
2. Waits 2 seconds for the deletion to propagate.
3. Calls `POST /repos/<upstream_owner>/<repo>/forks` to re-fork from upstream.
4. Polls the new fork's URL via `git ls-remote` for up to 30 seconds until reachable.
5. Proceeds with the normal clone + fork-remote registration.

After a successful re-fork, autocoder posts a one-line chatops notification:

> :warning: \`<repo>\`: re-forked at workspace reinitialization (previous fork deleted; any open PRs from this fork are now closed)

The notification is gated by the same `chatops.notifications.failure_alerts` toggle as the other operator-visible failure alerts.

Requirements:

- The operator's PAT must include the `delete_repo` scope. Without it, the DELETE returns 403, autocoder logs an ERROR naming the missing scope, and falls back to the conservative non-recreating init path (clone + fetch fork). The iteration still makes progress; the fork is unchanged.
- The flag is global on the `github:` block, not per-repository — all configured repos in a single autocoder process share the same fork owner, and the fork-recreation policy is uniform across them.

Defaults to `false`. With the default, the workspace-deleted recovery preserves fork state (see [Workspace directory deleted](#workspace-directory-deleted)).

## Workspace directory deleted

If a workspace directory under `/tmp/workspaces/` is removed while autocoder is running (or while stopped), the daemon's next iteration treats this as a fresh-clone case: it clones upstream into the path again. In fork-PR mode it also fetches ONLY the configured agent branch from the `fork` remote at that time (via `git fetch fork +refs/heads/<agent_branch>:refs/remotes/fork/<agent_branch>`) so the local `refs/remotes/fork/<agent_branch>` tracking ref reflects the fork's actual state. Without that fetch the next `git push --force-with-lease fork <agent_branch>` would compare an empty local tracking value against the fork's existing commits and reject with `! [rejected] <agent_branch> -> <agent_branch> (stale info)`, leaving the daemon stuck. The fetch deliberately restricts itself to one branch: a wholesale `git fetch fork` would populate `refs/remotes/fork/<every-branch>`, and if any fork branch shadows an upstream name (e.g. both `origin/dev` and `fork/dev` exist), the next `git checkout <base_branch>` would fail with `fatal: 'dev' matched multiple (2) remote tracking branches`. The post-clone fork fetch is best-effort: if it fails (network blip, fork doesn't yet exist, agent branch doesn't yet exist on the fork), the daemon proceeds and the next push will surface any real divergence via the existing branch-push-failure alert.

## Partial-clone self-heal

When a `git clone` is interrupted mid-flight (network drop, signal, transient auth blip), git leaves the destination directory created but without a `.git/` subdirectory. Previously this state hard-stuck the daemon — every subsequent iteration logged `workspace path exists but is not a git repository (no .git directory): <path>` and never attempted recovery; the only way out was an operator-side `rm -rf`.

The daemon now auto-recovers. When `workspace::ensure_initialized` detects that the workspace path exists AND has no `.git/`, it runs a safety check, deletes the partial directory, and re-attempts the clone as if the workspace had never existed. If the re-clone succeeds, the iteration proceeds normally; if it fails, the returned error carries the real clone failure (auth, network, etc.) — operators see the actual cause in journalctl and in any chatops `WorkspaceInitFailure` alert, rather than the misleading secondary detection.

**WARN log line.** Each auto-cleanup emits exactly one WARN naming the workspace path, the repo URL, and the action:

```
WARN workspace=/path/to/ws repo=<url> workspace exists without .git; partial clone artifact detected. Deleting and re-cloning.
```

**Safety-check tripwires.** Before deleting, the daemon refuses auto-cleanup if the partial directory contains any of:

- `.in-progress*` lock files at any depth (would suggest an active iteration somehow racing this path).
- `openspec/changes/<slug>/.perma-stuck.json` or `openspec/changes/<slug>/.needs-spec-revision.json` at any depth (operator-managed markers that survived a previous successful clone).
- `openspec/changes/<slug>/.question.json` or `openspec/changes/<slug>/.answer.json` (AskUser markers).

When a tripwire fires, the daemon returns the original "exists but no `.git`" error extended with `(partial cleanup refused: <tripwire>; manual operator inspection required)` and the directory is NOT deleted. Operators inspect the directory and decide manually. See [TROUBLESHOOTING.md](TROUBLESHOOTING.md) for the manual recovery flow.

**Not a tripwire:** a stray `.alert-state.json` at the workspace root. As of `a16-consolidate-workspace-bookkeeping-to-state-dir`, alert-throttle state lives at `<state_dir>/alert-state/<workspace-basename>.json` and the workspace SHOULD NOT contain the file at all. If a transient copy appears (e.g. fresh re-clone of a repo whose history transiently committed it before the migration completed), the workspace-init invariant check removes it; destroying it manually is also harmless.

**Re-clone failure classification.** When the re-clone itself fails (the actual transport call after the partial-cleanup decision), the surfaced error feeds into the same mid-iteration classifier described under [Dirty workspace auto-recovery](#dirty-workspace-auto-recovery): transient (network blip, GitHub `5xx`, auth token blip) retries on the next polling tick with a throttled alert, while permanent (config error, missing binary) skips the iteration and fires the operator-inspection alert. See [CHATOPS.md → Throttled failure alerts](CHATOPS.md#throttled-failure-alerts-) for the alert text variants.

## Dirty workspace auto-recovery

If a workspace under `/tmp/workspaces/` is left dirty between polls (uncommitted edits, untracked files, or a checked-out branch other than the base), autocoder recovers automatically at the next startup or poll cycle: it checks out the configured `base_branch`, runs `git reset --hard origin/<base_branch>`, and runs `git clean -fd`. The repo then re-enters its normal polling loop. If recovery itself fails (e.g. the remote is unreachable), the repo is skipped for the daemon's lifetime and an error is logged — restart the daemon once the underlying problem is fixed.

Recovery runs at two points in the lifecycle:

1. **Startup** (`autocoder run` boot): every configured repo passes through `repo_passes_startup_check`. A dirty workspace at this point usually means a daemon restart after a previous run was killed mid-iteration. Recovery resets the workspace and the repo proceeds to normal polling; if recovery itself fails the repo is excluded for the process lifetime.
2. **Per iteration** (`run_pass_through_commits` pre-pass check): a failed executor invocation that returned `Failed` or timed out without committing leaves tracked-file modifications behind. The next iteration's pre-pass dirty check runs the same recovery before the iteration's normal flow begins. On success the iteration proceeds and no operator notification fires. Only when recovery itself errors (or the workspace is somehow still dirty after the recovery commands complete) does autocoder post the `WorkspaceDirtyMidIteration` chatops alert and return the iteration as failed.

Wholesale wiping of the workspace is safe at both points because the agent branch is rebuilt from base each iteration via `recreate_branch` — any local state the recovery destroys would have been overwritten anyway. The recovery does NOT touch the fork remote; it operates purely on the local working tree.

**Mid-iteration recovery failures are classified transient vs. permanent.** Starting with `a14`, a recovery operation that fails during a poll (workspace re-init, `git fetch`, dirty cleanup) runs the returned `anyhow::Error` through `classify_recovery_failure`:

- **Transient** — DNS resolution failures, `Connection timed out / refused / reset`, TLS handshake failures, "the remote end hung up", GitHub HTTP `5xx` (502, 503, 504, 522, 524), HTTP 401 / 403 (auth blip — recoverable by rotating the env-var-backed token and calling `autocoder reload` without restarting), HTTP 429 (rate limit), and `std::io::ErrorKind` values matching transport hiccups (`TimedOut`, `ConnectionReset`, `ConnectionAborted`, `BrokenPipe`, `WouldBlock`). The iteration logs a WARN line tagged `class=transient`, fires the existing 24h-throttled chatops alert (see [CHATOPS.md → Throttled failure alerts](CHATOPS.md#throttled-failure-alerts-) for the suffix variants), and returns from the iteration. The next polling tick attempts the recovery again — no special backoff state is kept.
- **Permanent** — configuration errors (missing required field, malformed YAML, no matching token route), missing required binaries (`openspec`, `git`, `claude` not on PATH), and the "remains dirty after recovery" branch (recovery commands all succeed but `git status --porcelain` is still non-empty). The iteration logs an ERROR line tagged `class=permanent`, fires the throttled alert with the operator-inspection suffix, and returns. Recovery on the next iteration will fail the same way, so the alert is the operator's signal to SSH in and investigate.

Unclassified errors default to **transient** — the conservative choice is to retry, since operators have the chatops `🛑 perma-stuck` plus manual-skip escape hatches when a genuinely-permanent failure mis-classifies. The classification logic applies to **mid-iteration recovery only**; startup-time recovery (the initial `repo_passes_startup_check` pass) keeps its skip-for-lifetime contract for any failure — a future spec may extend classification there too.

Operators who want to inspect a dirty workspace before any daemon action should stop the systemd unit first:

```bash
sudo systemctl stop autocoder
# inspect /tmp/workspaces/<repo>/ at your leisure
sudo systemctl start autocoder
```

## Busy marker

At the start of each polling iteration, autocoder writes a per-repo JSON marker at `/tmp/autocoder/busy/<workspace-basename>.json` and holds it through every stage of the pass (executor → review → push → PR). The marker is removed when the pass returns normally. A daemon crash that bypasses normal cleanup (SIGKILL, segfault, host power loss) intentionally leaves the marker for the next pass to discover.

Marker contents: `repo_url`, `pid`, `pgid` (Linux process group for `killpg` recovery), `comm` (process name from `/proc/<pid>/comm` at acquire time), `started_at`, and `stage` (one of `executor`, `commit`, `review`, `push`, `pr`).

On the next iteration's startup, autocoder classifies any pre-existing marker in this order — the first matching row wins:

| Marker state | Action |
|---|---|
| File absent | Acquire, run iteration |
| Malformed JSON | Treat as stale: WARN log, clear marker, proceed |
| **PID dead** (recorded `pid` not in `/proc`) | **Auto-recover IMMEDIATELY: clear marker, WARN log, proceed. NO age check** — a pid that no longer exists cannot be doing legitimate work |
| Age < `executor.busy_marker_stale_threshold_secs`, PID alive | Skip iteration with INFO log (`age=… threshold=… pid_alive=true recovery_eligible=false`) — another pass is working |
| Age ≥ threshold, PID alive + `comm` matches | Stuck: `SIGTERM` the process group, wait 5s, `SIGKILL` if still alive, clear marker, post chatops alert, proceed |
| Age ≥ threshold, PID alive + `comm` differs | Ambiguous (PID reuse suspected) — ERROR log, post chatops alert, SKIP iteration, leave marker for human inspection |

The stale-threshold is a dedicated `executor.busy_marker_stale_threshold_secs` config field (default `600` seconds = 10 minutes, max `7200` clamped with a WARN). It is **decoupled** from `executor.timeout_secs` — raising the executor timeout for one legitimately long-running change does NOT proportionally delay stale-marker recovery on unrelated iterations.

Pre-`a08-busy-marker-recovery-semantics` builds derived the threshold as `executor.timeout_secs + 600`, which had two problems: (1) a daemon killed mid-iteration left a dead-pid marker that the next pass refused to recover until the derived threshold elapsed (51+ minute production incidents); (2) bumping `timeout_secs` for one stubborn change silently delayed stale-marker recovery on all other iterations. Both are fixed: dead-pid markers recover immediately, and the live-pid stale threshold is now a separate operator-controlled field.

When a daemon upgrades to a build that ships this fix AND the operator has NOT set `busy_marker_stale_threshold_secs` explicitly AND the pre-spec implicit threshold (`timeout_secs + 600`) would have been longer than the new default, the daemon emits one INFO line at startup naming both values:

```
busy marker stale threshold is now 600s (was implicit 6000s via timeout_secs+10min). \
Pre-spec operators raising timeout_secs no longer see proportional recovery delays. \
Set executor.busy_marker_stale_threshold_secs explicitly to override.
```

Operators who genuinely need the longer threshold (executor expected to legitimately not check in for >10 min) set the field in `config.yaml`:

```yaml
executor:
  timeout_secs: 5400
  busy_marker_stale_threshold_secs: 5500
```

The INFO line emitted when an existing marker is skipped now carries the marker's age, the resolved threshold, the PID-alive state, and a `recovery_eligible` boolean — operators reading `journalctl` see the diagnostic state inline:

```
INFO busy marker present; skipping iteration url=git@github.com:owner/repo.git pid=490170 \
     stage=executor age=53m threshold=10m pid_alive=false recovery_eligible=true
```

Operators inspecting the file:
```bash
sudo -u autocoder cat /tmp/autocoder/busy/<basename>.json
```

To force a recovery from a stuck state, stop the systemd unit, delete the marker file, and start the unit again:
```bash
sudo systemctl stop autocoder
sudo -u autocoder rm /tmp/autocoder/busy/<basename>.json
sudo systemctl start autocoder
```

The per-change run logs (`<logs_dir>/runs/<basename>/<change>.log`) and the busy markers share the same daemon-paths root.

If you're seeing operator-visible inconsistencies between writers and readers (`status` says idle while the busy marker exists; `send it` returns `?` on a real audit thread), check `journalctl` AND the resolved paths the daemon is using — this class of bug is prevented going forward by the `path_literals_audit` CI test introduced in `a09`, which fails the build on any new hard-coded `/tmp/autocoder/` literal in `autocoder/src/`. See [`docs/STATE-LAYOUT.md`](STATE-LAYOUT.md#path-resolution-rule) for the resolver-only rule.

## Per-change run log shape

Each iteration writes TWO sibling log files per change (per `a20a2`):

- **Summary log** at `<logs_dir>/runs/<workspace-basename>/<change>.log` — operator-facing. Contains PROMPT, an ACTIONS pointer line, FINAL ANSWER, and STDERR sections. Short, signal-dense, no agent-controllable action stream.
- **Stream log** at `<logs_dir>/runs/<workspace-basename>/<change>.stream.log` — verbose. The full action stream (`[tool_use]`, `[tool_result]`, `[assistant]`, `[raw]`, `[unknown:<type>]` lines). Consulted when diagnosing what the agent actually did.

The split exists to isolate the high-volume agent-controllable action stream from the daemon-readable summary. Daemon-internal consumers (sentinel scanner, PR-comment composer) read from the summary log; they cannot be tricked by sentinel-shaped substrings hiding in tool-result echoes.

Summary log shape (with `executor.output_format: json`, the default):

```
=== PROMPT (<n> bytes) ===
<the full prompt sent to the wrapped Claude CLI>

=== ACTIONS (see <change>.stream.log) ===

=== FINAL ANSWER (<n> bytes) ===
<the agent's closing conversational summary — same content the PR comment shows>

=== STDERR (<n> bytes) ===
<anything the wrapped CLI emitted on stderr, typically empty>
```

Stream log shape (no section headers, one continuous stream):

```
[tool_use] Read autocoder/src/foo.rs
[tool_result] (4128 bytes returned)
[tool_use] Edit autocoder/src/foo.rs
[tool_result] (200 bytes returned)
[assistant] I've identified the issue in line 42 and applied the fix.
[tool_use] Bash cargo test --lib
[tool_result] (1024 bytes returned)
...
```

Section descriptions:

- **PROMPT** (summary) — exactly what autocoder sent on stdin (template + `openspec instructions apply` output + the per-change context). Use this when an agent ran on the wrong prompt.
- **ACTIONS pointer** (summary) — one line naming the sibling stream log file. The slot exists so the structural invariant ("all four section headers present") holds even when the stream is empty.
- **FINAL ANSWER** (summary) — the closing `result` event's text, captured separately so it is the ONE thing the PR's `## Agent implementation notes` comment shows. Empty when the run timed out before reaching `result`.
- **STDERR** (summary) — bytes the wrapped CLI wrote on stderr. Usually empty; populated on framework errors.
- **Stream content** — one line per JSON event the wrapped CLI emitted. `[tool_use]`/`[tool_result]` for Read/Edit/Bash tool activity (results show byte counts), `[assistant]` for intermediate assistant text, `[raw]` for lines that failed JSON parsing, `[unknown:<type>]` for forward-compat event types. When triaging a timeout, the last line names what the agent was doing when the kill fired.

Operator CLI snippets:

```bash
# Read the operator-facing summary (PROMPT + FINAL ANSWER + STDERR):
tail -f /var/log/autocoder/runs/<basename>/<change>.log

# Watch the live action stream during a run:
tail -f /var/log/autocoder/runs/<basename>/<change>.stream.log

# Grep for specific tool activity:
grep '\[tool_use\] Edit' /var/log/autocoder/runs/<basename>/<change>.stream.log
```

**Migration note for operator tooling.** Tools that previously grepped `<change>.log` for `[tool_use]`, `[tool_result]`, `[assistant]`, `[raw]`, OR `[unknown:<type>]` patterns SHALL be redirected to the `<change>.stream.log` file. The summary log no longer contains those patterns — only the pointer line. The change is a simple file-path swap in your scripts.

The legacy log shape (`=== STDOUT === / === STDERR ===`) is preserved when `executor.output_format: text` is set; that mode skips JSON event parsing entirely and uses today's at-exit capture. Text mode does NOT produce a stream log file.

**Retention.** Per-change logs are pruned at daemon startup and once every 24 hours during operation. A summary log is eligible for deletion when its mtime is older than `executor.log_retention_days` (default 30) AND its corresponding change directory under `openspec/changes/<change>/` no longer exists. Deletion is **pair-atomic**: when a summary log is deleted, its sibling stream log is deleted in the same pass. Active changes' logs are preserved as a pair regardless of age — operators triaging a long-running stuck change want both files even if old. Orphan stream logs (a `<change>.stream.log` without its summary sibling, from a pre-spec migration or manual operator action) are cleaned up with a WARN when their age exceeds the window and no change directory exists.

**PR-comment stability.** The `## Agent implementation notes` comment on every PR continues to contain ONLY the agent's closing conversational summary — the same content operators have always seen since the section was introduced. With JSON streaming mode on, autocoder captures that text more precisely (from the closing `result` event) instead of slicing it out of the raw stdout buffer, but reviewers see the same shape. The intermediate tool-call stream stays in the log file and never ships to GitHub. Existing PR-review workflows do not change.

**PR commit ordering (a12).** When an iteration produces both pending change implementation commits AND audit creation commits, the implementation commits land FIRST on the agent branch and the audit creation commits land AFTER. This follows directly from the iteration-sequence change in `a12-changes-have-precedence-over-audits` — the pending queue walk runs before the audit phase, so its commits are older on the agent branch. Reviewers scanning the PR's commit list see the change work at the top. (Prior to `a12`, audit creation commits came first because audits ran before `list_pending`.)

## Pre-flight checks

Before invoking the executor on a pending change, autocoder runs a layered set of pre-flight checks. Each catches a different failure mode AND has a different cost; together they prevent the expensive-then-fail cycle of running the executor on a change that would have failed at archive time anyway.

The checks run in order. A failure in any layer halts the queue walk for the iteration (writes `.needs-spec-revision.json` AND posts a chatops alert under `AlertCategory::SpecNeedsRevision`); the executor is never invoked.

| Layer | Purpose | Cost | Opt-in | Failure mode |
|---|---|---|---|---|
| **1. `openspec validate --strict`** | Well-formedness — frontmatter present, sections named correctly, scenarios use proper `WHEN`/`THEN` structure, normative keywords (`SHALL` / `MUST` / `SHOULD`) appear. The change couldn't be loaded by `openspec` at all without passing this. | Free (mechanical, sub-millisecond) | Always on | The change is excluded from `list_pending` until the operator fixes the structural problem AND re-runs `openspec validate`. |
| **2. Archivability check (a17)** | Mechanical structural check: each `## ADDED` / `## MODIFIED` / `## REMOVED` / `## RENAMED Requirements` block's `### Requirement:` header must satisfy its kind's precondition against the canonical `openspec/specs/<capability>/spec.md` (e.g. MODIFIED's title MUST exist; ADDED's title MUST NOT exist). Catches the a07 class of bug where an invented title was used. | Free (mechanical, sub-millisecond) | Always on | `.needs-spec-revision.json` written with `unarchivable_deltas` populated. Operator action: edit the spec deltas to match canonical AND clear the marker. See [Spec marked as needing revision](#spec-marked-as-needing-revision). |
| **3. Change-internal contradiction check (a19)** | LLM-based semantic check: detects requirements within the same change that cannot all hold simultaneously (e.g. ADDED A "all secrets in env vars" + ADDED B "API key in `config.yaml`"). The LLM input is small (concatenated spec-delta files only); cost is **~$0.01 per checked change** at current pricing. | LLM call (~$0.01 per checked change) | Opt-in: `executor.change_internal_contradiction_check: enabled` + `executor.change_internal_contradiction_check_llm:` block | `.needs-spec-revision.json` written with `revision_suggestion` populated from the contradictions narrative (`unarchivable_deltas` AND `unimplementable_tasks` left empty for this semantic case). **Fail-OPEN posture:** LLM transport / parse / malformed-response failures log a WARN AND proceed to the executor; the daemon does NOT gate work on a failed check. Operators see the WARN cadence in journalctl AND decide whether to investigate. |

**Opt-in posture for the contradiction check.** Initially shipped disabled because (a) the small per-change LLM cost is non-zero AND charged regardless of whether the check finds anything; (b) early operator experience may produce false positives — the gated rollout lets operators opt in when ready; (c) the prompt template needs iteration AND operators may want to override it for their domain.

Operators trading a small per-change LLM cost for the catch of semantic self-contradictions enable it. Default-off operators see no behavior change.

**Why fail-open for the contradiction check?** A flaky pre-flight should not block work. The same conservative bias applies as `a14`'s transient-failure handling: better to incur a redundant executor run than to halt the queue on a transient LLM outage. The WARN log lets operators investigate via journalctl AND decide whether to investigate further.

## Self-heal for already-implemented changes

When a rebase or merge lands the work for a change on the base branch without moving the change directory into `archive/`, the agent sees the implementation already done and returns `Completed` without modifying the workspace. Normally that's classified as Failed (no-op completion) and retried on every poll, burning tokens to re-confirm the same answer. autocoder self-heals this case instead:

When the executor returns `Completed`, `git status --porcelain` is empty, `openspec validate <change> --strict` exits 0, AND every checkbox in `openspec/changes/<change>/tasks.md` is `[x]`, autocoder runs the archive move itself, commits it with subject `archive: <change>: implementation already in base`, and ships a PR through the normal push + PR flow.

If any of the four preconditions fails — including `openspec validate` erroring or any task still `[ ]` — autocoder falls through to the existing Failed path, so non-self-heal cases retain their prior behavior.

The PR body for a pass that self-healed one or more changes is prefixed with:

> _This PR archives one or more changes whose implementation was already present on the base branch. No code diff is included; only the openspec archive move._

The disclaimer identifies these passes for reviewers regardless of whether the pass also includes normally-implemented changes.

## Skipping iterations while a PR is open

Before each polling iteration begins its work, autocoder queries GitHub for open PRs whose `head` matches the configured agent branch (`<fork_owner>:<agent_branch>` in fork-PR mode, `<repo_owner>:<agent_branch>` in direct mode, base = the configured base branch). If an open PR is found, the iteration is skipped: no executor invocation, no commits, no push, no PR creation attempt. The skip persists until the open PR is closed or merged. This prevents the daemon from re-implementing the same changes on every poll while a PR sits awaiting review, which would otherwise force-push new commits over the PR's branch and burn agent tokens redundantly.

To re-implement after rejecting a PR: close it (don't merge). The next poll proceeds. To accept the implementation: merge it; the archive moves land on the base branch and the changes drop out of `list_pending`.

If the GitHub query itself fails (transport error, non-2xx), the iteration proceeds as if no PR existed — better to incur a redundant Claude run than to halt the repo on a flaky API. The failure is logged at WARN.

## Migrations

At startup, the daemon checks for one-shot migration markers and runs
each missing migration before any polling task starts. Marker files are
written only when a migration completes cleanly; a per-entry failure
suppresses the marker so the next startup retries.

| Marker | Migration | When it runs | Force re-scan |
| --- | --- | --- | --- |
| `<state>/.migration-from-tmp-done` | `state-paths-out-of-tmp` (`a07`) — moves legacy `/tmp/...` workspaces, state files, and run logs into the resolved standard layout (`<cache>/workspaces/`, `<state>/<shape>/`, `<logs>/runs/`). Cross-partition `EXDEV` falls back to copy + delete. | First startup after upgrading to the `a07` build, if any legacy `/tmp/` data is present. | `rm <state>/.migration-from-tmp-done` and restart. |
| `<state>/alert-state/.migration-from-workspace-done` | `alert-state-from-workspace` (`a16`) — moves any pre-existing `<workspace>/.alert-state.json` into `<state>/alert-state/<workspace-basename>.json`. If both versions exist, the state-dir copy wins. If the workspace file is tracked by git, runs `git rm --cached` + commit + push to the base branch. | First startup after upgrading to the `a16` build. | `rm <state>/alert-state/.migration-from-workspace-done` and restart. |

Both migrations are idempotent (already-migrated workspaces become
no-ops on re-scan) and per-repo error-tolerant (one failing repo does
not block the rest). Per-entry failures are logged at ERROR with the
suggested operator action; `journalctl -u autocoder | grep migration`
surfaces every line.

---

## Onboarding existing projects {#onboarding-existing-projects}

autocoder's spec-driven workflow assumes `openspec/specs/<capability>/spec.md` already exists for every capability the operator wants to evolve. When you onboard a repository that predates spec-driven development — or you're working in an OSS-contribution workspace where specs live in a sibling repo separate from the upstream project — you start with no canonical specs at all. The `brownfield` chatops verb is the first step.

**Brownfield-drafting as the entry point.** `@<bot> brownfield <repo> <capability-name> [optional guidance]` queues a brownfield-draft run for the named capability. The polling iteration reads the codebase, drafts a `openspec/changes/brownfield-<capability-name>/` change that captures **existing** behavior (no code modifications), AND opens a spec-only PR. Operators review the PR like any other autocoder-opened PR, iterate via `@<bot> revise <text>` until the spec matches reality, AND merge. After merge, `openspec/specs/<capability-name>/spec.md` is canonical AND `brownfield` will refuse to overwrite it on subsequent invocations. See [CHATOPS.md → `brownfield`](CHATOPS.md#drafting-a-spec-for-existing-behavior-brownfield) for the verb syntax, refusal cases, AND lifecycle-thread behavior.

**`brownfield` vs `propose`.** The two verbs cover the full lifecycle of bringing an existing project under spec-driven development:

- `brownfield` — **one-shot per capability**. Documents existing behavior. Produces a spec-only PR. Use it when the capability has no canonical spec yet.
- `propose` — **per-change**. Proposes new behavior or a modification as a spec change. Produces a single spec-only PR (a43); after merge, the implementer writes the code on the next iteration. Use it for every change after the capability's canonical spec exists.

`brownfield` refuses with a pointer to `propose` when the canonical spec already exists in the workspace, so the boundary is enforced at the verb level.

**Recommended cadence.** Run one brownfield per capability. Review the resulting PR, merge (or iterate via `revise` first), THEN move on to the next capability. Running multiple brownfields concurrently against the same repo works (the polling iteration drains them one per pass), but it tends to flood reviewers with overlapping context — the human review step is what makes the spec accurate, AND parallel reviews scale poorly. The polling iteration deliberately drains AT MOST one brownfield per pass to keep the iteration cost predictable.

**Capability granularity.** The capability name is the operator's call. Reasonable slices include: `scheduler`, `auth`, `chatops-manager`, `executor`, `audits-framework`. Avoid slices that are too broad ("the whole CLI" — most projects can't slot that into one cohesive set of requirements) OR too narrow (one helper function — the spec layer is the wrong granularity for a single utility). When the LLM can't reconcile your slug with one cohesive slice of the codebase, the resulting proposal's "Why" section surfaces the ambiguity AND draft a best-effort spec — `revise` from there until the boundary makes sense.

**OSS-fork mode.** When `openspec/` lives in a sibling repo separate from the upstream project (the "specs-only fork" pattern), brownfield is the bootstrap step that produces the initial canonical-spec set the rest of autocoder's machinery reads from. After every capability's canonical spec is in place, `propose` handles the ongoing work the same way it would for a green-field project.

For the "whole codebase has no specs yet AND I don't yet know what the right capability boundaries are" case, see [Bootstrapping specs for an existing project](#bootstrapping-specs-for-an-existing-project) below.

---

## Bootstrapping specs for an existing project {#bootstrapping-specs-for-an-existing-project}

When a codebase has no canonical specs at all AND the operator does NOT already have a list of "the 8 capabilities here are X, Y, Z…" memorized, the recommended workflow is the **survey → review → `send it` batch** loop. This sits one layer above the single-capability [`brownfield`](#onboarding-existing-projects) verb: instead of asking the operator to identify each capability by name in turn, the daemon proposes a list AND lets the operator decide what to batch-generate.

**When to use this vs single-capability `brownfield`.**

- Use **`brownfield-survey` + `send it`** for whole-project bootstrap — the codebase is new to you, you want a curated list of plausible capability boundaries, AND batch generation is more efficient than typing one `@<bot> brownfield` per capability.
- Use **`brownfield`** (per `a23`) for the targeted "this one capability needs a spec" case — you already know the slug, the rest of the codebase is either specced OR irrelevant.

The two flows interoperate: a `brownfield-survey` excludes already-specced capabilities AND the batch handler skips an item whose `openspec/specs/<slug>/spec.md` appears mid-batch (e.g., because the operator merged a sibling `brownfield` PR in parallel).

**The loop.**

1. **Survey.** `@<bot> brownfield-survey <repo> [optional guidance]` queues a survey run. The polling iteration invokes the agent CLI under a read-only sandbox (Read, Glob, Grep, Bash read-only) with the embedded `prompts/brownfield-survey.md` system prompt. The handler validates the executor's JSON response, persists `<workspace>/.state/brownfield_surveys/<request_id>.json`, AND posts the rendered capability list to the lifecycle thread. The list is capped at `features.brownfield_survey.max_capabilities` (default 20; valid range `1..=50`).
2. **Review.** Read the list. Each item names a proposed slug, a complexity heuristic (`small` | `medium` | `large`), a one-line summary, scope-in / scope-out paragraphs, AND the source-tree paths the capability covers. The survey's tone is "candidates for consideration," not ranked recommendations.
3. **Refine, if needed.** If the list looks off — too broad, too narrow, miscategorized boundaries — re-run `@<bot> brownfield-survey <repo> <refined guidance>` with focus text (e.g., `focus on the data layer; skip CLI commands which are well-understood`). The fresh survey supersedes the prior one. Repeat until the list is acceptable.
4. **Batch.** Reply inside the survey thread with `@<bot> send it`. The bot transitions the survey to `InProgress` AND replies `✓ Queued <N> capability spec generations. The first will start on the next iteration.` Subsequent polling iterations drain **one item per iteration**, each invoking the canonical [single-capability brownfield flow](#onboarding-existing-projects) for that slug. The item's `scope_in`, `scope_out`, AND `source_modules` are appended to the brownfield prompt so the LLM scopes its draft accordingly.
5. **Per-item progress.** The lifecycle thread receives one status reply per item: `✅ Spec PR opened for \`<slug>\` (M/N done): <pr-url>` on success; `⏭ Skipped \`<slug>\` (M/N done): spec already exists.` when the spec file appears mid-batch (e.g., the operator merged a sibling `brownfield` PR); `✗ Spec for \`<slug>\` failed (M/N done): <reason> (continuing with next)` on per-item failure. A per-item failure does NOT abort the batch — the next iteration moves to the next item.
6. **Completion.** When every item reaches a terminal state, the bot posts `✅ Brownfield batch complete. <X> succeeded, <Y> skipped (already specced), <Z> failed.` The survey state's `status` flips to `Completed` AND the state file remains on disk for audit (operators can prune with `@<bot> clear-survey <repo>`).

**Why one item per iteration?** Each brownfield run gets its own fresh executor invocation. A whole-project batch run as a single executor pass would hit context compression mid-batch — once the model crosses the compression threshold, later capabilities receive less attention than earlier ones AND the resulting specs degrade. Spreading the work across iterations is the deliberate mechanism for keeping every capability's draft as fresh as the first.

**Worked-example operator transcript.**

```
operator: @<bot> brownfield-survey myrepo focus on the storage and scheduling layers
<bot>:    ✓ Queued brownfield-survey for git@github.com:acme/myrepo.git.
          The next polling iteration will run it (~Nm). Follow along in this thread.

(One iteration later, in the thread:)
<bot>:    📋 Surveyed capabilities for git@github.com:acme/myrepo.git:

          1. `scheduler` — medium — Cron-style job scheduling
             Scope-in:  …
             Scope-out: …
             Source:    src/scheduler/, src/cron/

          2. `storage-engine` — large — Pluggable backend for durable state
             Scope-in:  …
             Scope-out: …
             Source:    src/storage/

          3. `migrations` — small — Schema migration runner
             …

          Reply with @<bot> send it to batch-generate ALL 6 specs (one per iteration).
          Or re-run @<bot> brownfield-survey <repo> <refined guidance> to refresh.

operator: (in the thread) @<bot> send it
<bot>:    ✓ Queued 6 capability spec generations. The first will start on the next iteration.

(Six iterations later:)
<bot>:    ✅ Spec PR opened for `scheduler` (1/6 done): https://github.com/acme/myrepo/pull/42
<bot>:    ✅ Spec PR opened for `storage-engine` (2/6 done): https://github.com/acme/myrepo/pull/43
<bot>:    ⏭ Skipped `migrations` (3/6 done): spec already exists.
…
<bot>:    ✅ Brownfield batch complete. 4 succeeded, 1 skipped (already specced), 1 failed.
          See the survey thread for individual PR links AND failure reasons.
```

**Revision loop.** Each spec PR is a normal autocoder-opened PR participating in the standard `@<bot> revise <text>` flow (see [Revising an open PR via comment](#revising-an-open-pr-via-comment)). Revising mid-batch is safe — the revise handler operates per-PR AND does not interact with the in-progress survey state.

**Configuration.** The survey is enabled by default. Toggles, limits, AND prompt override live under `features.brownfield_survey` — see [CONFIG.md → `features.brownfield_survey`](CONFIG.md#featuresbrownfield_survey).

**Operator recovery.** `@<bot> clear-survey <repo>` wipes every `BrownfieldSurveyState` file for the matched repo. Use it to abort an in-progress batch (the next iteration's drain finds no in-progress survey AND becomes a no-op), free the workspace's "one-batch-at-a-time" slot, OR force the next survey to start from a clean slate. See [CHATOPS.md → `clear-survey`](CHATOPS.md#clear-survey).

---

## Finding things to work on {#finding-things-to-work-on}

`propose`, `brownfield`, AND `send it` all assume the operator has already decided what to work on. The upstream question — "I have a few minutes and an unfamiliar (or long-running) codebase; what's worth looking at?" — is what the `scout` → pick → `spec-it` loop is for.

The recommended discovery cadence is three steps:

1. **Scout.** `@<bot> scout <repo> [optional focus]` queues a scout-mode executor pass. The polling iteration runs the agent CLI under a read-only sandbox (Read, Glob, Grep, Bash including `gh` for the open-issues fetch), AND posts a curated triage list of opportunities to the lifecycle thread. The list is grouped by category (`security`, `bug`, `error_handling`, `type_tightening`, `code_smell`, `perf`, `documentation`, `test_coverage`, `issue`, `todo_fixme`, `research`) with one line per item including a source pointer (`<file>:<line>`, an issue URL, or a commit range) AND a tractability tag (`small`, `medium`, `large`).
2. **Pick.** Read the list. The scout's tone is "things you might consider," not ranked recommendations — the operator decides which item is worth pursuing.
3. **Spec-it.** Reply inside the same scout thread with `@<bot> spec-it <N> [optional guidance]`. The polling iteration translates the picked item into a `ProposeRequest` (the item's title + body + source/category/tractability lines + your guidance) AND hands it to the standard propose lifecycle. The resulting spec-only PR matches a normal `@<bot> propose` run.

When to use the loop:

- **OSS-contribution workspaces.** You've forked an unfamiliar upstream project AND want to land small targeted PRs (swallowed errors, type tightening, security gaps). Scout's category list maps directly to the kinds of contributions reviewers welcome; `spec-it` produces the same spec-and-PR shape the rest of the autocoder workflow expects.
- **Long-running owned projects.** You periodically want a "what would a fresh pair of eyes notice" pass without committing to any specific fix. Run `@<bot> scout <repo>` with a focus area (e.g., `focus on error handling` or `focus on the auth surface`) AND let the curated list tell you whether anything's worth scoping.

**Staleness.** If you wait more than `features.scout.staleness_warn_days` days (default 7) OR the workspace HEAD has moved since the scout ran, `spec-it` posts a warning naming the gap before submitting the propose-request. The warning does NOT block; if the picked item is no longer relevant, re-run `@<bot> scout <repo>` for a fresh list. Operators who want to force a fresh state can wipe prior runs via `@<bot> clear-scout <repo>` (see [CHATOPS.md → `clear-scout`](CHATOPS.md#clear-scout)).

**Configuration.** Scout is enabled by default. Toggles AND limits live under `features.scout` — see [CONFIG.md → `features.scout`](CONFIG.md#featuresscout) for the schema.

---

## Canonical-spec RAG

When the operator configures `canonical_rag:` in `config.yaml` (see
[CONFIG.md → `canonical_rag:`](CONFIG.md#canonical_rag-optional)), the
daemon maintains a per-workspace in-memory vector store over each
`openspec/specs/<capability>/spec.md` file. The implementer's MCP child
exposes a `query_canonical_specs(query, top_k?)` tool that relays
through the daemon's control socket so the agent can retrieve
canonical-spec chunks ranked by semantic similarity.

### Re-embed cadence

The pipeline re-embeds at exactly two events:

1. **Workspace init.** The first iteration of a workspace after daemon
   start (OR after a workspace wipe) embeds the entire canonical
   corpus synchronously before invoking the executor. Subsequent
   iterations skip if embeds already exist.
2. **Post-archive.** After an iteration's archive lands a commit that
   touched at least one `openspec/specs/<cap>/spec.md` file, ONLY the
   affected capabilities are rebuilt — not the whole corpus. Detection
   is `git diff --name-only HEAD~1 HEAD -- openspec/specs/`. When
   `reembed_on_archive: false`, this step is skipped and the store
   goes stale until restart.

### In-memory persistence

Embeds are kept in RAM only. There is NO on-disk cache. Daemon restart
re-embeds from scratch for every configured workspace — typically
sub-second on GPU AND ~30 seconds on CPU for a typical corpus. The
cost is paid once at startup AND once per archive that touches
canonical specs; queries themselves take tens of milliseconds (one
embed + cosine sim across O(1000) chunks).

### Failure modes

- **Embedding-provider error at init** (network, auth, rate-limit) →
  WARN log naming the error AND the workspace's store is omitted from
  the daemon's registry. Subsequent `query_canonical_specs` calls
  return `{"hits": [], "error_hint": "rag init failed; see daemon log"}`.
  The polling iteration proceeds normally — RAG availability never
  gates iteration progress. Subsequent iterations retry the init.
- **Per-query provider error** → WARN log AND empty array returned to
  the caller with `error_hint: "query failed: <reason>"`.
- **Control-socket unreachable from the MCP child** → the tool returns
  `{"hits": [], "error_hint": "control socket unreachable: <error>"}`
  after a 10-second timeout. The agent falls back to its non-RAG
  behaviour.
- **MCP child env vars absent** (RAG not configured for this execution)
  → tool returns `{"hits": [], "error_hint": "rag not configured for this execution"}`
  without attempting a socket connection.

### Cost expectations

- Embedding: sub-second on GPU; ~30s on CPU for a typical corpus
  (~50 capabilities, ~500 chunks).
- Re-embed-on-archive: typically a fraction of cold-start cost (1
  capability, not the whole corpus).
- Per-query: tens of milliseconds (one embed call + cosine sim across
  ≤O(1000) chunks; no external calls beyond the embed).

### Operator log lines

- `canonical RAG embedded N chunks for workspace <basename>` —
  successful workspace-init embed.
- `canonical RAG re-embedded N capabilities after archive: [...]` —
  successful post-archive re-embed.
- `canonical RAG workspace-init failed: <error>; query_canonical_specs
  will return empty Vec` — WARN at init.
- `canonical RAG post-archive re-embed failed: <error>; prior embeds
  retained` — WARN at archive.

## OSS contribution workflow

Autocoder's default deployment model assumes the operator owns every
repository it works on: specs live alongside the code, PR creation is
automatic, and the workspace's only remote is the operator's own host.
For OSS-contribution workflows — where the operator wants autocoder to
help land small, targeted PRs against upstream projects they do NOT
own — three per-repo config knobs (a26) combine into a coherent
fork-friendly workflow.

This section describes the recommended setup as a discrete operator
workflow. The knobs are independently useful, but for the canonical
OSS-fork case you'll likely want all three.

### Step-by-step setup

1. **Fork the upstream project on GitHub.** Use your personal account
   (or any account you control); autocoder will iterate directly on
   this fork.

2. **Clone the fork as the autocoder workspace.** Either point your
   per-repo `url` at the fork (and let autocoder clone), or pre-clone
   the fork to your workspace directory.

3. **Configure the `upstream` block** pointing at the upstream repo
   (the one you do NOT own):

   ```yaml
   repositories:
     - url: "git@github.com:my-handle/upstream-project-fork.git"
       base_branch: main
       agent_branch: agent-q
       poll_interval_sec: 1800
       upstream:
         remote: upstream      # default
         branch: main          # default
         url: "https://github.com/upstream-org/upstream-project.git"
   ```

   Autocoder will ensure the workspace has an `upstream` remote
   pointing at this URL AND will `git fetch upstream` opportunistically
   at the start of every polling iteration. The fetch is best-effort:
   failures log a WARN but do not block the iteration. This block
   enables — but does NOT trigger — automatic upstream syncing.
   Operator-initiated sync runs via `@<bot> sync-upstream <repo>`
   (see `docs/CHATOPS.md`).

4. **Set `auto_submit_pr: false`** so autocoder pushes the agent
   branch but does NOT auto-open a PR to upstream:

   ```yaml
   repositories:
     - url: "git@github.com:my-handle/upstream-project-fork.git"
       # ...
       auto_submit_pr: false
   ```

   At end-of-iteration, the chatops notification posts the branch URL
   AND a `gh pr create` command suggestion so you can review the work
   locally before submitting the upstream PR yourself. Auto-submitting
   bad PRs to upstream projects damages your reputation with
   maintainers in a way internal PR mistakes do not — this knob
   prevents that failure mode.

5. **Configure `spec_storage.path`** pointing at a sibling specs repo
   you own:

   ```yaml
   repositories:
     - url: "git@github.com:my-handle/upstream-project-fork.git"
       # ...
       spec_storage:
         path: "../my-specs"   # workspace-relative; absolute also OK
   ```

   Canonical specs cannot live inside the upstream repo — that would
   force unrelated `openspec/` directories into every PR. Setting
   `spec_storage.path` redirects spec reads AND writes to an external
   git working tree. The path SHALL be a directory that is a git
   working tree (verified at config-load via `git -C <path> rev-parse
   --is-inside-work-tree`) AND contain an `openspec/` subdirectory.

6. **(Optional) Tighter implementer-prompt override.** OSS upstream
   maintainers value minimal-diff PRs that follow the project's
   existing conventions over sweeping refactors. Override
   `executor.implementer.prompt_path` with a tighter prompt:

   ```yaml
   executor:
     implementer:
       prompt_path: "./prompts/oss-implementer.md"
   ```

   Sample snippet operators can adapt (save as
   `./prompts/oss-implementer.md`):

   ```
   You are implementing a change in an upstream open-source project
   that the operator does NOT own.

   Hard constraints:
   - Minimal diff: change only what the task requires.
   - Follow the project's existing conventions (naming, formatting,
     test placement, file structure).
   - No large refactors. No drive-by cleanups.
   - No new abstractions unless the task explicitly demands one.
   - If you find existing tech debt adjacent to your change, leave
     it alone unless touching it is strictly necessary.

   Before writing code, scan three similar features in the existing
   codebase and match their idiomatic patterns. The reviewer (an
   upstream maintainer with no autocoder context) will reject PRs
   that don't blend in.
   ```

   Adapt the constraints to the specific upstream project's culture
   (some projects welcome refactors; some are strict no-mixing).

### The typical operator loop

Once configured, your daily loop is:

1. `@<bot> scout <fork>` — survey the upstream codebase and get a
   triage list of opportunities.
2. `@<bot> spec-it <N>` — pick an item from the scout list; autocoder
   drafts a spec into your `spec_storage` repo AND a fork PR opens
   against the fork's `agent_branch`.
3. Review the fork PR — both the spec change (in the
   `spec_storage` repo) AND the code change (in the fork repo).
4. Merge the fork PR — agent branch lands on the fork's base branch.
5. `gh pr create --base main --head <branch>` — manually submit the
   upstream PR after a final polish (rewrite the PR description for
   the upstream audience, squash trivial commits, etc.).
6. (Periodically) `@<bot> sync-upstream <fork>` — pull upstream's
   newer commits into your fork's base branch so future agent runs
   start from current upstream.

### Why three independent knobs instead of one mode

Each knob is useful in isolation:

- **`spec_storage`** is useful even on own-projects when the operator
  wants spec history in a separate tree (auditability, multi-project
  spec sharing).
- **`upstream`** is useful even with `auto_submit_pr: true` for any
  fork-based deployment that wants opportunistic upstream visibility.
- **`auto_submit_pr: false`** is useful even on own-projects for
  sensitive repos where every PR should pass an operator eyes-on
  gate.

Treating them as separate knobs gives you flexibility; combining all
three gives you the canonical OSS-fork workflow.

### Caveats

- `auto_submit_pr` applies UNIFORMLY to both code-workspace AND
  `spec_storage` PRs (when set). If you want different behavior for
  the two repos, split the workspace into separate per-repo
  configurations.
- `sync-upstream` NEVER pushes the rebased base branch. After a
  successful rebase, you decide when to `git push --force-with-lease
  origin <base>` to update the fork's base branch. This is intentional:
  autocoder should not silently overwrite the fork's published history.
- The opportunistic `git fetch upstream` is best-effort. If you need a
  guaranteed-current view, run `@<bot> sync-upstream <repo>` directly.
