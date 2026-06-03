# Implementation tasks

## 1. Surface `final_answer` in the revision success comment

- [x] 1.1 In `autocoder/src/revisions.rs`, change the `Ok(ExecutorOutcome::Completed { .. })` arm of the revision dispatcher (around line 943) to destructure `final_answer` AND make it available in the success-path scope.
- [x] 1.2 In the success-path block (around lines 990-993), change the reply composition:
  - Build the success line: `format!("✅ Revision applied: {}. Revision count: {} of {}.", commit_subject, state.revisions_applied, state.revision_cap)`.
  - When `final_answer` is `Some(text)` AND `text.trim()` is non-empty: append `\n\n{text}` to the reply.
  - Otherwise: leave the reply as the single-line success form (today's behavior).
- [x] 1.3 Apply `truncate_to_fit` (currently in `polling_loop.rs:5352`) to the composed reply BEFORE posting. The helper's current truncation marker (`_[implementer summary truncated to fit GitHub comment limit; full output at <logs_dir>/runs/<workspace-basename>/<change>.log]_`) is implementer-specific; either:
  - Make the marker text a parameter the caller supplies (revision composer passes a revision-specific marker), OR
  - Generalize the marker to apply equally to both (e.g., `_[summary truncated to fit GitHub comment limit; full output at <logs_dir>/runs/<workspace-basename>/<change>.log]_` — drop the `implementer` word).
  Pick whichever produces less churn. If `truncate_to_fit` is private (`fn` not `pub fn`), promote it to `pub(crate)` AND import where needed.

## 2. Update `prompts/implementer-revision.md` with outcome-tool guidance

- [x] 2.1 Add a new section near the bottom of `prompts/implementer-revision.md` (before the `--- BEGIN CHANGES IN THIS PR ---` template marker). The section name SHOULD be `## Outcome signal` or similar.
- [x] 2.2 Section content (adapt wording but preserve the required markers per the spec):

  ```
  ## Outcome signal

  At end-of-run, call `outcome_success` with a brief `final_answer` (5-10 lines) summarizing the revision. This text becomes the body of the success comment posted under the operator's revision request. Cover:

  - What the reviewer asked for (one line restating the request).
  - What you changed in response — name the files / functions touched.
  - Whether you agreed with the reviewer's claim. If you concluded the request was wrong (mistaken about the code, asks for something that would break tests, references a symbol that doesn't exist), say so AND explain why you declined OR partially honored it. Declining is a valid outcome; report it explicitly.
  - Test counts: new tests added (if any), pass/fail from the final run.
  - `cargo clippy --all-targets -- -D warnings` AND `openspec validate <change> --strict` results when applicable.

  Worked example:

  > Reviewer asked for case-insensitive prefix matching on the new resolver in `queue.rs::resolve_change_prefix`. Investigated the slug-naming convention (memory: `[openspec-stacked-change-naming]`) AND confirmed all in-repo slugs are lowercase by convention. Declined the request: case-insensitive matching would let `A37` match `a37-foo` AND also let `archive` partial-match `a` (the archive dir is filtered, but the resolver's diagnostic message would still confuse). No code changed.
  >
  > Tests: 0 added (declined revision).
  > `cargo clippy --all-targets -- -D warnings`: clean.
  > `openspec validate a40-chatops-tolerant-change-args --strict`: pass.
  ```

- [x] 2.3 If the prompt has an existing "Your job" numbered list, add a final step `N. On the success path, call `outcome_success` with a `final_answer` per the content guidance above.`

## 3. Tests

- [x] 3.1 Unit test in `autocoder/src/revisions.rs`'s test module: invoke the revision success-comment composer (extract to a helper if it's currently inline) with `Completed { final_answer: Some("Did X, declined Y because Z.") }` AND assert the resulting body contains the success line AND `Did X, declined Y because Z.` separated by a blank line.
- [x] 3.2 Unit test: invoke with `Completed { final_answer: None }` AND assert the body is the single-line success form.
- [x] 3.3 Unit test: invoke with `Completed { final_answer: Some("   ") }` AND assert the body is the single-line success form (whitespace-only treated as empty).
- [x] 3.4 Unit test: invoke with a `final_answer` longer than the GitHub comment limit AND assert the body is truncated with the truncation marker.
- [x] 3.5 Prompt-content regression test: extend the existing `a41-link-openspec-conventions` regression test (OR add a sibling test) asserting `prompts/implementer-revision.md` contains the substrings `outcome_success`, `final_answer`, `declined`, AND `Test counts`. Combined-failure reporting per the a41/a42 pattern.

## 4. Acceptance gate

- [x] 4.1 `cargo test` passes for the autocoder crate, including the new tests.
- [x] 4.2 `openspec validate a45-revision-summary-surfaces-in-pr-comment --strict` passes.
- [ ] 4.3 Manual end-to-end: against a test repo's open PR, post `@<bot> revise <substantive request>`. Confirm the success comment contains both the success line AND the agent's summary; verify the truncation marker appears when forcing a long summary (e.g., via a fixture). (NOT performed inside the autocoder sandbox — requires a live deployed daemon, a real chatops/GitHub backend, and a configured test repo with an open PR. This is a post-deploy operator verification step; the runtime behavior it checks is covered by the §3 unit tests: 3.1 surfaces the summary after a blank line, 3.2/3.3 keep the single-line form when `final_answer` is absent/empty, AND 3.4 asserts the truncation marker on an oversize summary.)
