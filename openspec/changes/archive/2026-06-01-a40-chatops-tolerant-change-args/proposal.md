## Why

Operators recovering from a chatops marker alert (`.perma-stuck.json` or `.needs-spec-revision.json`) copy-paste the change slug out of the alert message AND issue a recovery verb. The alert template wraps the slug in single backticks for readability (`` `a37-unify-llm-provider-config` ``); Slack does NOT strip them when the operator copies. The operator pastes the backtick-wrapped slug as the verb's argument AND immediately hits the existing argument-sanitization regex:

```
@autocoder clear-revision autocoder `a37-unify-llm-provider-config`
✗ invalid change name (must match ^[a-zA-Z0-9_-]+$, max 64 chars)
```

The regex is doing its job — backticks aren't valid filesystem characters AND the sanitization layer is what keeps path-traversal payloads out of the marker-removal handlers. The problem is that backticks aren't a security threat; they're a markdown wrapper introduced by the alert templates themselves. The operator-visible error blames the operator for a hostility the operator never expressed.

Beyond the backtick frustration: the change slug is always the FULL `aNN-<descriptive-suffix>` form even though the disambiguator the operator types from memory (or finds quickly in scrollback) is the leading `aNN` prefix. Recovery verbs operate on changes carrying a specific marker file. When a repository has exactly one change with `.needs-spec-revision.json` (the common case — operators address blockers as they fire, AND markers don't pile up unless something is badly wrong), the leading prefix is sufficient to identify the change with zero ambiguity. Requiring the operator to type or paste the full slug in this case is friction that scales with operator session count, not with operator skill.

Both gaps share a shape: arguments that the operator's intent makes obvious, but the parser's surface contract refuses to accept. This change relaxes both without weakening the path-traversal AND shell-metacharacter protections that the existing sanitization buys.

## What Changes

**Backtick stripping at the parser tokenization step.** After splitting the post-mention text into whitespace-delimited tokens, each token SHALL have surrounding backticks stripped (`token.trim_matches('`')`) BEFORE the existing regex-based validation runs. The strip is symmetric (leading AND trailing) AND applies once — embedded backticks in the middle of a token are preserved AND will continue to fail the regex (the regex is the validation surface; the strip only handles markdown-wrapper hygiene). The strip applies uniformly to every operator-supplied argument: change slugs, repo substrings, audit substrings, capability names. The existing `^[a-zA-Z0-9_-]+$` AND `^[a-zA-Z0-9._/-]+$` regexes run unchanged on the stripped token; path-traversal AND shell-metacharacter rejection is preserved.

**Partial change-slug resolution at the control-socket layer**, scoped per marker-clearing action:

| Action | Marker scope for prefix matching |
| --- | --- |
| `clear_revision_marker` | Changes with `.needs-spec-revision.json` present |
| `clear_perma_stuck_marker` | Changes with `.perma-stuck.json` present |
| `ignore_for_queue_marker` | Changes with `.perma-stuck.json` OR `.needs-spec-revision.json` present |
| `clear_ignore_for_queue_marker` | Changes with `.ignore-for-queue.json` present |

When the operator-submitted `change` argument does NOT exactly match a directory name under `<workspace>/openspec/changes/`, the handler SHALL enumerate change directories carrying the action's relevant marker AND find candidates whose directory name starts with the supplied argument (case-sensitive prefix match — the slugs are lowercase by convention AND case-sensitivity preserves the existing error semantics for typos). A unique candidate is resolved to its canonical slug AND the action proceeds with the canonical slug. Zero candidates produces an error naming the action's marker scope (e.g., `no change matching prefix 'a37' has a .needs-spec-revision.json marker`). Multiple candidates produce an error listing the candidates (`multiple changes match prefix 'a3': a37-..., a38-...`). The dispatcher's success reply uses the canonical slug so the operator sees what was resolved.

Exact-match behavior is unchanged: when the supplied argument already names a directory verbatim, the resolver returns it without enumerating; today's error path for `change directory not found` (e.g., a typo or a slug for a change that no longer exists) is preserved when the supplied argument matches no directory AND is not a prefix of any candidate.

**Tokenizer-layer backtick stripping is the only sanitization change.** The existing regex set, length cap (64 chars for change slugs, 128 for repo substrings), AND oversize-rejection scenarios continue to fire unchanged. An operator pasting `` `<much-longer-string>` `` still gets the oversize rejection on the inner string after backticks are stripped.

**No new config knobs.** Behavior is unconditional. Backtick stripping is a parser hygiene step; partial-slug resolution is a per-action filesystem lookup that only fires when the exact-match path returns "not found." Neither has a "disable" pathway because neither has a failure mode an operator would want to opt out of.

## Impact

- **Affected specs:**
  - `chatops-manager` — MODIFIED the canonical "Argument sanitization at parser entry" requirement to insert a backtick-stripping step before the regex check. The four existing scenarios (path-traversal, shell-metacharacter, oversize, valid-args) are preserved verbatim; two new scenarios cover backtick stripping on change slug AND on repo substring.
  - `orchestrator-cli` — ADDED a new requirement defining the partial-change-slug resolution rule, marker-scope-per-action, AND the dispatch-time error shapes (no-match, multi-match, success with canonical slug).
- **Affected code:**
  - `autocoder/src/chatops/operator_commands.rs` — the post-mention tokenizer (currently `let rest: Vec<&str> = tokens.collect();`) wraps each token with `trim_matches('\`')` before collection. Affects every verb's argument path uniformly.
  - `autocoder/src/queue.rs` — new `resolve_change_prefix(workspace, prefix, marker_filter) -> Result<String, ResolvePrefixError>` helper. The `marker_filter` parameter accepts an enum `{NeedsRevision, PermaStuck, EitherBlocking, IgnoreForQueue}` so each call site stays explicit about the scope.
  - `autocoder/src/control_socket.rs` — `handle_clear_revision`, `handle_clear_perma_stuck`, `handle_ignore_for_queue`, AND `handle_clear_ignore_for_queue` call the resolver before the existing marker-removal step. The resolved canonical slug replaces the operator-supplied prefix in all downstream calls (queue operations, commit subjects, response JSON).
  - Unit tests for the new resolver covering exact-match (passthrough), unique-prefix-match (resolved), zero-match (error message), multi-match (candidate list). Unit tests for the parser layer covering backtick-wrapped change slugs, backtick-wrapped repo substrings, asymmetric backticks (only on one side — also stripped), AND embedded backticks (rejected by regex).
  - Integration test covering the full chatops → control-socket → marker-removal path with a backtick-wrapped prefix-only slug argument.
- **Operator-visible behavior:**
  - `@<bot> clear-revision <repo> \`a37\`` AND `@<bot> clear-revision <repo> \`a37-unify-llm-provider-config\`` AND `@<bot> clear-revision <repo> a37` all resolve identically when exactly one change in the repo carries `.needs-spec-revision.json`.
  - When multiple changes share a prefix AND multiple have the marker, the operator sees a candidate list AND retypes with a longer prefix (or the full slug).
  - When no change matches the prefix OR no matching change has the marker, the operator sees a precise error that names the missing-marker condition.
  - The dispatcher's success reply names the resolved canonical slug, so the operator's scrollback retains a record of what was actually cleared.
- **Backward compatibility:** existing exact-slug verb invocations continue to work unchanged. The relaxed parser accepts a strict superset of inputs that were valid before. No config migration; no message-format changes; no alert-template changes.
- **Dependencies:** none. Independent of `a37`, `a38`, `a39`, AND every other queued change. Can land in any order.
- **Acceptance:** `cargo test` passes; `openspec validate a40-chatops-tolerant-change-args --strict` passes. Tests:
  - Parser: `@<bot> clear-revision myrepo \`a37-foo\`` parses to `ClearRevision { repo_substring: "myrepo", change: "a37-foo" }` (backticks stripped).
  - Parser: `@<bot> clear-revision myrepo a37\`foo` (embedded backtick) fails sanitization with the existing invalid-change-slug error.
  - Parser: `@<bot> clear-revision myrepo \`a;rm -rf /\`` still rejects the inner shell-metacharacter payload (strip-then-regex preserves the shell-metacharacter scenario from the existing requirement).
  - Resolver: a workspace with `a37-foo` carrying `.needs-spec-revision.json` AND `a38-bar` carrying nothing resolves prefix `a3` AND prefix `a37` to `a37-foo`.
  - Resolver: a workspace with `a37-foo` AND `a38-bar` both carrying `.needs-spec-revision.json` rejects prefix `a3` with a multi-match error listing both AND resolves prefix `a37` AND prefix `a38` unambiguously.
  - Resolver: a workspace where the prefix matches a directory but the matched directory has no relevant marker returns the no-matching-marker error (NOT a generic not-found).
  - Handler: `handle_clear_revision` called with prefix `a37` against a workspace where `a37-foo` has the marker writes the success response with `change: "a37-foo"` (the canonical slug).
  - Integration: end-to-end chatops message `@<bot> clear-revision myrepo \`a37\`` for a fixture workspace where `a37-foo/.needs-spec-revision.json` exists removes the marker AND the dispatcher reply names `a37-foo`.
