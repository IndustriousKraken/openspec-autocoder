# Implementation tasks

## 1. Parser-layer backtick stripping

- [x] 1.1 In `autocoder/src/chatops/operator_commands.rs`, change the post-mention tokenization in `parse_command_outcome_in_thread` so each token is run through `str::trim_matches('\`')` before being collected. The simplest shape is `let rest: Vec<&str> = tokens.map(|t| t.trim_matches('\`')).collect();` keeping `rest` as `Vec<&str>` (the trim returns a slice of the original). Apply uniformly — no verb-specific gating; every verb's arguments benefit AND no verb's behavior depends on backticks being significant.
- [x] 1.2 Add unit tests in the parser test module covering:
  - `@<bot> clear-revision myrepo \`a37-foo\`` parses to `ClearRevision { repo_substring: "myrepo", change: "a37-foo" }`.
  - `@<bot> clear-perma-stuck \`myrepo\` \`a37-foo\`` strips backticks from BOTH the repo substring AND the change slug.
  - Asymmetric backticks (only-leading OR only-trailing) are also stripped (e.g., `` `a37-foo `` AND `` a37-foo` ``).
  - Embedded backticks (`a37\`foo`) fail the existing `change_slug_regex` AND produce the existing invalid-change-slug reply (the strip is for SURROUNDING backticks only; mid-token backticks are still invalid).
  - Shell-metacharacter payload wrapped in backticks (`` `a;rm -rf /` ``) is stripped to `a;rm -rf /` AND then rejected by the regex with the existing invalid-change-slug error. Path-traversal payload wrapped in backticks (`` `../../etc/passwd` ``) is similarly stripped AND then rejected. Both preserve the parser's "no FS or socket I/O" guarantee from the canonical "Argument sanitization at parser entry" requirement.
  - The existing four sanitization scenarios (path-traversal, shell-metacharacter, oversize, valid-args) continue to pass unchanged.

## 2. Resolver helper for partial change-slug → canonical slug

- [x] 2.1 In `autocoder/src/queue.rs`, add a public enum naming the marker filters the resolver supports:
  ```rust
  pub enum ChangePrefixMarkerScope {
      NeedsRevision,       // .needs-spec-revision.json
      PermaStuck,          // .perma-stuck.json
      EitherBlocking,      // .needs-spec-revision.json OR .perma-stuck.json
      IgnoreForQueue,      // .ignore-for-queue.json
  }
  ```
- [x] 2.2 Add `pub fn resolve_change_prefix(workspace: &Path, prefix: &str, scope: ChangePrefixMarkerScope) -> Result<String, ResolvePrefixError>` AND `pub enum ResolvePrefixError { NoMatch { scope: ChangePrefixMarkerScope }, MultiMatch { candidates: Vec<String> } }`. The resolver:
  1. If `<workspace>/openspec/changes/<prefix>/` exists AND that directory carries the scope-required marker, return `Ok(prefix.to_string())` (exact-match fast path).
  2. Otherwise, enumerate directories under `<workspace>/openspec/changes/`, skip the archive dir AND dotfile entries (matching the existing `list_pending` skip rules), filter to those carrying the scope-required marker (per the table — `EitherBlocking` matches when EITHER marker file exists), AND collect entries whose directory name starts with `prefix` (case-sensitive `str::starts_with`).
  3. Exactly one candidate → `Ok(candidate)`. Zero candidates → `Err(NoMatch { scope })`. Two or more candidates → `Err(MultiMatch { candidates })` with the candidate list sorted ascending for deterministic output.
- [x] 2.3 Add `impl ResolvePrefixError { pub fn to_operator_message(&self, prefix: &str) -> String }` so call sites format consistently:
  - `NoMatch { scope: NeedsRevision }` → `"no change matching prefix '<prefix>' has a .needs-spec-revision.json marker"`
  - `NoMatch { scope: PermaStuck }` → `"no change matching prefix '<prefix>' has a .perma-stuck.json marker"`
  - `NoMatch { scope: EitherBlocking }` → `"no change matching prefix '<prefix>' has an operator-action marker (.perma-stuck.json OR .needs-spec-revision.json)"`
  - `NoMatch { scope: IgnoreForQueue }` → `"no change matching prefix '<prefix>' has a .ignore-for-queue.json marker"`
  - `MultiMatch { candidates }` → `"multiple changes match prefix '<prefix>': <comma-sep candidates>. Retype with a longer prefix or the full slug."`
- [x] 2.4 Unit tests in `queue.rs`'s test module:
  - Exact-match passthrough returns the input unchanged when the directory exists AND has the scope marker.
  - Exact-match name with NO scope marker returns `NoMatch` (the directory exists but has no relevant marker; this is the same shape as a no-match-no-marker case AND the operator gets the same actionable message).
  - Single prefix match resolves to the canonical slug for each scope.
  - Two prefix matches with markers both present returns `MultiMatch` with both candidates listed.
  - Prefix that matches a candidate WITHOUT the marker AND no other candidate has the marker returns `NoMatch`.
  - `EitherBlocking` scope matches a candidate carrying ONLY `.perma-stuck.json`, AND a candidate carrying ONLY `.needs-spec-revision.json`, AND a candidate carrying BOTH.
  - Archive dir (`archive/`) AND dotfile entries (`.foo`) are skipped during enumeration even when they share a prefix.

## 3. Control-socket handler integration

- [x] 3.1 In `autocoder/src/control_socket.rs::handle_clear_revision`, before calling `queue::remove_revision_marker(&workspace_path, &change)`, call `queue::resolve_change_prefix(&workspace_path, &change, ChangePrefixMarkerScope::NeedsRevision)`. On `Ok(canonical)`, replace `change` with `canonical` for the rest of the handler (so the marker-removal call AND the success-response JSON both use the canonical slug). On `Err(e)`, return `json!({"ok": false, "error": e.to_operator_message(&change)})` AND log the resolution failure at INFO with the supplied prefix.
- [x] 3.2 Apply the same integration to `handle_clear_perma_stuck` (scope `PermaStuck`).
- [x] 3.3 Apply the same integration to `handle_ignore_for_queue` (scope `EitherBlocking` — the existing "has no operator-action marker" refusal in the handler is now performed AT resolution time; remove the post-resolution duplicate check since the resolver guarantees the marker exists).
- [x] 3.4 Apply the same integration to `handle_clear_ignore_for_queue` (scope `IgnoreForQueue`).
- [x] 3.5 Log line for each resolution success: `INFO control_socket: resolved partial change '<prefix>' → '<canonical>' for action <action>` (omitted when the prefix exactly equals the canonical slug — only log when the resolver did meaningful work).
- [x] 3.6 Unit tests for each handler covering: exact-slug input (unchanged behavior), prefix-only input that resolves, prefix-only input that fails to resolve (no-match), prefix-only input that fails to resolve (multi-match). The existing exact-match tests for these handlers SHALL continue to pass without modification.

## 4. Integration test

- [x] 4.1 Add an integration test that drives the chatops `handle_message` path with the literal text `@<bot> clear-revision <substr> \`a37\``, using a fixture workspace where `a37-foo/.needs-spec-revision.json` exists AND no other change carries the marker. Assert:
  - The control-socket submission carries `change: "a37"` (the parser-layer string, with backticks stripped).
  - The control-socket handler resolves the prefix AND removes `<workspace>/openspec/changes/a37-foo/.needs-spec-revision.json`.
  - The dispatcher's reply text contains `a37-foo` (the canonical slug), NOT `a37`.
  - The success-response JSON returned by the control socket has `change: "a37-foo"`.

## 5. Documentation updates

- [x] 5.1 Update `docs/CHATOPS.md` near the recovery-verb table to call out the two new operator-friendly behaviors: surrounding backticks are tolerated, AND a leading-prefix slug is sufficient when only one change in the repo carries the action's relevant marker. One short paragraph; one example each. Avoid prescriptive tone — describe the relaxation, not "now you can finally...".
- [x] 5.2 Update `docs/TROUBLESHOOTING.md` if the existing entries reference the invalid-change-slug error in a way that becomes stale (i.e., if there's an entry that says "the alert template wraps the slug in backticks; don't paste them" — that workaround is no longer needed). Otherwise skip.
- [x] 5.3 No CONFIG.md update needed (no config knobs).
- [x] 5.4 No CLI.md update needed (this is a chatops-layer change).

## 6. Acceptance gate

- [x] 6.1 `cargo test` passes for the autocoder crate.
- [x] 6.2 `openspec validate a40-chatops-tolerant-change-args --strict` passes.
- [x] 6.3 Spot-check: the existing parser-layer scenarios (path-traversal, shell-metachar, oversize, valid-args) in `chatops-manager` still pass without modification — backtick stripping was inserted BEFORE the regex check, NOT instead of it.
