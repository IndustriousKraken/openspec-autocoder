## Why

The PR comment posted after a successful `@<bot> revise <text>` carries only the success line AND the revision count:

```
✅ Revision applied: revise: a39-sigterm-aware-classifier: Please investigate the code reviewer's claim that backtick s. Revision count: 1 of 5.
```

The operator gets the confirmation AND nothing else. To understand what the revision agent actually did — which files it touched, whether it agreed with the original reviewer's claim, what tests it ran, whether any follow-up is recommended — the operator has to read the commit diff AND the code reviewer's section on the next iteration. That's friction at a moment where a brief summary would prevent the operator from going to the diff at all for routine revisions.

The information IS already produced. The revision agent runs through `executor.run_revision()` which produces `ExecutorOutcome::Completed { final_answer: Option<String> }`. The `final_answer` field carries the agent's `outcome_success`-passed summary text, captured the same way the implementer's `final_answer` is captured. The PR-comment composer at `autocoder/src/revisions.rs:990-993` currently destructures `Ok(ExecutorOutcome::Completed { .. })` AND discards the field. So the agent's summary is being thrown away at the comment-composition step.

Two fixes, paired because either alone is incomplete:

1. **Comment composition** — `revisions.rs` SHALL include the `final_answer` text (when present AND non-empty) in the success comment body, under the success line.
2. **Prompt guidance** — `prompts/implementer-revision.md` SHALL instruct the revision agent to call `outcome_success` with `final_answer` carrying a brief content-shaped summary. The current rewrite (from this session's earlier prompt-tightening pass) does NOT mention `outcome_success` at all, so the revision agent has no signal to produce substantive summary content. Without the prompt update, surfacing the field would frequently produce empty additions.

Related but explicitly out-of-scope for a45: the operator's TODO note about "critical-evaluation prompt for the revising agent" (TODO.md, "Auto-revise trigger trace + critical-evaluation prompt"). That would extend the revision-agent prompt to actively push back on reviewer requests that would damage the codebase. a45's prompt addition is a foundation that change could build on, but a45 itself stays narrow on the summary surfacing.

## What Changes

**Revision success-comment body gains an optional summary section.** When the `Completed` outcome's `final_answer` is `Some(text)` AND `text.trim()` is non-empty, the composed PR comment body SHALL be:

```
✅ Revision applied: <subject>. Revision count: <n> of <cap>.

<final_answer text>
```

The success line stays at the top (operators scanning for the ✓ confirmation see it immediately). A blank line separates it from the agent's summary. The summary is the agent's verbatim text; no transformation, no re-wrapping, no truncation beyond the GitHub comment-size cap (which the existing `truncate_to_fit` helper already enforces for implementer notes — the revision composer SHALL apply the same helper).

When `final_answer` is `None` (legacy text mode OR no outcome tool was called) OR is `Some("")` (empty after trim), the comment body remains the current single-line form. This preserves backward compatibility AND avoids posting a comment with an awkward trailing blank section.

**Revision prompt gains outcome-tool content guidance.** `prompts/implementer-revision.md` SHALL include a new section near the bottom (before the `--- BEGIN ... ---` template markers) that:

- Names `outcome_success` AND its `final_answer` argument.
- Instructs the agent to pass a brief summary (5-10 lines — half the implementer's 10-20 since revisions are smaller scope).
- Lists the content categories: what the reviewer asked for, what the agent changed (modules / functions), whether the agent agreed with the reviewer's claim OR concluded the request was off-base (explicit signal that "I declined the request because X" is a valid outcome), test counts AND results.
- Provides a 5-10 line worked example.

The "explicit signal that declining is valid" wording is the seed that the TODO's critical-evaluation prompt can later expand into. a45 does NOT add behavioral guidance about HOW to evaluate the reviewer's claim — just that declining is reportable, NOT a failure.

**Bundle with a44.** a44's MCP outcome-tool description rewrite affects every consumer of `outcome_success`, including the revision agent. a44 + a45 together produce the full picture: the tool description encourages substantive content, the implementer-revision prompt directs the agent to use that surface, AND the comment composer surfaces the result. None of the three is order-dependent; they can land in any sequence.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — MODIFIED the canonical "Revision execution updates the agent branch and posts a reply comment" requirement to update its `Completed` scenario for the new comment shape; the `AskUser` AND `Failed` scenarios are preserved verbatim.
  - `project-documentation` — ADDED a new requirement defining the content shape of `prompts/implementer-revision.md`'s outcome-tool section. A regression test asserts the required markers are present.
- **Affected code:**
  - `autocoder/src/revisions.rs` — the success-path block (around line 990) destructures `final_answer` from `Completed` AND appends it to the reply body when non-empty. Truncation via the existing `truncate_to_fit` helper (currently in `polling_loop.rs`, may need a `pub` accessor OR a small move).
  - `prompts/implementer-revision.md` — new section per the spec.
  - Unit test for the comment composition: success comment WITH final_answer includes the summary; success comment WITHOUT final_answer is unchanged; truncation kicks in at the GitHub limit.
  - Unit test for prompt content: required substrings present.
- **Operator-visible behavior:**
  - Revision success PR comments include a substantive summary from the agent. Operators reading the PR after a `@<bot> revise <text>` see what was done AND why, without opening the diff.
  - Revision comments where the agent's `final_answer` is empty look exactly as today — the change is purely additive.
- **Backward compatibility:** none affected. Existing tests for the success-comment shape may need updating to match the new body when `final_answer` is non-empty in their fixtures; the test changes are narrow.
- **Dependencies:** none HARD. Soft synergy with a44 (MCP tool descriptions); a44 lands first → revision agent produces substantive `final_answer` more reliably → a45's surfacing is more useful. a45 can land before a44 without breakage; the comment composition gracefully handles the empty-final_answer case.
- **Acceptance:** `cargo test` passes; `openspec validate a45-revision-summary-surfaces-in-pr-comment --strict` passes. Tests:
  - Comment composition: `Completed { final_answer: Some("Did X, declined Y because Z.") }` produces a comment whose body contains BOTH the `✅ Revision applied:` line AND the summary text, separated by a blank line.
  - Comment composition: `Completed { final_answer: None }` produces the current single-line body.
  - Comment composition: `Completed { final_answer: Some("   ") }` (whitespace-only) is treated as empty AND produces the current single-line body.
  - Comment composition: a `final_answer` longer than the GitHub comment limit is truncated via `truncate_to_fit` with the existing truncation marker.
  - Prompt content: `prompts/implementer-revision.md` contains the substrings `outcome_success`, `final_answer`, `declined`, AND `Test counts` (or equivalent markers from the section).
