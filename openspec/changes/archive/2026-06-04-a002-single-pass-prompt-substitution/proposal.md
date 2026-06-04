## Why

The reviewer assembles its prompt with naive chained `String::replace`:

```rust
template.replace("{{change_context}}", change_context)
        .replace("{{changed_files}}", changed_files)  // injects file CONTENTS
        .replace("{{diff}}", diff)                     // re-scans everything above
```

Because each pass re-scans content the previous passes injected, any placeholder token that appears *inside* a substituted value gets expanded by a later pass. When PR #87 (a change to the reviewer's own code and specs) was reviewed, the injected `code_reviewer.rs` + `code-reviewer`/`executor`/`orchestrator-cli` spec files contained ~58 literal `{{diff}}` tokens — and the final `.replace("{{diff}}", …)` stamped the 76 KB diff into every one of them, exploding the prompt from ~1.09 MB to **5.5 MB** (`prompt_bytes=5520417`, 276% of budget). The size budget can't catch it: it runs in `render_sections`, *before* substitution. Worse than the overflow, this silently *corrupts* every review of a change touching files that contain `{{…}}` tokens (templates, docs, the reviewer itself) — the diff is smeared dozens of times through the file text.

An audit for the same pattern found it in **seven** more prompt-assembly sites:

- the `scout`, `brownfield-draft`, and `brownfield-survey` polling-iteration prompts — each chains `.replace` over injected `README` / docs-listing / symbols-overview / operator-`guidance` content;
- four executor prompt builders — `build_revision_prompt` (injects `{{pr_body}}`, `{{pr_diff}}`, `{{revision_request}}`), `build_triage_prompt` (`{{findings}}`, `{{canonical_specs_index}}`), `build_chat_triage_prompt` (operator `{{request_text}}`), and `build_changelog_prompt` (`{{changelog_json}}`, `{{revision_text}}`) — whose `{{…}}` placeholders are held in named constants, which is why a literal grep missed them.

The executor's revision prompt carries the **same high-severity self-hosting trigger** as the reviewer: `prompts/implementer-revision.md` itself contains `{{pr_diff}}`, `{{revision_request}}`, and `{{pr_body}}`, so revising a PR whose diff touches that template re-expands them — corrupting the revision prompt while the autocoder works on its own repo. `build_prompt` (implementer, a single replace), `build_recovery_prompt` (append-based), the `{{MAX_PROPOSALS}}` audit substitution (single, numeric), the contradiction check (appends deltas via `format!`), the `PromptLoader` (no `{{}}` substitution), and the audits (static prompts, agent reads files itself) are all NOT affected.

## What Changes

**A shared single-pass substitution helper.** A new helper renders a `{{placeholder}}` template by scanning the template ONCE and replacing every recognized placeholder with its value, never re-scanning injected values. A placeholder token appearing inside a substituted value (a README, a diff, a changed file's contents, operator guidance) stays literal in the output. Rendering is linear in input size — it cannot multiply.

**All eight placeholder-substitution prompt sites adopt it.** The reviewer (`review_with_preamble`), the `scout` / `brownfield-draft` / `brownfield-survey` polling prompts, AND the four executor builders (`build_revision_prompt`, `build_triage_prompt`, `build_chat_triage_prompt`, `build_changelog_prompt`) replace their chained-`.replace` blocks with the single-pass helper. Behavior is identical for inputs that don't contain placeholder tokens; inputs that do are no longer corrupted. The single-replace sites (`build_prompt`, the audit `{{MAX_PROPOSALS}}` substitution) are left as-is — a single replace cannot re-expand.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — ADDED `Prompt-template substitution is single-pass (no placeholder re-expansion)` (the shared helper + the scout/brownfield/survey prompts adopt it).
  - `code-reviewer` — ADDED `Reviewer renders its prompt with single-pass substitution`.
  - `executor` — ADDED `Executor prompt builders use single-pass substitution`.
- **Affected code:**
  - A shared helper (e.g. `prompts::render_template(template, &[(key, value)])`) doing single-pass substitution (split-on-placeholder-and-interleave, or an aho-corasick / regex single pass).
  - `code_reviewer.rs::review_with_preamble` — replace the 4-call `.replace` chain with the helper.
  - `polling/scout.rs`, `polling/brownfield.rs`, `polling/brownfield_survey.rs` — replace their `.replace` chains with the helper.
  - `executor/claude_cli.rs` — `build_revision_prompt`, `build_triage_prompt`, `build_chat_triage_prompt`, `build_changelog_prompt` use the helper. `build_prompt` (single replace) is unchanged.
- **Operator-visible behavior:** reviews and spec-drafting prompts for changes touching files that contain `{{…}}` tokens are no longer corrupted or inflated. For all other inputs, output is byte-identical to today. The PR #87 reviewer overflow does not recur (its prompt drops back to ~1.09 MB).
- **Acceptance:** `cargo test` passes; `openspec validate a002-single-pass-prompt-substitution --strict` passes. Tests: a value containing another placeholder token is not re-expanded; rendered size grows linearly (not multiplicatively) with the count of placeholder literals in injected values; all placeholders still substitute once for normal inputs; a `ReviewContext` whose changed files contain literal `{{diff}}`/`{{changed_files}}` renders a prompt bounded by the section sizes.
- **Dependencies:** none — independent of the fleet stream. Fixes a live defect in the default (oneshot) reviewer AND the spec-drafting prompts. a58's agentic reviewer separately sidesteps the reviewer case (it does not substitute file contents into a placeholder template), but a66 fixes the oneshot path AND the polling prompts, which a58 does not touch.
