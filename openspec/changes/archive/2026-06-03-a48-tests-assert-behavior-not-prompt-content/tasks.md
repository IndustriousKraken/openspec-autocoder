# Implementation tasks

## 1. Add the healthy-test-form requirement (project-documentation)

- [x] 1.1 No code artifact is required for the requirement itself; it is a convention the drift audit reads. Ensure `docs/test-reliability.md` cross-references it: add a short subsection stating that tests assert behavior or derivation, never prompt/message wording, and that behavior tests use synthetic fixtures (link the spec requirement by name). Keep it dry; no kitsch.

## 2. Delete pure wording-assertion tests

- [x] 2.1 In `autocoder/src/audits/security_bug.rs`, DELETE the test `low_confidence_finding_filtering_explicit_in_prompt` (security_bug.rs:335) in full. This is the test that is currently RED on the merged `dev` branch (the prompt reword in 79aabd3 broke its verbatim assertions); deleting it clears the red `cargo test` gate. Do NOT replace it with a token-check variant.
- [x] 2.2 In `autocoder/src/code_reviewer.rs`, DELETE the wording assertions that read `DEFAULT_TEMPLATE` and check for hand-authored prose: the group asserting `revision-requests` / `should_request_revision` / `actionable_request` / `most-critical-first`, AND the group asserting `"You are reviewing code quality only. Do NOT assess whether the diff implements the spec; that is handled separately by the verifier step."` and `"VERDICT:"`. If these live inside a test whose remaining assertions are also wording checks, delete the whole test; if the test mixes wording checks with behavior checks, delete only the wording lines and keep the behavior ones.
- [x] 2.3 DELETE the file `autocoder/tests/openspec_pointers.rs` in full. It holds BOTH the `a41` OpenSpec-pointer regression test (nine files contain `https://github.com/Fission-AI/OpenSpec` + a topical hint) AND the `a45` implementer-revision marker test (asserts `prompts/implementer-revision.md` contains `outcome_success` / `final_answer` / `declined` / `Test counts`). Both are prompt-wording content tests. Leave the OpenSpec links in the prompts/`docs/README.md` AND the outcome-signal section of `prompts/implementer-revision.md` unchanged — they are content, not under test.
- [x] 2.4 In `autocoder/src/mcp_askuser_server.rs`, DELETE the `a44` content test `outcome_descriptions_satisfy_marker_rules` (mcp_askuser_server.rs:1747) AND its helper `collect_description_marker_violations` (~1712) AND the required/forbidden substring lists (~1693). Replace with ONE structural behavior test: request/construct the `tools/list` response AND assert each of `outcome_success`, `outcome_request_iteration`, `outcome_spec_needs_revision` is advertised with a NON-EMPTY `description` — no substring-of-wording assertions.

## 3. Refactor behavior-relevant checks onto synthetic fixtures / identity

- [x] 3.1 In `autocoder/src/code_reviewer.rs` (~the loader-override test near line 1228), the assertion that the loaded template does NOT contain the default's scope sentence couples to the default's wording. Refactor it to assert template identity instead: the loaded value equals the synthetic custom template the test wrote, AND is not equal to `DEFAULT_TEMPLATE` (symbol comparison, no prose substring).
- [x] 3.2 ADD a behavior test that verifies the shipped `prompts/code-review-default.md` references all three placeholders: render `DEFAULT_TEMPLATE` with a distinct sentinel value per placeholder (`{{change_context}}`, `{{changed_files}}`, `{{diff}}`) and assert each sentinel appears in the rendered output. Assert the substituted sentinel values only — do NOT assert any instruction wording of the template. This replaces the deleted placeholder-presence wording assertions.
- [x] 3.3 Scan `autocoder/src/code_reviewer.rs` and `autocoder/src/audits/*.rs` for any other test that reads a real embedded prompt (`include_str!` constants, the prompt loader pointed at a default) and asserts a hand-authored substring of its prose. For each, delete it (pure wording) or refactor it to a synthetic fixture / sentinel-substitution (behavior-relevant). Tests that assert synthetic-fixture output or symbol identity are already healthy and are left unchanged.

## 4. Spec deltas

- [x] 4.1 `specs/project-documentation/spec.md` — ADD `Tests assert behavior or derivation, never message wording`; REMOVE `OpenSpec upstream-docs pointer is regression-tested across the spec-drafting prompt set AND `docs/README.md`` (a41); REMOVE `` `prompts/implementer-revision.md` instructs the revision agent on `outcome_success` AND `final_answer` content `` (a45).
- [x] 4.2 `specs/code-reviewer/spec.md` — MODIFY `Default prompt template enforces code-quality scope` per this change's delta.
- [x] 4.3 `specs/orchestrator-cli/spec.md` — MODIFY `Security & bug audit` per this change's delta (scenario `Prompt instructs confidence-filtered output` softened to intent; all other scenarios preserved verbatim).
- [x] 4.4 `specs/executor/spec.md` — MODIFY `MCP outcome-tool description fields encourage substantive content AND drop narrative history` (a44) to the intent version: drop the required/forbidden-substring contract AND the substring regression test; keep the operational-guidance intent (drift-audited) plus a structural "descriptions are served, non-empty" scenario.

## 5. Acceptance gate

- [x] 5.1 `cargo test` passes for the autocoder crate, including the new sentinel-substitution test (3.2). The previously-red `low_confidence_finding_filtering_explicit_in_prompt` is gone, not skipped.
- [x] 5.2 `cargo clippy --all-targets -- -D warnings` is clean for any test files touched.
- [x] 5.3 `openspec validate a48-tests-assert-behavior-not-prompt-content --strict` passes.
- [x] 5.4 Confirm no token-check replacement tests were introduced for any deleted wording assertion (grep the touched test modules for new `.contains("…")` on embedded-prompt constants; there should be none beyond sentinel-substitution).
