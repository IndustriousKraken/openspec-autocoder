## 1. Config field for opt-in

- [ ] 1.1 In `autocoder/src/config.rs`, extend `ReviewerConfig` with `pub auto_revise_on_block: bool` defaulting to `false` via `#[serde(default)]` (Rust's default for `bool` is `false`, no helper function needed).
- [ ] 1.2 Tests:
  - Default config (no `reviewer.auto_revise_on_block` field) parses with `auto_revise_on_block == false`.
  - Explicit `reviewer.auto_revise_on_block: true` parses with `true`.

## 2. Reviewer output schema extension

- [ ] 2.1 In `autocoder/src/code_reviewer.rs`, extend the parsed-concern type:
  ```rust
  pub struct ReviewConcern {
      pub summary: String,                       // existing field
      pub actionable_request: Option<String>,    // NEW; None when reviewer didn't supply one
      pub should_request_revision: bool,         // NEW; default false
      // other existing fields...
  }
  ```
  Use `#[serde(default)]` on the two new fields so older reviewer responses (templates not yet updated) parse cleanly with defaults.
- [ ] 2.2 Update the response parser to populate the new fields from the reviewer's structured output. The reviewer's response format is the existing JSON/YAML/markdown shape the parser already handles; just extend the schema to recognize the new keys.
- [ ] 2.3 Tests:
  - Parse a reviewer response containing one concern with `actionable_request: "fix the find_user function"` and `should_request_revision: true` → both fields populated correctly.
  - Parse a reviewer response with concerns missing the new fields → `actionable_request: None`, `should_request_revision: false`.
  - Parse a malformed reviewer response → the existing parse-failure-defaults-to-Concerns path continues to fire; the new fields default to their unset values.

## 3. Default reviewer prompt template update

- [ ] 3.1 Append a section to `prompts/code-review-default.md` instructing the LLM on the new per-concern fields:
  - For each concern, produce `actionable_request` if and only if the concern has a concrete, executable fix the implementer agent can apply without further clarification.
  - Set `should_request_revision: true` only when `actionable_request` is non-empty AND the fix is unambiguous.
  - Style preferences, philosophical disagreements, "consider whether..." suggestions should leave `should_request_revision: false` — they are commentary, not revision requests.
  - Order concerns most-critical-first so cap-budget truncation drops the lowest-priority ones.
- [ ] 3.2 The template change is backwards-compatible: reviewer templates the operator has customized (and not updated) will simply not produce the new fields, which defaults the new fields to their unset values; auto-revise will not fire for that operator's setup. A daemon WARN logs at the reviewer's first run if no concerns have `should_request_revision` populated AND `auto_revise_on_block == true` — surfaces the "you flipped the flag but your template doesn't produce the new fields" case.
- [ ] 3.3 Tests:
  - Render the updated template with sample inputs (the existing template test scaffolding) — the new instructions are present.

## 4. Self-author filter exception

- [ ] 4.1 In `autocoder/src/revisions.rs` (from `a01-pr-comment-revision-loop`), update the dispatcher's self-author filter:
  ```rust
  // before parse_revision_trigger:
  if comment.user_login == bot_username
      && !comment.body.trim_start().starts_with("<!-- reviewer-revision -->")
  {
      continue;  // filter as today
  }
  ```
  Bot-authored comments whose body's first non-whitespace text is the literal HTML-comment marker `<!-- reviewer-revision -->` bypass the filter and are parsed normally. All other bot-authored comments continue to be filtered.
- [ ] 4.2 The parser `parse_revision_trigger` does NOT need to understand the marker — by the time it sees the body, the dispatcher has already decided whether to pass it through. The marker line is plain text from the parser's perspective; the mention + verb appear on the second line.
- [ ] 4.3 Tests:
  - Bot-authored comment with body `<!-- reviewer-revision -->\n@<bot> revise foo` → dispatcher passes it through; parser returns `Some("foo")`.
  - Bot-authored comment with body `@<bot> revise foo` (no marker) → dispatcher filters it; parser is not called.
  - Bot-authored comment with body `✅ Revision applied: foo` (the bot's own reply, no marker) → dispatcher filters it.
  - Human-authored comment with body `@<bot> revise foo` → dispatcher passes it through (marker is irrelevant for non-bot authors); parser returns `Some("foo")`.

## 5. Posting protocol in the polling iteration

- [ ] 5.1 In `autocoder/src/polling_loop.rs`, after PR creation (or update) and after the reviewer pass, add a new step `post_reviewer_revision_comments(...)`. Flow:
  1. Check `reviewer_config.auto_revise_on_block` — if false, return Ok (no-op).
  2. Check `verdict == Verdict::Block` — if not, return Ok.
  3. Collect concerns where `should_request_revision == true && actionable_request.is_some()`.
  4. If the collection is empty AND `auto_revise_on_block` is true, log WARN: "reviewer auto-revise is enabled but no concerns had actionable_request + should_request_revision populated; verify the reviewer prompt template has been updated to emit these fields."
  5. Look up the per-PR remaining cap budget (read `RevisionState` for this PR; budget = `revision_cap - revisions_applied`, treating the future cap as if these auto-revisions were the next ones to count).
  6. Take the first `budget` concerns; the remainder is the "dropped due to cap budget" set.
  7. For each taken concern, post a PR comment with body:
     ```
     <!-- reviewer-revision -->
     @<bot-username> revise <actionable_request>
     ```
     using the `post_issue_comment` helper from `a01-pr-comment-revision-loop`.
  8. For each dropped concern, append to the `## Code Review` PR body section: `- (not auto-revised; cap budget exhausted) <summary>`. Update the PR body via the existing PR-update helper.
- [ ] 5.2 Posting failures are logged at WARN per concern but do not abort the iteration. A partial post is recorded — subsequent comments may still attempt to post. If ALL posts fail, the iteration continues normally (the PR is still created/updated; just without the reviewer-revision comments).
- [ ] 5.3 The reviewer-posted comments increment `revisions_applied` ONLY when the dispatcher processes them in a later iteration (the existing increment logic). The posting step itself does NOT pre-increment. The cap-budget check at posting time is a forward-looking estimate to avoid posting comments the dispatcher would then refuse to act on.
- [ ] 5.4 Tests:
  - `auto_revise_on_block: false` + Block verdict → zero comments posted.
  - `auto_revise_on_block: true` + Pass verdict → zero comments posted.
  - `auto_revise_on_block: true` + Concerns verdict → zero comments posted.
  - `auto_revise_on_block: true` + Block verdict + 2 should-revise concerns + 5 remaining budget → 2 comments posted with the documented body shape.
  - `auto_revise_on_block: true` + Block verdict + 3 should-revise concerns + remaining budget of 2 → 2 comments posted; the PR body's Code Review section gains one `(not auto-revised; cap budget exhausted)` entry for the third concern.
  - `auto_revise_on_block: true` + Block verdict + 0 should-revise concerns → zero comments posted; WARN logged.

## 6. End-to-end integration

- [ ] 6.1 Test using a stubbed reviewer returning `Block` with two `should_request_revision: true` concerns AND a stubbed executor that handles revisions successfully:
  1. Iteration 1: executor implements original change → reviewer returns Block + 2 concerns → 2 comments posted to PR.
  2. Iteration 2: revision dispatcher picks up both comments → executor runs revisions → 2 commits force-pushed → 2 `✅ Revision applied:` replies posted.
  3. Assert the PR's commit history has 3 commits: original implementation + 2 revisions.
  4. Assert the PR's comment timeline has the original 2 reviewer-revision comments AND the 2 bot reply comments (4 comments total).

- [ ] 6.2 Cap-interaction test:
  1. Iteration 1: stubbed reviewer returns Block + 6 should-revise concerns; `max_revisions_per_pr: 5`; remaining budget at posting time is 5.
  2. Assert: 5 comments posted; 1 entry in the Code Review section's cap-exhausted list.
  3. Iteration 2: dispatcher processes 5 revisions; `revisions_applied` reaches 5; cap reached on the 5th.
  4. (No 6th to process since it was never posted.)
  5. If a human now posts `@<bot> revise <something>`, the cap-decline comment fires (existing behaviour from `a01-pr-comment-revision-loop`).

## 7. README + docs updates

- [ ] 7.1 In `docs/CODE-REVIEW.md`, add a section "Reviewer-initiated revisions on Block verdicts" describing the `auto_revise_on_block` config, the verdict-gating rule, the per-concern `should_request_revision` decision the reviewer makes, the cap-budget interaction, and the requirement that operator-customized reviewer templates be updated to emit the new fields.
- [ ] 7.2 In `docs/CONFIG.md`'s `reviewer:` reference, add the `auto_revise_on_block` field.
- [ ] 7.3 In `docs/OPERATIONS.md`'s revision-loop section (from `a01-pr-comment-revision-loop`), cross-reference reviewer-initiated revisions so operators know both flows share the cap and the dispatcher.

## 8. Spec delta

- [ ] 8.1 The ADDED requirement in `openspec/changes/a01-reviewer-initiated-revisions/specs/code-reviewer/spec.md` codifies: the opt-in flag, the per-concern shape extension, the posting protocol (HTML-comment marker + trigger pattern), the self-author filter exception, the cap-budget interaction with the dropped-concerns annotation, the verdict-gating (Block only), the no-reviewer-re-run rule, and the backwards-compatibility default behaviour for unaware reviewer templates.

## 9. Verification

- [ ] 9.1 `cargo test` passes (new + existing).
- [ ] 9.2 `openspec validate a01-reviewer-initiated-revisions --strict` passes.
- [ ] 9.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
