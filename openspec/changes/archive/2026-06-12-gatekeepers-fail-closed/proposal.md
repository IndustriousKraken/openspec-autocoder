## Why

autocoder is a framework of controls that keep LLM-built work on the rails. A control that fails *open* — that treats "I could not run" as "everything's fine" — is not a control; it silently removes the rail while reporting green. This has bitten us repeatedly, always the same shape (an inability-to-run collapsing into a passing verdict):

- the reviewer defaulting to **Approve** on a parse failure,
- a zero-item per-change synthesis defaulting to **Approve** (the empty-session bug),
- the `[in]`/`[canon]` pre-flight gates **failing open** ("treat as no contradictions found") on any session error,
- most recently, an opencode reviewer and opencode gates whose `submit_*` never routed (an unfinished MCP seam) — the reviewer surfaced it (it discards), but the **gates silently fail-open'd as "no findings"**, so a misconfigured or non-functioning gate looked exactly like a passing one.

There is no current spec forbidding this, so nothing catches it. We want a project-wide standard that the drift audit AND the `[canon]`/`[out]` gates can flag.

## What Changes

Add a project-wide engineering standard: **control-plane gatekeepers fail closed — an inability to run is a distinct, surfaced, non-passing state, never a pass.** It governs every gatekeeper (the `[in]`/`[canon]`/`[out]` gates, the code reviewer, future verifiers, AND audits that gate `send it`) and pins the specific traps: non-passing verdict defaults/initializers, zero-item aggregations, error/timeout/unavailable-CLI paths, and parse/no-result paths. The action on error follows the gatekeeper's role — a **blocking** gatekeeper holds the work in an explicit failed-to-run state (an operator clears it; distinct from a "found a problem" verdict); an **advisory** gatekeeper renders an explicit "failed to run" result rather than omit its output or claim success — but none is silent-pass. Transient tolerance is **bounded retry, then errored**, not fail-open.

## Impact

- **Affected specs:** `project-documentation` — ADD `Control-plane gatekeepers fail closed, never to a passing verdict`.
- **Conformance status (named, not silently assumed):**
  - The code reviewer **conforms** (it discards a review with no valid submission; it does not default Approve).
  - The `[in]`/`[canon]` gates **violate** it today (orchestrator-cli mandates fail-open) — they will be brought into conformance by a follow-on change that reverses the fail-open posture to the explicit held state (MODIFY `Change-internal contradiction pre-flight check`, `Change-vs-canonical contradiction pre-flight check`, AND the verifier-gate framework). The `[out]` advisory gate likewise moves from "omit on error" to "render FAILED TO RUN".
  - Establishing the standard first is deliberate: it makes the current fail-open gates **detectable drift** in the interim, which is the point.
- **Enforcement:** the standard is a canonical requirement, so the periodic `drift_audit` AND the `[canon]` gate read it AND can flag a gatekeeper that defaults to pass. A developer-facing note records it for humans.
- **Non-goals:** this change does NOT itself reverse the gates' runtime behavior (that is the follow-on conformance change, scoped separately because it adds a held-state marker, chatops surfacing, and an operator-clear path). It establishes the invariant AND its detectability.
- **Acceptance:** `openspec validate gatekeepers-fail-closed --strict` passes; the standard's scenarios pin the known traps (cannot-run ≠ pass; blocking holds; advisory reports failed-to-run; defaults are non-passing).
