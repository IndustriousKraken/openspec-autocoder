# TODO

Design questions and future-work items that aren't yet ready for an OpenSpec change proposal. Each section is a candidate spec; when the design solidifies enough to draft, lift the section into `openspec/changes/<slug>/proposal.md`.

## Cohesion check and compression of canonical spec

An audit that can be triggered to check the canon for internal consistency - ie: the whole canon does not contradict itself with conflicting reqs. Example: "All data stored in a relational database" and "Store the customer records in MongoDB." The second is impossible if we respect the first, so if Mongo really is required, the spec author should MODIFY the original spec when adding the Mongo dependency. If that slipped past the spec-checking gates, or if the spec existed before said gates existed, then we have this audit to discover the contradictions, alert the repo maintainers, and create a spec to heal them. At the same time, I think there are specs that likely grow in an inefficient way. There could be duplicate specs for the same thing, for example. Or specs that add two things as separate when they are so closely related that they should really be one spec. For example, if the repo initially implements Stripe API and later adds Paypal API, some basic elements of the scenario / spec might overlap completely with different titles. Evaluate whether a spec-refactoring-spec is a good idea for cases like this. It might keep specs compact and readable, and avoid needing to read 10+ specs in the canon just to understand one thing. The user would look over these in the PR (instructions should be clear that user involvement is important) to ensure there isn't a subtle information loss. For example, we might end up with a spec that combines "All data will be stored in a relational database" with "Make a postgresql database available" to get "All data will be stored in Postgresql". The information loss is that the first spec was a prescription for the whole project while Postgres was an implementation for one feature. It shouldn't be a problem to later add support for MariaDB, unless we combine the two specs. We can guard against this with a good Spec Evaluator prompt, but a human should be the ultimate arbitor of whether the end result belongs in their repo. `@<bot> revise` requests on the PR can handle revision rounds, or it could be refined in chat and then `send it`'d, but I suspect the form might become too long for chats.

## Visualization (exploratory feature)

Consider how we can use tools like Claude Design, Gemini and other models' image generation and vision capabilities. If a change or issue required diagnosing a problem like "the contrast seems too low", we might have a path for vision-enabled models to snapshot and view the problem item. If the change or `propose` called for interface designs, we might even generate versions to the user and let them choose and refine the interface in an iterative process. We could potentially use this for front end testing if the server installed a browser available to the autocoder. Discuss usefulness and feasability.

## Change-vs-canon contradiction pre-flight using RAG (future change)

`a19` ships the change-INTERNAL contradiction check. `a21` ships the RAG infrastructure. A future change would combine them to check changes against EXISTING canonical material — catching cases where a change's new ADDED requirements contradict canonical without explicitly modifying it.

The natural shape: for each ADDED requirement in the change, RAG-query the canonical corpus for top-K similar canonical requirements (excluding any the change explicitly MODIFIES or REMOVES). Hand the small bundle to an LLM with a "does this change's new requirement contradict any of these existing ones?" prompt. Findings flow through the existing `.needs-spec-revision.json` mechanism.

### Open design questions

1. **Scope of "v-canon"**: check the change's ADDED requirements against EVERY canonical capability, or scope to capabilities the change explicitly touches (the change's `specs/<cap>/spec.md` directory contents)? Narrower scope is cheaper AND lower false-positive rate; broader is more thorough. The retrieval step naturally narrows the search regardless of scope, but cost / noise tradeoffs differ.

2. **MODIFIED-as-resolution**: when the change MODIFIES a canonical requirement AND the new wording resolves what would otherwise be a contradiction, the check must recognize that and NOT flag. The detection needs to understand "this MODIFIED supersedes the canonical version we'd otherwise flag against." Probably implementable by: for each retrieved canonical chunk, check whether the change's MODIFIED block targets it by header. If yes, exclude from contradiction consideration (the change is updating it on purpose).

3. **LLM cost gating**: the check runs whenever a change has at least one ADDED requirement? Or only when a `canonical_rag.contradiction_check_enabled: true` flag is set (opt-in like `a19`'s internal check)? Cost per change is small (one LLM call, bounded input via retrieval) but non-zero.

4. **Interaction with RAG fail-open**: when `a21`'s store is unavailable (init failed, provider unreachable), the contradiction check has no canonical chunks to compare against. Fail-open (skip the check) OR fail-closed (block the change pending RAG availability)? Probably fail-open, matching `a14`'s posture.

5. **False-positive ergonomics**: the LLM may flag pairs that aren't really contradictions. The operator's recourse is `@<bot> clear-revision` without editing — same as `a19`. Worth noting that this check's higher abstraction makes false positives more likely than the structural checks; documentation should set expectations.

## Layer A RAG — full context injection (deferred)

`a21` exposes retrieval as a tool the implementer calls on demand. Layer A would proactively inject relevant canonical-spec chunks into the implementer's prompt at iteration start, before the implementer decides what to do. Larger surface area; bigger prompt budget impact; would need to interact with `a07`'s prompt-budget config.

Worth considering only if `a21`'s on-demand surface proves under-used (the implementer doesn't naturally call `query_canonical_specs` enough to surface relevant context). Hold for after `a21` ships and we see real usage patterns.

## install.sh + update.sh + docker-compose for Ollama as one-liner

The `a21` install wizard offers "install Ollama via docker" as option 1 but stops short of auto-running `docker compose up`. A more aggressive quick-start would: detect docker; offer to run the compose file as part of the install wizard; wait for Ollama to come up; pull the embedding model; verify the embed pipeline end-to-end. Trade-off: more wizard surface area, more failure modes. Worth doing only if operators report friction with the manual `docker compose up` step.

## Brightline-ignore extension to RAG-aware contradiction check

`a15` adds `.brightline-ignore` for intentional code duplication. The same concept could apply to the contradiction-pre-flight check above: a `.contradiction-ignore.yaml` lists requirement pairs the operator has reviewed AND confirmed are not actually contradictory. The check honors entries and stops flagging known-good pairs. Same architecture (file at workspace root, LLM-populated via `send it`, audit-time stale-pruning).

Defer until the contradiction-pre-flight ships and we see false-positive rates.

## Model attribution on reviewer / audit comments — AUTHORED as `a49-model-attribution-on-llm-output`

A redaction-safe accessor (positive allowlist: provider + model; never api_key/base_url) returns `<provider>/<model>` (provider is the `LlmProvider` KIND, e.g. `openai_compatible/moonshotai/kimi-latest`, not the upstream brand). Each daemon-composed operator-facing output carries `*<Role>: <provider>/<model>*` — `Reviewer` (initial + rerun), `Auditor (<type>)`, `Contradiction-check`. The accessor's redaction is testable behavior (not a content test). The **executor** implementation-notes are explicitly OUT of scope: the executor wraps the Claude CLI with no daemon-known model; its attribution defers to the model-registry work (fleet change 2).

## Auto-revise trigger trace + critical-evaluation prompt — BOTH RESOLVED

**(1) Trigger-path trace** → resolved by `a46-auto-revise-fires-on-actionable`: the trigger gated on `verdict == Block` (polling_loop.rs:5109), so it never fired for the common `Concerns` verdict; a46 moves the trigger to the per-concern `should_request_revision` + `actionable_request` signal regardless of verdict.

**(2) Critical-evaluation prompt** → authored as `a52-revision-agent-critical-evaluation`: the revision prompt instructs the agent to read the cited code, verify the request's claim, decline/partially-honor when wrong, and report the declination via `final_answer` (surfaced by a45). a52 also fixes the latent bug where a no-change declination false-reports as a commit/push failure — clean-tree `Completed` becomes a reported declination. Stacks on a45. Prompt guidance is drift-audited intent, not a content test (see [[test_behavior_not_message_content]]).

## Reviewer `mode: per_change` not honored on rerun path — AUTHORED as `a53-reviewer-mode-honored-on-rerun`

Spec-compliance bug: canon already requires the rerun path to honor `reviewer.mode` (the `Reviewer entry point is reusable…` requirement says the function "SHALL use the configured `reviewer.mode`"; the re-review requirement expects per-change output). Root cause: `review_pr_at_state_with` (code_reviewer.rs:418) calls the bundled entry point unconditionally, AND the `ReviewResult` contract omitted `per_change_sections` so the rerun composer (revisions.rs:1281) had nothing to render. a53 adds `per_change_sections` to the contract, dispatches per mode in the reusable function, and renders per-change sections in the rerun comment.

## Prompt-content tests are the wrong category — AUTHORED as `a48-tests-assert-behavior-not-prompt-content`

A test must assert what the code DOES (behavior) or that derived output matches its source (derivation); it must never read a real shipped prompt/message and assert a hand-authored substring of its prose. Coarse "tripwire" content checks (assert a URL/keyword is present) are the same anti-pattern, not an exception — that includes the `a41` OpenSpec-pointer regression test. Prompt design *intent* lives in requirement prose and is verified by the drift audit's semantic judgment, not a unit test.

a48 encodes this as a project-documentation requirement (`Tests assert behavior or derivation, never message wording` — the source of truth the drift audit enforces against), removes the `a41` requirement, softens the orchestrator-cli `Security & bug audit` confidence scenario and the code-reviewer scope scenario from verbatim → intent + sentinel-substitution, and deletes the offending tests (incl. the red `low_confidence_finding_filtering_explicit_in_prompt`). The broader sweep of other audits' wording scenarios is intentionally left to the drift audit (no hand-sweep). See [[test_behavior_not_message_content]].

## On-demand audit re-run after operator merges a fix — DROPPED (resolved by design)

Not needed. Post-`a43` (auditors send spec-only PRs), a merged spec PR triggers the executor to produce the code change, which advances HEAD; the audit's `requires_head_change` gate then clears naturally on the next cadence. The original concern (`last_run_sha` unchanged) only held under the old one-PR-with-code flow. Operators wanting immediate verification still have `@<bot> audit <type> <repo>`.

---

# Agentic fleet migration (planned spec stream)

A coordinated stream of changes that gives EVERY LLM-driven step — executor, reviewer, pre-checks, post-checks, audits — the same shape: a wrapped agent CLI running an agentic session in a read-only-capable sandbox, with structured output via per-role MCP tools, and a swappable CLI strategy so any provider's model (Anthropic, OpenRouter, Ollama) can drive any role. The purpose is to make larger/more complex LLM-built projects possible by keeping them on the rails with diverse, independent, well-controlled checks — model diversity is load-bearing (a different model reviewing than implementing catches blind-spots a single model's training assumptions miss).

Author the changes in dependency order. Each entry below is a candidate spec; lift it into `openspec/changes/<slug>/` when its turn comes. Keep this manifest in sync as changes graduate.

## Architecture umbrella (shared across the stream)

**The agentic-run primitive.** Wrap a CLI as a subprocess; hand it a prompt; let it run its own session to completion (it decides when done; the CLI owns its own context compaction — the executor already proves long multi-step sessions work). Shared sandbox tools for every role: `Read`, `Grep`, `Glob`, AND `query_canonical_specs` (the a21 semantic-search MCP tool, now fleet-wide). Structured output via a per-role `submit_*` MCP tool — NO stdout-JSON parsing anywhere (stdout-JSON is the fragility behind the Grok-refuses / Qwen-9B-confabulates behavior).

**CLI strategy (two jobs, one trait).** Each wrapped CLI has a strategy implementation that (1) builds the invocation — flags, sandbox/allowed-tools, MCP-config-file format — AND (2) translates the resolved model config into that CLI's model-selection mechanism. `claude` → `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` + `ANTHROPIC_MODEL` env, AND only speaks the Anthropic Messages wire format. `opencode` → its provider config file, speaks OpenAI-compatible + Ollama + many others natively. The Anthropic-wire constraint is exactly why a provider-agnostic CLI is required for non-Anthropic agentic runs — it is not optional given model diversity is the point.

**Model registry.** A top-level `models:` block defines `(provider, model, base_url, key)` once under a nickname; every role references a nickname (`model: beefy_security`) instead of duplicating provider/key. The registry entry's `provider` resolves the default CLI strategy for agentic runs (anthropic → claude; openai_compatible / ollama → opencode; overridable per model since opencode can also drive Anthropic). The operator thinks only in models; the CLI is resolved underneath. This removes any per-role `command:` field.

**`kind: agentic | oneshot`.** Agentic is the default for reasoning roles. `oneshot` (HTTP `LlmClient.complete()`, the a37 surface) is retained as (a) a fast/cheap opt-in, AND (b) the only path for non-Anthropic models during the claude-only window before the opencode strategy lands. RAG embedding stays one-shot PERMANENTLY — producing an embedding vector is a single forward pass, not a reasoning session; `agentic` is meaningless for it.

**Per-role submit tools.** `submit_review`, `submit_findings`, `submit_contradictions`, `submit_verdict` — per-role, not one generic tool (matches the executor's existing `outcome_*` family idiom; clearer schemas, model can't pick the wrong one). Each schema mirrors the structure that role's downstream consumer needs (e.g. `submit_review` carries verdict + per-concern entries with `actionable_request` + `should_request_revision`, per change, to preserve auto-revise + per_change).

## Change sequence

### 1a. Auto-revise trigger fix (a46 — authored; independent, near-term, live dormant bug)
`partition_and_annotate_reviewer_revisions` (polling_loop.rs:5109) returns empty unless `verdict == Block`, so auto-revise never fires for the common `Concerns` verdict — even when concerns carry `should_request_revision: true` + a valid `actionable_request`. Conservative reviewers rarely Block, so the feature is dormant. Fix: trigger on the actionable signal (`should_request_revision: true` + non-empty `actionable_request`) REGARDLESS of verdict; rename `reviewer.auto_revise_on_block` → `reviewer.auto_revise` (serde alias). Bounded by the EXISTING `executor.max_revisions_per_pr` cap (which caps all revisions) until 1b refines it, so no runaway in the gap. Authored as `a46-auto-revise-fires-on-actionable`.

### 1b. Caps reframing — automatic-only (a47 — sibling of a46)
Reframe caps so only AUTOMATIC chains are bounded: the revision cap counts ONLY reviewer-marked (`<!-- reviewer-revision -->`) revisions; human `@<bot> revise` is uncapped. Rename `executor.max_revisions_per_pr` → `executor.max_auto_revisions_per_pr` (alias). Uncap human `@<bot> code-review` entirely (all re-reviews are human — the "No reviewer re-run after revision lands" requirement guarantees no automatic re-review — so the re-review cap guards a deliberate human act with no runaway risk; default it to unlimited, keep the field as an opt-in ceiling). Touches ~5 reviewer-spec requirements + the orchestrator-cli revision-cap requirement; that breadth is why it's split from a46. The malformed-verdict-defaults-to-Approve fix (memory `reviewer-verdict-parse-failure-defaults-to-approve`) is NOT folded into a47: it resolves for free at change 5 (agentic reviewer) — once the reviewer submits via an MCP `submit_review` tool, a malformed verdict becomes a bad tool call, which that change specs to discard the review and alert the operator. No separate pre-fix needed.

### 2. Model registry — AUTHORED as `a55-model-registry`
Top-level `models:` registry; nickname references discriminated by presence of `provider` (block with `provider` = legacy inline, unchanged; block without = `model` is a nickname). Dual-acceptance forever. Defines the `provider → default CLI` rule (anthropic→claude; openai_compatible/ollama→opencode; per-entry `cli` override). orchestrator-cli config-schema requirement. Validated.

### 3. Extract the agentic-run primitive + CLI-strategy trait + submission MCP infra — AUTHORED as `a56-extract-agentic-run-primitive`
Collapses the five `run_subprocess` copies (executor + 4 audits) into one `agentic_run`; `CliStrategy` trait + claude impl (model selection via ANTHROPIC_* env, none when no model → CLI-default preserved); per-role submission MCP framework (relay via control-socket `record_submission`/`consume_submission`, role-scoped advertisement via `ORCH_MCP_ROLE`). Behavior-neutral extraction + additive surface; concrete `submit_*` tools land with their roles (4/5/6/8). executor + orchestrator-cli deltas. Validated.

**Numbering for the rest:** change 4 = a57, 5 = a58, 6 = a59, 7 = a60, 8 = a61/a62/a63. All AUTHORED + `--strict`-valid.

### 4. Migrate audits stdout-JSON → `submit_findings` MCP — AUTHORED as `a57-advisory-audits-submit-findings`
The three ADVISORY audits (drift, architecture_consultative, documentation_audit) move onto `submit_findings`; their orchestrator-cli requirements are MODIFIED (transport stdout-JSON → submit_findings; "malformed stdout fails" → "no valid submission fails", a schema-reject now an in-session correctable). The specs-writing audits (missing_tests, security_bug) stay no-MCP — they emit on-disk proposals, not findings, so they are explicitly OUT of a57's scope (the original "+ specs-writing audits" framing was dropped). Advisory audits switch to `agentic_run` WITH MCP (capture mode). Stacks on a56.

### 5. Agentic reviewer — AUTHORED as `a58-agentic-reviewer`
`reviewer.kind: oneshot | agentic` (default `oneshot`). Agentic: read-only sandbox `Read`/`Glob`/`Grep` (NO Bash — resolves the open question below), diff + briefs + file-list prompt, files read on demand (no 2M truncation, `prompt_budget_chars` N/A), `submit_review`. Preserves `reviewer.mode: per_change`, auto-revise, `@<bot> code-review`, caps. No-valid-submission → discard + alert (this is the agentic retirement of the malformed-verdict-defaults-to-approve bug). MODIFY `AI-driven code-quality review` (scoped to oneshot) + ADD `Agentic reviewer mode`. Default flip to agentic is deferred (Anthropic-only until a60) and left as a deliberate operator choice, NOT auto-flipped. Stacks on a55 + a56.

### 6. Agentic contradiction-check — AUTHORED as `a59-agentic-contradiction-check`
`change_internal_contradiction_check` off `LlmClient.complete()` onto `agentic_run` + `submit_contradictions`, read-only sandbox. Migrated wholesale (no `kind` selector) because the check is fail-open: a not-yet-registered strategy (non-Anthropic pre-a60) degrades to a logged no-op, never a break. Stacks on a55 + a56.

### 7. OpenCode CLI strategy — AUTHORED as `a60-opencode-cli-strategy`
`OpencodeStrategy` (second `CliStrategy`): `opencode run`, `--model provider/model`, `opencode.json` (mcp + provider), sandbox→opencode-permissions mapping, capture mode (streaming stays claude). Registering it unblocks the non-Anthropic agentic paths of a58/a59 (pure-ADD: a56/a58/a59 "no registered strategy errors" scenarios were generalized to be timeless so a60 needs no cascade MODIFY). Spike TASKS gate three headless-opencode unknowns (prompt delivery; MCP tool-call surfacing; correctable tool errors). Stacks on a55 + a56.

### 8. Verifier-gate framework + trio — AUTHORED as `a61` / `a62` / `a63` (split: three changes)
Decision (asked): **three changes**, and the `[out]` gate is **advisory** (annotates, never auto-acts). Lifecycle: `change ─[in]→ self-consistent? ─[canon]→ contradicts canon? ─► executor ─[out]→ code implements spec?`
- **a61 `verifier-gate-framework`** — thin reframe: names the three gates (`[in]`/`[canon]`/`[out]`), lifecycle positions, fail-open-vs-advisory posture, per-gate diagnostic labels; assigns `[in]` to the a59 check; `[canon]`/`[out]` inert until a62/a63. No config rename, no a59 reproduction. Stacks on a59.
- **a62 `change-vs-canonical-gate`** — `[canon]` pre-flight: opt-in, fail-open, agentic `submit_canon_contradictions`, marker+alert+halt on a real finding (mirrors a59's disposition). Canon access follows the docs-audit pattern: direct `Read` of `openspec/specs`, `query_canonical_specs` opportunistically when a21 RAG is on (NOT a hard RAG dependency). Stacks on a56 + a61.
- **a63 `code-implements-spec-gate`** — `[out]` post-executor verifier (the step the reviewer defers to): opt-in, ADVISORY, agentic `submit_verdict`, renders a `## Spec Verification` PR section + chatops note on gaps; never revises, never blocks. Stacks on a56 + a61.

### 9. Flip reviewer default to agentic — AUTHORED as `a64-reviewer-agentic-by-default`
Follow-on to change 5. `reviewer.kind` default `oneshot` → `agentic`, now safe because a60 makes agentic provider-agnostic. Upgrade-safe: when the effective kind is agentic but the resolved reviewer CLI is unavailable at startup (unregistered strategy OR missing binary), the reviewer falls back to `oneshot` HTTP for that boot with ONE loud WARN — review never disabled (every provider has a working HTTP client). Replaces a58's "unregistered strategy → error" scenario with graceful fallback (reviewer role only; the gate roles keep fail-open/advisory). MODIFY `Agentic reviewer mode`. Stacks on a58 + a60.

## Default prompts assume Rust/this-project tooling — AUTHORED as `a51-language-neutral-default-prompts`

Detect-and-run approach (the TODO's preferred "mix" default): a project-documentation requirement states default prompts name the project's own tooling (detected from the build config), not a specific toolchain; `openspec validate --strict` stays (shared). Drift-audited, no content test (a negative "no `cargo`" scanner is unenumerable AND the wording-assertion anti-pattern). Sweeps `prompts/`; concrete fixes in `implementer.md` (`cargo clippy` ×2) and `brownfield-draft.md` (`cargo test`). Per-repo tooling-config override deferred to a future change. SHOULD land after a45 to also clean a45's worked example.

## Open design questions — RESOLVED while authoring

- **Submit-tool relay vs in-process.** RESOLVED (a56): same control-socket relay as the outcome tools (`record_submission`/`consume_submission`), for uniformity AND daemon-side ownership of results.
- **Per-role sandbox toolsets.** RESOLVED: reviewer (a58), contradiction-checks (a59/a62), and the `[out]` gate (a63) are strictly read-only `Read`/`Glob`/`Grep` — NO Bash. Only the audits (drift/arch/docs) keep read-only Bash, per their canonical sandbox.
- **Registry migration ergonomics.** RESOLVED (a55): indefinite dual-acceptance — a block WITH `provider` is legacy inline (unchanged), a block WITHOUT is a nickname reference. No forced cutover.
- **opencode MCP maturity.** RESOLVED into spike TASKS (a60 §1): web-search spike confirmed `opencode run` + `opencode.json` mcp block are viable; the three load-bearing unknowns (prompt delivery stdin-vs-arg; MCP tool-call surfacing; correctable tool errors for the submit_* retry loop) are verified as implementation tasks before the strategy is wired, with an explicit STOP-and-report if unmet.
