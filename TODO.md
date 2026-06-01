# TODO

Design questions and future-work items that aren't yet ready for an OpenSpec change proposal. Each section is a candidate spec; when the design solidifies enough to draft, lift the section into `openspec/changes/<slug>/proposal.md`.

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

## Model attribution on reviewer / executor / audit comments

Operator-facing comments (code review, executor implementation notes, audit findings, contradiction-check findings) don't currently identify which model produced them. With multiple LLM providers/models configurable across these surfaces — AND with operators experimenting across reviewer tiers — the lack of attribution makes it hard to associate a comment's quality with the model that produced it.

The fix is small. A redaction-safe accessor on the resolved config (the same primitive that gives selective config access without leaking API keys) returns `(provider, model)` for each LLM-driven surface. Each comment composer prepends or appends a one-line attribution: `*Reviewer: openrouter/moonshotai/kimi-latest*` (or `*Executor: ...*`, `*Auditor: ...*`). Render points: `revisions.rs:~1250` (rerun reviews), the initial-review PR-body builder, the executor implementation-notes section, each audit's chatops + PR comment formatter.

Scope considerations: identifier format should be stable across providers (e.g. `<provider>/<model>` rather than each provider's native naming). The accessor MUST refuse to return anything that could be an API key, base URL, or other secret-bearing field — explicit allowlist of safe fields, not a denylist.

Worth doing soon — it cleanly closes the "which model produced this?" gap that operators (and Claude itself, helping operators debug) currently bridge by memory.

## Auto-revise trigger trace + critical-evaluation prompt for the revising agent

Two related concerns about the auto-revise pipeline that surfaced during the multi-reviewer PR #79 trial:

**(1) Trigger-path trace.** Confirm which combination of `Verdict` (Approve | Block) + `should_request_revision: true` actually fires auto-revise, AND whether the operator-triggered rerun path (`@<bot> code-review`) participates or only the initial-review path does. Observed: owl-alpha's rerun review had two `should_request_revision: true` items (one impossible to action, one targeting a fabricated test name) — neither triggered an auto-revise commit. That's accidental safety; the underlying logic may or may not currently gate against this case correctly.

**(2) Critical-evaluation prompt.** When auto-revise DOES fire, the prompt handed off to the revising agent should explicitly instruct it to evaluate the original reviewer's request critically — not assume the previous reviewer is correct about the need. Concrete reviewers tested (owl-alpha, MiMo) have both produced `should_request_revision: true` items that would actively damage the codebase if applied: removing a spec-traced test the reviewer mistakenly believed was redundant; churning working idiomatic code (`.tmp` extension → `NamedTempFile`) for protection that doesn't apply. The implementing agent should: (a) read the actual code at the cited location; (b) verify the reviewer's claim against current state; (c) reject the revision when the claim is wrong; (d) post a chatops comment naming what it rejected and why, so the operator sees the trail. Models with strong instruction-following (Claude, Opus) will do this naturally if asked; cheaper executors may need the rejection mechanic spelled out explicitly.

Worth scoping together because (2)'s prompt is only load-bearing if (1) confirms the trigger fires on these inputs.

## Reviewer `mode: per_change` not honored on rerun path

When `reviewer.mode: per_change` is set in config, the expected output is one `## Code Review: <slug>` section per change. Observed on PR #79 reruns (owl-alpha, laguna-m.1): both produced a single bundled `## Code Review` block, suggesting the rerun path forces `ReviewerMode::Bundled` regardless of config — OR the config-to-reviewer mode threading uses the default rather than reading from `ReviewerConfig`. Infrastructure exists (`PerChangeSection`, `with_mode(ReviewerMode::PerChange)`, the `per_change_sections: Vec<...>` field in `ReviewReport`); investigation needs to trace the operator-trigger code path (`@<bot> code-review` → `review_pr_at_state` in `revisions.rs`) and confirm where the mode is or isn't propagated.

Worth fixing because per-change review is materially more useful than bundled when a PR carries multiple unrelated changes — operators want to see "change a35 is approved; change a36 has concerns" not one combined verdict that hides per-change differences.

## On-demand audit re-run after operator merges a fix

When an audit fires (drift, brightline, etc.) and the operator addresses the findings via `send it`, the audit's `last_run_sha` is unchanged — the audit only re-runs when HEAD changes. The next audit fire could be days later. An operator who fixes findings and wants to verify the fix worked has to wait for the next cadence OR explicitly re-trigger via `@<bot> audit <type> <repo>`.

Could be improved: when `send it` produces a PR that merges, automatically re-queue the audit that triggered the `send it`. Closes the loop without operator action. Small spec.
