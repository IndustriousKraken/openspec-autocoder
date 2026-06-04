## Why

A single cap, `executor.max_revisions_per_pr` (default `5`), currently bounds ALL revisions per PR — both operator-initiated `@<bot> revise` requests AND automatic reviewer-marked revisions (the `<!-- reviewer-revision -->` comments the code-reviewer auto-revise path posts). Conflating the two is wrong for both:

- **Automatic chains are the runaway risk.** Now that a46 makes auto-revise actually fire on actionable concerns, a reviewer can drive a chain of automatic revisions. That chain is exactly what a cap should bound.
- **Human requests are deliberate and should not be capped.** An operator typing `@<bot> revise <text>` is making a considered request. Hitting "🛑 Revision cap reached" on a human request — because earlier automatic revisions consumed the budget — is a poor experience and bounds the wrong thing.

Likewise, the re-review cap (`reviewer.max_code_reviews_per_pr`, default `5`) bounds operator-initiated `@<bot> code-review`. But every re-review is a deliberate human action — there is no automatic re-review path (the canonical "No reviewer re-run after a reviewer-initiated revision lands" requirement guarantees it) — so there is no runaway to bound. A default cap of 5 just blocks a human from re-reviewing a seventh time for no safety reason.

This change reframes both caps so only AUTOMATIC chains are bounded; deliberate human actions are uncapped (with the re-review cap retained as an opt-in ceiling).

## What Changes

**The revision cap counts only automatic revisions (orchestrator-cli + code-reviewer).** `executor.max_revisions_per_pr` is renamed to `executor.max_auto_revisions_per_pr` (serde alias keeps the old key working). The cap counts ONLY reviewer-marked (`<!-- reviewer-revision -->`) revisions; the per-PR state tracks an automatic-revision counter distinct from human revisions. Human `@<bot> revise` comments are never counted against the cap AND are never declined for cap reasons — an operator's revision request always processes. The reviewer-posting cap-budget interaction (which already bounds reviewer-revision posts) is updated to reference the renamed auto cap.

**Re-reviews are uncapped by default (code-reviewer).** `reviewer.max_code_reviews_per_pr` defaults to UNLIMITED. When unset, `@<bot> code-review` always processes. When the operator sets a positive integer (ceiling `20`, WARN-and-clamp), it acts as an opt-in ceiling with the existing one-time-decline behavior. The cap remains independent of the auto-revision cap.

**The malformed-verdict-defaults-to-Approve fix is explicitly NOT in scope.** It resolves at the agentic-reviewer migration (the planned `submit_review` MCP tool turns a malformed verdict into a bad tool call that discards the review and alerts the operator), so no separate pre-fix is warranted here.

**Stacks on a46.** a46 renamed `reviewer.auto_revise_on_block` → `reviewer.auto_revise` and moved the auto-revise trigger to the per-concern actionable signal. a47 builds on that: the revisions the cap now counts are exactly the reviewer-marked ones a46 posts. a47 does not re-modify a46's requirements; it modifies the cap requirements, which a46 left untouched. a46 lands first.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — MODIFIED `Revision cap per PR, with one-time decline`: cap counts only automatic revisions; human `@<bot> revise` uncapped; field renamed with alias.
  - `code-reviewer` — MODIFIED `Cap-budget interaction with reviewer-posted comments` (reference the renamed auto cap). MODIFIED `Re-review cap (`reviewer.max_code_reviews_per_pr`) is independent of revision cap` (default unlimited; opt-in ceiling).
- **Affected code:**
  - `autocoder/src/config.rs` — rename `ExecutorConfig::max_revisions_per_pr` → `max_auto_revisions_per_pr` with `#[serde(alias = "max_revisions_per_pr")]`; change `ReviewerConfig::max_code_reviews_per_pr` to `Option<u32>` (default `None` = unlimited) OR a sentinel meaning unlimited; keep the `20` clamp when a value is set.
  - `autocoder/src/revisions.rs` — the revision dispatcher distinguishes reviewer-marked (`<!-- reviewer-revision -->`) triggers from human `@<bot> revise` triggers; only the former increment the automatic-revision counter and are subject to the cap/decline. Human triggers always process. The re-review path checks the cap only when it is set.
  - Per-PR state file — an automatic-revision counter distinct from human revisions (rename `revisions_applied` → `auto_revisions_applied`, OR add a field; pick lower churn). The re-review counter is unchanged in shape; only its cap-check becomes conditional.
  - `config.example.yaml` — rename the example field with an alias note; document `max_code_reviews_per_pr` default-unlimited semantics.
- **Operator-visible behavior:** automatic revision chains are bounded by `max_auto_revisions_per_pr`; human `@<bot> revise` and `@<bot> code-review` are uncapped by default. Existing configs using `max_revisions_per_pr` load unchanged via the alias (now bounding automatic revisions specifically).
- **Acceptance:** `cargo test` passes; `openspec validate a47-auto-only-revision-caps --strict` passes. Tests: a reviewer-marked revision increments the auto counter and is capped; a human `@<bot> revise` at/over the auto cap still processes and does not increment the auto counter; legacy `max_revisions_per_pr` loads via alias; `max_code_reviews_per_pr` unset → unlimited re-reviews (no decline); set → opt-in ceiling with decline.
- **Dependencies:** stacks on **a46** (auto-revise trigger + flag rename). Independent of a44/a45/a48/a49/a51/a52/a53.
