## Why

In `reviewer.mode: per_change`, the reviewer splits the review context into one sub-context per archived change (`split_per_change_contexts`, over `ctx.archived_changes`) and reviews each. When that split yields **zero** sub-contexts — because no archived-change briefs resolved for the PR — the consequences are silent and wrong:

- `review_per_change(&[])` makes **zero** reviewer invocations, so no prompt is built and no LLM call happens (no `reviewer prompt built`);
- `synthesize_per_change_report(vec![])` returns its initializer verbatim: `verdict = Pass`, empty `markdown`, no sections;
- the caller posts a **blank `Approve`** — an approval the reviewer never actually performed.

This reproduces deterministically when a PR is created under one daemon build and re-reviewed under another (e.g. a PR opened by a newer build, then `@<bot> code-review` after rolling back): the re-review cannot resolve the change briefs against the current workspace, `archived_changes` comes back empty, and the PR is silently approved with no review. It was observed on two repositories. A PR created and reviewed under the same build resolves its briefs and reviews normally, which is why the failure looked intermittent. The verdict-from-nothing default is the same anti-pattern as a parse failure defaulting to approve: an empty result must never become a pass.

## What Changes

`per_change` mode SHALL **fall back to a single bundled review of the whole context** whenever the per-change split yields zero sub-contexts, instead of synthesizing a verdict from zero reviews. This guarantees every emitted verdict is derived from at least one actual reviewer invocation: a PR whose change set fails to resolve is reviewed in bundled form (its diff and changed files still reach the reviewer) rather than rubber-stamped. An empty per-change synthesis is never treated as `Pass`/`Approve`.

The populated per_change path is unchanged — one reviewer call per change, synthesized as today.

## Impact

- **Affected specs:** `code-reviewer` — ADDED `Per-change review falls back to bundled when the change set is empty`.
- **Affected code:** `code_reviewer.rs` — `review_pr_at_state_with`'s `PerChange` arm: when `split_per_change_contexts(ctx)` is empty, dispatch the bundled path (`reviewer.review(ctx)`) instead of `review_per_change(&[])` → `synthesize_per_change_report(vec![])`. `synthesize_per_change_report` gains an empty-input guard so it can never be the source of a defaulted `Pass`.
- **Operator-visible behavior:** a `per_change` PR whose change set does not resolve is reviewed (bundled) and gets a real verdict, instead of a blank `Approve`; the reviewer prompt is built (one invocation) where previously none was. No change for PRs whose changes resolve.
- **Dependencies:** none — `review_pr_at_state_with`, `split_per_change_contexts`, `synthesize_per_change_report`, and the bundled `reviewer.review` path are all canonical. Standalone.
- **Acceptance:** `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean; `openspec validate a015-reviewer-per-change-bundled-fallback --strict` passes. Tests: an empty `archived_changes` context with a non-empty diff is reviewed bundled (one reviewer invocation, verdict from that review, not a defaulted Pass); a populated `archived_changes` context still dispatches per change with no fallback.
