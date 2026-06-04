# Implementation tasks

## 1. Shared single-pass substitution helper

- [x] 1.1 Add `prompts::render_template(template: &str, vars: &[(&str, &str)]) -> String` (or equivalent) that scans the template ONCE and replaces each recognized `{{key}}` with its value, never re-scanning substituted values. Implement via split-on-placeholder-and-interleave, an aho-corasick single pass, or a regex `replace_all` with a closure over `vars` — any approach where injected content is not re-scanned.
- [x] 1.2 Unrecognized `{{tokens}}` in the template are left verbatim (today's `.replace` leaves unmatched placeholders untouched; preserve that). A `{{key}}` token appearing inside a VALUE is emitted verbatim, never expanded.

## 2. Migrate the four prompt-assembly sites

- [x] 2.1 `code_reviewer.rs::review_with_preamble` — replace the `.replace("{{cross_change_preamble}}", …).replace("{{change_context}}", …).replace("{{changed_files}}", …).replace("{{diff}}", …)` chain with one `render_template` call carrying the four pairs.
- [x] 2.2 `polling/scout.rs` — replace the 9-call `.replace` chain (`{{max_items}}`, `{{guidance}}`, `{{repo_url}}`, `{{head_sha}}`, `{{readme}}`, `{{docs_listing}}`, `{{symbols_overview}}`, `{{recent_activity}}`, `{{open_issues}}`) with `render_template`.
- [x] 2.3 `polling/brownfield.rs` — replace the `.replace` chain (`{{capability_name}}`, `{{guidance}}`, `{{repo_url}}`, `{{readme}}`, `{{docs_listing}}`, `{{symbols_overview}}`) with `render_template`.
- [x] 2.4 `polling/brownfield_survey.rs` — replace the `.replace` chain (`{{max_capabilities}}`, `{{guidance}}`, `{{repo_url}}`, `{{readme}}`, `{{docs_listing}}`, `{{symbols_overview}}`, `{{already_specced}}`) with `render_template`.
- [x] 2.5 `executor/claude_cli.rs` — replace the chained `.replace(*_PLACEHOLDER, …)` blocks in `build_revision_prompt` (`{{pr_body}}`, `{{pr_change_list}}`, `{{agent_implementation_notes}}`, `{{pr_diff}}`, `{{revision_request}}`), `build_triage_prompt` (`{{findings}}`, `{{audit_type}}`, `{{repo_url}}`, `{{canonical_specs_index}}`), `build_chat_triage_prompt` (`{{request_text}}`, `{{repo_url}}`, `{{canonical_specs_index}}`), AND `build_changelog_prompt` (`{{changelog_json}}`, `{{repo_url}}`, `{{revision_text}}`) with `render_template`. Leave `build_prompt` (single `{{change_body}}` replace) AND `build_recovery_prompt` (append-based) unchanged.

## 3. Tests

- [x] 3.1 Helper: a value containing another placeholder token (e.g. `vars` includes `("readme", "...{{symbols_overview}}...")`) renders that token verbatim; the template's own `{{symbols_overview}}` is substituted exactly once — regardless of pair order.
- [x] 3.2 Helper: linear growth — a value containing K copies of `{{x}}` grows the output by `K × len("{{x}}")`, NOT `K × len(value_of_x)`.
- [x] 3.3 Helper: normal inputs (no placeholder tokens in values) render byte-identically to the old chained `.replace`.
- [x] 3.4 Reviewer regression: a `ReviewContext` whose changed files contain literal `{{diff}}` and `{{changed_files}}` renders a prompt whose size is bounded by `change_context + changed_files + diff + template` (no multiplicative blowup), AND those literals survive verbatim in the changed-files section.
- [x] 3.5 Executor regression: `build_revision_prompt` with a `pr_diff` whose text contains literal `{{revision_request}}` / `{{pr_body}}` (the self-hosting case — the diff touches `prompts/implementer-revision.md`) does NOT re-expand them; the revision request AND PR body are each inserted exactly once.

## 4. Acceptance gate

- [x] 4.1 `cargo test` passes for the autocoder crate.
- [x] 4.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 4.3 `openspec validate a002-single-pass-prompt-substitution --strict` passes.
