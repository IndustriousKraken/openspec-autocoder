# project-documentation — delta for a48-tests-assert-behavior-not-prompt-content

## ADDED Requirements

### Requirement: Tests assert behavior or derivation, never message wording
The test suite SHALL assert what the code DOES (behavior) or that mechanically-derived output matches its source of truth (derivation). A test SHALL NOT read a real shipped prompt, message, or other human-authored content artifact and assert a hand-authored substring of its prose.

Design intent about a prompt's content — for example "the security-bug audit prompt steers the agent toward high-confidence findings" — SHALL be captured as requirement prose and verified by the drift audit's semantic judgment, NOT by a unit test that pins verbatim wording. A unit test that reads a real embedded prompt (via `include_str!`, a named default-prompt constant, or the prompt loader resolving to a default) and asserts a hand-authored sentence or phrase is present is prohibited: it encodes no independent truth, breaks on meaning-preserving rewrites, and catches nothing code review and the drift audit do not.

Behavior tests that exercise prompt- or message-handling code SHALL supply their own synthetic fixture (a template or input the test defines) and assert on the transformed output; they SHALL NOT depend on the content of the real shipped artifact. When a property of a real shipped prompt is genuinely behavior-relevant — for example it must reference a placeholder the substitution code fills — the test SHALL render the real prompt with a distinct sentinel value per placeholder and assert the sentinels appear in the output, asserting the substituted values and never the surrounding instruction prose.

Coarse "tripwire" content checks — asserting a URL or keyword is merely present in a real artifact — are the same prohibited category, not an exception; they guarantee nothing review and the drift audit do not.

This requirement is the source of truth the drift audit enforces against: a unit test that asserts prompt or message wording is a drift-audit finding, with the disposition to delete it (or, when it guards a behavior-relevant property, refactor it to a sentinel-substitution test) — never to substitute a less-brittle token check.

#### Scenario: Behavior test uses a synthetic fixture rather than the real prompt
- **GIVEN** a test that verifies prompt-placeholder substitution
- **WHEN** the test is written per this requirement
- **THEN** it constructs a synthetic template the test itself defines (e.g. `"ctx={{change_context}}"`) AND asserts the substituted value appears in the rendered output
- **AND** it does NOT read the real shipped prompt to assert any substring of its instruction prose

#### Scenario: Verifying a behavior-relevant property of a real prompt via sentinels
- **GIVEN** a real shipped default template that must reference `{{change_context}}`, `{{changed_files}}`, AND `{{diff}}` because the substitution code fills them
- **WHEN** a test verifies the template references all three
- **THEN** it renders the real default with a distinct sentinel value per placeholder AND asserts each sentinel appears in the rendered output
- **AND** it asserts the substituted sentinel values, NOT the template's hand-authored instruction wording

#### Scenario: A wording-assertion test is a drift-audit finding
- **GIVEN** a unit test that reads a real embedded prompt AND asserts a hand-authored sentence is present as a substring
- **WHEN** the drift audit reads this requirement against the test code
- **THEN** the test is reported as a finding for asserting message wording rather than behavior or derivation
- **AND** the recommended disposition is to delete the test, or refactor it to a sentinel-substitution test when it guards a behavior-relevant property — NOT to add a less-brittle token check

## REMOVED Requirements

### Requirement: OpenSpec upstream-docs pointer is regression-tested across the spec-drafting prompt set AND `docs/README.md`

Removed because it is a prompt-content test with no behavior: it asserts nine files each contain the `https://github.com/Fission-AI/OpenSpec` substring plus a topical hint. By the new requirement `Tests assert behavior or derivation, never message wording`, this is the prohibited category — a coarse presence-tripwire on hand-authored content that guarantees nothing code review and the drift audit do not. The intent (agents and humans have a pointer to OpenSpec's conventions) survives as ordinary prompt and documentation content, reviewed like any other edit. The regression test it mandated is deleted in this change; the OpenSpec links in the prompts and `docs/README.md` are left in place.

### Requirement: `prompts/implementer-revision.md` instructs the revision agent on `outcome_success` AND `final_answer` content

Removed because it mandates that `prompts/implementer-revision.md` contain the substrings `outcome_success`, `final_answer`, `declined`, AND `Test counts`, enforced by a regression test reading the file — the same prohibited prompt-wording category as the requirement above. The design intent (the revision prompt instructs the agent to call `outcome_success` with a content-shaped `final_answer`, and that declining is a valid reported outcome) survives as prompt content, verified by review AND the drift audit. The mandated regression test is deleted in this change (it shares `autocoder/tests/openspec_pointers.rs` with the pointer test above). The prompt's outcome-signal section itself is left in place.
