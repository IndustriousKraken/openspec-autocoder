## Why

The reviewer's auto-revise feature has never fired in production despite being enabled in config. Root cause is a single gate in `partition_and_annotate_reviewer_revisions` (`autocoder/src/polling_loop.rs:5109`):

```rust
if report.verdict != ReviewVerdict::Block {
    return Vec::new();
}
```

Auto-revise posts its `<!-- reviewer-revision -->` comments ONLY when the verdict is `Block`. But the actionability signal lives at a different granularity: each *concern* carries `should_request_revision: true` + an `actionable_request`. A careful reviewer reserves `Block` for "real harm if merged" (per the reviewer prompt) and emits `Concerns` for most actionable findings. The two signals are ANDed — `verdict == Block` AND `should_request_revision == true` — so a conservative reviewer that rarely Blocks leaves the feature dormant: actionable revision requests under a `Concerns` verdict go nowhere.

Observed directly: PR #81's a40 review returned `VERDICT: Concerns` with a `should_request_revision: true` item (a real backtick-uniformity bug). That actionable item was silently dropped because the verdict was Concerns, not Block. This also explains the earlier TODO observation that owl-alpha's reruns produced `should_request_revision: true` items that "never triggered auto-revise" — it was the Block gate, not chance.

The fix is to make the trigger the *actionable* signal, not the *verdict* signal: auto-revise fires for any concern with `should_request_revision: true` + a non-empty `actionable_request`, regardless of whether the verdict is Pass, Concerns, or Block. The `Block` verdict retains its other meaning (PR marked draft); it just stops gating auto-revise.

The config flag `reviewer.auto_revise_on_block` is renamed to `reviewer.auto_revise` to match the new, verdict-independent semantics; a serde alias keeps existing config files loading unchanged.

This change is scoped narrowly to the **trigger**. It does NOT touch the cap model: auto-revise remains bounded by the existing `executor.max_revisions_per_pr` cap (which today caps all revisions), so there is no runaway risk introduced here. Reframing caps to bound only automatic chains while uncapping human-initiated revisions/re-reviews is a separate sibling change (`a47`); the existing cap protects the gap between the two.

## What Changes

**Auto-revise trigger decouples from the `Block` verdict.** The canonical requirement "Reviewer-initiated revision comments on Block verdicts" is REMOVED and replaced (via an ADDED requirement) with "Reviewer-initiated revision comments on actionable concerns." The new requirement fires reviewer-revision comments for every concern with `should_request_revision: true` AND a non-empty `actionable_request`, under ANY verdict. The off-by-default posture is preserved (the feature requires `reviewer.auto_revise: true`). The marker-line / trigger-pattern body shape, the self-author-filter bypass, and the cap-budget interaction are all unchanged.

**The `partition_and_annotate_reviewer_revisions` gate changes** from "return empty unless verdict == Block" to "return empty unless at least one concern has `should_request_revision: true` + non-empty `actionable_request`." The verdict is no longer consulted by this function. The existing WARN ("auto-revise enabled but no actionable concerns") still fires when the filter yields nothing.

**Config field rename with backward-compat alias.** `reviewer.auto_revise_on_block` → `reviewer.auto_revise`. A serde alias on the field accepts the old name verbatim, so existing config files load with no change AND no warning. New documentation uses the new name.

**The "Backwards compatibility for unaware reviewer templates" requirement is MODIFIED** only to reference the renamed flag (`reviewer.auto_revise`); its behavior and scenario are otherwise preserved verbatim.

**Out of scope (sibling `a47`):** automatic-only cap semantics, uncapping human `@<bot> revise` and `@<bot> code-review`, the `max_revisions_per_pr` → `max_auto_revisions_per_pr` rename, and the compounding malformed-verdict-defaults-to-Approve fix. a46 leaves the cap model exactly as it is today.

## Impact

- **Affected specs:**
  - `code-reviewer` — REMOVED the requirement "Reviewer-initiated revision comments on Block verdicts". ADDED the requirement "Reviewer-initiated revision comments on actionable concerns" (verdict-independent trigger). MODIFIED "Backwards compatibility for unaware reviewer templates" to reference `reviewer.auto_revise`.
- **Affected code:**
  - `autocoder/src/polling_loop.rs::partition_and_annotate_reviewer_revisions` (~line 5105) — replace the `verdict != Block` early-return with an "any actionable concern?" check; stop consulting the verdict.
  - `autocoder/src/config.rs` — rename `ReviewerConfig::auto_revise_on_block` → `auto_revise` with `#[serde(alias = "auto_revise_on_block")]`; update the accessor (`auto_revise_on_block()` → `auto_revise()`, or keep the accessor name with a deprecation note — pick the lower-churn path).
  - Call sites of the accessor (`polling_loop.rs`, `revisions.rs`) updated to the new name.
  - Tests: the existing auto-revise tests that assert "Pass/Concerns post nothing" are inverted to assert "actionable concerns post comments under Concerns"; a new test asserts a `Concerns` verdict with one `should_request_revision: true` concern posts one reviewer-revision comment; the "no actionable concerns → WARN + empty" test is preserved.
- **Operator-visible behavior:**
  - Auto-revise actually fires: a `Concerns` (or `Pass`, or `Block`) verdict carrying actionable concerns now posts `<!-- reviewer-revision -->` comments that the dispatcher acts on, bounded by the existing revision cap.
  - Existing `auto_revise_on_block: true` config keeps working unchanged (alias); operators may migrate to `auto_revise: true` at leisure.
  - PRs whose reviews carry no actionable concerns are unaffected (no comments, same WARN as before when the flag is on but nothing is actionable).
- **Backward compatibility:** config files using `auto_revise_on_block` load identically via the serde alias. The only behavior change is the intended one — auto-revise now fires on actionable concerns rather than only on Block. Operators relying on "auto-revise only on Block" (unlikely; the feature was dormant) would see it fire more often; this is the fix, not a regression.
- **Dependencies:** none hard. Sibling `a47` (caps reframing) builds on this but is independent; a46 ships safely alone under the existing cap.
- **Acceptance:** `cargo test` passes; `openspec validate a46-auto-revise-fires-on-actionable --strict` passes. Tests:
  - `partition_and_annotate_reviewer_revisions` with a `Concerns` verdict + one `should_request_revision: true` concern (non-empty `actionable_request`) returns that concern (no longer empty).
  - Same function with a `Block` verdict + actionable concerns still returns them (Block path preserved).
  - Same function with any verdict + zero actionable concerns returns empty AND logs the existing WARN.
  - A `Pass` verdict + one actionable concern returns the concern (verdict no longer gates).
  - Config: `reviewer.auto_revise_on_block: true` loads as `auto_revise == true` via the alias; `reviewer.auto_revise: true` loads identically.
  - End-to-end: an initial review returning `Concerns` with an actionable concern posts exactly one `<!-- reviewer-revision -->` comment whose body is `<!-- reviewer-revision -->\n@<bot> revise <actionable_request>`.
